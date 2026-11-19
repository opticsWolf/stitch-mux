#![cfg(unix)]

//! End-to-end proof that the daemon answers terminal DA/DSR queries.
//!
//! Unix-only: the child puts its own PTY into raw mode so the reply is
//! delivered without waiting for a newline. Windows coverage comes from the
//! scanner unit tests; the PTY does not expose a portable raw-mode helper.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, command};
use stitch_mux_protocol::{Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, ViewMode};

fn test_endpoint() -> (Endpoint, Option<PathBuf>) {
    let unique = format!("m3q{}", nanos() % 1_000_000_000);
    if cfg!(windows) {
        (Endpoint::from_override(&unique), None)
    } else {
        let path = std::env::temp_dir().join(format!("{unique}.sock"));
        (
            Endpoint::from_override(path.to_string_lossy().as_ref()),
            Some(path),
        )
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

/// Accept any valid CPR reply. The row reported by a fresh PTY can differ
/// between CI runners; the contract is that the daemon answers the query.
fn contains_cpr_reply(hex: &str) -> bool {
    let bytes = hex.as_bytes();
    for start in 0..bytes.len().saturating_sub(8) {
        if !hex[start..].starts_with("1b5b") {
            continue;
        }
        let rest = &hex[start + 4..];
        let Some(semicolon) = rest.find("3b") else {
            continue;
        };
        let row = &rest[..semicolon];
        let after = &rest[semicolon + 2..];
        let Some(terminator) = after.rfind("52") else {
            continue;
        };
        let column = &after[..terminator];
        if !row.is_empty()
            && !column.is_empty()
            && row.bytes().all(|byte| byte.is_ascii_hexdigit())
            && column.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return true;
        }
    }
    false
}

fn hello() -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: "query".into(),
        client_instance: format!("query-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

#[test]
fn recognizes_a_cursor_position_report() {
    assert!(contains_cpr_reply("prefix1b5b313b3152suffix"));
    assert!(contains_cpr_reply("1b5b323b313552"));
    assert!(!contains_cpr_reply("1b5b6e"));
}

#[tokio::test]
async fn cursor_position_query_reaches_the_child() {
    let (endpoint, socket) = test_endpoint();
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "queries".into(),
            program: "sh".into(),
            // Raw mode delivers the reply immediately instead of waiting for
            // a newline; the child prints the six reply bytes as hex.
            args: vec![
                "-c".into(),
                "stty raw -echo; printf '\\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d '[:space:]'; stty sane".into(),
            ],
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("new session");
    assert!(created.ok, "{:?}", created.error);
    let pane_id = created.sessions[0].panes[0].id;

    let client = ClientSession::attach(&endpoint, "queries", hello())
        .await
        .expect("attach");
    let mut screen = String::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            screen = client
                .view()
                .pane(pane_id)
                .and_then(|pane| pane.snapshot.as_ref())
                .map(|snapshot| {
                    snapshot
                        .cells
                        .iter()
                        .map(|row| {
                            row.cells
                                .iter()
                                .map(|cell| cell.text.as_str())
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            // ESC [ row ; col R as hex, proving the daemon replied.
            if contains_cpr_reply(&screen) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("cursor-position reply; screen={screen:?}"));

    client.detach().await;
    server.abort();
    if let Some(path) = socket {
        let _ = std::fs::remove_file(path);
    }
}
