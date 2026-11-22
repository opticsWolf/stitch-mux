//! MessagePack DTOs for the v1 wire contract.
//!
//! Fields marked optional in PROTOCOL.md use Option so older peers may omit
//! them. Enum variants are append-only within a protocol version.

use crate::core::{
    ActivitySummary, Attrs, CaptureFormat, Cell, Color, Cursor, FloatBounds, LayoutNode, Modes,
    ProgramStatus, Rect, Row,
};
use crate::protocol::frame::{Frame, FrameError, MessageKind, decode_message};
use serde::{Deserialize, Serialize};

pub use crate::core::PanePlacement;

pub type ActionArgs = serde_json::Value;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    Structured,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub client_name: String,
    pub client_instance: String,
    pub view_mode: ViewMode,
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
    pub capabilities: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HelloAck {
    pub protocol_version: u16,
    pub server_version: String,
    pub capabilities: u64,
    pub sessions: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attach {
    pub session_name: String,
    pub takeover: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attached {
    pub session_id: u64,
    pub session_name: String,
    pub view_mode: ViewMode,
    pub cols: u16,
    pub rows: u16,
    pub status_rows: u16,
    pub attached_clients: u32,
    pub layout_revision: u64,
    pub activity: ActivitySummary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneGeometry {
    pub pane_id: u64,
    pub outer: Rect,
    pub content: Rect,
    pub title: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FloatingPaneInfo {
    pub layer_id: u64,
    pub pane_id: u64,
    pub bounds: FloatBounds,
    pub z: u32,
    pub minimized: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub name: String,
    pub root: LayoutNode,
    #[serde(default)]
    pub zoomed_pane: Option<u64>,
    #[serde(default)]
    pub floating: Vec<FloatingPaneInfo>,
    #[serde(default)]
    pub panes: Vec<PaneGeometry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub session_id: u64,
    pub revision: u64,
    pub cols: u16,
    pub rows: u16,
    pub status_rows: u16,
    #[serde(default)]
    pub attached_clients: u32,
    pub focus: Option<u64>,
    pub root: LayoutNode,
    pub panes: Vec<PaneGeometry>,
    /// Set while the session is zoomed on one pane. `root` is the zoom target
    /// in that case, so a client cannot tell a zoomed session from a
    /// single-pane one without this field.
    #[serde(default)]
    pub zoomed_pane: Option<u64>,
    /// Full workspace for GUI clients. Older peers ignore these fields and
    /// read `root`/`panes` as the active-window projection.
    #[serde(default)]
    pub windows: Vec<WindowInfo>,
    #[serde(default)]
    pub active_window_id: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: Option<u32>,
    pub name: String,
    pub cwd: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellMarks {
    pub prompt_rows: Vec<u64>,
    pub command_rows: Vec<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    #[default]
    Remove,
    Normal,
    Error,
    Indeterminate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub state: ProgressState,
    #[serde(default)]
    pub value: Option<u8>,
    #[serde(default)]
    pub message: Option<String>,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub title: Option<String>,
    pub message: String,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub pane_id: u64,
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Row>,
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground: Option<ProcessInfo>,
    #[serde(default)]
    pub shell_marks: Option<ShellMarks>,
    #[serde(default)]
    pub program_status: Option<ProgramStatus>,
    /// Latest OSC 9/777 notification. Current value, not a queue.
    #[serde(default)]
    pub notification: Option<Notification>,
    /// Latest OSC 9;4 progress record. Current value, not a queue.
    #[serde(default)]
    pub progress: Option<Progress>,
    pub alt_screen: bool,
    pub scrollback_len: u64,
    /// Absolute row count ever written to this pane. Clients use it with
    /// `ShellMarks` to jump to a prompt without parsing screen text.
    #[serde(default)]
    pub total_lines: u64,
    pub scrollback_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RowUpdate {
    pub row: u16,
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneDelta {
    pub pane_id: u64,
    pub seq: u64,
    pub rows: Vec<RowUpdate>,
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground: Option<ProcessInfo>,
    #[serde(default)]
    pub shell_marks: Option<ShellMarks>,
    #[serde(default)]
    pub program_status: Option<ProgramStatus>,
    /// Latest OSC 9/777 notification. Current value, not a queue.
    #[serde(default)]
    pub notification: Option<Notification>,
    /// Latest OSC 9;4 progress record. Current value, not a queue.
    #[serde(default)]
    pub progress: Option<Progress>,
    pub alt_screen: bool,
    pub scrollback_len: u64,
    #[serde(default)]
    pub total_lines: u64,
    pub scrollback_bytes: u64,
    pub bell: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FocusRequest {
    pub pane_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Focus {
    pub pane_id: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyEventKind {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    Null,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: u16,
    pub kind: KeyEventKind,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseKind {
    Down,
    Up,
    Drag,
    Move,
    ScrollUp,
    ScrollDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MouseEvent {
    pub x: u16,
    pub y: u16,
    pub kind: MouseKind,
    pub button: u8,
    pub modifiers: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputEvent {
    Key(KeyEvent),
    Text(String),
    Paste(String),
    Mouse(MouseEvent),
    Focus(bool),
    Bytes(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResizeRequest {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResizeEvent {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SetLayout {
    pub base_revision: u64,
    pub root: crate::core::LayoutNode,
    #[serde(default)]
    pub window_id: Option<u64>,
    #[serde(default)]
    pub floating: Option<Vec<FloatingPaneInfo>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Detach {
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Resync {
    pub reason: String,
}

/// Acknowledgement for the highest contiguous structured pane frame applied
/// by a client.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneFrameAck {
    pub pane_id: u64,
    pub seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardSelection {
    Clipboard,
    Primary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Clipboard {
    pub pane_id: u64,
    pub selection: ClipboardSelection,
    pub text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionExit {
    pub session_id: u64,
    pub pane_id: Option<u64>,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolVersion,
    BadFrame,
    UnknownKind,
    TooLarge,
    NotFound,
    InvalidArgs,
    Unsupported,
    Busy,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CaptureRequest {
    pub pane_id: u64,
    pub start: u64,
    pub lines: u32,
    pub format: CaptureFormat,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapturedRow {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CaptureResponse {
    pub pane_id: u64,
    pub start: u64,
    pub total_lines: u64,
    pub format: CaptureFormat,
    #[serde(default)]
    pub rows: Vec<CapturedRow>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: u64,
    pub title: String,
    pub cwd: Option<String>,
    /// Current PTY geometry, so a session picker can show pane sizes without
    /// attaching. Older peers omit both fields and read zero.
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    pub alive: bool,
    pub placement: PanePlacement,
    pub window_id: Option<u64>,
    pub owner_client_id: Option<u64>,
    pub program_status: Option<ProgramStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub id: u64,
    pub name: String,
    pub view_mode: ViewMode,
    #[serde(default)]
    pub session_id: Option<u64>,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub focus: Option<u64>,
    pub attached_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u64,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub pane_count: u32,
    pub detached_pane_count: u32,
    pub window_count: u32,
    pub attached_clients: u32,
    pub created_at_unix_ms: u64,
    pub last_activity_unix_ms: u64,
    pub activity: ActivitySummary,
    pub panes: Vec<PaneInfo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    pub pane_id: u64,
    pub window_id: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitTarget {
    Session { name: String },
    Pane { pane_id: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WaitResult {
    pub target: WaitTarget,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActionInfo {
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub args_schema: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyBinding {
    pub keys: Vec<String>,
    pub action: String,
    pub args: Option<ActionArgs>,
    /// `None` is the base layer; named modes override base bindings.
    #[serde(default)]
    pub mode: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct KeyMode {
    pub name: String,
    pub exclusive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Keymap {
    pub bindings: Vec<KeyBinding>,
    pub modes: Vec<KeyMode>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubscribeEvents {
    #[serde(default)]
    pub session: Option<String>,
    pub all: bool,
    #[serde(default)]
    pub since_seq: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub session_id: Option<u64>,
    pub kind: String,
    pub payload: ActionArgs,
}

/// Decoded representation of a Render frame. The wire payload is a binary
/// prefix plus raw ANSI bytes; see frame::encode_render.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Render {
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    #[serde(with = "serde_bytes")]
    pub ansi: Vec<u8>,
}

/// Decode a typed payload from a frame and check its kind.
pub fn message<T: serde::de::DeserializeOwned>(
    frame: &Frame,
    expected: MessageKind,
) -> Result<T, FrameError> {
    if frame.kind != expected {
        return Err(FrameError::WrongKind {
            expected,
            actual: frame.kind,
        });
    }
    decode_message(frame)
}

/// Re-exported only so golden fixtures can build cells concisely.
pub fn test_cell(text: &str, fg: Color, bg: Color, attrs: Attrs, width: u8) -> Cell {
    Cell {
        text: text.to_owned().into(),
        fg,
        bg,
        attrs,
        width,
        link: None,
    }
}
