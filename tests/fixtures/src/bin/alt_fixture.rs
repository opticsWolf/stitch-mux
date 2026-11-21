//! Real-PTY child that enters and exits the alternate screen.

use std::io::Write;
use std::time::Duration;

fn emit(text: &str) {
    print!("{text}");
    let _ = std::io::stdout().flush();
}

fn main() {
    // Give the test time to attach before the first transition.
    std::thread::sleep(Duration::from_secs(3));
    emit("shell-before\r\n");
    emit("\x1b[?1049h");
    emit("alt-view\r\n");
    // Long enough for a polling client to observe the authoritative flag.
    std::thread::sleep(Duration::from_secs(2));
    emit("\x1b[?1049l");
    emit("shell-after\r\n");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
