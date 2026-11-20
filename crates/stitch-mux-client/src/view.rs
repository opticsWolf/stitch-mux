//! Toolkit-independent session state. Frontends only read this view.

use std::collections::BTreeMap;

use stitch_mux_core::{ActivitySummary, Row, Size};
use stitch_mux_protocol::{
    ActionInfo, Attached, CommandResponse, ErrorInfo, Event, Focus, Keymap, Layout, PaneDelta,
    PaneSnapshot, Render, ResizeEvent, SessionExit,
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
        // A hostile or corrupted peer can send u64::MAX; plain `+ 1` would
        // panic in debug builds and wrap into a valid-looking sequence.
        let expected = self.last_seq.saturating_add(1);
        if delta.seq != expected {
            return Err(ApplyError::Gap(SequenceGap {
                pane_id: self.pane_id,
                expected,
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
        snapshot.notification = delta.notification;
        snapshot.progress = delta.progress;
        snapshot.alt_screen = delta.alt_screen;
        snapshot.scrollback_len = delta.scrollback_len;
        snapshot.total_lines = delta.total_lines;
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
    pub last_command: Option<CommandResponse>,
    /// Latest composed ANSI frame, used only by the terminal projection.
    ///
    /// Structured clients never consume it; the daemon still owns all screen
    /// state and decides whether a frame is full or incremental.
    pub render: Option<Render>,
    pub render_seq: u64,
    pub error: Option<ErrorInfo>,
    pub exit: Option<SessionExit>,
    /// Latest bounded local event from `SubscribeEvents`. Frontends that want
    /// history must keep their own bounded projection; the wire has no replay.
    pub last_event: Option<Event>,
    /// Cached action/keymap responses from `ListActions`/`GetKeymap`.
    pub actions: Vec<ActionInfo>,
    pub keymap: Option<Keymap>,
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
        self.render = None;
        self.render_seq = 0;
        self.needs_resync = false;
    }

    pub fn apply_layout(&mut self, layout: Layout) {
        if layout.revision.saturating_add(1) < self.layout_revision && self.layout.is_some() {
            self.needs_resync = true;
            return;
        }
        if layout.revision < self.layout_revision {
            return; // older layout; keep the newer authoritative geometry
        }
        if layout.revision > self.layout_revision.saturating_add(1) && self.layout.is_some() {
            self.needs_resync = true;
        }
        self.layout_revision = layout.revision;
        self.size = Size::new(layout.cols, layout.rows);
        self.status_rows = layout.status_rows;
        self.attached_clients = layout.attached_clients;
        self.focus = layout.focus;
        let visible: std::collections::BTreeSet<u64> =
            layout.panes.iter().map(|pane| pane.pane_id).collect();
        self.panes.retain(|pane_id, _| visible.contains(pane_id));
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

    pub fn apply_event(&mut self, event: Event) {
        self.last_event = Some(event);
    }

    pub fn apply_catalog(&mut self, response: &CommandResponse) {
        if !response.actions.is_empty() {
            self.actions = response.actions.clone();
        }
        if let Some(keymap) = &response.keymap {
            self.keymap = Some(keymap.clone());
        }
    }

    pub fn apply_command_response(&mut self, response: CommandResponse) {
        if let Some(layout) = response.layout.clone() {
            self.apply_layout(layout);
        }
        if !response.ok
            && let Some(error) = response.error.clone()
        {
            self.error = Some(error);
        }
        self.last_command = Some(response);
    }

    /// Apply the server-composed ANSI projection. No client parses these
    /// bytes; a terminal frontend writes them to its output as-is.
    pub fn apply_render(&mut self, render: Render) {
        self.apply_render_seq(render.seq);
        self.size = Size::new(render.cols, render.rows);
        if render.full {
            self.needs_resync = false;
        }
        self.render = Some(render);
    }

    pub fn apply_render_seq(&mut self, seq: u64) {
        if seq <= self.render_seq {
            return;
        }
        if seq > self.render_seq.saturating_add(1) && self.render_seq != 0 {
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
        self.render = None;
        self.last_event = None;
        self.needs_resync = true;
    }

    /// Mark the session lost without discarding a typed error already received
    /// from the daemon (for example `Busy` from `DetachClient`).
    pub fn mark_lost_preserving_error(&mut self, reason: impl Into<String>) {
        if self.error.is_none() {
            self.mark_lost(reason);
            return;
        }
        self.connection = ConnectionState::Lost;
        self.layout = None;
        self.panes.clear();
        self.render = None;
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
        self.render = None;
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
    use stitch_mux_protocol::{PaneDelta, ProcessInfo, RowUpdate, ShellMarks};

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
            notification: None,
            progress: None,

            alt_screen: false,
            scrollback_len: 0,
            total_lines: 0,
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
            notification: None,
            progress: None,

            alt_screen: false,
            scrollback_len: 0,
            total_lines: 0,
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
    fn event_updates_keep_only_the_latest_record() {
        let mut view = SessionView::default();
        view.apply_event(Event {
            seq: 1,
            session_id: Some(3),
            kind: "activity_changed".into(),
            payload: serde_json::json!({"working": 1}),
        });
        view.apply_event(Event {
            seq: 2,
            session_id: Some(3),
            kind: "program_status_changed".into(),
            payload: serde_json::json!({"state": "done"}),
        });
        assert_eq!(view.last_event.as_ref().map(|event| event.seq), Some(2));
        assert_eq!(
            view.last_event.as_ref().map(|event| event.kind.as_str()),
            Some("program_status_changed")
        );
    }

    #[test]
    fn extreme_sequences_from_a_corrupt_peer_do_not_panic() {
        let mut view = SessionView::default();
        let mut snapshot = snapshot(1, u64::MAX, "a");
        snapshot.cells[0].cells[0].text = "z".into();
        view.apply_snapshot(snapshot);

        // A smaller sequence is stale, not a gap; neither path may overflow.
        assert_eq!(view.apply_delta(delta(1, 1, "b")), Ok(()));
        assert_eq!(view.pane(1).unwrap().last_seq, u64::MAX);
        assert_eq!(view.pane_cells(1).unwrap()[0].cells[0].text, "z");
        view.apply_render_seq(u64::MAX);
        view.apply_render_seq(1);
        assert_eq!(view.render_seq, u64::MAX);
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
    fn optional_metadata_survives_snapshot_replacement_and_deltas() {
        let mut view = SessionView::default();
        let mut first = snapshot(1, 1, "a");
        first.cwd = Some("/one".into());
        first.foreground = Some(ProcessInfo {
            pid: Some(1),
            name: "sh".into(),
            cwd: Some("/one".into()),
        });
        first.shell_marks = Some(ShellMarks {
            prompt_rows: vec![0],
            command_rows: vec![],
        });
        first.program_status = Some(ProgramStatus {
            state: ProgramState::Working,
            ..Default::default()
        });
        view.apply_snapshot(first);

        let mut change = delta(1, 2, "b");
        change.cwd = Some("/two".into());
        change.shell_marks = Some(ShellMarks {
            prompt_rows: vec![1],
            command_rows: vec![0],
        });
        change.program_status = Some(ProgramStatus {
            state: ProgramState::Done,
            ..Default::default()
        });
        view.apply_delta(change).unwrap();
        let pane = view.pane(1).expect("pane");
        let current = pane.snapshot.as_ref().expect("snapshot");
        assert_eq!(current.cwd.as_deref(), Some("/two"));
        assert_eq!(
            current.shell_marks.as_ref().map(|marks| &marks.prompt_rows),
            Some(&vec![1])
        );
        assert_eq!(
            current.program_status.as_ref().map(|status| status.state),
            Some(ProgramState::Done)
        );
        assert_eq!(view.activity.done, 1);

        let mut replacement = snapshot(1, 3, "c");
        replacement.cwd = Some("/three".into());
        replacement.shell_marks = Some(ShellMarks {
            prompt_rows: vec![2],
            command_rows: vec![1],
        });
        view.apply_snapshot(replacement);
        let pane = view.pane(1).expect("pane");
        let current = pane.snapshot.as_ref().expect("snapshot");
        assert_eq!(current.cwd.as_deref(), Some("/three"));
        assert_eq!(
            current.shell_marks.as_ref().map(|marks| &marks.prompt_rows),
            Some(&vec![2])
        );
        assert_eq!(current.program_status, None);
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
    fn layout_prunes_panes_that_no_longer_exist() {
        let mut view = SessionView::default();
        view.apply_snapshot(snapshot(1, 1, "a"));
        view.apply_snapshot(snapshot(2, 1, "b"));
        view.apply_layout(stitch_mux_protocol::Layout {
            session_id: 1,
            revision: 2,
            cols: 80,
            rows: 24,
            status_rows: 0,
            attached_clients: 1,
            focus: Some(1),
            root: stitch_mux_core::LayoutNode::leaf(stitch_mux_core::PaneId::new(1)),
            panes: vec![stitch_mux_protocol::PaneGeometry {
                pane_id: 1,
                outer: stitch_mux_core::Rect::new(0, 0, 80, 24),
                content: stitch_mux_core::Rect::new(1, 1, 78, 22),
                title: "sh".into(),
            }],
            zoomed_pane: None,
        });
        assert!(view.pane(1).is_some());
        assert!(view.pane(2).is_none());
    }

    #[test]
    fn lost_connection_preserves_a_typed_daemon_error() {
        let mut view = SessionView {
            error: Some(ErrorInfo {
                code: stitch_mux_protocol::ErrorCode::Busy,
                message: "detached by control client".into(),
            }),
            ..Default::default()
        };
        view.mark_lost_preserving_error("io error");
        assert_eq!(view.connection, ConnectionState::Lost);
        assert_eq!(
            view.error.as_ref().map(|error| error.code),
            Some(stitch_mux_protocol::ErrorCode::Busy)
        );
    }

    #[test]
    fn disconnect_clears_stale_panes() {
        let mut view = SessionView::default();
        view.apply_snapshot(snapshot(1, 1, "a"));
        view.mark_disconnected();
        assert!(view.panes.is_empty());
        assert!(view.needs_resync);
    }

    #[test]
    fn render_frames_are_stored_without_client_side_parsing() {
        let mut view = SessionView::default();
        view.apply_render(Render {
            seq: 1,
            cols: 90,
            rows: 30,
            full: true,
            ansi: b"\\x1b[2Jhello".to_vec(),
        });
        assert_eq!(view.render_seq, 1);
        assert_eq!(view.size, Size::new(90, 30));
        assert_eq!(view.render.as_ref().unwrap().ansi, b"\\x1b[2Jhello");
        assert!(!view.needs_resync);

        view.apply_render(Render {
            seq: 3,
            cols: 90,
            rows: 30,
            full: false,
            ansi: b"more".to_vec(),
        });
        assert!(view.needs_resync);
        assert_eq!(view.render.as_ref().unwrap().ansi, b"more");
    }
}
