//! End-to-end M1 loop over a real local endpoint and a real child process.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, Outbound, command};
use stitch_mux_core::Size;
use stitch_mux_protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, MessageKind, ViewMode,
};

fn test_endpoint() -> (Endpoint, Option<PathBuf>) {
    let unique = format!("stitch-mux-session-{}-{}", std::process::id(), nanos());
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
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
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

async fn wait_for<F: Fn(&stitch_mux_client::SessionView) -> bool>(
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

fn has_text(view: &stitch_mux_client::SessionView, needle: &str) -> bool {
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
    let (endpoint, socket_path) = test_endpoint();
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });

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
            format: stitch_mux_core::CaptureFormat::Text,
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

    session.send(Outbound::Resize(stitch_mux_protocol::ResizeRequest {
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
    // 80x24 client is now the only one, so the session grows back to it.
    assert_eq!(reattached.view().size, Size::new(80, 24));
    // The echo may have moved into bounded history after the resize; capture
    // is the authoritative check that the child's screen survived.
    let restored = command(
        &endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 200,
            format: stitch_mux_core::CaptureFormat::Text,
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
    let (endpoint, socket_path) = test_endpoint();
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });

    let (connection, _ack) = stitch_mux_client::connect(&endpoint, hello())
        .await
        .expect("handshake");
    let (mut reader, mut writer) = connection.split();
    writer
        .send(
            MessageKind::Attach,
            &stitch_mux_protocol::Attach {
                session_name: "missing".into(),
                takeover: false,
            },
        )
        .await
        .unwrap();
    let frame = reader.recv_frame().await.expect("error frame");
    assert_eq!(frame.kind, MessageKind::Error);
    let error: stitch_mux_protocol::ErrorInfo =
        stitch_mux_protocol::decode_message(&frame).unwrap();
    assert_eq!(error.code, stitch_mux_protocol::ErrorCode::NotFound);

    server.abort();
    if let Some(path) = socket_path {
        let _ = tokio::fs::remove_file(path).await;
    }
}
