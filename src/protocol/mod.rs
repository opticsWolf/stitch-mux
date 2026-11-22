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

pub use capabilities::{
    DEFAULT_CLIENT_CAPABILITIES, DEFAULT_SERVER_CAPABILITIES, client as client_caps,
    server as server_caps,
};
pub use command::{ClaimMode, Command, CommandRequest, CommandResponse, RunPlacement, WindowFocus};
pub use connection::{Connection, FrameReader, FrameWriter};
pub use dto::*;
pub use endpoint::{Endpoint, EndpointError, Listener, Stream};
pub use frame::{
    Frame, FrameDecoder, FrameDirection, FrameError, FrameKindSpec, FrameStatus, HEADER_LEN,
    MAX_LARGE_PAYLOAD, MAX_SMALL_PAYLOAD, MessageKind, PROTOCOL_VERSION, WIRE_LEDGER_V1,
    decode_message, decode_render, encode_message, encode_render,
};
pub use logging::{init_logging, log_path};
pub use pane_frame::{
    CELL_LEN, DecodedPaneFrame, PaneFrameMeta, PaneRowRecord, PaneWireCell, StructuredPaneFrame,
    decode_pane_frame,
};
