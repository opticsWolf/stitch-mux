//! M4 ANSI conformance corpus.
//!
//! These fixtures reproduce the cell/style patterns produced by the common
//! local programs named in the implementation plan. They exercise the same
//! authoritative snapshots that a real child produces, without depending on
//! which programs happen to be installed on a CI runner.

use std::collections::BTreeMap;

use stitch_mux::core::{Attrs, Cell, Color, Cursor, LayoutNode, Modes, PaneId, Rect, Row};
use stitch_mux::protocol::{Attached, Layout, PaneGeometry, PaneSnapshot, ViewMode};
use stitch_mux::server::AnsiCompositor;

fn blank(cols: u16, rows: u16) -> Vec<Row> {
    (0..rows)
        .map(|_| Row {
            cells: (0..cols).map(|_| Cell::blank()).collect(),
            wrapped: false,
        })
        .collect()
}

fn write(rows: &mut [Row], y: usize, x: usize, text: &str, cell: Cell) {
    for (offset, ch) in text.chars().enumerate() {
        let mut glyph = cell.clone();
        glyph.text = ch.to_string().into();
        rows[y].cells[x + offset] = glyph;
    }
}

fn scene(cols: u16, rows: u16, title: &str, alt_screen: bool) -> (Attached, Layout, PaneSnapshot) {
    let attached = Attached {
        session_id: 1,
        session_name: "conformance".into(),
        view_mode: ViewMode::Terminal,
        cols,
        rows,
        status_rows: 0,
        attached_clients: 1,
        layout_revision: 1,
        activity: Default::default(),
    };
    let layout = Layout {
        session_id: 1,
        revision: 1,
        cols,
        rows,
        status_rows: 0,
        attached_clients: 1,
        focus: Some(101),
        root: LayoutNode::leaf(PaneId::new(101)),
        panes: vec![PaneGeometry {
            pane_id: 101,
            outer: Rect::new(0, 0, cols, rows),
            content: Rect::new(1, 1, cols - 2, rows - 2),
            title: title.into(),
        }],
        zoomed_pane: None,
        windows: Vec::new(),
        active_window_id: None,
    };
    let snapshot = PaneSnapshot {
        pane_id: 101,
        seq: 1,
        cols: cols - 2,
        rows: rows - 2,
        cells: blank(cols - 2, rows - 2),
        cursor: Cursor {
            x: 0,
            y: 0,
            visible: true,
            ..Cursor::default()
        },
        modes: Modes {
            cursor_visible: true,
            alt_screen,
            ..Modes::default()
        },
        title: title.into(),
        cwd: None,
        foreground: None,
        shell_marks: None,
        program_status: None,
        notification: None,
        progress: None,

        alt_screen,
        scrollback_len: 0,
        total_lines: 0,
        scrollback_bytes: 0,
    };
    (attached, layout, snapshot)
}

fn compose(attached: &Attached, layout: &Layout, snapshot: &PaneSnapshot) -> String {
    let mut panes = BTreeMap::new();
    panes.insert(snapshot.pane_id, snapshot.clone());
    String::from_utf8(AnsiCompositor::new().full(attached, layout, &panes).ansi)
        .expect("compositor emits UTF-8")
}

fn styled(fg: Color, bg: Color, attrs: Attrs) -> Cell {
    Cell {
        fg,
        bg,
        attrs,
        ..Cell::blank()
    }
}

#[test]
fn bash_and_zsh_prompts_keep_color_and_text() {
    let (attached, layout, mut snapshot) = scene(60, 8, "bash", false);
    write(
        &mut snapshot.cells,
        0,
        0,
        "user@host",
        styled(Color::Rgb(40, 200, 120), Color::Default, Attrs::BOLD),
    );
    write(
        &mut snapshot.cells,
        0,
        10,
        " ~/src ",
        styled(Color::Rgb(90, 150, 255), Color::Default, Attrs::empty()),
    );
    let ansi = compose(&attached, &layout, &snapshot);
    assert!(ansi.contains("user@host"));
    assert!(ansi.contains("~/src"));
    assert!(ansi.contains("\x1b[38;2;40;200;120m"));
    assert!(ansi.contains("\x1b[38;2;90;150;255m"));
}

#[test]
fn vim_alternate_screen_keeps_tilde_fill_and_reverse_status() {
    let (attached, layout, mut snapshot) = scene(50, 10, "vim", true);
    for row in 0..snapshot.cells.len() - 1 {
        write(
            &mut snapshot.cells,
            row,
            0,
            "~",
            styled(Color::Indexed(8), Color::Default, Attrs::empty()),
        );
    }
    let last = snapshot.cells.len() - 1;
    write(
        &mut snapshot.cells,
        last,
        0,
        "README.md",
        styled(Color::Default, Color::Indexed(7), Attrs::BOLD),
    );
    let ansi = compose(&attached, &layout, &snapshot);
    assert!(ansi.contains('~'));
    assert!(ansi.contains("README.md"));
    assert!(ansi.contains("\x1b[48;5;7m"));
    assert!(ansi.contains("\x1b[7m") || ansi.contains("\x1b[48;5;"));
}

#[test]
fn less_pager_keeps_the_bottom_prompt() {
    let (attached, layout, mut snapshot) = scene(60, 8, "less", true);
    write(
        &mut snapshot.cells,
        0,
        0,
        "line one",
        styled(Color::Default, Color::Default, Attrs::empty()),
    );
    let last = snapshot.cells.len() - 1;
    write(
        &mut snapshot.cells,
        last,
        0,
        ":",
        styled(Color::Default, Color::Default, Attrs::empty()),
    );
    let ansi = compose(&attached, &layout, &snapshot);
    assert!(ansi.contains("line one"));
    assert!(ansi.contains(':'));
}

#[test]
fn htop_bars_keep_indexed_colors() {
    let (attached, layout, mut snapshot) = scene(60, 8, "htop", true);
    write(
        &mut snapshot.cells,
        0,
        0,
        "[||||      ]",
        styled(Color::Indexed(2), Color::Indexed(0), Attrs::BOLD),
    );
    let ansi = compose(&attached, &layout, &snapshot);
    assert!(ansi.contains("[||||"));
    assert!(ansi.contains("\x1b[38;5;2m"));
    assert!(ansi.contains("\x1b[48;5;0m"));
}

#[test]
fn git_log_graph_keeps_wide_and_styled_runs() {
    let (attached, layout, mut snapshot) = scene(60, 8, "git log", false);
    write(
        &mut snapshot.cells,
        0,
        0,
        "* commit",
        styled(Color::Rgb(255, 120, 80), Color::Default, Attrs::BOLD),
    );
    snapshot.cells[1].cells[0] = Cell {
        text: "界".into(),
        width: 2,
        ..Cell::blank()
    };
    snapshot.cells[1].cells[1] = Cell {
        text: " ".into(),
        width: 0,
        ..Cell::blank()
    };
    let ansi = compose(&attached, &layout, &snapshot);
    assert!(ansi.contains("* commit"));
    assert!(ansi.contains('界'));
    assert!(ansi.contains("\x1b[38;2;255;120;80m"));
}
