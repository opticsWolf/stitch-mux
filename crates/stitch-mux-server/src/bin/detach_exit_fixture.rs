//! Child that exits after a short hold, used to test detached-pane exit.

use std::io::Write;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

fn main() -> ExitCode {
    print!("ready\r\n");
    let _ = std::io::stdout().flush();
    thread::sleep(Duration::from_secs(3));
    print!("done\r\n");
    let _ = std::io::stdout().flush();
    ExitCode::from(7)
}
