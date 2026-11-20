//! Mux-side OSC 7501 scanner.
//!
//! `stitch-pty` owns the terminal screen but does not expose program-status
//! records. Observe the same PTY bytes, decode the generic key/value payload
//! described in ARCHITECTURE.md, and store only the latest record. This never
//! forks the emulator and never becomes an event queue.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use stitch_mux_core::{ProgramState, ProgramStatus, ProgramStatusKind};

const PREFIX: &[u8] = b"\x1b]7501;";
/// A status record is metadata, not a terminal payload. Refuse to buffer an
/// unbounded OSC if a child emits the introducer and never terminates it.
const MAX_RECORD: usize = 8 * 1024;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Default)]
pub struct StatusScanner {
    pending: Vec<u8>,
}

impl StatusScanner {
    /// Return the last complete status record in `bytes`, if any.
    pub fn scan(&mut self, bytes: &[u8]) -> Option<ProgramStatus> {
        self.pending.extend_from_slice(bytes);
        let mut latest = None;
        let mut from = 0;
        loop {
            let Some(relative) = find_bytes(&self.pending[from..], PREFIX) else {
                // Keep only a possible partial prefix at the tail.
                let keep = PREFIX.len().saturating_sub(1).min(self.pending.len());
                let drop = self.pending.len() - keep;
                if drop > 0 {
                    self.pending.drain(..drop);
                }
                break;
            };
            let start = from + relative;
            let payload_start = start + PREFIX.len();
            let Some((end, skip)) = terminator(&self.pending, payload_start) else {
                if start > 0 {
                    self.pending.drain(..start);
                }
                if self.pending.len() > MAX_RECORD {
                    self.pending.clear();
                }
                break;
            };
            let payload = &self.pending[payload_start..end];
            if payload.len() <= MAX_RECORD
                && let Some(status) = parse_status(payload)
            {
                latest = Some(status);
            }
            from = end + skip;
            if from >= self.pending.len() {
                self.pending.clear();
                break;
            }
        }
        latest
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// OSC strings may end with BEL or ST (`ESC \`).
fn terminator(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    for index in from..bytes.len() {
        if bytes[index] == 0x07 {
            return Some((index, 1));
        }
        if bytes[index] == 0x1b && index + 1 < bytes.len() && bytes[index + 1] == b'\\' {
            return Some((index, 2));
        }
    }
    None
}

fn parse_status(payload: &[u8]) -> Option<ProgramStatus> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut status = ProgramStatus {
        updated_at_unix_ms: now_unix_ms(),
        ..ProgramStatus::default()
    };
    let mut saw_state = false;
    for field in text.split(':') {
        let Some((key, value)) = field.split_once('=') else {
            continue; // forward-compatible with bare extension fields
        };
        match key {
            "state" => {
                status.state = match value {
                    "idle" => ProgramState::Idle,
                    "working" => ProgramState::Working,
                    "done" => ProgramState::Done,
                    "blocked" => ProgramState::Blocked,
                    "error" => ProgramState::Error,
                    _ => return None,
                };
                saw_state = true;
            }
            "kind" => {
                status.kind = Some(match value {
                    "permission" => ProgramStatusKind::Permission,
                    "question" => ProgramStatusKind::Question,
                    "auth" => ProgramStatusKind::Auth,
                    _ => return None,
                });
            }
            "id" => status.id = Some(value.to_owned()),
            "app" => status.app = Some(value.to_owned()),
            "msg" => status.message = Some(decode_message(value)),
            _ => {} // forward-compatible: ignore unknown fields
        }
    }
    saw_state.then_some(status)
}

fn decode_message(value: &str) -> String {
    let bytes = STANDARD
        .decode(value)
        .or_else(|_| URL_SAFE.decode(value))
        .unwrap_or_else(|_| value.as_bytes().to_vec());
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(payload: &str) -> Vec<u8> {
        let mut bytes = PREFIX.to_vec();
        bytes.extend_from_slice(payload.as_bytes());
        bytes.extend_from_slice(b"\x1b\\");
        bytes
    }

    #[test]
    fn decodes_the_documented_record() {
        let mut scanner = StatusScanner::default();
        let status = scanner
            .scan(&osc(
                "state=working:kind=question:app=demo:id=7:msg=TmVlZCBpbnB1dA==",
            ))
            .expect("status");
        assert_eq!(status.state, ProgramState::Working);
        assert_eq!(status.kind, Some(ProgramStatusKind::Question));
        assert_eq!(status.app.as_deref(), Some("demo"));
        assert_eq!(status.id.as_deref(), Some("7"));
        assert_eq!(status.message.as_deref(), Some("Need input"));
        assert!(status.updated_at_unix_ms > 0);
    }

    #[test]
    fn accepts_bel_and_records_split_across_reads() {
        let mut scanner = StatusScanner::default();
        let mut bytes = PREFIX.to_vec();
        bytes.extend_from_slice(b"state=blocked:msg=UmVh");
        assert!(scanner.scan(&bytes).is_none());
        bytes.clear();
        bytes.extend_from_slice(b"ZGluZw==\x07");
        let status = scanner.scan(&bytes).expect("status");
        assert_eq!(status.state, ProgramState::Blocked);
        assert_eq!(status.message.as_deref(), Some("Reading"));
    }

    #[test]
    fn keeps_the_last_record_and_ignores_malformed_ones() {
        let mut scanner = StatusScanner::default();
        let mut bytes = osc("state=working:msg=V29ya2luZw==");
        bytes.extend(osc("state=nonsense"));
        bytes.extend(osc("state=done:msg=RG9uZQ=="));
        let status = scanner.scan(&bytes).expect("status");
        assert_eq!(status.state, ProgramState::Done);
        assert_eq!(status.message.as_deref(), Some("Done"));
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_state_is_not_a_status() {
        let mut scanner = StatusScanner::default();
        assert!(scanner.scan(&osc("future=yes:app=demo")).is_none());
        let status = scanner
            .scan(&osc("state=error:app=demo:future=yes"))
            .expect("status");
        assert_eq!(status.state, ProgramState::Error);
        assert_eq!(status.app.as_deref(), Some("demo"));
    }

    #[test]
    fn an_unterminated_record_cannot_grow_without_bound() {
        let mut scanner = StatusScanner::default();
        let mut bytes = PREFIX.to_vec();
        bytes.extend(vec![b'x'; MAX_RECORD * 2]);
        assert!(scanner.scan(&bytes).is_none());
        assert!(scanner.pending.len() < PREFIX.len());
    }
}
