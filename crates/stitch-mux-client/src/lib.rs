//! Toolkit-independent stitch-mux client: connection, handshake, and SessionView.
//!
//! No egui/GPUI/Qt or terminal dependency is allowed in this crate.

mod connection;
mod view;

pub use connection::{ClientError, connect};
pub use view::{ApplyError, ConnectionState, PaneView, SequenceGap, SessionView};
