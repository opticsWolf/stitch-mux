//! stitch-mux command-line entry point.
//!
//! M0 surface: start the local daemon in the foreground and verify the
//! handshake with `ping`. Session commands arrive in M1.

use clap::{Parser, Subcommand};
use std::error::Error;
use std::process::ExitCode;

use stitch_mux_client::{ClientError, connect};
use stitch_mux_protocol::{DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, MessageKind, ViewMode};

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
    /// Run the daemon in the foreground (M0 debugging entry point).
    Server {
        /// Kept for compatibility with the documented CLI; M0 always runs foreground.
        #[arg(long, default_value_t = true)]
        foreground: bool,
    },
    /// Verify that a daemon is reachable and print its HelloAck as JSON.
    Ping,
    /// Placeholder for the M1 session list.
    Ls,
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

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("stitch-mux: {err}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode, Box<dyn Error>> {
    let cli = Cli::parse();
    let endpoint = endpoint(&cli)?;
    match cli.command {
        Commands::Server { foreground } => {
            if !foreground {
                eprintln!("stitch-mux: M0 only supports --foreground; running in foreground");
            }
            stitch_mux_server::run_foreground(endpoint).await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Ping => {
            let (mut connection, ack) = match connect(&endpoint, hello(ViewMode::Structured)).await
            {
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
        Commands::Ls => {
            eprintln!("stitch-mux: session listing arrives in M1");
            Ok(ExitCode::from(2))
        }
    }
}
