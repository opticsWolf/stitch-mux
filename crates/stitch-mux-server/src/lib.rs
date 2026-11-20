//! Local stitch-mux daemon.
//!
//! M3 scope: own PTY panes and a server-validated binary split layout, project
//! bounded structured snapshots/deltas to attached clients, and keep per-client
//! focus local to each connection. Sessions survive detach/reattach.

pub mod ansi;
pub mod client_updates;
pub mod link_scan;
pub mod notify_scan;
pub mod pane;
pub mod session;
pub mod shell_scan;
pub mod status_scan;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use stitch_mux_core::action::{ActionError, ActionRegistry};
use stitch_mux_core::keymap::{ActionInvocation, Keymap as CoreKeymap, KeymapError};
use stitch_mux_core::{Direction, LayoutError, Size, SplitAxis, layout_leaves};
use stitch_mux_protocol::{
    ActionInfo, Attach, CaptureRequest, ClaimMode, ClientInfo, Command, CommandRequest,
    CommandResponse, Connection, DEFAULT_SERVER_CAPABILITIES, Endpoint, EndpointError, ErrorCode,
    ErrorInfo, Event as WireEvent, Frame, FrameError, FrameWriter, Hello, HelloAck, InputEvent,
    KeyBinding, KeyMode, Keymap as WireKeymap, Layout, MessageKind, PROTOCOL_VERSION,
    ResizeRequest, RunPlacement, RunResult, Stream, SubscribeEvents, ViewMode, WaitResult,
    WaitTarget, capabilities, decode_message,
};
use thiserror::Error;
use tokio::sync::{Mutex, Notify, broadcast, mpsc, watch};
use tokio::task::JoinHandle;

pub use ansi::AnsiCompositor;
pub use pane::{PaneError, PaneRuntime, PaneSpec, PaneUpdate};
pub use session::{Session, SessionError, SessionRegistry};

type Writer = FrameWriter<tokio::io::WriteHalf<Stream>>;

/// Decoded frames waiting for the connection's event loop. Bounded so a
/// pipelining client cannot make the daemon buffer arbitrary payloads.
const INCOMING_FRAMES: usize = 16;
/// Bounded broadcast backlog for local automation subscribers. A lagging
/// subscriber gets a typed error and is unsubscribed, never an unbounded queue.
const EVENT_QUEUE: usize = 256;

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
    /// Stable client identity from `Hello`; used to restore this client's
    /// focus when it detaches and attaches again.
    instance: String,
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

#[derive(Clone, Debug)]
struct EventFilter {
    session_id: Option<u64>,
    all: bool,
}

pub struct ServerState {
    pub registry: Mutex<SessionRegistry>,
    clients: StdMutex<BTreeMap<u64, ClientRecord>>,
    /// Last focused pane per `Hello.client_instance`, across attach cycles.
    focus_memory: StdMutex<BTreeMap<String, u64>>,
    next_client_id: AtomicU64,
    shutdown_tx: watch::Sender<bool>,
    events_tx: broadcast::Sender<Arc<WireEvent>>,
    event_seq: AtomicU64,
    watched_panes: StdMutex<std::collections::BTreeSet<u64>>,
    actions: ActionRegistry,
    keymap: StdRwLock<CoreKeymap>,
    keymap_path: Option<PathBuf>,
}

impl Default for ServerState {
    fn default() -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        let (events_tx, _) = broadcast::channel(EVENT_QUEUE);
        let actions = ActionRegistry::builtin();
        let keymap_path = std::env::var_os("STITCH_MUX_KEYMAP").map(PathBuf::from);
        let keymap = match &keymap_path {
            Some(path) => match fs::read_to_string(path) {
                Ok(json) => match CoreKeymap::builtin().with_user_json(&json, &actions) {
                    Ok(keymap) => keymap,
                    Err(err) => {
                        tracing::warn!(path = %path.display(), %err, "ignoring invalid keymap");
                        CoreKeymap::builtin()
                    }
                },
                Err(err) => {
                    tracing::warn!(path = %path.display(), %err, "cannot read keymap");
                    CoreKeymap::builtin()
                }
            },
            None => CoreKeymap::builtin(),
        };
        Self {
            registry: Mutex::new(SessionRegistry::new()),
            clients: StdMutex::new(BTreeMap::new()),
            focus_memory: StdMutex::new(BTreeMap::new()),
            next_client_id: AtomicU64::new(1),
            shutdown_tx,
            events_tx,
            event_seq: AtomicU64::new(0),
            watched_panes: StdMutex::new(std::collections::BTreeSet::new()),
            actions,
            keymap: StdRwLock::new(keymap),
            keymap_path,
        }
    }
}

impl ServerState {
    fn register(&self, hello: &Hello) -> u64 {
        let id = self.next_client_id.fetch_add(1, Ordering::SeqCst);
        let record = ClientRecord {
            id,
            name: hello.client_name.clone(),
            instance: hello.client_instance.clone(),
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

    /// Publish one bounded, monotonic local event. Never blocks on a
    /// subscriber: broadcast drops the oldest entry for a lagging receiver.
    fn publish_event(&self, session_id: Option<u64>, kind: &str, payload: serde_json::Value) {
        let seq = self.event_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let event = Arc::new(WireEvent {
            seq,
            session_id,
            kind: kind.to_owned(),
            payload,
        });
        let _ = self.events_tx.send(event);
    }

    fn action_infos(&self) -> Vec<ActionInfo> {
        self.actions
            .list()
            .into_iter()
            .map(|action| ActionInfo {
                name: action.name.clone(),
                title: action.title.clone(),
                description: action.description.clone(),
                category: action.category.clone(),
                args_schema: serde_json::to_string(&action.args).ok(),
            })
            .collect()
    }

    fn wire_keymap(&self) -> WireKeymap {
        let keymap = self.keymap.read().expect("keymap");
        WireKeymap {
            bindings: keymap
                .bindings
                .iter()
                .map(|binding| KeyBinding {
                    keys: binding
                        .keys
                        .iter()
                        .map(|key| key.as_str().to_owned())
                        .collect(),
                    action: binding.action.clone(),
                    args: binding.args.clone(),
                    mode: binding.mode.clone(),
                    source: binding.source.clone(),
                })
                .collect(),
            modes: keymap
                .modes
                .iter()
                .map(|mode| KeyMode {
                    name: mode.name.clone(),
                    exclusive: mode.exclusive,
                })
                .collect(),
        }
    }

    fn reload_keymap(&self, path: Option<String>) -> Result<String, KeymapError> {
        let path = path
            .map(PathBuf::from)
            .or_else(|| self.keymap_path.clone())
            .ok_or(KeymapError::NoPath)?;
        let json = fs::read_to_string(&path)?;
        let keymap = CoreKeymap::builtin().with_user_json(&json, &self.actions)?;
        *self.keymap.write().expect("keymap") = keymap;
        Ok(path.display().to_string())
    }

    /// Fan every pane's status records and exits into the event bus once.
    fn watch_session(self: &Arc<Self>, session: Arc<Session>) {
        for pane in session.panes() {
            self.watch_pane(Arc::clone(&session), pane);
        }
    }

    fn watch_pane(self: &Arc<Self>, session: Arc<Session>, pane: Arc<PaneRuntime>) {
        let mut watched = self.watched_panes.lock().expect("watched panes");
        if !watched.insert(pane.id) {
            return;
        }
        drop(watched);
        let state = Arc::clone(self);
        let pane_id = pane.id;
        let mut statuses = pane.subscribe_status();
        let mut updates = pane.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    status = statuses.recv() => match status {
                        Ok(status) => {
                            state.publish_event(
                                Some(session.id),
                                "program_status_changed",
                                serde_json::json!({ "pane_id": pane_id, "status": status }),
                            );
                            state.publish_event(
                                Some(session.id),
                                "activity_changed",
                                serde_json::json!({ "activity": session.info(0).activity }),
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            state.publish_event(
                                Some(session.id),
                                "event_gap",
                                serde_json::json!({ "pane_id": pane_id }),
                            );
                        }
                    },
                    update = updates.recv() => match update {
                        Ok(update) => {
                            if let PaneUpdate::Exit(exit) = update.as_ref() {
                                state.publish_event(
                                    Some(session.id),
                                    "pane_exited",
                                    serde_json::json!({
                                        "pane_id": pane_id,
                                        "reason": exit.reason,
                                    }),
                                );
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                    },
                }
            }
        });
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
        let instance = {
            let mut clients = self.clients.lock().expect("client registry");
            let Some(client) = clients.get_mut(&client_id) else {
                return;
            };
            client.focus = Some(focus);
            client.instance.clone()
        };
        let mut memory = self.focus_memory.lock().expect("focus memory");
        // Local single-user daemons can outlive many GUI processes. Bound the
        // remembered instances; losing old focus is cheaper than unbounded
        // growth.
        if memory.len() >= 1024 && !memory.contains_key(&instance) {
            memory.clear();
        }
        memory.insert(instance, focus);
    }

    /// Remembered focus for this client instance, if that pane still exists in
    /// the session's visible layout.
    fn remembered_focus(&self, client_id: u64, session: &Session) -> Option<u64> {
        let instance = self
            .clients
            .lock()
            .expect("client registry")
            .get(&client_id)?
            .instance
            .clone();
        let pane = *self
            .focus_memory
            .lock()
            .expect("focus memory")
            .get(&instance)?;
        session.visible_pane_ids().contains(&pane).then_some(pane)
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
    fn canonical_size(&self, session_id: u64, owners: BTreeSet<u64>) -> Option<Size> {
        let clients = self.clients.lock().expect("client registry");
        let mut smallest: Option<Size> = None;
        for client in clients.values().filter(|client| {
            client.session_id == Some(session_id)
                && client.cols > 0
                && client.rows > 0
                && (owners.is_empty() || owners.contains(&client.id))
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
        let size = self.canonical_size(session.id, session.owners().into_values().collect())?;
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
    serve(listener).await
}

/// Serve an already-bound listener. Used by integration tests and the CLI.
pub async fn serve(listener: stitch_mux_protocol::Listener) -> Result<(), ServerError> {
    tracing::info!("stitch-mux daemon listening");
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
    // Bounded: a client can pipeline frames, and an unbounded queue would hold
    // every decoded payload in memory. Awaiting here stops reading the socket,
    // which is the backpressure the bounded queue is meant to apply.
    let (frame_tx, mut frame_rx) = mpsc::channel::<Result<Frame, FrameError>>(INCOMING_FRAMES);
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match reader.recv_frame().await {
                Ok(frame) => {
                    if frame_tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = frame_tx.send(Err(err)).await;
                    break;
                }
            }
        }
    });

    let view_mode = hello.view_mode;
    let client_caps = hello.capabilities;
    let mut attached: Option<Arc<Session>> = None;
    let mut focus: Option<u64> = None;
    let mut update_rx: Option<client_updates::ClientUpdateQueue> = None;
    let mut coalescer = client_updates::UpdateCoalescer::default();
    let mut compositor = AnsiCompositor::new();
    let mut update_tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut layout_rx: Option<broadcast::Receiver<Arc<Layout>>> = None;
    let mut events_rx = state.events_tx.subscribe();
    let mut event_filter: Option<EventFilter> = None;
    let mut failure: Option<ServerError> = None;
    loop {
        if update_rx
            .as_ref()
            .is_some_and(|queue| queue.overflows() >= client_updates::MAX_CLIENT_OVERFLOWS)
        {
            let _ = send_backpressure_error(&mut writer).await;
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
            event = events_rx.recv() => match event {
                Ok(event) => Event::Wire(event),
                Err(broadcast::error::RecvError::Lagged(_)) => Event::WireLagged,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            layout = next_layout(layout_rx.as_mut()) => match layout {
                Some(Ok(_)) => Event::Layout,
                Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                    if let Some(session) = attached.as_ref()
                        && let Err(err) =
                            send_session_state(&mut writer, session, focus, &state, view_mode, &mut compositor, &mut coalescer).await
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
            Event::Wire(event) => {
                let wanted = event_filter.as_ref().is_some_and(|filter| {
                    filter
                        .session_id
                        .is_none_or(|id| event.session_id == Some(id))
                        && (filter.all
                            || event.session_id.is_some()
                            || event.kind == "session_list")
                });
                if wanted && let Err(err) = writer.send(MessageKind::Event, event.as_ref()).await {
                    failure = Some(ServerError::Frame(err));
                    break;
                }
            }
            Event::WireLagged => {
                if event_filter.take().is_some()
                    && let Err(err) =
                        send_error(&mut writer, ErrorCode::Busy, "event subscriber fell behind")
                            .await
                {
                    failure = Some(err);
                    break;
                }
            }
            Event::Frame(frame) => {
                match handle_frame(
                    frame,
                    client_id,
                    client_caps,
                    &mut event_filter,
                    &state,
                    &mut attached,
                    &mut focus,
                    &mut update_rx,
                    &mut coalescer,
                    &mut update_tasks,
                    &mut layout_rx,
                    &mut writer,
                    view_mode,
                    &mut compositor,
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
                if view_mode == ViewMode::Terminal {
                    // The compositor reads the current authoritative snapshots,
                    // so coalesced cell messages would be wasted work.
                    let mut exits = Vec::new();
                    let mut accept = |item: Arc<PaneUpdate>| {
                        if let PaneUpdate::Exit(exit) = item.as_ref() {
                            exits.push(exit.clone());
                        }
                    };
                    accept(update);
                    if let Some(queue) = update_rx.as_mut() {
                        for queued in queue.drain_batch() {
                            accept(queued);
                        }
                    }
                    if let Err(err) = send_exit_frames(&mut writer, &exits).await {
                        failure = Some(ServerError::Frame(err));
                        break;
                    }
                    if let Some(session) = attached.as_ref()
                        && let Err(err) = send_terminal_update(
                            &mut writer,
                            session,
                            focus,
                            &state,
                            &mut compositor,
                        )
                        .await
                    {
                        failure = Some(err);
                        break;
                    }
                } else {
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
                            let _ = send_backpressure_error(&mut writer).await;
                            break;
                        }
                    }
                }
            }
            Event::DetachedByControl => {
                // Do not use `?` here: an early return would skip the cleanup
                // below and leave a dead client in the registry, which would
                // pin the canonical session size to it forever.
                let _ =
                    send_error(&mut writer, ErrorCode::Busy, "detached by control client").await;
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
                if let Some(id) = focus {
                    state.set_focus(client_id, id);
                }
                if let Err(err) = send_session_state(
                    &mut writer,
                    session,
                    focus,
                    &state,
                    view_mode,
                    &mut compositor,
                    &mut coalescer,
                )
                .await
                {
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
    Wire(Arc<WireEvent>),
    WireLagged,
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

fn pane_snapshots(
    session: &Session,
    layout: &Layout,
) -> BTreeMap<u64, stitch_mux_protocol::PaneSnapshot> {
    layout
        .panes
        .iter()
        .filter_map(|geometry| {
            session
                .pane(geometry.pane_id)
                .map(|pane| (geometry.pane_id, pane.snapshot()))
        })
        .collect()
}

async fn send_exit_frames(
    writer: &mut Writer,
    exits: &[stitch_mux_protocol::SessionExit],
) -> Result<(), FrameError> {
    for exit in exits {
        writer.send(MessageKind::Exit, exit).await?;
    }
    Ok(())
}

async fn send_render_frame(
    writer: &mut Writer,
    render: stitch_mux_protocol::Render,
) -> Result<(), ServerError> {
    writer
        .send_render(
            render.seq,
            render.cols,
            render.rows,
            render.full,
            &render.ansi,
        )
        .await?;
    Ok(())
}

/// Publish a full authoritative state. Structured clients get geometry plus
/// cell snapshots; terminal clients get geometry (for focus) plus one composed
/// ANSI frame. A connection never receives both cell frames and Render frames.
async fn send_session_state(
    writer: &mut Writer,
    session: &Session,
    focus: Option<u64>,
    state: &ServerState,
    view_mode: ViewMode,
    compositor: &mut AnsiCompositor,
    coalescer: &mut client_updates::UpdateCoalescer,
) -> Result<(), ServerError> {
    let layout = with_attached_count(state, session.layout(focus));
    writer.send(MessageKind::Layout, &layout).await?;
    match view_mode {
        ViewMode::Structured => {
            for geometry in &layout.panes {
                if let Some(pane) = session.pane(geometry.pane_id) {
                    let snapshot = pane.snapshot();
                    writer.send(MessageKind::PaneSnapshot, &snapshot).await?;
                    coalescer.seed(snapshot.pane_id, snapshot.seq);
                }
            }
        }
        ViewMode::Terminal => {
            let attached = session.attached(layout.attached_clients, view_mode);
            let snapshots = pane_snapshots(session, &layout);
            let render = compositor.full(&attached, &layout, &snapshots);
            send_render_frame(writer, render).await?;
        }
    }
    if let Some(pane_id) = focus {
        writer
            .send(MessageKind::Focus, &stitch_mux_protocol::Focus { pane_id })
            .await?;
    }
    Ok(())
}

/// Send the next incremental ANSI projection after pane updates or a resize.
async fn send_terminal_update(
    writer: &mut Writer,
    session: &Session,
    focus: Option<u64>,
    state: &ServerState,
    compositor: &mut AnsiCompositor,
) -> Result<(), ServerError> {
    let layout = with_attached_count(state, session.layout(focus));
    let attached = session.attached(layout.attached_clients, ViewMode::Terminal);
    let snapshots = pane_snapshots(session, &layout);
    if let Some(render) = compositor.update(&attached, &layout, &snapshots) {
        send_render_frame(writer, render).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    frame: Frame,
    client_id: u64,
    client_caps: u64,
    event_filter: &mut Option<EventFilter>,
    state: &Arc<ServerState>,
    attached: &mut Option<Arc<Session>>,
    focus: &mut Option<u64>,
    update_rx: &mut Option<client_updates::ClientUpdateQueue>,
    coalescer: &mut client_updates::UpdateCoalescer,
    update_tasks: &mut Vec<JoinHandle<()>>,
    layout_rx: &mut Option<broadcast::Receiver<Arc<Layout>>>,
    writer: &mut Writer,
    view_mode: ViewMode,
    compositor: &mut AnsiCompositor,
) -> Result<bool, ServerError> {
    match frame.kind {
        MessageKind::Ping => {
            let nonce: u8 = decode_message(&frame).unwrap_or(1);
            writer.send(MessageKind::Pong, &nonce).await?;
        }
        MessageKind::CommandRequest => {
            let request: CommandRequest = decode_message(&frame)?;
            let response = handle_command(request, state, client_id, client_caps, focus).await;
            writer.send(MessageKind::CommandResponse, &response).await?;
        }
        MessageKind::Attach => {
            let request: Attach = decode_message(&frame)?;
            if request.takeover {
                send_error(
                    writer,
                    ErrorCode::Unsupported,
                    "takeover is not supported in protocol v1",
                )
                .await?;
                return Ok(false);
            }
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
            *focus = state
                .remembered_focus(client_id, &session)
                .or_else(|| session.visible_pane_ids().first().copied());
            state.set_attached(client_id, session.id, *focus);
            tracing::info!(
                client_id,
                session = %request.session_name,
                "client attached"
            );
            state.sync_size(&session);
            session.publish_layout();
            let clients = state.count(session.id);
            *attached = Some(Arc::clone(&session));
            compositor.invalidate();
            writer
                .send(MessageKind::Attached, &session.attached(clients, view_mode))
                .await?;
            send_session_state(
                writer, &session, *focus, state, view_mode, compositor, coalescer,
            )
            .await?;
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
                if view_mode == ViewMode::Terminal {
                    compositor.invalidate();
                    send_terminal_update(writer, session, *focus, state, compositor).await?;
                }
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
            match session
                .set_layout(
                    request.root,
                    request.base_revision,
                    request.window_id,
                    request.floating,
                )
                .await
            {
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
                compositor.invalidate();
                send_session_state(
                    writer, session, *focus, state, view_mode, compositor, coalescer,
                )
                .await?;
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
        MessageKind::SubscribeEvents => {
            let request: SubscribeEvents = decode_message(&frame)?;
            if client_caps & capabilities::client::EVENTS == 0 {
                send_error(
                    writer,
                    ErrorCode::Unsupported,
                    "EVENTS capability not requested",
                )
                .await?;
                return Ok(false);
            }
            let session_id = match &request.session {
                Some(name) => {
                    let session = state.registry.lock().await.get(name).map(|s| s.id);
                    match session {
                        Some(id) => Some(id),
                        None => {
                            send_error(writer, ErrorCode::NotFound, "session not found").await?;
                            return Ok(false);
                        }
                    }
                }
                None => None,
            };
            *event_filter = Some(EventFilter {
                session_id,
                all: request.all,
            });
            let sessions = state.infos().await;
            state.publish_event(
                session_id,
                "session_list",
                serde_json::json!({ "sessions": sessions }),
            );
        }
        MessageKind::Clipboard => {
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

fn focused_pane(session: &Session, focus: Option<u64>) -> Option<u64> {
    let visible = session.visible_pane_ids();
    focus
        .filter(|pane_id| visible.contains(pane_id))
        .or_else(|| visible.first().copied())
}

async fn execute_action(
    state: &Arc<ServerState>,
    session: Option<&Arc<Session>>,
    focus: &mut Option<u64>,
    invocation: &ActionInvocation,
) -> Result<(), (ErrorCode, String)> {
    state
        .actions
        .validate(&invocation.action, invocation.args.as_ref())
        .map_err(|err| match err {
            ActionError::Unknown(_) => (ErrorCode::Unsupported, err.to_string()),
            ActionError::InvalidArgs { .. } => (ErrorCode::InvalidArgs, err.to_string()),
        })?;
    let session = session.ok_or((
        ErrorCode::Unsupported,
        "attach to a session before invoking a pane action".to_owned(),
    ))?;
    let pane_id = focused_pane(session, *focus);
    match invocation.action.as_str() {
        "pane.split" => {
            let args = invocation.args.as_ref().expect("validated");
            let axis = match args["axis"].as_str() {
                Some("columns") => SplitAxis::Columns,
                Some("rows") => SplitAxis::Rows,
                _ => return Err((ErrorCode::InvalidArgs, "unknown split axis".into())),
            };
            let ratio = args["ratio_bps"]
                .as_u64()
                .unwrap_or(5000)
                .min(u16::MAX as u64) as u16;
            let pane_id = pane_id.ok_or((ErrorCode::NotFound, "no focused pane".into()))?;
            session
                .split(pane_id, axis, ratio)
                .await
                .map_err(|err| (ErrorCode::Busy, err.to_string()))?;
            state.watch_session(Arc::clone(session));
            Ok(())
        }
        "pane.focus-left" | "pane.focus-right" | "pane.focus-up" | "pane.focus-down" => {
            let direction = match invocation.action.as_str() {
                "pane.focus-left" => Direction::Left,
                "pane.focus-right" => Direction::Right,
                "pane.focus-up" => Direction::Up,
                _ => Direction::Down,
            };
            let pane_id = pane_id.ok_or((ErrorCode::NotFound, "no focused pane".into()))?;
            *focus = session.focus_direction(pane_id, direction);
            Ok(())
        }
        "pane.close" => {
            let pane_id = pane_id.ok_or((ErrorCode::NotFound, "no focused pane".into()))?;
            session
                .close(pane_id)
                .await
                .map_err(|err| (ErrorCode::Busy, err.to_string()))?;
            *focus = focused_pane(session, None);
            Ok(())
        }
        "pane.zoom-toggle" => {
            let pane_id = pane_id.ok_or((ErrorCode::NotFound, "no focused pane".into()))?;
            let zoomed = session.layout(Some(pane_id)).zoomed_pane;
            session
                .zoom(if zoomed.is_some() {
                    None
                } else {
                    Some(pane_id)
                })
                .await
                .map_err(|err| (ErrorCode::Busy, err.to_string()))?;
            Ok(())
        }
        "pane.resize-left" | "pane.resize-right" | "pane.resize-up" | "pane.resize-down" => {
            let (cols, rows) = (session.size().cols, session.size().rows);
            let (cols, rows) = match invocation.action.as_str() {
                "pane.resize-left" => (cols.saturating_sub(2).max(1), rows),
                "pane.resize-right" => (cols.saturating_add(2), rows),
                "pane.resize-up" => (cols, rows.saturating_sub(1).max(1)),
                _ => (cols, rows.saturating_add(1)),
            };
            session.resize(cols, rows);
            Ok(())
        }
        "client.detach" | "help.which-key" | "prompt.previous" | "prompt.next" => Err((
            ErrorCode::Unsupported,
            "this action is handled by the frontend".into(),
        )),
        other if other.starts_with("mode.") => Err((
            ErrorCode::Unsupported,
            "key modes are local to the frontend".into(),
        )),
        other => Err((ErrorCode::Unsupported, format!("{other} is not executable"))),
    }
}

async fn wait_for_pane(pane: Arc<PaneRuntime>, deadline: Option<Instant>) -> (Option<i32>, bool) {
    let mut updates = pane.subscribe();
    loop {
        if let Some((_, code)) = pane.exit_snapshot() {
            return (code, false);
        }
        let update = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(remaining, updates.recv()).await {
                    Ok(update) => update,
                    Err(_) => return (None, true),
                }
            }
            None => updates.recv().await,
        };
        match update {
            Ok(update) if matches!(update.as_ref(), PaneUpdate::Exit(_)) => {
                return pane
                    .exit_snapshot()
                    .map_or((None, false), |(_, code)| (code, false));
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => {
                return pane
                    .exit_snapshot()
                    .map_or((None, false), |(_, code)| (code, false));
            }
        }
    }
}

fn session_error_code(error: &SessionError) -> ErrorCode {
    match error {
        SessionError::PaneNotFound(_) | SessionError::NotFound(_) => ErrorCode::NotFound,
        SessionError::StaleLayout { .. } => ErrorCode::Busy,
        SessionError::LayoutMismatch | SessionError::Layout(LayoutError::InvalidBounds) => {
            ErrorCode::InvalidArgs
        }
        SessionError::AlreadyExists(_) | SessionError::NotOwner { .. } => ErrorCode::Busy,
        SessionError::PaneNotTiled(_) => ErrorCode::InvalidArgs,
        _ => ErrorCode::Internal,
    }
}

async fn handle_command(
    request: CommandRequest,
    state: &Arc<ServerState>,
    client_id: u64,
    client_caps: u64,
    focus: &mut Option<u64>,
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
                Ok(session) => {
                    tracing::info!(session = %name, "session created");
                    state.watch_session(Arc::clone(&session));
                    response
                        .sessions
                        .push(session.info(state.count(session.id)));
                }
                Err(err) => {
                    tracing::warn!(session = %name, error = %err, "session creation failed");
                    set_error(&mut response, session_error_code(&err), err);
                }
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
                Ok(session) => {
                    state.watch_session(Arc::clone(&session));
                    response
                        .sessions
                        .push(session.info(state.count(session.id)));
                }
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
            match result {
                Ok(()) => tracing::info!(session = %name, "session killed"),
                Err(err) => {
                    tracing::warn!(session = %name, error = %err, "session kill failed");
                    set_error(&mut response, session_error_code(&err), err);
                }
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
                    Ok(_) => {
                        state.watch_session(Arc::clone(&session));
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
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
                    Ok(Some(_)) => response.layout = Some(response_layout(state, &session, *focus)),
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
        Command::DetachPane { pane_id } => {
            if client_caps & capabilities::client::DETACHED_PANES == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks DETACHED_PANES",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_pane(pane_id);
            match session {
                Some(session) => match session.detach_pane(pane_id).await {
                    Ok(_) => {
                        state.publish_event(
                            Some(session.id),
                            "pane_placement_changed",
                            serde_json::json!({
                                "pane_id": pane_id,
                                "placement": "detached",
                            }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::PlacePane {
            pane_id,
            window_id,
            anchor_pane_id,
            axis,
            ratio_bps,
        } => {
            if client_caps & capabilities::client::DETACHED_PANES == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks DETACHED_PANES",
                );
                response.sessions = state.infos().await;
                return response;
            }
            if window_id.is_some() && client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            {
                let session = state.registry.lock().await.find_by_pane(pane_id);
                match session {
                    Some(session) => match session
                        .place_pane(pane_id, window_id, anchor_pane_id, axis, ratio_bps)
                        .await
                    {
                        Ok(_) => {
                            state.publish_event(
                                Some(session.id),
                                "pane_placement_changed",
                                serde_json::json!({
                                    "pane_id": pane_id,
                                    "placement": "tiled",
                                }),
                            );
                            response.layout = Some(response_layout(state, &session, *focus));
                        }
                        Err(err) => set_error(&mut response, session_error_code(&err), err),
                    },
                    None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
                }
            }
            response.sessions = state.infos().await;
        }
        Command::ClaimPane { pane_id, mode } => {
            if client_caps & capabilities::client::DETACHED_PANES == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks DETACHED_PANES",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_pane(pane_id);
            match session {
                Some(session) => match session.claim(pane_id, client_id, mode == ClaimMode::Take) {
                    Ok(()) => {
                        state.sync_size(&session);
                        state.publish_event(
                            Some(session.id),
                            "pane_owner_changed",
                            serde_json::json!({ "pane_id": pane_id, "owner_client_id": client_id }),
                        );
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::ReleasePane { pane_id } => {
            if client_caps & capabilities::client::DETACHED_PANES == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks DETACHED_PANES",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_pane(pane_id);
            match session {
                Some(session) => match session.release(pane_id, client_id) {
                    Ok(()) => {
                        state.sync_size(&session);
                        state.publish_event(
                            Some(session.id),
                            "pane_owner_changed",
                            serde_json::json!({
                                "pane_id": pane_id,
                                "owner_client_id": session.owner(pane_id),
                            }),
                        );
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::CreateWindow { session, name } => {
            if client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.get(&session);
            match session {
                Some(session) => match session.create_window(name.clone()).await {
                    Ok(window_id) => {
                        state.publish_event(
                            Some(session.id),
                            "window_created",
                            serde_json::json!({ "window_id": window_id }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "session not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::CloseWindow { window_id } => {
            if client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_window(window_id);
            match session {
                Some(session) => match session.close_window(window_id).await {
                    Ok(()) => {
                        state.publish_event(
                            Some(session.id),
                            "window_closed",
                            serde_json::json!({ "window_id": window_id }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "window not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::RenameWindow { window_id, name } => {
            if client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_window(window_id);
            match session {
                Some(session) => match session.rename_window(window_id, &name) {
                    Ok(()) => {
                        state.publish_event(
                            Some(session.id),
                            "window_renamed",
                            serde_json::json!({ "window_id": window_id, "name": name }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "window not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::MoveWindow { window_id, index } => {
            if client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_window(window_id);
            match session {
                Some(session) => match session.move_window(window_id, index) {
                    Ok(()) => {
                        state.publish_event(
                            Some(session.id),
                            "window_moved",
                            serde_json::json!({ "window_id": window_id, "index": index }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "window not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::FocusWindow { target } => {
            if client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session_id = state
                .clients
                .lock()
                .expect("client registry")
                .get(&client_id)
                .and_then(|client| client.session_id);
            let session = match session_id {
                Some(id) => state.registry.lock().await.get_by_id(id),
                None => state.registry.lock().await.first(),
            };
            match session {
                Some(session) => match session.focus_window(target) {
                    Ok(window_id) => {
                        state.publish_event(
                            Some(session.id),
                            "window_focused",
                            serde_json::json!({ "window_id": window_id }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "no active session"),
            }
            response.sessions = state.infos().await;
        }
        Command::FloatPane { pane_id, bounds } => {
            if client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_pane(pane_id);
            match session {
                Some(session) => match session.float_pane(pane_id, bounds).await {
                    Ok(_) => {
                        state.publish_event(
                            Some(session.id),
                            "pane_floated",
                            serde_json::json!({ "pane_id": pane_id }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::TilePane {
            pane_id,
            window_id,
            anchor_pane_id,
            axis,
            ratio_bps,
        } => {
            if client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_pane(pane_id);
            match session {
                Some(session) => match session
                    .tile_pane(pane_id, Some(window_id), anchor_pane_id, axis, ratio_bps)
                    .await
                {
                    Ok(_) => {
                        state.publish_event(
                            Some(session.id),
                            "pane_tiled",
                            serde_json::json!({ "pane_id": pane_id, "window_id": window_id }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::SetFloatingPane {
            layer_id,
            bounds,
            z,
            minimized,
        } => {
            if client_caps & capabilities::client::MULTI_WINDOW == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let session = state.registry.lock().await.find_by_layer(layer_id);
            match session {
                Some(session) => match session
                    .set_floating_pane(layer_id, bounds, z, minimized)
                    .await
                {
                    Ok(_) => {
                        state.publish_event(
                            Some(session.id),
                            "floating_changed",
                            serde_json::json!({ "layer_id": layer_id }),
                        );
                        response.layout = Some(response_layout(state, &session, *focus));
                    }
                    Err(err) => set_error(&mut response, session_error_code(&err), err),
                },
                None => set_error(&mut response, ErrorCode::NotFound, "layer not found"),
            }
            response.sessions = state.infos().await;
        }
        Command::ZoomPane { session, pane_id } => {
            let target = state.registry.lock().await.get(&session);
            match target {
                Some(target) => match target.zoom(pane_id).await {
                    Ok(_) => response.layout = Some(response_layout(state, &target, *focus)),
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
                // Attached viewports own the canonical size (smallest client
                // wins). A control resize would leave the PTY and the clients
                // disagreeing until the next attach, so only a session with no
                // viewport client may be resized from here.
                Some(target)
                    if state
                        .canonical_size(target.id, target.owners().into_values().collect())
                        .is_some() =>
                {
                    set_error(
                        &mut response,
                        ErrorCode::Unsupported,
                        "attached clients own the canonical size; resize the client viewport instead",
                    )
                }
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
            window_id,
            floating,
        } => {
            let session = state.registry.lock().await.find_by_pane(
                layout_leaves(&root)
                    .first()
                    .copied()
                    .map(stitch_mux_core::PaneId::get)
                    .unwrap_or(0),
            );
            match session {
                Some(session) => {
                    match session
                        .set_layout(root, base_revision, window_id, floating)
                        .await
                    {
                        Ok(_) => response.layout = Some(response_layout(state, &session, *focus)),
                        Err(SessionError::StaleLayout { expected, current }) => {
                            set_error(
                                &mut response,
                                ErrorCode::Busy,
                                format!(
                                    "layout revision {expected} is stale; current is {current}"
                                ),
                            );
                            response.layout = Some(response_layout(state, &session, *focus));
                        }
                        Err(err) => set_error(&mut response, session_error_code(&err), err),
                    }
                }
                None => set_error(&mut response, ErrorCode::NotFound, "pane not found"),
            }
        }
        Command::Run {
            session: session_name,
            program,
            args,
            cwd,
            env,
            placement,
            label: _,
            wait,
            keep_open,
        } => {
            if !matches!(placement, RunPlacement::Split { .. })
                && client_caps & capabilities::client::MULTI_WINDOW == 0
            {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "client lacks MULTI_WINDOW",
                );
                response.sessions = state.infos().await;
                return response;
            }
            let spec = PaneSpec::new(&program, &args, cwd.as_deref(), env.as_deref());
            let spawned = match placement {
                RunPlacement::Split {
                    pane_id,
                    axis,
                    ratio_bps,
                } => {
                    let session = state
                        .registry
                        .lock()
                        .await
                        .find_by_pane(pane_id)
                        .filter(|session| session.name() == session_name);
                    match session {
                        Some(session) => session
                            .run(pane_id, spec, axis, ratio_bps)
                            .await
                            .map(|(pane_id, layout)| (session, pane_id, 1, layout)),
                        None => {
                            set_error(
                                &mut response,
                                ErrorCode::NotFound,
                                "session or pane not found",
                            );
                            response.sessions = state.infos().await;
                            return response;
                        }
                    }
                }
                RunPlacement::NewWindow => {
                    let session = state.registry.lock().await.get(&session_name);
                    let Some(session) = session else {
                        set_error(&mut response, ErrorCode::NotFound, "session not found");
                        response.sessions = state.infos().await;
                        return response;
                    };
                    let window_id = match session.create_window(None).await {
                        Ok(id) => id,
                        Err(err) => {
                            set_error(&mut response, session_error_code(&err), err);
                            response.sessions = state.infos().await;
                            return response;
                        }
                    };
                    session
                        .run_in_window(window_id, None, Some(spec), SplitAxis::Columns, 5000)
                        .await
                        .map(|(pane_id, layout)| (session, pane_id, window_id, layout))
                }
                RunPlacement::Window { window_id } => {
                    let session = state
                        .registry
                        .lock()
                        .await
                        .find_by_window(window_id)
                        .filter(|session| session.name() == session_name);
                    match session {
                        Some(session) => session
                            .run_in_window(window_id, None, Some(spec), SplitAxis::Columns, 5000)
                            .await
                            .map(|(pane_id, layout)| (session, pane_id, window_id, layout)),
                        None => {
                            set_error(
                                &mut response,
                                ErrorCode::NotFound,
                                "session or window not found",
                            );
                            response.sessions = state.infos().await;
                            return response;
                        }
                    }
                }
                RunPlacement::Floating { window_id, bounds } => {
                    let session = state
                        .registry
                        .lock()
                        .await
                        .find_by_window(window_id)
                        .filter(|session| session.name() == session_name);
                    match session {
                        Some(session) => session
                            .run_floating(window_id, spec, bounds)
                            .await
                            .map(|(pane_id, _, layout)| (session, pane_id, window_id, layout)),
                        None => {
                            set_error(
                                &mut response,
                                ErrorCode::NotFound,
                                "session or window not found",
                            );
                            response.sessions = state.infos().await;
                            return response;
                        }
                    }
                }
            };
            match spawned {
                Ok((session, new_pane_id, window_id, _layout)) => {
                    state.watch_session(Arc::clone(&session));
                    state.publish_event(
                        Some(session.id),
                        "pane_created",
                        serde_json::json!({ "pane_id": new_pane_id, "window_id": window_id, "program": program }),
                    );
                    let mut result = RunResult {
                        pane_id: new_pane_id,
                        window_id,
                        exit_code: None,
                        timed_out: false,
                    };
                    if wait {
                        let pane = session
                            .pane(new_pane_id)
                            .expect("newly spawned pane is registered");
                        let (exit_code, timed_out) = wait_for_pane(Arc::clone(&pane), None).await;
                        result.exit_code = exit_code;
                        result.timed_out = timed_out;
                        if !keep_open
                            && !timed_out
                            && let Err(err) = session.close(new_pane_id).await
                        {
                            set_error(&mut response, session_error_code(&err), err);
                        }
                    }
                    response.run = Some(result);
                    response.layout = Some(response_layout(state, &session, *focus));
                }
                Err(err) => set_error(&mut response, session_error_code(&err), err),
            }
            response.sessions = state.infos().await;
        }
        Command::Wait { target, timeout_ms } => {
            let deadline = timeout_ms
                .map(|ms| Instant::now() + Duration::from_millis(ms.min(i64::MAX as u64)));
            let (exit_code, timed_out) = match &target {
                WaitTarget::Pane { pane_id } => {
                    let pane = state
                        .registry
                        .lock()
                        .await
                        .find_by_pane(*pane_id)
                        .and_then(|session| session.pane(*pane_id));
                    match pane {
                        Some(pane) => wait_for_pane(pane, deadline).await,
                        None => {
                            set_error(&mut response, ErrorCode::NotFound, "pane not found");
                            (None, false)
                        }
                    }
                }
                WaitTarget::Session { name } => {
                    let session = state.registry.lock().await.get(name);
                    match session {
                        Some(session) => {
                            let mut code = None;
                            let mut timed_out = false;
                            for pane in session.panes() {
                                let (pane_code, pane_timed_out) =
                                    wait_for_pane(pane, deadline).await;
                                if code.is_none() {
                                    code = pane_code;
                                }
                                if pane_timed_out {
                                    timed_out = true;
                                    break;
                                }
                            }
                            (code, timed_out)
                        }
                        None => {
                            set_error(&mut response, ErrorCode::NotFound, "session not found");
                            (None, false)
                        }
                    }
                }
            };
            response.wait = Some(WaitResult {
                target: target.clone(),
                exit_code,
                timed_out,
            });
        }
        Command::ListActions => {
            if client_caps & capabilities::client::ACTIONS == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "ACTIONS capability not requested",
                );
            } else {
                response.actions = state.action_infos();
            }
        }
        Command::GetKeymap => {
            if client_caps & capabilities::client::ACTIONS == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "ACTIONS capability not requested",
                );
            } else {
                response.keymap = Some(state.wire_keymap());
            }
        }
        Command::InvokeAction {
            action,
            args,
            focus_pane_id,
        } => {
            if client_caps & capabilities::client::ACTIONS == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "ACTIONS capability not requested",
                );
                return response;
            }
            let invocation = ActionInvocation { action, args };
            let requested_focus = focus_pane_id.or(*focus);
            let session = match requested_focus {
                Some(pane_id) => state.registry.lock().await.find_by_pane(pane_id),
                None => None,
            };
            let mut effective = requested_focus;
            match execute_action(state, session.as_ref(), &mut effective, &invocation).await {
                Ok(()) => {
                    *focus = effective;
                    if let Some(session) = &session {
                        response.layout = Some(response_layout(state, session, *focus));
                    }
                    state.publish_event(
                        session.as_ref().map(|session| session.id),
                        "action_invoked",
                        serde_json::json!({
                            "action": invocation.action,
                            "pane_id": effective,
                            "client_id": client_id,
                        }),
                    );
                }
                Err((code, message)) => set_error(&mut response, code, message),
            }
        }
        Command::ReloadKeymap { path } => {
            if client_caps & capabilities::client::ACTIONS == 0 {
                set_error(
                    &mut response,
                    ErrorCode::Unsupported,
                    "ACTIONS capability not requested",
                );
                return response;
            }
            match state.reload_keymap(path) {
                Ok(path) => {
                    state.publish_event(
                        None,
                        "keymap_changed",
                        serde_json::json!({ "path": path }),
                    );
                    state.publish_event(None, "actions_changed", serde_json::json!({}));
                    response.keymap = Some(state.wire_keymap());
                    response.actions = state.action_infos();
                }
                Err(err) => set_error(&mut response, ErrorCode::InvalidArgs, err),
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
            tracing::info!("shutdown requested");
            // Do not detach the control connection that is waiting for this
            // response; every other client gets a typed close.
            state.shutdown(Some(client_id));
        }
    }
    response
}

/// Build a layout for a command response using the requesting connection's
/// focus. Control-only connections get the first visible pane, which keeps the
/// response self-describing without inventing a global focus.
fn response_layout(state: &ServerState, session: &Session, focus: Option<u64>) -> Layout {
    let visible = session.visible_pane_ids();
    let focus = focus
        .filter(|pane_id| visible.contains(pane_id))
        .or_else(|| visible.first().copied());
    with_attached_count(state, session.layout(focus))
}

fn set_error(response: &mut CommandResponse, code: ErrorCode, message: impl std::fmt::Display) {
    response.ok = false;
    response.error = Some(ErrorInfo {
        code,
        message: message.to_string(),
    });
}

/// Send the typed slow-client error. The write gets a generous deadline: the
/// peer may not be reading yet, and cancelling mid-frame would destroy the
/// error itself.
async fn send_backpressure_error<W>(writer: &mut FrameWriter<W>) -> Result<(), ServerError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(
        Duration::from_secs(30),
        send_error(writer, ErrorCode::Busy, "client fell behind"),
    )
    .await;
    Ok(())
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
    use stitch_mux_protocol::FrameReader;

    #[tokio::test]
    async fn slow_client_error_is_typed_busy() {
        let (client, server) = tokio::io::duplex(4096);
        {
            let mut writer = FrameWriter::new(server);
            send_backpressure_error(&mut writer)
                .await
                .expect("error frame");
        }
        let mut reader = FrameReader::new(client);
        let frame = reader.recv_frame().await.expect("error frame");
        assert_eq!(frame.kind, MessageKind::Error);
        let error: ErrorInfo = decode_message(&frame).expect("typed error");
        assert_eq!(error.code, ErrorCode::Busy);
        assert_eq!(error.message, "client fell behind");
    }

    #[test]
    fn an_explicit_pane_owner_overrides_the_smallest_client_policy() {
        let state = ServerState::default();
        let large = state.register(&Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "large".into(),
            client_instance: "large".into(),
            view_mode: ViewMode::Structured,
            cols: 120,
            rows: 40,
            pixel_width: 0,
            pixel_height: 0,
            capabilities: 1,
        });
        let small = state.register(&Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "small".into(),
            client_instance: "small".into(),
            view_mode: ViewMode::Structured,
            cols: 70,
            rows: 20,
            pixel_width: 0,
            pixel_height: 0,
            capabilities: 1,
        });
        state.set_attached(large, 1, Some(1));
        state.set_attached(small, 1, Some(1));

        assert_eq!(
            state.canonical_size(1, BTreeSet::new()),
            Some(Size::new(70, 20))
        );
        assert_eq!(
            state.canonical_size(1, BTreeSet::from([large])),
            Some(Size::new(120, 40))
        );
        assert_eq!(
            state.canonical_size(1, BTreeSet::from([small])),
            Some(Size::new(70, 20))
        );
    }

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
        assert_eq!(
            state.canonical_size(1, BTreeSet::new()),
            Some(Size::new(70, 20))
        );
        state.unregister(second);
        assert_eq!(
            state.canonical_size(1, BTreeSet::new()),
            Some(Size::new(120, 40))
        );
        state.unregister(first);
        assert_eq!(state.canonical_size(1, BTreeSet::new()), None);
    }
}
