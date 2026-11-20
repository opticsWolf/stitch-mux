//! Server-side primitives for the M8 local automation CLI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use stitch_mux_client::command;
use stitch_mux_core::CaptureFormat;
use stitch_mux_protocol::{Command, Endpoint, RunPlacement, WaitTarget};

static ENDPOINT_SEQ: AtomicU64 = AtomicU64::new(0);

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    // Three tests in this file start daemons in parallel. A clock-only suffix
    // can collide on coarse-grained platform clocks, so include the process
    // and a monotonically increasing counter as well.
    let unique = format!(
        "m8a{label}{}-{}-{}",
        std::process::id(),
        ENDPOINT_SEQ.fetch_add(1, Ordering::Relaxed),
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

fn exit_fixture() -> String {
    env!("CARGO_BIN_EXE_exit_fixture").to_owned()
}

async fn start_daemon() -> (
    Endpoint,
    tokio::task::JoinHandle<Result<(), stitch_mux_server::ServerError>>,
    Option<PathBuf>,
) {
    let (endpoint, socket) = test_endpoint("auto");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));
    (endpoint, server, socket)
}

async fn new_session(endpoint: &Endpoint) -> u64 {
    let response = command(
        endpoint,
        Command::NewSession {
            name: "auto".into(),
            program: exit_fixture(),
            args: vec!["--hold".into()],
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("new session");
    assert!(response.ok, "{:?}", response.error);
    let pane_id = response.sessions[0].panes[0].id;
    let listed = command(endpoint, Command::ListSessions)
        .await
        .expect("list sessions");
    assert_eq!(listed.sessions[0].id, response.sessions[0].id);
    pane_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_wait_capture_and_close_without_a_gui() {
    let (endpoint, server, socket) = start_daemon().await;
    let anchor = new_session(&endpoint).await;

    let run = command(
        &endpoint,
        Command::Run {
            session: "auto".into(),
            program: exit_fixture(),
            args: vec!["7".into()],
            cwd: None,
            env: None,
            placement: RunPlacement::Split {
                pane_id: anchor,
                axis: stitch_mux_core::SplitAxis::Columns,
                ratio_bps: 5000,
            },
            label: Some("short task".into()),
            wait: false,
            keep_open: true,
        },
    )
    .await
    .expect("run");
    assert!(run.ok, "{:?}", run.error);
    let pane_id = run.run.expect("run result").pane_id;
    assert_ne!(pane_id, anchor);

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
    let wait = waited.wait.expect("wait result");
    assert!(!wait.timed_out);
    assert_eq!(wait.exit_code, Some(7));
    // ConPTY can report the child exit before its final bytes are observable
    // through the screen; capture is a snapshot of the fed viewport.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let captured = command(
        &endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 1000,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture");
    assert!(captured.ok, "{:?}", captured.error);
    let capture = captured.capture.expect("capture result");
    assert!(
        capture
            .text
            .as_deref()
            .unwrap_or_default()
            .contains("ready"),
        "child output was not captured"
    );

    let closed = command(&endpoint, Command::ClosePane { pane_id })
        .await
        .expect("close");
    assert!(closed.ok, "{:?}", closed.error);

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_wait_returns_the_child_status_and_can_remove_the_pane() {
    let (endpoint, server, socket) = start_daemon().await;
    let anchor = new_session(&endpoint).await;

    let run = command(
        &endpoint,
        Command::Run {
            session: "auto".into(),
            program: exit_fixture(),
            args: vec!["0".into()],
            cwd: None,
            env: None,
            placement: RunPlacement::Split {
                pane_id: anchor,
                axis: stitch_mux_core::SplitAxis::Rows,
                ratio_bps: 5000,
            },
            label: None,
            wait: true,
            keep_open: false,
        },
    )
    .await
    .expect("run --wait");
    assert!(run.ok, "{:?}", run.error);
    let result = run.run.expect("run result");
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);
    let layout = run.layout.expect("layout");
    assert_eq!(
        layout.panes.len(),
        1,
        "completed pane should have been removed: {:?}",
        layout.panes.iter().map(|p| p.pane_id).collect::<Vec<_>>()
    );

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_times_out_and_reports_a_session_target() {
    let (endpoint, server, socket) = start_daemon().await;
    let anchor = new_session(&endpoint).await;

    let timed = command(
        &endpoint,
        Command::Wait {
            target: WaitTarget::Pane { pane_id: anchor },
            timeout_ms: Some(150),
        },
    )
    .await
    .expect("timeout wait");
    assert!(timed.ok, "{:?}", timed.error);
    let wait = timed.wait.expect("wait result");
    assert!(wait.timed_out);
    assert_eq!(wait.exit_code, None);

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "finishing".into(),
            program: exit_fixture(),
            args: vec!["3".into()],
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("finishing session");
    assert!(created.ok, "{:?}", created.error);

    let session_wait = command(
        &endpoint,
        Command::Wait {
            target: WaitTarget::Session {
                name: "finishing".into(),
            },
            timeout_ms: Some(10_000),
        },
    )
    .await
    .expect("session wait");
    assert!(session_wait.ok, "{:?}", session_wait.error);
    let result = session_wait.wait.expect("session wait result");
    assert!(!result.timed_out);
    assert_eq!(result.exit_code, Some(3));

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}
