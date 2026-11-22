//! Versioned local wire protocol for stitch-mux.
//!
//! The frame envelope is explicit and bounded; payloads are MessagePack except
//! for Render, which uses a short binary prefix followed by raw ANSI bytes.
//! No toolkit, PTY, or filesystem state lives in the protocol layer.

pub mod capabilities;
pub mod command;
pub mod connection;
pub mod dto;
pub mod endpoint;
pub mod frame;
pub mod logging;
pub mod pane_frame;

#[doc(inline)]
pub use capabilities::{
    DEFAULT_CLIENT_CAPABILITIES, DEFAULT_SERVER_CAPABILITIES, client as client_caps,
    server as server_caps,
};
#[doc(inline)]
pub use command::{ClaimMode, Command, CommandRequest, CommandResponse, RunPlacement, WindowFocus};
#[doc(inline)]
pub use connection::{Connection, FrameReader, FrameWriter};
#[doc(inline)]
pub use dto::{
    ActionArgs, ActionInfo, Attach, Attached, CaptureRequest, CaptureResponse, CapturedRow,
    ClientInfo, Clipboard, ClipboardSelection, Detach, ErrorCode, ErrorInfo, Event,
    FloatingPaneInfo, Focus, FocusRequest, Hello, HelloAck, InputEvent, KeyBinding, KeyCode,
    KeyEvent, KeyEventKind, KeyMode, Keymap, Layout, MouseEvent, MouseKind, Notification,
    PaneDelta, PaneFrameAck, PaneGeometry, PaneInfo, PanePlacement, PaneSnapshot, ProcessInfo,
    Progress, ProgressState, Render, ResizeEvent, ResizeRequest, Resync, RowUpdate, RunResult,
    SessionExit, SessionInfo, SetLayout, ShellMarks, SubscribeEvents, ViewMode, WaitResult,
    WaitTarget, WindowInfo,
};
#[doc(inline)]
pub use endpoint::{Endpoint, EndpointError, Listener, Stream};
#[doc(inline)]
pub use frame::{
    Frame, FrameDecoder, FrameDirection, FrameError, FrameKindSpec, FrameStatus, HEADER_LEN,
    MAX_LARGE_PAYLOAD, MAX_SMALL_PAYLOAD, MessageKind, PROTOCOL_VERSION, WIRE_LEDGER_V1,
    decode_message, decode_render, encode_message, encode_render,
};
#[doc(inline)]
pub use logging::{init_logging, log_path};
#[doc(inline)]
pub use pane_frame::{
    CELL_LEN, DecodedPaneFrame, PaneFrameMeta, PaneRowRecord, PaneWireCell, StructuredPaneFrame,
    decode_pane_frame,
};
