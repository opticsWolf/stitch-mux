//! Bounded local event stream: program status, activity, and pane exit.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use stitch_mux_client::{command, connect};
use stitch_mux_core::ProgramState;
use stitch_mux_protocol::{
    Command, Connection, DEFAULT_CLIENT_CAPABILITIES, Endpoint, ErrorCode, ErrorInfo, Event, Hello,
    MessageKind, Stream, SubscribeEvents, ViewMode, capabilities, decode_message,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "m7ev{label}{}",
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

fn hello(capabilities: u64) -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: "events-test".into(),
        client_instance: format!("events-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities,
    }
}

async fn next_event(connection: &mut Connection<Stream>, deadline: Instant) -> Option<Event> {
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let frame = tokio::time::timeout(remaining, connection.recv_frame())
            .await
            .ok()?
            .ok()?;
        if frame.kind == MessageKind::Event {
            return Some(decode_message(&frame).expect("event payload"));
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_activity_and_exit_events_are_monotonic() {
    let (endpoint, socket) = test_endpoint("stream");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "events".into(),
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

    let mut connection = connect(&endpoint, hello(DEFAULT_CLIENT_CAPABILITIES))
        .await
        .expect("control connection")
        .0;
    connection
        .send(
            MessageKind::SubscribeEvents,
            &SubscribeEvents {
                session: Some("events".into()),
                all: false,
                since_seq: None,
            },
        )
        .await
        .expect("subscribe");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_seq = 0;
    let mut saw_status = false;
    let mut saw_activity = false;
    let mut saw_exit = false;
    while let Some(event) = next_event(&mut connection, deadline).await {
        assert!(event.seq > last_seq, "event sequence went backwards");
        last_seq = event.seq;
        match event.kind.as_str() {
            "session_list" => assert_eq!(event.session_id, Some(created.sessions[0].id)),
            "program_status_changed" => {
                assert_eq!(event.session_id, Some(created.sessions[0].id));
                let status: stitch_mux_core::ProgramStatus =
                    serde_json::from_value(event.payload["status"].clone()).expect("status");
                assert_eq!(event.payload["pane_id"], pane_id);
                if status.state == ProgramState::Done {
                    saw_status = true;
                    let killed = command(
                        &endpoint,
                        Command::KillSession {
                            name: "events".into(),
                        },
                    )
                    .await
                    .expect("kill");
                    assert!(killed.ok, "{:?}", killed.error);
                }
            }
            "activity_changed" => saw_activity = true,
            "pane_exited" => {
                assert_eq!(event.payload["pane_id"], pane_id);
                saw_exit = true;
                break;
            }
            other => panic!("unexpected event kind {other}"),
        }
    }
    assert!(saw_status, "no program_status_changed event");
    assert!(saw_activity, "no activity_changed event");
    assert!(saw_exit, "no pane_exited event");

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unscoped_subscribers_receive_the_initial_session_list() {
    let (endpoint, socket) = test_endpoint("list");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "listed".into(),
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

    let mut connection = connect(&endpoint, hello(DEFAULT_CLIENT_CAPABILITIES))
        .await
        .expect("control connection")
        .0;
    connection
        .send(
            MessageKind::SubscribeEvents,
            &SubscribeEvents {
                session: None,
                all: false,
                since_seq: None,
            },
        )
        .await
        .expect("subscribe");

    let event = next_event(&mut connection, Instant::now() + Duration::from_secs(5))
        .await
        .expect("session_list");
    assert_eq!(event.kind, "session_list");
    assert!(event.seq > 0);

    let killed = command(
        &endpoint,
        Command::KillSession {
            name: "listed".into(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_requires_the_events_capability() {
    let (endpoint, socket) = test_endpoint("cap");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let mut connection = connect(&endpoint, hello(capabilities::client::STRUCTURED_VIEW))
        .await
        .expect("control connection")
        .0;
    connection
        .send(
            MessageKind::SubscribeEvents,
            &SubscribeEvents {
                session: None,
                all: false,
                since_seq: None,
            },
        )
        .await
        .expect("subscribe");
    let frame = tokio::time::timeout(Duration::from_secs(5), connection.recv_frame())
        .await
        .expect("typed error")
        .expect("frame");
    assert_eq!(frame.kind, MessageKind::Error);
    let error: ErrorInfo = decode_message(&frame).expect("error payload");
    assert_eq!(error.code, ErrorCode::Unsupported);

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}
