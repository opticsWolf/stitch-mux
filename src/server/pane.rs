//! One PTY plus its authoritative screen model.
//!
//! `PtySession` is transport-only. The screen, keyboard flags, scrollback and
//! structured snapshot/delta stream belong here, not in a frontend.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

#[cfg(feature = "bench-profile")]
use crate::bench_profile;
use crate::core::{
    Attrs, CaptureFormat, Cell, CellText, Color, Cursor, CursorShape, Modes, ProgramStatus, Row,
};
use crate::protocol::{
    CaptureResponse, CapturedRow, Notification, PaneDelta, PaneSnapshot, ProcessInfo, Progress,
    RowUpdate, SessionExit, ShellMarks,
};
use stitch_pty::errors::PtyErrorKind;
use stitch_pty::terminal::events::TermEvent;
use stitch_pty::terminal::modes::{BRACKETED_PASTE, DECCKM};
use stitch_pty::terminal::{
    KeyCode as PtyKeyCode, KeyEvent as PtyKeyEvent, KeyEventKind as PtyKeyEventKind,
    Modifiers as PtyModifiers,
};
use stitch_pty::{Char as PtyCell, DrainEnd, HistoryScreen, PtySession, Winsize};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthChar;

use crate::server::osc::{OscMetadata, update_marks};

const UPDATE_QUEUE: usize = 256;
const READ_CHUNK: usize = 16 * 1024;
const DRAIN_LIMIT: Duration = Duration::from_secs(5);
/// Bounded so a child that stops reading cannot grow memory without limit.
const INPUT_QUEUE: usize = 256;
/// Default scrollback budget. A child can write forever without growing the
/// screen model past this many history rows.
const DEFAULT_SCROLLBACK: usize = 2000;

#[derive(Debug, Error)]
pub enum PaneError {
    #[error(transparent)]
    Pty(#[from] PtyErrorKind),
    #[error("invalid pane geometry {cols}x{rows}")]
    InvalidGeometry { cols: u16, rows: u16 },
}

#[derive(Debug, Error)]
pub enum PaneInputError {
    #[error("pane input queue is full")]
    Busy,
    #[error("pane input is closed")]
    Closed,
}

#[derive(Clone, Debug)]
pub struct PaneSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Option<Vec<(String, String)>>,
}

impl PaneSpec {
    pub fn new(
        program: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&[(String, String)]>,
    ) -> Self {
        Self {
            program: program.to_owned(),
            args: args.to_vec(),
            cwd: cwd.map(str::to_owned),
            env: env.map(|entries| entries.to_vec()),
        }
    }
}

#[derive(Clone, Debug)]
pub enum PaneUpdate {
    Snapshot(PaneSnapshot),
    Delta(PaneDelta),
    Exit(SessionExit),
}

struct PaneState {
    screen: HistoryScreen,
    seq: u64,
    program: String,
    last_activity_unix_ms: u64,
    /// Latest OSC 7501 record. Current state only, never a queue.
    program_status: Option<ProgramStatus>,
    /// Latest OSC 9/777 notification and OSC 9;4 progress. Current values only.
    notification: Option<Notification>,
    progress: Option<Progress>,
    /// Shell integration marks and cwd from OSC 133/1337. The scanner keeps
    /// only a bounded tail; no event history lives here.
    shell_cwd: Option<String>,
    shell_marks: ShellMarks,
    pending_metadata: Arc<Mutex<OscMetadata>>,
    /// Authoritative terminal state, recorded before any task cancellation.
    exit: Option<SessionExit>,
    exit_code: Option<i32>,
    /// Hash of each viewport row as it was last published. Comparing hashes
    /// avoids cloning the whole cell grid on every PTY read; only rows whose
    /// hash changed are converted into wire cells.
    last_row_hashes: Vec<u64>,
}

pub struct PaneRuntime {
    pub id: u64,
    pub session_id: u64,
    spec: PaneSpec,
    session: Arc<PtySession>,
    state: Mutex<PaneState>,
    updates: broadcast::Sender<Arc<PaneUpdate>>,
    /// Status records travel separately from cell updates so event subscribers
    /// cannot slow down the GUI's structured delta stream.
    status_tx: broadcast::Sender<ProgramStatus>,
    watching_events: AtomicBool,
    input_tx: mpsc::Sender<Vec<u8>>,
    dropped_input: Arc<AtomicU64>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// Cheap pane metadata: everything a picker, a layout title, or a session
/// listing needs, without cloning the screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneMeta {
    pub title: String,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

impl PaneRuntime {
    /// Spawn a child and start its reader, writer, and exit watcher tasks.
    pub async fn spawn(
        session_id: u64,
        id: u64,
        spec: &PaneSpec,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<Self>, PaneError> {
        if cols == 0 || rows == 0 {
            return Err(PaneError::InvalidGeometry { cols, rows });
        }
        let winsize = Winsize::new(rows, cols, 0, 0);
        let session = match &spec.env {
            None => {
                PtySession::spawn(
                    &spec.program,
                    &spec.args,
                    Some(winsize),
                    spec.cwd.as_deref(),
                )
                .await?
            }
            Some(entries) => {
                PtySession::spawn_with_env(
                    &spec.program,
                    &spec.args,
                    entries,
                    Some(winsize),
                    spec.cwd.as_deref(),
                )
                .await?
            }
        };
        let session = Arc::new(session);
        let mut screen = HistoryScreen::new(cols as usize, rows as usize, scrollback_budget());
        let (updates, _) = broadcast::channel(UPDATE_QUEUE);
        let (status_tx, _) = broadcast::channel(16);
        let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(INPUT_QUEUE);

        let dropped_input = Arc::new(AtomicU64::new(0));
        connect_terminal_replies(
            &mut screen,
            id,
            input_tx.clone(),
            Arc::clone(&dropped_input),
        );
        let pending_metadata = Arc::new(Mutex::new(OscMetadata::default()));
        let observer = Arc::clone(&pending_metadata);
        screen.set_osc_observer(move |event| {
            observer.lock().expect("OSC metadata").observe(event);
        });
        screen.set_hyperlink_tracking(true);

        let runtime = Arc::new(Self {
            id,
            session_id,
            session: Arc::clone(&session),
            spec: spec.clone(),
            state: Mutex::new(PaneState {
                last_row_hashes: screen.buffer().iter().map(|row| row_hash(row)).collect(),
                screen,
                seq: 0,
                program: spec.program.clone(),
                last_activity_unix_ms: now_unix_ms(),
                shell_cwd: None,
                shell_marks: ShellMarks::default(),
                pending_metadata,
                program_status: None,
                notification: None,
                progress: None,

                exit: None,
                exit_code: None,
            }),
            updates,
            status_tx,
            watching_events: AtomicBool::new(false),
            input_tx,
            dropped_input,
            tasks: Mutex::new(Vec::new()),
        });

        let writer_session = Arc::clone(&session);
        let writer = tokio::spawn(async move {
            while let Some(bytes) = input_rx.recv().await {
                if let Err(err) = writer_session.write_all(&bytes).await {
                    tracing::warn!("pane write failed: {err}");
                    break;
                }
            }
        });

        let writer_abort = writer.abort_handle();
        let reader_runtime = Arc::clone(&runtime);
        let reader = tokio::spawn(async move {
            reader_runtime.read_to_exit().await;
            writer_abort.abort();
        });

        let mut tasks = runtime.tasks.lock().expect("task vec");
        tasks.push(writer);
        tasks.push(reader);
        drop(tasks);
        Ok(runtime)
    }

    pub fn spec(&self) -> &PaneSpec {
        &self.spec
    }

    async fn read_to_exit(&self) {
        let mut buf = vec![0; READ_CHUNK];
        let wait = self.session.wait();
        tokio::pin!(wait);
        let status = loop {
            tokio::select! {
                biased;
                status = &mut wait => {
                    match self.session.drain_output(DRAIN_LIMIT, |chunk| self.ingest(chunk)).await {
                        Ok(DrainEnd::Eof | DrainEnd::Quiet) => {}
                        Ok(DrainEnd::Deadline) => {
                            tracing::warn!(pane_id = self.id, "pane output drain reached its deadline");
                        }
                        Err(err) => tracing::warn!(pane_id = self.id, "pane output drain failed: {err}"),
                    }
                    break status;
                }
                read = self.session.read(&mut buf) => match read {
                    Ok(0) => break wait.await,
                    Ok(n) => self.ingest(&buf[..n]),
                    Err(err) => {
                        tracing::warn!(pane_id = self.id, "pane read failed: {err}");
                        break wait.await;
                    }
                }
            }
        };
        let code = status.map(|status| status.exit_code().min(i32::MAX as u32) as i32);
        let reason = match code {
            Some(code) => format!("exited with {code}"),
            None => "exit status unavailable".to_owned(),
        };
        self.record_exit(code, reason);
    }

    fn ingest(&self, bytes: &[u8]) {
        let mut state = self.lock_state();
        let history_before = state.screen.history_size() as u64;
        #[cfg(feature = "bench-profile")]
        let feed_start = bench_profile::begin();
        state.screen.feed(bytes);
        #[cfg(feature = "bench-profile")]
        bench_profile::record(&bench_profile::PANE_FEED_NS, feed_start);
        let metadata = std::mem::take(&mut *state.pending_metadata.lock().expect("OSC metadata"));
        // Scrolling can shift clean rows. Compare the authoritative cells,
        // including color and hyperlink data, against the previous viewport.
        state.screen.take_dirty_rows();
        let events = state.screen.take_events();
        let evicted = history_before
            .saturating_add(scrollback_growth(&events))
            .saturating_sub(state.screen.history_size() as u64);
        let total = state.screen.total_lines() as u64;
        update_marks(
            &mut state.shell_marks,
            metadata.marks,
            history_before,
            evicted,
            total,
        );

        if let Some(status) = metadata.status {
            state.program_status = Some(status.clone());
            let _ = self.status_tx.send(status);
        }
        if metadata.cwd.is_some() {
            state.shell_cwd = metadata.cwd;
        }
        if metadata.notification.is_some() {
            state.notification = metadata.notification;
        }
        if metadata.progress.is_some() {
            state.progress = metadata.progress;
        }

        state.seq = state.seq.saturating_add(1);
        state.last_activity_unix_ms = now_unix_ms();
        #[cfg(feature = "bench-profile")]
        let diff_start = bench_profile::begin();
        let PaneState {
            last_row_hashes,
            screen,
            ..
        } = &mut *state;
        let rows = changed_rows_in_place(last_row_hashes, screen.buffer());
        #[cfg(feature = "bench-profile")]
        {
            bench_profile::record(&bench_profile::PANE_DIFF_NS, diff_start);
            bench_profile::add(&bench_profile::PANE_ROWS_CHANGED, rows.len() as u64);
        }
        let delta = common_fields(&state, self.id, state.seq);
        let bell = events.contains(&TermEvent::Bell);
        self.publish(PaneUpdate::Delta(PaneDelta {
            rows,
            bell,
            ..delta
        }));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<PaneUpdate>> {
        self.updates.subscribe()
    }

    /// Terminal exit recorded by the daemon, if the child has already exited.
    pub fn exit_snapshot(&self) -> Option<(SessionExit, Option<i32>)> {
        let state = self.lock_state();
        state.exit.clone().map(|exit| (exit, state.exit_code))
    }

    pub(crate) fn start_watching_events(&self) -> bool {
        !self.watching_events.swap(true, Ordering::Relaxed)
    }

    pub fn subscribe_status(&self) -> broadcast::Receiver<ProgramStatus> {
        self.status_tx.subscribe()
    }

    pub fn pid(&self) -> u32 {
        self.session.pid()
    }

    pub fn is_alive(&self) -> bool {
        self.session.is_alive()
    }

    pub fn write(&self, bytes: Vec<u8>) -> Result<(), PaneInputError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if !self.session.is_alive() {
            return Err(PaneInputError::Closed);
        }
        enqueue_input(self.id, &self.input_tx, &self.dropped_input, bytes)
    }

    /// Number of input chunks dropped because the child stopped reading.
    pub fn dropped_input_chunks(&self) -> u64 {
        self.dropped_input.load(Ordering::Relaxed)
    }

    /// Encode a frontend key event with the screen's active Kitty flags.
    pub fn encode_key(&self, key: &crate::protocol::KeyEvent) -> Vec<u8> {
        use crate::protocol::KeyCode as WireKeyCode;
        use crate::protocol::KeyEventKind as WireKeyEventKind;

        let code = match key.code {
            WireKeyCode::Char(value) => PtyKeyCode::char(value),
            WireKeyCode::Enter => PtyKeyCode::ENTER,
            WireKeyCode::Tab => PtyKeyCode::TAB,
            WireKeyCode::Backspace => PtyKeyCode::BACKSPACE,
            WireKeyCode::Esc => PtyKeyCode::ESCAPE,
            WireKeyCode::Up => PtyKeyCode::UP,
            WireKeyCode::Down => PtyKeyCode::DOWN,
            WireKeyCode::Left => PtyKeyCode::LEFT,
            WireKeyCode::Right => PtyKeyCode::RIGHT,
            WireKeyCode::Home => PtyKeyCode::HOME,
            WireKeyCode::End => PtyKeyCode::END,
            WireKeyCode::PageUp => PtyKeyCode::PAGE_UP,
            WireKeyCode::PageDown => PtyKeyCode::PAGE_DOWN,
            WireKeyCode::Insert => PtyKeyCode::INSERT,
            WireKeyCode::Delete => PtyKeyCode::DELETE,
            WireKeyCode::F(number) => PtyKeyCode::function(number),
            WireKeyCode::Null => PtyKeyCode::from_u32(0),
        };
        let kind = match key.kind {
            WireKeyEventKind::Press => PtyKeyEventKind::Press,
            WireKeyEventKind::Repeat => PtyKeyEventKind::Repeat,
            WireKeyEventKind::Release => PtyKeyEventKind::Release,
        };
        let mut event =
            PtyKeyEvent::new(code, PtyModifiers::from_bits_truncate(key.modifiers)).with_kind(kind);
        if let Some(text) = &key.text {
            event = event.with_text(text.clone());
        }
        let state = self.lock_state();
        state.screen.encode_key(&event)
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        if let Err(err) = self.session.resize(rows, cols) {
            tracing::warn!("pane resize failed: {err}");
        }
        let mut state = self.lock_state();
        if state.screen.columns() != usize::from(cols)
            || (state.screen.alt_screen() && state.screen.lines() != usize::from(rows))
        {
            // A reflow or resize of the parked primary buffer has no mapping
            // from the old mark rows to the resized rows.
            state.shell_marks = ShellMarks::default();
        }
        let history_before = state.screen.history_size() as u64;
        state.screen.resize(rows as usize, cols as usize);
        let events = state.screen.take_events();
        let evicted = history_before
            .saturating_add(scrollback_growth(&events))
            .saturating_sub(state.screen.history_size() as u64);
        let total = state.screen.total_lines() as u64;
        update_marks(
            &mut state.shell_marks,
            ShellMarks::default(),
            history_before,
            evicted,
            total,
        );
        let PaneState {
            last_row_hashes,
            screen,
            ..
        } = &mut *state;
        last_row_hashes.clear();
        last_row_hashes.extend(screen.buffer().iter().map(|row| row_hash(row)));
        let seq = state.seq.saturating_add(1);
        state.seq = seq;
        let snapshot = snapshot_from(&state, self.id, seq);
        self.publish(PaneUpdate::Snapshot(snapshot));
    }

    pub fn snapshot(&self) -> PaneSnapshot {
        let state = self.lock_state();
        snapshot_from(&state, self.id, state.seq)
    }

    /// Cheap metadata for session listings and layout titles.
    ///
    /// `snapshot()` clones every visible cell; a picker or a layout broadcast
    /// only needs these fields, and a busy pane used to burn that copy on
    /// every layout revision.
    pub fn meta(&self) -> PaneMeta {
        let state = self.lock_state();
        PaneMeta {
            title: state.screen.title().to_owned(),
            cwd: metadata_cwd(&state),
            cols: state.screen.columns().min(u16::MAX as usize) as u16,
            rows: state.screen.lines().min(u16::MAX as usize) as u16,
        }
    }

    pub fn title(&self) -> String {
        self.lock_state().screen.title().to_owned()
    }

    /// Last time the child produced output, used by the session picker.
    pub fn last_activity_unix_ms(&self) -> u64 {
        self.lock_state().last_activity_unix_ms
    }

    /// Current OSC 7501 record, without cloning the screen.
    pub fn program_status(&self) -> Option<ProgramStatus> {
        self.lock_state().program_status.clone()
    }

    pub fn capture(&self, start: u64, lines: u32, format: CaptureFormat) -> CaptureResponse {
        let state = self.lock_state();
        let start = start as usize;
        let count = (lines as usize).min(1000);
        let total_lines = state.screen.total_lines() as u64;
        let end = start.saturating_add(count).min(state.screen.total_lines());
        let rows: Vec<CapturedRow> = (start..end)
            .filter_map(|index| state.screen.row_cells(index))
            .map(|line| CapturedRow {
                cells: cells_from_row(line),
                wrapped: false,
            })
            .collect();
        let (text, html) = match format {
            CaptureFormat::Text => (Some(plain_text(&rows)), None),
            CaptureFormat::Vt => (Some(vt_text(&rows)), None),
            CaptureFormat::Html => (None, Some(html_text(&rows))),
            CaptureFormat::Cells | CaptureFormat::Json => (None, None),
        };
        CaptureResponse {
            pane_id: self.id,
            start: start as u64,
            total_lines,
            format,
            rows,
            text,
            html,
        }
    }

    pub async fn kill(&self) {
        let _ = self.session.terminate(Duration::from_millis(250)).await;
        self.record_exit(None, "killed".to_owned());
        let mut tasks = self.tasks.lock().expect("task vec");
        for task in tasks.drain(..) {
            task.abort();
        }
    }

    fn record_exit(&self, code: Option<i32>, reason: String) {
        let exit = SessionExit {
            session_id: self.session_id,
            pane_id: Some(self.id),
            reason,
        };
        let mut state = self.lock_state();
        if state.exit.is_some() {
            return;
        }
        state.exit = Some(exit.clone());
        state.exit_code = code;
        drop(state);
        self.publish(PaneUpdate::Exit(exit));
    }

    fn publish(&self, update: PaneUpdate) {
        // No receivers is normal for detached sessions; keep the state anyway.
        #[cfg(feature = "bench-profile")]
        let publish_start = bench_profile::begin();
        let _ = self.updates.send(Arc::new(update));
        #[cfg(feature = "bench-profile")]
        {
            bench_profile::record(&bench_profile::PANE_PUBLISH_NS, publish_start);
            bench_profile::add(&bench_profile::PANE_INGESTS, 1);
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, PaneState> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

fn common_fields(state: &PaneState, pane_id: u64, seq: u64) -> PaneDelta {
    let screen = &state.screen;
    let cursor = screen.cursor();
    let mode = screen.mode();
    PaneDelta {
        pane_id,
        seq,
        rows: Vec::new(),
        cursor: Cursor {
            x: cursor.x.min(u16::MAX as usize) as u16,
            y: cursor.y.min(u16::MAX as usize) as u16,
            visible: !cursor.hidden,
            shape: match screen.cursor_shape() {
                "bar" => CursorShape::Bar,
                "underline" => CursorShape::Underline,
                _ => CursorShape::Block,
            },
            blink: screen.cursor_blink(),
        },
        modes: Modes {
            alt_screen: screen.alt_screen(),
            app_cursor: mode.has_private(DECCKM),
            bracketed_paste: mode.has_private(BRACKETED_PASTE),
            mouse_proto: mode.mouse_protocol(),
            sgr_mouse: mode.sgr_mouse(),
            cursor_visible: !cursor.hidden,
            keyboard_flags: screen.keyboard_mode(),
        },
        title: screen.title().to_owned(),
        cwd: metadata_cwd(state),
        foreground: Some(ProcessInfo {
            pid: None,
            name: state.program.clone(),
            cwd: metadata_cwd(state),
        }),
        shell_marks: metadata_marks(state),
        program_status: state.program_status.clone(),
        notification: state.notification.clone(),
        progress: state.progress.clone(),
        alt_screen: screen.alt_screen(),
        scrollback_len: screen.history_size() as u64,
        total_lines: screen.total_lines() as u64,
        scrollback_bytes: (screen.total_lines() * screen.columns() * 4) as u64,
        bell: false,
    }
}

fn snapshot_from(state: &PaneState, pane_id: u64, seq: u64) -> PaneSnapshot {
    let screen = &state.screen;
    let cells = screen
        .buffer()
        .iter()
        .map(|line| Row {
            cells: cells_from_row(line),
            wrapped: false,
        })
        .collect();
    let delta = common_fields(state, pane_id, seq);
    PaneSnapshot {
        pane_id,
        seq,
        cols: screen.columns().min(u16::MAX as usize) as u16,
        rows: screen.lines().min(u16::MAX as usize) as u16,
        cells,
        cursor: delta.cursor,
        modes: delta.modes,
        title: delta.title,
        cwd: delta.cwd,
        foreground: delta.foreground,
        shell_marks: delta.shell_marks,
        program_status: delta.program_status,
        notification: delta.notification,
        progress: delta.progress,
        alt_screen: delta.alt_screen,
        scrollback_len: delta.scrollback_len,
        total_lines: delta.total_lines,
        scrollback_bytes: delta.scrollback_bytes,
    }
}

fn metadata_cwd(state: &PaneState) -> Option<String> {
    state
        .screen
        .cwd()
        .or(state.shell_cwd.as_deref())
        .map(str::to_owned)
}

fn metadata_marks(state: &PaneState) -> Option<ShellMarks> {
    let marks = &state.shell_marks;
    (!state.screen.alt_screen()
        && (!marks.prompt_rows.is_empty() || !marks.command_rows.is_empty()))
    .then(|| marks.clone())
}

fn scrollback_growth(events: &[TermEvent]) -> u64 {
    events
        .iter()
        .find_map(|event| match event {
            TermEvent::ScrollbackGrew(count) => Some(*count),
            _ => None,
        })
        .unwrap_or_default()
}

fn connect_terminal_replies(
    screen: &mut HistoryScreen,
    pane_id: u64,
    sender: mpsc::Sender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
) {
    screen.set_write_process_input(move |response| {
        let _ = enqueue_input(pane_id, &sender, &dropped, response.as_bytes().to_vec());
    });
}

fn enqueue_input(
    pane_id: u64,
    sender: &mpsc::Sender<Vec<u8>>,
    dropped: &AtomicU64,
    bytes: Vec<u8>,
) -> Result<(), PaneInputError> {
    match sender.try_send(bytes) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => {
            if dropped.fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::warn!(
                    pane_id,
                    "pane input queue is full; dropping input until the child reads"
                );
            }
            Err(PaneInputError::Busy)
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err(PaneInputError::Closed),
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Rows whose styled content differs from the previously published viewport.
///
/// The parser's dirty-row set only names rows that were written directly, so a
/// scroll (which shifts every row up and only marks the new bottom row) would
/// leave the client with stale rows. Diffing the viewport is the only way to
/// keep a cell-delta client correct.
fn row_hash(line: &[PtyCell]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for cell in line {
        cell.data.hash(&mut hasher);
        cell.fg.hash(&mut hasher);
        cell.bg.hash(&mut hasher);
        cell.bold.hash(&mut hasher);
        cell.dim.hash(&mut hasher);
        cell.italics.hash(&mut hasher);
        cell.underscore.hash(&mut hasher);
        cell.blink.hash(&mut hasher);
        cell.reverse.hash(&mut hasher);
        cell.hidden.hash(&mut hasher);
        cell.strikethrough.hash(&mut hasher);
        cell.hyperlink.as_deref().hash(&mut hasher);
    }
    hasher.finish()
}

fn changed_rows_at(
    previous: &mut [u64],
    visible: &[Vec<PtyCell>],
    candidates: impl Iterator<Item = usize>,
) -> Vec<RowUpdate> {
    let mut rows = Vec::new();
    for index in candidates {
        let Some(line) = visible.get(index) else {
            continue;
        };
        let Some(slot) = previous.get_mut(index) else {
            continue;
        };
        let hash = row_hash(line);
        if *slot == hash {
            continue;
        }
        *slot = hash;
        rows.push(RowUpdate {
            row: index as u16,
            cells: cells_from_row(line),
            wrapped: false,
        });
    }
    rows
}

/// Recomputes every visible row hash in place and reports changed rows.
///
/// This is deliberately a full recomputation. Previous attempts at partial,
/// scroll-aware hashing broke the screen oracle and stay reverted.
fn changed_rows_in_place(previous: &mut Vec<u64>, visible: &[Vec<PtyCell>]) -> Vec<RowUpdate> {
    if previous.len() < visible.len() {
        previous.resize(visible.len(), 0);
    }
    let rows = changed_rows_at(previous, visible, 0..visible.len());
    previous.truncate(visible.len());
    rows
}

fn scrollback_budget() -> usize {
    let configured = std::env::var("STITCH_MUX_SCROLLBACK")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SCROLLBACK);
    configured.clamp(100, 20_000)
}

fn plain_text(rows: &[CapturedRow]) -> String {
    rows.iter()
        .map(|row| {
            row.cells
                .iter()
                .filter(|cell| !cell.is_continuation())
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sgr_style(cell: &Cell) -> String {
    let mut codes = vec![0_u16];
    push_sgr_color(&mut codes, 38, cell.fg);
    push_sgr_color(&mut codes, 48, cell.bg);
    for (flag, code) in [
        (Attrs::BOLD, 1),
        (Attrs::DIM, 2),
        (Attrs::ITALIC, 3),
        (Attrs::UNDERLINE, 4),
        (Attrs::BLINK, 5),
        (Attrs::REVERSE, 7),
        (Attrs::HIDDEN, 8),
        (Attrs::STRIKETHROUGH, 9),
    ] {
        if cell.attrs.contains(flag) {
            codes.push(code);
        }
    }
    format!(
        "\x1b[{}m",
        codes
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(";")
    )
}

fn push_sgr_color(codes: &mut Vec<u16>, base: u16, color: Color) {
    match color {
        Color::Default => {}
        Color::Indexed(value) => codes.extend([base, 5, value.into()]),
        Color::Rgb(red, green, blue) => {
            codes.extend([base, 2, red.into(), green.into(), blue.into()]);
        }
    }
}

fn vt_text(rows: &[CapturedRow]) -> String {
    let mut out = String::new();
    let mut current = String::new();
    for (y, row) in rows.iter().enumerate() {
        if y > 0 {
            out.push('\n');
        }
        for cell in row.cells.iter().filter(|cell| !cell.is_continuation()) {
            let style = sgr_style(cell);
            if style != current {
                out.push_str(&style);
                current = style;
            }
            out.push_str(if cell.text.is_empty() {
                " "
            } else {
                &cell.text
            });
        }
    }
    if current != "\x1b[0m" {
        out.push_str("\x1b[0m");
    }
    out
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(character),
        }
    }
    out
}

fn css_color(color: Color) -> Option<String> {
    match color {
        Color::Default => None,
        Color::Indexed(index) => {
            const PALETTE: &[&str] = &[
                "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080",
                "#c0c0c0", "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff",
                "#00ffff", "#ffffff",
            ];
            Some(
                PALETTE
                    .get(index as usize)
                    .copied()
                    .unwrap_or("#ffffff")
                    .to_owned(),
            )
        }
        Color::Rgb(red, green, blue) => Some(format!("#{red:02x}{green:02x}{blue:02x}")),
    }
}

fn cell_style(cell: &Cell) -> String {
    let mut styles = Vec::new();
    if let Some(color) = css_color(cell.fg) {
        styles.push(format!("color:{color}"));
    }
    if let Some(color) = css_color(cell.bg) {
        styles.push(format!("background-color:{color}"));
    }
    if cell.attrs.contains(Attrs::BOLD) {
        styles.push("font-weight:bold".into());
    }
    if cell.attrs.contains(Attrs::ITALIC) {
        styles.push("font-style:italic".into());
    }
    if cell.attrs.contains(Attrs::UNDERLINE) {
        styles.push("text-decoration:underline".into());
    }
    if cell.attrs.contains(Attrs::STRIKETHROUGH) {
        styles.push("text-decoration:line-through".into());
    }
    if cell.attrs.contains(Attrs::REVERSE) {
        styles.push("filter:invert(1)".into());
    }
    if styles.is_empty() {
        String::new()
    } else {
        format!(" style=\"{}\"", styles.join(";"))
    }
}

fn html_text(rows: &[CapturedRow]) -> String {
    let mut out = String::from("<pre class=\"stitch-mux-capture\">");
    for row in rows {
        out.push_str("<span class=\"row\">");
        for cell in row.cells.iter().filter(|cell| !cell.is_continuation()) {
            out.push_str("<span");
            out.push_str(&cell_style(cell));
            out.push('>');
            out.push_str(&escape_html(if cell.text.is_empty() {
                " "
            } else {
                &cell.text
            }));
            out.push_str("</span>");
        }
        out.push_str("</span>\n");
    }
    out.push_str("</pre>");
    out
}

fn cells_from_row(line: &[PtyCell]) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(line.len());
    let mut previous_wide = false;
    let mut previous_link = None;
    for cell in line {
        let width = cell
            .data
            .chars()
            .next()
            .and_then(UnicodeWidthChar::width)
            .unwrap_or(1);
        let continuation = previous_wide && (cell.data.is_empty() || cell.data == " ");
        previous_wide = width >= 2;
        let link = if continuation {
            previous_link
        } else {
            cell.hyperlink.as_deref()
        };
        previous_link = link;
        let mut attrs = Attrs::empty();
        for (enabled, attr) in [
            (cell.bold, Attrs::BOLD),
            (cell.dim, Attrs::DIM),
            (cell.italics, Attrs::ITALIC),
            (cell.underscore, Attrs::UNDERLINE),
            (cell.blink, Attrs::BLINK),
            (cell.reverse, Attrs::REVERSE),
            (cell.hidden, Attrs::HIDDEN),
            (cell.strikethrough, Attrs::STRIKETHROUGH),
        ] {
            if enabled {
                attrs.insert(attr);
            }
        }
        cells.push(Cell {
            text: CellText::new(&cell.data),
            fg: parse_color(&cell.fg),
            bg: parse_color(&cell.bg),
            attrs,
            width: if continuation { 0 } else { width.min(2) as u8 },
            link: link.map(str::to_owned),
        });
    }
    cells
}

/// `stitch-pty` stores SGR colors as names for the 16 ANSI colors and as
/// `rrggbb` for everything else. Keep the 16 names as indexed colors so
/// structured frontends and the ANSI projection stay colored.
fn parse_color(value: &str) -> Color {
    match value {
        "default" | "" => return Color::Default,
        "black" => return Color::Indexed(0),
        "red" => return Color::Indexed(1),
        "green" => return Color::Indexed(2),
        "brown" => return Color::Indexed(3),
        "blue" => return Color::Indexed(4),
        "magenta" => return Color::Indexed(5),
        "cyan" => return Color::Indexed(6),
        "white" => return Color::Indexed(7),
        "brightblack" => return Color::Indexed(8),
        "brightred" => return Color::Indexed(9),
        "brightgreen" => return Color::Indexed(10),
        "brightbrown" => return Color::Indexed(11),
        "brightblue" => return Color::Indexed(12),
        "brightmagenta" => return Color::Indexed(13),
        "brightcyan" => return Color::Indexed(14),
        "brightwhite" => return Color::Indexed(15),
        _ => {}
    }
    if value.len() == 6
        && let Ok(rgb) = u32::from_str_radix(value, 16)
    {
        return Color::Rgb(
            ((rgb >> 16) & 0xff) as u8,
            ((rgb >> 8) & 0xff) as u8,
            (rgb & 0xff) as u8,
        );
    }
    Color::Default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ansi_colors() {
        assert_eq!(parse_color("default"), Color::Default);
        assert_eq!(parse_color("ff8000"), Color::Rgb(255, 128, 0));
        assert_eq!(parse_color("white"), Color::Indexed(7));
        assert_eq!(parse_color("red"), Color::Indexed(1));
        assert_eq!(parse_color("brightcyan"), Color::Indexed(14));
        assert_eq!(parse_color("not-a-color"), Color::Default);
    }

    #[test]
    fn scrolled_rows_are_reported_even_though_the_parser_only_marks_the_bottom() {
        let row = |text: &str| vec![PtyCell::new(text)];
        let previous = [row("a"), row("b"), row("c"), row("d")];
        // One line scrolled off: the parser reports only row 3 as dirty.
        let visible = [row("b"), row("c"), row("d"), row("e")];
        let previous_hashes = previous.iter().map(|row| row_hash(row)).collect::<Vec<_>>();
        let mut visible_hashes = previous_hashes.clone();
        let updates = changed_rows_in_place(&mut visible_hashes, &visible);
        assert_eq!(
            updates.iter().map(|update| update.row).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(updates[0].cells[0].text, "b");
        assert_eq!(updates[3].cells[0].text, "e");

        // A no-op repaint reports nothing.
        assert!(changed_rows_in_place(&mut visible_hashes, &visible).is_empty());

        // A viewport of a different height does not panic and reports the tail.
        let shorter = vec![row("c"), row("d")];
        let mut shorter_hashes = previous_hashes;
        let updates = changed_rows_in_place(&mut shorter_hashes, &shorter);
        assert_eq!(updates.len(), 2);
    }

    #[test]
    fn in_place_row_hashes_match_copying_reference() {
        let row = |text: &str| vec![PtyCell::new(text)];
        let scenes = [
            vec![row("a"), row("b")],
            vec![row("a"), row("b"), row("c")],
            vec![row("x"), row("b")],
        ];
        let initial = vec![0_u64; 3];
        for visible in &scenes {
            let mut expected_hashes = initial.clone();
            let expected = changed_rows_at(&mut expected_hashes, visible, 0..visible.len());
            if expected_hashes.len() > visible.len() {
                expected_hashes.truncate(visible.len());
            }

            let mut actual_hashes = initial.clone();
            let actual = changed_rows_in_place(&mut actual_hashes, visible);

            assert_eq!(actual, expected);
            assert_eq!(actual_hashes, expected_hashes);
        }
    }

    #[test]
    fn terminal_replies_join_the_pane_writer_queue_at_dispatch() {
        let mut screen = HistoryScreen::new(20, 10, 20);
        let (sender, mut writer) = mpsc::channel(8);
        connect_terminal_replies(&mut screen, 1, sender, Arc::new(AtomicU64::new(0)));
        screen.feed(b"\x1b[c\x1b[>c\x1b[5n\x1b[3;5H\x1b[6");
        screen.feed(b"n\x1b[?6n\x1b[1;1H\x1b[>3u\x1b[?u\x1b[<u");
        for expected in [
            &b"\x1b[?6c"[..],
            b"\x1b[>0;276;0c",
            b"\x1b[0n",
            b"\x1b[3;5R",
            b"\x1b[?3;5R",
            b"\x1b[?3u",
        ] {
            assert_eq!(writer.try_recv().unwrap(), expected);
        }
        assert!(writer.try_recv().is_err());
    }

    #[tokio::test]
    async fn input_queue_distinguishes_full_and_closed() {
        let shell = if cfg!(windows) { "cmd.exe" } else { "sh" };
        let pane = PaneRuntime::spawn(1, 1, &PaneSpec::new(shell, &[], None, None), 80, 24)
            .await
            .expect("spawn pane");
        // Fill the queue before yielding to the writer on this runtime.
        for _ in 0..INPUT_QUEUE {
            pane.write(vec![b'x']).expect("queue input");
        }
        assert!(matches!(pane.write(vec![b'x']), Err(PaneInputError::Busy)));
        pane.kill().await;
        assert!(matches!(
            pane.write(vec![b'x']),
            Err(PaneInputError::Closed)
        ));
    }

    #[tokio::test]
    async fn osc_marks_follow_wrapping_and_retained_scrollback() {
        let shell = if cfg!(windows) { "cmd.exe" } else { "sh" };
        let pane = PaneRuntime::spawn(1, 1, &PaneSpec::new(shell, &[], None, None), 8, 2)
            .await
            .expect("spawn pane");
        pane.lock_state().screen.set_scrollback_lines(2);

        // The ninth character soft-wraps to row 1. The split OSC starts there.
        pane.ingest(b"\x1b]133;A\x07abcdefghx\x1b]133;");
        pane.ingest(b"A\x1b\\\r\none\r\ntwo\r\nthree\x1b]133;A\x07");
        let snapshot = pane.snapshot();
        assert_eq!(snapshot.scrollback_len, 2);
        assert_eq!(snapshot.shell_marks.unwrap().prompt_rows, [0, 3]);

        pane.ingest(b"\x1b[?1049h\x1b]133;A\x07alternate");
        assert!(pane.snapshot().shell_marks.is_none());
        pane.ingest(b"\x1b[?1049l");
        assert_eq!(pane.snapshot().shell_marks.unwrap().prompt_rows, [0, 3]);
        pane.kill().await;
    }

    #[tokio::test]
    async fn hyperlink_only_changes_reach_deltas_and_links_survive_resize() {
        let shell = if cfg!(windows) { "cmd.exe" } else { "sh" };
        let pane = PaneRuntime::spawn(1, 1, &PaneSpec::new(shell, &[], None, None), 12, 3)
            .await
            .expect("spawn pane");
        let mut updates = pane.subscribe();
        pane.ingest(b"AB");
        let _ = updates.try_recv().unwrap();

        // Redraw the same text with a link and a foreground color change.
        pane.ingest(b"\r\x1b[31m\x1b]8;;https://example.test\x07AB\x1b]8;;\x07");
        let update = updates.try_recv().unwrap();
        let PaneUpdate::Delta(delta) = update.as_ref() else {
            panic!("expected a row delta");
        };
        assert_eq!(delta.rows.len(), 1);
        let cells = &delta.rows[0].cells;
        assert_eq!(cells[0].link.as_deref(), Some("https://example.test"));
        assert_eq!(cells[0].fg, Color::Indexed(1));

        // A link-only overwrite must update the row even with identical SGR.
        pane.ingest(b"\rA");
        let update = updates.try_recv().unwrap();
        let PaneUpdate::Delta(delta) = update.as_ref() else {
            panic!("expected a row delta");
        };
        assert!(delta.rows[0].cells[0].link.is_none());

        pane.resize(20, 3);
        let capture = pane.capture(0, 3, CaptureFormat::Cells);
        let linked: Vec<_> = capture
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .filter(|cell| cell.link.is_some())
            .map(|cell| cell.text.as_str())
            .collect();
        assert_eq!(linked, ["B"]);
        pane.kill().await;
    }

    #[test]
    fn marks_wide_character_continuation() {
        let line = [
            PtyCell::new("\u{6f22}"),
            PtyCell::blank(),
            PtyCell::new("a"),
        ];
        let cells = cells_from_row(&line);
        assert_eq!(cells[0].width, 2);
        assert_eq!(cells[1].width, 0);
        assert_eq!(cells[2].width, 1);
    }
}
