//! Public Rust surface for stitch-mux.
//!
//! The daemon is authoritative; this library exposes the toolkit-neutral
//! client state machine, the protocol types it consumes, and a narrow daemon
//! entry point. Frontends never parse ANSI or encode keys. The optional `gui`
//! feature adds the egui reference adapter used by `stitch-mux gui`.

pub mod client;
pub mod core;
pub mod protocol;
pub mod server;
pub mod terminal;

#[cfg(feature = "gui")]
pub mod gui;

pub use client::{
    ApplyError, AttachState, ClientError, ClientSession, ConnectionState, Outbound, PaneView,
    SequenceGap, SessionView, command, connect, size_from_pixels,
};
pub use protocol::{
    ClaimMode, Command, CommandRequest, CommandResponse, DEFAULT_CLIENT_CAPABILITIES, Endpoint,
    ErrorInfo, Event, Hello, InputEvent, Layout, MessageKind, RunPlacement, SessionInfo, ViewMode,
    WaitTarget, WindowFocus,
};
pub use server::{ServerError, run_foreground, serve};
