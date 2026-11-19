//! M4 end-to-end check: a terminal projection and a structured GUI share one
//! authoritative session over a real local endpoint and a real child process.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, Outbound, SessionView, command};
use stitch_mux_core::CaptureFormat;
use stitch_mux_protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, ViewMode,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!("m4{label}{}", nanos() % 1_000_000_000);
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

fn hello(label: &str, view_mode: ViewMode) -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: format!("terminal-test-{label}"),
        client_instance: format!("{label}-{}", std::process::id()),
        view_mode,
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
    tokio::time::timeout(Duration::from_secs(20), async {
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

async fn wait_for_render_text(label: &str, session: &ClientSession, needle: &str, after: u64) {
    let mut receiver = session.subscribe();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            {
                let view = receiver.borrow_and_update();
                if view.render_seq > after
                    && view.render.as_ref().is_some_and(|render| {
                        String::from_utf8_lossy(&render.ansi).contains(needle)
                    })
                {
                    return;
                }
            }
            if receiver.changed().await.is_err() {
                panic!("connection closed before {label}");
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label} within timeout"));
}

async fn captured_text(endpoint: &Endpoint, pane_id: u64) -> String {
    let response = command(
        endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 500,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture response");
    assert!(response.ok, "{:?}", response.error);
    response
        .capture
        .and_then(|capture| capture.text)
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_projection_shares_layout_and_output_with_a_structured_client() {
    let (endpoint, socket_path) = test_endpoint("shared");
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

    let terminal = ClientSession::attach(&endpoint, "demo", hello("terminal", ViewMode::Terminal))
        .await
        .expect("terminal attach");
    wait_for("first Render", &terminal, |view| {
        view.render.is_some() && view.render_seq >= 1
    })
    .await;
    let first_seq = terminal.view().render_seq;
    let first = terminal.view().render.expect("render");
    assert!(!first.ansi.is_empty());
    assert_eq!((first.cols, first.rows), (80, 24));
    assert!(terminal.view().layout.is_some());

    let gui = ClientSession::attach(&endpoint, "demo", hello("gui", ViewMode::Structured))
        .await
        .expect("structured attach");
    wait_for("structured snapshot", &gui, |view| {
        view.pane(pane_id)
            .and_then(|pane| pane.snapshot.as_ref())
            .is_some()
    })
    .await;

    terminal.send(Outbound::Input(InputEvent::Text("echo M4-SHARED\n".into())));
    wait_for_render_text("output in Render", &terminal, "M4-SHARED", first_seq).await;
    let mut text = String::new();
    for _ in 0..40 {
        text = captured_text(&endpoint, pane_id).await;
        if text.contains("M4-SHARED") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(text.contains("M4-SHARED"), "capture: {text:?}");
    let delta = terminal.view().render.expect("render");
    assert!(!delta.full, "steady-state output must use a delta frame");

    let base_revision = terminal.view().layout_revision;
    let split = command(
        &endpoint,
        Command::SplitPane {
            pane_id,
            axis: stitch_mux_core::SplitAxis::Columns,
            ratio_bps: 5000,
        },
    )
    .await
    .expect("split response");
    assert!(split.ok, "{:?}", split.error);
    wait_for("terminal layout revision after split", &terminal, |view| {
        view.layout_revision > base_revision
            && view
                .layout
                .as_ref()
                .is_some_and(|layout| layout.panes.len() == 2)
    })
    .await;
    wait_for("structured layout revision after split", &gui, |view| {
        view.layout_revision > base_revision
    })
    .await;
    let terminal_view = terminal.view();
    let gui_view = gui.view();
    assert_eq!(terminal_view.size, gui_view.size);
    assert_eq!(
        terminal_view.layout.as_ref().unwrap().panes,
        gui_view.layout.as_ref().unwrap().panes
    );

    terminal.detach().await;
    wait_for("gui sees the detach", &gui, |view| {
        view.attached_clients == 1
    })
    .await;
    assert!(
        captured_text(&endpoint, pane_id)
            .await
            .contains("M4-SHARED")
    );

    gui.detach().await;
    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_view_rejects_missing_ansi_capability() {
    let (endpoint, socket_path) = test_endpoint("caps");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });

    let mut hello = hello("nocaps", ViewMode::Terminal);
    hello.capabilities = 0;
    let result = stitch_mux_client::connect(&endpoint, hello).await;
    assert!(
        result.is_err(),
        "handshake must fail without ANSI capability"
    );

    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}
