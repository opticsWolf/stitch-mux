//! ANSI terminal compatibility frontend.
//!
//! This adapter only writes server-composed ANSI frames and maps host key
//! events to protocol input events. It never parses ANSI, owns a screen model,
//! or encodes a key into terminal bytes: the daemon remains authoritative.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as HostKeyCode,
    KeyEvent as HostKeyEvent, KeyEventKind as HostKeyEventKind, KeyModifiers as HostModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags, poll,
    read,
};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use stitch_mux_client::{ClientError, ClientSession, Outbound};
use stitch_mux_core::Direction;
use stitch_mux_core::keymap::{
    DispatchOutcome, KeyDispatcher, KeySpec, Keymap as CoreKeymap, KeymapError,
};
use stitch_mux_protocol::{
    Command, CommandRequest, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, KeyCode,
    KeyEvent, KeyEventKind, Keymap as WireKeymap, PROTOCOL_VERSION, ResizeRequest, ViewMode,
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

fn host_key_name(key: &HostKeyEvent) -> Option<String> {
    match key.code {
        HostKeyCode::Char(value) => Some(value.to_string()),
        HostKeyCode::Enter => Some("Enter".into()),
        HostKeyCode::Tab => Some("Tab".into()),
        HostKeyCode::BackTab => Some("Tab".into()),
        HostKeyCode::Backspace => Some("Backspace".into()),
        HostKeyCode::Esc => Some("Esc".into()),
        HostKeyCode::Left => Some("Left".into()),
        HostKeyCode::Right => Some("Right".into()),
        HostKeyCode::Up => Some("Up".into()),
        HostKeyCode::Down => Some("Down".into()),
        HostKeyCode::Home => Some("Home".into()),
        HostKeyCode::End => Some("End".into()),
        HostKeyCode::PageUp => Some("PageUp".into()),
        HostKeyCode::PageDown => Some("PageDown".into()),
        HostKeyCode::Delete => Some("Delete".into()),
        HostKeyCode::Insert => Some("Insert".into()),
        HostKeyCode::F(number) => Some(format!("F{number}")),
        _ => None,
    }
}

fn host_modifiers(key: &HostKeyEvent) -> Vec<&'static str> {
    let mut modifiers = Vec::new();
    if key.modifiers.contains(HostModifiers::CONTROL) {
        modifiers.push("ctrl");
    }
    if key.modifiers.contains(HostModifiers::ALT) {
        modifiers.push("alt");
    }
    if key.modifiers.contains(HostModifiers::SHIFT) {
        modifiers.push("shift");
    }
    modifiers
}

fn protocol_key(code: &str) -> Option<KeyCode> {
    match code {
        "Enter" => Some(KeyCode::Enter),
        "Tab" => Some(KeyCode::Tab),
        "Backspace" => Some(KeyCode::Backspace),
        "Esc" => Some(KeyCode::Esc),
        "Left" => Some(KeyCode::Left),
        "Right" => Some(KeyCode::Right),
        "Up" => Some(KeyCode::Up),
        "Down" => Some(KeyCode::Down),
        "Home" => Some(KeyCode::Home),
        "End" => Some(KeyCode::End),
        "PageUp" => Some(KeyCode::PageUp),
        "PageDown" => Some(KeyCode::PageDown),
        "Delete" => Some(KeyCode::Delete),
        "Insert" => Some(KeyCode::Insert),
        value if value.starts_with('F') && value[1..].parse::<u8>().is_ok() => {
            Some(KeyCode::F(value[1..].parse::<u8>().ok()?))
        }
        value => value.chars().next().map(KeyCode::Char),
    }
}

fn key_event(spec: &KeySpec, kind: KeyEventKind) -> Option<KeyEvent> {
    let mut modifiers = 0_u16;
    for modifier in spec.modifiers() {
        modifiers |= match modifier {
            "C" => 4,
            "M" => 2,
            "S" => 1,
            _ => 0,
        };
    }
    Some(KeyEvent {
        code: protocol_key(spec.key())?,
        modifiers,
        kind,
        text: None,
    })
}

fn core_keymap(wire: &WireKeymap) -> Result<CoreKeymap, KeymapError> {
    let mut core = CoreKeymap::default();
    for binding in &wire.bindings {
        core.bindings.push(stitch_mux_core::keymap::KeyBinding {
            keys: binding
                .keys
                .iter()
                .map(|key| KeySpec::parse(key))
                .collect::<Result<_, _>>()?,
            action: binding.action.clone(),
            args: binding.args.clone(),
            mode: binding.mode.clone(),
            source: binding.source.clone(),
        });
    }
    core.modes = wire
        .modes
        .iter()
        .map(|mode| stitch_mux_core::keymap::KeyMode {
            name: mode.name.clone(),
            exclusive: mode.exclusive,
        })
        .collect();
    Ok(core)
}

fn paint_status(stdout: &mut io::Stdout, text: &str) -> io::Result<()> {
    let (_, rows) = crossterm::terminal::size()?;
    execute!(
        stdout,
        SavePosition,
        MoveTo(0, rows.saturating_sub(1)),
        Clear(ClearType::CurrentLine),
        crossterm::style::Print(text)
    )?;
    execute!(stdout, RestorePosition)
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
    client_ref.send(Outbound::Command(CommandRequest {
        request_id: 1,
        command: Command::ListActions,
    }));
    client_ref.send(Outbound::Command(CommandRequest {
        request_id: 2,
        command: Command::GetKeymap,
    }));
    let mut dispatcher = KeyDispatcher::new(CoreKeymap::builtin());
    let mut keymap_loaded = false;
    let mut request_id = 10_u64;
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
                    if !keymap_loaded && let Some(keymap) = &view.keymap {
                        match core_keymap(keymap) {
                            Ok(keymap) => dispatcher.set_keymap(keymap),
                            Err(err) => eprintln!("stitch-mux: invalid keymap from daemon: {err}"),
                        }
                        keymap_loaded = true;
                    }
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
                if key.kind == HostKeyEventKind::Press
                    && let Some(name) = host_key_name(&key)
                    && let Ok(spec) = KeySpec::from_parts(&name, &host_modifiers(&key))
                {
                    match dispatcher.dispatch(spec) {
                        DispatchOutcome::NeedMore { hints } => {
                            let _ = paint_status(stdout, &format!("keys: {}", hints.join(" | ")));
                            continue;
                        }
                        DispatchOutcome::Passthrough(keys) => {
                            for spec in keys {
                                if let Some(event) = key_event(&spec, KeyEventKind::Press) {
                                    client_ref.send(Outbound::Input(InputEvent::Key(event)));
                                }
                            }
                            continue;
                        }
                        DispatchOutcome::ModeEntered(mode) => {
                            let _ = paint_status(stdout, &format!("-- {mode} --"));
                            continue;
                        }
                        DispatchOutcome::ModeExited => {
                            let _ = paint_status(stdout, "");
                            continue;
                        }
                        DispatchOutcome::Action(invocation) => {
                            if invocation.action == "client.detach" {
                                detached = true;
                                break;
                            }
                            if invocation.action == "help.which-key" {
                                let hints = dispatcher.hints();
                                let _ =
                                    paint_status(stdout, &format!("keys: {}", hints.join(" | ")));
                                continue;
                            }
                            client_ref.send(Outbound::Command(CommandRequest {
                                request_id,
                                command: Command::InvokeAction {
                                    action: invocation.action,
                                    args: invocation.args,
                                    focus_pane_id: client_ref.view().focus,
                                },
                            }));
                            request_id = request_id.wrapping_add(1).max(10);
                            continue;
                        }
                    }
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
