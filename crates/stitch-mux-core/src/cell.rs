use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Attrs(pub u32);

impl Attrs {
    pub const BOLD: Self = Self(1 << 0);
    pub const DIM: Self = Self(1 << 1);
    pub const ITALIC: Self = Self(1 << 2);
    pub const UNDERLINE: Self = Self(1 << 3);
    pub const BLINK: Self = Self(1 << 4);
    pub const REVERSE: Self = Self(1 << 5);
    pub const HIDDEN: Self = Self(1 << 6);
    pub const STRIKETHROUGH: Self = Self(1 << 7);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub width: u8,
    pub link: Option<String>,
}

impl Cell {
    pub fn blank() -> Self {
        Self {
            text: String::new(),
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
            width: 1,
            link: None,
        }
    }

    pub fn is_continuation(&self) -> bool {
        self.width == 0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    pub shape: CursorShape,
    pub blink: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Modes {
    pub alt_screen: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_proto: u16,
    pub sgr_mouse: bool,
    pub cursor_visible: bool,
    pub keyboard_flags: u16,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attrs_compose() {
        let mut a = Attrs::BOLD;
        a.insert(Attrs::UNDERLINE);
        assert!(a.contains(Attrs::BOLD));
        assert!(a.contains(Attrs::UNDERLINE));
        assert!(!a.contains(Attrs::ITALIC));
        a.remove(Attrs::BOLD);
        assert!(!a.contains(Attrs::BOLD));
    }

    #[test]
    fn continuation_cells_are_marked() {
        assert!(!Cell::blank().is_continuation());
        let mut continuation = Cell::blank();
        continuation.width = 0;
        assert!(continuation.is_continuation());
    }
}
