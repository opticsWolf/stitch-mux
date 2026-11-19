//! ANSI terminal compatibility frontend.
//!
//! This adapter only writes server-composed ANSI frames and maps host key
//! events to protocol input events. It never parses ANSI, owns a screen model,
//! or encodes a key into terminal bytes: the daemon remains authoritative.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as HostKeyCode,
    KeyEvent as HostKeyEvent, KeyEventKind as HostKeyEventKind, KeyModifiers as HostModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags, poll,
    read,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use stitch_mux_client::{ClientError, ClientSession, Outbound};
use stitch_mux_core::Direction;
use stitch_mux_protocol::{
    DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, KeyCode, KeyEvent, KeyEventKind,
    PROTOCOL_VERSION, ResizeRequest, ViewMode,
};
use thiserror::Error;
use tokio::sync::mpsc;

/// Errors that can end a terminal attach session.
#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("host terminal I/O: {0}")]
    Io(#[from] io::Error),
    #[error("stitch-mux client: {0}")]
    Client(#[from] ClientError),
    #[error("stitch-mux frame: {0}")]
    Frame(#[from] stitch_mux_protocol::FrameError),
}

/// A terminal-local command. Mux keys never turn into key bytes for a pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCommand {
    Detach,
    Resync,
    Focus(Direction),
}

/// Interpret the detach prefix (`Ctrl-]`) without encoding any key bytes.
///
/// The returned mutable flag is the prefix state. Unknown post-prefix keys are
/// consumed instead of leaking into the child, which keeps detach/focus keys
/// predictable inside full-screen programs.
pub fn classify_local_key(key: &HostKeyEvent, prefix: &mut bool) -> Option<LocalCommand> {
    if key.kind != HostKeyEventKind::Press {
        return None;
    }
    if *prefix {
        *prefix = false;
        return match key.code {
            HostKeyCode::Char('d') | HostKeyCode::Char('D') => Some(LocalCommand::Detach),
            HostKeyCode::Char('r') | HostKeyCode::Char('R') => Some(LocalCommand::Resync),
            HostKeyCode::Left => Some(LocalCommand::Focus(Direction::Left)),
            HostKeyCode::Right => Some(LocalCommand::Focus(Direction::Right)),
            HostKeyCode::Up => Some(LocalCommand::Focus(Direction::Up)),
            HostKeyCode::Down => Some(LocalCommand::Focus(Direction::Down)),
            _ => None,
        };
    }
    if key.code == HostKeyCode::Char(']') && key.modifiers.contains(HostModifiers::CONTROL) {
        *prefix = true;
    }
    None
}

/// Map a host key to the protocol code and modifiers. The daemon decides how
/// to encode it for the focused pane's negotiated Kitty flags.
pub fn map_key(key: HostKeyEvent) -> Option<KeyEvent> {
    let mut shift = key.modifiers.contains(HostModifiers::SHIFT);
    let code = match key.code {
        HostKeyCode::Char(value) => KeyCode::Char(value),
        HostKeyCode::Enter => KeyCode::Enter,
        HostKeyCode::Tab => KeyCode::Tab,
        HostKeyCode::BackTab => {
            shift = true;
            KeyCode::Tab
        }
        HostKeyCode::Backspace => KeyCode::Backspace,
        HostKeyCode::Esc => KeyCode::Esc,
        HostKeyCode::Up => KeyCode::Up,
        HostKeyCode::Down => KeyCode::Down,
        HostKeyCode::Left => KeyCode::Left,
        HostKeyCode::Right => KeyCode::Right,
        HostKeyCode::Home => KeyCode::Home,
        HostKeyCode::End => KeyCode::End,
        HostKeyCode::PageUp => KeyCode::PageUp,
        HostKeyCode::PageDown => KeyCode::PageDown,
        HostKeyCode::Insert => KeyCode::Insert,
        HostKeyCode::Delete => KeyCode::Delete,
        HostKeyCode::F(number) => KeyCode::F(number),
        HostKeyCode::Null => KeyCode::Null,
        _ => return None,
    };
    let mut modifiers = 0_u16;
    if shift {
        modifiers |= 1;
    }
    if key.modifiers.contains(HostModifiers::ALT) {
        modifiers |= 2;
    }
    if key.modifiers.contains(HostModifiers::CONTROL) {
        modifiers |= 4;
    }
    if key
        .modifiers
        .intersects(HostModifiers::SUPER | HostModifiers::HYPER | HostModifiers::META)
    {
        modifiers |= 8;
    }
    let kind = match key.kind {
        HostKeyEventKind::Press => KeyEventKind::Press,
        HostKeyEventKind::Repeat => KeyEventKind::Repeat,
        HostKeyEventKind::Release => KeyEventKind::Release,
    };
    let text = match key.code {
        HostKeyCode::Char(value)
            if !key.modifiers.intersects(
                HostModifiers::CONTROL
                    | HostModifiers::ALT
                    | HostModifiers::SUPER
                    | HostModifiers::HYPER
                    | HostModifiers::META,
            ) =>
        {
            Some(value.to_string())
        }
        _ => None,
    };
    Some(KeyEvent {
        code,
        modifiers,
        kind,
        text,
    })
}

/// Clamp a host resize to a valid protocol size.
pub fn resize_request(cols: u16, rows: u16) -> ResizeRequest {
    ResizeRequest {
        cols: cols.max(1),
        rows: rows.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn hello(cols: u16, rows: u16) -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: format!("stitch-mux-terminal {}", env!("CARGO_PKG_VERSION")),
        client_instance: format!("terminal-{}", std::process::id()),
        view_mode: ViewMode::Terminal,
        cols: cols.max(1),
        rows: rows.max(1),
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

fn restore_terminal(out: &mut impl io::Write) {
    let _ = execute!(
        out,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Restores the host terminal when the process unwinds.
///
/// A panic in the attach loop used to leave the terminal in raw mode and on the
/// alternate screen, which hides the panic message and breaks the shell. The
/// guard restores on every exit path, including unwinding.
struct TerminalGuard<W: io::Write> {
    out: W,
    armed: bool,
}

impl<W: io::Write> TerminalGuard<W> {
    fn new(out: W) -> Self {
        Self { out, armed: true }
    }

    fn restore(&mut self) {
        if self.armed {
            restore_terminal(&mut self.out);
            self.armed = false;
        }
    }
}

impl<W: io::Write> Drop for TerminalGuard<W> {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Attach a real terminal to a named session and run until detach or loss.
pub async fn run(endpoint: Endpoint, session_name: &str) -> Result<(), TerminalError> {
    let (cols, rows) = crossterm::terminal::size()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(err) = execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        crossterm::cursor::Hide
    ) {
        let _ = crossterm::terminal::disable_raw_mode();
        return Err(TerminalError::Io(err));
    }
    // Progressive enhancement is optional; older hosts keep legacy escapes
    // and the daemon still encodes keys for the focused pane.
    let _ = execute!(stdout, PushKeyboardEnhancementFlags(KEYBOARD_FLAGS));

    let mut guard = TerminalGuard::new(stdout);
    let result = run_attached(&endpoint, session_name, hello(cols, rows), &mut guard.out).await;
    guard.restore();
    match result {
        Ok(detached) => {
            if detached {
                println!("detached from {session_name}");
            }
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Returns `true` when the user detached cleanly.
async fn run_attached(
    endpoint: &Endpoint,
    session_name: &str,
    hello: Hello,
    stdout: &mut io::Stdout,
) -> Result<bool, TerminalError> {
    let mut client = Some(
        ClientSession::attach(endpoint, session_name, hello)
            .await
            .map_err(TerminalError::Client)?,
    );
    let client_ref = client.as_ref().expect("client attached");
    let mut view_rx = client_ref.subscribe();
    let mut written_seq = 0_u64;
    let mut prefix = false;
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let input_stop = Arc::clone(&stop);
    let input_task = tokio::task::spawn_blocking(move || {
        while !input_stop.load(Ordering::Relaxed) {
            if poll(Duration::from_millis(100))? {
                let event = read()?;
                if input_tx.send(event).is_err() {
                    break;
                }
            }
        }
        Ok::<(), io::Error>(())
    });

    let mut detached = false;
    let mut failure: Option<TerminalError> = None;
    loop {
        let event = tokio::select! {
            input = input_rx.recv() => match input {
                Some(event) => event,
                None => break,
            },
            changed = view_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                {
                    let view = view_rx.borrow_and_update();
                    if view.connection == stitch_mux_client::ConnectionState::Lost {
                        if let Some(error) = &view.error {
                            eprintln!("stitch-mux: session lost: {:?}: {}", error.code, error.message);
                        } else {
                            eprintln!("stitch-mux: session lost");
                        }
                        break;
                    }
                    if view.needs_resync {
                        client_ref.send(Outbound::Resync);
                        continue;
                    }
                    let Some(render) = view.render.as_ref() else {
                        continue;
                    };
                    if render.seq == written_seq {
                        continue;
                    }
                    if render.full || render.seq == written_seq.saturating_add(1) {
                        if let Err(err) = paint(stdout, &render.ansi) {
                            failure = Some(TerminalError::Io(err));
                            break;
                        }
                        written_seq = render.seq;
                    } else {
                        // A watch update coalesced away frames this terminal
                        // never wrote. Never apply an orphan delta.
                        client_ref.send(Outbound::Resync);
                    }
                }
                continue;
            }
        };
        match event {
            Event::Key(key) => {
                if let Some(local) = classify_local_key(&key, &mut prefix) {
                    match local {
                        LocalCommand::Detach => {
                            detached = true;
                            break;
                        }
                        LocalCommand::Resync => client_ref.send(Outbound::Resync),
                        LocalCommand::Focus(direction) => {
                            client_ref.send(Outbound::FocusDirection(direction))
                        }
                    }
                    continue;
                }
                if let Some(key) = map_key(key) {
                    client_ref.send(Outbound::Input(InputEvent::Key(key)));
                }
            }
            Event::Paste(text) => {
                client_ref.send(Outbound::Input(InputEvent::Paste(text)));
            }
            Event::Resize(cols, rows) => {
                client_ref.send(Outbound::Resize(resize_request(cols, rows)));
            }
            Event::FocusGained => {
                client_ref.send(Outbound::Input(InputEvent::Focus(true)));
            }
            Event::FocusLost => {
                client_ref.send(Outbound::Input(InputEvent::Focus(false)));
            }
            Event::Mouse(_) => {}
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = input_task.await;
    if detached {
        if let Some(client) = client.take() {
            client.detach().await;
        }
    } else {
        drop(client.take());
    }
    match failure {
        Some(err) => Err(err),
        None => Ok(detached),
    }
}

fn paint(stdout: &mut io::Stdout, ansi: &[u8]) -> io::Result<()> {
    stdout.write_all(ansi)?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_keys_to_protocol_events_without_encoding_terminal_bytes() {
        let key = HostKeyEvent::new(HostKeyCode::Char('a'), HostModifiers::SHIFT);
        let mapped = map_key(key).expect("mapped key");
        assert_eq!(mapped.code, KeyCode::Char('a'));
        assert_eq!(mapped.modifiers, 1);
        assert_eq!(mapped.text.as_deref(), Some("a"));

        let tab = HostKeyEvent::new(HostKeyCode::BackTab, HostModifiers::SHIFT);
        let mapped = map_key(tab).expect("mapped tab");
        assert_eq!(mapped.code, KeyCode::Tab);
        assert_eq!(mapped.modifiers, 1);
        assert_eq!(mapped.text, None);

        let ctrl_c = HostKeyEvent::new(HostKeyCode::Char('c'), HostModifiers::CONTROL);
        let mapped = map_key(ctrl_c).expect("mapped ctrl-c");
        assert_eq!(mapped.modifiers, 4);
        assert_eq!(mapped.text, None);
    }

    #[test]
    fn detach_prefix_and_focus_are_local_commands() {
        let mut prefix = false;
        let start = HostKeyEvent::new(HostKeyCode::Char(']'), HostModifiers::CONTROL);
        assert_eq!(classify_local_key(&start, &mut prefix), None);
        assert!(prefix);

        let detach = HostKeyEvent::new(HostKeyCode::Char('d'), HostModifiers::NONE);
        assert_eq!(
            classify_local_key(&detach, &mut prefix),
            Some(LocalCommand::Detach)
        );
        assert!(!prefix);

        let focus = HostKeyEvent::new(HostKeyCode::Right, HostModifiers::NONE);
        let mut prefix = true;
        assert_eq!(
            classify_local_key(&focus, &mut prefix),
            Some(LocalCommand::Focus(Direction::Right))
        );
    }

    #[test]
    fn resize_clamps_missing_dimensions() {
        let request = resize_request(0, 0);
        assert_eq!((request.cols, request.rows), (1, 1));
    }
}
