//! Capture formats, HTML escaping, and the bounded scrollback budget.

mod common;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stitch_mux::client::command;
use stitch_mux::core::CaptureFormat;
use stitch_mux::protocol::{Command, Endpoint, WaitTarget};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "m10cap{label}{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
            % 1_000_000_000
    );
    if cfg!(windows) {
        (Endpoint::from_override(&unique), None)
    } else {
        let path = std::env::temp_dir().join(&unique).join("default.sock");
        (
            Endpoint::from_override(path.to_string_lossy().as_ref()),
            Some(path),
        )
    }
}

async fn new_session(endpoint: &Endpoint, name: &str, args: &[&str]) -> u64 {
    let response = command(
        endpoint,
        Command::NewSession {
            name: name.into(),
            program: common::fixture_bin("capture_fixture"),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("new session");
    assert!(response.ok, "{:?}", response.error);
    response.sessions[0].panes[0].id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_vt_html_and_cells_capture_the_same_rows() {
    let (endpoint, socket) = test_endpoint("formats");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux::server::serve(listener));
    let pane_id = new_session(&endpoint, "formats", &[]).await;

    let waited = command(
        &endpoint,
        Command::Wait {
            target: WaitTarget::Pane { pane_id },
            timeout_ms: Some(10_000),
        },
    )
    .await
    .expect("wait");
    assert!(waited.ok, "{:?}", waited.error);
    // ConPTY can report exit before the final bytes are observable in capture.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let capture = |format| {
        let endpoint = endpoint.clone();
        async move {
            command(
                &endpoint,
                Command::Capture {
                    pane_id,
                    start: 0,
                    lines: 100,
                    format,
                },
            )
            .await
            .expect("capture")
            .capture
            .expect("capture result")
        }
    };

    let text = capture(CaptureFormat::Text).await;
    assert!(text.text.as_deref().unwrap_or_default().contains("<red>&"));

    let vt = capture(CaptureFormat::Vt).await;
    let vt_text = vt.text.as_deref().unwrap_or_default();
    assert!(
        vt_text.contains("\x1b["),
        "no SGR in VT capture: {vt_text:?}"
    );
    assert!(vt_text.contains("<red>&"));

    let html = capture(CaptureFormat::Html).await;
    let html_text = html.html.as_deref().unwrap_or_default();
    assert!(html_text.contains("&lt;"), "unexpected HTML");
    assert!(html_text.contains("&amp;"));
    assert!(!html_text.contains("<red>"));
    assert!(html_text.contains("stitch-mux-capture"));

    let cells = capture(CaptureFormat::Cells).await;
    assert!(cells.text.is_none() && cells.html.is_none());
    assert!(!cells.rows.is_empty());
    let cell_text: String = cells.rows[0]
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect();
    assert!(cell_text.contains("<red>"));

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrollback_and_capture_ranges_stay_bounded() {
    let (endpoint, socket) = test_endpoint("flood");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux::server::serve(listener));
    let pane_id = new_session(&endpoint, "flood", &["--flood"]).await;

    tokio::time::sleep(Duration::from_millis(1200)).await;
    let response = command(
        &endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 100_000,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture");
    assert!(response.ok, "{:?}", response.error);
    let capture = response.capture.expect("capture result");
    assert!(capture.rows.len() <= 1000, "{} rows", capture.rows.len());
    assert!(
        capture.total_lines <= 2000 + 24,
        "{} total rows",
        capture.total_lines
    );

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}
