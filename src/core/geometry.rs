use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    pub const fn min(self, other: Self) -> Self {
        Self {
            cols: if self.cols < other.cols {
                self.cols
            } else {
                other.cols
            },
            rows: if self.rows < other.rows {
                self.rows
            } else {
                other.rows
            },
        }
    }

    pub const fn clamp_min(self, minimum: Self) -> Self {
        Self {
            cols: if self.cols < minimum.cols {
                minimum.cols
            } else {
                self.cols
            },
            rows: if self.rows < minimum.rows {
                minimum.rows
            } else {
                self.rows
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, cols: u16, rows: u16) -> Self {
        Self { x, y, cols, rows }
    }

    pub const fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x
            && y >= self.y
            && x.saturating_sub(self.x) < self.cols
            && y.saturating_sub(self.y) < self.rows
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self
            .x
            .saturating_add(self.cols)
            .min(other.x.saturating_add(other.cols));
        let bottom = self
            .y
            .saturating_add(self.rows)
            .min(other.y.saturating_add(other.rows));
        if right <= x || bottom <= y {
            None
        } else {
            Some(Self::new(x, y, right - x, bottom - y))
        }
    }

    /// Shrink by a border on all sides; a too-small rect becomes zero-sized.
    pub fn content(self, border: u16) -> Self {
        let twice = border.saturating_mul(2);
        if self.cols <= twice || self.rows <= twice {
            return Self::new(
                self.x.saturating_add(border.min(self.cols)),
                self.y.saturating_add(border.min(self.rows)),
                0,
                0,
            );
        }
        Self::new(
            self.x + border,
            self.y + border,
            self.cols - twice,
            self.rows - twice,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_contains_and_intersects() {
        let a = Rect::new(2, 3, 10, 8);
        assert!(a.contains(2, 3));
        assert!(a.contains(11, 10));
        assert!(!a.contains(12, 10));
        assert_eq!(
            a.intersection(Rect::new(8, 0, 10, 5)),
            Some(Rect::new(8, 3, 4, 2))
        );
        assert_eq!(a.intersection(Rect::new(20, 0, 2, 2)), None);
    }

    #[test]
    fn content_removes_border() {
        assert_eq!(Rect::new(0, 0, 20, 10).content(1), Rect::new(1, 1, 18, 8));
        assert_eq!(Rect::new(0, 0, 1, 1).content(1).cols, 0);
    }
}
