use crate::geometry::{Rect, Size};
use crate::ids::PaneId;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Columns,
    Rows,
}

/// Where a pane currently lives. `Tiled` panes are leaves in a window's
/// split tree; `Floating` and `Detached` are reserved for later milestones.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanePlacement {
    Tiled,
    Floating { layer_id: u64 },
    Detached,
}

/// Maximum accepted split nesting. A tiled layout never needs more than a few
/// dozen levels; the cap exists because decoding is recursive and a hostile
/// `SetLayout` frame could otherwise overflow a 2 MiB runtime thread stack.
pub const MAX_LAYOUT_DEPTH: u32 = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutNode {
    /// An empty window. Panes may be detached while their child keeps running.
    Empty,
    Leaf {
        pane_id: u64,
    },
    Split {
        axis: SplitAxis,
        ratio_bps: u16,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

thread_local! {
    static LAYOUT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct DepthGuard;

impl DepthGuard {
    fn enter<E: serde::de::Error>() -> Result<Self, E> {
        LAYOUT_DEPTH.with(|cell| {
            let depth = cell.get();
            if depth >= MAX_LAYOUT_DEPTH {
                return Err(E::custom("layout tree is nested too deeply"));
            }
            cell.set(depth + 1);
            Ok(Self)
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        LAYOUT_DEPTH.with(|cell| cell.set(cell.get().saturating_sub(1)));
    }
}

impl<'de> Deserialize<'de> for LayoutNode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // The nested `Box<LayoutNode>` fields re-enter this impl, so the guard
        // counts the whole tree and fails before the recursion gets deep.
        let _guard = DepthGuard::enter::<D::Error>()?;

        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Repr {
            Empty,
            Leaf {
                pane_id: u64,
            },
            Split {
                axis: SplitAxis,
                ratio_bps: u16,
                first: Box<LayoutNode>,
                second: Box<LayoutNode>,
            },
        }

        Ok(match Repr::deserialize(deserializer)? {
            Repr::Empty => Self::Empty,
            Repr::Leaf { pane_id } => Self::Leaf { pane_id },
            Repr::Split {
                axis,
                ratio_bps,
                first,
                second,
            } => Self::Split {
                axis,
                ratio_bps,
                first,
                second,
            },
        })
    }
}

impl LayoutNode {
    pub fn empty() -> Self {
        Self::Empty
    }

    pub fn leaf(pane_id: PaneId) -> Self {
        Self::Leaf {
            pane_id: pane_id.get(),
        }
    }

    pub fn split(axis: SplitAxis, ratio_bps: u16, first: Self, second: Self) -> Self {
        Self::Split {
            axis,
            ratio_bps,
            first: Box::new(first),
            second: Box::new(second),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedPane {
    pub pane_id: u64,
    pub outer: Rect,
    pub content: Rect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    InvalidRatio(u16),
    DuplicatePane(u64),
    PaneNotFound(u64),
    ImpossibleSplit {
        axis: SplitAxis,
        available: u16,
        minimum: u16,
    },
    InvalidBounds,
    TooManyWindows,
    TooManyLayers,
    WindowNotFound(u64),
    LayerNotFound(u64),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRatio(value) => write!(f, "invalid split ratio {value} bps"),
            Self::DuplicatePane(id) => write!(f, "pane {id} appears more than once"),
            Self::PaneNotFound(id) => write!(f, "pane {id} is not in the layout"),
            Self::ImpossibleSplit {
                axis,
                available,
                minimum,
            } => write!(
                f,
                "{axis:?} split needs {minimum} cells per pane, only {available} available"
            ),
            Self::InvalidBounds => write!(f, "floating bounds must stay inside the viewport"),
            Self::TooManyWindows => write!(f, "workspace window limit exceeded"),
            Self::TooManyLayers => write!(f, "window floating-layer limit exceeded"),
            Self::WindowNotFound(id) => write!(f, "window {id} not found"),
            Self::LayerNotFound(id) => write!(f, "floating layer {id} not found"),
        }
    }
}

impl std::error::Error for LayoutError {}

/// Validate a layout tree before it is applied by the daemon.
pub fn validate(root: &LayoutNode) -> Result<(), LayoutError> {
    let mut seen = BTreeSet::new();
    validate_inner(root, &mut seen)
}

fn validate_inner(root: &LayoutNode, seen: &mut BTreeSet<u64>) -> Result<(), LayoutError> {
    match root {
        LayoutNode::Empty => Ok(()),
        LayoutNode::Leaf { pane_id } => {
            if !seen.insert(*pane_id) {
                return Err(LayoutError::DuplicatePane(*pane_id));
            }
            Ok(())
        }
        LayoutNode::Split {
            ratio_bps,
            first,
            second,
            ..
        } => {
            if *ratio_bps == 0 || *ratio_bps > 9999 {
                return Err(LayoutError::InvalidRatio(*ratio_bps));
            }
            validate_inner(first, seen)?;
            validate_inner(second, seen)
        }
    }
}

/// Resolve a split tree into outer/content rectangles relative to the area.
///
/// The border parameter reserves chrome for a GUI or the ANSI compositor.
/// A single leaf may be smaller than the minimum; a split that cannot satisfy
/// the minimum for both children is rejected.
pub fn resolve(
    root: &LayoutNode,
    area: Rect,
    minimum: Size,
    border: u16,
) -> Result<Vec<ResolvedPane>, LayoutError> {
    validate(root)?;
    let mut out = Vec::new();
    resolve_inner(root, area, minimum, border, &mut out)?;
    Ok(out)
}

fn resolve_inner(
    root: &LayoutNode,
    area: Rect,
    minimum: Size,
    border: u16,
    out: &mut Vec<ResolvedPane>,
) -> Result<(), LayoutError> {
    match root {
        LayoutNode::Empty => Ok(()),
        LayoutNode::Leaf { pane_id } => {
            out.push(ResolvedPane {
                pane_id: *pane_id,
                outer: area,
                content: area.content(border),
            });
            Ok(())
        }
        LayoutNode::Split {
            axis,
            ratio_bps,
            first,
            second,
        } => {
            let (available, min) = match axis {
                SplitAxis::Columns => (area.cols, minimum.cols),
                SplitAxis::Rows => (area.rows, minimum.rows),
            };
            if available < min.saturating_mul(2) {
                return Err(LayoutError::ImpossibleSplit {
                    axis: *axis,
                    available,
                    minimum: min,
                });
            }
            let total = u32::from(available);
            let mut first_size = ((total * u32::from(*ratio_bps)) / 10_000) as u16;
            let max_first = available - min;
            first_size = first_size.clamp(min, max_first);
            let second_size = available - first_size;

            let (first_area, second_area) = match axis {
                SplitAxis::Columns => (
                    Rect::new(area.x, area.y, first_size, area.rows),
                    Rect::new(area.x + first_size, area.y, second_size, area.rows),
                ),
                SplitAxis::Rows => (
                    Rect::new(area.x, area.y, area.cols, first_size),
                    Rect::new(area.x, area.y + first_size, area.cols, second_size),
                ),
            };
            resolve_inner(first, first_area, minimum, border, out)?;
            resolve_inner(second, second_area, minimum, border, out)
        }
    }
}

/// All pane IDs in left-to-right tree order.
pub fn leaves(root: &LayoutNode) -> Vec<PaneId> {
    fn collect(root: &LayoutNode, out: &mut Vec<PaneId>) {
        match root {
            LayoutNode::Empty => {}
            LayoutNode::Leaf { pane_id } => out.push(PaneId::new(*pane_id)),
            LayoutNode::Split { first, second, .. } => {
                collect(first, out);
                collect(second, out);
            }
        }
    }
    let mut out = Vec::new();
    collect(root, &mut out);
    out
}

/// Whether a pane participates in this layout.
pub fn contains(root: &LayoutNode, pane_id: PaneId) -> bool {
    leaves(root).contains(&pane_id)
}

/// Deterministic first leaf, used after a focused pane closes.
pub fn first_leaf(root: &LayoutNode) -> Option<PaneId> {
    leaves(root).into_iter().next()
}

/// Replace one leaf with a split; `new_pane_id` must not already be present.
pub fn split_leaf(
    root: &LayoutNode,
    pane_id: PaneId,
    new_pane_id: PaneId,
    axis: SplitAxis,
    ratio_bps: u16,
) -> Result<LayoutNode, LayoutError> {
    if !(1..=9999).contains(&ratio_bps) {
        return Err(LayoutError::InvalidRatio(ratio_bps));
    }
    if contains(root, new_pane_id) {
        return Err(LayoutError::DuplicatePane(new_pane_id.get()));
    }
    fn rebuild(
        root: &LayoutNode,
        pane_id: PaneId,
        new_pane_id: PaneId,
        axis: SplitAxis,
        ratio_bps: u16,
        changed: &mut bool,
    ) -> Result<LayoutNode, LayoutError> {
        match root {
            LayoutNode::Empty => Ok(LayoutNode::empty()),
            LayoutNode::Leaf { pane_id: id } if *id == pane_id.get() => {
                *changed = true;
                Ok(LayoutNode::split(
                    axis,
                    ratio_bps,
                    LayoutNode::leaf(pane_id),
                    LayoutNode::leaf(new_pane_id),
                ))
            }
            LayoutNode::Leaf { pane_id } => Ok(LayoutNode::leaf(PaneId::new(*pane_id))),
            LayoutNode::Split {
                axis: parent_axis,
                ratio_bps: parent_ratio,
                first,
                second,
            } => Ok(LayoutNode::split(
                *parent_axis,
                *parent_ratio,
                rebuild(first, pane_id, new_pane_id, axis, ratio_bps, changed)?,
                rebuild(second, pane_id, new_pane_id, axis, ratio_bps, changed)?,
            )),
        }
    }
    let mut changed = false;
    let next = rebuild(root, pane_id, new_pane_id, axis, ratio_bps, &mut changed)?;
    if !changed {
        return Err(LayoutError::PaneNotFound(pane_id.get()));
    }
    validate(&next)?;
    Ok(next)
}

/// Remove a leaf, collapsing its sibling upward. `None` means the last pane
/// closed.
pub fn close_leaf(root: &LayoutNode, pane_id: PaneId) -> Result<Option<LayoutNode>, LayoutError> {
    fn rebuild(
        root: &LayoutNode,
        pane_id: PaneId,
        changed: &mut bool,
    ) -> Result<Option<LayoutNode>, LayoutError> {
        match root {
            LayoutNode::Empty => Ok(Some(LayoutNode::empty())),
            LayoutNode::Leaf { pane_id: id } if *id == pane_id.get() => {
                *changed = true;
                Ok(None)
            }
            LayoutNode::Leaf { pane_id } => Ok(Some(LayoutNode::leaf(PaneId::new(*pane_id)))),
            LayoutNode::Split {
                axis,
                ratio_bps,
                first,
                second,
            } => {
                let first = rebuild(first, pane_id, changed)?;
                let second = rebuild(second, pane_id, changed)?;
                Ok(match (first, second) {
                    (Some(first), Some(second)) => {
                        Some(LayoutNode::split(*axis, *ratio_bps, first, second))
                    }
                    (Some(node), None) | (None, Some(node)) => Some(node),
                    (None, None) => None,
                })
            }
        }
    }
    let mut changed = false;
    let next = rebuild(root, pane_id, &mut changed)?;
    if !changed {
        return Err(LayoutError::PaneNotFound(pane_id.get()));
    }
    if let Some(next) = &next {
        validate(next)?;
    }
    Ok(next)
}

/// Remove a pane from the tiled tree without killing it. Unlike `close_leaf`,
/// removing the last pane yields an explicit empty window.
pub fn remove_leaf(root: &LayoutNode, pane_id: PaneId) -> Result<LayoutNode, LayoutError> {
    fn rebuild(
        root: &LayoutNode,
        pane_id: PaneId,
        changed: &mut bool,
    ) -> Result<LayoutNode, LayoutError> {
        Ok(match root {
            LayoutNode::Empty => LayoutNode::empty(),
            LayoutNode::Leaf { pane_id: id } if *id == pane_id.get() => {
                *changed = true;
                LayoutNode::empty()
            }
            LayoutNode::Leaf { pane_id } => LayoutNode::leaf(PaneId::new(*pane_id)),
            LayoutNode::Split {
                axis,
                ratio_bps,
                first,
                second,
            } => {
                let first = rebuild(first, pane_id, changed)?;
                let second = rebuild(second, pane_id, changed)?;
                match (first, second) {
                    (node, LayoutNode::Empty) | (LayoutNode::Empty, node) => node,
                    (first, second) => LayoutNode::split(*axis, *ratio_bps, first, second),
                }
            }
        })
    }
    let mut changed = false;
    let next = rebuild(root, pane_id, &mut changed)?;
    if !changed {
        return Err(LayoutError::PaneNotFound(pane_id.get()));
    }
    validate(&next)?;
    Ok(next)
}

/// Place a previously detached pane into the tiled tree. If `anchor` is
/// `None`, the new pane is split against the whole existing window.
pub fn place_leaf(
    root: &LayoutNode,
    anchor: Option<PaneId>,
    pane_id: PaneId,
    axis: SplitAxis,
    ratio_bps: u16,
) -> Result<LayoutNode, LayoutError> {
    if contains(root, pane_id) {
        return Err(LayoutError::DuplicatePane(pane_id.get()));
    }
    let next = match root {
        LayoutNode::Empty => LayoutNode::leaf(pane_id),
        _ => match anchor {
            Some(anchor) if contains(root, anchor) => {
                split_leaf(root, anchor, pane_id, axis, ratio_bps)?
            }
            Some(anchor) => return Err(LayoutError::PaneNotFound(anchor.get())),
            None => LayoutNode::split(axis, ratio_bps, root.clone(), LayoutNode::leaf(pane_id)),
        },
    };
    validate(&next)?;
    Ok(next)
}

/// Pick the pane whose outer rectangle contains a cell.
pub fn hit_test(panes: &[ResolvedPane], x: u16, y: u16) -> Option<PaneId> {
    panes
        .iter()
        .rev()
        .find(|pane| pane.outer.contains(x, y))
        .map(|pane| PaneId::new(pane.pane_id))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Pick the nearest pane in a direction, using outer-rect centers.
pub fn focus_direction(
    panes: &[ResolvedPane],
    from: PaneId,
    direction: Direction,
) -> Option<PaneId> {
    let current = panes.iter().find(|pane| pane.pane_id == from.get())?;
    let center = |pane: &ResolvedPane| {
        (
            f64::from(pane.outer.x) + f64::from(pane.outer.cols) / 2.0,
            f64::from(pane.outer.y) + f64::from(pane.outer.rows) / 2.0,
        )
    };
    let (cx, cy) = center(current);
    let mut best: Option<(f64, PaneId)> = None;
    for pane in panes.iter().filter(|pane| pane.pane_id != from.get()) {
        let (x, y) = center(pane);
        let forward = match direction {
            Direction::Left => x < cx,
            Direction::Right => x > cx,
            Direction::Up => y < cy,
            Direction::Down => y > cy,
        };
        if !forward {
            continue;
        }
        let dx = x - cx;
        let dy = y - cy;
        let distance = match direction {
            Direction::Left | Direction::Right => dx.abs() + dy.abs() * 2.0,
            Direction::Up | Direction::Down => dy.abs() + dx.abs() * 2.0,
        };
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, PaneId::new(pane.pane_id)));
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> LayoutNode {
        LayoutNode::split(
            SplitAxis::Columns,
            5000,
            LayoutNode::split(
                SplitAxis::Rows,
                5000,
                LayoutNode::leaf(PaneId::new(1)),
                LayoutNode::leaf(PaneId::new(2)),
            ),
            LayoutNode::leaf(PaneId::new(3)),
        )
    }

    #[test]
    fn resolves_nested_splits_without_gaps() {
        let panes = resolve(&tree(), Rect::new(0, 0, 101, 50), Size::new(5, 3), 1).unwrap();
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].outer, Rect::new(0, 0, 50, 25));
        assert_eq!(panes[1].outer, Rect::new(0, 25, 50, 25));
        assert_eq!(panes[2].outer, Rect::new(50, 0, 51, 50));
        assert_eq!(panes[0].content, Rect::new(1, 1, 48, 23));
        assert_eq!(hit_test(&panes, 0, 49), Some(PaneId::new(2)));
        assert_eq!(hit_test(&panes, 100, 0), Some(PaneId::new(3)));
    }

    #[test]
    fn rejects_bad_ratios_and_duplicates() {
        let bad = LayoutNode::split(
            SplitAxis::Rows,
            0,
            LayoutNode::leaf(PaneId::new(1)),
            LayoutNode::leaf(PaneId::new(2)),
        );
        assert_eq!(validate(&bad), Err(LayoutError::InvalidRatio(0)));
        let dup = LayoutNode::split(
            SplitAxis::Rows,
            5000,
            LayoutNode::leaf(PaneId::new(1)),
            LayoutNode::leaf(PaneId::new(1)),
        );
        assert_eq!(validate(&dup), Err(LayoutError::DuplicatePane(1)));
    }

    #[test]
    fn rejects_impossible_splits_and_accepts_tiny_leaves() {
        let split = LayoutNode::split(
            SplitAxis::Columns,
            5000,
            LayoutNode::leaf(PaneId::new(1)),
            LayoutNode::leaf(PaneId::new(2)),
        );
        assert!(resolve(&split, Rect::new(0, 0, 9, 10), Size::new(5, 2), 0).is_err());
        let leaf = LayoutNode::leaf(PaneId::new(1));
        let panes = resolve(&leaf, Rect::new(0, 0, 1, 1), Size::new(20, 5), 1).unwrap();
        assert_eq!(panes[0].content.cols, 0);
    }

    #[test]
    fn edits_nested_trees_without_leaving_holes() {
        let mut root = tree();
        root = split_leaf(
            &root,
            PaneId::new(3),
            PaneId::new(4),
            SplitAxis::Columns,
            2500,
        )
        .unwrap();
        assert_eq!(leaves(&root).len(), 4);
        root = close_leaf(&root, PaneId::new(1)).unwrap().unwrap();
        assert_eq!(
            leaves(&root),
            vec![PaneId::new(2), PaneId::new(3), PaneId::new(4)]
        );
        assert_eq!(first_leaf(&root), Some(PaneId::new(2)));
        assert!(!contains(&root, PaneId::new(1)));
        assert_eq!(
            close_leaf(&root, PaneId::new(99)),
            Err(LayoutError::PaneNotFound(99))
        );
        let single = LayoutNode::leaf(PaneId::new(1));
        assert_eq!(close_leaf(&single, PaneId::new(1)).unwrap(), None);
    }

    #[test]
    fn layouts_deeper_than_the_limit_are_rejected() {
        let mut node = LayoutNode::leaf(PaneId::new(1));
        for depth in 0..(MAX_LAYOUT_DEPTH as u64 + 8) {
            node = LayoutNode::split(
                SplitAxis::Rows,
                5000,
                node,
                LayoutNode::leaf(PaneId::new(depth + 2)),
            );
        }
        let encoded = rmp_serde::to_vec(&node).expect("encode");
        let decoded = rmp_serde::from_slice::<LayoutNode>(&encoded);
        let err = decoded.expect_err("a deep tree must not decode");
        assert!(
            err.to_string().contains("nested too deeply"),
            "unexpected error: {err}"
        );

        let mut shallow = LayoutNode::leaf(PaneId::new(1));
        for depth in 0..8_u64 {
            shallow = LayoutNode::split(
                SplitAxis::Columns,
                5000,
                shallow,
                LayoutNode::leaf(PaneId::new(depth + 2)),
            );
        }
        let encoded = rmp_serde::to_vec(&shallow).expect("encode");
        let round_trip: LayoutNode = rmp_serde::from_slice(&encoded).expect("decode");
        assert_eq!(round_trip, shallow);
    }

    #[test]
    fn nested_splits_use_the_requested_axis() {
        // Pane 1 lives inside a Rows split. Splitting it along Columns must
        // add a Columns node, not inherit the parent's axis.
        let root = split_leaf(
            &tree(),
            PaneId::new(1),
            PaneId::new(4),
            SplitAxis::Columns,
            4000,
        )
        .unwrap();
        let LayoutNode::Split { first, .. } = root else {
            panic!("root split");
        };
        let LayoutNode::Split {
            axis, first: inner, ..
        } = first.as_ref()
        else {
            panic!("rows split on the left");
        };
        assert_eq!(*axis, SplitAxis::Rows, "the enclosing axis must not change");
        let LayoutNode::Split {
            axis: inner_axis,
            ratio_bps: inner_ratio,
            ..
        } = inner.as_ref()
        else {
            panic!("new columns split");
        };
        assert_eq!(*inner_axis, SplitAxis::Columns);
        assert_eq!(*inner_ratio, 4000);

        // Pane 3 is a top-level leaf under a Columns split; a Rows split must
        // likewise keep its own axis.
        let root = split_leaf(
            &tree(),
            PaneId::new(3),
            PaneId::new(5),
            SplitAxis::Rows,
            6000,
        )
        .unwrap();
        let LayoutNode::Split { second, .. } = root else {
            panic!("root split");
        };
        let LayoutNode::Split {
            axis, ratio_bps, ..
        } = second.as_ref()
        else {
            panic!("new rows split");
        };
        assert_eq!(*axis, SplitAxis::Rows);
        assert_eq!(*ratio_bps, 6000);
    }

    #[test]
    fn rejects_impossible_tree_edits() {
        let root = LayoutNode::leaf(PaneId::new(1));
        assert_eq!(
            split_leaf(&root, PaneId::new(1), PaneId::new(2), SplitAxis::Rows, 0,),
            Err(LayoutError::InvalidRatio(0))
        );
        assert_eq!(
            split_leaf(&root, PaneId::new(1), PaneId::new(1), SplitAxis::Rows, 5000,),
            Err(LayoutError::DuplicatePane(1))
        );
        assert_eq!(
            split_leaf(&root, PaneId::new(7), PaneId::new(2), SplitAxis::Rows, 5000,),
            Err(LayoutError::PaneNotFound(7))
        );
    }

    #[test]
    fn focus_moves_between_neighbors() {
        let panes = resolve(&tree(), Rect::new(0, 0, 100, 50), Size::new(5, 3), 1).unwrap();
        assert_eq!(
            focus_direction(&panes, PaneId::new(1), Direction::Right),
            Some(PaneId::new(3))
        );
        assert_eq!(
            focus_direction(&panes, PaneId::new(2), Direction::Up),
            Some(PaneId::new(1))
        );
        assert_eq!(
            focus_direction(&panes, PaneId::new(3), Direction::Right),
            None
        );
    }
}
