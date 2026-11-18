//! Disposable egui reference adapter.
//!
//! This crate proves the structured protocol and detach/reattach loop. The
//! product surface is toolkit-independent in `stitch-mux-client`.

use std::error::Error;

use eframe::egui;
use stitch_mux_client::{ClientSession, Outbound, SessionView, command, size_from_pixels};
use stitch_mux_core::{Color, Size};
use stitch_mux_protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, KeyCode, KeyEvent,
    KeyEventKind, ViewMode,
};
use tokio::sync::watch;

const CELL_W: f32 = 8.4;
const CELL_H: f32 = 18.0;
const FONT_SIZE: f32 = 14.0;

/// Launch the reference GUI. `session` may be omitted to show the picker.
pub fn run(endpoint: Endpoint, session: Option<String>) -> Result<(), Box<dyn Error>> {
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
    let sessions: Vec<String> = response
        .sessions
        .into_iter()
        .map(|info| info.name)
        .collect();

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

struct MuxApp {
    endpoint: Endpoint,
    sessions: Vec<String>,
    selected: String,
    client: Option<ClientSession>,
    view_rx: Option<watch::Receiver<SessionView>>,
    handle: tokio::runtime::Handle,
    last_size: Size,
    error: Option<String>,
}

impl MuxApp {
    fn new(
        endpoint: Endpoint,
        session: Option<String>,
        sessions: Vec<String>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        let selected = session
            .or_else(|| sessions.first().cloned())
            .unwrap_or_default();
        let mut app = Self {
            endpoint,
            sessions,
            selected: selected.clone(),
            client: None,
            view_rx: None,
            handle,
            last_size: Size::new(80, 24),
            error: None,
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
                self.selected = name.to_owned();
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn send(&self, outbound: Outbound) {
        if let Some(client) = &self.client {
            client.send(outbound);
        }
    }
}

impl eframe::App for MuxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.client.is_none() {
            ui.heading("stitch-mux");
            if self.sessions.is_empty() {
                ui.label("No sessions. Run `stitch-mux new -s demo -- sh` first.");
            } else {
                ui.label("Pick a session:");
                for name in self.sessions.clone() {
                    if ui.button(&name).clicked() {
                        self.attach(&name);
                    }
                }
            }
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            if ui.button("Refresh").clicked() {
                match self
                    .handle
                    .block_on(command(&self.endpoint, Command::ListSessions))
                {
                    Ok(response) => {
                        self.sessions = response.sessions.into_iter().map(|s| s.name).collect();
                    }
                    Err(err) => self.error = Some(err.to_string()),
                }
            }
            return;
        }

        let Some(receiver) = self.view_rx.as_mut() else {
            return;
        };
        let view = receiver.borrow_and_update().clone();

        let available = ui.available_size();
        let next_size = size_from_pixels(available.x, available.y, CELL_W, CELL_H);
        if next_size != self.last_size {
            self.last_size = next_size;
            self.send(Outbound::Resize(stitch_mux_protocol::ResizeRequest {
                cols: next_size.cols,
                rows: next_size.rows,
                pixel_width: 0,
                pixel_height: 0,
            }));
        }

        draw_panes(ui, &view);

        ui.horizontal(|ui| {
            ui.label(format!("session: {}", view.name));
            ui.label(format!("{:?}", view.connection));
            ui.label(format!("{}x{}", view.size.cols, view.size.rows));
            ui.label(format!(
                "clients: {}  working: {}  bell: {}",
                view.attached_clients,
                view.activity.working,
                view.panes.values().any(|pane| pane.bell)
            ));
            if view.needs_resync {
                ui.colored_label(egui::Color32::YELLOW, "resyncing…");
                self.send(Outbound::Resync);
            }
            if let Some(error) = &view.error {
                ui.colored_label(egui::Color32::RED, &error.message);
            }
            if let Some(exit) = &view.exit {
                ui.label(format!("child: {}", exit.reason));
            }
            if ui.button("Detach").clicked()
                && let Some(client) = self.client.take()
            {
                self.view_rx = None;
                self.handle.spawn(async move { client.detach().await });
            }
        });

        handle_input(ui, &self.handle, self.client.as_ref());
    }

    fn on_exit(&mut self) {
        if let Some(client) = self.client.take() {
            client.send(Outbound::Detach);
        }
    }
}

fn draw_panes(ui: &mut egui::Ui, view: &SessionView) {
    let rect = ui.available_rect_before_wrap();
    let height = (ui.available_height() - 28.0).max(CELL_H);
    let rect = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), height));
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(16));

    let Some(layout) = view.layout.as_ref() else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "waiting for layout…",
            egui::FontId::proportional(16.0),
            egui::Color32::GRAY,
        );
        return;
    };

    for geometry in &layout.panes {
        let Some(snapshot) = view
            .pane(geometry.pane_id)
            .and_then(|pane| pane.snapshot.as_ref())
        else {
            continue;
        };
        for (y, row) in snapshot.cells.iter().enumerate() {
            let mut line = String::new();
            for cell in &row.cells {
                if cell.width > 0 {
                    line.push_str(&cell.text);
                }
            }
            let origin = rect.min + egui::vec2(6.0, 4.0 + y as f32 * CELL_H);
            let color = row
                .cells
                .first()
                .map(|cell| color32(cell.fg))
                .unwrap_or(egui::Color32::LIGHT_GRAY);
            painter.text(
                origin,
                egui::Align2::LEFT_TOP,
                line,
                egui::FontId::monospace(FONT_SIZE),
                color,
            );
        }
        if snapshot.cursor.visible {
            let cursor_rect = egui::Rect::from_min_size(
                rect.min
                    + egui::vec2(
                        6.0 + snapshot.cursor.x as f32 * CELL_W,
                        4.0 + snapshot.cursor.y as f32 * CELL_H,
                    ),
                egui::vec2(CELL_W, CELL_H),
            );
            painter.rect_filled(
                cursor_rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(120, 180, 255, 90),
            );
        }
    }
}

fn color32(color: Color) -> egui::Color32 {
    match color {
        Color::Default => egui::Color32::LIGHT_GRAY,
        Color::Indexed(index) => egui::Color32::from_gray(60 + index.saturating_mul(6)),
        Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
    }
}

fn handle_input(
    ui: &mut egui::Ui,
    handle: &tokio::runtime::Handle,
    client: Option<&ClientSession>,
) {
    let Some(client) = client else {
        return;
    };
    let events = ui.input(|input| input.events.clone());
    let has_focus = ui.memory(|memory| memory.has_focus(egui::Id::unique("stitch-mux-pane")));
    if !has_focus {
        ui.memory_mut(|memory| memory.request_focus(egui::Id::unique("stitch-mux-pane")));
    }
    ui.interact(
        ui.available_rect_before_wrap(),
        egui::Id::unique("stitch-mux-pane"),
        egui::Sense::click(),
    );

    for event in events {
        match event {
            egui::Event::Text(text) if !text.contains('\n') && !text.contains('\r') => {
                client.send(Outbound::Input(InputEvent::Text(text)));
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
                let Some(code) = map_key(key) else {
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
    // Keep the runtime handle used by the event loop explicit; attach and
    // detach both run on it.
    let _ = handle;
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
