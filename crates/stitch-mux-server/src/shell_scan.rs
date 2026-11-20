//! Mux-side shell-integration metadata scanner.
//!
//! `stitch-pty` owns the terminal screen, and the daemon owns the structured
//! projection. This scanner observes the same PTY bytes to learn shell marks
//! (OSC 133), the current directory (OSC 1337), and nothing else. It never
//! modifies the byte stream and never becomes an event queue: only the latest
//! bounded mark set and cwd are retained.

use stitch_mux_protocol::ShellMarks;

/// A single OSC record is metadata. Refuse to buffer an unbounded OSC if a
/// child emits an introducer and never terminates it.
const MAX_RECORD: usize = 8 * 1024;
/// Prompt/command marks are navigation aids, not a log. Keep a bounded tail.
const MAX_MARKS: usize = 256;
const MAX_CWD: usize = 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShellScan {
    pub cwd: Option<String>,
    pub marks: ShellMarks,
}

#[derive(Default)]
pub struct ShellScanner {
    pending: Vec<u8>,
    escape: bool,
    in_osc: bool,
    /// Absolute screen row where the pending OSC started.
    pending_row: u64,
    marks: ShellMarks,
    cwd: Option<String>,
}

impl ShellScanner {
    /// Scan a fresh PTY read.
    ///
    /// `origin_row` is the absolute row the cursor occupied before the bytes
    /// were fed to the screen. Newlines inside the read advance rows; OSC
    /// payloads do not move the cursor.
    pub fn marks(&self) -> &ShellMarks {
        &self.marks
    }

    pub fn scan(&mut self, bytes: &[u8], origin_row: u64) -> ShellScan {
        let mut line_offset = 0_u64;
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if self.in_osc {
                index += self.consume_osc_byte(byte);
                continue;
            }
            if self.escape {
                self.escape = false;
                if byte == b']' {
                    self.in_osc = true;
                    self.pending.clear();
                    self.pending_row = origin_row.saturating_add(line_offset);
                } else if byte == b'\n' {
                    line_offset = line_offset.saturating_add(1);
                }
                // Other escape sequences are left to the terminal screen.
                index += 1;
                continue;
            }
            match byte {
                0x1b => self.escape = true,
                b'\n' => line_offset = line_offset.saturating_add(1),
                _ => {}
            }
            index += 1;
        }
        ShellScan {
            cwd: self.cwd.clone(),
            marks: self.marks.clone(),
        }
    }

    /// Consume one byte of an OSC payload. Returns how many input bytes were
    /// consumed so an `ESC \` terminator can swallow both bytes.
    fn consume_osc_byte(&mut self, byte: u8) -> usize {
        if byte == 0x07 {
            self.finish();
            return 1;
        }
        if self.escape {
            self.escape = false;
            if byte == b'\\' {
                self.finish();
                return 1;
            }
        }
        if byte == 0x1b {
            self.escape = true;
            return 1;
        }
        if self.pending.len() < MAX_RECORD {
            self.pending.push(byte);
        } else {
            // A malformed, never-terminated record must not retain a huge
            // buffer or emit a mark from a truncated payload.
            self.pending.clear();
            self.in_osc = false;
            self.escape = false;
        }
        1
    }

    fn finish(&mut self) {
        let payload = std::mem::take(&mut self.pending);
        self.in_osc = false;
        self.escape = false;
        let Some(record) = parse(&payload) else {
            return;
        };
        match record {
            Record::Prompt => push_mark(&mut self.marks.prompt_rows, self.pending_row),
            Record::Command => push_mark(&mut self.marks.command_rows, self.pending_row),
            Record::Cwd(path) => self.cwd = Some(path),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Record {
    Prompt,
    Command,
    Cwd(String),
}

fn parse(payload: &[u8]) -> Option<Record> {
    let text = std::str::from_utf8(payload).ok()?;
    let (command, rest) = text.split_once(';')?;
    match command {
        "133" => match rest.split(';').next()? {
            "A" => Some(Record::Prompt),
            "B" | "C" | "D" => Some(Record::Command),
            _ => None,
        },
        "1337" => {
            let value = rest.strip_prefix("CurrentDir=")?;
            let path = value.trim_end_matches(['\0', '\r']);
            if path.is_empty() || path.len() > MAX_CWD {
                return None;
            }
            Some(Record::Cwd(path.to_owned()))
        }
        _ => None,
    }
}

fn push_mark(marks: &mut Vec<u64>, row: u64) {
    if marks.last() == Some(&row) {
        return;
    }
    if marks.len() == MAX_MARKS {
        marks.remove(0);
    }
    marks.push(row);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(payload: &str) -> Vec<u8> {
        let mut bytes = b"\x1b]".to_vec();
        bytes.extend_from_slice(payload.as_bytes());
        bytes.push(0x07);
        bytes
    }

    #[test]
    fn records_prompt_and_command_rows() {
        let mut scanner = ShellScanner::default();
        let mut bytes = osc("133;A");
        bytes.extend_from_slice(b"prompt$ ");
        bytes.extend(osc("133;B"));
        bytes.extend_from_slice(b"echo hi\n");
        bytes.extend(osc("133;C"));
        bytes.extend_from_slice(b"hi\n");
        bytes.extend(osc("133;D;0"));
        let scan = scanner.scan(&bytes, 10);
        assert_eq!(scan.marks.prompt_rows, vec![10]);
        assert_eq!(scan.marks.command_rows, vec![10, 11, 12]);
    }

    #[test]
    fn accepts_a_split_osc_without_moving_the_row() {
        let mut scanner = ShellScanner::default();
        let mut bytes = b"\x1b]1337;CurrentDir=/tmp/".to_vec();
        assert_eq!(scanner.scan(&bytes, 3).cwd, None);
        bytes.clear();
        bytes.extend_from_slice(b"work\x1b\\");
        assert_eq!(scanner.scan(&bytes, 99).cwd.as_deref(), Some("/tmp/work"));
    }

    #[test]
    fn keeps_only_a_bounded_tail_of_marks() {
        let mut scanner = ShellScanner::default();
        for row in 0..(MAX_MARKS + 10) {
            let _ = scanner.scan(&osc("133;A"), row as u64);
        }
        let scan = scanner.scan(&[], 0);
        assert_eq!(scan.marks.prompt_rows.len(), MAX_MARKS);
        assert_eq!(scan.marks.prompt_rows.first(), Some(&10));
    }

    #[test]
    fn an_unterminated_record_cannot_grow_without_bound() {
        let mut scanner = ShellScanner::default();
        let mut bytes = b"\x1b]1337;CurrentDir=".to_vec();
        bytes.extend(vec![b'x'; MAX_RECORD * 2]);
        assert_eq!(scanner.scan(&bytes, 0).cwd, None);
        assert!(scanner.pending.len() < MAX_RECORD);
    }

    #[test]
    fn malformed_records_are_ignored() {
        let mut scanner = ShellScanner::default();
        let scan = scanner.scan(&osc("133;Z"), 0);
        assert!(scan.marks.prompt_rows.is_empty());
        assert!(scan.marks.command_rows.is_empty());
    }
}
