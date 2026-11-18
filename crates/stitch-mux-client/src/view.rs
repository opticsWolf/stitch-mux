//! Toolkit-independent session state. Frontends only read this view.

use std::collections::BTreeMap;

use stitch_mux_core::{ActivitySummary, Row, Size};
use stitch_mux_protocol::{
    Attached, ErrorInfo, Focus, Layout, PaneDelta, PaneSnapshot, ResizeEvent, SessionExit,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Lost,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequenceGap {
    pub pane_id: u64,
    pub expected: u64,
    pub got: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyError {
    Gap(SequenceGap),
    NoSnapshot(u64),
}

#[derive(Clone, Debug, Default)]
pub struct PaneView {
    pub pane_id: u64,
    pub snapshot: Option<PaneSnapshot>,
    pub last_seq: u64,
    pub bell: bool,
}

impl PaneView {
    pub fn apply_snapshot(&mut self, snapshot: PaneSnapshot) {
        self.last_seq = snapshot.seq;
        self.snapshot = Some(snapshot);
        self.bell = false;
    }

    pub fn apply_delta(&mut self, delta: PaneDelta) -> Result<(), ApplyError> {
        if delta.seq <= self.last_seq {
            return Ok(()); // duplicate or stale; the authoritative state is newer
        }
        if delta.seq != self.last_seq + 1 {
            return Err(ApplyError::Gap(SequenceGap {
                pane_id: self.pane_id,
                expected: self.last_seq + 1,
                got: delta.seq,
            }));
        }
        let Some(snapshot) = self.snapshot.as_mut() else {
            return Err(ApplyError::NoSnapshot(self.pane_id));
        };
        for update in delta.rows {
            if let Some(row) = snapshot.cells.get_mut(usize::from(update.row)) {
                row.cells = update.cells;
                row.wrapped = update.wrapped;
            }
        }
        snapshot.seq = delta.seq;
        snapshot.cursor = delta.cursor;
        snapshot.modes = delta.modes;
        snapshot.title = delta.title;
        snapshot.cwd = delta.cwd;
        snapshot.foreground = delta.foreground;
        snapshot.shell_marks = delta.shell_marks;
        snapshot.program_status = delta.program_status;
        snapshot.alt_screen = delta.alt_screen;
        snapshot.scrollback_len = delta.scrollback_len;
        snapshot.scrollback_bytes = delta.scrollback_bytes;
        self.last_seq = delta.seq;
        self.bell = delta.bell;
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionView {
    pub session_id: u64,
    pub name: String,
    pub size: Size,
    pub status_rows: u16,
    pub layout_revision: u64,
    pub focus: Option<u64>,
    pub layout: Option<Layout>,
    pub panes: BTreeMap<u64, PaneView>,
    pub activity: ActivitySummary,
    pub attached_clients: u32,
    pub connection: ConnectionState,
    pub needs_resync: bool,
    pub render_seq: u64,
    pub error: Option<ErrorInfo>,
    pub exit: Option<SessionExit>,
}

impl SessionView {
    pub fn apply_attached(&mut self, attached: Attached) {
        self.session_id = attached.session_id;
        self.name = attached.session_name;
        self.size = Size::new(attached.cols, attached.rows);
        self.status_rows = attached.status_rows;
        self.attached_clients = attached.attached_clients;
        self.activity = attached.activity;
        self.layout_revision = attached.layout_revision;
        self.connection = ConnectionState::Connected;
        // A fresh attach replaces all local buffers; never draw a stale pane.
        self.layout = None;
        self.panes.clear();
        self.needs_resync = false;
    }

    pub fn apply_layout(&mut self, layout: Layout) {
        if layout.revision + 1 < self.layout_revision && self.layout.is_some() {
            self.needs_resync = true;
            return;
        }
        if layout.revision < self.layout_revision {
            return; // older layout; keep the newer authoritative geometry
        }
        if layout.revision > self.layout_revision + 1 && self.layout.is_some() {
            self.needs_resync = true;
        }
        self.layout_revision = layout.revision;
        self.size = Size::new(layout.cols, layout.rows);
        self.status_rows = layout.status_rows;
        self.focus = layout.focus;
        self.layout = Some(layout);
    }

    pub fn apply_snapshot(&mut self, snapshot: PaneSnapshot) {
        let pane_id = snapshot.pane_id;
        self.panes
            .entry(pane_id)
            .or_insert_with(|| PaneView {
                pane_id,
                ..Default::default()
            })
            .apply_snapshot(snapshot);
        self.refresh_activity();
    }

    pub fn apply_delta(&mut self, delta: PaneDelta) -> Result<(), ApplyError> {
        let pane_id = delta.pane_id;
        let pane = self.panes.entry(pane_id).or_insert_with(|| PaneView {
            pane_id,
            ..Default::default()
        });
        let result = pane.apply_delta(delta);
        if result.is_err() {
            self.needs_resync = true;
        }
        self.refresh_activity();
        result
    }

    pub fn apply_focus(&mut self, focus: Focus) {
        self.focus = Some(focus.pane_id);
    }

    pub fn apply_resize(&mut self, event: ResizeEvent) {
        self.size = Size::new(event.cols, event.rows);
    }

    pub fn apply_render_seq(&mut self, seq: u64) {
        if seq <= self.render_seq {
            return;
        }
        if seq > self.render_seq + 1 && self.render_seq != 0 {
            self.needs_resync = true;
        }
        self.render_seq = seq;
    }

    pub fn mark_connecting(&mut self) {
        self.connection = ConnectionState::Connecting;
    }

    pub fn mark_disconnected(&mut self) {
        self.connection = ConnectionState::Disconnected;
        self.layout = None;
        self.panes.clear();
        self.needs_resync = true;
    }

    pub fn mark_lost(&mut self, reason: impl Into<String>) {
        self.connection = ConnectionState::Lost;
        self.error = Some(ErrorInfo {
            code: stitch_mux_protocol::ErrorCode::NotFound,
            message: reason.into(),
        });
        self.layout = None;
        self.panes.clear();
        self.needs_resync = true;
    }

    fn refresh_activity(&mut self) {
        self.activity = ActivitySummary::from_statuses(self.panes.values().map(|pane| {
            pane.snapshot
                .as_ref()
                .and_then(|s| s.program_status.as_ref())
        }));
    }

    pub fn pane(&self, pane_id: u64) -> Option<&PaneView> {
        self.panes.get(&pane_id)
    }

    pub fn pane_cells(&self, pane_id: u64) -> Option<&[Row]> {
        self.panes
            .get(&pane_id)?
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.cells.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stitch_mux_core::{ActivitySummary, Cell, Cursor, Modes, ProgramState, ProgramStatus, Row};
    use stitch_mux_protocol::{PaneDelta, RowUpdate};

    fn snapshot(pane_id: u64, seq: u64, text: &str) -> PaneSnapshot {
        PaneSnapshot {
            pane_id,
            seq,
            cols: 2,
            rows: 1,
            cells: vec![Row {
                cells: vec![Cell {
                    text: text.into(),
                    ..Cell::blank()
                }],
                wrapped: false,
            }],
            cursor: Cursor::default(),
            modes: Modes::default(),
            title: "sh".into(),
            cwd: None,
            foreground: None,
            shell_marks: None,
            program_status: None,
            alt_screen: false,
            scrollback_len: 0,
            scrollback_bytes: 0,
        }
    }

    fn delta(pane_id: u64, seq: u64, text: &str) -> PaneDelta {
        PaneDelta {
            pane_id,
            seq,
            rows: vec![RowUpdate {
                row: 0,
                cells: vec![Cell {
                    text: text.into(),
                    ..Cell::blank()
                }],
                wrapped: false,
            }],
            cursor: Cursor::default(),
            modes: Modes::default(),
            title: "sh".into(),
            cwd: None,
            foreground: None,
            shell_marks: None,
            program_status: None,
            alt_screen: false,
            scrollback_len: 0,
            scrollback_bytes: 0,
            bell: false,
        }
    }

    #[test]
    fn applies_snapshots_and_deltas() {
        let mut view = SessionView::default();
        view.apply_snapshot(snapshot(1, 3, "a"));
        view.apply_snapshot(snapshot(2, 1, "b"));
        view.apply_delta(delta(1, 4, "z")).unwrap();
        assert_eq!(view.pane_cells(1).unwrap()[0].cells[0].text, "z");
        assert_eq!(view.pane_cells(2).unwrap()[0].cells[0].text, "b");
        assert_eq!(view.pane(1).unwrap().last_seq, 4);
        assert!(!view.needs_resync);
    }

    #[test]
    fn detects_sequence_gaps_and_ignores_stale_deltas() {
        let mut view = SessionView::default();
        view.apply_snapshot(snapshot(1, 3, "a"));
        assert_eq!(
            view.apply_delta(delta(1, 5, "b")),
            Err(ApplyError::Gap(SequenceGap {
                pane_id: 1,
                expected: 4,
                got: 5,
            }))
        );
        assert!(view.needs_resync);
        assert_eq!(view.pane_cells(1).unwrap()[0].cells[0].text, "a");
        view.needs_resync = false;
        assert!(view.apply_delta(delta(1, 3, "old")).is_ok());
        assert!(!view.needs_resync);
    }

    #[test]
    fn aggregates_current_status_only() {
        let mut view = SessionView::default();
        let mut first = snapshot(1, 1, "a");
        first.program_status = Some(ProgramStatus {
            state: ProgramState::Working,
            ..Default::default()
        });
        view.apply_snapshot(first);
        assert_eq!(
            view.activity,
            ActivitySummary {
                working: 1,
                ..Default::default()
            }
        );
        let mut change = delta(1, 2, "a");
        change.program_status = None;
        view.apply_delta(change).unwrap();
        assert_eq!(view.activity, ActivitySummary::default());
    }

    #[test]
    fn terminal_render_gap_requests_resync() {
        let mut view = SessionView::default();
        view.apply_render_seq(1);
        view.apply_render_seq(2);
        assert!(!view.needs_resync);
        view.apply_render_seq(5);
        assert!(view.needs_resync);
    }

    #[test]
    fn disconnect_clears_stale_panes() {
        let mut view = SessionView::default();
        view.apply_snapshot(snapshot(1, 1, "a"));
        view.mark_disconnected();
        assert!(view.panes.is_empty());
        assert!(view.needs_resync);
    }
}
