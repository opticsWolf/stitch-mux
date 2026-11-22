//! Harness-owned outer PTY for the attach-client comparison lane.
//!
//! `portable-pty` supplies the transport only. The small `LineOracle` strips
//! ANSI using Alacritty's `vte` parser and keeps a bounded line/tail buffer so
//! a sentinel marker is accepted only when it was actually written to the
//! outer PTY by the attach client.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, PtySize, SlavePty, native_pty_system};
use vte::{Params, Parser, Perform};

const COLS: usize = 80;
const ROWS: usize = 24;

const DSR_REQUEST: &[u8] = b"\x1b[6n";
const DSR_RESPONSE: &[u8] = b"\x1b[1;1R";
const DSR_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Stateful DSR filter for the outer PTY.
///
/// ConPTY/crossterm clients probe the host with `ESC[6n`. A real terminal
/// answers, otherwise the attach client blocks before writing frames. This
/// buffer handles requests split across PTY reads without corrupting the
/// marker oracle.
#[derive(Debug)]
struct DsrBootstrap {
    deadline: Instant,
    pending: Vec<u8>,
}

impl DsrBootstrap {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + DSR_BOOTSTRAP_TIMEOUT,
            pending: Vec::new(),
        }
    }

    fn filter(&mut self, bytes: &[u8]) -> (Vec<u8>, usize) {
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(bytes);
        let mut responses = 0;

        loop {
            let Some(offset) = find_subslice(&combined, DSR_REQUEST) else {
                break;
            };
            let mut next = Vec::with_capacity(combined.len() - DSR_REQUEST.len());
            next.extend_from_slice(&combined[..offset]);
            next.extend_from_slice(&combined[offset + DSR_REQUEST.len()..]);
            combined = next;
            responses += 1;
        }

        if Instant::now() < self.deadline {
            let prefix = partial_prefix_len(&combined, DSR_REQUEST);
            let keep = combined.len() - prefix;
            self.pending.extend_from_slice(&combined[keep..]);
            combined.truncate(keep);
        }

        (combined, responses)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn partial_prefix_len(bytes: &[u8], needle: &[u8]) -> usize {
    (1..needle.len())
        .rev()
        .find(|len| bytes.ends_with(&needle[..*len]))
        .unwrap_or(0)
}

struct Sink {
    all: String,
    cells: Vec<Vec<char>>,
    x: usize,
    y: usize,
}

impl Default for Sink {
    fn default() -> Self {
        Self {
            all: String::new(),
            cells: vec![vec![' '; COLS]; ROWS],
            x: 0,
            y: 0,
        }
    }
}

impl Sink {
    fn put(&mut self, c: char) {
        self.all.push(c);
        if self.y < ROWS && self.x < COLS {
            self.cells[self.y][self.x] = c;
        }
        self.x += 1;
        if self.x >= COLS {
            self.x = 0;
            self.y = (self.y + 1).min(ROWS - 1);
        }
    }

    fn clear_screen(&mut self) {
        for row in &mut self.cells {
            row.fill(' ');
        }
        self.x = 0;
        self.y = 0;
    }

    fn clear_line(&mut self) {
        if self.y < ROWS {
            self.cells[self.y].fill(' ');
        }
    }

    fn text(&self) -> String {
        self.cells
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Perform for Sink {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.x = 0,
            b'\n' => self.y = (self.y + 1).min(ROWS - 1),
            b'\x08' => self.x = self.x.saturating_sub(1),
            b'\x07' => {}
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let value = |index: usize, default: usize| -> usize {
            params
                .iter()
                .nth(index)
                .and_then(|slice| slice.first().copied())
                .map(|n| n as usize)
                .unwrap_or(default)
                .max(1)
        };
        match action {
            'H' | 'f' => {
                self.y = value(0, 1).saturating_sub(1).min(ROWS - 1);
                self.x = value(1, 1).saturating_sub(1).min(COLS - 1);
            }
            'A' => self.y = self.y.saturating_sub(value(0, 1)),
            'B' => self.y = self.y.saturating_add(value(0, 1)).min(ROWS - 1),
            'C' => self.x = self.x.saturating_add(value(0, 1)).min(COLS - 1),
            'D' => self.x = self.x.saturating_sub(value(0, 1)),
            'G' => self.x = value(0, 1).saturating_sub(1).min(COLS - 1),
            'J' => self.clear_screen(),
            'K' => self.clear_line(),
            _ => {}
        }
    }
}

pub struct LineOracle {
    parser: Parser,
    sink: Sink,
}

impl LineOracle {
    fn new() -> Self {
        Self {
            parser: Parser::new(),
            sink: Sink::default(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.sink, bytes);
    }

    fn contains(&self, marker: &str) -> bool {
        let text = self.sink.text();
        if text.contains(marker) {
            return true;
        }
        // With a PTY, typed input is echoed by the line discipline and again
        // by the child fixture. The composed frame can therefore show every
        // marker character twice; accept that shape too.
        let doubled: String = marker.chars().flat_map(|c| [c, c]).collect();
        text.contains(&doubled)
    }

    fn debug_text(&self) -> &str {
        &self.sink.all
    }

    fn debug_screen(&self) -> String {
        self.sink.text()
    }
}

pub struct OuterSession {
    writer: Box<dyn Write + Send>,
    reader: Receiver<Vec<u8>>,
    oracle: LineOracle,
    dsr: DsrBootstrap,
    raw: Vec<u8>,
    child: Box<dyn Child + Send + Sync>,
    _slave: Box<dyn SlavePty + Send>,
}

impl OuterSession {
    pub fn spawn(program: &str, args: &[&str], cols: u16, rows: u16) -> Self {
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open outer PTY");
        let mut builder = CommandBuilder::new(program);
        builder.env("TERM", "xterm-256color");
        for arg in args {
            builder.arg(*arg);
        }
        let writer = pair.master.take_writer().expect("outer PTY writer");
        let mut outer_reader = pair.master.try_clone_reader().expect("outer PTY reader");
        let slave = pair.slave;
        let child = slave
            .spawn_command(builder)
            .expect("spawn attach client in outer PTY");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0_u8; 8 * 1024];
            loop {
                match outer_reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            writer,
            reader: rx,
            oracle: LineOracle::new(),
            dsr: DsrBootstrap::new(),
            raw: Vec::new(),
            child,
            _slave: slave,
        }
    }

    pub fn send(&mut self, text: &str) {
        if self.writer.write_all(text.as_bytes()).is_err() {
            return;
        }
        let _ = self.writer.flush();
    }

    pub fn debug_text(&self) -> &str {
        self.oracle.debug_text()
    }

    pub fn debug_compact(&self) -> String {
        self.oracle.debug_screen()
    }

    pub fn debug_raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn wait_for(&mut self, marker: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain();
            if self.oracle.contains(marker) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let slice = remaining.min(Duration::from_millis(5));
            match self.reader.recv_timeout(slice) {
                Ok(bytes) => {
                    self.absorb(&bytes);
                    if self.oracle.contains(marker) {
                        return true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.drain();
                    return self.oracle.contains(marker);
                }
            }
        }
    }

    fn absorb(&mut self, bytes: &[u8]) {
        let (visible, responses) = self.dsr.filter(bytes);
        for _ in 0..responses {
            let _ = self.writer.write_all(DSR_RESPONSE);
            let _ = self.writer.flush();
        }
        self.raw.extend_from_slice(&visible);
        self.oracle.feed(&visible);
    }

    fn drain(&mut self) {
        while let Ok(bytes) = self.reader.try_recv() {
            self.absorb(&bytes);
        }
    }
}

impl Drop for OuterSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::{DSR_REQUEST, DSR_RESPONSE, DsrBootstrap, partial_prefix_len};

    #[test]
    fn dsr_prefix_split_across_reads_is_answered_once() {
        let mut filter = DsrBootstrap::new();
        let (first, responses) = filter.filter(b"abc\x1b[");
        assert_eq!(responses, 0);
        assert_eq!(first, b"abc");

        let (second, responses) = filter.filter(b"6nrest");
        assert_eq!(responses, 1);
        assert_eq!(second, b"rest");
        assert_eq!(DSR_REQUEST.len(), 4);
        assert_eq!(DSR_RESPONSE, b"\x1b[1;1R");
    }

    #[test]
    fn partial_prefix_helper_tracks_longest_suffix() {
        assert_eq!(partial_prefix_len(b"abc\x1b", DSR_REQUEST), 1);
        assert_eq!(partial_prefix_len(b"abc\x1b[6", DSR_REQUEST), 3);
        assert_eq!(partial_prefix_len(b"abc", DSR_REQUEST), 0);
    }
}
