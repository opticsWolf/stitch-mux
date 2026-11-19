//! One PTY plus its authoritative screen model.
//!
//! `PtySession` is transport-only. The screen, keyboard flags, scrollback and
//! structured snapshot/delta stream belong here, not in a frontend.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use stitch_mux_core::{Attrs, CaptureFormat, Cell, Color, Cursor, CursorShape, Modes, Row};
use stitch_mux_protocol::{
    CaptureResponse, CapturedRow, PaneDelta, PaneSnapshot, ProcessInfo, RowUpdate, SessionExit,
};
use stitch_pty::errors::PtyErrorKind;
use stitch_pty::terminal::events::TermEvent;
use stitch_pty::terminal::modes::{BRACKETED_PASTE, DECCKM};
use stitch_pty::terminal::{
    KeyCode as PtyKeyCode, KeyEvent as PtyKeyEvent, KeyEventKind as PtyKeyEventKind,
    Modifiers as PtyModifiers,
};
use stitch_pty::{HistoryScreen, PtySession, Winsize};
use thiserror::Error;
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthChar;

const UPDATE_QUEUE: usize = 256;
const READ_CHUNK: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum PaneError {
    #[error(transparent)]
    Pty(#[from] PtyErrorKind),
    #[error("invalid pane geometry {cols}x{rows}")]
    InvalidGeometry { cols: u16, rows: u16 },
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
    bell: bool,
    last_activity_unix_ms: u64,
    queries: QueryScanner,
}

/// Minimal terminal-query responder.
///
/// `stitch-pty` owns the screen but its `HistoryScreen` response callback is
/// not settable through the published API, so DA/DSR requests would otherwise
/// go unanswered and leave programs waiting. Scan the PTY byte stream for the
/// queries the server must answer and write the replies back to the child.
/// Remove this once `stitch-pty` exposes a response sink.
#[derive(Default)]
struct QueryScanner {
    pending: Vec<u8>,
}

impl QueryScanner {
    fn scan(&mut self, bytes: &[u8], cursor: (usize, usize)) -> Vec<Vec<u8>> {
        const MAX_PENDING: usize = 64;
        const ESC: u8 = 0x1b;
        const FINAL_MIN: u8 = 0x40;
        const FINAL_MAX: u8 = 0x7e;

        self.pending.extend_from_slice(bytes);
        let mut responses = Vec::new();
        let mut consumed = 0;
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i] != ESC {
                i += 1;
                continue;
            }
            if i + 1 >= self.pending.len() {
                break;
            }
            if self.pending[i + 1] != b'[' {
                i += 1;
                continue;
            }
            let mut end = i + 2;
            while end < self.pending.len() && !(FINAL_MIN..=FINAL_MAX).contains(&self.pending[end])
            {
                end += 1;
            }
            if end == self.pending.len() {
                break;
            }
            let params = &self.pending[i + 2..end];
            let final_byte = self.pending[end];
            if let Some(response) = query_response(params, final_byte, cursor) {
                responses.push(response);
            }
            i = end + 1;
            consumed = i;
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        if self.pending.len() > MAX_PENDING {
            self.pending.clear();
        }
        responses
    }
}

fn query_response(params: &[u8], final_byte: u8, cursor: (usize, usize)) -> Option<Vec<u8>> {
    let private = params.first() == Some(&b'?');
    let secondary = params.first() == Some(&b'>');
    let body = params
        .strip_prefix(b"?")
        .or_else(|| params.strip_prefix(b">"))
        .unwrap_or(params);
    match final_byte {
        b'c' if secondary => Some(b"\x1b[>0;276;0c".to_vec()),
        b'c' if body.is_empty() || body == b"0" => Some(b"\x1b[?6c".to_vec()),
        b'n' if body == b"5" && !private && !secondary => Some(b"\x1b[0n".to_vec()),
        b'n' if body == b"6" => {
            let (row, col) = cursor;
            if private {
                Some(format!("\x1b[?{};{}R", row + 1, col + 1).into_bytes())
            } else {
                Some(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes())
            }
        }
        _ => None,
    }
}

pub struct PaneRuntime {
    pub id: u64,
    pub session_id: u64,
    spec: PaneSpec,
    session: Arc<PtySession>,
    state: Mutex<PaneState>,
    updates: broadcast::Sender<Arc<PaneUpdate>>,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    reader_done: Arc<Notify>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
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
        let screen = HistoryScreen::new(cols as usize, rows as usize, 2000);
        let (updates, _) = broadcast::channel(UPDATE_QUEUE);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let reader_done = Arc::new(Notify::new());

        let runtime = Arc::new(Self {
            id,
            session_id,
            session: Arc::clone(&session),
            spec: spec.clone(),
            state: Mutex::new(PaneState {
                screen,
                seq: 0,
                program: spec.program.clone(),
                bell: false,
                last_activity_unix_ms: now_unix_ms(),
                queries: QueryScanner::default(),
            }),
            updates,
            input_tx,
            reader_done: Arc::clone(&reader_done),
            tasks: Mutex::new(Vec::new()),
        });

        let writer_session = Arc::clone(&session);
        let writer = tokio::spawn(async move {
            while let Some(bytes) = input_rx.recv().await {
                if let Err(err) = writer_session.write_all(&bytes).await {
                    tracing::warn!("pane write failed: {err}");
                }
            }
        });

        let reader_runtime = Arc::clone(&runtime);
        let reader_notify = Arc::clone(&reader_done);
        let reader = tokio::spawn(async move {
            let mut buf = vec![0_u8; READ_CHUNK];
            loop {
                match reader_runtime.session.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let (dirty, events, responses) = {
                            let mut state = reader_runtime.lock_state();
                            state.screen.feed(&buf[..n]);
                            let cursor = {
                                let cursor = state.screen.cursor();
                                (cursor.y, cursor.x)
                            };
                            let responses = state.queries.scan(&buf[..n], cursor);
                            (
                                state.screen.take_dirty_rows(),
                                state.screen.take_events(),
                                responses,
                            )
                        };
                        for response in responses {
                            let _ = reader_runtime.input_tx.send(response);
                        }
                        reader_runtime.publish_delta(dirty, &events);
                    }
                    Err(err) => {
                        tracing::warn!("pane read failed: {err}");
                        break;
                    }
                }
            }
            reader_notify.notify_one();
        });

        let exit_runtime = Arc::clone(&runtime);
        let watcher = tokio::spawn(async move {
            let status = exit_runtime.session.wait().await;
            // The reader owns the PTY until EOF, so a fast child that exits
            // after a large burst cannot lose its final bytes to a fixed
            // drain sleep. Wait for that EOF, but do not hang the exit event
            // forever if a platform leaves the reader blocked.
            let _drained =
                tokio::time::timeout(Duration::from_secs(5), exit_runtime.reader_done.notified());
            exit_runtime.publish(PaneUpdate::Exit(SessionExit {
                session_id: exit_runtime.session_id,
                pane_id: Some(exit_runtime.id),
                reason: match status {
                    Some(status) => format!("exited with {}", status.exit_code()),
                    None => "exit status unavailable".to_owned(),
                },
            }));
        });

        let mut tasks = runtime.tasks.lock().expect("task vec");
        tasks.push(writer);
        tasks.push(reader);
        tasks.push(watcher);
        drop(tasks);
        Ok(runtime)
    }

    pub fn spec(&self) -> &PaneSpec {
        &self.spec
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<PaneUpdate>> {
        self.updates.subscribe()
    }

    pub fn pid(&self) -> u32 {
        self.session.pid()
    }

    pub fn is_alive(&self) -> bool {
        self.session.is_alive()
    }

    pub fn write(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.input_tx.send(bytes);
        }
    }

    /// Encode a frontend key event with the screen's active Kitty flags.
    pub fn encode_key(&self, key: &stitch_mux_protocol::KeyEvent) -> Vec<u8> {
        use stitch_mux_protocol::KeyCode as WireKeyCode;
        use stitch_mux_protocol::KeyEventKind as WireKeyEventKind;

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
        state.screen.resize(rows as usize, cols as usize);
        let seq = state.seq + 1;
        state.seq = seq;
        let snapshot = snapshot_from(&state, self.id, seq);
        drop(state);
        self.publish(PaneUpdate::Snapshot(snapshot));
    }

    pub fn snapshot(&self) -> PaneSnapshot {
        let state = self.lock_state();
        snapshot_from(&state, self.id, state.seq)
    }

    /// Last time the child produced output, used by the session picker.
    pub fn last_activity_unix_ms(&self) -> u64 {
        self.lock_state().last_activity_unix_ms
    }

    pub fn capture(&self, start: u64, lines: u32, format: CaptureFormat) -> CaptureResponse {
        let state = self.lock_state();
        let start = start as usize;
        let count = (lines as usize).min(1000);
        let tuples = state.screen.styled_range(start, count);
        let total_lines = state.screen.total_lines() as u64;
        let rows: Vec<CapturedRow> = tuples
            .iter()
            .map(|line| CapturedRow {
                cells: cells_from_row(line),
                wrapped: false,
            })
            .collect();
        let text = match format {
            CaptureFormat::Text | CaptureFormat::Vt | CaptureFormat::Html => Some(
                tuples
                    .iter()
                    .map(|line| {
                        line.iter()
                            .map(|(text, _, _, _)| text.as_str())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            CaptureFormat::Cells | CaptureFormat::Json => None,
        };
        CaptureResponse {
            pane_id: self.id,
            start: start as u64,
            total_lines,
            format,
            rows,
            text,
            html: None,
        }
    }

    pub async fn kill(&self) {
        let _ = self.session.terminate(Duration::from_millis(250)).await;
        let mut tasks = self.tasks.lock().expect("task vec");
        for task in tasks.drain(..) {
            task.abort();
        }
    }

    fn publish(&self, update: PaneUpdate) {
        // No receivers is normal for detached sessions; keep the state anyway.
        let _ = self.updates.send(Arc::new(update));
    }

    fn publish_delta(&self, dirty: Vec<usize>, events: &[TermEvent]) {
        let mut state = self.lock_state();
        state.seq += 1;
        state.last_activity_unix_ms = now_unix_ms();
        let seq = state.seq;
        let screen = &state.screen;
        let visible = screen.styled_range(screen.history_size(), screen.lines());
        let rows = dirty
            .into_iter()
            .filter_map(|index| {
                visible.get(index).map(|line| RowUpdate {
                    row: index as u16,
                    cells: cells_from_row(line),
                    wrapped: false,
                })
            })
            .collect();
        for event in events {
            // Title/cwd/alt-screen changes are already applied to the screen
            // by the parser and are read back below.
            if event == &TermEvent::Bell {
                state.bell = true;
            }
        }
        let bell = std::mem::take(&mut state.bell);
        let delta = common_fields(&state, self.id, seq);
        drop(state);
        self.publish(PaneUpdate::Delta(PaneDelta {
            rows,
            bell,
            ..delta
        }));
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
        cwd: screen.cwd().map(str::to_owned),
        foreground: Some(ProcessInfo {
            pid: None,
            name: state.program.clone(),
            cwd: screen.cwd().map(str::to_owned),
        }),
        shell_marks: None,
        program_status: None,
        alt_screen: screen.alt_screen(),
        scrollback_len: screen.history_size() as u64,
        scrollback_bytes: (screen.total_lines() * screen.columns() * 4) as u64,
        bell: false,
    }
}

fn snapshot_from(state: &PaneState, pane_id: u64, seq: u64) -> PaneSnapshot {
    let screen = &state.screen;
    let start = screen.history_size();
    let cells = screen
        .styled_range(start, screen.lines())
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
        alt_screen: delta.alt_screen,
        scrollback_len: delta.scrollback_len,
        scrollback_bytes: delta.scrollback_bytes,
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn cells_from_row(line: &[(String, String, String, u8)]) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(line.len());
    let mut previous_wide = false;
    for (text, fg, bg, attrs) in line {
        let width = text
            .chars()
            .next()
            .and_then(UnicodeWidthChar::width)
            .unwrap_or(1);
        let continuation = previous_wide && (text.is_empty() || text == " ");
        previous_wide = width >= 2;
        cells.push(Cell {
            text: text.clone(),
            fg: parse_color(fg),
            bg: parse_color(bg),
            attrs: Attrs(*attrs as u32),
            width: if continuation { 0 } else { width.min(2) as u8 },
            link: None,
        });
    }
    cells
}

fn parse_color(value: &str) -> Color {
    if value == "default" || value.is_empty() {
        return Color::Default;
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
        assert_eq!(parse_color("white"), Color::Default);
    }

    #[test]
    fn marks_wide_character_continuation() {
        let line = vec![
            ("漢".to_owned(), "default".into(), "default".into(), 0),
            (" ".to_owned(), "default".into(), "default".into(), 0),
            ("a".to_owned(), "default".into(), "default".into(), 0),
        ];
        let cells = cells_from_row(&line);
        assert_eq!(cells[0].width, 2);
        assert_eq!(cells[1].width, 0);
        assert_eq!(cells[2].width, 1);
    }
}

#[cfg(test)]
mod query_tests {
    use super::QueryScanner;

    #[test]
    fn answers_da_and_dsr_queries() {
        let mut scanner = QueryScanner::default();
        let mut responses = scanner.scan(b"\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[?6n", (2, 4));
        assert_eq!(responses.len(), 5);
        assert_eq!(responses.remove(0), b"\x1b[?6c");
        assert_eq!(responses.remove(0), b"\x1b[>0;276;0c");
        assert_eq!(responses.remove(0), b"\x1b[0n");
        assert_eq!(responses.remove(0), b"\x1b[3;5R");
        assert_eq!(responses.remove(0), b"\x1b[?3;5R");
    }

    #[test]
    fn buffers_a_query_split_across_reads() {
        let mut scanner = QueryScanner::default();
        assert!(scanner.scan(b"\x1b[6", (1, 1)).is_empty());
        let responses = scanner.scan(b"n", (1, 1));
        assert_eq!(responses, vec![b"\x1b[2;2R".to_vec()]);
    }

    #[test]
    fn ignores_other_escape_sequences() {
        let mut scanner = QueryScanner::default();
        assert!(
            scanner
                .scan(b"\x1b[2J\x1b[31mhello\x1b[0m", (0, 0))
                .is_empty()
        );
    }
}
