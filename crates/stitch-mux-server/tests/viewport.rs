//! Structured clients must see the same screen the daemon owns.
//!
//! These tests exist because the cell-delta stream can look healthy — sequence
//! numbers advance and panes stay alive — while the viewport silently drifts:
//!
//! * a scroll shifts rows without the parser marking them dirty, and
//! * per-client delta sequences used to restart at 1 after an attach snapshot,
//!   so a client discarded the first `snapshot.seq` deltas as stale.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, Outbound};
use stitch_mux_core::CaptureFormat;
use stitch_mux_protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, ViewMode,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!("vp{label}{}", nanos() % 1_000_000_000);
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

fn hello(instance: &str) -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: "viewport-test".into(),
        client_instance: instance.into(),
        view_mode: ViewMode::Structured,
        cols: 40,
        rows: 8,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

fn shell() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("cmd.exe", vec![])
    } else {
        ("sh", vec![])
    }
}

async fn wait_for<F: Fn(&stitch_mux_client::SessionView) -> bool>(
    label: &str,
    session: &ClientSession,
    f: F,
) {
    let mut receiver = session.subscribe();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if f(&receiver.borrow_and_update()) {
                return;
            }
            if receiver.changed().await.is_err() {
                panic!("connection closed before {label}");
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label} within timeout"));
}

fn viewport(view: &stitch_mux_client::SessionView, pane_id: u64) -> Vec<String> {
    view.pane(pane_id)
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
                .collect()
        })
        .unwrap_or_default()
}

async fn authoritative_lines(endpoint: &Endpoint, pane_id: u64) -> Vec<String> {
    let captured = stitch_mux_client::command(
        endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 200,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture response");
    assert!(captured.ok, "{:?}", captured.error);
    captured
        .capture
        .expect("capture")
        .text
        .expect("text")
        .lines()
        .map(|line| line.trim_end().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

/// The last rows of the client viewport must match the daemon's own screen,
/// including after output has scrolled the viewport.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewport_follows_scrolling_output() {
    let (endpoint, socket_path) = test_endpoint("scroll");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });

    let (program, args) = shell();
    let created = stitch_mux_client::command(
        &endpoint,
        Command::NewSession {
            name: "demo".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 40,
            rows: 8,
        },
    )
    .await
    .expect("new session");
    assert!(created.ok, "{:?}", created.error);
    let pane_id = created.sessions[0].panes[0].id;

    let session = ClientSession::attach(&endpoint, "demo", hello("scroll"))
        .await
        .expect("attach");
    wait_for("initial snapshot", &session, |view| {
        view.pane(pane_id).is_some()
    })
    .await;

    // Enough lines to scroll the eight-row viewport several times.
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    let script = if cfg!(windows) {
        format!("for /l %i in (1,1,14) do @echo ROW-%i{newline}")
    } else {
        format!("for i in $(seq 1 14); do echo ROW-$i; done{newline}")
    };
    session.send(Outbound::Input(InputEvent::Text(script)));
    wait_for("scrolled output", &session, |view| {
        viewport(view, pane_id)
            .iter()
            .any(|line| line.contains("ROW-13"))
    })
    .await;

    let rows = viewport(&session.view(), pane_id);
    let visible: Vec<String> = rows
        .iter()
        .map(|line| line.trim_end().to_owned())
        .filter(|line| !line.is_empty())
        .collect();
    let expected = authoritative_lines(&endpoint, pane_id).await;
    let tail = &expected[expected
        .len()
        .saturating_sub(visible.len().min(expected.len()))..];
    assert_eq!(
        &visible[visible.len().saturating_sub(tail.len())..],
        tail,
        "client viewport:\n{visible:#?}\nauthoritative tail:\n{tail:#?}"
    );

    session.detach().await;
    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}

/// After a reattach the client snapshots at the pane's current sequence. The
/// following deltas must continue from there instead of restarting at 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reattached_client_receives_new_output_immediately() {
    let (endpoint, socket_path) = test_endpoint("reattach");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });

    let (program, args) = shell();
    let created = stitch_mux_client::command(
        &endpoint,
        Command::NewSession {
            name: "demo".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 40,
            rows: 8,
        },
    )
    .await
    .expect("new session");
    assert!(created.ok, "{:?}", created.error);
    let pane_id = created.sessions[0].panes[0].id;
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };

    // Warm the pane so the next snapshot sequence is well above zero.
    let first = ClientSession::attach(&endpoint, "demo", hello("warm"))
        .await
        .expect("attach");
    wait_for("initial snapshot", &first, |view| {
        view.pane(pane_id).is_some()
    })
    .await;
    for index in 0..4 {
        first.send(Outbound::Input(InputEvent::Text(format!(
            "echo WARM-{index}{newline}"
        ))));
        wait_for("warm row", &first, |view| {
            viewport(view, pane_id)
                .iter()
                .any(|line| line.contains(&format!("WARM-{index}")))
        })
        .await;
    }
    first.detach().await;

    let second = ClientSession::attach(&endpoint, "demo", hello("warm"))
        .await
        .expect("reattach");
    wait_for("reattach snapshot", &second, |view| {
        view.pane(pane_id).is_some()
    })
    .await;
    let snapshot_seq = second
        .view()
        .pane(pane_id)
        .map(|pane| pane.last_seq)
        .unwrap_or_default();
    assert!(
        snapshot_seq > 0,
        "the warm-up output should have advanced the pane sequence"
    );

    second.send(Outbound::Input(InputEvent::Text(format!(
        "echo MARKER-AFTER-ATTACH{newline}"
    ))));
    wait_for("marker after reattach", &second, |view| {
        viewport(view, pane_id)
            .iter()
            .any(|line| line.contains("MARKER-AFTER-ATTACH"))
    })
    .await;

    second.detach().await;
    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}
