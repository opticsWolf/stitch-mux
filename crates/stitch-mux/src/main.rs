//! stitch-mux command-line entry point.
//!
//! M3 surface: daemon, session/pane lifecycle, layout control, client control, and the reference GUI launcher.

use clap::{Parser, Subcommand};
use std::error::Error;
use std::process::ExitCode;

use stitch_mux_client::{ClientError, command as send_control};
use stitch_mux_core::SplitAxis;
use stitch_mux_protocol::{
    Command, CommandResponse, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, MessageKind, ViewMode,
};

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
    match send_control(endpoint, request).await {
        Ok(response) => Ok(print_response(response)),
        Err(ClientError::Endpoint(_)) => Ok(ExitCode::from(4)),
        Err(err) => Err(Box::new(err)),
    }
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
