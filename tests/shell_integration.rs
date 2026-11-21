//! Real-PTY OSC 133/1337 metadata reaches the structured client view.

mod common;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stitch_mux::client::{ClientSession, SessionView, command};
use stitch_mux::protocol::{Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, ViewMode};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "m10{label}{}",
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
        protocol_version: stitch_mux::protocol::PROTOCOL_VERSION,
        client_name: "shell-test".into(),
        client_instance: format!("shell-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_marks_and_cwd_survive_the_snapshot_pipeline() {
    let (endpoint, socket) = test_endpoint("shell");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux::server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "shell".into(),
            program: common::fixture_bin("shell_fixture"),
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

    let client = ClientSession::attach(&endpoint, "shell", hello())
        .await
        .expect("attach");

    let result = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let view: SessionView = client.view();
            let snapshot = view.pane(pane_id).and_then(|pane| pane.snapshot.as_ref());
            if snapshot.is_some_and(|snapshot| {
                snapshot.cwd.as_deref() == Some("/work")
                    && snapshot
                        .shell_marks
                        .as_ref()
                        .is_some_and(|marks| !marks.prompt_rows.is_empty())
                    && snapshot.total_lines > 0
                    && snapshot.notification.is_some()
                    && snapshot.progress.is_some()
                    && snapshot.cells.iter().any(|row| {
                        row.cells
                            .iter()
                            .any(|cell| cell.link.as_deref() == Some("https://example.com/docs"))
                    })
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "shell metadata did not reach the client view; snapshot={:?}",
        client
            .view()
            .pane(pane_id)
            .and_then(|pane| pane.snapshot.clone())
    );

    let snapshot = client
        .view()
        .pane(pane_id)
        .and_then(|pane| pane.snapshot.clone())
        .expect("snapshot");
    assert_eq!(snapshot.cwd.as_deref(), Some("/work"));
    let marks = snapshot.shell_marks.expect("shell marks");
    assert!(!marks.prompt_rows.is_empty(), "no prompt rows: {marks:?}");
    assert!(!marks.command_rows.is_empty(), "no command rows: {marks:?}");
    assert!(
        marks
            .command_rows
            .iter()
            .all(|row| *row >= marks.prompt_rows[0])
    );
    assert_eq!(
        snapshot.notification.as_ref().unwrap().message,
        "Stage 2 failed"
    );
    assert_eq!(
        snapshot.progress.as_ref().unwrap().state,
        stitch_mux::protocol::ProgressState::Error
    );
    assert_eq!(snapshot.progress.as_ref().unwrap().value, Some(75));
    assert!(snapshot.total_lines >= snapshot.scrollback_len);
    assert!(
        snapshot.cells.iter().any(|row| {
            row.cells
                .iter()
                .any(|cell| cell.link.as_deref() == Some("https://example.com/docs"))
        }),
        "OSC 8 URI was not attached to any cell"
    );

    client.detach().await;
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("socket parent")).await;
    }
}
