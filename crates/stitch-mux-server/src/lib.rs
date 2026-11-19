//! Local stitch-mux daemon.
//!
//! M2 scope: own PTY panes and a server-validated binary split layout, project
//! structured snapshots/deltas to attached clients, and keep per-client focus
//! local to each connection. Sessions survive detach/reattach.

pub mod client_updates;
pub mod pane;
pub mod session;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use stitch_mux_core::{Size, layout_leaves};
use stitch_mux_protocol::{
    Attach, CaptureRequest, ClientInfo, Command, CommandRequest, CommandResponse, Connection,
    DEFAULT_SERVER_CAPABILITIES, Endpoint, EndpointError, ErrorCode, ErrorInfo, Frame, FrameError,
    FrameWriter, Hello, HelloAck, InputEvent, Layout, MessageKind, PROTOCOL_VERSION, ResizeRequest,
    Stream, ViewMode, capabilities, decode_message,
};
use thiserror::Error;
use tokio::sync::{Mutex, Notify, broadcast, mpsc, watch};
use tokio::task::JoinHandle;

pub use pane::{PaneError, PaneRuntime, PaneSpec, PaneUpdate};
pub use session::{Session, SessionError, SessionRegistry};

type Writer = FrameWriter<tokio::io::WriteHalf<Stream>>;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Frame(#[from] FrameError),
}

/// One daemon-side view of a connected client. Control-only connections have
/// no `session_id` and do not affect canonical session size.
#[derive(Clone)]
struct ClientRecord {
    id: u64,
    name: String,
    view_mode: ViewMode,
    session_id: Option<u64>,
    cols: u16,
    rows: u16,
    focus: Option<u64>,
    attached_at_unix_ms: u64,
    detach: Arc<Notify>,
}

impl ClientRecord {
    fn info(&self) -> ClientInfo {
        ClientInfo {
            id: self.id,
            name: self.name.clone(),
            view_mode: self.view_mode,
            session_id: self.session_id,
            cols: self.cols,
            rows: self.rows,
            focus: self.focus,
            attached_at_unix_ms: self.attached_at_unix_ms,
        }
    }
}

pub struct ServerState {
    pub registry: Mutex<SessionRegistry>,
    clients: StdMutex<BTreeMap<u64, ClientRecord>>,
    next_client_id: AtomicU64,
    shutdown_tx: watch::Sender<bool>,
}

impl Default for ServerState {
    fn default() -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            registry: Mutex::new(SessionRegistry::new()),
            clients: StdMutex::new(BTreeMap::new()),
            next_client_id: AtomicU64::new(1),
            shutdown_tx,
        }
    }
}

impl ServerState {
    fn register(&self, hello: &Hello) -> u64 {
        let id = self.next_client_id.fetch_add(1, Ordering::SeqCst);
        let record = ClientRecord {
            id,
            name: hello.client_name.clone(),
            view_mode: hello.view_mode,
            session_id: None,
            cols: hello.cols,
            rows: hello.rows,
            focus: None,
            attached_at_unix_ms: now_unix_ms(),
            detach: Arc::new(Notify::new()),
        };
        self.clients
            .lock()
            .expect("client registry")
            .insert(id, record);
        id
    }

    fn set_attached(&self, client_id: u64, session_id: u64, focus: Option<u64>) {
        if let Some(client) = self
            .clients
            .lock()
            .expect("client registry")
            .get_mut(&client_id)
        {
            client.session_id = Some(session_id);
            client.focus = focus;
            client.attached_at_unix_ms = now_unix_ms();
        }
    }

    fn set_focus(&self, client_id: u64, focus: u64) {
        if let Some(client) = self
            .clients
            .lock()
            .expect("client registry")
            .get_mut(&client_id)
        {
            client.focus = Some(focus);
        }
    }

    fn set_size(&self, client_id: u64, cols: u16, rows: u16) {
        if let Some(client) = self
            .clients
            .lock()
            .expect("client registry")
            .get_mut(&client_id)
        {
            client.cols = cols;
            client.rows = rows;
        }
    }

    fn unregister(&self, client_id: u64) -> Option<ClientRecord> {
        self.clients
            .lock()
            .expect("client registry")
            .remove(&client_id)
    }

    fn detach_handle(&self, client_id: u64) -> Option<Arc<Notify>> {
        self.clients
            .lock()
            .expect("client registry")
            .get(&client_id)
            .map(|client| Arc::clone(&client.detach))
    }

    fn count(&self, session_id: u64) -> u32 {
        self.clients
            .lock()
            .expect("client registry")
            .values()
            .filter(|client| client.session_id == Some(session_id))
            .count() as u32
    }

    fn client_infos(&self, session: Option<&Session>) -> Vec<ClientInfo> {
        self.clients
            .lock()
            .expect("client registry")
            .values()
            .filter(|client| match session {
                Some(session) => client.session_id == Some(session.id),
                None => true,
            })
            .map(ClientRecord::info)
            .collect()
    }

    /// Smallest attached viewport wins. Control-only clients are ignored.
    fn canonical_size(&self, session_id: u64) -> Option<Size> {
        let clients = self.clients.lock().expect("client registry");
        let mut smallest: Option<Size> = None;
        for client in clients.values().filter(|client| {
            client.session_id == Some(session_id) && client.cols > 0 && client.rows > 0
        }) {
            let size = Size::new(client.cols, client.rows);
            smallest = Some(match smallest {
                None => size,
                Some(current) => {
                    Size::new(current.cols.min(size.cols), current.rows.min(size.rows))
                }
            });
        }
        smallest
    }

    /// Apply the smallest-client policy and return the resulting canonical size.
    fn sync_size(&self, session: &Session) -> Option<Size> {
        let size = self.canonical_size(session.id)?;
        if size != session.size() {
            session.resize(size.cols, size.rows);
        }
        Some(size)
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

    fn shutdown(&self, except: Option<u64>) {
        for client in self
            .clients
            .lock()
            .expect("client registry")
            .values()
            .filter(|client| Some(client.id) != except)
        {
            client.detach.notify_one();
        }
        let _ = self.shutdown_tx.send(true);
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
    let mut shutdown = state.shutdown_tx.subscribe();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = accepted?;
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, state).await {
                        tracing::debug!("client connection ended: {err}");
                    }
                });
            }
            _ = shutdown.changed() => break Ok(()),
        }
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
    let client_id = state.register(&hello);
    let detach = state
        .detach_handle(client_id)
        .expect("client was just registered");
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
    let mut focus: Option<u64> = None;
    let mut update_rx: Option<client_updates::ClientUpdateQueue> = None;
    let mut coalescer = client_updates::UpdateCoalescer::default();
    let mut update_tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut layout_rx: Option<broadcast::Receiver<Arc<Layout>>> = None;
    let mut failure: Option<ServerError> = None;
    loop {
        if update_rx
            .as_ref()
            .is_some_and(|queue| queue.overflows() >= client_updates::MAX_CLIENT_OVERFLOWS)
        {
            let _ = tokio::time::timeout(
                Duration::from_secs(30),
                send_error(&mut writer, ErrorCode::Busy, "client fell behind"),
            )
            .await;
            break;
        }
        let event = tokio::select! {
            _ = detach.notified() => Event::DetachedByControl,
            incoming = frame_rx.recv() => match incoming {
                Some(Ok(frame)) => Event::Frame(frame),
                Some(Err(err)) => {
                    failure = Some(ServerError::Frame(err));
                    break;
                }
                None => break,
            },
            update = next_update(update_rx.as_mut()) => match update {
                Some(update) => Event::Update(update),
                None => break,
            },
            layout = next_layout(layout_rx.as_mut()) => match layout {
                Some(Ok(_)) => Event::Layout,
                Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                    if let Some(session) = attached.as_ref()
                        && let Err(err) = send_session_state(&mut writer, session, focus, &state).await
                    {
                        failure = Some(err);
                        break;
                    }
                    continue;
                }
                Some(Err(broadcast::error::RecvError::Closed)) | None => break,
            },
        };
        match event {
            Event::Frame(frame) => {
                match handle_frame(
                    frame,
                    client_id,
                    &state,
                    &mut attached,
                    &mut focus,
                    &mut update_rx,
                    &mut coalescer,
                    &mut update_tasks,
                    &mut layout_rx,
                    &mut writer,
                )
                .await
                {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(err) => {
                        failure = Some(err);
                        break;
                    }
                }
            }
            Event::Update(update) => {
                coalescer.add(update);
                if let Some(queue) = update_rx.as_mut() {
                    for queued in queue.drain_batch() {
                        coalescer.add(queued);
                    }
                }
                match tokio::time::timeout(Duration::from_secs(2), coalescer.flush(&mut writer))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        failure = Some(ServerError::Frame(err));
                        break;
                    }
                    Err(_) => {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(2),
                            send_error(&mut writer, ErrorCode::Busy, "client fell behind"),
                        )
                        .await;
                        break;
                    }
                }
            }
            Event::DetachedByControl => {
                send_error(&mut writer, ErrorCode::Busy, "detached by control client").await?;
                break;
            }
            Event::Layout => {
                let Some(session) = attached.as_ref() else {
                    continue;
                };
                if let Some(id) = focus {
                    focus = session.next_focus(id);
                } else {
                    focus = session.visible_pane_ids().first().copied();
                }
                if let Err(err) = send_session_state(&mut writer, session, focus, &state).await {
                    failure = Some(err);
                    break;
                }
            }
        }
    }

    for task in &update_tasks {
        task.abort();
    }
    state.unregister(client_id);
    if let Some(session) = attached.as_ref() {
        state.sync_size(session);
        session.publish_layout();
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
    Layout,
    DetachedByControl,
}

async fn next_update(
    receiver: Option<&mut client_updates::ClientUpdateQueue>,
) -> Option<Arc<PaneUpdate>> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_layout(
    receiver: Option<&mut broadcast::Receiver<Arc<Layout>>>,
) -> Option<Result<Arc<Layout>, broadcast::error::RecvError>> {
    match receiver {
        Some(receiver) => Some(receiver.recv().await),
        None => std::future::pending().await,
    }
}

fn with_attached_count(state: &ServerState, mut layout: Layout) -> Layout {
    layout.attached_clients = state.count(layout.session_id);
    layout
}

async fn send_session_state(
    writer: &mut Writer,
    session: &Session,
    focus: Option<u64>,
    state: &ServerState,
) -> Result<(), ServerError> {
    let layout = with_attached_count(state, session.layout(focus));
    // Layout first, then a full snapshot for every pane it contains.
    writer.send(MessageKind::Layout, &layout).await?;
    for geometry in &layout.panes {
        if let Some(pane) = session.pane(geometry.pane_id) {
            writer
                .send(MessageKind::PaneSnapshot, &pane.snapshot())
                .await?;
        }
    }
    if let Some(pane_id) = focus {
        writer
            .send(MessageKind::Focus, &stitch_mux_protocol::Focus { pane_id })
            .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    frame: Frame,
    client_id: u64,
    state: &Arc<ServerState>,
    attached: &mut Option<Arc<Session>>,
    focus: &mut Option<u64>,
    update_rx: &mut Option<client_updates::ClientUpdateQueue>,
    coalescer: &mut client_updates::UpdateCoalescer,
    update_tasks: &mut Vec<JoinHandle<()>>,
    layout_rx: &mut Option<broadcast::Receiver<Arc<Layout>>>,
    writer: &mut Writer,
) -> Result<bool, ServerError> {
    match frame.kind {
        MessageKind::Ping => {
            let nonce: u8 = decode_message(&frame).unwrap_or(1);
            writer.send(MessageKind::Pong, &nonce).await?;
        }
        MessageKind::CommandRequest => {
            let request: CommandRequest = decode_message(&frame)?;
            let response = handle_command(request, state, client_id).await;
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
            // Subscribe before snapshotting; updates produced in the gap stay
            // queued and are replayed after the full state.
            let queue = client_updates::ClientUpdateQueue::new();
            let update_tx = queue.sender();
            let overflow = queue.overflow_counter();
            for pane in session.panes() {
                let mut pane_rx = pane.subscribe();
                let update_tx = update_tx.clone();
                let overflow = Arc::clone(&overflow);
                let task = tokio::spawn(async move {
                    loop {
                        match pane_rx.recv().await {
                            Ok(update) => {
                                client_updates::offer_update(&update_tx, &overflow, update);
                            }
                            Err(broadcast::error::RecvError::Lagged(_lagged)) => {
                                // Never block the pane reader; offer the newest
                                // authoritative screen instead of lost rows.
                                overflow.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                client_updates::offer_update(
                                    &update_tx,
                                    &overflow,
                                    Arc::new(PaneUpdate::Snapshot(pane.snapshot())),
                                );
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });
                update_tasks.push(task);
            }
            drop(update_tx);
            *update_rx = Some(queue);
            coalescer.reset();
            *layout_rx = Some(session.subscribe_layout());
            *focus = session.visible_pane_ids().first().copied();
            state.set_attached(client_id, session.id, *focus);
            state.sync_size(&session);
            session.publish_layout();
            let clients = state.count(session.id);
            *attached = Some(Arc::clone(&session));
            writer
                .send(MessageKind::Attached, &session.attached(clients))
                .await?;
            send_session_state(writer, &session, *focus, state).await?;
        }
        MessageKind::Input => {
            let event: InputEvent = decode_message(&frame)?;
            if let Some(session) = attached.as_ref() {
                route_input(session, *focus, event);
            }
        }
        MessageKind::ResizeRequest => {
            let request: ResizeRequest = decode_message(&frame)?;
            if let Some(session) = attached.as_ref() {
                state.set_size(client_id, request.cols, request.rows);
                let size = state.sync_size(session).unwrap_or(session.size());
                writer
                    .send(
                        MessageKind::ResizeEvent,
                        &stitch_mux_protocol::ResizeEvent {
                            cols: size.cols,
                            rows: size.rows,
                        },
                    )
                    .await?;
            }
        }
        MessageKind::FocusRequest => {
            let request: stitch_mux_protocol::FocusRequest = decode_message(&frame)?;
            let valid = attached
                .as_ref()
                .is_some_and(|session| session.visible_pane_ids().contains(&request.pane_id));
            if !valid {
                send_error(writer, ErrorCode::NotFound, "pane not found").await?;
                return Ok(false);
            }
            *focus = Some(request.pane_id);
            state.set_focus(client_id, request.pane_id);
            writer
                .send(
                    MessageKind::Focus,
                    &stitch_mux_protocol::Focus {
                        pane_id: request.pane_id,
                    },
                )
                .await?;
        }
        MessageKind::SetLayout => {
            let request: stitch_mux_protocol::SetLayout = decode_message(&frame)?;
            let Some(session) = attached.as_ref() else {
                send_error(writer, ErrorCode::NotFound, "no session attached").await?;
                return Ok(false);
            };
            match session.set_layout(request.root, request.base_revision) {
                Ok(layout) => {
                    writer
                        .send(MessageKind::Layout, &with_attached_count(state, layout))
                        .await?;
                }
                Err(SessionError::StaleLayout { expected, current }) => {
                    send_error(
                        writer,
                        ErrorCode::Busy,
                        format!("layout revision {expected} is stale; current is {current}"),
                    )
                    .await?;
                    writer
                        .send(
                            MessageKind::Layout,
                            &with_attached_count(state, session.layout(*focus)),
                        )
                        .await?;
                }
                Err(err) => {
                    send_error(writer, session_error_code(&err), err.to_string()).await?;
                }
            }
        }
        MessageKind::Resync => {
            if let Some(session) = attached.as_ref() {
                send_session_state(writer, session, *focus, state).await?;
            }
        }
        MessageKind::CaptureRequest => {
            let request: CaptureRequest = decode_message(&frame)?;
            let response = attached
                .as_ref()
                .and_then(|session| session.pane(request.pane_id))
                .map(|pane| pane.capture(request.start, request.lines, request.format));
            if let Some(response) = response {
                writer.send(MessageKind::CaptureResponse, &response).await?;
            } else {
                send_error(writer, ErrorCode::NotFound, "pane not found").await?;
            }
        }
        MessageKind::Detach => return Ok(true),
        MessageKind::Error => {
            let info: ErrorInfo = decode_message(&frame)?;
            tracing::debug!("client error: {:?}: {}", info.code, info.message);
        }
        MessageKind::Clipboard | MessageKind::SubscribeEvents => {
            send_error(writer, ErrorCode::Unsupported, "this message arrives later").await?;
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

fn route_input(session: &Session, focus: Option<u64>, event: InputEvent) {
    let Some(pane_id) = focus else {
        return;
    };
    let Some(pane) = session.pane(pane_id) else {
        return;
    };
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
        InputEvent::Key(key) => {
            let bytes = pane.encode_key(&key);
            pane.write(bytes);
        }
        InputEvent::Focus(_) | InputEvent::Mouse(_) => {}
    }
}

fn session_error_code(error: &SessionError) -> ErrorCode {
    match error {
        SessionError::PaneNotFound(_) | SessionError::NotFound(_) => ErrorCode::NotFound,
        SessionError::StaleLayout { .. } => ErrorCode::Busy,
        SessionError::AlreadyExists(_) | SessionError::LastPane => ErrorCode::Busy,
        _ => ErrorCode::Internal,
    }
}

async fn handle_command(
    request: CommandRequest,
    state: &Arc<ServerState>,
    client_id: u64,
) -> CommandResponse {
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
                Err(err) => set_error(&mut response, session_error_code(&err), err),
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
                Err(err) => set_error(&mut response, session_error_code(&err), err),
            }
        }
        Command::ListSessions => response.sessions = state.infos().await,
        Command::ListPanes { session } => {
            let info = state.registry.lock().await.get(&session).map(|session| {
                let count = state.count(session.id);
                session.info(count)
            });
            match info {
                Some(info) => response.panes = info.panes,
                None => set_error(&mut response, ErrorCode::NotFound, "session not found"),
            }
        }
        Command::KillSession { name } => {
            let result = state.registry.lock().await.kill(&name).await;
            if let Err(err) = result {
                set_error(&mut response, session_error_code(&err), err);
            }
        }
        Command::SplitPane {
            pane_id,
            axis,
            ratio_bps,
        } => {
            let session = state.registry.lock().await.find_by_pane(pane_id);
            match session {
                Some(session) => match session.split(pane_id, axis, ratio_bps).await {
                    Ok(layout) => response.layout = Some(with_attached_count(state, layout)),
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::ClosePane { pane_id } => {
            let registry = state.registry.lock().await;
            let session = registry.find_by_pane(pane_id);
            let name = registry.names().into_iter().find(|name| {
                registry
                    .get(name)
                    .is_some_and(|found| session.as_ref().is_some_and(|s| s.id == found.id))
            });
            drop(registry);
            match session {
                Some(session) => match session.close(pane_id).await {
                    Ok(Some(layout)) => response.layout = Some(with_attached_count(state, layout)),
                    Ok(None) => {
                        if let Some(name) = name {
                            let _ = state.registry.lock().await.kill(&name).await;
                        }
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::ZoomPane { session, pane_id } => {
            let target = state.registry.lock().await.get(&session);
            match target {
                Some(target) => match target.zoom(pane_id) {
                    Ok(layout) => response.layout = Some(with_attached_count(state, layout)),
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "session not found"),
            }
        }
        Command::ResizeSession {
            session,
            cols,
            rows,
        } => {
            let target = state.registry.lock().await.get(&session);
            match target {
                Some(target) => {
                    target.resize(cols, rows);
                    response.sessions = state.infos().await;
                }
                None => set_error(&mut response, ErrorCode::NotFound, "session not found"),
            }
        }
        Command::SetLayout {
            base_revision,
            root,
        } => {
            let session = state.registry.lock().await.find_by_pane(
                layout_leaves(&root)
                    .first()
                    .copied()
                    .map(stitch_mux_core::PaneId::get)
                    .unwrap_or(0),
            );
            match session {
                Some(session) => match session.set_layout(root, base_revision) {
                    Ok(layout) => response.layout = Some(with_attached_count(state, layout)),
                    Err(SessionError::StaleLayout { expected, current }) => {
                        set_error(
                            &mut response,
                            ErrorCode::Busy,
                            format!("layout revision {expected} is stale; current is {current}"),
                        );
                        response.layout = Some(with_attached_count(state, session.layout(None)));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
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
                .find_by_pane(pane_id)
                .and_then(|session| session.pane(pane_id))
                .map(|pane| pane.capture(start, lines, format));
            match capture {
                Some(capture) => response.capture = Some(capture),
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
        }
        Command::SendText { pane_id, text } => {
            let pane = state
                .registry
                .lock()
                .await
                .find_by_pane(pane_id)
                .and_then(|session| session.pane(pane_id));
            match pane {
                Some(pane) => pane.write(text.into_bytes()),
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
        }
        Command::SendKeys { pane_id, bytes } => {
            let pane = state
                .registry
                .lock()
                .await
                .find_by_pane(pane_id)
                .and_then(|session| session.pane(pane_id));
            match pane {
                Some(pane) => pane.write(bytes),
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
        }
        Command::ListClients { session } => {
            let target = state.registry.lock().await.get(&session);
            match target {
                Some(target) => response.clients = state.client_infos(Some(&target)),
                None => set_error(&mut response, ErrorCode::NotFound, "session not found"),
            }
        }
        Command::DetachClient { client_id } => match state.detach_handle(client_id) {
            Some(handle) => handle.notify_one(),
            None => set_error(&mut response, ErrorCode::NotFound, "client not found"),
        },
        Command::RenameSession { name, new_name } => {
            let result = state.registry.lock().await.rename(&name, &new_name);
            match result {
                Ok(()) => response.sessions = state.infos().await,
                Err(err) => set_error(&mut response, session_error_code(&err), err),
            }
        }
        Command::Shutdown => {
            // Do not detach the control connection that is waiting for this
            // response; every other client gets a typed close.
            state.shutdown(Some(client_id));
        }
        other => set_error(
            &mut response,
            ErrorCode::Unsupported,
            format!("{other:?} is not implemented yet"),
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
    message: impl AsRef<str>,
) -> Result<(), ServerError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    writer
        .send(
            MessageKind::Error,
            &ErrorInfo {
                code,
                message: message.as_ref().to_owned(),
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
        assert!(state.clients.lock().expect("clients").is_empty());
        let registry = state.registry.blocking_lock();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn smallest_attached_client_determines_canonical_size() {
        let state = ServerState::default();
        let first = state.register(&Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "first".into(),
            client_instance: "a".into(),
            view_mode: ViewMode::Structured,
            cols: 120,
            rows: 40,
            pixel_width: 0,
            pixel_height: 0,
            capabilities: 1,
        });
        let second = state.register(&Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "second".into(),
            client_instance: "b".into(),
            view_mode: ViewMode::Structured,
            cols: 70,
            rows: 20,
            pixel_width: 0,
            pixel_height: 0,
            capabilities: 1,
        });
        state.set_attached(first, 1, Some(1));
        state.set_attached(second, 1, Some(2));
        assert_eq!(state.canonical_size(1), Some(Size::new(70, 20)));
        state.unregister(second);
        assert_eq!(state.canonical_size(1), Some(Size::new(120, 40)));
        state.unregister(first);
        assert_eq!(state.canonical_size(1), None);
    }
}
