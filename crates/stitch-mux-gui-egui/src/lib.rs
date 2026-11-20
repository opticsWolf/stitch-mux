//! Disposable egui reference adapter.
//!
//! This crate proves the structured protocol, tiled layout, per-client focus,
//! and detach/reattach loop. The product surface is toolkit-independent in
//! `stitch-mux-client`.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use stitch_mux_client::{ClientSession, Outbound, SessionView, command, size_from_pixels};
use stitch_mux_core::keymap::{
    DispatchOutcome, KeyBinding as CoreKeyBinding, KeyDispatcher, KeyMode as CoreKeyMode, KeySpec,
    Keymap as CoreKeymap, KeymapError,
};
use stitch_mux_core::{
    ActivitySummary, Color, LayoutNode, PaneId, ProgramState, ProgramStatus, ProgramStatusKind,
    Rect, Row, Size, SplitAxis,
};
use stitch_mux_protocol::{
    ActionInfo, Command, CommandRequest, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent,
    KeyCode, KeyEvent, KeyEventKind, Keymap as WireKeymap, Layout, PaneGeometry, SessionInfo,
    ViewMode,
};
use tokio::sync::watch;

const CELL_W: f32 = 8.4;
const CELL_H: f32 = 18.0;
const FONT_SIZE: f32 = 14.0;
const BORDER_PX: f32 = 2.0;
/// Rows retained by the disposable GUI while it is attached. The daemon owns
/// the authoritative scrollback; this is only enough for wheel scrolling in a
/// reference adapter.
const MAX_LOCAL_SCROLLBACK: usize = 1000;
const INITIAL_RETRY: Duration = Duration::from_millis(500);
const MAX_RETRY: Duration = Duration::from_secs(8);

/// Launch the reference GUI. `session` may be omitted to show the picker.
pub fn run(endpoint: Endpoint, session: Option<String>) -> Result<(), Box<dyn Error>> {
    let _ = stitch_mux_protocol::init_logging();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    // Keep the runtime alive while eframe blocks the calling thread.
    std::thread::spawn(move || {
        let _guard = runtime.enter();
        runtime.block_on(std::future::pending::<()>());
    });

    let response = handle.block_on(command(&endpoint, Command::ListSessions))?;
    if !response.ok {
        return Err(response
            .error
            .map(|err| err.message)
            .unwrap_or_else(|| "list sessions failed".into())
            .into());
    }
    let sessions = response.sessions;

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "stitch-mux",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(MuxApp::new(endpoint, session, sessions, handle)) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|err| -> Box<dyn Error> { Box::new(err) })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CellPos {
    /// Absolute screen row, so a selection survives scrollback growth.
    row: u64,
    col: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Selection {
    anchor: CellPos,
    head: CellPos,
}

impl Selection {
    fn ordered(&self) -> (CellPos, CellPos) {
        if (self.anchor.row, self.anchor.col) <= (self.head.row, self.head.col) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PaneScroll {
    seq: u64,
    scrollback_len: u64,
    visible: Vec<Row>,
    history: VecDeque<Row>,
    offset: usize,
    /// Fractional row scrolled beyond `offset`, accumulated from smooth wheel
    /// deltas. Rendering shifts by this fraction and paints one extra row.
    fraction: f32,
    selection: Option<Selection>,
}

fn core_keymap(wire: &WireKeymap) -> Result<CoreKeymap, KeymapError> {
    let mut core = CoreKeymap::default();
    for binding in &wire.bindings {
        core.bindings.push(CoreKeyBinding {
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
        .map(|mode| CoreKeyMode {
            name: mode.name.clone(),
            exclusive: mode.exclusive,
        })
        .collect();
    Ok(core)
}

fn egui_key_name(key: egui::Key) -> Option<String> {
    use egui::Key;
    let name = match key {
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::Escape => "Esc",
        Key::Enter => "Enter",
        Key::Tab => "Tab",
        Key::Backspace => "Backspace",
        Key::Space => "Space",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::Questionmark => "?",
        Key::Colon => ":",
        Key::Comma => ",",
        Key::Backslash => "\\",
        Key::Slash => "/",
        Key::Pipe => "|",
        Key::Exclamationmark => "!",
        Key::OpenBracket => "[",
        Key::CloseBracket => "]",
        Key::OpenCurlyBracket => "{",
        Key::CloseCurlyBracket => "}",
        Key::Backtick => "`",
        Key::Minus => "-",
        Key::Period => ".",
        Key::Plus => "+",
        Key::Equals => "=",
        Key::Semicolon => ";",
        Key::Quote => "'",
        Key::A => "a",
        Key::B => "b",
        Key::C => "c",
        Key::D => "d",
        Key::E => "e",
        Key::F => "f",
        Key::G => "g",
        Key::H => "h",
        Key::I => "i",
        Key::J => "j",
        Key::K => "k",
        Key::L => "l",
        Key::M => "m",
        Key::N => "n",
        Key::O => "o",
        Key::P => "p",
        Key::Q => "q",
        Key::R => "r",
        Key::S => "s",
        Key::T => "t",
        Key::U => "u",
        Key::V => "v",
        Key::W => "w",
        Key::X => "x",
        Key::Y => "y",
        Key::Z => "z",
        Key::Num0 => "0",
        Key::Num1 => "1",
        Key::Num2 => "2",
        Key::Num3 => "3",
        Key::Num4 => "4",
        Key::Num5 => "5",
        Key::Num6 => "6",
        Key::Num7 => "7",
        Key::Num8 => "8",
        Key::Num9 => "9",
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        _ => return None,
    };
    Some(name.to_owned())
}

fn key_spec_code(spec: &KeySpec) -> Option<KeyCode> {
    match spec.key() {
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
        "Space" => Some(KeyCode::Char(' ')),
        value if value.starts_with('F') && value[1..].parse::<u8>().is_ok() => {
            Some(KeyCode::F(value[1..].parse::<u8>().ok()?))
        }
        value => value.chars().next().map(KeyCode::Char),
    }
}

fn key_spec_event(spec: &KeySpec, kind: KeyEventKind) -> Option<KeyEvent> {
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
        code: key_spec_code(spec)?,
        modifiers,
        kind,
        text: None,
    })
}

struct MuxApp {
    endpoint: Endpoint,
    sessions: Vec<SessionInfo>,
    selected: String,
    client: Option<ClientSession>,
    view_rx: Option<watch::Receiver<SessionView>>,
    handle: tokio::runtime::Handle,
    last_size: Size,
    next_request_id: u64,
    error: Option<String>,
    retry_backoff: Duration,
    next_retry: Option<Instant>,
    /// Last session the daemon lost, shown in the picker until reattached.
    lost_session: Option<String>,
    /// Layout revision captured when a splitter drag begins. Sending the
    /// stale revision on every mouse frame would make the daemon reject all
    /// but the first resize.
    split_drag: Option<(u64, LayoutNode)>,
    /// Structured rows that have scrolled off each pane while this GUI was
    /// attached, plus the user's wheel offset.
    pane_scroll: HashMap<u64, PaneScroll>,
    /// F1 / the status-bar button opens local connection diagnostics.
    show_diagnostics: bool,
    endpoint_text: String,
    log_path_text: String,
    /// Session picker filter and local unread/attention bookkeeping.
    session_filter: String,
    attention_only: bool,
    seen_activity: HashMap<String, u64>,
    /// Daemon-owned action catalog and effective keymap, cached for local
    /// dispatch. The daemon remains authoritative.
    actions: Vec<ActionInfo>,
    dispatcher: KeyDispatcher,
    keymap_request_id: u64,
    applied_keymap_request: u64,
    key_hints: Vec<String>,
    show_palette: bool,
    palette_filter: String,
    /// A custom-scheme OSC 8 URI awaiting explicit user confirmation.
    pending_link: Option<String>,
    /// Copy the selected terminal text as soon as a drag settles.
    copy_on_selection: bool,
}

impl MuxApp {
    fn new(
        endpoint: Endpoint,
        session: Option<String>,
        sessions: Vec<SessionInfo>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        let selected = session
            .or_else(|| sessions.first().map(|info| info.name.clone()))
            .unwrap_or_default();
        let endpoint_text = endpoint.to_string();
        let log_path_text = stitch_mux_protocol::log_path().display().to_string();
        let mut app = Self {
            endpoint,
            sessions,
            selected: selected.clone(),
            client: None,
            view_rx: None,
            handle,
            last_size: Size::new(80, 24),
            next_request_id: 1,
            error: None,
            retry_backoff: INITIAL_RETRY,
            next_retry: None,
            lost_session: None,
            split_drag: None,
            pane_scroll: HashMap::new(),
            show_diagnostics: false,
            endpoint_text,
            log_path_text,
            session_filter: String::new(),
            attention_only: false,
            seen_activity: HashMap::new(),
            actions: Vec::new(),
            dispatcher: KeyDispatcher::new(CoreKeymap::builtin()),
            keymap_request_id: 0,
            applied_keymap_request: 0,
            key_hints: Vec::new(),
            show_palette: false,
            palette_filter: String::new(),
            pending_link: None,
            copy_on_selection: true,
        };
        if !selected.is_empty() {
            app.attach(&selected);
        }
        app
    }

    fn attach(&mut self, name: &str) {
        let hello = Hello {
            protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
            client_name: "stitch-mux-gui-egui".into(),
            client_instance: format!("egui-{}", std::process::id()),
            view_mode: ViewMode::Structured,
            cols: self.last_size.cols,
            rows: self.last_size.rows,
            pixel_width: 0,
            pixel_height: 0,
            capabilities: DEFAULT_CLIENT_CAPABILITIES,
        };
        match self
            .handle
            .block_on(ClientSession::attach(&self.endpoint, name, hello))
        {
            Ok(client) => {
                self.view_rx = Some(client.subscribe());
                self.client = Some(client);
                self.keymap_request_id = self.send_command(Command::GetKeymap);
                self.send_command(Command::ListActions);
                self.selected = name.to_owned();
                self.error = None;
                self.lost_session = None;
                self.retry_backoff = INITIAL_RETRY;
                self.next_retry = None;
            }
            Err(err) => {
                tracing::warn!(error = %err, "attach failed");
                self.error = Some(err.to_string());
                self.retry_backoff = next_backoff(self.retry_backoff);
                self.next_retry = Some(Instant::now() + self.retry_backoff);
            }
        }
    }

    /// Retry a lost session with bounded exponential backoff. A daemon that is
    /// down is never silently replaced with a fresh session.
    fn maybe_reconnect(&mut self, view: &SessionView) {
        if view.connection != stitch_mux_client::ConnectionState::Lost {
            self.next_retry = None;
            return;
        }
        let now = Instant::now();
        match self.next_retry {
            None => self.next_retry = Some(now + self.retry_backoff),
            Some(at) if now >= at => {
                let name = self.selected.clone();
                self.next_retry = None;
                self.attach(&name);
            }
            Some(_) => {}
        }
    }

    fn create_session_and_attach(&mut self) {
        let name = if self.selected.is_empty() {
            "default".to_owned()
        } else {
            self.selected.clone()
        };
        let (program, args) = if cfg!(windows) {
            ("cmd.exe".to_owned(), Vec::new())
        } else {
            ("sh".to_owned(), Vec::new())
        };
        let response = self.handle.block_on(command(
            &self.endpoint,
            Command::EnsureSession {
                name: name.clone(),
                program,
                args,
                cwd: None,
                env: None,
                cols: self.last_size.cols,
                rows: self.last_size.rows,
            },
        ));
        match response {
            Ok(response) if response.ok => self.attach(&name),
            Ok(response) => {
                self.error = response.error.map(|error| error.message);
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn send(&self, outbound: Outbound) {
        if let Some(client) = &self.client {
            client.send(outbound);
        }
    }

    fn send_command(&mut self, command: Command) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.send(Outbound::Command(CommandRequest {
            request_id,
            command,
        }));
        request_id
    }

    /// Keep a bounded local copy of rows that scroll off the visible screen.
    /// The daemon remains authoritative; a fresh snapshot resets this cache.
    fn sync_scroll(&mut self, view: &SessionView) {
        self.pane_scroll
            .retain(|pane_id, _| view.panes.contains_key(pane_id));
        for (pane_id, pane) in &view.panes {
            let Some(snapshot) = pane.snapshot.as_ref() else {
                continue;
            };
            let state = self.pane_scroll.entry(*pane_id).or_default();
            if state.seq > snapshot.seq || state.scrollback_len > snapshot.scrollback_len {
                *state = PaneScroll::default();
            }
            if state.visible.is_empty() {
                state.seq = snapshot.seq;
                state.scrollback_len = snapshot.scrollback_len;
                state.visible = snapshot.cells.clone();
                continue;
            }
            if snapshot.seq == state.seq {
                continue; // no new output since the last frame
            }
            // Lines that entered the daemon's scrollback are exactly the rows
            // that left the top of the previous viewport.
            let scrolled = (snapshot.scrollback_len - state.scrollback_len) as usize;
            for row in state.visible.iter().take(scrolled) {
                state.history.push_back(row.clone());
            }
            while state.history.len() > MAX_LOCAL_SCROLLBACK {
                state.history.pop_front();
            }
            state.seq = snapshot.seq;
            state.scrollback_len = snapshot.scrollback_len;
            state.visible = snapshot.cells.clone();
            state.offset = state.offset.min(state.history.len());
            let visible_len = snapshot.cells.len() as u64;
            let oldest = snapshot
                .total_lines
                .saturating_sub(state.history.len() as u64 + visible_len);
            if state.selection.is_some_and(|selection| {
                let (start, end) = selection.ordered();
                start.row < oldest || end.row >= snapshot.total_lines
            }) {
                state.selection = None;
            }
        }
    }

    fn refresh_sessions(&mut self) {
        match self
            .handle
            .block_on(command(&self.endpoint, Command::ListSessions))
        {
            Ok(response) => {
                if response.ok {
                    self.sessions = response.sessions;
                    let names: std::collections::BTreeSet<_> =
                        self.sessions.iter().map(|info| info.name.clone()).collect();
                    self.seen_activity.retain(|name, _| names.contains(name));
                } else {
                    self.error = response.error.map(|err| err.message);
                }
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    /// Local-only diagnostics: connection state, the endpoint in use, the log
    /// file and a fresh session list. Never writes to the daemon except a
    /// `ListSessions` command when the panel is opened or refreshed.
    fn diagnostics_ui(&mut self, ctx: &egui::Context, view: &SessionView) {
        if !self.show_diagnostics {
            return;
        }
        let sessions = self.sessions.clone();
        let endpoint = self.endpoint_text.clone();
        let log_path = self.log_path_text.clone();
        let mut refresh = false;
        egui::Window::new("stitch-mux diagnostics")
            .open(&mut self.show_diagnostics)
            .show(ctx, |ui| {
                ui.label(format!("Endpoint: {endpoint}"));
                ui.label(format!("Connection: {:?}", view.connection));
                ui.label(format!("Session: {}", self.selected));
                ui.label(format!(
                    "Geometry: {}x{} r{} ({} attached)",
                    view.size.cols, view.size.rows, view.layout_revision, view.attached_clients
                ));
                ui.label(format!("Needs resync: {}", view.needs_resync));
                if let Some(error) = &view.error {
                    ui.colored_label(egui::Color32::RED, format!("Error: {}", error.message));
                }
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::RED, format!("Last failure: {error}"));
                }
                ui.small(format!("Log: {log_path}"));
                ui.separator();
                ui.label("Daemon sessions:");
                for info in &sessions {
                    ui.label(format!(
                        "{} — {} panes, {} clients",
                        info.name, info.pane_count, info.attached_clients
                    ));
                    for pane in &info.panes {
                        ui.small(format!(
                            "  #{} {} ({}x{})",
                            pane.id, pane.title, pane.cols, pane.rows
                        ));
                    }
                }
                if sessions.is_empty() {
                    ui.label("(none)");
                }
                refresh = ui.button("Refresh sessions").clicked();
            });
        if refresh {
            self.refresh_sessions();
        }
    }
}

impl eframe::App for MuxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.input(|input| input.key_pressed(egui::Key::F1)) {
            self.show_diagnostics = !self.show_diagnostics;
            if self.show_diagnostics {
                self.refresh_sessions();
            }
        }
        if self.client.is_none() {
            self.picker_ui(ui);
            if self.show_diagnostics {
                let view = SessionView::default();
                self.diagnostics_ui(ui.ctx(), &view);
            }
            return;
        }

        let Some(receiver) = self.view_rx.as_mut() else {
            return;
        };
        // egui only repaints on input by default. A terminal must keep painting
        // while the daemon streams output, otherwise a busy pane freezes until
        // the user moves the mouse.
        ui.ctx().request_repaint_after(Duration::from_millis(16));
        let view = receiver.borrow_and_update().clone();
        if let Some(last) = &view.last_command {
            if !last.actions.is_empty() {
                self.actions = last.actions.clone();
            }
            if last.request_id == self.keymap_request_id
                && self.applied_keymap_request != last.request_id
                && let Some(keymap) = &last.keymap
            {
                match core_keymap(keymap) {
                    Ok(keymap) => self.dispatcher.set_keymap(keymap),
                    Err(err) => self.error = Some(format!("invalid daemon keymap: {err}")),
                }
                self.applied_keymap_request = last.request_id;
            }
        }
        let open_palette = ui.input(|input| {
            input.modifiers.ctrl
                && input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::P,
                            ..
                        }
                    )
                })
        });
        if open_palette {
            self.show_palette = !self.show_palette;
            self.palette_filter.clear();
        }
        // Reading a session's status marks it seen. Unread state is local to
        // this GUI; the daemon only publishes the current status record.
        for pane in view.panes.values() {
            if let Some(status) = pane
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.program_status.as_ref())
            {
                let seen = self.seen_activity.entry(view.name.clone()).or_insert(0);
                *seen = (*seen).max(status.updated_at_unix_ms);
            }
        }
        self.maybe_reconnect(&view);
        if view.connection == stitch_mux_client::ConnectionState::Lost {
            self.lost_ui(ui, &view);
            self.diagnostics_ui(ui.ctx(), &view);
            return;
        }

        let (detach, diagnostics) = self.status_ui(ui, &view);
        if diagnostics {
            self.show_diagnostics = !self.show_diagnostics;
            if self.show_diagnostics {
                self.refresh_sessions();
            }
        }
        // The status bar is a real child of this layout, so the available rect
        // is already the pane grid: subtracting STATUS_H here left a dead strip
        // at the bottom and made the requested size disagree with the grid.
        let grid = ui.available_rect_before_wrap();
        let next_size = size_from_pixels(grid.width(), grid.height(), CELL_W, CELL_H);
        if next_size != self.last_size {
            self.last_size = next_size;
            self.send(Outbound::Resize(stitch_mux_protocol::ResizeRequest {
                cols: next_size.cols,
                rows: next_size.rows,
                pixel_width: 0,
                pixel_height: 0,
            }));
        }
        self.sync_scroll(&view);
        let actions = draw_panes(
            ui,
            &view,
            grid,
            &mut self.pane_scroll,
            self.copy_on_selection,
        );
        if let Some(text) = actions.copy {
            copy_to_clipboard(ui.ctx(), &text);
        }
        if let Some(uri) = actions.link {
            if safe_link_scheme(&uri) {
                ui.ctx().open_url(egui::OpenUrl::new_tab(uri));
            } else {
                self.pending_link = Some(uri);
            }
        }
        for action in actions.panes {
            match action {
                PaneAction::Focus(pane_id) => self.send(Outbound::Focus(pane_id)),
                PaneAction::Split { pane_id, axis } => {
                    self.send_command(Command::SplitPane {
                        pane_id,
                        axis,
                        ratio_bps: 5000,
                    });
                }
                PaneAction::Close(pane_id) => {
                    self.send_command(Command::ClosePane { pane_id });
                }
                PaneAction::Zoom { pane_id, unzoom } => {
                    self.send_command(Command::ZoomPane {
                        session: view.name.clone(),
                        pane_id: unzoom.then_some(pane_id),
                    });
                }
            }
        }
        if let Some(drag) = actions.drag {
            if drag.started {
                self.split_drag = Some((view.layout_revision, drag.node.clone()));
            }
            if drag.stopped {
                let base_revision = self
                    .split_drag
                    .as_ref()
                    .map(|(revision, _)| *revision)
                    .unwrap_or(view.layout_revision);
                self.send_command(Command::SetLayout {
                    base_revision,
                    root: set_ratio(&drag.node, &drag.node, drag.ratio_bps),
                });
                self.split_drag = None;
            }
        }
        if detach && let Some(client) = self.client.take() {
            self.view_rx = None;
            self.handle.spawn(async move { client.detach().await });
        }
        if !self.show_palette {
            let detach_from_key = handle_input(ui, self, grid, &view);
            if detach_from_key && let Some(client) = self.client.take() {
                self.view_rx = None;
                self.handle.spawn(async move { client.detach().await });
            }
        }
        self.palette_ui(ui, &view);
        self.diagnostics_ui(ui.ctx(), &view);
        self.link_ui(ui.ctx());
    }

    fn on_exit(&mut self) {
        if let Some(client) = self.client.take() {
            client.send(Outbound::Detach);
        }
    }
}

impl MuxApp {
    fn lost_ui(&mut self, ui: &mut egui::Ui, view: &SessionView) {
        ui.heading("Session lost");
        ui.colored_label(
            egui::Color32::RED,
            "The daemon connection was lost. Child processes are not restored by this client; retry or start a new session explicitly.",
        );
        if let Some(error) = &view.error {
            ui.label(format!("Reason: {}", error.message));
        }
        ui.small(format!("Endpoint: {}", self.endpoint_text));
        ui.small(format!("Log: {}", self.log_path_text));
        if let Some(at) = self.next_retry {
            ui.label(format!(
                "Retrying in {:.1}s",
                at.saturating_duration_since(Instant::now()).as_secs_f32()
            ));
        }
        ui.horizontal(|ui| {
            if ui.button("Retry now").clicked() {
                let name = self.selected.clone();
                self.next_retry = None;
                self.attach(&name);
            }
            if ui.button("New session").clicked() {
                self.create_session_and_attach();
            }
            if ui.button("Back to picker").clicked() {
                self.client = None;
                self.view_rx = None;
                self.lost_session = Some(self.selected.clone());
                self.error = view.error.as_ref().map(|error| error.message.clone());
                self.refresh_sessions();
            }
        });
    }

    fn picker_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("stitch-mux");
        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.text_edit_singleline(&mut self.session_filter);
            ui.checkbox(&mut self.attention_only, "Needs attention/new");
        });
        let filter = self.session_filter.clone();
        let attention_only = self.attention_only;
        let now = now_unix_ms();
        let sessions = self.sessions.clone();
        let visible: Vec<_> = sessions
            .iter()
            .filter(|info| session_matches(info, &filter, attention_only, now, &self.seen_activity))
            .cloned()
            .collect();
        if visible.is_empty() {
            ui.label("No matching sessions. Run `stitch-mux new -s demo -- sh` first.");
        } else {
            ui.label("Pick a session:");
            for info in visible {
                let panes = info
                    .panes
                    .iter()
                    .map(|pane| {
                        format!(
                            "{}{} ({}) {}x{}",
                            status_badge(pane.program_status.as_ref()),
                            pane.title,
                            pane.id,
                            pane.cols,
                            pane.rows
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let unread = info.last_activity_unix_ms
                    > self
                        .seen_activity
                        .get(&info.name)
                        .copied()
                        .unwrap_or(info.last_activity_unix_ms);
                let label = session_label(&info, self.lost_session.as_deref(), now, unread, &panes);
                if ui.button(label).clicked() {
                    self.seen_activity
                        .insert(info.name.clone(), info.last_activity_unix_ms);
                    self.attach(&info.name);
                }
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }
        if ui.button("Refresh").clicked() {
            self.refresh_sessions();
        }
        ui.small(format!("Endpoint: {}", self.endpoint_text));
        ui.small(format!("Log: {}", self.log_path_text));
    }

    /// Paint session information in a status bar above the pane grid.
    fn view_focus(&self) -> Option<u64> {
        self.view_rx
            .as_ref()
            .map(|rx| rx.borrow().focus)
            .unwrap_or(None)
    }

    fn palette_ui(&mut self, ui: &mut egui::Ui, view: &SessionView) {
        if !self.show_palette {
            return;
        }
        let actions = self.actions.clone();
        let mut open = self.show_palette;
        let mut filter = self.palette_filter.clone();
        let mut selected: Option<String> = None;
        egui::Window::new("Command palette")
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    ui.label("Action:");
                    ui.text_edit_singleline(&mut filter);
                });
                let needle = filter.to_ascii_lowercase();
                for action in &actions {
                    if !action.name.contains(&needle)
                        && !action.title.to_ascii_lowercase().contains(&needle)
                    {
                        continue;
                    }
                    if ui
                        .button(format!("{} — {}", action.name, action.title))
                        .clicked()
                    {
                        selected = Some(action.name.clone());
                    }
                }
                if actions.is_empty() {
                    ui.label("(waiting for the daemon action catalog)");
                }
            });
        self.show_palette = open;
        self.palette_filter = filter;
        if let Some(action) = selected {
            if action == "prompt.previous" || action == "prompt.next" {
                jump_prompt(&mut self.pane_scroll, view, action == "prompt.previous");
            }
            self.send_command(Command::InvokeAction {
                action,
                args: None,
                focus_pane_id: view.focus,
            });
        }
    }

    fn link_ui(&mut self, ctx: &egui::Context) {
        let Some(uri) = self.pending_link.clone() else {
            return;
        };
        let mut open = true;
        let mut confirmed = false;
        let mut cancel = false;
        egui::Window::new("Open link?")
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("This terminal link uses a custom scheme:");
                ui.code(&uri);
                ui.horizontal(|ui| {
                    confirmed = ui.button("Open").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if confirmed || cancel || !open {
            self.pending_link = None;
        }
        if confirmed {
            ctx.open_url(egui::OpenUrl::new_tab(uri));
        }
    }

    fn status_ui(&mut self, ui: &mut egui::Ui, view: &SessionView) -> (bool, bool) {
        let mut detach = false;
        let mut diagnostics = false;
        ui.horizontal(|ui| {
            let color = if view.needs_resync {
                egui::Color32::YELLOW
            } else if view.error.is_some() {
                egui::Color32::RED
            } else {
                egui::Color32::LIGHT_GRAY
            };
            ui.colored_label(color, status_text(view));
            if let Some(mode) = self.dispatcher.active_mode() {
                ui.label(format!("[{mode}]"));
            }
            if !self.key_hints.is_empty() {
                ui.small(self.key_hints.join(" | "));
            }
            if let Some(error) = &view.error {
                ui.colored_label(egui::Color32::RED, &error.message);
            }
            if let Some(snapshot) = view
                .focus
                .and_then(|pane_id| view.pane(pane_id))
                .and_then(|pane| pane.snapshot.as_ref())
            {
                if let Some(progress) = &snapshot.progress
                    && progress.state != stitch_mux_protocol::ProgressState::Remove
                {
                    let text = match progress.value {
                        Some(value) => format!("[{value}%]"),
                        None => "[working]".into(),
                    };
                    ui.label(text);
                }
                if let Some(notification) = &snapshot.notification {
                    let title = notification.title.as_deref().unwrap_or("notify");
                    ui.label(format!("[{title}: {}]", notification.message));
                }
            }
            if let Some(exit) = &view.exit {
                ui.label(format!("child: {}", exit.reason));
            }
            if view.needs_resync {
                self.send(Outbound::Resync);
            }
            if ui.button("‹P").clicked() {
                jump_prompt(&mut self.pane_scroll, view, true);
            }
            if ui.button("P›").clicked() {
                jump_prompt(&mut self.pane_scroll, view, false);
            }
            detach = ui.button("Detach").clicked();
            if ui
                .button(if self.copy_on_selection {
                    "Copy: drag"
                } else {
                    "Copy: off"
                })
                .clicked()
            {
                self.copy_on_selection = !self.copy_on_selection;
            }
            if ui.button("Actions").clicked() {
                self.show_palette = true;
                self.palette_filter.clear();
            }
            diagnostics = ui.button("Diagnostics").clicked();
        });
        ui.separator();
        (detach, diagnostics)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneAction {
    Focus(u64),
    Split { pane_id: u64, axis: SplitAxis },
    Close(u64),
    Zoom { pane_id: u64, unzoom: bool },
}

struct SplitDrag {
    started: bool,
    stopped: bool,
    node: LayoutNode,
    ratio_bps: u16,
}

struct DrawActions {
    panes: Vec<PaneAction>,
    drag: Option<SplitDrag>,
    link: Option<String>,
    copy: Option<String>,
}

fn draw_panes(
    ui: &mut egui::Ui,
    view: &SessionView,
    grid: egui::Rect,
    scroll: &mut HashMap<u64, PaneScroll>,
    copy_on_selection: bool,
) -> DrawActions {
    let mut actions = DrawActions {
        panes: Vec::new(),
        drag: None,
        link: None,
        copy: None,
    };
    ui.painter()
        .rect_filled(grid, 0.0, egui::Color32::from_gray(12));

    let Some(layout) = view.layout.as_ref() else {
        ui.painter().text(
            grid.center(),
            egui::Align2::CENTER_CENTER,
            "waiting for layout…",
            egui::FontId::proportional(16.0),
            egui::Color32::GRAY,
        );
        return actions;
    };
    let cols = layout.cols.max(1);
    let rows = layout.rows.max(1);
    let cell_w = grid.width() / f32::from(cols);
    let cell_h = grid.height() / f32::from(rows);
    // A single-pane session also has a leaf root, so the daemon's explicit
    // zoom marker is what distinguishes "zoomed" from "one pane".
    let zoomed_pane = layout.zoomed_pane;

    for geometry in &layout.panes {
        let pane_id = geometry.pane_id;
        let outer = cell_rect(grid, geometry.outer, cell_w, cell_h);
        let focused = view.focus == Some(pane_id);
        let border = if focused {
            egui::Color32::from_rgb(96, 165, 250)
        } else {
            egui::Color32::from_gray(60)
        };
        ui.painter()
            .rect_filled(outer, 0.0, egui::Color32::from_gray(16));
        ui.painter().rect_stroke(
            outer.shrink(BORDER_PX / 2.0),
            0.0,
            egui::Stroke::new(BORDER_PX, border),
            egui::StrokeKind::Inside,
        );

        // Title bar lives in the pane's border row, so it never covers PTY
        // content. Buttons are real egui interactions, not painted labels.
        let title_rect = egui::Rect::from_min_size(
            outer.min + egui::vec2(BORDER_PX, BORDER_PX),
            egui::vec2(
                (outer.width() - 2.0 * BORDER_PX).max(0.0),
                (cell_h - BORDER_PX).max(8.0),
            ),
        );
        ui.painter().rect_filled(
            title_rect,
            0.0,
            if focused {
                egui::Color32::from_rgb(32, 54, 82)
            } else {
                egui::Color32::from_gray(28)
            },
        );
        let button_w = 18.0;
        let button_h = title_rect.height().max(12.0);
        let button_y = title_rect.max.y - button_h;
        let mut buttons: Vec<(&str, PaneAction)> = Vec::new();
        buttons.push(("X", PaneAction::Close(pane_id)));
        buttons.push((
            if zoomed_pane == Some(pane_id) {
                "U"
            } else {
                "Z"
            },
            PaneAction::Zoom {
                pane_id,
                unzoom: zoomed_pane == Some(pane_id),
            },
        ));
        buttons.push((
            "TB",
            PaneAction::Split {
                pane_id,
                axis: SplitAxis::Rows,
            },
        ));
        buttons.push((
            "LR",
            PaneAction::Split {
                pane_id,
                axis: SplitAxis::Columns,
            },
        ));
        let pointer = ui.input(|input| {
            (
                input
                    .pointer
                    .button_clicked(egui::PointerButton::Primary)
                    .then(|| input.pointer.latest_pos())
                    .flatten(),
                input.pointer.button_down(egui::PointerButton::Primary),
                input.pointer.button_released(egui::PointerButton::Primary),
                input.pointer.latest_pos(),
            )
        });
        let click_pos = pointer.0;
        // Title-bar buttons sit on the foreground layer so pane content (which
        // is painted afterwards and clipped to its own rect) can never hide
        // them.
        let button_painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("stitch-mux-pane-buttons"),
        ));
        let mut button_clicked = false;
        let mut button_x = title_rect.max.x;
        for (label, action) in buttons {
            let rect = egui::Rect::from_min_size(
                egui::pos2(button_x - button_w, button_y),
                egui::vec2(button_w, button_h),
            );
            let hovered = click_pos.is_some_and(|pos| rect.contains(pos));
            button_painter.rect_filled(
                rect,
                2.0,
                if hovered {
                    egui::Color32::from_rgb(70, 90, 120)
                } else {
                    egui::Color32::from_gray(50)
                },
            );
            button_painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(9.0),
                egui::Color32::WHITE,
            );
            if hovered && click_pos.is_some() && !button_clicked {
                actions.panes.push(action);
                button_clicked = true;
            }
            button_x -= button_w;
        }
        let title_width = (button_x - title_rect.min.x - 4.0).max(0.0);
        let state = scroll.entry(pane_id).or_default();
        let status = view
            .pane(pane_id)
            .and_then(|pane| pane.snapshot.as_ref())
            .and_then(|snapshot| snapshot.program_status.as_ref());
        let mut title = format!("{}{} ({})", status_badge(status), geometry.title, pane_id);
        if state.offset > 0 {
            title.push_str(&format!(" ↑{}", state.offset));
        }
        let title: String = title.chars().take((title_width / 6.0) as usize).collect();
        ui.painter().text(
            title_rect.min + egui::vec2(3.0, 0.0),
            egui::Align2::LEFT_TOP,
            title,
            egui::FontId::proportional(11.0),
            if focused {
                egui::Color32::from_rgb(180, 220, 255)
            } else {
                egui::Color32::GRAY
            },
        );

        let response = ui.interact(
            outer,
            egui::Id::unique(("pane", pane_id)),
            egui::Sense::click(),
        );
        if response.clicked() && !button_clicked {
            actions.panes.push(PaneAction::Focus(pane_id));
            ui.memory_mut(|memory| memory.request_focus(input_focus()));
        }

        let Some(snapshot) = view.pane(pane_id).and_then(|pane| pane.snapshot.as_ref()) else {
            continue;
        };
        let content = cell_rect(grid, geometry.content, cell_w, cell_h);
        let hover = ui.input(|input| input.pointer.hover_pos());
        // egui reports a positive delta when the wheel moves the content down
        // (scrolling up), which is the same direction as our history offset, so
        // the sign is used as-is.
        let wheel = ui.input(|input| input.smooth_scroll_delta.y);
        if hover.is_some_and(|pos| content.contains(pos)) && wheel != 0.0 {
            apply_wheel(state, wheel, cell_h);
        }
        let visible_rows = usize::from(geometry.content.rows).min(snapshot.cells.len());
        let render_offset = state.render_offset();
        let render_shift = state.render_shift(cell_h);
        let (start, end) = scroll_window_range(
            state.history.len(),
            snapshot.cells.len(),
            render_offset,
            visible_rows,
        );
        let old_clip = ui.clip_rect();
        ui.set_clip_rect(content);
        for (y, index) in (start..end).enumerate() {
            let Some(row) = scroll_row(&state.history, &snapshot.cells, index) else {
                continue;
            };
            let line: String = row
                .cells
                .iter()
                .filter(|cell| cell.width > 0)
                .map(|cell| cell.text.as_str())
                .collect();
            let color = row
                .cells
                .first()
                .map(|cell| color32(cell.fg))
                .unwrap_or(egui::Color32::LIGHT_GRAY);
            let origin = content.min + egui::vec2(0.0, y as f32 * cell_h + render_shift);
            let max_chars = usize::from(geometry.content.cols);
            let absolute_row = local_absolute_row(state, snapshot, index);
            if let Some(absolute_row) = absolute_row
                && let Some((first, last)) = selection_columns(state.selection, absolute_row)
            {
                let rect = egui::Rect::from_min_size(
                    content.min
                        + egui::vec2(first.min(max_chars) as f32 * cell_w, y as f32 * cell_h),
                    egui::vec2(
                        (last
                            .min(max_chars.saturating_sub(1))
                            .saturating_sub(first.min(max_chars))
                            + 1) as f32
                            * cell_w,
                        cell_h,
                    ),
                );
                ui.painter().rect_filled(
                    rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(96, 165, 250, 90),
                );
            }
            let shown: String = line.chars().take(max_chars).collect();
            ui.painter().text(
                origin,
                egui::Align2::LEFT_TOP,
                shown,
                egui::FontId::monospace(FONT_SIZE.min(cell_h)),
                color,
            );
        }
        if let Some(pos) = click_pos
            && !button_clicked
            && content.contains(pos)
            && let Some(absolute_row) = pointer_row(state, snapshot, content, cell_h, pos)
        {
            let col = ((pos.x - content.min.x) / cell_w.max(1.0)) as usize;
            state.selection = Some(Selection {
                anchor: CellPos {
                    row: absolute_row,
                    col,
                },
                head: CellPos {
                    row: absolute_row,
                    col,
                },
            });
        }
        if pointer.1
            && state.selection.is_some()
            && let Some(pos) = pointer.3
            && content.contains(pos)
            && let Some(absolute_row) = pointer_row(state, snapshot, content, cell_h, pos)
        {
            let col = ((pos.x - content.min.x) / cell_w.max(1.0)) as usize;
            if let Some(selection) = state.selection.as_mut() {
                selection.head = CellPos {
                    row: absolute_row,
                    col,
                };
            }
        }
        if pointer.2
            && copy_on_selection
            && actions.copy.is_none()
            && pointer.3.is_some_and(|pos| content.contains(pos))
            && state
                .selection
                .is_some_and(|selection| selection.anchor != selection.head)
            && let Some(text) = selection_text(state, snapshot)
            && !text.is_empty()
        {
            actions.copy = Some(text);
        }
        if let Some(pos) = click_pos
            && actions.link.is_none()
            && content.contains(pos)
        {
            let x = ((pos.x - content.min.x) / cell_w.max(1.0)) as usize;
            let y = ((pos.y - content.min.y) / cell_h.max(1.0)) as usize;
            if let Some(index) = (start..end).nth(y)
                && let Some(row) = scroll_row(&state.history, &snapshot.cells, index)
                && let Some(cell) = row.cells.get(x)
                && let Some(uri) = cell.link.as_deref()
            {
                actions.link = Some(uri.to_owned());
            }
        }
        if state.offset == 0 && snapshot.cursor.visible {
            let cursor_rect = egui::Rect::from_min_size(
                content.min
                    + egui::vec2(
                        f32::from(snapshot.cursor.x) * cell_w,
                        f32::from(snapshot.cursor.y) * cell_h + render_shift,
                    ),
                egui::vec2(cell_w, cell_h),
            );
            ui.painter().rect_filled(
                cursor_rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(120, 180, 255, 90),
            );
        }
        ui.set_clip_rect(old_clip);
        if let Some((track, thumb)) =
            scrollbar_rect(content, state.history.len(), state.offset, visible_rows)
        {
            ui.painter()
                .rect_filled(track, 2.0, egui::Color32::from_gray(32));
            ui.painter()
                .rect_filled(thumb, 2.0, egui::Color32::from_rgb(96, 165, 250));
        }
    }
    actions.drag = draw_splitters(ui, layout, grid, cell_w, cell_h);
    actions
}

fn draw_splitters(
    ui: &mut egui::Ui,
    layout: &Layout,
    grid: egui::Rect,
    cell_w: f32,
    cell_h: f32,
) -> Option<SplitDrag> {
    let mut splits = Vec::new();
    collect_splits(
        &layout.root,
        &layout.panes,
        grid,
        cell_w,
        cell_h,
        &mut splits,
    );
    let mut drag = None;
    for (index, (axis, strip, union, node)) in splits.into_iter().enumerate() {
        let response = ui.interact(
            strip,
            egui::Id::unique(("stitch-mux-split", index)),
            egui::Sense::drag(),
        );
        if response.hovered() || response.dragged() {
            ui.painter().rect_filled(
                strip,
                0.0,
                egui::Color32::from_rgba_unmultiplied(96, 165, 250, 140),
            );
        }
        if (response.dragged() || response.drag_stopped())
            && let Some(pos) = ui.input(|input| input.pointer.latest_pos())
        {
            let ratio = match axis {
                SplitAxis::Columns => (pos.x - union.min.x) / union.width().max(1.0),
                SplitAxis::Rows => (pos.y - union.min.y) / union.height().max(1.0),
            };
            drag = Some(SplitDrag {
                started: response.drag_started(),
                stopped: response.drag_stopped(),
                node: node.clone(),
                ratio_bps: (ratio.clamp(0.01, 0.99) * 10_000.0) as u16,
            });
        }
    }
    drag
}

fn collect_splits(
    node: &LayoutNode,
    geometries: &[PaneGeometry],
    grid: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    out: &mut Vec<(SplitAxis, egui::Rect, egui::Rect, LayoutNode)>,
) {
    let LayoutNode::Split {
        axis,
        first,
        second,
        ..
    } = node
    else {
        return;
    };
    let first_bounds = node_bounds(first, geometries, grid, cell_w, cell_h);
    let second_bounds = node_bounds(second, geometries, grid, cell_w, cell_h);
    if let (Some(first), Some(second)) = (first_bounds, second_bounds) {
        let union = first.union(second);
        let strip = match axis {
            SplitAxis::Columns => egui::Rect::from_min_size(
                egui::pos2(first.max.x - 2.0, union.min.y),
                egui::vec2(4.0, union.height()),
            ),
            SplitAxis::Rows => egui::Rect::from_min_size(
                egui::pos2(union.min.x, first.max.y - 2.0),
                egui::vec2(union.width(), 4.0),
            ),
        };
        out.push((*axis, strip, union, node.clone()));
    }
    collect_splits(first, geometries, grid, cell_w, cell_h, out);
    collect_splits(second, geometries, grid, cell_w, cell_h, out);
}

fn node_bounds(
    node: &LayoutNode,
    geometries: &[PaneGeometry],
    grid: egui::Rect,
    cell_w: f32,
    cell_h: f32,
) -> Option<egui::Rect> {
    match node {
        LayoutNode::Empty => None,
        LayoutNode::Leaf { pane_id } => geometries
            .iter()
            .find(|geometry| geometry.pane_id == *pane_id)
            .map(|geometry| cell_rect(grid, geometry.outer, cell_w, cell_h)),
        LayoutNode::Split { first, second, .. } => {
            let first = node_bounds(first, geometries, grid, cell_w, cell_h)?;
            let second = node_bounds(second, geometries, grid, cell_w, cell_h)?;
            Some(first.union(second))
        }
    }
}

fn set_ratio(root: &LayoutNode, target: &LayoutNode, ratio_bps: u16) -> LayoutNode {
    if root == target
        && let LayoutNode::Split {
            axis,
            first,
            second,
            ..
        } = root
    {
        return LayoutNode::split(*axis, ratio_bps, (**first).clone(), (**second).clone());
    }
    match root {
        LayoutNode::Empty => LayoutNode::empty(),
        LayoutNode::Leaf { pane_id } => LayoutNode::leaf(PaneId::new(*pane_id)),
        LayoutNode::Split {
            axis,
            ratio_bps: current,
            first,
            second,
        } => LayoutNode::split(
            *axis,
            *current,
            set_ratio(first, target, ratio_bps),
            set_ratio(second, target, ratio_bps),
        ),
    }
}

fn cell_rect(grid: egui::Rect, cell: Rect, cell_w: f32, cell_h: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        grid.min + egui::vec2(f32::from(cell.x) * cell_w, f32::from(cell.y) * cell_h),
        egui::vec2(f32::from(cell.cols) * cell_w, f32::from(cell.rows) * cell_h),
    )
}

fn status_text(view: &SessionView) -> String {
    let bell = view.panes.values().any(|pane| pane.bell);
    format!(
        "session: {} {:?} {}x{} r{} clients: {}  {}  bell: {}",
        view.name,
        view.connection,
        view.size.cols,
        view.size.rows,
        view.layout_revision,
        view.attached_clients,
        activity_label(&view.activity),
        bell
    )
}

fn input_focus() -> egui::Id {
    egui::Id::unique("stitch-mux-pane-input")
}

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_RETRY)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn status_badge(status: Option<&ProgramStatus>) -> &'static str {
    match status.map(|status| (status.state, status.kind)) {
        Some((ProgramState::Error, _)) => "[!]",
        Some((ProgramState::Blocked, Some(ProgramStatusKind::Question))) => "[?]",
        Some((ProgramState::Blocked, Some(ProgramStatusKind::Permission))) => "[key]",
        Some((ProgramState::Blocked, Some(ProgramStatusKind::Auth))) => "[auth]",
        Some((ProgramState::Blocked, None)) => "[wait]",
        Some((ProgramState::Working, _)) => "[~]",
        Some((ProgramState::Done, _)) => "[ok]",
        Some((ProgramState::Idle, _)) | None => "",
    }
}

fn activity_label(activity: &ActivitySummary) -> String {
    format!(
        "working {} blocked {} done {} error {}",
        activity.working, activity.blocked, activity.done, activity.error
    )
}

fn session_matches(
    info: &SessionInfo,
    filter: &str,
    attention_only: bool,
    now_ms: u64,
    seen: &HashMap<String, u64>,
) -> bool {
    let query = filter.trim().to_ascii_lowercase();
    let text_match = query.is_empty()
        || info.name.to_ascii_lowercase().contains(&query)
        || info
            .panes
            .iter()
            .any(|pane| pane.title.to_ascii_lowercase().contains(&query));
    if !text_match {
        return false;
    }
    if !attention_only {
        return true;
    }
    let unread = info.last_activity_unix_ms
        > seen
            .get(&info.name)
            .copied()
            .unwrap_or(info.last_activity_unix_ms);
    info.activity.working > 0
        || info.activity.blocked > 0
        || info.activity.error > 0
        || unread
        || age_label_at(info.last_activity_unix_ms, now_ms) == "just now"
}

fn session_label(
    info: &SessionInfo,
    lost_session: Option<&str>,
    now_ms: u64,
    unread: bool,
    panes: &str,
) -> String {
    let lost = if lost_session == Some(info.name.as_str()) {
        " [last session lost]"
    } else {
        ""
    };
    let new = if unread { " [new]" } else { "" };
    format!(
        "{}{}{} — {}x{}, {} pane(s), {} client(s), {}
last activity {}
{}",
        info.name,
        lost,
        new,
        info.cols,
        info.rows,
        info.pane_count,
        info.attached_clients,
        activity_label(&info.activity),
        age_label_at(info.last_activity_unix_ms, now_ms),
        panes
    )
}

fn age_label_at(unix_ms: u64, now_ms: u64) -> String {
    if unix_ms == 0 {
        return "unknown".into();
    }
    let seconds = now_ms.saturating_sub(unix_ms) / 1000;
    match seconds {
        0..=4 => "just now".into(),
        5..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// The 16 base ANSI colors, used for named/indexed cells.
const ANSI_PALETTE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0xcd, 0x00, 0x00],
    [0x00, 0xcd, 0x00],
    [0xcd, 0xcd, 0x00],
    [0x00, 0x00, 0xee],
    [0xcd, 0x00, 0xcd],
    [0x00, 0xcd, 0xcd],
    [0xe5, 0xe5, 0xe5],
    [0x7f, 0x7f, 0x7f],
    [0xff, 0x00, 0x00],
    [0x00, 0xff, 0x00],
    [0xff, 0xff, 0x00],
    [0x5c, 0x5c, 0xff],
    [0xff, 0x00, 0xff],
    [0x00, 0xff, 0xff],
    [0xff, 0xff, 0xff],
];

fn color32(color: Color) -> egui::Color32 {
    match color {
        Color::Default => egui::Color32::LIGHT_GRAY,
        Color::Indexed(index) => {
            let [r, g, b] = if index < 16 {
                ANSI_PALETTE[usize::from(index)]
            } else if index < 232 {
                let n = index - 16;
                let level = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
                [level(n / 36), level((n / 6) % 6), level(n % 6)]
            } else {
                let value = 8 + (index - 232) * 10;
                [value, value, value]
            };
            egui::Color32::from_rgb(r, g, b)
        }
        Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
    }
}

fn handle_input(ui: &mut egui::Ui, app: &mut MuxApp, grid: egui::Rect, view: &SessionView) -> bool {
    let Some(client) = app.client.as_ref() else {
        return false;
    };
    ui.interact(grid, input_focus(), egui::Sense::focusable_noninteractive());
    let focused = ui.memory(|memory| memory.focused());
    if focused.is_none() {
        ui.memory_mut(|memory| memory.request_focus(input_focus()));
    } else if focused != Some(input_focus()) {
        return false;
    }
    let events = ui.input(|input| input.events.clone());
    let mut detach = false;

    for event in events {
        if let egui::Event::Key { key, modifiers, .. } = &event {
            let focused_pane = view.focus;
            if *key == egui::Key::A && modifiers.ctrl && modifiers.shift {
                if let Some(pane_id) = focused_pane
                    && let Some(state) = app.pane_scroll.get_mut(&pane_id)
                    && let Some(snapshot) =
                        view.pane(pane_id).and_then(|pane| pane.snapshot.as_ref())
                {
                    select_all(state, snapshot);
                }
                continue;
            }
            if *key == egui::Key::C && modifiers.ctrl && !modifiers.alt && !modifiers.command {
                let text: Option<String> = focused_pane.and_then(|pane_id| {
                    app.pane_scroll
                        .get(&pane_id)
                        .zip(view.pane(pane_id).and_then(|pane| pane.snapshot.as_ref()))
                        .and_then(|(state, snapshot)| selection_text(state, snapshot))
                });
                if let Some(text) = text {
                    copy_to_clipboard(ui.ctx(), &text);
                    continue;
                }
            }
            if *key == egui::Key::Escape
                && app.dispatcher.active_mode().is_none()
                && let Some(pane_id) = focused_pane
                && let Some(state) = app.pane_scroll.get_mut(&pane_id)
            {
                state.selection = None;
            }
        }
        match event {
            egui::Event::Text(text) if app.dispatcher.pending().is_empty() => {
                if !text.contains('\n') && !text.contains('\r') {
                    client.send(Outbound::Input(InputEvent::Text(text)));
                }
            }
            egui::Event::Text(text) => {
                for character in text.chars() {
                    if let Ok(spec) = KeySpec::parse(&character.to_string()) {
                        match app.dispatcher.dispatch(spec) {
                            DispatchOutcome::NeedMore { hints } => app.key_hints = hints,
                            DispatchOutcome::Passthrough(keys) => send_keys(client, &keys),
                            DispatchOutcome::ModeEntered(mode) => {
                                app.key_hints = vec![format!("-- {mode} --")];
                            }
                            DispatchOutcome::ModeExited => app.key_hints.clear(),
                            DispatchOutcome::Action(invocation) => {
                                if invocation.action == "help.which-key" {
                                    app.key_hints = app.dispatcher.hints();
                                }
                                if invoke_frontend_action(
                                    client,
                                    &app.actions,
                                    &invocation,
                                    &app.dispatcher,
                                ) {
                                    detach = true;
                                } else {
                                    if invocation.action == "prompt.previous"
                                        || invocation.action == "prompt.next"
                                    {
                                        jump_prompt(
                                            &mut app.pane_scroll,
                                            view,
                                            invocation.action == "prompt.previous",
                                        );
                                    }
                                    client.send(Outbound::Command(CommandRequest {
                                        request_id: app.next_request_id.wrapping_add(1).max(1),
                                        command: Command::InvokeAction {
                                            action: invocation.action,
                                            args: invocation.args,
                                            focus_pane_id: app.view_focus(),
                                        },
                                    }));
                                    app.next_request_id =
                                        app.next_request_id.wrapping_add(1).max(1);
                                }
                            }
                        }
                    }
                }
            }
            egui::Event::Paste(text) => {
                client.send(Outbound::Input(InputEvent::Paste(text)));
            }
            egui::Event::Key {
                key,
                repeat,
                modifiers,
                ..
            } => {
                if !repeat && let Some(name) = egui_key_name(key) {
                    let mut names = Vec::new();
                    if modifiers.ctrl {
                        names.push("ctrl");
                    }
                    if modifiers.alt {
                        names.push("alt");
                    }
                    if modifiers.shift {
                        names.push("shift");
                    }
                    if let Ok(spec) = KeySpec::from_parts(&name, &names) {
                        match app.dispatcher.dispatch(spec) {
                            DispatchOutcome::NeedMore { hints } => app.key_hints = hints,
                            DispatchOutcome::Passthrough(keys) => send_keys(client, &keys),
                            DispatchOutcome::ModeEntered(mode) => {
                                app.key_hints = vec![format!("-- {mode} --")];
                            }
                            DispatchOutcome::ModeExited => app.key_hints.clear(),
                            DispatchOutcome::Action(invocation) => {
                                if invocation.action == "help.which-key" {
                                    app.key_hints = app.dispatcher.hints();
                                }
                                if invoke_frontend_action(
                                    client,
                                    &app.actions,
                                    &invocation,
                                    &app.dispatcher,
                                ) {
                                    detach = true;
                                } else {
                                    if invocation.action == "prompt.previous"
                                        || invocation.action == "prompt.next"
                                    {
                                        jump_prompt(
                                            &mut app.pane_scroll,
                                            view,
                                            invocation.action == "prompt.previous",
                                        );
                                    }
                                    client.send(Outbound::Command(CommandRequest {
                                        request_id: app.next_request_id.wrapping_add(1).max(1),
                                        command: Command::InvokeAction {
                                            action: invocation.action,
                                            args: invocation.args,
                                            focus_pane_id: app.view_focus(),
                                        },
                                    }));
                                    app.next_request_id =
                                        app.next_request_id.wrapping_add(1).max(1);
                                }
                            }
                        }
                        continue;
                    }
                }
                let Some(code) = map_key(key).or_else(|| {
                    if modifiers.ctrl || modifiers.alt || modifiers.command {
                        map_modified_key(key)
                    } else {
                        None
                    }
                }) else {
                    continue;
                };
                let mut bits = 0u16;
                if modifiers.shift {
                    bits |= 1;
                }
                if modifiers.alt {
                    bits |= 2;
                }
                if modifiers.ctrl {
                    bits |= 4;
                }
                if modifiers.command {
                    bits |= 8;
                }
                client.send(Outbound::Input(InputEvent::Key(KeyEvent {
                    code,
                    modifiers: bits,
                    kind: if repeat {
                        KeyEventKind::Repeat
                    } else {
                        KeyEventKind::Press
                    },
                    text: None,
                })));
            }
            _ => {}
        }
    }
    detach
}

fn send_keys(client: &ClientSession, keys: &[KeySpec]) {
    for spec in keys {
        if let Some(event) = key_spec_event(spec, KeyEventKind::Press) {
            client.send(Outbound::Input(InputEvent::Key(event)));
        }
    }
}

/// Returns `true` when the action detached this client locally.
fn copy_to_clipboard(ctx: &egui::Context, text: &str) {
    ctx.copy_text(text.to_owned());
    ctx.send_cmd(egui::OutputCommand::TextSelectionSettled(text.to_owned()));
}

fn local_absolute_row(
    state: &PaneScroll,
    snapshot: &stitch_mux_protocol::PaneSnapshot,
    index: usize,
) -> Option<u64> {
    let visible_len = snapshot.cells.len() as u64;
    let oldest = snapshot
        .total_lines
        .saturating_sub(state.history.len() as u64 + visible_len);
    (index as u64)
        .checked_add(oldest)
        .filter(|row| *row < snapshot.total_lines)
}

impl PaneScroll {
    fn render_offset(&self) -> usize {
        self.offset + usize::from(self.fraction > 0.0)
    }

    fn render_shift(&self, cell_h: f32) -> f32 {
        if self.fraction > 0.0 {
            -(1.0 - self.fraction) * cell_h
        } else {
            0.0
        }
    }
}

fn apply_wheel(state: &mut PaneScroll, wheel: f32, cell_h: f32) {
    let max_offset = state.history.len() as f32;
    let desired = state.offset as f32 + state.fraction + wheel / cell_h.max(1.0);
    let clamped = desired.clamp(0.0, max_offset);
    state.offset = clamped as usize;
    state.fraction = clamped - state.offset as f32;
    if state.fraction < 0.0 || state.fraction >= 1.0 {
        state.fraction = 0.0;
    }
}

fn pointer_row(
    state: &PaneScroll,
    snapshot: &stitch_mux_protocol::PaneSnapshot,
    content: egui::Rect,
    cell_h: f32,
    pos: egui::Pos2,
) -> Option<u64> {
    let visible_rows = usize::from(snapshot.rows).min(snapshot.cells.len());
    let (start, end) = scroll_window_range(
        state.history.len(),
        snapshot.cells.len(),
        state.render_offset(),
        visible_rows,
    );
    let y = ((pos.y - content.min.y - state.render_shift(cell_h)) / cell_h.max(1.0)).floor();
    if y < 0.0 {
        return None;
    }
    (start..end)
        .nth(y as usize)
        .and_then(|index| local_absolute_row(state, snapshot, index))
}

fn selection_columns(selection: Option<Selection>, absolute_row: u64) -> Option<(usize, usize)> {
    let selection = selection?;
    let (start, end) = selection.ordered();
    if absolute_row < start.row || absolute_row > end.row {
        return None;
    }
    let first = if absolute_row == start.row {
        start.col
    } else {
        0
    };
    let last = if absolute_row == end.row {
        end.col
    } else {
        usize::MAX
    };
    Some((first, last))
}

fn selection_text(
    state: &PaneScroll,
    snapshot: &stitch_mux_protocol::PaneSnapshot,
) -> Option<String> {
    let selection = state.selection?;
    let (start, end) = selection.ordered();
    let visible_len = snapshot.cells.len() as u64;
    let oldest = snapshot
        .total_lines
        .saturating_sub(state.history.len() as u64 + visible_len);
    if start.row < oldest || end.row >= snapshot.total_lines {
        return None;
    }
    let mut lines = Vec::new();
    for row in start.row..=end.row.min(start.row.saturating_add(999)) {
        let index = (row.saturating_sub(oldest)) as usize;
        let Some(cells) = scroll_row(&state.history, &snapshot.cells, index) else {
            continue;
        };
        let (first, last) = selection_columns(Some(selection), row)?;
        let last = last.min(cells.cells.len().saturating_sub(1));
        if first > last {
            continue;
        }
        let text: String = cells.cells[first..=last]
            .iter()
            .filter(|cell| cell.width > 0)
            .map(|cell| cell.text.as_str())
            .collect();
        lines.push(text);
    }
    let joined = lines.join("\n");
    (joined.len() <= 1024 * 1024).then_some(joined)
}

fn select_all(state: &mut PaneScroll, snapshot: &stitch_mux_protocol::PaneSnapshot) {
    let oldest = snapshot
        .total_lines
        .saturating_sub(state.history.len() as u64 + snapshot.cells.len() as u64);
    let last_col = snapshot
        .cells
        .first()
        .map_or(0, |row| row.cells.len().saturating_sub(1));
    state.selection = Some(Selection {
        anchor: CellPos {
            row: oldest,
            col: 0,
        },
        head: CellPos {
            row: snapshot.total_lines.saturating_sub(1),
            col: last_col,
        },
    });
}

/// Only ubiquitous web/mail schemes open without a confirmation dialog. A
/// child-controlled custom scheme must never be launched implicitly.
fn safe_link_scheme(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
}

/// Move the focused pane's local scrollback window to a shell prompt mark.
///
/// Marks are absolute screen rows published by the daemon; this never parses
/// screen text. A target inside the live viewport resolves to offset zero.
fn jump_prompt(scroll: &mut HashMap<u64, PaneScroll>, view: &SessionView, previous: bool) {
    let Some(pane_id) = view.focus else {
        return;
    };
    let Some(snapshot) = view.pane(pane_id).and_then(|pane| pane.snapshot.as_ref()) else {
        return;
    };
    let Some(marks) = snapshot.shell_marks.as_ref() else {
        return;
    };
    let rows = &marks.prompt_rows;
    if rows.is_empty() {
        return;
    }
    let state = scroll.entry(pane_id).or_default();
    let end = snapshot.total_lines;
    let anchor = end.saturating_sub(state.offset as u64);
    let target = if previous {
        rows.iter()
            .rev()
            .find(|row| **row < anchor)
            .copied()
            .or_else(|| rows.last().copied())
    } else {
        rows.iter()
            .find(|row| **row > anchor)
            .copied()
            .or_else(|| rows.first().copied())
    };
    let Some(target) = target else {
        return;
    };
    state.offset = end.saturating_sub(target).min(state.history.len() as u64) as usize;
}

fn invoke_frontend_action(
    client: &ClientSession,
    _actions: &[ActionInfo],
    invocation: &stitch_mux_core::keymap::ActionInvocation,
    dispatcher: &KeyDispatcher,
) -> bool {
    match invocation.action.as_str() {
        "client.detach" => true,
        "help.which-key" => {
            let _ = client;
            let _ = dispatcher;
            false
        }
        _ => false,
    }
}

fn map_key(key: egui::Key) -> Option<KeyCode> {
    use egui::Key;
    match key {
        Key::Enter => Some(KeyCode::Enter),
        Key::Tab => Some(KeyCode::Tab),
        Key::Backspace => Some(KeyCode::Backspace),
        Key::Escape => Some(KeyCode::Esc),
        Key::ArrowUp => Some(KeyCode::Up),
        Key::ArrowDown => Some(KeyCode::Down),
        Key::ArrowLeft => Some(KeyCode::Left),
        Key::ArrowRight => Some(KeyCode::Right),
        Key::Home => Some(KeyCode::Home),
        Key::End => Some(KeyCode::End),
        Key::PageUp => Some(KeyCode::PageUp),
        Key::PageDown => Some(KeyCode::PageDown),
        Key::Insert => Some(KeyCode::Insert),
        Key::Delete => Some(KeyCode::Delete),
        Key::F1 => Some(KeyCode::F(1)),
        Key::F2 => Some(KeyCode::F(2)),
        Key::F3 => Some(KeyCode::F(3)),
        Key::F4 => Some(KeyCode::F(4)),
        Key::F5 => Some(KeyCode::F(5)),
        Key::F6 => Some(KeyCode::F(6)),
        Key::F7 => Some(KeyCode::F(7)),
        Key::F8 => Some(KeyCode::F(8)),
        Key::F9 => Some(KeyCode::F(9)),
        Key::F10 => Some(KeyCode::F(10)),
        Key::F11 => Some(KeyCode::F(11)),
        Key::F12 => Some(KeyCode::F(12)),
        _ => None,
    }
}

/// Letters, digits, and punctuation for modified key events. Unmodified
/// printable keys come through `Event::Text` instead.
fn map_modified_key(key: egui::Key) -> Option<KeyCode> {
    use egui::Key;
    let code = match key {
        Key::A => 'a',
        Key::B => 'b',
        Key::C => 'c',
        Key::D => 'd',
        Key::E => 'e',
        Key::F => 'f',
        Key::G => 'g',
        Key::H => 'h',
        Key::I => 'i',
        Key::J => 'j',
        Key::K => 'k',
        Key::L => 'l',
        Key::M => 'm',
        Key::N => 'n',
        Key::O => 'o',
        Key::P => 'p',
        Key::Q => 'q',
        Key::R => 'r',
        Key::S => 's',
        Key::T => 't',
        Key::U => 'u',
        Key::V => 'v',
        Key::W => 'w',
        Key::X => 'x',
        Key::Y => 'y',
        Key::Z => 'z',
        Key::Num0 => '0',
        Key::Num1 => '1',
        Key::Num2 => '2',
        Key::Num3 => '3',
        Key::Num4 => '4',
        Key::Num5 => '5',
        Key::Num6 => '6',
        Key::Num7 => '7',
        Key::Num8 => '8',
        Key::Num9 => '9',
        Key::Space => ' ',
        Key::Comma => ',',
        Key::Period => '.',
        Key::Minus => '-',
        Key::Equals => '=',
        Key::Plus => '+',
        Key::Semicolon => ';',
        Key::Quote => '\'',
        Key::Backslash => '\\',
        Key::Slash => '/',
        Key::Backtick => '`',
        Key::OpenBracket => '[',
        Key::CloseBracket => ']',
        Key::Pipe => '|',
        Key::Colon => ':',
        Key::Questionmark => '?',
        Key::Exclamationmark => '!',
        Key::OpenCurlyBracket => '{',
        Key::CloseCurlyBracket => '}',
        _ => return None,
    };
    Some(KeyCode::Char(code))
}

/// Half-open index range in `history ++ visible` for a wheel offset.
fn scroll_window_range(
    history_len: usize,
    visible_len: usize,
    offset: usize,
    rows: usize,
) -> (usize, usize) {
    let total = history_len + visible_len;
    if rows == 0 || total == 0 {
        return (0, 0);
    }
    let rows = rows.min(total);
    if offset == 0 {
        let start = history_len.min(total - rows);
        return (start, start + rows);
    }
    let end = total.saturating_sub(offset).clamp(rows, total);
    (end - rows, end)
}

fn scroll_row<'a>(history: &'a VecDeque<Row>, visible: &'a [Row], index: usize) -> Option<&'a Row> {
    if index < history.len() {
        history.get(index)
    } else {
        visible.get(index - history.len())
    }
}

fn scrollbar_rect(
    content: egui::Rect,
    history_len: usize,
    offset: usize,
    visible_len: usize,
) -> Option<(egui::Rect, egui::Rect)> {
    let total = history_len + visible_len;
    if visible_len == 0 || total <= visible_len {
        return None;
    }
    let track = egui::Rect::from_min_size(
        egui::pos2(content.max.x - 4.0, content.min.y),
        egui::vec2(4.0, content.height()),
    );
    let thumb_h = (track.height() * visible_len as f32 / total as f32)
        .max(6.0)
        .min(track.height());
    let progress = (history_len.saturating_sub(offset)) as f32 / (total - visible_len) as f32;
    let thumb = egui::Rect::from_min_size(
        egui::pos2(
            track.min.x,
            track.min.y + (track.height() - thumb_h) * progress.clamp(0.0, 1.0),
        ),
        egui::vec2(track.width(), thumb_h),
    );
    Some((track, thumb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stitch_mux_core::{ActivitySummary, Cell, Cursor, Modes, Row};
    use stitch_mux_protocol::{PaneInfo, PanePlacement, PaneSnapshot, ShellMarks};

    fn info(name: &str) -> SessionInfo {
        SessionInfo {
            id: 1,
            name: name.into(),
            cols: 120,
            rows: 40,
            pane_count: 2,
            detached_pane_count: 0,
            window_count: 1,
            attached_clients: 3,
            created_at_unix_ms: 1_000,
            last_activity_unix_ms: 121_000,
            activity: ActivitySummary {
                working: 1,
                ..ActivitySummary::default()
            },
            panes: vec![PaneInfo {
                id: 7,
                title: "vim".into(),
                cwd: None,
                cols: 60,
                rows: 38,
                alive: true,
                placement: PanePlacement::Tiled,
                window_id: Some(1),
                owner_client_id: None,
                program_status: None,
            }],
        }
    }

    fn snapshot_with_marks(total_lines: u64, prompts: &[u64]) -> PaneSnapshot {
        PaneSnapshot {
            pane_id: 7,
            seq: 1,
            cols: 80,
            rows: 24,
            cells: vec![Row {
                cells: Vec::new(),
                wrapped: false,
            }],
            cursor: Cursor::default(),
            modes: Modes::default(),
            title: "sh".into(),
            cwd: None,
            foreground: None,
            shell_marks: Some(ShellMarks {
                prompt_rows: prompts.to_vec(),
                command_rows: Vec::new(),
            }),
            program_status: None,
            notification: None,
            progress: None,

            alt_screen: false,
            scrollback_len: 20,
            total_lines,
            scrollback_bytes: 0,
        }
    }

    #[test]
    fn smooth_wheel_deltas_accumulate_into_fractional_rows() {
        let mut state = PaneScroll::default();
        for _ in 0..20 {
            state.history.push_back(Row {
                cells: Vec::new(),
                wrapped: false,
            });
        }
        apply_wheel(&mut state, 4.0, 20.0); // 0.2 rows up
        assert_eq!(state.offset, 0);
        assert!((state.fraction - 0.2).abs() < 0.001);
        assert_eq!(state.render_offset(), 1);
        apply_wheel(&mut state, 16.0, 20.0); // now exactly one row
        assert_eq!(state.offset, 1);
        assert!(state.fraction.abs() < 0.001);
        apply_wheel(&mut state, -10.0, 20.0); // half a row back down
        assert_eq!(state.offset, 0);
        assert!((state.fraction - 0.5).abs() < 0.001);
    }

    #[test]
    fn selection_extracts_text_across_scrollback_and_skips_continuations() {
        let row = |text: &str| Row {
            cells: vec![Cell {
                text: text.into(),
                width: 1,
                ..Cell::blank()
            }],
            wrapped: false,
        };
        let snapshot = stitch_mux_protocol::PaneSnapshot {
            pane_id: 1,
            seq: 1,
            cols: 2,
            rows: 2,
            cells: vec![row("c"), row("d")],
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
            scrollback_len: 2,
            total_lines: 4,
            scrollback_bytes: 0,
        };
        let mut state = PaneScroll::default();
        state.history.push_back(row("a"));
        state.history.push_back(row("b"));
        state.visible = snapshot.cells.clone();
        state.selection = Some(Selection {
            anchor: CellPos { row: 1, col: 0 },
            head: CellPos { row: 2, col: 0 },
        });
        assert_eq!(selection_text(&state, &snapshot).as_deref(), Some("b\nc"));

        select_all(&mut state, &snapshot);
        assert_eq!(
            selection_text(&state, &snapshot).as_deref(),
            Some("a\nb\nc\nd")
        );
    }

    #[test]
    fn only_web_and_mail_links_open_without_confirmation() {
        assert!(safe_link_scheme("HTTPS://example.com"));
        assert!(safe_link_scheme("http://example.com"));
        assert!(safe_link_scheme("mailto:ops@example.com"));
        assert!(!safe_link_scheme("file:///etc/passwd"));
        assert!(!safe_link_scheme("custom:payload"));
        assert_eq!(
            Cell {
                text: "x".into(),
                link: Some("https://example.com".into()),
                ..Cell::blank()
            }
            .link
            .as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn prompt_jumps_use_absolute_marks_and_clamp_to_local_scrollback() {
        let mut view = SessionView {
            focus: Some(7),
            ..SessionView::default()
        };
        view.panes.insert(
            7,
            stitch_mux_client::PaneView {
                pane_id: 7,
                snapshot: Some(snapshot_with_marks(100, &[10, 50, 90])),
                last_seq: 1,
                bell: false,
            },
        );
        let mut scroll: HashMap<u64, PaneScroll> = HashMap::new();
        let state = scroll.entry(7).or_default();
        for _ in 0..20 {
            state.history.push_back(Row {
                cells: Vec::new(),
                wrapped: false,
            });
        }

        jump_prompt(&mut scroll, &view, true);
        assert_eq!(scroll[&7].offset, 10); // target row 90
        jump_prompt(&mut scroll, &view, true);
        assert_eq!(scroll[&7].offset, 20); // target row 50, clamped
        jump_prompt(&mut scroll, &view, false);
        assert_eq!(scroll[&7].offset, 10); // next mark is row 90
        jump_prompt(&mut scroll, &view, false);
        assert_eq!(scroll[&7].offset, 20); // wrap to row 10, clamped
    }

    #[test]
    fn retry_backoff_doubles_and_caps_at_eight_seconds() {
        let mut backoff = INITIAL_RETRY;
        for expected in [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(8),
        ] {
            backoff = next_backoff(backoff);
            assert_eq!(backoff, expected);
        }
    }

    #[test]
    fn picker_marks_only_the_lost_session_and_shows_activity() {
        let label = session_label(&info("demo"), Some("demo"), 181_000, false, "vim (7) 60x38");
        assert!(label.contains("demo [last session lost]"), "{label}");
        assert!(
            label.contains("120x40, 2 pane(s), 3 client(s), working 1"),
            "{label}"
        );
        assert!(label.contains("last activity 1m ago"), "{label}");
        assert!(label.contains("vim (7) 60x38"), "{label}");

        let other = session_label(&info("other"), Some("demo"), 181_000, false, "");
        assert!(!other.contains("last session lost"), "{other}");
    }

    #[test]
    fn status_badges_are_textual_and_distinguish_attention_states() {
        let status = |state, kind| {
            Some(ProgramStatus {
                state,
                kind,
                ..ProgramStatus::default()
            })
        };
        assert_eq!(status_badge(None), "");
        assert_eq!(
            status_badge(status(ProgramState::Working, None).as_ref()),
            "[~]"
        );
        assert_eq!(
            status_badge(status(ProgramState::Done, None).as_ref()),
            "[ok]"
        );
        assert_eq!(
            status_badge(status(ProgramState::Error, None).as_ref()),
            "[!]"
        );
        assert_eq!(
            status_badge(status(ProgramState::Blocked, Some(ProgramStatusKind::Question)).as_ref(),),
            "[?]"
        );
        assert_eq!(
            status_badge(
                status(ProgramState::Blocked, Some(ProgramStatusKind::Permission)).as_ref(),
            ),
            "[key]"
        );
        assert_eq!(
            activity_label(&ActivitySummary {
                working: 1,
                blocked: 2,
                error: 3,
                ..ActivitySummary::default()
            }),
            "working 1 blocked 2 done 0 error 3"
        );
    }

    #[test]
    fn picker_filters_by_name_and_attention() {
        let info = info("demo");
        assert!(session_matches(&info, "dem", false, 0, &HashMap::new()));
        assert!(!session_matches(&info, "other", false, 0, &HashMap::new()));
        assert!(session_matches(&info, "", true, 0, &HashMap::new()));
        let mut quiet = info.clone();
        quiet.activity = ActivitySummary::default();
        quiet.last_activity_unix_ms = 0;
        let mut seen = HashMap::new();
        seen.insert("demo".to_owned(), 10_u64);
        assert!(!session_matches(&quiet, "", true, 60_000, &seen));
        let label = session_label(&quiet, None, 60_000, true, "");
        assert!(label.contains("[new]"), "{label}");
    }

    #[test]
    fn activity_age_is_deterministic() {
        assert_eq!(age_label_at(0, 181_000), "unknown");
        assert_eq!(age_label_at(180_000, 181_000), "just now");
        assert_eq!(age_label_at(121_000, 181_000), "1m ago");
        assert_eq!(age_label_at(1_000, 181_000), "3m ago");
    }

    #[test]
    fn status_text_reports_session_state_in_one_line() {
        let mut view = SessionView {
            name: "demo".into(),
            ..Default::default()
        };
        view.size = Size::new(168, 66);
        view.layout_revision = 307;
        view.attached_clients = 1;
        let text = status_text(&view);
        assert!(text.contains("session: demo"), "{text}");
        assert!(text.contains("168x66 r307"), "{text}");
        assert!(text.contains("clients: 1"), "{text}");
        assert!(
            text.contains("working 0 blocked 0 done 0 error 0"),
            "{text}"
        );
        assert!(text.contains("bell: false"), "{text}");
        assert!(!text.contains('\n'));
    }

    #[test]
    fn title_bar_actions_distinguish_both_split_axes() {
        let left_right = PaneAction::Split {
            pane_id: 101,
            axis: SplitAxis::Columns,
        };
        let top_bottom = PaneAction::Split {
            pane_id: 101,
            axis: SplitAxis::Rows,
        };
        assert_ne!(left_right, top_bottom);
        assert_eq!(PaneAction::Close(101), PaneAction::Close(101));
        assert_eq!(
            PaneAction::Zoom {
                pane_id: 101,
                unzoom: true
            },
            PaneAction::Zoom {
                pane_id: 101,
                unzoom: true
            }
        );
    }

    #[test]
    fn scroll_window_clamps_offsets_and_visible_rows() {
        assert_eq!(scroll_window_range(3, 5, 0, 5), (3, 8));
        assert_eq!(scroll_window_range(3, 5, 2, 5), (1, 6));
        assert_eq!(scroll_window_range(3, 5, 3, 5), (0, 5));
        assert_eq!(scroll_window_range(3, 5, 99, 5), (0, 5));
        assert_eq!(scroll_window_range(3, 5, 0, 2), (3, 5));

        let history: VecDeque<Row> = VecDeque::from(vec![Row::default(); 2]);
        let visible = vec![Row::default(); 3];
        assert!(scroll_row(&history, &visible, 4).is_some());
        assert!(scroll_row(&history, &visible, 5).is_none());
    }

    #[test]
    fn modified_keys_reach_the_pane_instead_of_being_dropped() {
        // egui suppresses Event::Text for these, so Ctrl-C and Alt-B must be
        // mapped from the key event itself.
        assert_eq!(map_modified_key(egui::Key::C), Some(KeyCode::Char('c')));
        assert_eq!(map_modified_key(egui::Key::B), Some(KeyCode::Char('b')));
        assert_eq!(map_modified_key(egui::Key::Num1), Some(KeyCode::Char('1')));
        assert_eq!(
            map_modified_key(egui::Key::Backslash),
            Some(KeyCode::Char('\\'))
        );
        assert_eq!(map_modified_key(egui::Key::Space), Some(KeyCode::Char(' ')));
        // Plain letters stay on the text path; mapping them here would double
        // every typed character.
        assert_eq!(map_key(egui::Key::C), None);
        assert_eq!(map_key(egui::Key::Enter), Some(KeyCode::Enter));
    }

    #[test]
    fn indexed_colors_use_the_ansi_palette() {
        assert_eq!(color32(Color::Default), egui::Color32::LIGHT_GRAY);
        assert_eq!(
            color32(Color::Indexed(1)),
            egui::Color32::from_rgb(0xcd, 0, 0)
        );
        assert_eq!(
            color32(Color::Indexed(15)),
            egui::Color32::from_rgb(0xff, 0xff, 0xff)
        );
        // 232 starts the grayscale ramp.
        assert_eq!(
            color32(Color::Indexed(232)),
            egui::Color32::from_rgb(8, 8, 8)
        );
        assert_eq!(
            color32(Color::Rgb(1, 2, 3)),
            egui::Color32::from_rgb(1, 2, 3)
        );
    }

    #[test]
    fn scrollbar_appears_only_with_history_and_moves_with_offset() {
        let content = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 50.0));
        assert!(scrollbar_rect(content, 0, 0, 5).is_none());
        let (track, bottom) = scrollbar_rect(content, 5, 0, 5).unwrap();
        assert!(bottom.max.y > track.max.y - 8.0);
        let (_, top) = scrollbar_rect(content, 5, 5, 5).unwrap();
        assert_eq!(top.min.y, track.min.y);
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;
    use stitch_mux_protocol::{KeyBinding, KeyMode};

    #[test]
    fn wire_keymap_is_converted_for_local_dispatch() {
        let wire = WireKeymap {
            bindings: vec![KeyBinding {
                keys: vec!["C-b".into(), "%".into()],
                action: "pane.split".into(),
                args: Some(serde_json::json!({"axis": "columns", "ratio_bps": 5000})),
                mode: None,
                source: "user".into(),
            }],
            modes: vec![KeyMode {
                name: "resize".into(),
                exclusive: true,
            }],
        };
        let core = core_keymap(&wire).expect("conversion");
        let mut dispatcher = KeyDispatcher::new(core);
        assert!(matches!(
            dispatcher.dispatch(KeySpec::parse("C-b").expect("prefix")),
            DispatchOutcome::NeedMore { .. }
        ));
        assert_eq!(
            dispatcher.dispatch(KeySpec::parse("%").expect("split")),
            DispatchOutcome::Action(stitch_mux_core::keymap::ActionInvocation {
                action: "pane.split".into(),
                args: Some(serde_json::json!({"axis": "columns", "ratio_bps": 5000})),
            })
        );
    }

    #[test]
    fn gui_key_names_are_canonical() {
        assert_eq!(egui_key_name(egui::Key::ArrowLeft).as_deref(), Some("Left"));
        assert_eq!(egui_key_name(egui::Key::Escape).as_deref(), Some("Esc"));
        assert_eq!(egui_key_name(egui::Key::X).as_deref(), Some("x"));
        assert_eq!(egui_key_name(egui::Key::F12).as_deref(), Some("F12"));
    }
}
