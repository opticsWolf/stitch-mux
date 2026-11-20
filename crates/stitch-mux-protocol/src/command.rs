//! Control-command DTOs shared by the CLI and the daemon.

use crate::dto::{
    ActionArgs, ActionInfo, CaptureResponse, ClientInfo, ErrorInfo, FloatingPaneInfo, Keymap,
    Layout, PaneInfo, RunResult, SessionInfo, WaitResult, WaitTarget,
};
use serde::{Deserialize, Serialize};
use stitch_mux_core::{CaptureFormat, FloatBounds, LayoutNode, SplitAxis};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPlacement {
    NewWindow,
    Split {
        pane_id: u64,
        axis: SplitAxis,
        ratio_bps: u16,
    },
    Window {
        window_id: u64,
    },
    Floating {
        window_id: u64,
        bounds: FloatBounds,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowFocus {
    Id(u64),
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimMode {
    Take,
    Request,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    NewSession {
        name: String,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        cols: u16,
        rows: u16,
    },
    EnsureSession {
        name: String,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        cols: u16,
        rows: u16,
    },
    ListSessions,
    ListClients {
        session: String,
    },
    ListPanes {
        session: String,
    },
    KillSession {
        name: String,
    },
    RenameSession {
        name: String,
        new_name: String,
    },
    DetachClient {
        client_id: u64,
    },
    SplitPane {
        pane_id: u64,
        axis: stitch_mux_core::SplitAxis,
        ratio_bps: u16,
    },
    ClosePane {
        pane_id: u64,
    },
    ZoomPane {
        session: String,
        /// `None` unzooms; `Some(pane_id)` makes that pane fill the viewport.
        pane_id: Option<u64>,
    },
    ResizeSession {
        session: String,
        cols: u16,
        rows: u16,
    },
    SendText {
        pane_id: u64,
        text: String,
    },
    SendKeys {
        pane_id: u64,
        bytes: Vec<u8>,
    },
    Run {
        session: String,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        placement: RunPlacement,
        label: Option<String>,
        wait: bool,
        keep_open: bool,
    },
    Wait {
        target: WaitTarget,
        timeout_ms: Option<u64>,
    },
    Capture {
        pane_id: u64,
        start: u64,
        lines: u32,
        format: CaptureFormat,
    },
    SetLayout {
        base_revision: u64,
        root: LayoutNode,
        /// Target window; omitted means the session's active window.
        #[serde(default)]
        window_id: Option<u64>,
        /// Full floating-layer state for that window. `None` leaves it alone.
        #[serde(default)]
        floating: Option<Vec<FloatingPaneInfo>>,
    },
    CreateWindow {
        session: String,
        name: Option<String>,
    },
    CloseWindow {
        window_id: u64,
    },
    RenameWindow {
        window_id: u64,
        name: String,
    },
    MoveWindow {
        window_id: u64,
        index: usize,
    },
    FocusWindow {
        target: WindowFocus,
    },
    FloatPane {
        pane_id: u64,
        bounds: FloatBounds,
    },
    TilePane {
        pane_id: u64,
        window_id: u64,
        anchor_pane_id: Option<u64>,
        axis: SplitAxis,
        ratio_bps: u16,
    },
    SetFloatingPane {
        layer_id: u64,
        #[serde(default)]
        bounds: Option<FloatBounds>,
        #[serde(default)]
        z: Option<u32>,
        #[serde(default)]
        minimized: Option<bool>,
    },
    DetachPane {
        pane_id: u64,
    },
    PlacePane {
        pane_id: u64,
        window_id: Option<u64>,
        anchor_pane_id: Option<u64>,
        axis: stitch_mux_core::SplitAxis,
        ratio_bps: u16,
    },
    ClaimPane {
        pane_id: u64,
        mode: ClaimMode,
    },
    ReleasePane {
        pane_id: u64,
    },
    ListActions,
    GetKeymap,
    InvokeAction {
        action: String,
        args: Option<ActionArgs>,
        /// Lets a short-lived control connection choose the focused pane.
        #[serde(default)]
        focus_pane_id: Option<u64>,
    },
    ReloadKeymap {
        path: Option<String>,
    },
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub request_id: u64,
    pub command: Command,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandResponse {
    pub request_id: u64,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<ErrorInfo>,
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
    #[serde(default)]
    pub clients: Vec<ClientInfo>,
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
    #[serde(default)]
    pub capture: Option<CaptureResponse>,
    /// Fresh authoritative layout when a `SetLayout` base revision was stale.
    #[serde(default)]
    pub layout: Option<Layout>,
    #[serde(default)]
    pub run: Option<RunResult>,
    #[serde(default)]
    pub wait: Option<WaitResult>,
    #[serde(default)]
    pub actions: Vec<ActionInfo>,
    #[serde(default)]
    pub keymap: Option<Keymap>,
}
