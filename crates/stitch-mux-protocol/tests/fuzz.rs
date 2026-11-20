//! Deterministic fuzz smoke for the v1 decoder.
//!
//! No external fuzzer is required in CI: a fixed-seed PRNG mutates valid
//! frames for every payload family, splits the result at arbitrary boundaries,
//! and requires that framing, typed payload decoding, and the raw Render path
//! either return a typed error or produce a value. Panics, out-of-range
//! indexing, and unbounded buffering are failures.

use stitch_mux_core::{Attrs, CaptureFormat, Cell, Color, Cursor, LayoutNode, Modes, Row};
use stitch_mux_protocol::{
    CaptureRequest, CaptureResponse, Command, CommandRequest, Frame, FrameDecoder, InputEvent,
    KeyCode, KeyEvent, Layout, MessageKind, PaneDelta, PaneSnapshot, RowUpdate, SetLayout,
    decode_message, decode_render, encode_message, encode_render,
};

/// Small deterministic PRNG so a failure is reproducible from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn cell() -> Cell {
    Cell {
        text: "x".into(),
        fg: Color::Default,
        bg: Color::Default,
        attrs: Attrs::empty(),
        width: 1,
        link: None,
    }
}

fn snapshot() -> PaneSnapshot {
    PaneSnapshot {
        pane_id: 7,
        seq: 3,
        cols: 2,
        rows: 1,
        cells: vec![Row {
            cells: vec![cell()],
            wrapped: false,
        }],
        cursor: Cursor::default(),
        modes: Modes::default(),
        title: "sh".into(),
        cwd: None,
        foreground: None,
        shell_marks: None,
        program_status: None,
        alt_screen: false,
        scrollback_len: 4,
        scrollback_bytes: 32,
    }
}

fn valid_frames() -> Vec<Frame> {
    let snapshot = snapshot();
    let delta = PaneDelta {
        pane_id: 7,
        seq: 4,
        rows: vec![RowUpdate {
            row: 0,
            cells: vec![cell()],
            wrapped: false,
        }],
        cursor: Cursor::default(),
        modes: Modes::default(),
        title: "sh".into(),
        cwd: None,
        foreground: None,
        shell_marks: None,
        program_status: None,
        alt_screen: false,
        scrollback_len: 5,
        scrollback_bytes: 40,
        bell: false,
    };
    let capture = CaptureResponse {
        pane_id: 7,
        start: 0,
        total_lines: 1,
        format: CaptureFormat::Text,
        rows: vec![stitch_mux_protocol::CapturedRow {
            cells: vec![cell()],
            wrapped: false,
        }],
        text: Some("x".into()),
        html: None,
    };
    let layout = Layout {
        session_id: 1,
        revision: 2,
        cols: 80,
        rows: 24,
        status_rows: 1,
        attached_clients: 1,
        focus: Some(7),
        root: LayoutNode::leaf(stitch_mux_core::PaneId::new(7)),
        panes: Vec::new(),
        zoomed_pane: None,
    };
    let input = InputEvent::Key(KeyEvent {
        code: KeyCode::Char('a'),
        modifiers: 0,
        kind: stitch_mux_protocol::KeyEventKind::Press,
        text: None,
    });
    vec![
        encode_message(MessageKind::PaneSnapshot, &snapshot).unwrap(),
        encode_message(MessageKind::PaneDelta, &delta).unwrap(),
        encode_message(MessageKind::CaptureResponse, &capture).unwrap(),
        encode_message(MessageKind::Layout, &layout).unwrap(),
        encode_message(
            MessageKind::SetLayout,
            &SetLayout {
                base_revision: 2,
                root: LayoutNode::leaf(stitch_mux_core::PaneId::new(7)),
            },
        )
        .unwrap(),
        encode_message(
            MessageKind::CommandRequest,
            &CommandRequest {
                request_id: 1,
                command: Command::ListSessions,
            },
        )
        .unwrap(),
        encode_message(
            MessageKind::CaptureRequest,
            &CaptureRequest {
                pane_id: 7,
                start: 0,
                lines: 100,
                format: CaptureFormat::Text,
            },
        )
        .unwrap(),
        encode_message(MessageKind::Input, &input).unwrap(),
        encode_render(4, 80, 24, true, b"[2J"),
    ]
}

/// Decode any payload family represented by a frame. A type mismatch or a
/// truncated MessagePack value is fine; a panic is not.
fn decode_payload(frame: &Frame) {
    match frame.kind {
        MessageKind::PaneSnapshot => {
            let _ = decode_message::<PaneSnapshot>(frame);
        }
        MessageKind::PaneDelta => {
            let _ = decode_message::<PaneDelta>(frame);
        }
        MessageKind::CaptureResponse => {
            let _ = decode_message::<CaptureResponse>(frame);
        }
        MessageKind::Layout => {
            let _ = decode_message::<Layout>(frame);
        }
        MessageKind::SetLayout => {
            let _ = decode_message::<SetLayout>(frame);
        }
        MessageKind::CommandRequest => {
            let _ = decode_message::<CommandRequest>(frame);
        }
        MessageKind::CaptureRequest => {
            let _ = decode_message::<CaptureRequest>(frame);
        }
        MessageKind::Input => {
            let _ = decode_message::<InputEvent>(frame);
        }
        MessageKind::Render => {
            let _ = decode_render(frame);
        }
        _ => {
            let _ = decode_message::<serde_json::Value>(frame);
        }
    }
}

fn feed(bytes: &[u8], rng: &mut Rng) {
    let mut decoder = FrameDecoder::new();
    let mut at = 0;
    while at < bytes.len() {
        let take = 1 + rng.below(37);
        let end = (at + take).min(bytes.len());
        match decoder.push(&bytes[at..end]) {
            Ok(frames) => {
                for frame in frames {
                    decode_payload(&frame);
                }
            }
            // A malformed envelope has no defined resynchronisation point.
            Err(_) => break,
        }
        assert!(
            decoder.buffered() <= bytes.len(),
            "decoder buffered more than it was given"
        );
        at = end;
    }
}

#[test]
fn mutated_frames_never_panic_or_buffer_unbounded() {
    let seeds = valid_frames();
    let mut rng = Rng(0x5eed_1234_5678_9abc);

    for seed in &seeds {
        let original = seed.encode();
        for _ in 0..120 {
            let mut bytes = original.clone();
            match rng.below(5) {
                0 => {
                    // Bit flips anywhere, including the envelope.
                    for _ in 0..=rng.below(3) {
                        let index = rng.below(bytes.len());
                        bytes[index] ^= 1 << rng.below(8);
                    }
                }
                1 => {
                    // Truncate at a random point, including mid-header.
                    bytes.truncate(rng.below(bytes.len() + 1));
                }
                2 => {
                    // Claim a different payload length.
                    let len = match rng.below(4) {
                        0 => u32::MAX,
                        1 => (bytes.len() as u32).saturating_add(1),
                        other => other as u32,
                    };
                    bytes[6..10].copy_from_slice(&len.to_le_bytes());
                }
                3 => {
                    // Splice two valid streams together.
                    let other = &seeds[rng.below(seeds.len())];
                    bytes.extend_from_slice(&other.encode());
                }
                _ => {
                    // Random single-byte substitution.
                    let index = rng.below(bytes.len());
                    bytes[index] = rng.next() as u8;
                }
            }
            feed(&bytes, &mut rng);
        }
    }
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = Rng(0xd00d_f00d_1234_5678);
    for _ in 0..300 {
        let len = rng.below(320);
        let bytes = (0..len).map(|_| rng.next() as u8).collect::<Vec<_>>();
        feed(&bytes, &mut rng);
    }
}
