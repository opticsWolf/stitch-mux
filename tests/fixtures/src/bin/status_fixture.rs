//! Real-PTY child that emits a sequence of OSC 7501 status records.

use std::io::Write;
use std::time::Duration;

fn emit(state: &str, message_b64: &str) {
    print!("\x1b]7501;state={state}:kind=question:app=status-fixture:msg={message_b64}\x1b\\");
    let _ = std::io::stdout().flush();
}

fn main() {
    // Give the test time to attach before the first record.
    std::thread::sleep(Duration::from_secs(3));
    emit("working", "V29ya2luZw==");
    std::thread::sleep(Duration::from_secs(1));
    emit("blocked", "V2FpdGluZw==");
    std::thread::sleep(Duration::from_secs(1));
    emit("done", "RmluaXNoZWQ=");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
