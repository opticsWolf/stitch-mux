//! End-to-end M1 loop over a real local endpoint and a real child process.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux::client::{ClientSession, Outbound, command};
use stitch_mux::core::Size;
use stitch_mux::protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, MessageKind, ViewMode,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!("m1s{label}{}", nanos() % 1_000_000_000);
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

fn hello() -> Hello {
    Hello {
        protocol_version: stitch_mux::protocol::PROTOCOL_VERSION,
        client_name: "session-test".into(),
        client_instance: format!("test-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
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

async fn wait_for<F: Fn(&stitch_mux::client::SessionView) -> bool>(
    label: &str,
    session: &ClientSession,
    f: F,
) {
    let mut receiver = session.subscribe();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if f(&receiver.borrow_and_update()) {
                return;
            }
            if receiver.changed().await.is_err() {
                panic!("connection closed before condition was met");
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label} within timeout"));
}

fn has_text(view: &stitch_mux::client::SessionView, needle: &str) -> bool {
    view.panes.values().any(|pane| {
        pane.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.cells.iter().any(|row| {
                let line: String = row.cells.iter().map(|cell| cell.text.as_str()).collect();
                line.contains(needle)
            })
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_detach_reattach_resize_and_capture() {
    let (endpoint, socket_path) = test_endpoint("echo");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux::server::serve(listener).await });

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "demo".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("new session response");
    assert!(created.ok, "{:?}", created.error);
    let pane_id = created.sessions[0].panes[0].id;

    let session = ClientSession::attach(&endpoint, "demo", hello())
        .await
        .expect("attach");
    wait_for("initial snapshot", &session, |view| {
        view.pane(pane_id).is_some()
    })
    .await;

    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    session.send(Outbound::Input(InputEvent::Text(format!(
        "echo stitch-mux-echo{newline}"
    ))));
    wait_for("echo visible", &session, |view| {
        has_text(view, "stitch-mux-echo")
    })
    .await;

    let captured = command(
        &endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 50,
            format: stitch_mux::core::CaptureFormat::Text,
        },
    )
    .await
    .expect("capture response");
    assert!(captured.ok, "{:?}", captured.error);
    assert!(
        captured
            .capture
            .unwrap()
            .text
            .unwrap()
            .contains("stitch-mux-echo")
    );

    session.send(Outbound::Resize(stitch_mux::protocol::ResizeRequest {
        cols: 60,
        rows: 20,
        pixel_width: 0,
        pixel_height: 0,
    }));
    wait_for("resize", &session, |view| view.size == Size::new(60, 20)).await;

    session.detach().await;

    // The child and its screen survive a full client disconnect.
    let reattached = ClientSession::attach(&endpoint, "demo", hello())
        .await
        .expect("reattach");
    wait_for("reattach snapshot", &reattached, |view| {
        view.pane(pane_id).is_some()
    })
    .await;
    // The smallest attached client sets the canonical size; the reattaching
    // 80x24 client is now the only one, so the session grows back to it. The
    // daemon may still be unregistering the old connection, so wait for the
    // canonical size instead of asserting immediately.
    wait_for("canonical size after reattach", &reattached, |view| {
        view.size == Size::new(80, 24)
    })
    .await;
    // The echo may have moved into bounded history after the resize; capture
    // is the authoritative check that the child's screen survived.
    let restored = command(
        &endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 200,
            format: stitch_mux::core::CaptureFormat::Text,
        },
    )
    .await
    .expect("capture after reattach");
    assert!(restored.ok, "{:?}", restored.error);
    assert!(
        restored
            .capture
            .unwrap()
            .text
            .unwrap()
            .contains("stitch-mux-echo")
    );
    reattached.detach().await;

    let listed = command(&endpoint, Command::ListSessions)
        .await
        .expect("list response");
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].name, "demo");
    let info = &listed.sessions[0];
    assert_eq!(info.panes[0].cols, info.cols - 2);
    assert_eq!(info.panes[0].rows, info.rows - 2);
    assert!(info.last_activity_unix_ms >= info.created_at_unix_ms);

    let killed = command(
        &endpoint,
        Command::KillSession {
            name: "demo".into(),
        },
    )
    .await
    .expect("kill response");
    assert!(killed.ok, "{:?}", killed.error);
    let listed = command(&endpoint, Command::ListSessions)
        .await
        .expect("list response");
    assert!(listed.sessions.is_empty());

    server.abort();
    if let Some(path) = socket_path {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[tokio::test]
async fn unknown_session_attach_is_a_typed_error() {
    let (endpoint, socket_path) = test_endpoint("unknown");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux::server::serve(listener).await });

    let (connection, _ack) = stitch_mux::client::connect(&endpoint, hello())
        .await
        .expect("handshake");
    let (mut reader, mut writer) = connection.split();
    writer
        .send(
            MessageKind::Attach,
            &stitch_mux::protocol::Attach {
                session_name: "missing".into(),
                takeover: false,
            },
        )
        .await
        .unwrap();
    let frame = reader.recv_frame().await.expect("error frame");
    assert_eq!(frame.kind, MessageKind::Error);
    let error: stitch_mux::protocol::ErrorInfo =
        stitch_mux::protocol::decode_message(&frame).unwrap();
    assert_eq!(error.code, stitch_mux::protocol::ErrorCode::NotFound);

    server.abort();
    if let Some(path) = socket_path {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_exit_becomes_a_clean_exit_event() {
    let (endpoint, socket_path) = test_endpoint("exit");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux::server::serve(listener).await });

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "exit".into(),
            program: program.into(),
            args,
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

    let session = ClientSession::attach(&endpoint, "exit", hello())
        .await
        .expect("attach");
    wait_for("initial snapshot", &session, |view| {
        view.pane(pane_id).is_some()
    })
    .await;
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    // A freshly spawned shell may swallow the first line; re-send until the
    // child has actually exited.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while session.view().exit.is_none() && tokio::time::Instant::now() < deadline {
        let sent = command(
            &endpoint,
            Command::SendText {
                pane_id,
                text: format!("exit 7{newline}"),
            },
        )
        .await
        .expect("send exit");
        assert!(sent.ok, "{:?}", sent.error);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    wait_for("clean exit event", &session, |view| view.exit.is_some()).await;
    let exit = session.view().exit.expect("exit event");
    assert_eq!(exit.pane_id, Some(pane_id));
    assert!(
        exit.reason.contains('7'),
        "unexpected exit: {}",
        exit.reason
    );

    // The daemon stays healthy after a child exit.
    let listed = command(&endpoint, Command::ListSessions)
        .await
        .expect("list after exit");
    assert!(listed.ok, "{:?}", listed.error);

    let _ = command(
        &endpoint,
        Command::KillSession {
            name: "exit".into(),
        },
    )
    .await;
    session.detach().await;
    server.abort();
    if let Some(path) = socket_path {
        let _ = tokio::fs::remove_file(path).await;
    }
}
