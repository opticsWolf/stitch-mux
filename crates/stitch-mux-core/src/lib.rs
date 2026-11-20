//! Pure, toolkit-independent domain types for stitch-mux.
//!
//! This crate has no tokio, filesystem, PTY, or UI dependency. It owns IDs,
//! geometry, layout resolution, cell/frame types, status, and capture formats.

pub mod action;
pub mod capture;
pub mod cell;
pub mod frame;
pub mod geometry;
pub mod ids;
pub mod keymap;
pub mod layout;
pub mod status;

pub use action::ActionId;
pub use capture::CaptureFormat;
pub use cell::{Attrs, Cell, Color, Cursor, CursorShape, Modes, Row};
pub use frame::{Frame, changed_rows};
pub use geometry::{Rect, Size};
pub use ids::{ClientId, PaneId, SessionId, WindowId};
pub use layout::{
    Direction, LayoutError, LayoutNode, PanePlacement, ResolvedPane, SplitAxis, close_leaf,
    contains as layout_contains, first_leaf, focus_direction, hit_test, leaves as layout_leaves,
    place_leaf, remove_leaf, resolve as resolve_layout, split_leaf, validate as validate_layout,
};
pub use status::{ActivitySummary, ProgramState, ProgramStatus, ProgramStatusKind};
