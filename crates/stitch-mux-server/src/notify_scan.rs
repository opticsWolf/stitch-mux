//! Mux-side scanner for OSC 9/777 notifications and OSC 9;4 progress.
//!
//! `stitch-pty` passes these OSC sequences through to the screen. The daemon
//! observes the same bytes and keeps only the latest typed record per pane;
//! nothing here queues notifications or gains a remote side effect.

use std::time::{SystemTime, UNIX_EPOCH};

use stitch_mux_protocol::{Notification, Progress, ProgressState};

const PREFIXES: [&[u8]; 3] = [b"\x1b]9;", b"\x1b]777;notify;", b"\x1b]9;4;"];
const MAX_RECORD: usize = 8 * 1024;
const MAX_TEXT: usize = 512;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NotifyScan {
    pub notification: Option<Notification>,
    pub progress: Option<Progress>,
}

#[derive(Default)]
pub struct NotifyScanner {
    pending: Vec<u8>,
    prefix: Option<&'static [u8]>,
}

impl NotifyScanner {
    /// Return the last complete notification/progress record in `bytes`.
    pub fn scan(&mut self, bytes: &[u8]) -> NotifyScan {
        let mut out = NotifyScan::default();
        let mut from = 0;

        if let Some(prefix) = self.prefix {
            let prior = self.pending.len();
            self.pending.extend_from_slice(bytes);
            if let Some((end, skip)) = terminator(&self.pending, prefix.len()) {
                if let Some((notification, progress)) =
                    parse(prefix, &self.pending[prefix.len()..end])
                {
                    out.notification = notification;
                    out.progress = progress;
                }
                from = end
                    .saturating_add(skip)
                    .saturating_sub(prior)
                    .min(bytes.len());
            } else {
                if self.pending.len() > MAX_RECORD {
                    self.pending.clear();
                    self.prefix = None;
                }
                return out;
            }
            self.pending.clear();
            self.prefix = None;
        }

        while let Some((start, prefix)) = self.find_prefix(bytes, from) {
            let payload_start = start + prefix.len();
            let Some((end, skip)) = terminator(bytes, payload_start) else {
                // Retain only a possible partial record at the tail.
                self.pending = bytes[start..].to_vec();
                self.prefix = Some(prefix);
                break;
            };
            if let Some((notification, progress)) = parse(prefix, &bytes[payload_start..end]) {
                if notification.is_some() {
                    out.notification = notification;
                }
                if progress.is_some() {
                    out.progress = progress;
                }
            }
            from = end + skip;
            if from >= bytes.len() {
                break;
            }
        }
        out
    }

    fn find_prefix(&self, bytes: &[u8], from: usize) -> Option<(usize, &'static [u8])> {
        let mut best: Option<(usize, &'static [u8])> = None;
        for prefix in PREFIXES {
            if let Some(offset) = find_bytes(&bytes[from..], prefix) {
                let position = from + offset;
                let better = best.is_none_or(|(current, current_prefix)| {
                    position < current
                        || (position == current && prefix.len() > current_prefix.len())
                });
                if better {
                    best = Some((position, prefix));
                }
            }
        }
        best
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

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

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn sanitize(value: &str) -> Option<String> {
    let text: String = value
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_TEXT)
        .collect();
    (!text.is_empty()).then_some(text)
}

fn parse(prefix: &[u8], payload: &[u8]) -> Option<(Option<Notification>, Option<Progress>)> {
    let text = std::str::from_utf8(payload).ok()?;
    let now = now_unix_ms();
    if prefix == b"\x1b]9;4;" {
        let mut fields = text.split(';');
        let state = match fields.next()? {
            "0" => ProgressState::Remove,
            "1" => ProgressState::Normal,
            "2" => ProgressState::Error,
            "3" => ProgressState::Indeterminate,
            _ => return None,
        };
        let value = fields.next().and_then(|value| value.parse::<u8>().ok());
        let message = fields.next().and_then(sanitize);
        return Some((
            None,
            Some(Progress {
                state,
                value: value.filter(|value| *value <= 100),
                message,
                updated_at_unix_ms: now,
            }),
        ));
    }
    if prefix == b"\x1b]777;notify;" {
        let (title, message) = text.split_once(';')?;
        return Some((
            Some(Notification {
                title: sanitize(title),
                message: sanitize(message)?,
                updated_at_unix_ms: now,
            }),
            None,
        ));
    }
    // OSC 9;;message (and the legacy OSC 9;message form).
    let message = text.trim_start_matches(';');
    Some((
        Some(Notification {
            title: None,
            message: sanitize(message)?,
            updated_at_unix_ms: now,
        }),
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(prefix: &[u8], payload: &str) -> Vec<u8> {
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(payload.as_bytes());
        bytes.push(0x07);
        bytes
    }

    #[test]
    fn parses_notifications_and_progress() {
        let mut scanner = NotifyScanner::default();
        let mut bytes = osc(b"\x1b]9;", ";Build finished");
        bytes.extend(osc(b"\x1b]777;notify;", "CI;Stage 2 failed"));
        bytes.extend(osc(b"\x1b]9;4;", "1;42; uploading"));
        let scan = scanner.scan(&bytes);
        assert_eq!(
            scan.notification.as_ref().unwrap().message,
            "Stage 2 failed"
        );
        assert_eq!(
            scan.notification.as_ref().unwrap().title.as_deref(),
            Some("CI")
        );
        let progress = scan.progress.unwrap();
        assert_eq!(progress.state, ProgressState::Normal);
        assert_eq!(progress.value, Some(42));
        assert_eq!(progress.message.as_deref(), Some("uploading"));
    }

    #[test]
    fn strips_control_characters_and_bounds_text() {
        let mut scanner = NotifyScanner::default();
        let long = "x".repeat(MAX_TEXT + 20);
        let payload = format!(";{long}<script>");
        let scan = scanner.scan(&osc(b"\x1b]9;", &payload));
        let message = &scan.notification.unwrap().message;
        assert_eq!(message.len(), MAX_TEXT);
        assert!(!message.contains('<'));
    }

    #[test]
    fn parses_a_record_split_across_reads() {
        let mut scanner = NotifyScanner::default();
        assert_eq!(scanner.scan(b"\x1b]9;4;1;75;").progress, None);
        let scan = scanner.scan(b"copying\x07");
        assert_eq!(scan.progress.unwrap().value, Some(75));
    }

    #[test]
    fn ignores_malformed_progress_states() {
        let mut scanner = NotifyScanner::default();
        assert_eq!(scanner.scan(&osc(b"\x1b]9;4;", "9;50;bad")).progress, None);
    }
}
