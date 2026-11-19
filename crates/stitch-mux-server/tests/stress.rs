//! End-to-end stress: 10 MiB through a real PTY to two reading clients, plus a
//! deliberately stalled third client that must receive the typed `Busy` frame
//! and be disconnected without affecting the healthy clients.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, ConnectionState, connect};
use stitch_mux_protocol::{
    Attach, Command, CommandRequest, CommandResponse, Connection, DEFAULT_CLIENT_CAPABILITIES,
    Endpoint, ErrorInfo, Hello, MessageKind, Stream, ViewMode,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!("m3st{label}{}", nanos() % 1_000_000_000);
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

fn hello(label: &str) -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: format!("stress-{label}"),
        client_instance: format!("{label}-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

struct Control {
    connection: Connection<Stream>,
    next_request_id: u64,
}

impl Control {
    async fn connect(endpoint: &Endpoint) -> Self {
        let (connection, _ack) = connect(endpoint, hello("control")).await.expect("control");
        Self {
            connection,
            next_request_id: 1,
        }
    }

    async fn send(&mut self, command: Command) -> CommandResponse {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.connection
            .send(
                MessageKind::CommandRequest,
                &CommandRequest {
                    request_id: id,
                    command,
                },
            )
            .await
            .expect("send command");
        self.connection
            .recv(MessageKind::CommandResponse)
            .await
            .expect("command response")
    }
}

async fn wait_for_text(label: &str, session: &ClientSession, pane_id: u64, needle: &str) {
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if session
                .view()
                .pane(pane_id)
                .and_then(|pane| pane.snapshot.as_ref())
                .is_some_and(|snapshot| {
                    snapshot.cells.iter().any(|row| {
                        row.cells
                            .iter()
                            .map(|cell| cell.text.as_str())
                            .collect::<String>()
                            .contains(needle)
                    })
                })
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if result.is_err() {
        let view = session.view();
        let pane = view.pane(pane_id);
        let tail = pane
            .and_then(|pane| pane.snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .cells
                    .iter()
                    .rev()
                    .take(6)
                    .map(|row| {
                        row.cells
                            .iter()
                            .map(|cell| cell.text.as_str())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
            });
        panic!(
            "{label}: {needle} within timeout; state={:?} error={:?} exit={:?} seq={:?} tail={tail:?}",
            view.connection,
            view.error,
            view.exit,
            pane.map(|pane| pane.last_seq)
        );
    }
}

#[ignore = "10 MiB terminal stress; the CI stress job runs it in release"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_megabytes_reach_two_clients_and_a_stalled_client_is_busy() {
    let (endpoint, socket) = test_endpoint("flood");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let mut control = Control::connect(&endpoint).await;
    let created = control
        .send(Command::NewSession {
            name: "stress".into(),
            program: env!("CARGO_BIN_EXE_flood_fixture").into(),
            args: Vec::new(),
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        })
        .await;
    assert!(created.ok, "{:?}", created.error);
    let pane_id = created.sessions[0].panes[0].id;

    let first = ClientSession::attach(&endpoint, "stress", hello("first"))
        .await
        .expect("first attach");
    let second = ClientSession::attach(&endpoint, "stress", hello("second"))
        .await
        .expect("second attach");
    for client in [&first, &second] {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if client.view().pane(pane_id).is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("initial view");
    }

    let (mut stalled, _ack) = connect(&endpoint, hello("stalled")).await.expect("stalled");
    stalled
        .send(
            MessageKind::Attach,
            &Attach {
                session_name: "stress".into(),
                takeover: false,
            },
        )
        .await
        .expect("stalled attach");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = stalled.recv_frame().await.expect("stalled frame");
            if frame.kind == MessageKind::Layout {
                return;
            }
        }
    })
    .await
    .expect("stalled layout");

    let go = control
        .send(Command::SendText {
            pane_id,
            text: "go\n".into(),
        })
        .await;
    assert!(go.ok, "{:?}", go.error);
    wait_for_text("first", &first, pane_id, "STRESS-DONE").await;
    wait_for_text("second", &second, pane_id, "STRESS-DONE").await;

    // The stalled socket buffered as many deltas as it could; the daemon must
    // have closed it with the typed `Busy` error instead of growing memory.
    let error = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let frame = stalled.recv_frame().await.expect("stalled frame");
            if frame.kind == MessageKind::Error {
                let error: ErrorInfo =
                    stitch_mux_protocol::decode_message(&frame).expect("error frame");
                return error;
            }
        }
    })
    .await
    .expect("typed busy frame");
    assert_eq!(error.code, stitch_mux_protocol::ErrorCode::Busy);
    assert_eq!(error.message, "client fell behind");

    // Healthy clients stay attached after the slow peer is dropped.
    for client in [&first, &second] {
        assert_eq!(client.view().connection, ConnectionState::Connected);
    }

    first.detach().await;
    second.detach().await;
    let killed = control
        .send(Command::KillSession {
            name: "stress".into(),
        })
        .await;
    assert!(killed.ok, "{:?}", killed.error);
    server.abort();
    if let Some(path) = socket {
        let _ = std::fs::remove_file(path);
    }
}
