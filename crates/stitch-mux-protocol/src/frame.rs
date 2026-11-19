use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 10;
pub const MAX_LARGE_PAYLOAD: u32 = 8 * 1024 * 1024;
pub const MAX_SMALL_PAYLOAD: u32 = 1024 * 1024;
const MAGIC: [u8; 2] = [0x53, 0x4D];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MessageKind {
    Hello = 1,
    HelloAck = 2,
    Attach = 3,
    Attached = 4,
    Layout = 5,
    PaneSnapshot = 6,
    PaneDelta = 7,
    Focus = 8,
    Render = 9,
    Input = 10,
    ResizeRequest = 11,
    ResizeEvent = 12,
    FocusRequest = 13,
    SetLayout = 14,
    Detach = 15,
    Clipboard = 16,
    Exit = 17,
    Error = 18,
    Ping = 19,
    Pong = 20,
    Resync = 21,
    CaptureRequest = 22,
    CaptureResponse = 23,
    CommandRequest = 24,
    CommandResponse = 25,
    SubscribeEvents = 26,
    Event = 27,
}

impl MessageKind {
    /// Every v1 message kind, in wire order. Used by conformance tests.
    pub const ALL: [Self; 27] = [
        Self::Hello,
        Self::HelloAck,
        Self::Attach,
        Self::Attached,
        Self::Layout,
        Self::PaneSnapshot,
        Self::PaneDelta,
        Self::Focus,
        Self::Render,
        Self::Input,
        Self::ResizeRequest,
        Self::ResizeEvent,
        Self::FocusRequest,
        Self::SetLayout,
        Self::Detach,
        Self::Clipboard,
        Self::Exit,
        Self::Error,
        Self::Ping,
        Self::Pong,
        Self::Resync,
        Self::CaptureRequest,
        Self::CaptureResponse,
        Self::CommandRequest,
        Self::CommandResponse,
        Self::SubscribeEvents,
        Self::Event,
    ];

    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn large_payload(self) -> bool {
        matches!(
            self,
            // CommandResponse can carry a bounded CaptureResponse.
            Self::Render | Self::PaneSnapshot | Self::CaptureResponse | Self::CommandResponse
        )
    }

    pub fn max_payload(self) -> u32 {
        if self.large_payload() {
            MAX_LARGE_PAYLOAD
        } else {
            MAX_SMALL_PAYLOAD
        }
    }
}

impl TryFrom<u8> for MessageKind {
    type Error = FrameError;

    fn try_from(value: u8) -> Result<Self, FrameError> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::HelloAck),
            3 => Ok(Self::Attach),
            4 => Ok(Self::Attached),
            5 => Ok(Self::Layout),
            6 => Ok(Self::PaneSnapshot),
            7 => Ok(Self::PaneDelta),
            8 => Ok(Self::Focus),
            9 => Ok(Self::Render),
            10 => Ok(Self::Input),
            11 => Ok(Self::ResizeRequest),
            12 => Ok(Self::ResizeEvent),
            13 => Ok(Self::FocusRequest),
            14 => Ok(Self::SetLayout),
            15 => Ok(Self::Detach),
            16 => Ok(Self::Clipboard),
            17 => Ok(Self::Exit),
            18 => Ok(Self::Error),
            19 => Ok(Self::Ping),
            20 => Ok(Self::Pong),
            21 => Ok(Self::Resync),
            22 => Ok(Self::CaptureRequest),
            23 => Ok(Self::CaptureResponse),
            24 => Ok(Self::CommandRequest),
            25 => Ok(Self::CommandResponse),
            26 => Ok(Self::SubscribeEvents),
            27 => Ok(Self::Event),
            other => Err(FrameError::UnknownKind(other)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub kind: MessageKind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: MessageKind, payload: Vec<u8>) -> Result<Self, FrameError> {
        let max = kind.max_payload();
        if payload.len() as u32 > max {
            return Err(FrameError::TooLarge {
                len: payload.len() as u32,
                max,
                kind,
            });
        }
        Ok(Self { kind, payload })
    }

    pub fn encode(&self) -> Vec<u8> {
        let len = self.payload.len() as u32;
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        out.push(self.kind.as_u8());
        out.push(0);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("bad frame magic {0:?}")]
    BadMagic([u8; 2]),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("reserved flags must be zero, got {0}")]
    BadFlags(u8),
    #[error("unknown message kind {0}")]
    UnknownKind(u8),
    #[error("payload of {len} bytes exceeds maximum {max} for {kind:?}")]
    TooLarge {
        len: u32,
        max: u32,
        kind: MessageKind,
    },
    #[error("messagepack encode failed: {0}")]
    Encode(String),
    #[error("messagepack decode failed: {0}")]
    Decode(String),
    #[error("wrong payload kind: expected {expected:?}, got {actual:?}")]
    WrongKind {
        expected: MessageKind,
        actual: MessageKind,
    },
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Incremental parser for frames split or coalesced across socket reads.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    read: usize,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, FrameError> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let remaining = self.buf.len() - self.read;
            if remaining < HEADER_LEN {
                break;
            }
            let header = &self.buf[self.read..self.read + HEADER_LEN];
            let magic = [header[0], header[1]];
            if magic != MAGIC {
                return Err(FrameError::BadMagic(magic));
            }
            let version = u16::from_le_bytes([header[2], header[3]]);
            if version != PROTOCOL_VERSION {
                return Err(FrameError::UnsupportedVersion(version));
            }
            let kind = MessageKind::try_from(header[4])?;
            if header[5] != 0 {
                return Err(FrameError::BadFlags(header[5]));
            }
            let len = u32::from_le_bytes([header[6], header[7], header[8], header[9]]);
            let max = kind.max_payload();
            if len > max {
                return Err(FrameError::TooLarge { len, max, kind });
            }
            let len = len as usize;
            if remaining < HEADER_LEN + len {
                break;
            }
            let start = self.read + HEADER_LEN;
            let payload = self.buf[start..start + len].to_vec();
            self.read = start + len;
            frames.push(Frame { kind, payload });
        }
        if self.read > 0 {
            if self.read == self.buf.len() {
                self.buf.clear();
                self.read = 0;
            } else if self.read >= 64 * 1024 {
                self.buf.drain(..self.read);
                self.read = 0;
            }
        }
        Ok(frames)
    }

    pub fn buffered(&self) -> usize {
        self.buf.len() - self.read
    }
}

pub fn encode_message<T: Serialize>(kind: MessageKind, value: &T) -> Result<Frame, FrameError> {
    let payload = rmp_serde::to_vec(value).map_err(|err| FrameError::Encode(err.to_string()))?;
    Frame::new(kind, payload)
}

pub fn decode_message<T: DeserializeOwned>(frame: &Frame) -> Result<T, FrameError> {
    rmp_serde::from_slice(&frame.payload).map_err(|err| FrameError::Decode(err.to_string()))
}

/// Render frames use a compact binary prefix followed by raw ANSI bytes:
/// seq u64 LE, cols u16 LE, rows u16 LE, full u8, then the ANSI payload.
pub fn encode_render(seq: u64, cols: u16, rows: u16, full: bool, ansi: &[u8]) -> Frame {
    let mut payload = Vec::with_capacity(13 + ansi.len());
    payload.extend_from_slice(&seq.to_le_bytes());
    payload.extend_from_slice(&cols.to_le_bytes());
    payload.extend_from_slice(&rows.to_le_bytes());
    payload.push(u8::from(full));
    payload.extend_from_slice(ansi);
    Frame::new(MessageKind::Render, payload)
        .expect("render frames must stay within the 8 MiB limit")
}

pub fn decode_render(frame: &Frame) -> Result<(u64, u16, u16, bool, Vec<u8>), FrameError> {
    if frame.kind != MessageKind::Render {
        return Err(FrameError::WrongKind {
            expected: MessageKind::Render,
            actual: frame.kind,
        });
    }
    if frame.payload.len() < 13 {
        return Err(FrameError::Decode(
            "render frame is shorter than its header".into(),
        ));
    }
    let bytes = &frame.payload;
    let seq = u64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes"));
    let cols = u16::from_le_bytes([bytes[8], bytes[9]]);
    let rows = u16::from_le_bytes([bytes[10], bytes[11]]);
    let full = bytes[12] != 0;
    Ok((seq, cols, rows, full, bytes[13..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_one_split_frame() {
        let frame = encode_message(MessageKind::Ping, &"hello").unwrap();
        let bytes = frame.encode();
        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(&bytes[..4]).unwrap(), vec![]);
        assert_eq!(decoder.push(&bytes[4..]).unwrap(), vec![frame]);
    }

    #[test]
    fn coalesces_multiple_frames() {
        let a = encode_message(MessageKind::Ping, &1_u8).unwrap().encode();
        let b = encode_message(MessageKind::Pong, &2_u8).unwrap().encode();
        let mut both = a.clone();
        both.extend_from_slice(&b);
        let mut decoder = FrameDecoder::new();
        let frames = decoder.push(&both).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].kind, MessageKind::Ping);
        assert_eq!(frames[1].kind, MessageKind::Pong);
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn rejects_bad_magic_version_flags_kind_and_size() {
        let good = encode_message(MessageKind::Ping, &1_u8).unwrap().encode();
        let mut bad = good.clone();
        bad[0] = 0;
        assert!(matches!(
            FrameDecoder::new().push(&bad),
            Err(FrameError::BadMagic(_))
        ));
        let mut bad = good.clone();
        bad[2] = 9;
        assert!(matches!(
            FrameDecoder::new().push(&bad),
            Err(FrameError::UnsupportedVersion(9))
        ));
        let mut bad = good.clone();
        bad[5] = 1;
        assert!(matches!(
            FrameDecoder::new().push(&bad),
            Err(FrameError::BadFlags(1))
        ));
        let mut bad = good.clone();
        bad[4] = 200;
        assert!(matches!(
            FrameDecoder::new().push(&bad),
            Err(FrameError::UnknownKind(200))
        ));
        let mut bad = good;
        bad[6..10].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            FrameDecoder::new().push(&bad),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_oversize_before_allocating() {
        assert!(Frame::new(MessageKind::Ping, vec![0; 1024 * 1024 + 1]).is_err());
        assert!(Frame::new(MessageKind::PaneSnapshot, vec![0; 1024 * 1024 + 1]).is_ok());
        assert!(Frame::new(MessageKind::CommandResponse, vec![0; 4 * 1024 * 1024]).is_ok());
    }

    #[test]
    fn round_trips_every_message_kind() {
        for kind in MessageKind::ALL {
            let frame = Frame::new(kind, b"payload".to_vec()).unwrap();
            let encoded = frame.encode();
            // Decode byte-by-byte as well: framing must not depend on the
            // payload arriving in one read.
            let mut decoder = FrameDecoder::new();
            let mut decoded = Vec::new();
            for byte in encoded {
                decoded.extend(decoder.push(&[byte]).unwrap());
            }
            assert_eq!(decoder.buffered(), 0);
            assert_eq!(decoded, vec![frame], "round trip failed for {kind:?}");
        }
    }

    #[test]
    fn truncated_payload_waits_for_the_rest() {
        let frame = Frame::new(MessageKind::Ping, b"hello".to_vec()).unwrap();
        let encoded = frame.encode();
        let split = HEADER_LEN + 2;
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&encoded[..split]).unwrap().is_empty());
        assert_eq!(decoder.buffered(), split);
        assert_eq!(decoder.push(&encoded[split..]).unwrap(), vec![frame]);
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn render_uses_raw_ansi_payload() {
        let frame = encode_render(7, 80, 24, true, b"[2J");
        assert_eq!(frame.kind, MessageKind::Render);
        assert_eq!(
            decode_render(&frame).unwrap(),
            (7, 80, 24, true, b"[2J".to_vec())
        );
    }
}
