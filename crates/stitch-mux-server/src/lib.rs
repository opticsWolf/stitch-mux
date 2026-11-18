//! Local stitch-mux daemon.
//!
//! M0 scope: bind the owner-scoped endpoint, answer Hello/HelloAck, and keep
//! control connections alive without touching PTYs. Session/PTY lifecycle is
//! added in M1 behind this same boundary.

use std::collections::BTreeMap;
use std::sync::Arc;

use stitch_mux_protocol::{
    CommandRequest, CommandResponse, Connection, DEFAULT_SERVER_CAPABILITIES, Endpoint,
    EndpointError, ErrorCode, ErrorInfo, FrameError, Hello, HelloAck, MessageKind,
    PROTOCOL_VERSION, Stream, capabilities, decode_message,
};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Frame(#[from] FrameError),
}

#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub id: u64,
    pub name: String,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: BTreeMap<String, SessionRecord>,
}

impl SessionRegistry {
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }
}

pub type SharedRegistry = Arc<Mutex<SessionRegistry>>;

/// Bind the endpoint and serve connections until the process is stopped.
pub async fn run_foreground(endpoint: Endpoint) -> Result<(), ServerError> {
    let listener = endpoint.bind().await?;
    tracing::info!("stitch-mux daemon listening");
    serve(listener).await
}

/// Serve an already-bound listener. Used by integration tests and by
/// run_foreground; PTY sessions are not yet created here.
pub async fn serve(listener: stitch_mux_protocol::Listener) -> Result<(), ServerError> {
    let registry: SharedRegistry = Arc::new(Mutex::new(SessionRegistry::default()));
    loop {
        let stream = listener.accept().await?;
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, registry).await {
                tracing::debug!("client connection ended: {err}");
            }
        });
    }
}

async fn handle_connection(stream: Stream, registry: SharedRegistry) -> Result<(), ServerError> {
    let mut connection = Connection::new(stream);
    let hello: Hello = connection.recv(MessageKind::Hello).await?;
    if hello.protocol_version != PROTOCOL_VERSION {
        send_error(
            &mut connection,
            ErrorCode::ProtocolVersion,
            "unsupported protocol version",
        )
        .await?;
        return Ok(());
    }
    let required = match hello.view_mode {
        stitch_mux_protocol::ViewMode::Structured => capabilities::client::STRUCTURED_VIEW,
        stitch_mux_protocol::ViewMode::Terminal => capabilities::client::ANSI,
    };
    if hello.capabilities & required == 0 {
        send_error(
            &mut connection,
            ErrorCode::Unsupported,
            "missing view capability",
        )
        .await?;
        return Ok(());
    }
    let sessions = registry.lock().await.len() as u32;
    connection
        .send(
            MessageKind::HelloAck,
            &HelloAck {
                protocol_version: PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").to_owned(),
                capabilities: DEFAULT_SERVER_CAPABILITIES,
                sessions,
            },
        )
        .await?;

    loop {
        let frame = connection.recv_frame().await?;
        match frame.kind {
            MessageKind::Ping => {
                let nonce: u8 = decode_message(&frame).unwrap_or(1);
                connection.send(MessageKind::Pong, &nonce).await?;
            }
            MessageKind::CommandRequest => {
                let request: CommandRequest = stitch_mux_protocol::decode_message(&frame)?;
                connection
                    .send(
                        MessageKind::CommandResponse,
                        &CommandResponse {
                            request_id: request.request_id,
                            ok: false,
                            error: Some(ErrorInfo {
                                code: ErrorCode::Unsupported,
                                message: "control commands arrive in M1".into(),
                            }),
                            ..Default::default()
                        },
                    )
                    .await?;
            }
            MessageKind::Detach => break,
            other => {
                send_error(
                    &mut connection,
                    ErrorCode::Unsupported,
                    &format!("{other:?} is not handled by the M0 daemon"),
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn send_error(
    connection: &mut Connection<Stream>,
    code: ErrorCode,
    message: &str,
) -> Result<(), ServerError> {
    connection
        .send(
            MessageKind::Error,
            &ErrorInfo {
                code,
                message: message.to_owned(),
            },
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_starts_empty() {
        let registry = SessionRegistry::default();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.names().is_empty());
    }
}
