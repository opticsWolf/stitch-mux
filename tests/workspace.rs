//! M12: windows and floating layers are daemon-owned and deterministic.

mod common;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stitch_mux::client::{ClientSession, command};
use stitch_mux::core::{FLOAT_BPS, FloatBounds, LayoutNode, PanePlacement, SplitAxis};
use stitch_mux::protocol::{
    ClaimMode, Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, ErrorCode, Hello, PROTOCOL_VERSION,
    ViewMode, WindowFocus,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "m12{label}{}",
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

fn bounds(x: u16, y: u16, w: u16, h: u16) -> FloatBounds {
    let bps = |percent: u16| (u32::from(percent) * FLOAT_BPS / 100) as u16;
    FloatBounds::new(bps(x), bps(y), bps(w), bps(h))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_windows_share_a_session_with_independent_tiled_layouts() {
    let (endpoint, socket) = test_endpoint("windows");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux::server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "workspace".into(),
            program: common::fixture_bin("exit_fixture"),
            args: vec!["--hold".into()],
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("new session");
    assert!(created.ok, "{:?}", created.error);
    let first_pane = created.sessions[0].panes[0].id;

    let created_window = command(
        &endpoint,
        Command::CreateWindow {
            session: "workspace".into(),
            name: Some("build".into()),
        },
    )
    .await
    .expect("create window");
    assert!(created_window.ok, "{:?}", created_window.error);
    let window_id = created_window
        .layout
        .as_ref()
        .and_then(|layout| layout.windows.iter().find(|window| window.name == "build"))
        .map(|window| window.id)
        .expect("new window");

    let run = command(
        &endpoint,
        Command::Run {
            session: "workspace".into(),
            program: common::fixture_bin("exit_fixture"),
            args: vec!["--hold".into()],
            cwd: None,
            env: None,
            placement: stitch_mux::protocol::RunPlacement::Window { window_id },
            label: None,
            wait: false,
            keep_open: true,
        },
    )
    .await
    .expect("run in window");
    assert!(run.ok, "{:?}", run.error);
    assert_eq!(run.run.expect("run result").window_id, window_id);
    let second_pane = run.run.expect("run result").pane_id;

    let split = command(
        &endpoint,
        Command::SplitPane {
            pane_id: second_pane,
            axis: SplitAxis::Rows,
            ratio_bps: 5000,
        },
    )
    .await
    .expect("split in second window");
    assert!(split.ok, "{:?}", split.error);
    let layout = split.layout.expect("layout");
    assert_eq!(layout.windows.len(), 2, "both windows must be projected");
    let first = layout.windows.iter().find(|window| window.id == 1).unwrap();
    let second = layout
        .windows
        .iter()
        .find(|window| window.id == window_id)
        .unwrap();
    assert!(matches!(first.root, LayoutNode::Leaf { .. }));
    assert!(matches!(second.root, LayoutNode::Split { .. }));
    assert_eq!(first.panes.len(), 1);
    assert_eq!(second.panes.len(), 2);
    assert_eq!(
        layout.active_window_id,
        Some(1),
        "creating a window must not steal focus"
    );
    assert_eq!(first.panes.len(), layout.panes.len(), "active projection");
    assert!(!layout.panes.iter().any(|pane| pane.pane_id == second_pane));

    let focused = command(
        &endpoint,
        Command::FocusWindow {
            target: WindowFocus::Next,
        },
    )
    .await
    .expect("focus next");
    assert!(focused.ok, "{:?}", focused.error);
    let layout = focused.layout.expect("layout");
    assert_eq!(layout.active_window_id, Some(window_id));
    assert_eq!(layout.panes.len(), 2);
    assert!(layout.panes.iter().any(|pane| pane.pane_id == second_pane));
    assert!(!layout.panes.iter().any(|pane| pane.pane_id == first_pane));

    let attached = ClientSession::attach(
        &endpoint,
        "workspace",
        Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "workspace test".into(),
            client_instance: "workspace-test".into(),
            view_mode: ViewMode::Structured,
            cols: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
            capabilities: DEFAULT_CLIENT_CAPABILITIES,
        },
    )
    .await
    .expect("attach separate client");
    let mut view = attached.subscribe();

    let renamed = command(
        &endpoint,
        Command::RenameWindow {
            window_id,
            name: "renamed".into(),
        },
    )
    .await
    .expect("rename window");
    assert!(renamed.ok, "{:?}", renamed.error);
    tokio::time::timeout(
        Duration::from_secs(5),
        view.wait_for(|state| {
            state.layout.as_ref().is_some_and(|layout| {
                layout
                    .windows
                    .iter()
                    .any(|window| window.id == window_id && window.name == "renamed")
            })
        }),
    )
    .await
    .expect("rename broadcast")
    .expect("client view");

    let moved = command(
        &endpoint,
        Command::MoveWindow {
            window_id,
            index: 0,
        },
    )
    .await
    .expect("move window");
    assert!(moved.ok, "{:?}", moved.error);
    assert_eq!(moved.layout.unwrap().windows[0].id, window_id);
    tokio::time::timeout(
        Duration::from_secs(5),
        view.wait_for(|state| {
            state.layout.as_ref().is_some_and(|layout| {
                layout
                    .windows
                    .first()
                    .is_some_and(|window| window.id == window_id)
            })
        }),
    )
    .await
    .expect("reorder broadcast")
    .expect("client view");
    attached.detach().await;

    let claimed = command(
        &endpoint,
        Command::ClaimPane {
            pane_id: second_pane,
            mode: ClaimMode::Take,
        },
    )
    .await
    .expect("claim");
    assert!(claimed.ok, "{:?}", claimed.error);
    let listed = command(
        &endpoint,
        Command::ListPanes {
            session: "workspace".into(),
        },
    )
    .await
    .expect("list panes");
    assert!(
        listed
            .panes
            .iter()
            .find(|pane| pane.id == second_pane)
            .unwrap()
            .window_id
            == Some(window_id)
    );

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("socket parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn floating_layers_are_bounded_raisable_and_explicitly_closed() {
    let (endpoint, socket) = test_endpoint("float");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux::server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "floating".into(),
            program: common::fixture_bin("exit_fixture"),
            args: vec!["--hold".into()],
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

    let floated = command(
        &endpoint,
        Command::FloatPane {
            pane_id,
            bounds: bounds(20, 10, 40, 50),
        },
    )
    .await
    .expect("float pane");
    assert!(floated.ok, "{:?}", floated.error);
    let layout = floated.layout.expect("layout");
    assert!(matches!(layout.windows[0].root, LayoutNode::Empty));
    assert_eq!(layout.windows[0].floating.len(), 1);
    let layer_id = layout.windows[0].floating[0].layer_id;
    assert_eq!(layout.panes.len(), 1, "floating layer is in the projection");

    let listed = command(
        &endpoint,
        Command::ListPanes {
            session: "floating".into(),
        },
    )
    .await
    .expect("list panes");
    assert!(matches!(
        listed.panes[0].placement,
        PanePlacement::Floating { layer_id: id } if id == layer_id
    ));

    let changed = command(
        &endpoint,
        Command::SetFloatingPane {
            layer_id,
            bounds: Some(bounds(10, 20, 50, 40)),
            z: Some(9),
            minimized: Some(true),
        },
    )
    .await
    .expect("set floating");
    assert!(changed.ok, "{:?}", changed.error);
    let layout = changed.layout.expect("layout");
    assert_eq!(layout.windows[0].floating[0].z, 9);
    assert!(layout.windows[0].floating[0].minimized);
    assert!(layout.panes.is_empty(), "minimized layer is not projected");

    let invalid = command(
        &endpoint,
        Command::SetFloatingPane {
            layer_id,
            bounds: Some(FloatBounds::new(1000, 0, 9500, 5000)),
            z: None,
            minimized: None,
        },
    )
    .await
    .expect("invalid bounds");
    assert!(!invalid.ok);
    assert_eq!(invalid.error.expect("error").code, ErrorCode::InvalidArgs);
    let layout = command(&endpoint, Command::ListSessions)
        .await
        .expect("sessions")
        .sessions
        .pop()
        .expect("session");
    assert_eq!(layout.window_count, 1);

    let closed = command(&endpoint, Command::ClosePane { pane_id })
        .await
        .expect("close floating");
    assert!(closed.ok, "{:?}", closed.error);
    assert!(closed.sessions.is_empty());

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("socket parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_floating_pane_can_be_tiled_without_touching_other_windows() {
    let (endpoint, socket) = test_endpoint("float-run");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux::server::serve(listener));

    let created = command(
        &endpoint,
        Command::NewSession {
            name: "float-run".into(),
            program: common::fixture_bin("exit_fixture"),
            args: vec!["--hold".into()],
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("new session");
    assert!(created.ok, "{:?}", created.error);
    let anchor = created.sessions[0].panes[0].id;

    let run = command(
        &endpoint,
        Command::Run {
            session: "float-run".into(),
            program: common::fixture_bin("exit_fixture"),
            args: vec!["--hold".into()],
            cwd: None,
            env: None,
            placement: stitch_mux::protocol::RunPlacement::Floating {
                window_id: 1,
                bounds: bounds(50, 50, 40, 40),
            },
            label: None,
            wait: false,
            keep_open: true,
        },
    )
    .await
    .expect("run floating");
    assert!(run.ok, "{:?}", run.error);
    let pane_id = run.run.expect("run result").pane_id;
    let layout = run.layout.expect("layout");
    assert_eq!(layout.windows[0].floating.len(), 1);
    assert!(matches!(layout.windows[0].root, LayoutNode::Leaf { .. }));

    let tiled = command(
        &endpoint,
        Command::TilePane {
            pane_id,
            window_id: 1,
            anchor_pane_id: Some(anchor),
            axis: SplitAxis::Columns,
            ratio_bps: 5000,
        },
    )
    .await
    .expect("tile floating pane");
    assert!(tiled.ok, "{:?}", tiled.error);
    let layout = tiled.layout.expect("layout");
    assert!(layout.windows[0].floating.is_empty());
    assert!(matches!(layout.windows[0].root, LayoutNode::Split { .. }));

    let listed = command(
        &endpoint,
        Command::ListPanes {
            session: "float-run".into(),
        },
    )
    .await
    .expect("list panes");
    assert_eq!(listed.panes.len(), 2);
    assert!(
        listed
            .panes
            .iter()
            .all(|pane| pane.placement == PanePlacement::Tiled)
    );

    let _ = tokio::time::timeout(
        Duration::from_millis(50),
        tokio::time::sleep(Duration::from_secs(1)),
    )
    .await;
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("socket parent")).await;
    }
}
