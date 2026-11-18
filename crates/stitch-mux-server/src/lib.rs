//! Local stitch-mux daemon.
//!
//! M1 scope: own PTY sessions, project structured snapshots/deltas to attached
//! clients, and keep sessions alive across detach/reattach.

pub mod pane;
pub mod session;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use stitch_mux_protocol::{
    Attach, CaptureRequest, Command, CommandRequest, CommandResponse, Connection,
    DEFAULT_SERVER_CAPABILITIES, Endpoint, EndpointError, ErrorCode, ErrorInfo, Frame, FrameError,
    FrameWriter, Hello, HelloAck, InputEvent, MessageKind, PROTOCOL_VERSION, ResizeRequest, Stream,
    ViewMode, capabilities, decode_message,
};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast, mpsc};

pub use pane::{PaneError, PaneRuntime, PaneUpdate};
pub use session::{Session, SessionError, SessionRegistry};

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Frame(#[from] FrameError),
}

pub struct ServerState {
    pub registry: Mutex<SessionRegistry>,
    attached: StdMutex<BTreeMap<u64, u32>>,
    next_client_id: AtomicU64,
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            registry: Mutex::new(SessionRegistry::new()),
            attached: StdMutex::new(BTreeMap::new()),
            next_client_id: AtomicU64::new(1),
        }
    }
}

impl ServerState {
    fn next_client_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::SeqCst)
    }

    fn attach(&self, session_id: u64) -> u32 {
        let mut attached = self.attached.lock().expect("attached map");
        let count = attached.entry(session_id).or_insert(0);
        *count += 1;
        *count
    }

    fn detach(&self, session_id: u64) {
        let mut attached = self.attached.lock().expect("attached map");
        if let Some(count) = attached.get_mut(&session_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                attached.remove(&session_id);
            }
        }
    }

    fn count(&self, session_id: u64) -> u32 {
        self.attached
            .lock()
            .expect("attached map")
            .get(&session_id)
            .copied()
            .unwrap_or(0)
    }

    async fn infos(&self) -> Vec<stitch_mux_protocol::SessionInfo> {
        let registry = self.registry.lock().await;
        registry
            .names()
            .into_iter()
            .filter_map(|name| registry.get(&name))
            .map(|session| session.info(self.count(session.id)))
            .collect()
    }
}

/// Bind the endpoint and serve connections until the process is stopped.
pub async fn run_foreground(endpoint: Endpoint) -> Result<(), ServerError> {
    let listener = endpoint.bind().await?;
    tracing::info!("stitch-mux daemon listening");
    serve(listener).await
}

/// Serve an already-bound listener. Used by integration tests and the CLI.
pub async fn serve(listener: stitch_mux_protocol::Listener) -> Result<(), ServerError> {
    let state = Arc::new(ServerState::default());
    loop {
        let stream = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, state).await {
                tracing::debug!("client connection ended: {err}");
            }
        });
    }
}

async fn handle_connection(stream: Stream, state: Arc<ServerState>) -> Result<(), ServerError> {
    let mut connection = Connection::new(stream);
    let hello: Hello = connection.recv(MessageKind::Hello).await?;
    if hello.protocol_version != PROTOCOL_VERSION {
        connection
            .send(
                MessageKind::Error,
                &ErrorInfo {
                    code: ErrorCode::ProtocolVersion,
                    message: "unsupported protocol version".into(),
                },
            )
            .await?;
        return Ok(());
    }
    let required = match hello.view_mode {
        ViewMode::Structured => capabilities::client::STRUCTURED_VIEW,
        ViewMode::Terminal => capabilities::client::ANSI,
    };
    if hello.capabilities & required == 0 {
        connection
            .send(
                MessageKind::Error,
                &ErrorInfo {
                    code: ErrorCode::Unsupported,
                    message: "missing view capability".into(),
                },
            )
            .await?;
        return Ok(());
    }
    let sessions = state.registry.lock().await.len() as u32;
    let _client_id = state.next_client_id();
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

    let (reader, mut writer) = connection.split();
    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<Result<Frame, FrameError>>();
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match reader.recv_frame().await {
                Ok(frame) => {
                    if frame_tx.send(Ok(frame)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = frame_tx.send(Err(err));
                    break;
                }
            }
        }
    });

    let mut attached: Option<Arc<Session>> = None;
    let mut updates: Option<broadcast::Receiver<Arc<PaneUpdate>>> = None;
    let mut failure: Option<ServerError> = None;
    loop {
        let event = tokio::select! {
            incoming = frame_rx.recv() => match incoming {
                Some(Ok(frame)) => Event::Frame(frame),
                Some(Err(err)) => {
                    failure = Some(ServerError::Frame(err));
                    break;
                }
                None => break,
            },
            update = next_update(updates.as_mut()) => match update {
                Some(update) => Event::Update(update),
                None => break,
            },
        };
        match event {
            Event::Frame(frame) => {
                match handle_frame(frame, &state, &mut attached, &mut updates, &mut writer).await {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(err) => {
                        failure = Some(err);
                        break;
                    }
                }
            }
            Event::Update(update) => {
                if let Err(err) = send_update(&mut writer, &update).await {
                    failure = Some(err);
                    break;
                }
            }
        }
    }

    if let Some(session) = attached {
        state.detach(session.id);
    }
    reader_task.abort();
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

enum Event {
    Frame(Frame),
    Update(Arc<PaneUpdate>),
}

async fn next_update(
    mut receiver: Option<&mut broadcast::Receiver<Arc<PaneUpdate>>>,
) -> Option<Arc<PaneUpdate>> {
    match receiver.as_mut() {
        Some(receiver) => receiver.recv().await.ok(),
        None => std::future::pending().await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    frame: Frame,
    state: &Arc<ServerState>,
    attached: &mut Option<Arc<Session>>,
    updates: &mut Option<broadcast::Receiver<Arc<PaneUpdate>>>,
    writer: &mut FrameWriter<tokio::io::WriteHalf<Stream>>,
) -> Result<bool, ServerError> {
    match frame.kind {
        MessageKind::Ping => {
            let nonce: u8 = decode_message(&frame).unwrap_or(1);
            writer.send(MessageKind::Pong, &nonce).await?;
        }
        MessageKind::CommandRequest => {
            let request: CommandRequest = decode_message(&frame)?;
            let response = handle_command(request, state).await;
            writer.send(MessageKind::CommandResponse, &response).await?;
        }
        MessageKind::Attach => {
            let request: Attach = decode_message(&frame)?;
            let session = {
                let registry = state.registry.lock().await;
                registry.get(&request.session_name)
            };
            let Some(session) = session else {
                send_error(writer, ErrorCode::NotFound, "session not found").await?;
                return Ok(false);
            };
            // Subscribe before snapshotting: updates produced in the gap stay
            // queued and are replayed after the full state.
            *updates = Some(session.pane.subscribe());
            let clients = state.attach(session.id);
            *attached = Some(Arc::clone(&session));
            writer
                .send(MessageKind::Attached, &session.attached(clients))
                .await?;
            writer.send(MessageKind::Layout, &session.layout()).await?;
            writer
                .send(MessageKind::PaneSnapshot, &session.pane.snapshot())
                .await?;
        }
        MessageKind::Input => {
            let event: InputEvent = decode_message(&frame)?;
            if let Some(session) = attached.as_ref() {
                route_input(session, event);
            }
        }
        MessageKind::ResizeRequest => {
            let request: ResizeRequest = decode_message(&frame)?;
            if let Some(session) = attached.as_ref() {
                session.resize(request.cols, request.rows);
                writer
                    .send(
                        MessageKind::ResizeEvent,
                        &stitch_mux_protocol::ResizeEvent {
                            cols: request.cols,
                            rows: request.rows,
                        },
                    )
                    .await?;
            }
        }
        MessageKind::FocusRequest => {
            let focus: stitch_mux_protocol::FocusRequest = decode_message(&frame)?;
            writer
                .send(
                    MessageKind::Focus,
                    &stitch_mux_protocol::Focus {
                        pane_id: focus.pane_id,
                    },
                )
                .await?;
        }
        MessageKind::Resync => {
            if let Some(session) = attached.as_ref() {
                writer
                    .send(MessageKind::PaneSnapshot, &session.pane.snapshot())
                    .await?;
            }
        }
        MessageKind::CaptureRequest => {
            let request: CaptureRequest = decode_message(&frame)?;
            let response = attached.as_ref().map(|session| {
                session
                    .pane
                    .capture(request.start, request.lines, request.format)
            });
            if let Some(response) = response {
                writer.send(MessageKind::CaptureResponse, &response).await?;
            } else {
                send_error(writer, ErrorCode::NotFound, "no session attached").await?;
            }
        }
        MessageKind::Detach => return Ok(true),
        MessageKind::Error => {
            let info: ErrorInfo = decode_message(&frame)?;
            tracing::debug!("client error: {:?}: {}", info.code, info.message);
        }
        MessageKind::SetLayout | MessageKind::Clipboard | MessageKind::SubscribeEvents => {
            send_error(
                writer,
                ErrorCode::Unsupported,
                "this message arrives after M1",
            )
            .await?;
        }
        other => {
            send_error(
                writer,
                ErrorCode::Unsupported,
                &format!("{other:?} is not a client-to-server message"),
            )
            .await?;
        }
    }
    Ok(false)
}

fn route_input(session: &Session, event: InputEvent) {
    let pane = &session.pane;
    match event {
        InputEvent::Text(text) => pane.write(text.into_bytes()),
        InputEvent::Paste(text) => {
            let bytes = if pane.snapshot().modes.bracketed_paste {
                format!("\x1b[200~{text}\x1b[201~").into_bytes()
            } else {
                text.into_bytes()
            };
            pane.write(bytes);
        }
        InputEvent::Bytes(bytes) => pane.write(bytes),
        InputEvent::Key(key) => pane.write(pane.encode_key(&key)),
        InputEvent::Focus(_) | InputEvent::Mouse(_) => {}
    }
}

async fn send_update(
    writer: &mut FrameWriter<tokio::io::WriteHalf<Stream>>,
    update: &PaneUpdate,
) -> Result<(), ServerError> {
    match update {
        PaneUpdate::Snapshot(snapshot) => {
            writer.send(MessageKind::PaneSnapshot, snapshot).await?;
        }
        PaneUpdate::Delta(delta) => {
            writer.send(MessageKind::PaneDelta, delta).await?;
        }
        PaneUpdate::Exit(exit) => {
            writer.send(MessageKind::Exit, exit).await?;
        }
    }
    Ok(())
}

async fn handle_command(request: CommandRequest, state: &Arc<ServerState>) -> CommandResponse {
    let mut response = CommandResponse {
        request_id: request.request_id,
        ok: true,
        ..Default::default()
    };
    match request.command {
        Command::NewSession {
            name,
            program,
            args,
            cwd,
            env,
            cols,
            rows,
        } => {
            let result = state
                .registry
                .lock()
                .await
                .create(
                    &name,
                    &program,
                    &args,
                    cwd.as_deref(),
                    env.as_deref(),
                    cols,
                    rows,
                    false,
                )
                .await;
            match result {
                Ok(session) => response
                    .sessions
                    .push(session.info(state.count(session.id))),
                Err(err) => set_error(&mut response, ErrorCode::Busy, err),
            }
        }
        Command::EnsureSession {
            name,
            program,
            args,
            cwd,
            env,
            cols,
            rows,
        } => {
            let result = state
                .registry
                .lock()
                .await
                .create(
                    &name,
                    &program,
                    &args,
                    cwd.as_deref(),
                    env.as_deref(),
                    cols,
                    rows,
                    true,
                )
                .await;
            match result {
                Ok(session) => response
                    .sessions
                    .push(session.info(state.count(session.id))),
                Err(err) => set_error(&mut response, ErrorCode::Internal, err),
            }
        }
        Command::ListSessions => response.sessions = state.infos().await,
        Command::KillSession { name } => {
            let result = state.registry.lock().await.kill(&name).await;
            if let Err(err) = result {
                set_error(&mut response, ErrorCode::NotFound, err);
            }
        }
        Command::Capture {
            pane_id,
            start,
            lines,
            format,
        } => {
            let registry = state.registry.lock().await;
            let capture = registry
                .names()
                .into_iter()
                .filter_map(|name| registry.get(&name))
                .find(|session| session.pane.id == pane_id)
                .map(|session| session.pane.capture(start, lines, format));
            match capture {
                Some(capture) => response.capture = Some(capture),
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
        }
        other => set_error(
            &mut response,
            ErrorCode::Unsupported,
            format!("{other:?} arrives after M1"),
        ),
    }
    response
}

fn set_error(response: &mut CommandResponse, code: ErrorCode, message: impl std::fmt::Display) {
    response.ok = false;
    response.error = Some(ErrorInfo {
        code,
        message: message.to_string(),
    });
}

async fn send_error<W>(
    writer: &mut FrameWriter<W>,
    code: ErrorCode,
    message: &str,
) -> Result<(), ServerError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    writer
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
    fn server_state_starts_empty() {
        let state = ServerState::default();
        assert_eq!(state.next_client_id(), 1);
        let registry = state.registry.blocking_lock();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }
}
