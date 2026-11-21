//! Tiny deterministic child for automation tests: prints a marker and exits.

use std::io::Write;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_else(|| "0".into());
    if first == "--hold" {
        print!("ready\r\n");
        let _ = std::io::stdout().flush();
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
    let code: u8 = first.parse().unwrap_or(0);
    print!("ready\r\nfinal-marker\r\n");
    let _ = std::io::stdout().flush();
    ExitCode::from(code)
}
