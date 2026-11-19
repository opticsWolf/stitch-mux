//! Real local-endpoint handshake test: client -> daemon -> detach.

use std::time::Duration;

use stitch_mux_client::connect;
use stitch_mux_protocol::{
    Command, Connection, DEFAULT_CLIENT_CAPABILITIES, Endpoint, ErrorCode, Hello, MessageKind,
    ViewMode,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<std::path::PathBuf>) {
    let unique = format!("m0h{label}{}", unique_suffix() % 1_000_000_000);
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

fn unique_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
}

#[tokio::test]
async fn client_and_daemon_exchange_hello_then_detach() {
    let (endpoint, socket_path) = test_endpoint("hello");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });

    let hello = Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: "handshake-test".into(),
        client_instance: "one".into(),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    };

    let (mut connection, ack) =
        tokio::time::timeout(Duration::from_secs(5), connect(&endpoint, hello))
            .await
            .expect("connect within timeout")
            .expect("handshake succeeds");
    assert_eq!(ack.protocol_version, 1);
    assert_eq!(ack.sessions, 0);

    connection.send(MessageKind::Ping, &7_u8).await.unwrap();
    let pong: u8 = connection.recv(MessageKind::Pong).await.unwrap();
    assert_eq!(pong, 7);
    connection
        .send(
            MessageKind::Detach,
            &stitch_mux_protocol::Detach {
                reason: "test".into(),
            },
        )
        .await
        .unwrap();
    connection.shutdown().await.unwrap();

    server.abort();
    if let Some(path) = socket_path {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[tokio::test]
async fn wrong_protocol_version_is_rejected() {
    let (endpoint, socket_path) = test_endpoint("version");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });

    let hello = Hello {
        protocol_version: 99,
        client_name: "old-client".into(),
        client_instance: "two".into(),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    };
    let stream = endpoint.connect().await.unwrap();
    let mut connection = Connection::new(stream);
    connection.send(MessageKind::Hello, &hello).await.unwrap();
    let error: stitch_mux_protocol::ErrorInfo = connection
        .recv(MessageKind::Error)
        .await
        .expect("typed protocol error");
    assert_eq!(error.code, stitch_mux_protocol::ErrorCode::ProtocolVersion);

    server.abort();
    if let Some(path) = socket_path {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[tokio::test]
async fn takeover_attach_is_rejected() {
    let (endpoint, socket_path) = test_endpoint("takeover");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });
    let (program, args) = if cfg!(windows) {
        ("cmd.exe".to_owned(), Vec::new())
    } else {
        ("sh".to_owned(), Vec::new())
    };
    let created = stitch_mux_client::command(
        &endpoint,
        Command::NewSession {
            name: "takeover".into(),
            program,
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

    let hello = Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: "takeover-test".into(),
        client_instance: "takeover".into(),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    };
    let (mut connection, _) = connect(&endpoint, hello).await.expect("handshake");
    connection
        .send(
            MessageKind::Attach,
            &stitch_mux_protocol::Attach {
                session_name: "takeover".into(),
                takeover: true,
            },
        )
        .await
        .unwrap();
    let error: stitch_mux_protocol::ErrorInfo = connection
        .recv(MessageKind::Error)
        .await
        .expect("typed takeover error");
    assert_eq!(error.code, ErrorCode::Unsupported);

    let _ = stitch_mux_client::command(
        &endpoint,
        Command::KillSession {
            name: "takeover".into(),
        },
    )
    .await;
    connection.shutdown().await.unwrap();
    server.abort();
    if let Some(path) = socket_path {
        let _ = tokio::fs::remove_file(path).await;
    }
}
