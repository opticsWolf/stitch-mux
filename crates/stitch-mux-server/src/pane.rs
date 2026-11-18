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
use tokio::sync::{broadcast, mpsc, watch};
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
}

pub struct PaneRuntime {
    pub id: u64,
    pub session_id: u64,
    session: Arc<PtySession>,
    state: Mutex<PaneState>,
    updates: broadcast::Sender<Arc<PaneUpdate>>,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    exit_tx: watch::Sender<bool>,
    exit_rx: watch::Receiver<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl PaneRuntime {
    /// Spawn a child and start its reader, writer, and exit watcher tasks.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        session_id: u64,
        id: u64,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&[(String, String)]>,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<Self>, PaneError> {
        if cols == 0 || rows == 0 {
            return Err(PaneError::InvalidGeometry { cols, rows });
        }
        let winsize = Winsize::new(rows, cols, 0, 0);
        let session = match env {
            None => PtySession::spawn(program, args, Some(winsize), cwd).await?,
            Some(entries) => {
                PtySession::spawn_with_env(program, args, entries, Some(winsize), cwd).await?
            }
        };
        let session = Arc::new(session);
        let screen = HistoryScreen::new(cols as usize, rows as usize, 2000);
        let (updates, _) = broadcast::channel(UPDATE_QUEUE);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (exit_tx, exit_rx) = watch::channel(false);

        let runtime = Arc::new(Self {
            id,
            session_id,
            session: Arc::clone(&session),
            state: Mutex::new(PaneState {
                screen,
                seq: 0,
                program: program.to_owned(),
                bell: false,
            }),
            updates,
            input_tx,
            exit_tx,
            exit_rx,
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
        let mut reader_exit = runtime.exit_rx.clone();
        let reader = tokio::spawn(async move {
            let mut buf = vec![0_u8; READ_CHUNK];
            loop {
                tokio::select! {
                    biased;
                    _ = reader_exit.changed() => break,
                    result = reader_runtime.session.read(&mut buf) => match result {
                        Ok(0) => break,
                        Ok(n) => {
                            let (dirty, events) = {
                                let mut state = reader_runtime.lock_state();
                                state.screen.feed(&buf[..n]);
                                (state.screen.take_dirty_rows(), state.screen.take_events())
                            };
                            reader_runtime.publish_delta(dirty, &events);
                        }
                        Err(err) => {
                            tracing::warn!("pane read failed: {err}");
                            break;
                        }
                    },
                }
            }
        });

        let exit_runtime = Arc::clone(&runtime);
        let watcher = tokio::spawn(async move {
            let status = exit_runtime.session.wait().await;
            // Let the reader drain the final bytes before announcing exit.
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = exit_runtime.exit_tx.send(true);
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

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<PaneUpdate>> {
        self.updates.subscribe()
    }

    pub fn pid(&self) -> u32 {
        self.session.pid()
    }

    pub fn is_alive(&self) -> bool {
        self.session.is_alive() && !*self.exit_rx.borrow()
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

    pub fn capture(&self, start: u64, lines: u32, format: CaptureFormat) -> CaptureResponse {
        let state = self.lock_state();
        let start = start as usize;
        let count = (lines as usize).min(10_000);
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
        let _ = self.exit_tx.send(true);
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
