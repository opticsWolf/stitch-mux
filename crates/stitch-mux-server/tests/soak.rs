//! M6 detach/reattach and multi-client focus soak over real endpoints and
//! real children.
//!
//! The ignored test runs in short form in the CI stress job. Set
//! `STITCH_MUX_SOAK_MS=1800000` for the 30-minute v0.1 acceptance soak: it
//! keeps cycling until the deadline instead of stopping after the short run.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use stitch_mux_client::{ClientSession, ConnectionState, Outbound, SessionView, command};
use stitch_mux_core::{CaptureFormat, SplitAxis};
use stitch_mux_protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, ViewMode,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    // macOS caps Unix socket paths at SUN_LEN; keep the temp filename short.
    let unique = format!("m6{label}{}", nanos() % 1_000_000_000);
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

fn hello(label: &str, cols: u16, rows: u16) -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: format!("m6-{label}"),
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
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if predicate(&session.view()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let view = session.view();
        panic!(
            "{label} within timeout (state={:?}, error={:?}, panes={})",
            view.connection,
            view.error,
            view.panes.len()
        )
    });
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

async fn send_until_captured(
    client: &ClientSession,
    endpoint: &Endpoint,
    pane_id: u64,
    text: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_send = Instant::now() - Duration::from_secs(2);
    while Instant::now() < deadline {
        if captured_text(endpoint, pane_id).await.contains(text) {
            return;
        }
        if last_send.elapsed() >= Duration::from_secs(2) {
            // A freshly spawned shell can swallow the first line before its
            // line discipline is ready.
            client.send(Outbound::Input(InputEvent::Text(format!("{text}\n"))));
            last_send = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("pane {pane_id} did not echo {text}");
}

async fn wait_for_detached(endpoint: &Endpoint, expected_panes: u32) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let listed = command(endpoint, Command::ListSessions)
                .await
                .expect("list sessions");
            if listed.ok
                && listed.sessions.len() == 1
                && listed.sessions[0].attached_clients == 0
                && listed.sessions[0].pane_count == expected_panes
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("daemon returned to detached state");
}

fn soak_deadline() -> Option<Duration> {
    std::env::var("STITCH_MUX_SOAK_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
}

#[ignore = "M6 soak; CI runs the short form, STITCH_MUX_SOAK_MS enables the 30-minute run"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_crash_and_clean_detach_keep_children_and_layout() {
    let (endpoint, socket) = test_endpoint("soak");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "soak".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 100,
            rows: 30,
        },
    )
    .await
    .expect("new session");
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
    .expect("split pane");
    assert!(split.ok, "{:?}", split.error);
    let second_pane = split
        .layout
        .expect("layout")
        .panes
        .iter()
        .map(|geometry| geometry.pane_id)
        .find(|id| *id != first_pane)
        .expect("second pane");
    let panes = [first_pane, second_pane];

    let deadline = soak_deadline().map(|duration| Instant::now() + duration);
    let started = Instant::now();
    let mut cycle = 0_u64;
    loop {
        cycle += 1;
        let marker = format!("M6-SOAK-{cycle}");
        let client =
            ClientSession::attach(&endpoint, "soak", hello(&format!("cycle{cycle}"), 80, 24))
                .await
                .expect("attach");
        for pane_id in panes {
            wait_for("snapshot after attach", &client, |view| {
                view.connection == ConnectionState::Connected && view.pane(pane_id).is_some()
            })
            .await;
        }
        let target = panes[cycle as usize % panes.len()];
        client.send(Outbound::Focus(target));
        wait_for("focus", &client, |view| view.focus == Some(target)).await;
        send_until_captured(&client, &endpoint, target, &marker).await;

        // Every third cycle simulates a process kill: no Detach frame, the
        // socket just goes away. The rest use the graceful path.
        if cycle.is_multiple_of(3) {
            client.abort().await;
        } else {
            client.detach().await;
        }
        wait_for_detached(&endpoint, 2).await;
        assert!(
            !server.is_finished(),
            "daemon died during soak cycle {cycle}"
        );

        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                break;
            }
        } else if cycle >= 12 {
            break;
        }
    }
    assert!(
        deadline.is_some() || cycle == 12,
        "short soak must run exactly 12 cycles"
    );

    // The last child is still alive and every pane kept its identity.
    let final_client = ClientSession::attach(&endpoint, "soak", hello("final", 80, 24))
        .await
        .expect("final attach");
    for pane_id in panes {
        wait_for("final snapshot", &final_client, |view| {
            view.pane(pane_id).is_some()
        })
        .await;
    }
    let last_marker = format!("M6-SOAK-{cycle}");
    let text = captured_text(&endpoint, panes[cycle as usize % panes.len()]).await;
    assert!(text.contains(&last_marker), "final child output: {text:?}");
    final_client.detach().await;
    wait_for_detached(&endpoint, 2).await;

    let killed = command(
        &endpoint,
        Command::KillSession {
            name: "soak".into(),
        },
    )
    .await
    .expect("kill session");
    assert!(killed.ok, "{:?}", killed.error);
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }
    eprintln!("soak completed {cycle} cycles in {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_clients_keep_independent_focus_under_repeated_switching() {
    let (endpoint, socket) = test_endpoint("focus");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let (program, args) = shell();
    let created = command(
        &endpoint,
        Command::NewSession {
            name: "focus".into(),
            program: program.into(),
            args,
            cwd: None,
            env: None,
            cols: 100,
            rows: 30,
        },
    )
    .await
    .expect("new session");
    assert!(created.ok, "{:?}", created.error);
    let first_pane = created.sessions[0].panes[0].id;
    let mut panes = vec![first_pane];
    for _ in 0..2 {
        let split = command(
            &endpoint,
            Command::SplitPane {
                pane_id: *panes.last().expect("pane"),
                axis: SplitAxis::Columns,
                ratio_bps: 5000,
            },
        )
        .await
        .expect("split pane");
        assert!(split.ok, "{:?}", split.error);
        let layout = split.layout.expect("layout");
        for geometry in layout.panes {
            if !panes.contains(&geometry.pane_id) {
                panes.push(geometry.pane_id);
            }
        }
    }
    assert_eq!(panes.len(), 3);

    let clients = vec![
        ClientSession::attach(&endpoint, "focus", hello("a", 80, 24))
            .await
            .expect("client a"),
        ClientSession::attach(&endpoint, "focus", hello("b", 80, 24))
            .await
            .expect("client b"),
        ClientSession::attach(&endpoint, "focus", hello("c", 80, 24))
            .await
            .expect("client c"),
    ];
    for client in &clients {
        wait_for("three panes", client, |view| {
            view.connection == ConnectionState::Connected
                && panes.iter().all(|pane_id| view.pane(*pane_id).is_some())
        })
        .await;
    }

    let mut expected = [panes[0], panes[1], panes[2]];
    for round in 0..20 {
        for (index, client) in clients.iter().enumerate() {
            expected[index] = panes[(index + round) % panes.len()];
            client.send(Outbound::Focus(expected[index]));
        }
        for (index, client) in clients.iter().enumerate() {
            let target = expected[index];
            wait_for("per-client focus after switch", client, |view| {
                view.focus == Some(target)
            })
            .await;
        }
    }

    let listed = command(
        &endpoint,
        Command::ListClients {
            session: "focus".into(),
        },
    )
    .await
    .expect("list clients");
    assert!(listed.ok, "{:?}", listed.error);
    for (index, name) in ["m6-a", "m6-b", "m6-c"].iter().enumerate() {
        let info = listed
            .clients
            .iter()
            .find(|client| client.name == *name)
            .expect("client in registry");
        assert_eq!(
            info.focus,
            Some(expected[index]),
            "registry focus for {name}"
        );
    }

    for client in clients {
        client.detach().await;
    }
    wait_for_detached(&endpoint, 3).await;
    let killed = command(
        &endpoint,
        Command::KillSession {
            name: "focus".into(),
        },
    )
    .await
    .expect("kill session");
    assert!(killed.ok, "{:?}", killed.error);
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }
}
