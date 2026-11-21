//! Shared log location for the daemon and frontends.
//!
//! Logging is local-only: a file under the user's state directory, or an
//! explicit `STITCH_MUX_LOG` override. No network sink and no global telemetry.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_subscriber::prelude::*;

/// Path used by every stitch-mux process unless `STITCH_MUX_LOG` overrides it.
pub fn log_path() -> PathBuf {
    if let Ok(value) = std::env::var("STITCH_MUX_LOG")
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            #[cfg(windows)]
            {
                std::env::var("LOCALAPPDATA").map(PathBuf::from).ok()
            }
            #[cfg(not(windows))]
            {
                std::env::var("HOME")
                    .map(|home| PathBuf::from(home).join(".local").join("state"))
                    .ok()
            }
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("stitch-mux").join("stitch-mux.log")
}

/// Install a file subscriber once per process and return the log path.
///
/// Calling this from both the CLI and a GUI library is harmless: the first
/// caller installs the subscriber, later callers only read the path.
pub fn init_logging() -> io::Result<PathBuf> {
    static INIT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        let path = log_path();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let file: File = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| err.to_string())?;
        // Only our own crates by default; GPU/UI libraries are noisy and their
        // warnings would bury detach/reattach diagnostics.
        let filter = tracing_subscriber::filter::Targets::new()
            .with_target("stitch_mux", tracing::Level::INFO)
            .with_target("stitch_mux_server", tracing::Level::INFO)
            .with_target("stitch_mux_client", tracing::Level::INFO)
            .with_target("stitch_mux_gui_egui", tracing::Level::INFO)
            .with_default(tracing::Level::ERROR);
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(move || file.try_clone().expect("clone log file"))
            .with_ansi(false)
            .with_target(false)
            .with_filter(filter);
        let _ = tracing_subscriber::registry().with(layer).try_init();
        Ok(path)
    });
    result.clone().map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_log_path_is_under_a_state_directory() {
        let path = log_path();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("stitch-mux.log")
        );
        assert!(
            path.parent()
                .is_some_and(|parent| !parent.as_os_str().is_empty())
        );
    }
}
