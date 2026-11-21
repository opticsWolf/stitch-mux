//! Toolkit-independent stitch-mux client: connection, handshake, and SessionView.
//!
//! No egui/GPUI/Qt or terminal dependency is allowed in this crate.

mod connection;
mod session;
mod view;

pub use connection::{ClientError, connect};
pub use session::{AttachState, ClientSession, Outbound, command, size_from_pixels};
pub use view::{ApplyError, ConnectionState, PaneView, SequenceGap, SessionView};
