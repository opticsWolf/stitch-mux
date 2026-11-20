//! Mux-side OSC 8 hyperlink scanner.
//!
//! `stitch-pty` renders text and style into `HistoryScreen` but does not carry
//! link metadata. This scanner observes the same PTY bytes, tracks printable
//! cell positions, and records the active OSC 8 URI for those cells. It never
//! rewrites child output and never parses the screen.

use unicode_width::UnicodeWidthChar;

const MAX_URI: usize = 2048;
const MAX_SPANS: usize = 4096;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LinkScan {
    /// Absolute cell indices and the URI active when they were printed.
    pub spans: Vec<(u64, String)>,
    /// True when a link started/stopped or a cell gained a link. A full
    /// snapshot is published in that frame so clients see links immediately.
    pub changed: bool,
}

#[derive(Default)]
pub struct LinkScanner {
    pending: Vec<u8>,
    utf8: Vec<u8>,
    current: Option<String>,
    escape: bool,
    in_osc: bool,
    in_csi: bool,
    /// Cursor position before the current read, tracked locally in absolute
    /// screen coordinates. Unknown control sequences invalidate tracking.
    row: u64,
    col: usize,
    cols: usize,
    invalid: bool,
}

impl LinkScanner {
    /// Scan one PTY read. `row`/`col` are the absolute cursor position before
    /// the bytes were fed to the screen.
    pub fn scan(&mut self, bytes: &[u8], row: u64, col: usize, cols: usize) -> LinkScan {
        self.row = row;
        self.col = col.min(cols.saturating_sub(1));
        self.cols = cols.max(1);
        self.invalid = false;
        let mut out = LinkScan::default();

        for &byte in bytes {
            if self.in_osc {
                self.consume_osc(byte, &mut out);
                continue;
            }
            if self.in_csi {
                self.consume_csi(byte);
                continue;
            }
            if self.escape {
                self.consume_escape(byte);
                continue;
            }
            match byte {
                0x1b => self.escape = true,
                b'\n' => {
                    self.row = self.row.saturating_add(1);
                    self.col = 0;
                }
                b'\r' => self.col = 0,
                0x08 => self.col = self.col.saturating_sub(1),
                b'\t' => self.advance(8 - (self.col % 8)),
                0x00..=0x1f | 0x7f => self.invalid = true,
                0x20..=0x7e => self.print_char(byte as char, &mut out),
                _ => self.consume_utf8(byte, &mut out),
            }
        }
        out
    }

    fn consume_escape(&mut self, byte: u8) {
        self.escape = false;
        match byte {
            b']' => {
                self.in_osc = true;
                self.pending.clear();
            }
            b'[' => {
                self.in_csi = true;
                self.pending.clear();
            }
            // OSC/CSI terminators and simple 2-byte escapes do not move the
            // cursor in ways this tracker needs to model.
            b'\\' | b'7' | b'8' | b'D' | b'M' | b'c' => {}
            _ => self.invalid = true,
        }
    }

    fn consume_csi(&mut self, byte: u8) {
        // CSI final bytes are 0x40..=0x7e. Cursor movement, erase, and insert
        // would make our local position tracker wrong, so stop assigning links
        // until the next read resets it from the authoritative screen cursor.
        if (0x40..=0x7e).contains(&byte) {
            self.in_csi = false;
            self.invalid = true;
        }
    }

    fn consume_osc(&mut self, byte: u8, out: &mut LinkScan) {
        if byte == 0x07 {
            self.finish_osc(out);
            return;
        }
        if byte == 0x1b {
            // The next byte may be `\` for ST; handle it without adding it to
            // the payload.
            self.pending.push(0x1b);
            if self.pending.len() > MAX_URI + 2 {
                self.pending.clear();
                self.in_osc = false;
            }
            return;
        }
        if self.pending.ends_with(&[0x1b]) {
            self.pending.pop();
            if byte == b'\\' {
                self.finish_osc(out);
                return;
            }
            // An embedded escape is not a valid OSC 8 URI.
            self.invalid = true;
        }
        if self.pending.len() < MAX_URI + 2 {
            self.pending.push(byte);
        } else {
            self.pending.clear();
            self.in_osc = false;
            self.invalid = true;
        }
    }

    fn finish_osc(&mut self, out: &mut LinkScan) {
        let payload = std::mem::take(&mut self.pending);
        self.in_osc = false;
        let Some(uri) = parse_osc8(&payload) else {
            return;
        };
        out.changed = true;
        self.current = uri;
    }

    fn consume_utf8(&mut self, byte: u8, out: &mut LinkScan) {
        self.utf8.push(byte);
        match std::str::from_utf8(&self.utf8) {
            Ok(text) => {
                let character = text.chars().next().expect("non-empty UTF-8");
                let len = character.len_utf8();
                self.utf8.drain(..len);
                self.print_char(character, out);
            }
            Err(_) if self.utf8.len() < 4 => {}
            Err(_) => {
                self.utf8.clear();
                self.invalid = true;
            }
        }
    }

    fn print_char(&mut self, character: char, out: &mut LinkScan) {
        if let Some(uri) = self.current.as_deref()
            && !self.invalid
            && out.spans.len() < MAX_SPANS
        {
            out.spans.push((self.cell_index(), uri.to_owned()));
            out.changed = true;
        }
        let width = character.width().unwrap_or(0).max(1);
        self.advance(width);
    }

    fn cell_index(&self) -> u64 {
        self.row
            .saturating_mul(self.cols as u64)
            .saturating_add(self.col as u64)
    }

    fn advance(&mut self, cells: usize) {
        self.col = self.col.saturating_add(cells);
        if self.col >= self.cols {
            let rows = self.col / self.cols;
            self.row = self.row.saturating_add(rows as u64);
            self.col %= self.cols;
        }
    }
}

fn parse_osc8(payload: &[u8]) -> Option<Option<String>> {
    let text = std::str::from_utf8(payload).ok()?;
    let rest = text.strip_prefix("8;")?;
    let (_, uri) = rest.split_once(';')?;
    if uri.is_empty() {
        return Some(None);
    }
    if uri.len() > MAX_URI || uri.chars().any(char::is_control) {
        return None;
    }
    Some(Some(uri.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(uri: &str) -> Vec<u8> {
        let mut bytes = b"\x1b]8;;".to_vec();
        bytes.extend_from_slice(uri.as_bytes());
        bytes.extend_from_slice(b"\x07");
        bytes
    }

    #[test]
    fn records_cells_while_the_link_is_active() {
        let mut scanner = LinkScanner::default();
        let mut bytes = osc("https://example.com");
        bytes.extend_from_slice(b"link");
        bytes.extend_from_slice(b"\x1b]8;;\x07");
        bytes.extend_from_slice(b" plain");
        let scan = scanner.scan(&bytes, 10, 2, 80);
        assert_eq!(
            scan.spans,
            vec![
                (802, "https://example.com".into()),
                (803, "https://example.com".into()),
                (804, "https://example.com".into()),
                (805, "https://example.com".into()),
            ]
        );
        assert!(scan.changed);
    }

    #[test]
    fn a_split_uri_is_joined_across_reads() {
        let mut scanner = LinkScanner::default();
        let mut first = b"\x1b]8;;https://example.".to_vec();
        assert!(!scanner.scan(&first, 0, 0, 80).changed);
        first.clear();
        first.extend_from_slice(b"com\x07xx");
        let scan = scanner.scan(&first, 0, 0, 80);
        assert_eq!(
            scan.spans,
            vec![
                (0, "https://example.com".into()),
                (1, "https://example.com".into())
            ]
        );
    }

    #[test]
    fn unknown_cursor_movement_stops_link_assignment() {
        let mut scanner = LinkScanner::default();
        // The CSI is consumed after a link starts; the following characters
        // are not attributed because their real screen position is unknown.
        let mut bytes = osc("https://example.com");
        bytes.extend_from_slice(b"\x1b[2Jxx");
        let scan = scanner.scan(&bytes, 0, 0, 80);
        assert!(scan.spans.is_empty());
        assert!(scan.changed); // the link itself changed
    }

    #[test]
    fn tracks_a_link_after_unrelated_shell_osc_records() {
        let mut scanner = LinkScanner::default();
        let mut bytes = b"]1337;CurrentDir=/work]133;Aprompt$ ]133;Becho hi

"
        .to_vec();
        bytes.extend(
            b"hi

]133;D;0",
        );
        bytes.extend(osc("https://example.com/docs"));
        bytes.extend_from_slice(b"docs");
        bytes.extend(b"]8;;");
        let scan = scanner.scan(&bytes, 0, 0, 80);
        assert!(
            scan.spans
                .iter()
                .all(|(_, uri)| uri == "https://example.com/docs"),
            "unexpected spans: {:?}",
            scan.spans
        );
        assert_eq!(scan.spans.len(), 4, "unexpected spans: {:?}", scan.spans);
    }

    #[test]
    fn an_unterminated_uri_cannot_grow_without_bound() {
        let mut scanner = LinkScanner::default();
        let mut bytes = b"\x1b]8;;".to_vec();
        bytes.extend(vec![b'x'; MAX_URI * 2]);
        let scan = scanner.scan(&bytes, 0, 0, 80);
        assert!(scan.spans.is_empty());
    }
}
