//! Control-command DTOs shared by the CLI and the daemon.

use crate::dto::{
    ActionArgs, ActionInfo, CaptureResponse, ClientInfo, ErrorInfo, Keymap, Layout, PaneInfo,
    RunResult, SessionInfo, WaitResult, WaitTarget,
};
use serde::{Deserialize, Serialize};
use stitch_mux_core::{CaptureFormat, LayoutNode};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPlacement {
    NewWindow,
    Split {
        pane_id: u64,
        axis: stitch_mux_core::SplitAxis,
        ratio_bps: u16,
    },
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
