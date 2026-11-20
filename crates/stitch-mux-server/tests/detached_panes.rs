//! M11: detached panes keep running and can be placed back without restart.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stitch_mux_client::command;
use stitch_mux_core::{CaptureFormat, PanePlacement, SplitAxis};
use stitch_mux_protocol::{Command, Endpoint, PaneInfo, WaitTarget};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "m11{label}{}",
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

async fn capture_text(endpoint: &Endpoint, pane_id: u64) -> String {
    let response = command(
        endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 100,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture");
    response
        .capture
        .and_then(|capture| capture.text)
        .unwrap_or_default()
}

fn pane_info(response: &stitch_mux_protocol::CommandResponse, pane_id: u64) -> PaneInfo {
    response
        .sessions
        .iter()
        .flat_map(|session| session.panes.iter())
        .find(|pane| pane.id == pane_id)
        .cloned()
        .expect("pane in session info")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_and_place_keeps_the_child_and_scrollback() {
    let (endpoint, socket) = test_endpoint("place");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "place".into(),
            program: env!("CARGO_BIN_EXE_exit_fixture").into(),
            args: vec!["--hold".into()],
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

    tokio::time::timeout(Duration::from_secs(5), async {
        while !capture_text(&endpoint, pane_id).await.contains("ready") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child never became ready");

    let detached = command(&endpoint, Command::DetachPane { pane_id })
        .await
        .expect("detach");
    assert!(detached.ok, "{:?}", detached.error);
    assert_eq!(
        pane_info(&detached, pane_id).placement,
        PanePlacement::Detached
    );
    let layout = detached.layout.expect("layout");
    assert!(layout.panes.is_empty(), "empty window should have no panes");
    assert!(capture_text(&endpoint, pane_id).await.contains("ready"));

    let placed = command(
        &endpoint,
        Command::PlacePane {
            pane_id,
            window_id: None,
            anchor_pane_id: None,
            axis: SplitAxis::Columns,
            ratio_bps: 5000,
        },
    )
    .await
    .expect("place");
    assert!(placed.ok, "{:?}", placed.error);
    assert_eq!(pane_info(&placed, pane_id).placement, PanePlacement::Tiled);
    let layout = placed.layout.expect("layout");
    assert_eq!(layout.panes.len(), 1);
    assert_eq!(layout.panes[0].pane_id, pane_id);
    let after = capture_text(&endpoint, pane_id).await;
    assert!(
        after.contains("ready"),
        "scrollback was lost across detach/place: {after:?}"
    );

    command(
        &endpoint,
        Command::KillSession {
            name: "place".into(),
        },
    )
    .await
    .expect("kill");
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("socket parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detached_pane_can_exit_and_wait_returns_its_code() {
    let (endpoint, socket) = test_endpoint("exit");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "detached-exit".into(),
            program: env!("CARGO_BIN_EXE_detach_exit_fixture").into(),
            args: Vec::new(),
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

    tokio::time::timeout(Duration::from_secs(5), async {
        while !capture_text(&endpoint, pane_id).await.contains("ready") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child never became ready");

    let detached = command(&endpoint, Command::DetachPane { pane_id })
        .await
        .expect("detach");
    assert!(detached.ok, "{:?}", detached.error);
    assert_eq!(
        pane_info(&detached, pane_id).placement,
        PanePlacement::Detached
    );

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
    let result = waited.wait.expect("wait result");
    assert_eq!(result.exit_code, Some(7));
    assert!(!result.timed_out);

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("socket parent")).await;
    }
}
