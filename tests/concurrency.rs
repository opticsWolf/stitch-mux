//! Concurrency and identity invariants.
//!
//! These are the failure modes that only appear with more than one actor: two
//! clients mutating the layout at the same time, pane IDs colliding across
//! sessions, and the control plane fighting the attached clients for the
//! canonical size.

use std::path::PathBuf;
use std::time::Duration;

use stitch_mux::client::{ClientSession, command};
use stitch_mux::core::SplitAxis;
use stitch_mux::protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, ErrorCode, Hello, ViewMode,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!("c{label}{}", nanos() % 1_000_000_000);
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

fn hello(instance: &str) -> Hello {
    Hello {
        protocol_version: stitch_mux::protocol::PROTOCOL_VERSION,
        client_name: "concurrency-test".into(),
        client_instance: instance.into(),
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

async fn create(endpoint: &Endpoint, name: &str) -> u64 {
    let (program, args) = shell();
    let response = command(
        endpoint,
        Command::NewSession {
            name: name.into(),
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
    assert!(response.ok, "{:?}", response.error);
    response.sessions[0].panes[0].id
}

async fn pane_ids(endpoint: &Endpoint, session: &str) -> Vec<u64> {
    let response = command(
        endpoint,
        Command::ListPanes {
            session: session.into(),
        },
    )
    .await
    .expect("list panes");
    assert!(response.ok, "{:?}", response.error);
    response.panes.into_iter().map(|pane| pane.id).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_splits_keep_every_pane() {
    let (endpoint, socket_path) = test_endpoint("split");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux::server::serve(listener).await });
    let root = create(&endpoint, "race").await;

    // Both splits rebuild from the same base pane. Without serialising the
    // read-edit-write step, one of the two new leaves is silently dropped from
    // the tree while its pane stays alive.
    let left = command(
        &endpoint,
        Command::SplitPane {
            pane_id: root,
            axis: SplitAxis::Columns,
            ratio_bps: 5000,
        },
    );
    let bottom = command(
        &endpoint,
        Command::SplitPane {
            pane_id: root,
            axis: SplitAxis::Rows,
            ratio_bps: 5000,
        },
    );
    let (left, bottom) = tokio::join!(left, bottom);
    assert!(left.expect("left split").ok);
    assert!(bottom.expect("bottom split").ok);

    let ids = pane_ids(&endpoint, "race").await;
    assert_eq!(ids.len(), 3, "every pane must stay in the layout: {ids:?}");
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 3, "pane IDs must be unique: {ids:?}");

    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pane_ids_are_unique_across_sessions() {
    let (endpoint, socket_path) = test_endpoint("ids");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux::server::serve(listener).await });

    let first = create(&endpoint, "one").await;
    let second = create(&endpoint, "two").await;
    assert_ne!(
        first, second,
        "pane IDs from different sessions must not collide"
    );
    // Session A keeps splitting while session B exists; B's next split must
    // still allocate a fresh ID instead of reusing A's range.
    let one = pane_ids(&endpoint, "one").await[0];
    let response = command(
        &endpoint,
        Command::SplitPane {
            pane_id: one,
            axis: SplitAxis::Columns,
            ratio_bps: 5000,
        },
    )
    .await
    .expect("split");
    assert!(response.ok, "{:?}", response.error);
    let mut all = pane_ids(&endpoint, "one").await;
    all.extend(pane_ids(&endpoint, "two").await);
    let mut unique = all.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        all.len(),
        "pane IDs must never repeat: {all:?}"
    );

    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_resize_defers_to_attached_clients() {
    let (endpoint, socket_path) = test_endpoint("resize");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux::server::serve(listener).await });
    create(&endpoint, "sized").await;

    let session = ClientSession::attach(&endpoint, "sized", hello("sized-1"))
        .await
        .expect("attach");
    // The client's 80x24 viewport is canonical while it is attached.
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut receiver = session.subscribe();
        loop {
            if receiver.borrow_and_update().size == stitch_mux::core::Size::new(80, 24) {
                return;
            }
            if receiver.changed().await.is_err() {
                panic!("connection closed");
            }
        }
    })
    .await
    .expect("canonical client size");

    let rejected = command(
        &endpoint,
        Command::ResizeSession {
            session: "sized".into(),
            cols: 120,
            rows: 40,
        },
    )
    .await
    .expect("resize response");
    assert!(!rejected.ok, "a control resize must not override clients");
    assert_eq!(
        rejected.error.as_ref().map(|error| error.code),
        Some(ErrorCode::Unsupported)
    );

    session.detach().await;
    // Wait until the daemon has really unregistered the client; detach is a
    // frame, not a round trip.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let listed = command(&endpoint, Command::ListSessions)
                .await
                .expect("list sessions");
            if listed.sessions[0].attached_clients == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("client unregistered");
    // Without a viewport client the control plane owns the size, which keeps
    // headless scripts able to size a session.
    let accepted = command(
        &endpoint,
        Command::ResizeSession {
            session: "sized".into(),
            cols: 120,
            rows: 40,
        },
    )
    .await
    .expect("resize response");
    assert!(accepted.ok, "{:?}", accepted.error);
    assert_eq!(accepted.sessions[0].cols, 120);
    assert_eq!(accepted.sessions[0].rows, 40);
    // One border cell per side, so the pane screen is two cells smaller.
    assert_eq!(accepted.sessions[0].panes[0].cols, 118);
    assert_eq!(accepted.sessions[0].panes[0].rows, 38);

    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_attaches_reach_the_daemon() {
    const CLIENTS: usize = 4;
    let (endpoint, socket_path) = test_endpoint("crowd");
    let listener = endpoint.bind().await.expect("bind endpoint");
    let server = tokio::spawn(async move { stitch_mux::server::serve(listener).await });
    create(&endpoint, "crowd").await;

    let mut attaching = Vec::new();
    for index in 0..CLIENTS {
        let endpoint = endpoint.clone();
        attaching.push(tokio::spawn(async move {
            ClientSession::attach(&endpoint, "crowd", hello(&format!("crowd-{index}"))).await
        }));
    }
    let mut sessions = Vec::new();
    for task in attaching {
        let session = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("attach within timeout")
            .expect("attach task")
            .expect("attach");
        sessions.push(session);
    }

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let listed = command(&endpoint, Command::ListSessions)
                .await
                .expect("list sessions");
            if listed.sessions[0].attached_clients == CLIENTS as u32 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("all clients registered");

    for session in sessions {
        session.detach().await;
    }
    server.abort();
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}
