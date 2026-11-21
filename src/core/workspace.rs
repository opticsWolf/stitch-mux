//! Pure workspace model: windows, tiled trees, and floating layers.
//!
//! The daemon owns this model. Frontends receive projections and never mutate
//! geometry locally. Floating bounds are normalised `[0, 1]` fractions of the
//! canonical session viewport, so a resize does not require rewriting every
//! layer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::core::{
    LayoutError, LayoutNode, PaneId, Rect, Size, layout_contains, layout_leaves, validate_layout,
};

/// Hard limits keep a hostile or buggy frontend from growing the daemon
/// without bound. Both are far above a normal local workspace.
pub const MAX_WINDOWS: usize = 16;
pub const MAX_LAYERS_PER_WINDOW: usize = 64;
/// Floating bounds are fixed-point fractions of the viewport in basis points.
/// This keeps the wire form deterministic, bounded, and free of NaN/Inf.
pub const FLOAT_BPS: u32 = 10_000;
/// A floating layer smaller than this is impossible to grab or read.
pub const MIN_FLOAT_DIM_BPS: u16 = 400;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FloatBounds {
    pub x_bps: u16,
    pub y_bps: u16,
    pub w_bps: u16,
    pub h_bps: u16,
}

impl FloatBounds {
    pub const fn new(x_bps: u16, y_bps: u16, w_bps: u16, h_bps: u16) -> Self {
        Self {
            x_bps,
            y_bps,
            w_bps,
            h_bps,
        }
    }

    pub fn validate(self) -> Result<(), LayoutError> {
        if self.w_bps < MIN_FLOAT_DIM_BPS
            || self.h_bps < MIN_FLOAT_DIM_BPS
            || u32::from(self.x_bps) + u32::from(self.w_bps) > FLOAT_BPS
            || u32::from(self.y_bps) + u32::from(self.h_bps) > FLOAT_BPS
        {
            return Err(LayoutError::InvalidBounds);
        }
        Ok(())
    }

    /// Convert fixed-point bounds into deterministic cell geometry. The right
    /// and bottom edges are clamped to the viewport so fractional bounds can
    /// never address cells outside the authoritative session size.
    pub fn to_rect(self, size: Size) -> Rect {
        let cols = size.cols.max(1);
        let rows = size.rows.max(1);
        let x = (u32::from(self.x_bps) * u32::from(cols) / FLOAT_BPS) as u16;
        let y = (u32::from(self.y_bps) * u32::from(rows) / FLOAT_BPS) as u16;
        let x = x.min(cols - 1);
        let y = y.min(rows - 1);
        let w = (u32::from(self.w_bps) * u32::from(cols)).div_ceil(FLOAT_BPS) as u16;
        let h = (u32::from(self.h_bps) * u32::from(rows)).div_ceil(FLOAT_BPS) as u16;
        Rect::new(x, y, w.max(1).min(cols - x), h.max(1).min(rows - y))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FloatingPane {
    pub layer_id: u64,
    pub pane_id: u64,
    pub bounds: FloatBounds,
    pub z: u32,
    pub minimized: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub id: u64,
    pub name: String,
    pub root: LayoutNode,
    pub zoom: Option<u64>,
    pub floating: BTreeMap<u64, FloatingPane>,
}

impl Window {
    pub fn new(id: u64, name: impl Into<String>, root: LayoutNode) -> Self {
        Self {
            id,
            name: name.into(),
            root,
            zoom: None,
            floating: BTreeMap::new(),
        }
    }

    /// The tree shown when zoom is active. `zoomed_pane` is reported
    /// separately so a zoomed one-pane window is not confused with a plain
    /// one-pane window.
    pub fn effective_root(&self) -> LayoutNode {
        match self.zoom {
            Some(pane_id) if layout_contains(&self.root, PaneId::new(pane_id)) => {
                LayoutNode::leaf(PaneId::new(pane_id))
            }
            _ => self.root.clone(),
        }
    }

    pub fn contains_pane(&self, pane_id: u64) -> bool {
        layout_contains(&self.root, PaneId::new(pane_id))
            || self.floating.values().any(|layer| layer.pane_id == pane_id)
    }

    pub fn max_z(&self) -> u32 {
        self.floating
            .values()
            .map(|layer| layer.z)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub windows: BTreeMap<u64, Window>,
    /// Tab order, independent of window IDs so tabs can be reordered freely.
    #[serde(default)]
    pub order: Vec<u64>,
    pub active_window: u64,
}

impl Workspace {
    pub fn new(root: LayoutNode) -> Self {
        let window = Window::new(1, "window 1", root);
        Self {
            windows: BTreeMap::from([(1, window)]),
            order: vec![1],
            active_window: 1,
        }
    }

    pub fn active(&self) -> &Window {
        self.windows
            .get(&self.active_window)
            .expect("active window exists")
    }

    pub fn active_mut(&mut self) -> &mut Window {
        self.windows
            .get_mut(&self.active_window)
            .expect("active window exists")
    }

    pub fn window_for_pane(&self, pane_id: u64) -> Option<u64> {
        self.windows
            .values()
            .find(|window| window.contains_pane(pane_id))
            .map(|window| window.id)
    }

    pub fn move_window(&mut self, window_id: u64, index: usize) -> Result<(), LayoutError> {
        let Some(position) = self.order.iter().position(|id| *id == window_id) else {
            return Err(LayoutError::WindowNotFound(window_id));
        };
        let id = self.order.remove(position);
        let target = index.min(self.order.len());
        self.order.insert(target, id);
        Ok(())
    }

    pub fn focus_step(&mut self, delta: i32) -> Result<(), LayoutError> {
        if self.order.is_empty() {
            return Err(LayoutError::TooManyWindows);
        }
        let current = self
            .order
            .iter()
            .position(|id| *id == self.active_window)
            .unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(self.order.len() as i32) as usize;
        self.active_window = self.order[next];
        Ok(())
    }

    /// Validate every window atomically before publishing a layout revision.
    /// A rejected change must leave the previous workspace untouched.
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.windows.is_empty() || self.windows.len() > MAX_WINDOWS {
            return Err(LayoutError::TooManyWindows);
        }
        if !self.windows.contains_key(&self.active_window) {
            return Err(LayoutError::WindowNotFound(self.active_window));
        }
        if self.order.len() != self.windows.len()
            || self
                .order
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.order.len()
            || self.order.iter().any(|id| !self.windows.contains_key(id))
        {
            return Err(LayoutError::TooManyWindows);
        }
        let mut seen = std::collections::BTreeSet::new();
        for window in self.windows.values() {
            validate_layout(&window.root)?;
            if window.floating.len() > MAX_LAYERS_PER_WINDOW {
                return Err(LayoutError::TooManyLayers);
            }
            for pane_id in layout_leaves(&window.root) {
                if !seen.insert(pane_id.get()) {
                    return Err(LayoutError::DuplicatePane(pane_id.get()));
                }
            }
            for layer in window.floating.values() {
                layer.bounds.validate()?;
                if !seen.insert(layer.pane_id) {
                    return Err(LayoutError::DuplicatePane(layer.pane_id));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: u16, y: u16, w: u16, h: u16) -> FloatBounds {
        let bounds = FloatBounds::new(x, y, w, h);
        bounds.validate().expect("valid bounds");
        bounds
    }

    #[test]
    fn bounds_are_normalised_and_cell_aligned() {
        let rect = bounds(2500, 5000, 5000, 2500).to_rect(Size::new(80, 24));
        assert_eq!(rect, Rect::new(20, 12, 40, 6));
    }

    #[test]
    fn invalid_bounds_are_rejected() {
        assert!(FloatBounds::new(1000, 0, 9500, 5000).validate().is_err());
        assert!(FloatBounds::new(0, 0, 100, 5000).validate().is_err());
        assert!(FloatBounds::new(7500, 0, 5000, 5000).validate().is_err());
    }

    #[test]
    fn workspace_validation_rejects_duplicate_panes_across_windows() {
        let root = LayoutNode::leaf(PaneId::new(7));
        let mut workspace = Workspace::new(root.clone());
        workspace.windows.insert(2, Window::new(2, "two", root));
        workspace.order.push(2);
        assert!(matches!(
            workspace.validate(),
            Err(LayoutError::DuplicatePane(7))
        ));
    }
}
