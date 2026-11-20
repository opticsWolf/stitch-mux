//! Registry/keymap commands and action execution through an attached client.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use stitch_mux_client::{ClientSession, Outbound, connect};
use stitch_mux_protocol::{
    Command, CommandRequest, CommandResponse, DEFAULT_CLIENT_CAPABILITIES, Endpoint, ErrorCode,
    Hello, MessageKind, ViewMode, capabilities, decode_message,
};

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "m9a{label}{}",
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

fn hello(capabilities: u64) -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: "actions-test".into(),
        client_instance: format!("actions-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities,
    }
}

async fn attached_command(
    client: &ClientSession,
    rx: &mut tokio::sync::watch::Receiver<stitch_mux_client::SessionView>,
    request_id: u64,
    command: Command,
) -> CommandResponse {
    client.send(Outbound::Command(CommandRequest {
        request_id,
        command,
    }));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::timeout(remaining, rx.changed())
            .await
            .expect("command response")
            .expect("view alive");
        let response = {
            let view = rx.borrow_and_update();
            view.last_command.clone()
        };
        if response
            .as_ref()
            .is_some_and(|response| response.request_id == request_id)
        {
            return response.expect("response");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_keymap_and_actions_work_over_an_attached_client() {
    let (endpoint, socket) = test_endpoint("registry");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let created = stitch_mux_client::command(
        &endpoint,
        Command::NewSession {
            name: "actions".into(),
            program: env!("CARGO_BIN_EXE_exit_fixture").into(),
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

    let client = ClientSession::attach(&endpoint, "actions", hello(DEFAULT_CLIENT_CAPABILITIES))
        .await
        .expect("attach");
    let mut rx = client.subscribe();
    client.send(Outbound::Focus(pane_id));

    let listed = attached_command(&client, &mut rx, 1, Command::ListActions).await;
    assert!(listed.ok, "{:?}", listed.error);
    assert!(
        listed
            .actions
            .iter()
            .any(|action| action.name == "pane.split")
    );
    assert!(
        listed
            .actions
            .iter()
            .all(|action| !action.name.starts_with('_'))
    );

    let keymap = attached_command(&client, &mut rx, 2, Command::GetKeymap).await;
    assert!(keymap.ok, "{:?}", keymap.error);
    let keymap = keymap.keymap.expect("keymap");
    assert!(
        keymap
            .bindings
            .iter()
            .any(|binding| { binding.keys == ["C-b", "%"] && binding.action == "pane.split" })
    );
    assert!(
        keymap
            .bindings
            .iter()
            .any(|binding| binding.mode.as_deref() == Some("resize")
                && binding.action == "mode.exit")
    );
    assert!(
        keymap
            .bindings
            .iter()
            .all(|binding| !binding.source.is_empty())
    );

    let invalid = attached_command(
        &client,
        &mut rx,
        3,
        Command::InvokeAction {
            action: "pane.split".into(),
            args: Some(serde_json::json!({"axis": "diagonal", "ratio_bps": 5000})),
        },
    )
    .await;
    assert!(!invalid.ok);
    assert_eq!(
        invalid.error.map(|error| error.code),
        Some(ErrorCode::InvalidArgs)
    );

    let split = attached_command(
        &client,
        &mut rx,
        4,
        Command::InvokeAction {
            action: "pane.split".into(),
            args: Some(serde_json::json!({"axis": "columns", "ratio_bps": 5000})),
        },
    )
    .await;
    assert!(split.ok, "{:?}", split.error);
    assert!(split.layout.expect("layout").panes.len() == 2);

    let unknown = attached_command(
        &client,
        &mut rx,
        5,
        Command::InvokeAction {
            action: "pane.nope".into(),
            args: None,
        },
    )
    .await;
    assert!(!unknown.ok);
    assert_eq!(
        unknown.error.map(|error| error.code),
        Some(ErrorCode::Unsupported)
    );

    client.detach().await;
    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_replaces_bindings_without_restarting() {
    let (endpoint, socket) = test_endpoint("reload");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let dir = std::env::temp_dir().join(format!(
        "m9keymap{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    tokio::fs::create_dir_all(&dir).await.expect("config dir");
    let config = dir.join("keymap.json");
    tokio::fs::write(
        &config,
        r#"{"bindings":[{"keys":["C-b","%"],"action":"pane.focus-left"}]}"#,
    )
    .await
    .expect("write config");

    let created = stitch_mux_client::command(
        &endpoint,
        Command::NewSession {
            name: "reload".into(),
            program: env!("CARGO_BIN_EXE_exit_fixture").into(),
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

    let client = ClientSession::attach(&endpoint, "reload", hello(DEFAULT_CLIENT_CAPABILITIES))
        .await
        .expect("attach");
    let mut rx = client.subscribe();
    client.send(Outbound::Focus(pane_id));

    let reloaded = attached_command(
        &client,
        &mut rx,
        1,
        Command::ReloadKeymap {
            path: Some(config.to_string_lossy().into_owned()),
        },
    )
    .await;
    assert!(reloaded.ok, "{:?}", reloaded.error);
    let keymap = reloaded.keymap.expect("keymap");
    let binding = keymap
        .bindings
        .iter()
        .find(|binding| binding.keys == ["C-b", "%"])
        .expect("replaced binding");
    assert_eq!(binding.action, "pane.focus-left");
    assert_eq!(binding.source, "user");

    client.detach().await;
    server.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_commands_require_the_capability() {
    let (endpoint, socket) = test_endpoint("cap");
    let listener = endpoint.bind().await.expect("bind");
    let server = tokio::spawn(stitch_mux_server::serve(listener));

    let mut connection = connect(&endpoint, hello(capabilities::client::STRUCTURED_VIEW))
        .await
        .expect("connect")
        .0;
    connection
        .send(
            MessageKind::CommandRequest,
            &CommandRequest {
                request_id: 1,
                command: Command::ListActions,
            },
        )
        .await
        .expect("request");
    let frame = tokio::time::timeout(Duration::from_secs(5), connection.recv_frame())
        .await
        .expect("typed error")
        .expect("frame");
    assert_eq!(frame.kind, MessageKind::CommandResponse);
    let response: CommandResponse = decode_message(&frame).expect("response");
    assert!(!response.ok);
    assert_eq!(
        response.error.map(|error| error.code),
        Some(ErrorCode::Unsupported)
    );

    server.abort();
    if let Some(path) = socket {
        let _ = tokio::fs::remove_dir_all(path.parent().expect("parent")).await;
    }
}
