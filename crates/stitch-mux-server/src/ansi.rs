//! Server-side ANSI compositor for the terminal compatibility projection.
//!
//! The daemon owns the authoritative cell grids. This module turns a layout
//! plus the current pane snapshots into a bounded ANSI frame: a full repaint
//! on attach/resize/layout change and a small cursor-addressed diff otherwise.
//! Terminal clients never parse ANSI or own emulator state.

use std::collections::BTreeMap;

use stitch_mux_core::{Attrs, Cell, Color, Rect};
use stitch_mux_protocol::{Attached, Layout, PaneSnapshot, Render};
use unicode_width::UnicodeWidthChar;

/// Bound both axes: a terminal projection must not turn a huge client viewport
/// into an unbounded allocation. Larger viewports are clipped; the session's
/// canonical layout stays authoritative for every frontend.
const MAX_COLS: u16 = 300;
const MAX_ROWS: u16 = 100;

#[derive(Clone, Debug, Eq, PartialEq)]
struct GridCell {
    text: String,
    fg: Color,
    bg: Color,
    attrs: Attrs,
    /// Second half of a wide glyph. The glyph is written at the preceding
    /// cell; writing here would overwrite part of it.
    continuation: bool,
}

impl GridCell {
    fn blank() -> Self {
        Self {
            text: String::new(),
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
            continuation: false,
        }
    }

    fn from_cell(cell: &Cell) -> Self {
        Self {
            text: cell.text.clone(),
            fg: cell.fg,
            bg: cell.bg,
            attrs: cell.attrs,
            continuation: cell.is_continuation(),
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.text.is_empty() || self.text == " " {
            self.text = text.to_owned();
        } else {
            self.text.push_str(text);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GridCursor {
    x: u16,
    y: u16,
    visible: bool,
    shape: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Grid {
    cols: u16,
    rows: u16,
    cells: Vec<GridCell>,
    cursor: Option<GridCursor>,
}

impl Grid {
    fn new(cols: u16, rows: u16) -> Self {
        let cells = vec![GridCell::blank(); usize::from(cols) * usize::from(rows)];
        Self {
            cols,
            rows,
            cells,
            cursor: None,
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        if x < self.cols && y < self.rows {
            Some(usize::from(y) * usize::from(self.cols) + usize::from(x))
        } else {
            None
        }
    }

    fn set(&mut self, x: u16, y: u16, cell: GridCell) {
        if let Some(index) = self.index(x, y) {
            self.cells[index] = cell;
        }
    }

    fn text(&mut self, x: u16, y: u16, text: &str, base: &GridCell) {
        let mut x = x;
        for ch in text.chars() {
            let mut cell = base.clone();
            cell.push_text(&ch.to_string());
            self.set(x, y, cell);
            x = x.saturating_add(1);
            if x >= self.cols {
                break;
            }
        }
    }
}

/// Incremental compositor state for one terminal client.
#[derive(Default)]
pub struct AnsiCompositor {
    grid: Option<Grid>,
    seq: u64,
    signature: (u16, u16, u64),
}

impl AnsiCompositor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discard the previous frame so the next one is a full repaint.
    pub fn invalidate(&mut self) {
        self.grid = None;
    }

    /// Compose a full frame. Used on attach, layout/resize changes, and after
    /// `Resync`; the ANSI stream always starts from a known empty screen.
    pub fn full(
        &mut self,
        attached: &Attached,
        layout: &Layout,
        snapshots: &BTreeMap<u64, PaneSnapshot>,
    ) -> Render {
        let grid = compose(attached, layout, snapshots);
        let (cols, rows) = (grid.cols, grid.rows);
        let ansi = encode_full(&grid, &sanitize_title(&attached.session_name));
        self.signature = (cols, rows, layout.revision);
        let render = self.next_render(cols, rows, true, ansi);
        self.grid = Some(grid);
        render
    }

    /// Compose an incremental frame. Geometry changes force a full repaint;
    /// otherwise only changed cells and the focused cursor are emitted.
    pub fn update(
        &mut self,
        attached: &Attached,
        layout: &Layout,
        snapshots: &BTreeMap<u64, PaneSnapshot>,
    ) -> Option<Render> {
        let grid = compose(attached, layout, snapshots);
        let (cols, rows) = (grid.cols, grid.rows);
        let signature = (cols, rows, layout.revision);
        let needs_full = signature != self.signature
            || self
                .grid
                .as_ref()
                .is_none_or(|old| old.cols != cols || old.rows != rows);
        let ansi = if needs_full {
            encode_full(&grid, &sanitize_title(&attached.session_name))
        } else {
            let old = self.grid.as_ref().expect("grid presence checked above");
            encode_diff(old, &grid)
        };
        if ansi.is_empty() {
            self.grid = Some(grid);
            self.signature = signature;
            return None;
        }
        let render = self.next_render(cols, rows, needs_full, ansi);
        self.grid = Some(grid);
        self.signature = signature;
        Some(render)
    }

    fn next_render(&mut self, cols: u16, rows: u16, full: bool, ansi: Vec<u8>) -> Render {
        self.seq = self.seq.saturating_add(1);
        Render {
            seq: self.seq,
            cols,
            rows,
            full,
            ansi,
        }
    }
}

fn compose(attached: &Attached, layout: &Layout, snapshots: &BTreeMap<u64, PaneSnapshot>) -> Grid {
    let cols = layout.cols.clamp(1, MAX_COLS);
    let rows = layout.rows.clamp(1, MAX_ROWS);
    let mut grid = Grid::new(cols, rows);
    let mut status_pane = layout.focus;
    if status_pane.is_none() {
        status_pane = layout.panes.first().map(|pane| pane.pane_id);
    }

    for geometry in &layout.panes {
        draw_border(
            &mut grid,
            geometry.outer,
            Some(geometry.pane_id) == layout.focus,
        );
        if let Some(snapshot) = snapshots.get(&geometry.pane_id) {
            draw_content(&mut grid, geometry.content, snapshot);
            if layout.focus == Some(geometry.pane_id) {
                grid.cursor = focused_cursor(geometry.content, snapshot);
            }
        }
    }

    if let Some(pane_id) = status_pane {
        let geometry = layout
            .panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .or_else(|| layout.panes.first());
        if let Some(geometry) = geometry {
            let status = format!(
                "[{}] rev={} clients={} focus={} {}: Ctrl-] d detach, Ctrl-] arrows focus",
                attached.session_name,
                layout.revision,
                layout.attached_clients,
                pane_id,
                geometry.title
            );
            grid.text(
                geometry.outer.x.saturating_add(2),
                geometry.outer.y,
                &status,
                &border_cell(true),
            );
        }
    } else {
        grid.text(1, 0, "no panes", &GridCell::blank());
    }
    grid
}

fn draw_border(grid: &mut Grid, outer: Rect, focused: bool) {
    let style = border_cell(focused);
    let right = outer.x.saturating_add(outer.cols.saturating_sub(1));
    let bottom = outer.y.saturating_add(outer.rows.saturating_sub(1));
    for x in outer.x..=right {
        put(grid, x, outer.y, "-", &style);
        put(grid, x, bottom, "-", &style);
    }
    for y in outer.y..=bottom {
        put(grid, outer.x, y, "|", &style);
        put(grid, right, y, "|", &style);
    }
    put(grid, outer.x, outer.y, "+", &style);
    put(grid, right, outer.y, "+", &style);
    put(grid, outer.x, bottom, "+", &style);
    put(grid, right, bottom, "+", &style);
}

fn border_cell(focused: bool) -> GridCell {
    let mut cell = GridCell::blank();
    if focused {
        cell.attrs.insert(Attrs::BOLD);
        cell.fg = Color::Indexed(14);
    }
    cell
}

fn put(grid: &mut Grid, x: u16, y: u16, text: &str, base: &GridCell) {
    let mut cell = base.clone();
    cell.push_text(text);
    grid.set(x, y, cell);
}

fn draw_content(grid: &mut Grid, content: Rect, snapshot: &PaneSnapshot) {
    for (row_index, row) in snapshot
        .cells
        .iter()
        .enumerate()
        .take(usize::from(content.rows))
    {
        let y = content.y.saturating_add(row_index as u16);
        for (col_index, cell) in row.cells.iter().enumerate().take(usize::from(content.cols)) {
            let x = content.x.saturating_add(col_index as u16);
            grid.set(x, y, GridCell::from_cell(cell));
        }
    }
}

fn focused_cursor(content: Rect, snapshot: &PaneSnapshot) -> Option<GridCursor> {
    if !snapshot.cursor.visible || !snapshot.modes.cursor_visible {
        return None;
    }
    if snapshot.cursor.x >= content.cols || snapshot.cursor.y >= content.rows {
        return None;
    }
    Some(GridCursor {
        x: content.x.saturating_add(snapshot.cursor.x),
        y: content.y.saturating_add(snapshot.cursor.y),
        visible: true,
        shape: match snapshot.cursor.shape {
            stitch_mux_core::CursorShape::Block => 1,
            stitch_mux_core::CursorShape::Bar => 5,
            stitch_mux_core::CursorShape::Underline => 3,
        },
    })
}

fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .filter(|ch| *ch != '\x07' && *ch != '\x1b' && !ch.is_control())
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Style {
    fg: Color,
    bg: Color,
    attrs: Attrs,
}

impl Style {
    fn of(cell: &GridCell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            attrs: cell.attrs,
        }
    }

    fn emit(self, out: &mut Vec<u8>, current: &mut Self) {
        if self == *current {
            return;
        }
        if self == Style::default() {
            out.extend_from_slice(b"\x1b[0m");
        } else {
            out.extend_from_slice(b"\x1b[0m");
            push_color(out, 38, self.fg);
            push_color(out, 48, self.bg);
            for (flag, code) in [
                (Attrs::BOLD, 1),
                (Attrs::DIM, 2),
                (Attrs::ITALIC, 3),
                (Attrs::UNDERLINE, 4),
                (Attrs::BLINK, 5),
                (Attrs::REVERSE, 7),
                (Attrs::HIDDEN, 8),
                (Attrs::STRIKETHROUGH, 9),
            ] {
                if self.attrs.contains(flag) {
                    out.extend_from_slice(format!("\x1b[{code}m").as_bytes());
                }
            }
        }
        *current = self;
    }
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
        }
    }
}

fn push_color(out: &mut Vec<u8>, base: u8, color: Color) {
    match color {
        Color::Default => {}
        Color::Indexed(value) => {
            out.extend_from_slice(format!("\x1b[{base};5;{value}m").as_bytes())
        }
        Color::Rgb(red, green, blue) => {
            out.extend_from_slice(format!("\x1b[{base};2;{red};{green};{blue}m").as_bytes())
        }
    }
}

fn cursor_position(out: &mut Vec<u8>, x: u16, y: u16) {
    out.extend_from_slice(
        format!("\x1b[{};{}H", y.saturating_add(1), x.saturating_add(1)).as_bytes(),
    );
}

fn cell_width(cell: &GridCell) -> u16 {
    if cell.continuation {
        return 1;
    }
    cell.text
        .chars()
        .next()
        .and_then(UnicodeWidthChar::width)
        .unwrap_or(1)
        .max(1) as u16
}

fn emit_cursor(out: &mut Vec<u8>, cursor: Option<GridCursor>, previous: Option<GridCursor>) {
    if cursor == previous {
        return;
    }
    out.extend_from_slice(b"\x1b[?25l");
    if let Some(cursor) = cursor {
        out.extend_from_slice(format!("\x1b[{} q", cursor.shape).as_bytes());
        cursor_position(out, cursor.x, cursor.y);
        if cursor.visible {
            out.extend_from_slice(b"\x1b[?25h");
        }
    }
}

fn encode_full(grid: &Grid, title: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("\x1b]0;{title}\x07").as_bytes());
    out.extend_from_slice(b"\x1b[?25l\x1b[H\x1b[2J");
    let mut current = Style::default();
    let mut at_x: u16 = 0;
    let mut at_y: u16 = 0;
    for y in 0..grid.rows {
        for x in 0..grid.cols {
            let index = grid.index(x, y).expect("grid index");
            let cell = &grid.cells[index];
            if cell.continuation {
                at_x = at_x.saturating_add(1);
                continue;
            }
            if x != at_x || y != at_y {
                cursor_position(&mut out, x, y);
            }
            Style::of(cell).emit(&mut out, &mut current);
            out.extend_from_slice(if cell.text.is_empty() {
                b" "
            } else {
                cell.text.as_bytes()
            });
            at_x = x.saturating_add(cell_width(cell));
            at_y = y;
        }
        at_x = 0;
    }
    if current != Style::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    emit_cursor(&mut out, grid.cursor, None);
    out
}

fn encode_diff(old: &Grid, new: &Grid) -> Vec<u8> {
    let mut out = Vec::new();
    let mut current = Style::default();
    for y in 0..new.rows {
        let mut x = 0;
        while x < new.cols {
            if equal_cell(old, new, x, y) {
                x += 1;
                continue;
            }
            // One cell of context on each side catches wide glyphs whose
            // continuation cell changed without their base cell changing.
            let start = x.saturating_sub(1);
            let mut end = x;
            while end + 1 < new.cols && !equal_cell(old, new, end + 1, y) {
                end += 1;
            }
            let end = (end + 1).min(new.cols - 1);
            cursor_position(&mut out, start, y);
            for index in start..=end {
                let cell = &new.cells[new.index(index, y).expect("grid index")];
                if cell.continuation {
                    continue;
                }
                Style::of(cell).emit(&mut out, &mut current);
                out.extend_from_slice(if cell.text.is_empty() {
                    b" "
                } else {
                    cell.text.as_bytes()
                });
            }
            x = end.saturating_add(1);
        }
    }
    if current != Style::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    emit_cursor(&mut out, new.cursor, old.cursor);
    out
}

fn equal_cell(old: &Grid, new: &Grid, x: u16, y: u16) -> bool {
    match (old.index(x, y), new.index(x, y)) {
        (Some(old_index), Some(new_index)) => old.cells[old_index] == new.cells[new_index],
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stitch_mux_core::{Cursor, LayoutNode, Modes, PaneId, Row};
    use stitch_mux_protocol::{PaneGeometry, ViewMode};

    fn snapshot(pane_id: u64, cols: u16, rows: u16) -> PaneSnapshot {
        PaneSnapshot {
            pane_id,
            seq: 1,
            cols,
            rows,
            cells: (0..rows)
                .map(|_| Row {
                    cells: (0..cols).map(|_| Cell::blank()).collect(),
                    wrapped: false,
                })
                .collect(),
            cursor: Cursor {
                visible: true,
                ..Cursor::default()
            },
            modes: Modes {
                cursor_visible: true,
                ..Modes::default()
            },
            title: "sh".into(),
            cwd: None,
            foreground: None,
            shell_marks: None,
            program_status: None,
            alt_screen: false,
            scrollback_len: 0,
            total_lines: 0,
            scrollback_bytes: 0,
        }
    }

    fn scene(cols: u16, rows: u16, focus: u64) -> (Attached, Layout, BTreeMap<u64, PaneSnapshot>) {
        let content = Rect::new(1, 1, cols.saturating_sub(2), rows.saturating_sub(2));
        let attached = Attached {
            session_id: 1,
            session_name: "demo".into(),
            view_mode: ViewMode::Terminal,
            cols,
            rows,
            status_rows: 0,
            attached_clients: 1,
            layout_revision: 3,
            activity: Default::default(),
        };
        let layout = Layout {
            session_id: 1,
            revision: 3,
            cols,
            rows,
            status_rows: 0,
            attached_clients: 1,
            focus: Some(focus),
            root: LayoutNode::leaf(PaneId::new(focus)),
            panes: vec![PaneGeometry {
                pane_id: focus,
                outer: Rect::new(0, 0, cols, rows),
                content,
                title: "sh".into(),
            }],
            zoomed_pane: None,
        };
        let mut panes = BTreeMap::new();
        panes.insert(focus, snapshot(focus, content.cols, content.rows));
        (attached, layout, panes)
    }

    #[test]
    fn full_frame_paints_styles_and_skips_wide_continuations() {
        let (attached, layout, mut panes) = scene(20, 6, 101);
        let pane = panes.get_mut(&101).expect("pane");
        pane.cells[0].cells[0] = Cell {
            text: "A".into(),
            fg: Color::Rgb(1, 2, 3),
            attrs: Attrs::BOLD,
            ..Cell::blank()
        };
        pane.cells[0].cells[1] = Cell {
            text: "B".into(),
            bg: Color::Indexed(4),
            ..Cell::blank()
        };
        pane.cells[0].cells[2] = Cell {
            text: "界".into(),
            width: 2,
            ..Cell::blank()
        };
        pane.cells[0].cells[3] = Cell {
            text: " ".into(),
            width: 0,
            ..Cell::blank()
        };
        let frame = AnsiCompositor::new().full(&attached, &layout, &panes);
        let ansi = String::from_utf8(frame.ansi).expect("utf8");
        assert!(frame.full);
        assert!(ansi.contains("\x1b[38;2;1;2;3m"));
        assert!(ansi.contains("\x1b[1m"));
        assert!(ansi.contains("\x1b[48;5;4m"));
        assert!(ansi.contains('A'));
        assert!(ansi.contains('B'));
        assert!(ansi.contains('界'));
    }

    #[test]
    fn status_bar_and_focused_cursor_are_composed() {
        let (attached, layout, mut panes) = scene(40, 8, 101);
        panes.get_mut(&101).expect("pane").cursor.x = 3;
        panes.get_mut(&101).expect("pane").cursor.y = 2;
        let frame = AnsiCompositor::new().full(&attached, &layout, &panes);
        let ansi = String::from_utf8(frame.ansi).expect("utf8");
        let grid = compose(&attached, &layout, &panes);
        let status: String = (2..40)
            .filter_map(|x| grid.index(x, 0).map(|index| &grid.cells[index]))
            .map(|cell| cell.text.clone())
            .collect();
        assert!(status.contains("[demo]"), "{status:?}");
        assert!(status.contains("clients=1"), "{status:?}");
        assert!(status.contains("focus=101"), "{status:?}");
        // Content starts at (1, 1); cursor (3, 2) maps to 1-based (5, 4).
        assert!(ansi.contains("\x1b[4;5H"), "{ansi:?}");
    }

    #[test]
    fn update_only_emits_changed_cells_and_can_force_a_full_frame() {
        let (attached, layout, mut panes) = scene(20, 6, 101);
        let mut compositor = AnsiCompositor::new();
        let first = compositor.full(&attached, &layout, &panes);
        assert_eq!(first.seq, 1);
        assert!(compositor.update(&attached, &layout, &panes).is_none());

        panes.get_mut(&101).expect("pane").cells[1].cells[0] = Cell {
            text: "z".into(),
            ..Cell::blank()
        };
        let delta = compositor
            .update(&attached, &layout, &panes)
            .expect("changed frame");
        assert!(!delta.full);
        assert_eq!(delta.seq, 2);
        let ansi = String::from_utf8(delta.ansi).expect("utf8");
        assert!(ansi.contains('z'));
        assert!(!ansi.contains("[demo]"));

        let mut resized = layout.clone();
        resized.cols = 30;
        let full = compositor
            .update(&attached, &resized, &panes)
            .expect("geometry change");
        assert!(full.full);
        assert_eq!(full.cols, 30);
    }

    #[test]
    fn composing_is_bounded_for_large_viewports() {
        let (mut attached, mut layout, _) = scene(20, 6, 101);
        attached.cols = u16::MAX;
        attached.rows = u16::MAX;
        layout.cols = u16::MAX;
        layout.rows = u16::MAX;
        let panes = BTreeMap::new();
        let frame = AnsiCompositor::new().full(&attached, &layout, &panes);
        assert_eq!((frame.cols, frame.rows), (MAX_COLS, MAX_ROWS));
    }
}
