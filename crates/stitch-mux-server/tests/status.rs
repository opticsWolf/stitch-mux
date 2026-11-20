//! Real-PTY OSC 7501 ingestion and current-status replacement.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stitch_mux_client::{ClientSession, SessionView, command};
use stitch_mux_core::{CaptureFormat, ProgramState, ProgramStatusKind};
use stitch_mux_protocol::{Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, ViewMode};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "m7{label}{}",
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

fn hello() -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: "status-test".into(),
        client_instance: format!("status-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

async fn wait_for_status(
    client: &ClientSession,
    endpoint: &Endpoint,
    pane_id: u64,
    state: ProgramState,
    text: &str,
) {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view: SessionView = client.view();
            let status = view
                .pane(pane_id)
                .and_then(|pane| pane.snapshot.as_ref())
                .and_then(|snapshot| snapshot.program_status.clone());
            if status.as_ref().is_some_and(|status| {
                status.state == state && status.message.as_deref() == Some(text)
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if result.is_err() {
        let view = client.view();
        let status = view
            .pane(pane_id)
            .and_then(|pane| pane.snapshot.as_ref())
            .and_then(|snapshot| snapshot.program_status.clone());
        let screen = command(
            endpoint,
            Command::Capture {
                pane_id,
                start: 0,
                lines: 100,
                format: CaptureFormat::Text,
            },
        )
        .await
        .ok()
        .and_then(|response| response.capture)
        .and_then(|capture| capture.text);
        panic!("status {state:?}/{text:?} within timeout; got {status:?}; screen={screen:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn osc_7501_updates_the_current_status_and_activity() {
    let (endpoint, socket) = test_endpoint("status");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "status".into(),
            program: env!("CARGO_BIN_EXE_status_fixture").into(),
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

    let client = ClientSession::attach(&endpoint, "status", hello())
        .await
        .expect("attach");
    wait_for_status(
        &client,
        &endpoint,
        pane_id,
        ProgramState::Working,
        "Working",
    )
    .await;
    let status = client
        .view()
        .pane(pane_id)
        .unwrap()
        .snapshot
        .clone()
        .unwrap()
        .program_status
        .unwrap();
    assert_eq!(status.kind, Some(ProgramStatusKind::Question));
    assert_eq!(status.app.as_deref(), Some("status-fixture"));

    // A later record replaces the earlier one: there is no status backlog.
    wait_for_status(
        &client,
        &endpoint,
        pane_id,
        ProgramState::Blocked,
        "Waiting",
    )
    .await;
    wait_for_status(&client, &endpoint, pane_id, ProgramState::Done, "Finished").await;
    let view = client.view();
    assert_eq!(view.activity.done, 1);
    assert_eq!(view.activity.working, 0);
    let listed = command(&endpoint, Command::ListSessions)
        .await
        .expect("list sessions");
    assert_eq!(listed.sessions[0].activity.done, 1);
    assert_eq!(
        listed.sessions[0].panes[0]
            .program_status
            .as_ref()
            .map(|status| status.state),
        Some(ProgramState::Done)
    );

    // The OSC bytes are metadata, not visible terminal text.
    let captured = command(
        &endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 100,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture");
    let text = captured
        .capture
        .and_then(|capture| capture.text)
        .unwrap_or_default();
    assert!(
        !text.contains("7501"),
        "OSC payload leaked into the screen: {text:?}"
    );

    client.detach().await;
    let killed = command(
        &endpoint,
        Command::KillSession {
            name: "status".into(),
        },
    )
    .await
    .expect("kill");
    assert!(killed.ok, "{:?}", killed.error);
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}
