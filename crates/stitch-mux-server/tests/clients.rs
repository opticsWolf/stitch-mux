//! End-to-end M3 multi-client loop over a real endpoint and real children.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, ConnectionState, Outbound, SessionView, command};
use stitch_mux_core::{CaptureFormat, Size, SplitAxis};
use stitch_mux_protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, ErrorCode, Hello, InputEvent, ViewMode,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "stitch-mux-clients-{label}-{}-{}",
        std::process::id(),
        nanos()
    );
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

fn hello(label: &str, cols: u16, rows: u16) -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: format!("clients-test-{label}"),
        client_instance: format!("{label}-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols,
        rows,
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
    tokio::time::timeout(Duration::from_secs(15), async {
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

async fn wait_for_capture(endpoint: &Endpoint, pane_id: u64, needle: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if captured_text(endpoint, pane_id).await.contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("capture contains {needle} within timeout"));
}

#[tokio::test]
async fn two_clients_share_a_session_with_independent_focus() {
    let (endpoint, socket) = test_endpoint("share");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "shared".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 100,
            rows: 30,
        },
    )
    .await
    .expect("new response");
    assert!(created.ok, "{:?}", created.error);
    let first_pane = created.sessions[0].panes[0].id;

    let split = command(
        &endpoint,
        Command::SplitPane {
            pane_id: first_pane,
            axis: SplitAxis::Columns,
            ratio_bps: 5000,
        },
    )
    .await
    .expect("split response");
    assert!(split.ok, "{:?}", split.error);
    let second_pane = split
        .layout
        .unwrap()
        .panes
        .iter()
        .map(|geometry| geometry.pane_id)
        .find(|id| *id != first_pane)
        .expect("second pane");

    // The larger client attaches first; the 60x18 client then owns the size.
    let first = ClientSession::attach(&endpoint, "shared", hello("large", 100, 30))
        .await
        .expect("first attach");
    let second = ClientSession::attach(&endpoint, "shared", hello("small", 60, 18))
        .await
        .expect("second attach");
    for client in [&first, &second] {
        wait_for("canonical size", client, |view| {
            view.size == Size::new(60, 18)
                && view
                    .layout
                    .as_ref()
                    .is_some_and(|layout| layout.panes.len() == 2)
        })
        .await;
    }

    first.send(Outbound::Focus(second_pane));
    second.send(Outbound::Focus(first_pane));
    wait_for("first focus", &first, |view| {
        view.focus == Some(second_pane)
    })
    .await;
    wait_for("second focus", &second, |view| {
        view.focus == Some(first_pane)
    })
    .await;

    first.send(Outbound::Input(InputEvent::Text("m3-first\n".into())));
    wait_for_capture(&endpoint, second_pane, "m3-first").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !captured_text(&endpoint, first_pane)
            .await
            .contains("m3-first")
    );

    first.detach().await;
    wait_for("survivor stays connected", &second, |view| {
        view.connection == ConnectionState::Connected && view.attached_clients == 1
    })
    .await;
    second.send(Outbound::Input(InputEvent::Text("m3-second\n".into())));
    wait_for_capture(&endpoint, first_pane, "m3-second").await;

    second.detach().await;
    let listed = command(&endpoint, Command::ListSessions)
        .await
        .expect("list sessions");
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].attached_clients, 0);

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[tokio::test]
async fn smaller_resize_reaches_every_client() {
    let (endpoint, socket) = test_endpoint("resize");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "resized".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 100,
            rows: 30,
        },
    )
    .await
    .expect("new response");
    assert!(created.ok, "{:?}", created.error);

    let large = ClientSession::attach(&endpoint, "resized", hello("large", 100, 30))
        .await
        .expect("large attach");
    let small = ClientSession::attach(&endpoint, "resized", hello("small", 60, 18))
        .await
        .expect("small attach");
    wait_for("initial canonical size", &large, |view| {
        view.size == Size::new(60, 18)
    })
    .await;

    small.send(Outbound::Resize(stitch_mux_protocol::ResizeRequest {
        cols: 40,
        rows: 12,
        pixel_width: 0,
        pixel_height: 0,
    }));
    wait_for("large client receives new geometry", &large, |view| {
        view.size == Size::new(40, 12)
            && view
                .layout
                .as_ref()
                .is_some_and(|layout| layout.cols == 40 && layout.rows == 12)
    })
    .await;
    wait_for("small client receives new geometry", &small, |view| {
        view.size == Size::new(40, 12)
    })
    .await;

    large.detach().await;
    small.detach().await;
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[tokio::test]
async fn registry_rename_detach_and_shutdown_are_typed() {
    let (endpoint, socket) = test_endpoint("control");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "before".into(),
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

    let first = ClientSession::attach(&endpoint, "before", hello("one", 80, 24))
        .await
        .expect("first attach");
    let second = ClientSession::attach(&endpoint, "before", hello("two", 80, 24))
        .await
        .expect("second attach");
    wait_for("first attached", &first, |view| {
        view.connection == ConnectionState::Connected
    })
    .await;
    wait_for("second attached", &second, |view| {
        view.connection == ConnectionState::Connected
    })
    .await;

    let clients = command(
        &endpoint,
        Command::ListClients {
            session: "before".into(),
        },
    )
    .await
    .expect("list clients");
    assert!(clients.ok, "{:?}", clients.error);
    assert_eq!(clients.clients.len(), 2);
    assert!(
        clients
            .clients
            .iter()
            .all(|client| client.session_id == Some(created.sessions[0].id))
    );

    let renamed = command(
        &endpoint,
        Command::RenameSession {
            name: "before".into(),
            new_name: "after".into(),
        },
    )
    .await
    .expect("rename response");
    assert!(renamed.ok, "{:?}", renamed.error);
    assert_eq!(renamed.sessions[0].name, "after");

    let first_id = clients
        .clients
        .iter()
        .find(|client| client.session_id.is_some())
        .expect("attached client")
        .id;
    let detached = command(
        &endpoint,
        Command::DetachClient {
            client_id: first_id,
        },
    )
    .await
    .expect("detach client response");
    assert!(detached.ok, "{:?}", detached.error);
    wait_for("detached client sees a typed error", &first, |view| {
        view.connection == ConnectionState::Lost
            && view
                .error
                .as_ref()
                .is_some_and(|error| error.code == ErrorCode::Busy)
    })
    .await;
    wait_for("other client survives", &second, |view| {
        view.connection == ConnectionState::Connected && view.attached_clients == 1
    })
    .await;

    let shutdown = command(&endpoint, Command::Shutdown)
        .await
        .expect("shutdown response");
    assert!(shutdown.ok, "{:?}", shutdown.error);
    wait_for("shutdown closes the survivor", &second, |view| {
        view.connection == ConnectionState::Lost
    })
    .await;
    let exited = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server exits after shutdown");
    assert!(exited.is_ok(), "{exited:?}");
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }
}
