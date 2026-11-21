//! Repository-only PTY child for the headless benchmark set.
//!
//! Two deterministic modes:
//!
//! * `--echo` prints a ready marker, then echoes each input line. Used for
//!   input-to-visible latency samples.
//! * `--burst <bytes> <marker>` prints a ready marker, waits for one input
//!   line, emits fixed-width numbered lines until at least `<bytes>` bytes
//!   have been written, then prints `<marker>` and exits. Used for output
//!   completion / ingest-rate samples.
//!
//! No sleeps in the burst path: the benchmark measures the mux, not the
//! fixture's pacing. The ready gate lets the harness attach before work starts.

use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;

const READY: &str = "FIXTURE-READY";
const LINE_WIDTH: usize = 72;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--echo") => echo(),
        Some("--burst") => {
            let bytes = args
                .next()
                .and_then(|value| value.parse::<usize>().ok())
                .expect("--burst needs a byte count");
            let marker = args.next().expect("--burst needs a completion marker");
            burst(bytes, &marker);
        }
        _ => {
            eprintln!("usage: bench_fixture --echo | --burst <bytes> <marker>");
            std::process::exit(2);
        }
    }
}

fn ready(stdout: &mut impl Write) {
    let _ = writeln!(stdout, "{READY}\r");
    let _ = stdout.flush();
}

fn echo() {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    ready(&mut stdout);
    for line in stdin.lock().lines() {
        let line = line.unwrap_or_default();
        let line = line.trim_end_matches(['\r', '\n']);
        if writeln!(stdout, "{line}\r").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

fn burst(bytes: usize, marker: &str) {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    ready(&mut stdout);

    // The gate only exists so the harness can attach before the burst.
    // Ignore EOF so a harness that cannot signal input still gets a sample.
    let _ = stdin.lock().lines().next();

    let index_width = 8;
    let payload_width = LINE_WIDTH.saturating_sub(index_width + 2); // index + space + CRLF
    let payload = "x".repeat(payload_width);
    let mut written = 0_usize;
    let mut index = 0_u64;
    while written < bytes {
        let line = format!("{index:08} {payload}\r\n");
        if stdout.write_all(line.as_bytes()).is_err() {
            return;
        }
        written += line.len();
        index += 1;
    }
    let _ = write!(stdout, "\r\n{marker}\r\n");
    let _ = stdout.flush();
    // Keep the slave alive briefly so the tail is not dropped by platforms
    // that discard unread bytes when the child exits immediately.
    thread::sleep(Duration::from_millis(2_000));
}
