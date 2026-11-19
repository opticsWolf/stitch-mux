//! Test fixture: emit a paced 10 MiB burst through a PTY.
//!
//! `crates/stitch-mux-server/tests/stress.rs` spawns this binary directly.
//! Pacing lets healthy clients drain coalesced updates while still filling a
//! deliberately stalled client's bounded queue.

use std::io::{self, Write};
use std::time::Duration;

const CHUNK: usize = 64 * 1024;
const CHUNKS: usize = 10 * 1024 * 1024 / CHUNK;

fn main() {
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"FIXTURE-READY\n").expect("ready");
    stdout.flush().expect("ready flush");
    // Give the test time to attach all clients before the flood starts.
    std::thread::sleep(Duration::from_secs(2));

    let chunk = vec![b'x'; CHUNK];
    for _ in 0..CHUNKS {
        stdout.write_all(&chunk).expect("flood write");
        stdout.flush().expect("chunk flush");
        std::thread::sleep(Duration::from_millis(20));
    }
    stdout.write_all(b"\nSTRESS-DONE\n").expect("marker");
    stdout.flush().expect("marker flush");
}
