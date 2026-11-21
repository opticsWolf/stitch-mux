//! Deterministic styled output for capture-format tests.

use std::io::Write;

fn main() {
    let flood = std::env::args().any(|arg| arg == "--flood");
    if flood {
        for line in 0..5000_u32 {
            println!("line-{line:04}");
        }
    } else {
        print!("\x1b[1;31m<red>&\x1b[0m plain\r\n");
    }
    let _ = std::io::stdout().flush();
}
