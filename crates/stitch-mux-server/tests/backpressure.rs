//! End-to-end M3 backpressure proof over a real endpoint and a real child.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, ConnectionState, SessionView, connect};
use stitch_mux_core::CaptureFormat;
use stitch_mux_protocol::{
    Attach, Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, ErrorCode, ErrorInfo, Hello,
    MessageKind, ViewMode, decode_message,
};

fn test_endpoint() -> (Endpoint, Option<PathBuf>) {
    // macOS caps Unix socket paths at SUN_LEN; keep the temp filename short.
    let unique = format!("m3bp{}", nanos() % 1_000_000_000);
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
        client_name: format!("backpressure-{label}"),
        client_instance: format!("{label}-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

fn noisy_program() -> (&'static str, Vec<String>) {
    // Emit output explicitly: some platforms disable PTY echo for a
    // non-interactive shell, which would make an input-only flood silent.
    if cfg!(windows) {
        (
            "cmd.exe",
            vec![
                "/Q".into(),
                "/C".into(),
                "for /L %i in (1,1,500000) do @echo aaaaaaaaaaaaaaaaaaaa".into(),
            ],
        )
    } else {
        (
            "sh",
            vec![
                "-c".into(),
                "while true; do echo aaaaaaaaaaaaaaaaaaaa; done".into(),
            ],
        )
    }
}

async fn wait_for(label: &str, session: &ClientSession, predicate: impl Fn(&SessionView) -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if predicate(&session.view()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label} within timeout"));
}

#[tokio::test]
async fn stalled_client_is_disconnected_with_busy_and_cannot_block_other_clients() {
    let (endpoint, socket) = test_endpoint();
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let (program, args) = noisy_program();
    let created = stitch_mux_client::command(
        &endpoint,
        Command::NewSession {
            name: "noisy".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("new response");
    assert!(created.ok, "{:?}", created.error);
    let pane_id = created.sessions[0].panes[0].id;

    // A normal client keeps draining while the other connection stalls.
    let healthy = ClientSession::attach(&endpoint, "noisy", hello("healthy"))
        .await
        .expect("healthy attach");
    wait_for("healthy attach", &healthy, |view| {
        view.pane(pane_id).is_some()
    })
    .await;

    let (mut stalled, _ack) = connect(&endpoint, hello("stalled"))
        .await
        .expect("stalled handshake");
    stalled
        .send(
            MessageKind::Attach,
            &Attach {
                session_name: "noisy".into(),
                takeover: false,
            },
        )
        .await
        .expect("stalled attach");
    // Deliberately do not read from `stalled`: its socket buffer and the
    // daemon's bounded update queue will fill under the child's output.

    // Short lines matter: canonical mode on macOS stops echoing a line once
    // its line-length limit is hit, so the flood must include newlines. Each
    // round writes about 9 MB; extra rounds cover slower CI socket buffers.
    let flood = format!("{}\n", "a".repeat(80)).repeat(11_000);
    async fn flood_pane(endpoint: &Endpoint, pane_id: u64, flood: &str) {
        for _ in 0..10 {
            let sent = stitch_mux_client::command(
                endpoint,
                Command::SendText {
                    pane_id,
                    text: flood.to_owned(),
                },
            )
            .await
            .expect("flood response");
            assert!(sent.ok, "{:?}", sent.error);
        }
    }
    flood_pane(&endpoint, pane_id, &flood).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    wait_for("healthy client keeps draining", &healthy, |view| {
        view.pane(pane_id)
            .and_then(|pane| pane.snapshot.as_ref())
            .is_some_and(|snapshot| {
                snapshot
                    .cells
                    .iter()
                    .any(|row| row.cells.iter().any(|cell| cell.text.contains('a')))
            })
    })
    .await;
    assert_eq!(healthy.view().connection, ConnectionState::Connected);

    let mut busy: Option<ErrorInfo> = None;
    let mut disconnected = false;
    for round in 0..3 {
        if round > 0 {
            flood_pane(&endpoint, pane_id, &flood).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), stalled.recv_frame()).await {
                Ok(Ok(frame)) => {
                    if frame.kind == MessageKind::Error
                        && let Ok(error) = decode_message::<ErrorInfo>(&frame)
                    {
                        busy = Some(error);
                        break;
                    }
                }
                Ok(Err(_)) => {
                    disconnected = true;
                    break;
                }
                Err(_) => {}
            }
        }
        if busy.is_some() || disconnected {
            break;
        }
    }
    assert!(
        busy.is_some() || disconnected,
        "slow client must be disconnected"
    );
    if let Some(error) = busy {
        assert_eq!(error.code, ErrorCode::Busy);
    }

    let capture = stitch_mux_client::command(
        &endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 20,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture response");
    assert!(capture.ok, "{:?}", capture.error);

    healthy.detach().await;
    let killed = stitch_mux_client::command(
        &endpoint,
        Command::KillSession {
            name: "noisy".into(),
        },
    )
    .await
    .expect("kill response");
    assert!(killed.ok, "{:?}", killed.error);
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }
}
