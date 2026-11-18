use crate::geometry::{Rect, Size};
use crate::ids::PaneId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Columns,
    Rows,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutNode {
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

impl LayoutNode {
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
    ImpossibleSplit {
        axis: SplitAxis,
        available: u16,
        minimum: u16,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRatio(value) => write!(f, "invalid split ratio {value} bps"),
            Self::DuplicatePane(id) => write!(f, "pane {id} appears more than once"),
            Self::ImpossibleSplit {
                axis,
                available,
                minimum,
            } => write!(
                f,
                "{axis:?} split needs {minimum} cells per pane, only {available} available"
            ),
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
