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
    // A child that emits output itself avoids shell parsing a multi-megabyte
    // command line and exercises the same structured delta pipeline.
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
        ("sh", vec!["-c".into(), "yes aaaaaaaaaaaaaaaaaaaa".into()])
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
async fn ten_megabytes_and_repeated_attach_stay_responsive() {
    let (endpoint, socket) = test_endpoint();
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let (program, args) = noisy_program();
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

    wait_for("first client sees the flood", &first, |view| {
        pane_has_a(view, pane_id)
    })
    .await;
    wait_for("second client sees the flood", &second, |view| {
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
        transient.detach().await;
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
    first.detach().await;
    second.detach().await;
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }
}
