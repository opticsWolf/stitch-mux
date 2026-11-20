//! End-to-end M2 layout loop over a real local endpoint and real children.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux_client::{ClientSession, Outbound, SessionView, command};
use stitch_mux_core::{LayoutNode, SplitAxis};
use stitch_mux_protocol::{Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, ViewMode};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!("m2{label}{}", nanos() % 1_000_000_000);
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
        client_name: format!("layout-test-{label}"),
        client_instance: format!("test-{}-{}", label, std::process::id()),
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

async fn wait_for<F: Fn(&SessionView) -> bool>(label: &str, session: &ClientSession, f: F) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if f(&session.view()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
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
            format: stitch_mux_core::CaptureFormat::Text,
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

async fn wait_for_capture(label: &str, endpoint: &Endpoint, pane_id: u64, needle: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if captured_text(endpoint, pane_id).await.contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{label} within timeout"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_focus_zoom_close_and_revision_guard() {
    let (endpoint, socket_path) = test_endpoint("split");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "layout".into(),
            program: program.into(),
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
    let first = created.sessions[0].panes[0].id;

    let client = ClientSession::attach(&endpoint, "layout", hello("first"))
        .await
        .expect("attach");
    wait_for("first layout", &client, |view| view.layout.is_some()).await;

    let split = command(
        &endpoint,
        Command::SplitPane {
            pane_id: first,
            axis: SplitAxis::Columns,
            ratio_bps: 5000,
        },
    )
    .await
    .expect("split");
    assert!(split.ok, "{:?}", split.error);
    let layout = split.layout.expect("split layout");
    assert_eq!(layout.panes.len(), 2);
    let second = layout.panes[1].pane_id;
    assert_ne!(first, second);
    tokio::time::sleep(Duration::from_millis(500)).await;
    wait_for("split reaches client", &client, |view| {
        view.layout
            .as_ref()
            .is_some_and(|layout| layout.panes.len() == 2)
    })
    .await;
    let view = client.view();
    let client_layout = view.layout.as_ref().expect("client layout");
    assert_eq!(client_layout.root, layout.root);
    let total = client_layout
        .panes
        .iter()
        .map(|pane| pane.outer.cols)
        .sum::<u16>();
    assert_eq!(total, 80);
    for geometry in &client_layout.panes {
        let snapshot = view
            .pane(geometry.pane_id)
            .expect("pane snapshot")
            .snapshot
            .as_ref()
            .expect("snapshot");
        assert_eq!(
            usize::from(snapshot.cols),
            usize::from(geometry.content.cols)
        );
        assert_eq!(
            usize::from(snapshot.rows),
            usize::from(geometry.content.rows)
        );
    }
    drop(view);

    client.send(Outbound::Focus(second));
    wait_for("second pane focused", &client, |view| {
        view.focus == Some(second)
    })
    .await;
    // Explicitly focus the first pane so the routing assertion is deterministic.
    client.send(Outbound::Focus(first));
    wait_for("first pane focused", &client, |view| {
        view.focus == Some(first)
    })
    .await;
    let newline = if cfg!(windows) { "\r\n" } else { "\n" };
    client.send(Outbound::Input(stitch_mux_protocol::InputEvent::Text(
        format!("echo m2-first{newline}"),
    )));
    wait_for_capture("input reaches first pane", &endpoint, first, "m2-first").await;

    // Focus the second pane again; the first pane must stop receiving input.
    client.send(Outbound::Focus(second));
    wait_for("second pane focused again", &client, |view| {
        view.focus == Some(second)
    })
    .await;
    client.send(Outbound::Input(stitch_mux_protocol::InputEvent::Text(
        format!("echo m2-second{newline}"),
    )));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!captured_text(&endpoint, first).await.contains("m2-second"));

    let zoom = command(
        &endpoint,
        Command::ZoomPane {
            session: "layout".into(),
            pane_id: Some(second),
        },
    )
    .await
    .expect("zoom");
    assert!(zoom.ok, "{:?}", zoom.error);
    assert!(matches!(
        zoom.layout.as_ref().map(|l| &l.root),
        Some(LayoutNode::Leaf { .. })
    ));
    assert_eq!(
        zoom.layout.as_ref().and_then(|l| l.zoomed_pane),
        Some(second),
        "a zoomed layout must say which pane it shows"
    );
    wait_for("zoom reaches client", &client, |view| {
        view.layout
            .as_ref()
            .is_some_and(|layout| layout.panes.len() == 1 && layout.panes[0].pane_id == second)
    })
    .await;

    client.detach().await;
    let reattached = ClientSession::attach(&endpoint, "layout", hello("second"))
        .await
        .expect("reattach");
    tokio::time::sleep(Duration::from_millis(500)).await;
    wait_for("restored zoom", &reattached, |view| {
        view.layout
            .as_ref()
            .is_some_and(|layout| layout.panes.len() == 1)
            && view.focus == Some(second)
    })
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let current = reattached.view().layout.as_ref().expect("layout").clone();
    let stale = command(
        &endpoint,
        Command::SetLayout {
            base_revision: current.revision.saturating_sub(1),
            window_id: None,
            floating: None,
            root: current.root.clone(),
        },
    )
    .await
    .expect("stale set layout");
    assert!(!stale.ok);
    assert_eq!(
        stale.error.as_ref().map(|err| err.code),
        Some(stitch_mux_protocol::ErrorCode::Busy)
    );
    let fresh = stale.layout.expect("fresh layout");
    assert_eq!(fresh.revision, current.revision);
    assert_eq!(fresh.root, current.root);
    assert_eq!(
        reattached.view().layout.as_ref().expect("layout").root,
        current.root
    );

    let unzoom = command(
        &endpoint,
        Command::ZoomPane {
            session: "layout".into(),
            pane_id: None,
        },
    )
    .await
    .expect("unzoom");
    assert!(unzoom.ok, "{:?}", unzoom.error);
    let unzoomed = unzoom.layout.expect("unzoom layout");
    assert_eq!(unzoomed.zoomed_pane, None);
    assert_eq!(unzoomed.panes.len(), 2);
    assert!(matches!(unzoomed.root, LayoutNode::Split { .. }));
    // A fresh attach observes the unzoomed layout without depending on a push
    // racing the control command's own connection teardown.
    reattached.detach().await;
    let final_client = ClientSession::attach(&endpoint, "layout", hello("third"))
        .await
        .expect("attach after unzoom");
    wait_for("unzoomed layout after attach", &final_client, |view| {
        view.layout
            .as_ref()
            .is_some_and(|layout| layout.panes.len() == 2)
    })
    .await;

    let closed = command(&endpoint, Command::ClosePane { pane_id: second })
        .await
        .expect("close pane");
    assert!(closed.ok, "{:?}", closed.error);
    let closed_layout = closed.layout.expect("close layout");
    assert_eq!(closed_layout.panes.len(), 1);
    assert_eq!(closed_layout.panes[0].pane_id, first);
    assert_eq!(
        closed_layout.root,
        LayoutNode::leaf(stitch_mux_core::PaneId::new(first))
    );
    final_client.detach().await;
    let final_client = ClientSession::attach(&endpoint, "layout", hello("fourth"))
        .await
        .expect("attach after close");
    wait_for("closed layout after attach", &final_client, |view| {
        view.layout
            .as_ref()
            .is_some_and(|layout| layout.panes.len() == 1 && layout.panes[0].pane_id == first)
            && view.focus == Some(first)
    })
    .await;

    let _ = command(
        &endpoint,
        Command::KillSession {
            name: "layout".into(),
        },
    )
    .await;
    final_client.detach().await;
    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn impossible_layouts_are_rejected() {
    let (endpoint, socket_path) = test_endpoint("bad");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux_server::serve(listener).await });
    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "tiny".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 12,
            rows: 6,
        },
    )
    .await
    .expect("new session");
    assert!(created.ok, "{:?}", created.error);
    let first = created.sessions[0].panes[0].id;

    let bad_ratio = command(
        &endpoint,
        Command::SplitPane {
            pane_id: first,
            axis: SplitAxis::Rows,
            ratio_bps: 0,
        },
    )
    .await
    .expect("bad ratio");
    assert!(!bad_ratio.ok);

    let bad_layout = command(
        &endpoint,
        Command::SetLayout {
            base_revision: 1,
            window_id: None,
            floating: None,
            root: LayoutNode::split(
                SplitAxis::Columns,
                5000,
                LayoutNode::leaf(stitch_mux_core::PaneId::new(first)),
                LayoutNode::leaf(stitch_mux_core::PaneId::new(999_999)),
            ),
        },
    )
    .await
    .expect("bad layout");
    assert!(!bad_layout.ok);

    let listed = command(&endpoint, Command::ListSessions)
        .await
        .expect("list sessions");
    assert_eq!(listed.sessions[0].pane_count, 1);

    let _ = command(
        &endpoint,
        Command::KillSession {
            name: "tiny".into(),
        },
    )
    .await;
    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}
