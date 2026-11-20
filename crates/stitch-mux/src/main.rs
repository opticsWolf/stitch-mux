//! stitch-mux command-line entry point.
//!
//! CLI surface: daemon, sessions, layout, clients, automation, and GUI launcher.

use clap::{Parser, Subcommand};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use stitch_mux_client::{ClientError, command as send_control, connect};
use stitch_mux_core::{CaptureFormat, SplitAxis};
use stitch_mux_protocol::{
    Command, CommandResponse, DEFAULT_CLIENT_CAPABILITIES, Detach, Endpoint, ErrorInfo, Event,
    Hello, MessageKind, RunPlacement, SubscribeEvents, ViewMode, WaitTarget,
};

const SKILL_MD: &str = include_str!("../../../agent-skill/stitch-mux/SKILL.md");
const COMMANDS_MD: &str = include_str!("../../../agent-skill/stitch-mux/references/commands.md");

#[derive(Debug, Parser)]
#[command(
    name = "stitch-mux",
    version,
    about = "GUI-first local terminal multiplexer"
)]
struct Cli {
    /// Override the local socket path / named pipe.
    #[arg(long, global = true, env = "STITCH_MUX_SOCKET")]
    socket: Option<String>,

    /// Emit stable JSON envelopes (the default for control commands).
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run the daemon in the foreground.
    Server {
        /// Kept for compatibility; M0+ runs foreground.
        #[arg(long, default_value_t = true)]
        foreground: bool,
    },
    /// Verify that a daemon is reachable and print its HelloAck as JSON.
    Ping,
    /// Create a session whose child stays alive after clients detach.
    New {
        #[arg(short = 's', long = "session", default_value = "default")]
        session: String,
        #[arg(long, default_value_t = 80)]
        cols: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
        #[arg(short = 'c', long)]
        cwd: Option<String>,
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(short = 'e', long)]
        ensure: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        program: Vec<String>,
    },
    /// List sessions as JSON.
    Ls,
    /// List attached and control clients as JSON.
    Clients {
        #[arg(short = 's', long = "session", default_value = "default")]
        session: String,
    },
    /// Ask the daemon to detach one client by ID.
    DetachClient {
        #[arg(long, value_name = "CLIENT_ID")]
        id: u64,
    },
    /// Rename a session.
    Rename {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// Stop the daemon and detach its clients.
    Shutdown,
    /// Kill a session and all of its children.
    Kill {
        #[arg(short = 't', long = "target")]
        target: String,
    },
    /// Split a pane into two tiled panes.
    Split {
        #[arg(short = 't', long = "target", value_name = "PANE_ID")]
        target: u64,
        #[arg(long, value_enum, default_value = "columns")]
        axis: CliSplitAxis,
        #[arg(long, default_value_t = 5000)]
        ratio: u16,
    },
    /// Close one pane and collapse its split.
    Close {
        #[arg(short = 't', long = "target", value_name = "PANE_ID")]
        target: u64,
    },
    /// Zoom one pane, or pass no target to unzoom.
    Zoom {
        #[arg(short = 's', long = "session", default_value = "default")]
        session: String,
        #[arg(short = 't', long = "target", value_name = "PANE_ID")]
        target: Option<u64>,
    },
    /// Launch the disposable egui reference GUI.
    Gui {
        #[arg(short = 's', long = "session")]
        session: Option<String>,
    },
    /// Attach with the ANSI compatibility frontend (Ctrl-] d detaches).
    Attach {
        #[arg(short = 's', long = "session", default_value = "default")]
        session: String,
    },
    /// Run a program in a new pane of an existing session (M12 adds windows).
    Run {
        #[arg(short = 's', long = "session", default_value = "default")]
        session: String,
        /// Pane to split; defaults to the session's first pane.
        #[arg(short = 't', long = "target", value_name = "PANE_ID")]
        target: Option<u64>,
        #[arg(long, value_enum, default_value = "columns")]
        axis: CliSplitAxis,
        #[arg(long, default_value_t = 5000)]
        ratio: u16,
        #[arg(short = 'c', long)]
        cwd: Option<String>,
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        keep_open: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        program: Vec<String>,
    },
    /// Wait for a pane or every pane in a session to exit.
    Wait {
        #[arg(
            short = 't',
            long = "target",
            value_name = "PANE_ID",
            conflicts_with = "session"
        )]
        target: Option<u64>,
        #[arg(short = 's', long = "session", conflicts_with = "target")]
        session: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Write literal text to a pane.
    Send {
        #[arg(short = 't', long = "target", value_name = "PANE_ID")]
        target: u64,
        text: String,
    },
    /// Send named keys or raw hex bytes to a pane.
    SendKeys {
        #[arg(short = 't', long = "target", value_name = "PANE_ID")]
        target: u64,
        /// Hex bytes, for example `1b5b41` for Up.
        #[arg(long)]
        hex: Option<String>,
        /// Key names such as `enter`, `ctrl+c`, `f1`; plain tokens are text.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        keys: Vec<String>,
    },
    /// Capture bounded scrollback from a pane.
    Capture {
        #[arg(short = 't', long = "target", value_name = "PANE_ID")]
        target: u64,
        #[arg(long, default_value_t = 0)]
        start: u64,
        #[arg(long, default_value_t = 100)]
        lines: u32,
        #[arg(long, value_enum, default_value = "text")]
        format: CliCaptureFormat,
        /// Print only the captured text instead of the JSON envelope.
        #[arg(long)]
        text: bool,
    },
    /// List daemon-owned actions as JSON.
    Actions,
    /// Print the effective keymap, including sources and modes.
    GetKeymap,
    /// Invoke a named action with optional KEY=VALUE arguments.
    Invoke {
        action: String,
        #[arg(long = "arg", value_name = "KEY=VALUE")]
        args: Vec<String>,
        #[arg(short = 't', long = "target", value_name = "PANE_ID")]
        target: Option<u64>,
    },
    /// Reload JSON key bindings without restarting the daemon.
    ReloadKeymap {
        #[arg(long)]
        path: Option<String>,
    },
    /// Install the local-only agent skill into user-level skill directories.
    SkillInstall {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long, default_value = "claude,codex,opencode,pi")]
        targets: String,
    },
    /// Follow the bounded local event stream as JSON lines.
    Events {
        #[arg(short = 's', long = "session")]
        session: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CliCaptureFormat {
    Text,
    Json,
    Cells,
    Vt,
    Html,
}

impl From<CliCaptureFormat> for CaptureFormat {
    fn from(value: CliCaptureFormat) -> Self {
        match value {
            CliCaptureFormat::Text => Self::Text,
            CliCaptureFormat::Json => Self::Json,
            CliCaptureFormat::Cells => Self::Cells,
            CliCaptureFormat::Vt => Self::Vt,
            CliCaptureFormat::Html => Self::Html,
        }
    }
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CliSplitAxis {
    Columns,
    Rows,
}

impl From<CliSplitAxis> for SplitAxis {
    fn from(value: CliSplitAxis) -> Self {
        match value {
            CliSplitAxis::Columns => Self::Columns,
            CliSplitAxis::Rows => Self::Rows,
        }
    }
}

fn endpoint(cli: &Cli) -> Result<Endpoint, Box<dyn Error>> {
    match &cli.socket {
        Some(value) => Ok(Endpoint::from_override(value)),
        None => Ok(Endpoint::default_endpoint()?),
    }
}

fn hello(view_mode: ViewMode) -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: format!("stitch-mux {}", env!("CARGO_PKG_VERSION")),
        client_instance: format!("cli-{}", std::process::id()),
        view_mode,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

fn parse_env(entries: &[String]) -> Result<Option<Vec<(String, String)>>, String> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut parsed = Vec::with_capacity(entries.len());
    for entry in entries {
        let (key, value) = entry
            .split_once('=')
            .ok_or_else(|| format!("invalid --env {entry:?}; expected KEY=VALUE"))?;
        parsed.push((key.to_owned(), value.to_owned()));
    }
    Ok(Some(parsed))
}

fn print_response(response: CommandResponse) -> ExitCode {
    if !response.ok {
        let message = response
            .error
            .map(|error| format!("{:?}: {}", error.code, error.message))
            .unwrap_or_else(|| "command failed".into());
        eprintln!("stitch-mux: {message}");
        return ExitCode::from(1);
    }
    match serde_json::to_string(&response) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("stitch-mux: {err}");
            eprintln!(
                "stitch-mux: log: {}",
                stitch_mux_protocol::log_path().display()
            );
            ExitCode::from(1)
        }
    }
}

async fn send_command(endpoint: &Endpoint, request: Command) -> Result<ExitCode, Box<dyn Error>> {
    match send_command_response(endpoint, request).await {
        Ok(response) => Ok(print_response(response)),
        Err(code) => Ok(code),
    }
}

async fn send_command_response(
    endpoint: &Endpoint,
    request: Command,
) -> Result<CommandResponse, ExitCode> {
    match send_control(endpoint, request).await {
        Ok(response) => Ok(response),
        Err(ClientError::Endpoint(_)) => Err(ExitCode::from(4)),
        Err(err) => {
            eprintln!("stitch-mux: {err}");
            Err(ExitCode::from(1))
        }
    }
}

fn parse_action_args(entries: &[String]) -> Result<Option<serde_json::Value>, Box<dyn Error>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut map = serde_json::Map::new();
    for entry in entries {
        let (key, raw) = entry
            .split_once('=')
            .ok_or_else(|| CliError(format!("invalid --arg {entry:?}; expected KEY=VALUE")))?;
        let value = match raw {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            "null" => serde_json::Value::Null,
            _ if raw.starts_with('{') || raw.starts_with('[') => serde_json::from_str(raw)?,
            _ if raw.parse::<i64>().is_ok() => serde_json::json!(raw.parse::<i64>()?),
            _ if raw.parse::<u64>().is_ok() => serde_json::json!(raw.parse::<u64>()?),
            _ => serde_json::Value::String(raw.to_owned()),
        };
        map.insert(key.to_owned(), value);
    }
    Ok(Some(serde_json::Value::Object(map)))
}

fn child_exit_code(response: &CommandResponse) -> ExitCode {
    let code = response
        .run
        .as_ref()
        .map(|run| run.exit_code)
        .or_else(|| response.wait.as_ref().map(|wait| wait.exit_code))
        .flatten();
    match code {
        Some(code) if (0..=255).contains(&code) => ExitCode::from(code as u8),
        Some(code) => {
            eprintln!("stitch-mux: child exit code {code} does not fit an ExitCode");
            ExitCode::from(1)
        }
        None => ExitCode::SUCCESS,
    }
}

async fn first_pane(endpoint: &Endpoint, session: &str) -> Result<u64, Box<dyn Error>> {
    let response = send_command_response(endpoint, Command::ListSessions)
        .await
        .map_err(|code| CliError(format!("list sessions failed with exit code {code:?}")))?;
    let found = response
        .sessions
        .into_iter()
        .find(|info| info.name == session)
        .and_then(|info| info.panes.into_iter().next())
        .map(|pane| pane.id);
    found.ok_or_else(|| {
        Box::new(CliError(format!("session {session:?} has no panes"))) as Box<dyn Error>
    })
}

fn parse_hex(value: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let clean: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err(Box::new(CliError(
            "--hex needs an even number of digits".into(),
        )));
    }
    (0..clean.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&clean[index..index + 2], 16).map_err(|err| {
                Box::new(CliError(format!("invalid --hex byte: {err}"))) as Box<dyn Error>
            })
        })
        .collect()
}

fn encode_keys(tokens: &[String]) -> Result<Vec<u8>, Box<dyn Error>> {
    use stitch_pty::{KeyCode, KeyEvent, KeyboardFlags, Modifiers, encode_key};

    let mut bytes = Vec::new();
    for token in tokens {
        let (modifier_names, name) = token.rsplit_once('+').unwrap_or(("", token.as_str()));
        let mut modifiers = Modifiers::empty();
        for part in modifier_names.split('+').filter(|part| !part.is_empty()) {
            let flag = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => Modifiers::CTRL,
                "alt" => Modifiers::ALT,
                "shift" => Modifiers::SHIFT,
                "super" | "cmd" => Modifiers::SUPER,
                other => {
                    return Err(Box::new(CliError(format!(
                        "unknown modifier {other:?}; expected ctrl/alt/shift/super"
                    ))));
                }
            };
            modifiers |= flag;
        }
        let code =
            KeyCode::from_name(name).ok_or_else(|| CliError(format!("unknown key {name:?}")))?;
        bytes.extend(encode_key(
            &KeyEvent::new(code, modifiers),
            KeyboardFlags::empty(),
        ));
    }
    Ok(bytes)
}

async fn run_events(
    endpoint: &Endpoint,
    session: Option<String>,
    all: bool,
    timeout_ms: Option<u64>,
) -> Result<ExitCode, Box<dyn Error>> {
    let (mut connection, _ack) = connect(
        endpoint,
        Hello {
            protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
            client_name: format!("stitch-mux {}", env!("CARGO_PKG_VERSION")),
            client_instance: format!("events-{}", std::process::id()),
            view_mode: ViewMode::Structured,
            cols: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
            capabilities: DEFAULT_CLIENT_CAPABILITIES,
        },
    )
    .await
    .map_err(|err| {
        if matches!(err, ClientError::Endpoint(_)) {
            Box::new(CliError("daemon is not reachable".into())) as Box<dyn Error>
        } else {
            Box::new(err) as Box<dyn Error>
        }
    })?;
    connection
        .send(
            MessageKind::SubscribeEvents,
            &SubscribeEvents {
                session,
                all,
                since_seq: None,
            },
        )
        .await?;

    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    loop {
        let frame = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(remaining, connection.recv_frame()).await {
                    Ok(frame) => frame,
                    Err(_) => break,
                }
            }
            None => {
                let frame = tokio::select! {
                    frame = connection.recv_frame() => frame,
                    _ = tokio::signal::ctrl_c() => break,
                };
                frame
            }
        };
        let frame = frame?;
        match frame.kind {
            MessageKind::Event => {
                let event: Event = stitch_mux_protocol::decode_message(&frame)?;
                println!("{}", serde_json::to_string(&event)?);
            }
            MessageKind::Error => {
                let error: ErrorInfo = stitch_mux_protocol::decode_message(&frame)?;
                eprintln!("stitch-mux: {:?}: {}", error.code, error.message);
                let _ = connection
                    .send(
                        MessageKind::Detach,
                        &Detach {
                            reason: "events".into(),
                        },
                    )
                    .await;
                return Ok(ExitCode::from(1));
            }
            other => {
                eprintln!("stitch-mux: unexpected event-stream frame {other:?}");
                return Ok(ExitCode::from(1));
            }
        }
    }
    let _ = connection
        .send(
            MessageKind::Detach,
            &Detach {
                reason: "events".into(),
            },
        )
        .await;
    let _ = connection.shutdown().await;
    Ok(ExitCode::SUCCESS)
}

fn write_if_changed(path: &Path, content: &str) -> Result<(), Box<dyn Error>> {
    if fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn install_skill(root: Option<PathBuf>, targets: &str) -> Result<ExitCode, Box<dyn Error>> {
    let root = match root {
        Some(root) => root,
        None => PathBuf::from(
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .ok_or_else(|| CliError("cannot determine home directory".into()))?,
        ),
    };
    for target in targets.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let relative = match target {
            "claude" => ".claude/skills/stitch-mux",
            "codex" => ".codex/skills/stitch-mux",
            "opencode" => ".config/opencode/skills/stitch-mux",
            "pi" => ".agents/skills/stitch-mux",
            other => {
                return Err(Box::new(CliError(format!(
                    "unknown skill target {other:?}"
                ))));
            }
        };
        let dir = root.join(relative);
        write_if_changed(&dir.join("SKILL.md"), SKILL_MD)?;
        write_if_changed(&dir.join("references/commands.md"), COMMANDS_MD)?;
        println!("installed stitch-mux skill: {}", dir.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let _ = stitch_mux_protocol::init_logging();
    if matches!(cli.command, Commands::Gui { .. }) {
        // winit requires the GUI event loop on the main thread, and eframe
        // blocks that thread, so no Tokio runtime may own it.
        return finish(run_gui(&cli));
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return finish(Err(Box::new(err))),
    };
    finish(runtime.block_on(run(&cli)))
}

fn finish(result: Result<ExitCode, Box<dyn Error>>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("stitch-mux: {err}");
            ExitCode::from(1)
        }
    }
}

fn run_gui(cli: &Cli) -> Result<ExitCode, Box<dyn Error>> {
    let Commands::Gui { session } = &cli.command else {
        return Ok(ExitCode::from(2));
    };
    stitch_mux_gui_egui::run(endpoint(cli)?, session.clone())?;
    Ok(ExitCode::SUCCESS)
}

async fn run(cli: &Cli) -> Result<ExitCode, Box<dyn Error>> {
    let endpoint = endpoint(cli)?;
    match &cli.command {
        Commands::Server { foreground } => {
            tracing::info!(endpoint = %endpoint, "daemon starting");
            if !foreground {
                eprintln!("stitch-mux: M1 runs the daemon in the foreground");
            }
            stitch_mux_server::run_foreground(endpoint).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Ping => {
            let (mut connection, ack) =
                match stitch_mux_client::connect(&endpoint, hello(ViewMode::Structured)).await {
                    Ok(result) => result,
                    Err(ClientError::Endpoint(_)) => return Ok(ExitCode::from(4)),
                    Err(err) => return Err(Box::new(err)),
                };
            println!("{}", serde_json::to_string(&ack)?);
            connection
                .send(
                    MessageKind::Detach,
                    &stitch_mux_protocol::Detach {
                        reason: "cli".into(),
                    },
                )
                .await?;
            connection.shutdown().await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::New {
            session,
            cols,
            rows,
            cwd,
            env,
            ensure,
            program,
        } => {
            let env = parse_env(env).map_err(CliError)?;
            let command = if *ensure {
                Command::EnsureSession {
                    name: session.clone(),
                    program: program[0].clone(),
                    args: program[1..].to_vec(),
                    cwd: cwd.clone(),
                    env,
                    cols: *cols,
                    rows: *rows,
                }
            } else {
                Command::NewSession {
                    name: session.clone(),
                    program: program[0].clone(),
                    args: program[1..].to_vec(),
                    cwd: cwd.clone(),
                    env,
                    cols: *cols,
                    rows: *rows,
                }
            };
            send_command(&endpoint, command).await
        }
        Commands::Ls => send_command(&endpoint, Command::ListSessions).await,
        Commands::Clients { session } => {
            send_command(
                &endpoint,
                Command::ListClients {
                    session: session.clone(),
                },
            )
            .await
        }
        Commands::DetachClient { id } => {
            send_command(&endpoint, Command::DetachClient { client_id: *id }).await
        }
        Commands::Rename { from, to } => {
            send_command(
                &endpoint,
                Command::RenameSession {
                    name: from.clone(),
                    new_name: to.clone(),
                },
            )
            .await
        }
        Commands::Shutdown => send_command(&endpoint, Command::Shutdown).await,
        Commands::Kill { target } => {
            send_command(
                &endpoint,
                Command::KillSession {
                    name: target.clone(),
                },
            )
            .await
        }
        Commands::Split {
            target,
            axis,
            ratio,
        } => {
            send_command(
                &endpoint,
                Command::SplitPane {
                    pane_id: *target,
                    axis: (*axis).into(),
                    ratio_bps: *ratio,
                },
            )
            .await
        }
        Commands::Close { target } => {
            send_command(&endpoint, Command::ClosePane { pane_id: *target }).await
        }
        Commands::Zoom { session, target } => {
            send_command(
                &endpoint,
                Command::ZoomPane {
                    session: session.clone(),
                    pane_id: *target,
                },
            )
            .await
        }
        Commands::Attach { session } => {
            stitch_mux_terminal::run(endpoint, session).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Run {
            session,
            target,
            axis,
            ratio,
            cwd,
            env,
            label,
            wait,
            keep_open,
            program,
        } => {
            let env = parse_env(env).map_err(CliError)?;
            let pane_id = match target {
                Some(id) => *id,
                None => first_pane(&endpoint, session).await?,
            };
            let response = match send_command_response(
                &endpoint,
                Command::Run {
                    session: session.clone(),
                    program: program[0].clone(),
                    args: program[1..].to_vec(),
                    cwd: cwd.clone(),
                    env,
                    placement: RunPlacement::Split {
                        pane_id,
                        axis: (*axis).into(),
                        ratio_bps: *ratio,
                    },
                    label: label.clone(),
                    wait: *wait,
                    keep_open: *keep_open,
                },
            )
            .await
            {
                Ok(response) => response,
                Err(code) => return Ok(code),
            };
            let status = print_response(response.clone());
            if status == ExitCode::SUCCESS && *wait {
                Ok(child_exit_code(&response))
            } else {
                Ok(status)
            }
        }
        Commands::Wait {
            target,
            session,
            timeout_ms,
        } => {
            let target = match (target, session) {
                (Some(id), _) => WaitTarget::Pane { pane_id: *id },
                (None, Some(name)) => WaitTarget::Session { name: name.clone() },
                (None, None) => {
                    return Err(Box::new(CliError(
                        "wait needs --target PANE_ID or --session NAME".into(),
                    )));
                }
            };
            let response = match send_command_response(
                &endpoint,
                Command::Wait {
                    target,
                    timeout_ms: *timeout_ms,
                },
            )
            .await
            {
                Ok(response) => response,
                Err(code) => return Ok(code),
            };
            let status = print_response(response.clone());
            if status == ExitCode::SUCCESS
                && response.wait.as_ref().is_some_and(|wait| !wait.timed_out)
            {
                Ok(child_exit_code(&response))
            } else {
                Ok(status)
            }
        }
        Commands::Send { target, text } => {
            send_command(
                &endpoint,
                Command::SendText {
                    pane_id: *target,
                    text: text.clone(),
                },
            )
            .await
        }
        Commands::SendKeys { target, hex, keys } => {
            let mut bytes = match hex {
                Some(value) => parse_hex(value)?,
                None => Vec::new(),
            };
            bytes.extend(encode_keys(keys)?);
            if bytes.is_empty() {
                return Err(Box::new(CliError(
                    "send-keys needs --hex or at least one key".into(),
                )));
            }
            send_command(
                &endpoint,
                Command::SendKeys {
                    pane_id: *target,
                    bytes,
                },
            )
            .await
        }
        Commands::Capture {
            target,
            start,
            lines,
            format,
            text,
        } => {
            let response = match send_command_response(
                &endpoint,
                Command::Capture {
                    pane_id: *target,
                    start: *start,
                    lines: *lines,
                    format: (*format).into(),
                },
            )
            .await
            {
                Ok(response) => response,
                Err(code) => return Ok(code),
            };
            if *text && response.ok {
                if let Some(body) = response.capture.and_then(|capture| capture.text) {
                    println!("{body}");
                }
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(print_response(response))
            }
        }
        Commands::Actions => send_command(&endpoint, Command::ListActions).await,
        Commands::GetKeymap => send_command(&endpoint, Command::GetKeymap).await,
        Commands::Invoke {
            action,
            args,
            target,
        } => {
            let args = parse_action_args(args)?;
            send_command(
                &endpoint,
                Command::InvokeAction {
                    action: action.clone(),
                    args,
                    focus_pane_id: *target,
                },
            )
            .await
        }
        Commands::ReloadKeymap { path } => {
            send_command(&endpoint, Command::ReloadKeymap { path: path.clone() }).await
        }
        Commands::SkillInstall { root, targets } => install_skill(root.clone(), targets),
        Commands::Events {
            session,
            all,
            timeout_ms,
        } => run_events(&endpoint, session.clone(), *all, *timeout_ms).await,
        Commands::Gui { .. } => Ok(ExitCode::from(2)),
    }
}

#[derive(Debug)]
struct CliError(String);

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for CliError {}
