//! End-to-end M3 stress proof over a real endpoint and a real child.
//!
//! The typed slow-client `Busy` frame is asserted deterministically in the
//! server unit tests; this test proves that heavy traffic and repeated
//! attach/detach leave healthy clients and the daemon responsive.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, ConnectionState, SessionView, command};
use stitch_mux_core::CaptureFormat;
use stitch_mux_protocol::{Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, ViewMode};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    // macOS caps Unix socket paths at SUN_LEN; keep the temp filename short.
    let unique = format!("m3bp{label}{}", nanos() % 1_000_000_000);
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

fn shell() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("cmd.exe", vec!["/Q".into()])
    } else {
        ("sh", Vec::new())
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

fn pane_has_a(view: &SessionView, pane_id: u64) -> bool {
    view.pane(pane_id)
        .and_then(|pane| pane.snapshot.as_ref())
        .is_some_and(|snapshot| {
            snapshot
                .cells
                .iter()
                .any(|row| row.cells.iter().any(|cell| cell.text.contains('a')))
        })
}

#[tokio::test]
async fn repeated_attach_and_delta_pump_stay_responsive() {
    let (endpoint, socket) = test_endpoint("pump");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "stress".into(),
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

    let first = ClientSession::attach(&endpoint, "stress", hello("first"))
        .await
        .expect("first attach");
    let second = ClientSession::attach(&endpoint, "stress", hello("second"))
        .await
        .expect("second attach");
    for label in ["first", "second"] {
        let client = if label == "first" { &first } else { &second };
        wait_for(label, client, |view| view.pane(pane_id).is_some()).await;
    }
    for _ in 0..10 {
        let echoed = command(
            &endpoint,
            Command::SendText {
                pane_id,
                text: "echo aaaaaaaaaaaaaaaaaaaa
"
                .into(),
            },
        )
        .await
        .expect("echo response");
        assert!(echoed.ok, "{:?}", echoed.error);
        if pane_has_a(&first.view(), pane_id) && pane_has_a(&second.view(), pane_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    wait_for("first client sees the output", &first, |view| {
        pane_has_a(view, pane_id)
    })
    .await;
    wait_for("second client sees the output", &second, |view| {
        pane_has_a(view, pane_id)
    })
    .await;
    assert_eq!(first.view().connection, ConnectionState::Connected);
    assert_eq!(second.view().connection, ConnectionState::Connected);

    for index in 0..3 {
        let transient = ClientSession::attach(&endpoint, "stress", hello("transient"))
            .await
            .expect("transient attach");
        wait_for("transient attach", &transient, |view| {
            view.pane(pane_id).is_some()
        })
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(10), transient.detach()).await;
        assert_eq!(
            first.view().connection,
            ConnectionState::Connected,
            "{index}"
        );
    }

    let capture = command(
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

    let killed = command(
        &endpoint,
        Command::KillSession {
            name: "stress".into(),
        },
    )
    .await
    .expect("kill response");
    assert!(killed.ok, "{:?}", killed.error);
    let _ = tokio::time::timeout(Duration::from_secs(10), first.detach()).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), second.detach()).await;
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }
}
