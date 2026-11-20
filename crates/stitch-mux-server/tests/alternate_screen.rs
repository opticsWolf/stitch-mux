//! Alternate-screen transitions restore the authoritative shell viewport.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stitch_mux_client::{ClientSession, SessionView, command};
use stitch_mux_core::CaptureFormat;
use stitch_mux_protocol::{Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, ViewMode};

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
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: "alt-test".into(),
        client_instance: format!("alt-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

async fn wait_for_alt_screen(client: &ClientSession, pane_id: u64, expected: bool) {
    let result = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let is_alt = client
                .view()
                .pane(pane_id)
                .and_then(|pane| pane.snapshot.as_ref())
                .is_some_and(|snapshot| snapshot.alt_screen == expected);
            if is_alt {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(result.is_ok(), "alt_screen never became {expected}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alternate_screen_exit_restores_the_shell_screen() {
    let (endpoint, socket) = test_endpoint("alt");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "alt".into(),
            program: env!("CARGO_BIN_EXE_alt_fixture").into(),
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

    let client = ClientSession::attach(&endpoint, "alt", hello())
        .await
        .expect("attach");

    wait_for_alt_screen(&client, pane_id, true).await;
    wait_for_alt_screen(&client, pane_id, false).await;

    let text = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = command(
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
            if let Some(text) = response.capture.and_then(|capture| capture.text)
                && text.contains("shell-before")
                && text.contains("shell-after")
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("shell screen was not restored");
    assert!(
        !text.contains("alt-view"),
        "alternate screen leaked: {text:?}"
    );

    let _: SessionView = client.view();
    client.detach().await;
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("socket parent")).await;
    }
}
