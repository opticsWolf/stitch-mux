//! Mux policy for OSC metadata framed by Stitch PTY.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use stitch_mux_core::{ProgramState, ProgramStatus, ProgramStatusKind};
use stitch_mux_protocol::{Notification, Progress, ProgressState, ShellMarks};
use stitch_pty::terminal::events::{OscEvent, OscPayload};

const MAX_MARKS: usize = 256;
const MAX_CWD: usize = 1024;
const MAX_TEXT: usize = 512;

#[derive(Default)]
pub(crate) struct OscMetadata {
    pub status: Option<ProgramStatus>,
    pub notification: Option<Notification>,
    pub progress: Option<Progress>,
    pub cwd: Option<String>,
    pub marks: ShellMarks,
}

impl OscMetadata {
    pub fn observe(&mut self, event: OscEvent<'_>) {
        let OscPayload::Bytes(payload) = event.payload else {
            return;
        };
        match event.code {
            7501 => {
                if let Some(status) = parse_status(payload) {
                    self.status = Some(status);
                }
            }
            9 | 777 => {
                if let Some((notification, progress)) = parse_notification(event.code, payload) {
                    if notification.is_some() {
                        self.notification = notification;
                    }
                    if progress.is_some() {
                        self.progress = progress;
                    }
                }
            }
            133 if !event.alternate_screen => {
                let row = event.pending_scrollback.saturating_add(event.row) as u64;
                match payload.split(|&byte| byte == b';').next() {
                    Some(b"A") => insert_mark(&mut self.marks.prompt_rows, row),
                    Some(b"B" | b"C" | b"D") => insert_mark(&mut self.marks.command_rows, row),
                    _ => {}
                }
            }
            1337 => {
                if let Some(cwd) = payload.strip_prefix(b"CurrentDir=")
                    && let Ok(cwd) = std::str::from_utf8(cwd)
                    && !cwd.is_empty()
                    && cwd.len() <= MAX_CWD
                {
                    self.cwd = Some(cwd.to_owned());
                }
            }
            _ => {}
        }
    }
}

/// Merge feed-relative marks, then translate all marks to the retained
/// capture coordinate space after the history cap evicts old lines.
pub(crate) fn update_marks(
    marks: &mut ShellMarks,
    added: ShellMarks,
    history_before: u64,
    evicted: u64,
    total: u64,
) {
    for (rows, incoming) in [
        (&mut marks.prompt_rows, added.prompt_rows),
        (&mut marks.command_rows, added.command_rows),
    ] {
        for relative in incoming {
            insert_mark(rows, history_before.saturating_add(relative));
        }
        rows.retain_mut(|row| {
            if let Some(retained) = row.checked_sub(evicted).filter(|&row| row < total) {
                *row = retained;
                true
            } else {
                false
            }
        });
    }
}

fn insert_mark(rows: &mut Vec<u64>, row: u64) {
    if let Err(index) = rows.binary_search(&row) {
        rows.insert(index, row);
        if rows.len() > MAX_MARKS {
            rows.remove(0);
        }
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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

fn sanitize(value: &str) -> Option<String> {
    let text: String = value
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_TEXT)
        .collect();
    (!text.is_empty()).then_some(text)
}

fn parse_notification(
    code: u16,
    payload: &[u8],
) -> Option<(Option<Notification>, Option<Progress>)> {
    let text = std::str::from_utf8(payload).ok()?;
    let now = now_unix_ms();
    if code == 9
        && let Some(text) = text.strip_prefix("4;")
    {
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
    if code == 777 {
        let text = text.strip_prefix("notify;")?;
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
    // Stitch PTY handles OSC 9;9 as a working-directory report.
    if text.starts_with("9;") {
        return None;
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

    #[test]
    fn status_policy_validates_state_and_decodes_messages() {
        let status =
            parse_status(b"state=blocked:kind=permission:id=42:app=mux:msg=SGVsbG8=:extra=x")
                .unwrap();
        assert_eq!(status.state, ProgramState::Blocked);
        assert_eq!(status.kind, Some(ProgramStatusKind::Permission));
        assert_eq!(status.message.as_deref(), Some("Hello"));
        assert!(parse_status(b"app=mux").is_none());
        assert!(parse_status(b"state=unsupported").is_none());
    }

    #[test]
    fn notification_policy_preserves_semicolons_and_ignores_cwd_reports() {
        let (notification, _) = parse_notification(777, b"notify;title;part one;part two").unwrap();
        assert_eq!(notification.unwrap().message, "part one;part two");
        let (_, progress) = parse_notification(9, b"4;2;75;error").unwrap();
        assert_eq!(progress.unwrap().value, Some(75));
        assert!(parse_notification(9, b"4;8;75").is_none());
        assert!(parse_notification(9, b"9;C:/work").is_none());
    }

    #[test]
    fn observer_retains_the_latest_metadata_and_discards_overflow() {
        let mut metadata = OscMetadata::default();
        let mut event = OscEvent {
            code: 7501,
            payload: OscPayload::Bytes(b"state=working"),
            row: 0,
            col: 0,
            alternate_screen: false,
            pending_scrollback: 0,
        };
        metadata.observe(event);
        event.payload = OscPayload::Bytes(b"state=done");
        metadata.observe(event);
        event.payload = OscPayload::Overflow;
        metadata.observe(event);
        assert_eq!(metadata.status.unwrap().state, ProgramState::Done);
    }

    #[test]
    fn marks_stay_sorted_bounded_and_unique() {
        let mut marks = ShellMarks::default();
        let rows: Vec<_> = (0..MAX_MARKS as u64 + 10).rev().collect();
        update_marks(
            &mut marks,
            ShellMarks {
                prompt_rows: rows,
                command_rows: vec![2, 2],
            },
            0,
            1,
            1000,
        );
        assert_eq!(marks.prompt_rows.len(), MAX_MARKS);
        assert_eq!(marks.prompt_rows.first(), Some(&9));
        assert_eq!(marks.command_rows, [1]);
    }
}
