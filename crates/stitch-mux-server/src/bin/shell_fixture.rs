//! Real-PTY child that emits shell-integration OSC 133/1337 records.

use std::io::Write;
use std::time::Duration;

fn osc(payload: &str) {
    print!("\x1b]{payload}\x07");
    let _ = std::io::stdout().flush();
}

fn main() {
    // Give the test time to attach before the first mark.
    std::thread::sleep(Duration::from_secs(3));
    osc("1337;CurrentDir=/work");
    osc("133;A");
    print!("prompt$ ");
    osc("133;B");
    print!("echo hi\r\n");
    osc("133;C");
    print!("hi\r\n");
    osc("133;D;0");
    // OSC 8 hyperlink metadata; the text itself is ordinary screen output.
    osc("8;;https://example.com/docs");
    print!("docs");
    osc("8;;");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
