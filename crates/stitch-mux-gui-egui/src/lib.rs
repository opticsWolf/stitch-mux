//! Disposable egui reference adapter.
//!
//! This crate proves the structured protocol, tiled layout, per-client focus,
//! and detach/reattach loop. The product surface is toolkit-independent in
//! `stitch-mux-client`.

use std::error::Error;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use stitch_mux_client::{ClientSession, Outbound, SessionView, command, size_from_pixels};
use stitch_mux_core::{Color, Direction, LayoutNode, PaneId, Rect, Size, SplitAxis};
use stitch_mux_protocol::{
    Command, CommandRequest, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, KeyCode,
    KeyEvent, KeyEventKind, Layout, PaneGeometry, SessionInfo, ViewMode,
};
use tokio::sync::watch;

const CELL_W: f32 = 8.4;
const CELL_H: f32 = 18.0;
const FONT_SIZE: f32 = 14.0;
const STATUS_H: f32 = 30.0;
const BORDER_PX: f32 = 2.0;
const INITIAL_RETRY: Duration = Duration::from_millis(500);
const MAX_RETRY: Duration = Duration::from_secs(8);

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
                self.lost_session = None;
                self.retry_backoff = INITIAL_RETRY;
                self.next_retry = None;
            }
            Err(err) => {
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

    fn send_command(&mut self, command: Command) {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.send(Outbound::Command(CommandRequest {
            request_id,
            command,
        }));
    }

    fn refresh_sessions(&mut self) {
        match self
            .handle
            .block_on(command(&self.endpoint, Command::ListSessions))
        {
            Ok(response) => {
                if response.ok {
                    self.sessions = response.sessions;
                } else {
                    self.error = response.error.map(|err| err.message);
                }
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }
}

impl eframe::App for MuxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.client.is_none() {
            self.picker_ui(ui);
            return;
        }

        let Some(receiver) = self.view_rx.as_mut() else {
            return;
        };
        let view = receiver.borrow_and_update().clone();
        self.maybe_reconnect(&view);
        if view.connection == stitch_mux_client::ConnectionState::Lost {
            self.lost_ui(ui, &view);
            return;
        }

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

        let detach = self.status_ui(ui, &view);
        let area = ui.available_rect_before_wrap();
        let grid = egui::Rect::from_min_size(
            area.min,
            egui::vec2(area.width(), (area.height() - STATUS_H).max(CELL_H)),
        );
        let actions = draw_panes(ui, &view, grid);
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
        handle_input(ui, self.client.as_ref(), grid);
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
        if self.sessions.is_empty() {
            ui.label("No sessions. Run `stitch-mux new -s demo -- sh` first.");
        } else {
            ui.label("Pick a session:");
            for info in self.sessions.clone() {
                let panes = info
                    .panes
                    .iter()
                    .map(|pane| format!("{} ({}) {}x{}", pane.title, pane.id, pane.cols, pane.rows))
                    .collect::<Vec<_>>()
                    .join(", ");
                let label =
                    session_label(&info, self.lost_session.as_deref(), now_unix_ms(), &panes);
                if ui.button(label).clicked() {
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
    }

    /// Paint session information in a status bar above the pane grid.
    fn status_ui(&mut self, ui: &mut egui::Ui, view: &SessionView) -> bool {
        let mut detach = false;
        ui.horizontal(|ui| {
            let color = if view.needs_resync {
                egui::Color32::YELLOW
            } else if view.error.is_some() {
                egui::Color32::RED
            } else {
                egui::Color32::LIGHT_GRAY
            };
            ui.colored_label(color, status_text(view));
            if let Some(error) = &view.error {
                ui.colored_label(egui::Color32::RED, &error.message);
            }
            if let Some(exit) = &view.exit {
                ui.label(format!("child: {}", exit.reason));
            }
            if view.needs_resync {
                self.send(Outbound::Resync);
            }
            detach = ui.button("Detach").clicked();
        });
        ui.separator();
        detach
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
}

fn draw_panes(ui: &mut egui::Ui, view: &SessionView, grid: egui::Rect) -> DrawActions {
    let mut actions = DrawActions {
        panes: Vec::new(),
        drag: None,
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
    let zoomed_pane = match layout.root {
        LayoutNode::Leaf { pane_id } => Some(pane_id),
        LayoutNode::Split { .. } => None,
    };

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
        let click_pos = ui.input(|input| {
            input
                .pointer
                .button_clicked(egui::PointerButton::Primary)
                .then(|| input.pointer.latest_pos())
                .flatten()
        });
        let mut button_clicked = false;
        let mut button_x = title_rect.max.x;
        for (label, action) in buttons {
            let rect = egui::Rect::from_min_size(
                egui::pos2(button_x - button_w, button_y),
                egui::vec2(button_w, button_h),
            );
            let hovered = click_pos.is_some_and(|pos| rect.contains(pos));
            ui.painter().rect_filled(
                rect,
                2.0,
                if hovered {
                    egui::Color32::from_rgb(70, 90, 120)
                } else {
                    egui::Color32::from_gray(50)
                },
            );
            ui.painter().text(
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
        let title: String = format!("{} ({})", geometry.title, pane_id)
            .chars()
            .take((title_width / 6.0) as usize)
            .collect();
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
        for (y, row) in snapshot.cells.iter().enumerate() {
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
            let origin = content.min + egui::vec2(0.0, y as f32 * cell_h);
            let max_chars = usize::from(geometry.content.cols);
            let shown: String = line.chars().take(max_chars).collect();
            ui.painter().text(
                origin,
                egui::Align2::LEFT_TOP,
                shown,
                egui::FontId::monospace(FONT_SIZE.min(cell_h)),
                color,
            );
        }
        if snapshot.cursor.visible {
            let cursor_rect = egui::Rect::from_min_size(
                content.min
                    + egui::vec2(
                        f32::from(snapshot.cursor.x) * cell_w,
                        f32::from(snapshot.cursor.y) * cell_h,
                    ),
                egui::vec2(cell_w, cell_h),
            );
            ui.painter().rect_filled(
                cursor_rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(120, 180, 255, 90),
            );
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
        "session: {} {:?} {}x{} r{} clients: {}  working: {}  bell: {}",
        view.name,
        view.connection,
        view.size.cols,
        view.size.rows,
        view.layout_revision,
        view.attached_clients,
        view.activity.working,
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

fn session_label(
    info: &SessionInfo,
    lost_session: Option<&str>,
    now_ms: u64,
    panes: &str,
) -> String {
    let lost = if lost_session == Some(info.name.as_str()) {
        " [last session lost]"
    } else {
        ""
    };
    format!(
        "{}{} — {}x{}, {} pane(s), {} client(s), working {}\nlast activity {}\n{}",
        info.name,
        lost,
        info.cols,
        info.rows,
        info.pane_count,
        info.attached_clients,
        info.activity.working,
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

fn color32(color: Color) -> egui::Color32 {
    match color {
        Color::Default => egui::Color32::LIGHT_GRAY,
        Color::Indexed(index) => egui::Color32::from_gray(60 + index.saturating_mul(6)),
        Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
    }
}

fn handle_input(ui: &mut egui::Ui, client: Option<&ClientSession>, grid: egui::Rect) {
    let Some(client) = client else {
        return;
    };
    // One focus id owns keyboard input for every pane. Pane clicks request it
    // explicitly; never pull focus away from title-bar buttons.
    ui.interact(grid, input_focus(), egui::Sense::focusable_noninteractive());
    let focused = ui.memory(|memory| memory.focused());
    if focused.is_none() {
        ui.memory_mut(|memory| memory.request_focus(input_focus()));
    } else if focused != Some(input_focus()) {
        return;
    }
    let events = ui.input(|input| input.events.clone());

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
                if modifiers.ctrl {
                    let direction = match key {
                        egui::Key::ArrowLeft => Some(Direction::Left),
                        egui::Key::ArrowRight => Some(Direction::Right),
                        egui::Key::ArrowUp => Some(Direction::Up),
                        egui::Key::ArrowDown => Some(Direction::Down),
                        _ => None,
                    };
                    if let Some(direction) = direction {
                        client.send(Outbound::FocusDirection(direction));
                        continue;
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use stitch_mux_core::ActivitySummary;
    use stitch_mux_protocol::{PaneInfo, PanePlacement};

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
        let label = session_label(&info("demo"), Some("demo"), 181_000, "vim (7) 60x38");
        assert!(label.contains("demo [last session lost]"), "{label}");
        assert!(
            label.contains("120x40, 2 pane(s), 3 client(s), working 1"),
            "{label}"
        );
        assert!(label.contains("last activity 1m ago"), "{label}");
        assert!(label.contains("vim (7) 60x38"), "{label}");

        let other = session_label(&info("other"), Some("demo"), 181_000, "");
        assert!(!other.contains("last session lost"), "{other}");
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
        assert!(text.contains("working: 0"), "{text}");
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
}
