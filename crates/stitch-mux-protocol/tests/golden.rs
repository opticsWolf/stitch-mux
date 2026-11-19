//! Golden fixtures for every v1 frame kind. Regenerate with `--nocapture`.

use stitch_mux_core::{
    ActivitySummary, Attrs, CaptureFormat, Cell, Color, Cursor, LayoutNode, Modes, ProgramState,
    ProgramStatus, Rect, Row,
};
use stitch_mux_protocol::{
    ActionInfo, Attached, CaptureRequest, CaptureResponse, CapturedRow, ClientInfo, Clipboard,
    ClipboardSelection, Command, CommandRequest, CommandResponse, Detach, ErrorCode, ErrorInfo,
    Event, Focus, FocusRequest, Frame, Hello, HelloAck, InputEvent, KeyCode, KeyEvent,
    KeyEventKind, Layout, MessageKind, PaneDelta, PaneGeometry, PaneInfo, PaneSnapshot,
    ProcessInfo, ResizeEvent, ResizeRequest, Resync, RowUpdate, SessionExit, SessionInfo,
    ShellMarks, SubscribeEvents, ViewMode, WaitResult, WaitTarget, decode_message, encode_message,
    encode_render,
};

fn fixture<T: serde::Serialize>(kind: MessageKind, value: &T) -> String {
    let bytes = encode_message(kind, value).unwrap().encode();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn row() -> Row {
    Row {
        cells: vec![Cell {
            text: "a".into(),
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::BOLD,
            width: 1,
            link: Some("https://example.com".into()),
        }],
        wrapped: false,
    }
}

fn cursor() -> Cursor {
    Cursor {
        x: 1,
        y: 2,
        visible: true,
        ..Cursor::default()
    }
}

fn modes() -> Modes {
    Modes {
        alt_screen: false,
        app_cursor: true,
        bracketed_paste: true,
        mouse_proto: 1006,
        sgr_mouse: true,
        cursor_visible: true,
        keyboard_flags: 1,
    }
}

fn status() -> ProgramStatus {
    ProgramStatus {
        id: Some("job-1".into()),
        state: ProgramState::Working,
        kind: None,
        app: Some("cargo".into()),
        message: Some("test".into()),
        updated_at_unix_ms: 1_700_000_000_000,
    }
}

fn all_fixtures() -> Vec<(MessageKind, String)> {
    let hello = Hello {
        protocol_version: 1,
        client_name: "golden".into(),
        client_instance: "inst".into(),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: 1,
    };
    let pane_snapshot = PaneSnapshot {
        pane_id: 7,
        seq: 3,
        cols: 1,
        rows: 1,
        cells: vec![row()],
        cursor: cursor(),
        modes: modes(),
        title: "sh".into(),
        cwd: Some("/tmp".into()),
        foreground: Some(ProcessInfo {
            pid: Some(11),
            name: "sh".into(),
            cwd: None,
        }),
        shell_marks: Some(ShellMarks {
            prompt_rows: vec![2],
            command_rows: vec![3],
        }),
        program_status: Some(status()),
        alt_screen: false,
        scrollback_len: 9,
        scrollback_bytes: 128,
    };
    let render = encode_render(5, 80, 24, true, b"\x1b[2J");
    let render_hex = render
        .encode()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let command = CommandRequest {
        request_id: 1,
        command: Command::ListSessions,
    };
    vec![
        (MessageKind::Hello, fixture(MessageKind::Hello, &hello)),
        (
            MessageKind::HelloAck,
            fixture(
                MessageKind::HelloAck,
                &HelloAck {
                    protocol_version: 1,
                    server_version: "0.1.0-dev".into(),
                    capabilities: 3,
                    sessions: 2,
                },
            ),
        ),
        (
            MessageKind::Attach,
            fixture(
                MessageKind::Attach,
                &stitch_mux_protocol::Attach {
                    session_name: "demo".into(),
                    takeover: false,
                },
            ),
        ),
        (
            MessageKind::Attached,
            fixture(
                MessageKind::Attached,
                &Attached {
                    session_id: 1,
                    session_name: "demo".into(),
                    view_mode: ViewMode::Structured,
                    cols: 80,
                    rows: 24,
                    status_rows: 1,
                    attached_clients: 1,
                    layout_revision: 4,
                    activity: ActivitySummary {
                        working: 1,
                        ..Default::default()
                    },
                },
            ),
        ),
        (
            MessageKind::Layout,
            fixture(
                MessageKind::Layout,
                &Layout {
                    session_id: 1,
                    revision: 4,
                    cols: 80,
                    rows: 24,
                    status_rows: 1,
                    attached_clients: 2,
                    focus: Some(7),
                    root: LayoutNode::leaf(stitch_mux_core::PaneId::new(7)),
                    panes: vec![PaneGeometry {
                        pane_id: 7,
                        outer: Rect::new(0, 0, 80, 23),
                        content: Rect::new(1, 1, 78, 21),
                        title: "sh".into(),
                    }],
                },
            ),
        ),
        (
            MessageKind::PaneSnapshot,
            fixture(MessageKind::PaneSnapshot, &pane_snapshot),
        ),
        (
            MessageKind::PaneDelta,
            fixture(
                MessageKind::PaneDelta,
                &PaneDelta {
                    pane_id: 7,
                    seq: 4,
                    rows: vec![RowUpdate {
                        row: 1,
                        cells: vec![Cell::blank()],
                        wrapped: true,
                    }],
                    cursor: cursor(),
                    modes: modes(),
                    title: "sh".into(),
                    cwd: None,
                    foreground: None,
                    shell_marks: None,
                    program_status: Some(status()),
                    alt_screen: false,
                    scrollback_len: 10,
                    scrollback_bytes: 160,
                    bell: true,
                },
            ),
        ),
        (
            MessageKind::Focus,
            fixture(MessageKind::Focus, &Focus { pane_id: 7 }),
        ),
        (MessageKind::Render, render_hex),
        (
            MessageKind::Input,
            fixture(
                MessageKind::Input,
                &InputEvent::Key(KeyEvent {
                    code: KeyCode::Char('a'),
                    modifiers: 1,
                    kind: KeyEventKind::Press,
                    text: Some("a".into()),
                }),
            ),
        ),
        (
            MessageKind::ResizeRequest,
            fixture(
                MessageKind::ResizeRequest,
                &ResizeRequest {
                    cols: 100,
                    rows: 30,
                    pixel_width: 800,
                    pixel_height: 600,
                },
            ),
        ),
        (
            MessageKind::ResizeEvent,
            fixture(
                MessageKind::ResizeEvent,
                &ResizeEvent {
                    cols: 100,
                    rows: 30,
                },
            ),
        ),
        (
            MessageKind::FocusRequest,
            fixture(MessageKind::FocusRequest, &FocusRequest { pane_id: 7 }),
        ),
        (
            MessageKind::SetLayout,
            fixture(
                MessageKind::SetLayout,
                &stitch_mux_protocol::SetLayout {
                    base_revision: 4,
                    root: LayoutNode::leaf(stitch_mux_core::PaneId::new(7)),
                },
            ),
        ),
        (
            MessageKind::Detach,
            fixture(
                MessageKind::Detach,
                &Detach {
                    reason: "user".into(),
                },
            ),
        ),
        (
            MessageKind::Clipboard,
            fixture(
                MessageKind::Clipboard,
                &Clipboard {
                    pane_id: 7,
                    selection: ClipboardSelection::Clipboard,
                    text: Some("copy".into()),
                },
            ),
        ),
        (
            MessageKind::Exit,
            fixture(
                MessageKind::Exit,
                &SessionExit {
                    session_id: 1,
                    pane_id: Some(7),
                    reason: "exited".into(),
                },
            ),
        ),
        (
            MessageKind::Error,
            fixture(
                MessageKind::Error,
                &ErrorInfo {
                    code: ErrorCode::NotFound,
                    message: "no session".into(),
                },
            ),
        ),
        (MessageKind::Ping, fixture(MessageKind::Ping, &1_u8)),
        (MessageKind::Pong, fixture(MessageKind::Pong, &2_u8)),
        (
            MessageKind::Resync,
            fixture(
                MessageKind::Resync,
                &Resync {
                    reason: "seq_gap".into(),
                },
            ),
        ),
        (
            MessageKind::CaptureRequest,
            fixture(
                MessageKind::CaptureRequest,
                &CaptureRequest {
                    pane_id: 7,
                    start: 0,
                    lines: 100,
                    format: CaptureFormat::Text,
                },
            ),
        ),
        (
            MessageKind::CaptureResponse,
            fixture(
                MessageKind::CaptureResponse,
                &CaptureResponse {
                    pane_id: 7,
                    start: 0,
                    total_lines: 1,
                    format: CaptureFormat::Cells,
                    rows: vec![CapturedRow {
                        cells: vec![Cell::blank()],
                        wrapped: false,
                    }],
                    text: None,
                    html: None,
                },
            ),
        ),
        (
            MessageKind::CommandRequest,
            fixture(MessageKind::CommandRequest, &command),
        ),
        (
            MessageKind::CommandResponse,
            fixture(
                MessageKind::CommandResponse,
                &CommandResponse {
                    request_id: 1,
                    ok: true,
                    sessions: vec![SessionInfo {
                        id: 1,
                        name: "demo".into(),
                        cols: 80,
                        rows: 24,
                        pane_count: 1,
                        detached_pane_count: 0,
                        window_count: 1,
                        attached_clients: 1,
                        created_at_unix_ms: 1,
                        last_activity_unix_ms: 2,
                        activity: ActivitySummary::default(),
                        panes: vec![PaneInfo {
                            id: 7,
                            title: "sh".into(),
                            cwd: None,
                            cols: 80,
                            rows: 24,
                            alive: true,
                            placement: stitch_mux_protocol::PanePlacement::Tiled,
                            window_id: Some(1),
                            owner_client_id: None,
                            program_status: None,
                        }],
                    }],
                    clients: vec![ClientInfo {
                        id: 3,
                        name: "stitch-mux-gui-egui".into(),
                        view_mode: ViewMode::Structured,
                        session_id: Some(1),
                        cols: 80,
                        rows: 24,
                        focus: Some(7),
                        attached_at_unix_ms: 4,
                    }],
                    actions: vec![ActionInfo {
                        name: "pane.split".into(),
                        title: "Split".into(),
                        description: None,
                        category: Some("pane".into()),
                        args_schema: None,
                    }],
                    layout: Some(Layout {
                        session_id: 1,
                        revision: 5,
                        cols: 80,
                        rows: 24,
                        status_rows: 1,
                        attached_clients: 2,
                        focus: Some(7),
                        root: LayoutNode::leaf(stitch_mux_core::PaneId::new(7)),
                        panes: vec![PaneGeometry {
                            pane_id: 7,
                            outer: Rect::new(0, 0, 80, 23),
                            content: Rect::new(1, 1, 78, 21),
                            title: "sh".into(),
                        }],
                    }),
                    wait: Some(WaitResult {
                        target: WaitTarget::Pane { pane_id: 7 },
                        exit_code: Some(0),
                        timed_out: false,
                    }),
                    ..Default::default()
                },
            ),
        ),
        (
            MessageKind::SubscribeEvents,
            fixture(
                MessageKind::SubscribeEvents,
                &SubscribeEvents {
                    session: Some("demo".into()),
                    all: false,
                    since_seq: Some(40),
                },
            ),
        ),
        (
            MessageKind::Event,
            fixture(
                MessageKind::Event,
                &Event {
                    seq: 41,
                    session_id: Some(1),
                    kind: "pane_exited".into(),
                    payload: serde_json::json!({"pane_id": 7}),
                },
            ),
        ),
    ]
}

/// Expected frames recorded from the v1 schema at the start of M1.
const EXPECTED: [(u8, &str); 27] = [
    (
        1,
        "534d010001001e0000009901a6676f6c64656ea4696e7374aa737472756374757265645018000001",
    ),
    (2, "534d010002000e0000009401a9302e312e302d6465760302"),
    (3, "534d010003000700000092a464656d6fc2"),
    (
        4,
        "534d010004001d0000009901a464656d6faa737472756374757265645018010104950001000000",
    ),
    (
        5,
        "534d0100050020000000990104501801020781a46c656166910791940794000050179401014e15a27368",
    ),
    (
        6,
        "534d01000600820000009f0703010191929196a161a764656661756c74a764656661756c740101b368747470733a2f2f6578616d706c652e636f6dc2950102c3a5626c6f636bc297c2c3c3cd03eec3c301a27368a42f746d70930ba27368c0929102910396a56a6f622d31a7776f726b696e67c0a5636172676fa474657374cf0000018bcfe56800c209cc80",
    ),
    (
        7,
        "534d01000700610000009e07049193019196a0a764656661756c74a764656661756c740001c0c3950102c3a5626c6f636bc297c2c3c3cd03eec3c301a27368c0c0c096a56a6f622d31a7776f726b696e67c0a5636172676fa474657374cf0000018bcfe56800c20acca0c3",
    ),
    (8, "534d01000800020000009107"),
    (9, "534d0100090011000000050000000000000050001800011b5b324a"),
    (
        10,
        "534d01000a001700000081a36b65799481a463686172a16101a57072657373a161",
    ),
    (11, "534d01000b000900000094641ecd0320cd0258"),
    (12, "534d01000c000300000092641e"),
    (13, "534d01000d00020000009107"),
    (14, "534d01000e000a000000920481a46c6561669107"),
    (15, "534d01000f000600000091a475736572"),
    (16, "534d01001000110000009307a9636c6970626f617264a4636f7079"),
    (17, "534d010011000a000000930107a6657869746564"),
    (
        18,
        "534d010012001600000092a96e6f745f666f756e64aa6e6f2073657373696f6e",
    ),
    (19, "534d010013000100000001"),
    (20, "534d010014000100000002"),
    (21, "534d010015000900000091a77365715f676170"),
    (22, "534d010016000900000094070064a474657874"),
    (
        23,
        "534d010017002500000097070001a563656c6c7391929196a0a764656661756c74a764656661756c740001c0c2c0c0",
    ),
    (24, "534d0100180011000000920191ad6c6973745f73657373696f6e73"),
    (
        25,
        "534d010019009d0000009c01c3c0919c01a464656d6f5018010001010102950000000000919a07a27368c05018c3a574696c656401c0c0919803b37374697463682d6d75782d6775692d65677569aa73747275637475726564015018070490c0990105501801020781a46c656166910791940794000050179401014e15a27368c09381a470616e65910700c29195aa70616e652e73706c6974a553706c6974c0a470616e65c0c0",
    ),
    (26, "534d01001a000800000093a464656d6fc228"),
    (
        27,
        "534d01001b0019000000942901ab70616e655f65786974656481a770616e655f696407",
    ),
];

#[test]
fn every_message_kind_has_a_stable_encoding() {
    let fixtures = all_fixtures();
    assert_eq!(fixtures.len(), EXPECTED.len());
    if std::env::var_os("STITCH_MUX_REGEN_GOLDEN").is_some() {
        println!("const EXPECTED: [(u8, &str); {}] = [", fixtures.len());
        for (kind, hex) in &fixtures {
            println!("    ({}, \"{}\"),", kind.as_u8(), hex);
        }
        println!("];");
    }
    for ((kind, hex), (expected_kind, expected_hex)) in fixtures.iter().zip(EXPECTED.iter()) {
        assert_eq!(kind.as_u8(), *expected_kind);
        assert_eq!(hex, expected_hex, "wrong encoding for {kind:?}");
    }
}

#[test]
fn older_peers_may_omit_optional_snapshot_fields() {
    let snapshot = PaneSnapshot {
        pane_id: 7,
        seq: 3,
        cols: 1,
        rows: 1,
        cells: vec![row()],
        cursor: cursor(),
        modes: modes(),
        title: "sh".into(),
        cwd: Some("/tmp".into()),
        foreground: Some(ProcessInfo {
            pid: Some(11),
            name: "sh".into(),
            cwd: Some("/tmp".into()),
        }),
        shell_marks: Some(ShellMarks {
            prompt_rows: vec![2],
            command_rows: vec![3],
        }),
        program_status: Some(status()),
        alt_screen: false,
        scrollback_len: 9,
        scrollback_bytes: 128,
    };
    let mut value = serde_json::to_value(&snapshot).expect("snapshot to json");
    let object = value.as_object_mut().expect("snapshot object");
    for key in ["cwd", "foreground", "shell_marks", "program_status"] {
        object.remove(key);
    }
    let payload = rmp_serde::to_vec_named(&value).expect("old payload");
    let frame = Frame::new(MessageKind::PaneSnapshot, payload).unwrap();
    let decoded: PaneSnapshot = decode_message(&frame).expect("old peer decodes");
    assert_eq!(decoded.title, "sh");
    assert_eq!(decoded.cwd, None);
    assert_eq!(decoded.foreground, None);
    assert_eq!(decoded.shell_marks, None);
    assert_eq!(decoded.program_status, None);
}
