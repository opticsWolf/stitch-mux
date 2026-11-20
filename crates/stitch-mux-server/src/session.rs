//! Session lifecycle. A session owns its PTY panes, binary split layout,
//! authoritative screen state, and layout revision. It stays alive when every
//! client detaches.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use stitch_mux_core::{
    ActivitySummary, Direction, LayoutError, LayoutNode, PaneId, Rect, Size, SplitAxis, close_leaf,
    first_leaf, layout_contains, layout_leaves, resolve_layout, split_leaf, validate_layout,
};
use stitch_mux_protocol::{Attached, Layout, PaneGeometry, PaneInfo, PanePlacement, SessionInfo};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;

use crate::pane::{PaneError, PaneRuntime, PaneSpec};

const MIN_PANE: Size = Size::new(4, 2);
const PANE_BORDER: u16 = 1;
const LAYOUT_QUEUE: usize = 64;

/// Pane IDs are unique for the lifetime of the daemon, across sessions.
static NEXT_PANE_ID: AtomicU64 = AtomicU64::new(1);

fn next_pane_id() -> u64 {
    NEXT_PANE_ID.fetch_add(1, Ordering::SeqCst)
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Pane(#[from] PaneError),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error("session {0} already exists")]
    AlreadyExists(String),
    #[error("session {0} not found")]
    NotFound(String),
    #[error("pane {0} not found")]
    PaneNotFound(u64),
    #[error("at least one pane must remain")]
    LastPane,
    #[error("layout does not match the session panes")]
    LayoutMismatch,
    #[error("layout revision {expected} is stale; current is {current}")]
    StaleLayout { expected: u64, current: u64 },
}

pub struct Session {
    pub id: u64,
    name: StdMutex<String>,
    cols: AtomicU16,
    rows: AtomicU16,
    pub created_at_unix_ms: u64,
    layout: StdMutex<LayoutState>,
    panes: StdMutex<BTreeMap<u64, Arc<PaneRuntime>>>,
    /// Serialises layout mutations. Reading the tree, editing it, and bumping
    /// the revision has to be one atomic step: two concurrent splits used to
    /// rebuild from the same stale root, so one subtree silently disappeared.
    ops: AsyncMutex<()>,
    layout_tx: broadcast::Sender<Arc<Layout>>,
}

#[derive(Clone, Debug)]
struct LayoutState {
    root: LayoutNode,
    zoom: Option<u64>,
    revision: u64,
}

impl LayoutState {
    /// Zoom shows one pane without changing the tree. A stale zoom target
    /// (already closed) falls back to the full layout.
    fn effective(&self) -> LayoutNode {
        match self.zoom {
            Some(pane_id) if layout_contains(&self.root, PaneId::new(pane_id)) => {
                LayoutNode::leaf(PaneId::new(pane_id))
            }
            _ => self.root.clone(),
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1).max(1);
    }
}

impl Session {
    pub fn attached(&self, clients: u32, view_mode: stitch_mux_protocol::ViewMode) -> Attached {
        let statuses: Vec<_> = self
            .panes()
            .into_iter()
            .map(|pane| pane.program_status())
            .collect();
        let activity = ActivitySummary::from_statuses(statuses.iter().map(Option::as_ref));
        Attached {
            session_id: self.id,
            session_name: self.name(),
            view_mode,
            cols: self.cols.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            status_rows: 0,
            attached_clients: clients,
            layout_revision: self.revision(),
            activity,
        }
    }

    pub fn revision(&self) -> u64 {
        self.layout.lock().expect("layout").revision
    }

    pub fn name(&self) -> String {
        self.name.lock().expect("session name").clone()
    }

    pub fn rename(&self, new_name: &str) {
        *self.name.lock().expect("session name") = new_name.to_owned();
    }

    pub fn size(&self) -> Size {
        Size::new(
            self.cols.load(Ordering::Relaxed),
            self.rows.load(Ordering::Relaxed),
        )
    }

    pub fn root(&self) -> LayoutNode {
        self.layout.lock().expect("layout").root.clone()
    }

    fn effective_root(&self) -> LayoutNode {
        self.layout.lock().expect("layout").effective()
    }

    /// Resolve the authoritative tree. A viewport too small for a split degrades
    /// to the first pane instead of producing zero-size PTYs or a panic.
    fn resolved(&self) -> Vec<stitch_mux_core::ResolvedPane> {
        self.resolve_root(&self.effective_root(), self.size())
    }

    fn resolve_root(&self, root: &LayoutNode, size: Size) -> Vec<stitch_mux_core::ResolvedPane> {
        let area = Rect::new(0, 0, size.cols, size.rows);
        match resolve_layout(root, area, MIN_PANE, PANE_BORDER) {
            Ok(panes) => panes,
            Err(_) => {
                let Some(pane_id) = first_leaf(root) else {
                    return Vec::new();
                };
                vec![stitch_mux_core::ResolvedPane {
                    pane_id: pane_id.get(),
                    outer: area,
                    content: area.content(PANE_BORDER),
                }]
            }
        }
    }

    pub fn layout(&self, focus: Option<u64>) -> Layout {
        let (root, revision, size, zoomed_pane) = {
            let state = self.layout.lock().expect("layout");
            (
                state.effective(),
                state.revision,
                Size::new(
                    self.cols.load(Ordering::Relaxed),
                    self.rows.load(Ordering::Relaxed),
                ),
                state
                    .zoom
                    .filter(|pane_id| layout_contains(&state.root, PaneId::new(*pane_id))),
            )
        };
        let panes = self.panes.lock().expect("panes");
        let geometry = self
            .resolve_root(&root, size)
            .into_iter()
            .map(|resolved| {
                let title = panes
                    .get(&resolved.pane_id)
                    .map(|pane| pane.title())
                    .unwrap_or_default();
                PaneGeometry {
                    pane_id: resolved.pane_id,
                    outer: resolved.outer,
                    content: resolved.content,
                    title,
                }
            })
            .collect();
        Layout {
            session_id: self.id,
            revision,
            cols: size.cols,
            rows: size.rows,
            status_rows: 0,
            attached_clients: 0,
            focus,
            root,
            panes: geometry,
            zoomed_pane,
        }
    }

    pub fn subscribe_layout(&self) -> broadcast::Receiver<Arc<Layout>> {
        self.layout_tx.subscribe()
    }

    pub fn publish_layout(&self) {
        let _ = self.layout_tx.send(Arc::new(self.layout(None)));
    }

    pub fn pane(&self, pane_id: u64) -> Option<Arc<PaneRuntime>> {
        self.panes.lock().expect("panes").get(&pane_id).cloned()
    }

    pub fn panes(&self) -> Vec<Arc<PaneRuntime>> {
        self.panes
            .lock()
            .expect("panes")
            .values()
            .cloned()
            .collect()
    }

    pub fn pane_ids(&self) -> Vec<u64> {
        layout_leaves(&self.root())
            .into_iter()
            .map(PaneId::get)
            .collect()
    }

    /// Panes present in the currently effective layout. Zoom hides the others.
    pub fn visible_pane_ids(&self) -> Vec<u64> {
        layout_leaves(&self.effective_root())
            .into_iter()
            .map(PaneId::get)
            .collect()
    }

    fn resize_panes(&self) {
        let geometry = self.resolved();
        let panes = self.panes.lock().expect("panes");
        for resolved in geometry {
            if let Some(pane) = panes.get(&resolved.pane_id) {
                pane.resize(resolved.content.cols, resolved.content.rows);
            }
        }
    }

    /// Resize only when the canonical size actually changed.
    ///
    /// Attach/detach recomputation runs often; a no-op must not burn a layout
    /// revision or resize every PTY.
    pub fn resize(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        {
            let mut state = self.layout.lock().expect("layout");
            if cols == self.cols.load(Ordering::Relaxed)
                && rows == self.rows.load(Ordering::Relaxed)
            {
                return;
            }
            self.cols.store(cols, Ordering::Relaxed);
            self.rows.store(rows, Ordering::Relaxed);
            state.bump();
        }
        self.resize_panes();
        self.publish_layout();
    }

    pub fn focus_direction(&self, pane_id: u64, direction: Direction) -> Option<u64> {
        stitch_mux_core::focus_direction(&self.resolved(), PaneId::new(pane_id), direction)
            .map(PaneId::get)
    }

    pub async fn split(
        &self,
        pane_id: u64,
        axis: SplitAxis,
        ratio_bps: u16,
    ) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        let spec = {
            let panes = self.panes.lock().expect("panes");
            panes
                .get(&pane_id)
                .ok_or(SessionError::PaneNotFound(pane_id))?
                .spec()
                .clone()
        };
        let new_id = next_pane_id();
        let (root, size) = {
            let state = self.layout.lock().expect("layout");
            (
                split_leaf(
                    &state.root,
                    PaneId::new(pane_id),
                    PaneId::new(new_id),
                    axis,
                    ratio_bps,
                )?,
                self.size(),
            )
        };
        let (cols, rows) = resolve_layout(
            &root,
            Rect::new(0, 0, size.cols, size.rows),
            MIN_PANE,
            PANE_BORDER,
        )
        .ok()
        .and_then(|panes| {
            panes
                .into_iter()
                .find(|pane| pane.pane_id == new_id)
                .map(|pane| (pane.content.cols.max(1), pane.content.rows.max(1)))
        })
        .unwrap_or((2, 1));
        let pane = PaneRuntime::spawn(self.id, new_id, &spec, cols, rows).await?;
        self.panes.lock().expect("panes").insert(new_id, pane);
        {
            let mut state = self.layout.lock().expect("layout");
            state.root = root;
            state.zoom = None;
            state.bump();
        }
        self.resize_panes();
        self.publish_layout();
        Ok(self.layout(Some(pane_id)))
    }

    /// Spawn an explicitly specified child into a new split pane. Used by
    /// `Command::Run`; the session stays daemon-owned and detach-safe.
    pub async fn run(
        &self,
        pane_id: u64,
        spec: PaneSpec,
        axis: SplitAxis,
        ratio_bps: u16,
    ) -> Result<(u64, Layout), SessionError> {
        let _op = self.ops.lock().await;
        if !self.panes.lock().expect("panes").contains_key(&pane_id) {
            return Err(SessionError::PaneNotFound(pane_id));
        }
        let new_id = next_pane_id();
        let (root, size) = {
            let state = self.layout.lock().expect("layout");
            (
                split_leaf(
                    &state.root,
                    PaneId::new(pane_id),
                    PaneId::new(new_id),
                    axis,
                    ratio_bps,
                )?,
                self.size(),
            )
        };
        let (cols, rows) = resolve_layout(
            &root,
            Rect::new(0, 0, size.cols, size.rows),
            MIN_PANE,
            PANE_BORDER,
        )
        .ok()
        .and_then(|panes| {
            panes
                .into_iter()
                .find(|pane| pane.pane_id == new_id)
                .map(|pane| (pane.content.cols.max(1), pane.content.rows.max(1)))
        })
        .unwrap_or((2, 1));
        let pane = PaneRuntime::spawn(self.id, new_id, &spec, cols, rows).await?;
        self.panes.lock().expect("panes").insert(new_id, pane);
        {
            let mut state = self.layout.lock().expect("layout");
            state.root = root;
            state.zoom = None;
            state.bump();
        }
        self.resize_panes();
        self.publish_layout();
        Ok((new_id, self.layout(Some(pane_id))))
    }

    pub async fn close(&self, pane_id: u64) -> Result<Option<Layout>, SessionError> {
        let _op = self.ops.lock().await;
        let next_root = {
            let state = self.layout.lock().expect("layout");
            close_leaf(&state.root, PaneId::new(pane_id))?
        };
        let pane = {
            let mut panes = self.panes.lock().expect("panes");
            if panes.len() == 1 && panes.contains_key(&pane_id) {
                return Err(SessionError::LastPane);
            }
            panes
                .remove(&pane_id)
                .ok_or(SessionError::PaneNotFound(pane_id))?
        };
        let Some(next_root) = next_root else {
            return Ok(None);
        };
        {
            let mut state = self.layout.lock().expect("layout");
            state.root = next_root;
            if state.zoom == Some(pane_id) {
                state.zoom = None;
            }
            state.bump();
        }
        pane.kill().await;
        self.resize_panes();
        self.publish_layout();
        Ok(Some(self.layout(None)))
    }

    pub async fn set_layout(
        &self,
        root: LayoutNode,
        base_revision: u64,
    ) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        self.check_revision(base_revision)?;
        validate_layout(&root)?;
        let requested: BTreeSet<u64> = layout_leaves(&root).into_iter().map(PaneId::get).collect();
        let known: BTreeSet<u64> = self.panes.lock().expect("panes").keys().copied().collect();
        if requested != known {
            return Err(SessionError::LayoutMismatch);
        }
        {
            let mut state = self.layout.lock().expect("layout");
            // A canonical resize can bump the revision while this layout is
            // validated; never apply a request that was already stale.
            if base_revision != state.revision {
                return Err(SessionError::StaleLayout {
                    expected: base_revision,
                    current: state.revision,
                });
            }
            state.root = root;
            state.zoom = None;
            state.bump();
        }
        self.resize_panes();
        self.publish_layout();
        Ok(self.layout(None))
    }

    fn check_revision(&self, base_revision: u64) -> Result<(), SessionError> {
        let current = self.revision();
        if base_revision != current {
            return Err(SessionError::StaleLayout {
                expected: base_revision,
                current,
            });
        }
        Ok(())
    }

    pub async fn zoom(&self, pane_id: Option<u64>) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        if let Some(id) = pane_id
            && self.pane(id).is_none()
        {
            return Err(SessionError::PaneNotFound(id));
        }
        {
            let mut state = self.layout.lock().expect("layout");
            state.zoom = pane_id;
            state.bump();
        }
        self.resize_panes();
        self.publish_layout();
        Ok(self.layout(pane_id))
    }

    pub fn next_focus(&self, pane_id: u64) -> Option<u64> {
        let visible = self.visible_pane_ids();
        if visible.contains(&pane_id) {
            Some(pane_id)
        } else {
            visible.first().copied()
        }
    }

    pub fn info(&self, attached_clients: u32) -> SessionInfo {
        let panes: Vec<_> = self.panes().into_iter().collect();
        let last_activity_unix_ms = panes
            .iter()
            .map(|pane| pane.last_activity_unix_ms())
            .max()
            .unwrap_or(self.created_at_unix_ms);
        let infos: Vec<PaneInfo> = self
            .pane_ids()
            .into_iter()
            .filter_map(|id| panes.iter().find(|pane| pane.id == id))
            .map(|pane| {
                let meta = pane.meta();
                PaneInfo {
                    id: pane.id,
                    title: meta.title,
                    cwd: meta.cwd,
                    cols: meta.cols,
                    rows: meta.rows,
                    alive: pane.is_alive(),
                    placement: PanePlacement::Tiled,
                    window_id: Some(1),
                    owner_client_id: None,
                    program_status: pane.program_status(),
                }
            })
            .collect();
        let activity =
            ActivitySummary::from_statuses(infos.iter().map(|info| info.program_status.as_ref()));
        SessionInfo {
            id: self.id,
            name: self.name(),
            cols: self.cols.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            pane_count: infos.len() as u32,
            detached_pane_count: 0,
            window_count: 1,
            attached_clients,
            created_at_unix_ms: self.created_at_unix_ms,
            last_activity_unix_ms,
            activity,
            panes: infos,
        }
    }
}

#[derive(Default)]
pub struct SessionRegistry {
    sessions: BTreeMap<String, Arc<Session>>,
    next_id: u64,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<Session>> {
        self.sessions.get(name).cloned()
    }

    pub fn find_by_pane(&self, pane_id: u64) -> Option<Arc<Session>> {
        self.sessions
            .values()
            .find(|session| session.pane(pane_id).is_some())
            .cloned()
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .values()
            .map(|session| session.info(0))
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &mut self,
        name: &str,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
        env: Option<&[(String, String)]>,
        cols: u16,
        rows: u16,
        ensure: bool,
    ) -> Result<Arc<Session>, SessionError> {
        if let Some(existing) = self.get(name) {
            if ensure {
                return Ok(existing);
            }
            return Err(SessionError::AlreadyExists(name.to_owned()));
        }
        let session_id = self.next_id + 1;
        let pane_id = next_pane_id();
        let spec = PaneSpec::new(program, args, cwd, env);
        let pane = PaneRuntime::spawn(session_id, pane_id, &spec, cols, rows).await?;
        let panes = BTreeMap::from([(pane_id, Arc::clone(&pane))]);
        let (layout_tx, _) = broadcast::channel(LAYOUT_QUEUE);
        let session = Arc::new(Session {
            id: session_id,
            name: StdMutex::new(name.to_owned()),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            created_at_unix_ms: now_unix_ms(),
            layout: StdMutex::new(LayoutState {
                root: LayoutNode::leaf(PaneId::new(pane_id)),
                zoom: None,
                revision: 1,
            }),
            panes: StdMutex::new(panes),
            ops: AsyncMutex::new(()),
            layout_tx,
        });
        self.sessions.insert(name.to_owned(), Arc::clone(&session));
        self.next_id = session_id;
        Ok(session)
    }

    pub async fn kill(&mut self, name: &str) -> Result<(), SessionError> {
        let session = self
            .sessions
            .remove(name)
            .ok_or_else(|| SessionError::NotFound(name.to_owned()))?;
        for pane in session.panes() {
            pane.kill().await;
        }
        Ok(())
    }

    pub fn rename(&mut self, name: &str, new_name: &str) -> Result<(), SessionError> {
        if name == new_name {
            return Ok(());
        }
        if self.sessions.contains_key(new_name) {
            return Err(SessionError::AlreadyExists(new_name.to_owned()));
        }
        let session = self
            .sessions
            .remove(name)
            .ok_or_else(|| SessionError::NotFound(name.to_owned()))?;
        session.rename(new_name);
        self.sessions.insert(new_name.to_owned(), session);
        Ok(())
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_last_pane_close() {
        let mut registry = SessionRegistry::new();
        let session = registry
            .create("demo", "sh", &[], None, None, 80, 24, false)
            .await
            .unwrap();
        let pane_id = session.pane_ids()[0];
        assert!(matches!(
            session.close(pane_id).await,
            Err(SessionError::LastPane)
        ));
        let stale = session.set_layout(session.root(), 99).await;
        assert!(matches!(stale, Err(SessionError::StaleLayout { .. })));
        assert_eq!(session.layout(None).revision, 1);
    }
}
