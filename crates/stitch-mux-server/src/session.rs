//! Session lifecycle. A session owns its PTY panes, binary split layout,
//! authoritative screen state, and layout revision. It stays alive when every
//! client detaches.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use stitch_mux_core::{
    ActivitySummary, Direction, FloatBounds, FloatingPane, LayoutError, LayoutNode,
    MAX_LAYERS_PER_WINDOW, PaneId, Rect, Size, SplitAxis, Window, Workspace, close_leaf,
    first_leaf, layout_contains, layout_leaves, place_leaf, remove_leaf, resolve_layout,
    validate_layout,
};
use stitch_mux_protocol::{
    Attached, FloatingPaneInfo, Layout, PaneGeometry, PaneInfo, PanePlacement, SessionInfo,
    WindowFocus, WindowInfo,
};
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
    #[error("cannot close the last window")]
    LastWindow,
    #[error("window {0} not found")]
    WindowNotFound(u64),
    #[error("floating layer {0} not found")]
    LayerNotFound(u64),
    #[error("layout does not match the session panes")]
    LayoutMismatch,
    #[error("pane {0} is not tiled")]
    PaneNotTiled(u64),
    #[error("client {client} does not own pane {pane}")]
    NotOwner { client: u64, pane: u64 },
    #[error("layout revision {expected} is stale; current is {current}")]
    StaleLayout { expected: u64, current: u64 },
}

pub struct Session {
    pub id: u64,
    name: StdMutex<String>,
    cols: AtomicU16,
    rows: AtomicU16,
    pub created_at_unix_ms: u64,
    workspace: StdMutex<WorkspaceState>,
    panes: StdMutex<BTreeMap<u64, Arc<PaneRuntime>>>,
    /// Pane placement is independent of the tiled tree: detached panes keep
    /// running but are not leaves in any window.
    placements: StdMutex<BTreeMap<u64, PanePlacement>>,
    /// Optional size owner. When set, that client's viewport drives canonical
    /// PTY size; otherwise the smallest attached client wins.
    owners: StdMutex<BTreeMap<u64, u64>>,
    /// Serialises layout mutations. Reading the tree, editing it, and bumping
    /// the revision has to be one atomic step: two concurrent splits used to
    /// rebuild from the same stale root, so one subtree silently disappeared.
    ops: AsyncMutex<()>,
    layout_tx: broadcast::Sender<Arc<Layout>>,
}

#[derive(Clone, Debug)]
struct WorkspaceState {
    workspace: Workspace,
    revision: u64,
}

impl WorkspaceState {
    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1).max(1);
    }
}

static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(2);
static NEXT_LAYER_ID: AtomicU64 = AtomicU64::new(1);

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
        self.workspace.lock().expect("workspace").revision
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

    fn state(&self) -> WorkspaceState {
        self.workspace.lock().expect("workspace").clone()
    }

    /// Mutate the workspace atomically. On any validation error the previous
    /// workspace is restored, so a rejected window/layer operation cannot
    /// leave half-applied geometry behind.
    fn apply_workspace<F>(&self, mutate: F) -> Result<(), SessionError>
    where
        F: FnOnce(&mut WorkspaceState) -> Result<(), SessionError>,
    {
        let mut guard = self.workspace.lock().expect("workspace");
        let previous = guard.clone();
        if let Err(err) = mutate(&mut guard) {
            *guard = previous;
            return Err(err);
        }
        if let Err(err) = guard.workspace.validate() {
            *guard = previous;
            return Err(err.into());
        }
        guard.bump();
        Ok(())
    }

    pub fn root(&self) -> LayoutNode {
        self.state().workspace.active().root.clone()
    }

    pub fn has_window(&self, window_id: u64) -> bool {
        self.state().workspace.windows.contains_key(&window_id)
    }

    pub fn has_layer(&self, layer_id: u64) -> bool {
        self.state()
            .workspace
            .windows
            .values()
            .any(|window| window.floating.contains_key(&layer_id))
    }

    pub fn active_window_id(&self) -> u64 {
        self.state().workspace.active_window
    }

    fn effective_root(&self) -> LayoutNode {
        self.state().workspace.active().effective_root()
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

    fn window_geometry(&self, window: &Window) -> Vec<PaneGeometry> {
        let size = self.size();
        let panes = self.panes.lock().expect("panes");
        let mut geometry: Vec<PaneGeometry> = self
            .resolve_root(&window.effective_root(), size)
            .into_iter()
            .map(|resolved| PaneGeometry {
                pane_id: resolved.pane_id,
                outer: resolved.outer,
                content: resolved.content,
                title: panes
                    .get(&resolved.pane_id)
                    .map(|pane| pane.title())
                    .unwrap_or_default(),
            })
            .collect();
        for layer in window.floating.values().filter(|layer| !layer.minimized) {
            let rect = layer.bounds.to_rect(size);
            geometry.push(PaneGeometry {
                pane_id: layer.pane_id,
                outer: rect,
                content: rect,
                title: panes
                    .get(&layer.pane_id)
                    .map(|pane| pane.title())
                    .unwrap_or_default(),
            });
        }
        geometry
    }

    /// The active-window projection is always present for terminal clients;
    /// `windows` carries the full workspace for GUI clients.
    pub fn layout(&self, focus: Option<u64>) -> Layout {
        let state = self.state();
        let revision = state.revision;
        let size = self.size();
        let windows: Vec<WindowInfo> = state
            .workspace
            .order
            .iter()
            .map(|id| &state.workspace.windows[id])
            .map(|window| WindowInfo {
                id: window.id,
                name: window.name.clone(),
                root: window.effective_root(),
                zoomed_pane: window
                    .zoom
                    .filter(|pane_id| layout_contains(&window.root, PaneId::new(*pane_id))),
                floating: window
                    .floating
                    .values()
                    .map(|layer| FloatingPaneInfo {
                        layer_id: layer.layer_id,
                        pane_id: layer.pane_id,
                        bounds: layer.bounds,
                        z: layer.z,
                        minimized: layer.minimized,
                    })
                    .collect(),
                panes: self.window_geometry(window),
            })
            .collect();
        let active = windows
            .iter()
            .find(|window| window.id == state.workspace.active_window)
            .expect("active window is projected")
            .clone();
        Layout {
            session_id: self.id,
            revision,
            cols: size.cols,
            rows: size.rows,
            status_rows: 0,
            attached_clients: 0,
            focus,
            root: active.root,
            panes: active.panes,
            zoomed_pane: active.zoomed_pane,
            windows,
            active_window_id: Some(active.id),
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

    /// Panes present in the active window, including visible floating layers.
    pub fn visible_pane_ids(&self) -> Vec<u64> {
        let state = self.state();
        let window = state.workspace.active();
        let mut ids: Vec<u64> = layout_leaves(&window.effective_root())
            .into_iter()
            .map(PaneId::get)
            .collect();
        ids.extend(
            window
                .floating
                .values()
                .filter(|layer| !layer.minimized)
                .map(|layer| layer.pane_id),
        );
        ids
    }

    fn resize_panes(&self) {
        let state = self.state();
        let size = self.size();
        let panes = self.panes.lock().expect("panes");
        for window in state.workspace.windows.values() {
            for resolved in self.resolve_root(&window.effective_root(), size) {
                if let Some(pane) = panes.get(&resolved.pane_id) {
                    pane.resize(resolved.content.cols, resolved.content.rows);
                }
            }
            for layer in window.floating.values() {
                let rect = layer.bounds.to_rect(size);
                if let Some(pane) = panes.get(&layer.pane_id) {
                    pane.resize(rect.cols.max(1), rect.rows.max(1));
                }
            }
        }
    }

    /// Resize only when the canonical size actually changed. Floating bounds
    /// are fractions, so this one pass updates every window and layer.
    pub fn resize(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        {
            let mut guard = self.workspace.lock().expect("workspace");
            if cols == self.cols.load(Ordering::Relaxed)
                && rows == self.rows.load(Ordering::Relaxed)
            {
                return;
            }
            self.cols.store(cols, Ordering::Relaxed);
            self.rows.store(rows, Ordering::Relaxed);
            guard.bump();
        }
        self.resize_panes();
        self.publish_layout();
    }

    pub fn focus_direction(&self, pane_id: u64, direction: Direction) -> Option<u64> {
        stitch_mux_core::focus_direction(&self.resolved(), PaneId::new(pane_id), direction)
            .map(PaneId::get)
    }

    fn resolved(&self) -> Vec<stitch_mux_core::ResolvedPane> {
        self.resolve_root(&self.effective_root(), self.size())
    }

    pub fn placement(&self, pane_id: u64) -> PanePlacement {
        self.placements
            .lock()
            .expect("placements")
            .get(&pane_id)
            .copied()
            .unwrap_or(PanePlacement::Tiled)
    }

    pub fn owner(&self, pane_id: u64) -> Option<u64> {
        self.owners.lock().expect("owners").get(&pane_id).copied()
    }

    pub fn owners(&self) -> BTreeMap<u64, u64> {
        self.owners.lock().expect("owners").clone()
    }

    pub fn claim(&self, pane_id: u64, client_id: u64, take: bool) -> Result<(), SessionError> {
        if !self.panes.lock().expect("panes").contains_key(&pane_id) {
            return Err(SessionError::PaneNotFound(pane_id));
        }
        let mut owners = self.owners.lock().expect("owners");
        match owners.get(&pane_id) {
            Some(current) if *current != client_id => {
                if take {
                    owners.insert(pane_id, client_id);
                } else {
                    return Err(SessionError::NotOwner {
                        client: client_id,
                        pane: pane_id,
                    });
                }
            }
            _ => {
                owners.insert(pane_id, client_id);
            }
        }
        Ok(())
    }

    pub fn release(&self, pane_id: u64, client_id: u64) -> Result<(), SessionError> {
        let mut owners = self.owners.lock().expect("owners");
        match owners.get(&pane_id) {
            Some(current) if *current == client_id => {
                owners.remove(&pane_id);
                Ok(())
            }
            Some(current) => Err(SessionError::NotOwner {
                client: client_id,
                pane: *current,
            }),
            None => Ok(()),
        }
    }

    pub async fn create_window(&self, name: Option<String>) -> Result<u64, SessionError> {
        let window_id = NEXT_WINDOW_ID.fetch_add(1, Ordering::SeqCst);
        let name = name.unwrap_or_else(|| format!("window {window_id}"));
        self.apply_workspace(|state| {
            state.workspace.windows.insert(
                window_id,
                Window::new(window_id, name.clone(), LayoutNode::empty()),
            );
            state.workspace.order.push(window_id);
            Ok(())
        })?;
        self.resize_panes();
        self.publish_layout();
        Ok(window_id)
    }

    pub async fn close_window(&self, window_id: u64) -> Result<(), SessionError> {
        let _op = self.ops.lock().await;
        let panes: Vec<Arc<PaneRuntime>> = {
            let state = self.state();
            let Some(window) = state.workspace.windows.get(&window_id) else {
                return Err(SessionError::WindowNotFound(window_id));
            };
            if state.workspace.windows.len() == 1 {
                return Err(SessionError::LastWindow);
            }
            let mut ids: Vec<u64> = layout_leaves(&window.root)
                .into_iter()
                .map(PaneId::get)
                .collect();
            ids.extend(window.floating.values().map(|layer| layer.pane_id));
            ids.iter()
                .filter_map(|id| self.panes.lock().expect("panes").get(id).cloned())
                .collect()
        };
        self.apply_workspace(|state| {
            state.workspace.windows.remove(&window_id);
            state.workspace.order.retain(|id| *id != window_id);
            if state.workspace.active_window == window_id {
                state.workspace.active_window = state.workspace.order[0];
            }
            Ok(())
        })?;
        {
            let mut placed = self.placements.lock().expect("placements");
            let mut owners = self.owners.lock().expect("owners");
            for pane in &panes {
                placed.remove(&pane.id);
                owners.remove(&pane.id);
            }
        }
        {
            let mut tiled = self.panes.lock().expect("panes");
            for pane in &panes {
                tiled.remove(&pane.id);
            }
        }
        for pane in panes {
            pane.kill().await;
        }
        self.resize_panes();
        self.publish_layout();
        Ok(())
    }

    pub fn rename_window(&self, window_id: u64, name: &str) -> Result<(), SessionError> {
        self.apply_workspace(|state| {
            let window = state
                .workspace
                .windows
                .get_mut(&window_id)
                .ok_or(SessionError::WindowNotFound(window_id))?;
            window.name = name.to_owned();
            Ok(())
        })?;
        self.publish_layout();
        Ok(())
    }

    pub fn move_window(&self, window_id: u64, index: usize) -> Result<(), SessionError> {
        self.apply_workspace(|state| {
            state.workspace.move_window(window_id, index)?;
            Ok(())
        })?;
        self.publish_layout();
        Ok(())
    }

    pub fn focus_window(&self, target: WindowFocus) -> Result<u64, SessionError> {
        let next = {
            let mut state = self.workspace.lock().expect("workspace");
            let previous = state.clone();
            let result = match target {
                WindowFocus::Id(id) if state.workspace.windows.contains_key(&id) => {
                    state.workspace.active_window = id;
                    Ok(id)
                }
                WindowFocus::Id(id) => Err(SessionError::WindowNotFound(id)),
                WindowFocus::Next => state
                    .workspace
                    .focus_step(1)
                    .map(|()| state.workspace.active_window)
                    .map_err(Into::into),
                WindowFocus::Previous => state
                    .workspace
                    .focus_step(-1)
                    .map(|()| state.workspace.active_window)
                    .map_err(Into::into),
            };
            match result {
                Ok(id) => {
                    if let Err(err) = state.workspace.validate() {
                        *state = previous;
                        return Err(err.into());
                    }
                    state.bump();
                    id
                }
                Err(err) => {
                    *state = previous;
                    return Err(err);
                }
            }
        };
        self.resize_panes();
        self.publish_layout();
        Ok(next)
    }

    fn window_id_for_pane(&self, pane_id: u64) -> Result<u64, SessionError> {
        self.state()
            .workspace
            .window_for_pane(pane_id)
            .ok_or(SessionError::PaneNotFound(pane_id))
    }

    /// Remove a pane from whichever window owns it, without killing the child.
    pub async fn detach_pane(&self, pane_id: u64) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        if !self.panes.lock().expect("panes").contains_key(&pane_id) {
            return Err(SessionError::PaneNotFound(pane_id));
        }
        let placement = self.placement(pane_id);
        if matches!(
            placement,
            PanePlacement::Detached | PanePlacement::Floating { .. }
        ) {
            return Err(SessionError::PaneNotTiled(pane_id));
        }
        self.apply_workspace(|state| {
            let window_id = state
                .workspace
                .window_for_pane(pane_id)
                .ok_or(SessionError::PaneNotFound(pane_id))?;
            let window = state
                .workspace
                .windows
                .get_mut(&window_id)
                .ok_or(SessionError::WindowNotFound(window_id))?;
            window.root = remove_leaf(&window.root, PaneId::new(pane_id))?;
            if window.zoom == Some(pane_id) {
                window.zoom = None;
            }
            Ok(())
        })?;
        self.placements
            .lock()
            .expect("placements")
            .insert(pane_id, PanePlacement::Detached);
        self.owners.lock().expect("owners").remove(&pane_id);
        self.resize_panes();
        self.publish_layout();
        Ok(self.layout(None))
    }

    /// Place a detached pane back into a window's tiled tree.
    pub async fn place_pane(
        &self,
        pane_id: u64,
        window_id: Option<u64>,
        anchor_pane_id: Option<u64>,
        axis: SplitAxis,
        ratio_bps: u16,
    ) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        if !self.panes.lock().expect("panes").contains_key(&pane_id) {
            return Err(SessionError::PaneNotFound(pane_id));
        }
        if matches!(self.placement(pane_id), PanePlacement::Floating { .. }) {
            return Err(SessionError::PaneNotTiled(pane_id));
        }
        self.apply_workspace(|state| {
            let target = window_id.unwrap_or(state.workspace.active_window);
            let window = state
                .workspace
                .windows
                .get_mut(&target)
                .ok_or(SessionError::WindowNotFound(target))?;
            window.root = place_leaf(
                &window.root,
                anchor_pane_id.map(PaneId::new),
                PaneId::new(pane_id),
                axis,
                ratio_bps,
            )?;
            Ok(())
        })?;
        self.placements
            .lock()
            .expect("placements")
            .insert(pane_id, PanePlacement::Tiled);
        self.resize_panes();
        self.publish_layout();
        Ok(self.layout(None))
    }

    /// Convert a tiled pane into a bounded floating layer in the same window.
    pub async fn float_pane(
        &self,
        pane_id: u64,
        bounds: FloatBounds,
    ) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        bounds.validate()?;
        if !self.panes.lock().expect("panes").contains_key(&pane_id) {
            return Err(SessionError::PaneNotFound(pane_id));
        }
        if self.placement(pane_id) != PanePlacement::Tiled {
            return Err(SessionError::PaneNotTiled(pane_id));
        }
        let layer_id = NEXT_LAYER_ID.fetch_add(1, Ordering::SeqCst);
        self.apply_workspace(|state| {
            let window_id = state
                .workspace
                .window_for_pane(pane_id)
                .ok_or(SessionError::PaneNotFound(pane_id))?;
            let window = state
                .workspace
                .windows
                .get_mut(&window_id)
                .ok_or(SessionError::WindowNotFound(window_id))?;
            window.root = remove_leaf(&window.root, PaneId::new(pane_id))?;
            if window.zoom == Some(pane_id) {
                window.zoom = None;
            }
            window.floating.insert(
                layer_id,
                FloatingPane {
                    layer_id,
                    pane_id,
                    bounds,
                    z: window.max_z().saturating_add(1),
                    minimized: false,
                },
            );
            Ok(())
        })?;
        self.placements
            .lock()
            .expect("placements")
            .insert(pane_id, PanePlacement::Floating { layer_id });
        self.resize_panes();
        self.publish_layout();
        Ok(self.layout(None))
    }

    /// Return a floating pane to a window's tiled tree.
    pub async fn tile_pane(
        &self,
        pane_id: u64,
        window_id: Option<u64>,
        anchor_pane_id: Option<u64>,
        axis: SplitAxis,
        ratio_bps: u16,
    ) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        let PanePlacement::Floating { layer_id } = self.placement(pane_id) else {
            return Err(SessionError::PaneNotTiled(pane_id));
        };
        self.apply_workspace(|state| {
            let target = window_id.unwrap_or(state.workspace.active_window);
            let window = state
                .workspace
                .windows
                .get_mut(&target)
                .ok_or(SessionError::WindowNotFound(target))?;
            window
                .floating
                .remove(&layer_id)
                .ok_or(SessionError::LayerNotFound(layer_id))?;
            window.root = place_leaf(
                &window.root,
                anchor_pane_id.map(PaneId::new),
                PaneId::new(pane_id),
                axis,
                ratio_bps,
            )?;
            Ok(())
        })?;
        self.placements
            .lock()
            .expect("placements")
            .insert(pane_id, PanePlacement::Tiled);
        self.resize_panes();
        self.publish_layout();
        Ok(self.layout(None))
    }

    pub async fn set_floating_pane(
        &self,
        layer_id: u64,
        bounds: Option<FloatBounds>,
        z: Option<u32>,
        minimized: Option<bool>,
    ) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        if let Some(bounds) = bounds {
            bounds.validate()?;
        }
        self.apply_workspace(|state| {
            let mut found = false;
            for window in state.workspace.windows.values_mut() {
                if let Some(layer) = window.floating.get_mut(&layer_id) {
                    if let Some(bounds) = bounds {
                        layer.bounds = bounds;
                    }
                    if let Some(z) = z {
                        layer.z = z;
                    }
                    if let Some(minimized) = minimized {
                        layer.minimized = minimized;
                    }
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(SessionError::LayerNotFound(layer_id));
            }
            Ok(())
        })?;
        self.resize_panes();
        self.publish_layout();
        Ok(self.layout(None))
    }

    pub async fn split(
        &self,
        pane_id: u64,
        axis: SplitAxis,
        ratio_bps: u16,
    ) -> Result<Layout, SessionError> {
        let window_id = self.window_id_for_pane(pane_id)?;
        self.run_in_window(window_id, Some(pane_id), None, axis, ratio_bps)
            .await
            .map(|(_, layout)| layout)
    }

    /// Spawn a child into a window's tiled tree. `anchor` splits an existing
    /// pane; `None` creates a new split against the whole window.
    pub async fn run_in_window(
        &self,
        window_id: u64,
        anchor_pane_id: Option<u64>,
        spec: Option<PaneSpec>,
        axis: SplitAxis,
        ratio_bps: u16,
    ) -> Result<(u64, Layout), SessionError> {
        let _op = self.ops.lock().await;
        let spec = match spec {
            Some(spec) => spec,
            None => {
                let panes = self.panes.lock().expect("panes");
                let anchor = anchor_pane_id.ok_or(SessionError::PaneNotFound(0))?;
                panes
                    .get(&anchor)
                    .ok_or(SessionError::PaneNotFound(anchor))?
                    .spec()
                    .clone()
            }
        };
        let new_id = next_pane_id();
        let root = {
            let state = self.state();
            let window = state
                .workspace
                .windows
                .get(&window_id)
                .ok_or(SessionError::WindowNotFound(window_id))?;
            place_leaf(
                &window.root,
                anchor_pane_id.map(PaneId::new),
                PaneId::new(new_id),
                axis,
                ratio_bps,
            )?
        };
        let (cols, rows) = resolve_layout(
            &root,
            Rect::new(0, 0, self.size().cols, self.size().rows),
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
        let applied = self.apply_workspace(|state| {
            let window = state
                .workspace
                .windows
                .get_mut(&window_id)
                .ok_or(SessionError::WindowNotFound(window_id))?;
            window.root = root.clone();
            window.zoom = None;
            Ok(())
        });
        if let Err(err) = applied {
            pane.kill().await;
            return Err(err);
        }
        self.panes.lock().expect("panes").insert(new_id, pane);
        self.placements
            .lock()
            .expect("placements")
            .insert(new_id, PanePlacement::Tiled);
        self.resize_panes();
        self.publish_layout();
        Ok((new_id, self.layout(Some(new_id))))
    }

    /// Spawn a new floating pane with deterministic bounds.
    pub async fn run_floating(
        &self,
        window_id: u64,
        spec: PaneSpec,
        bounds: FloatBounds,
    ) -> Result<(u64, u64, Layout), SessionError> {
        let _op = self.ops.lock().await;
        bounds.validate()?;
        let state = self.state();
        let window = state
            .workspace
            .windows
            .get(&window_id)
            .ok_or(SessionError::WindowNotFound(window_id))?;
        if window.floating.len() >= MAX_LAYERS_PER_WINDOW {
            return Err(LayoutError::TooManyLayers.into());
        }
        let layer_id = NEXT_LAYER_ID.fetch_add(1, Ordering::SeqCst);
        let pane_id = next_pane_id();
        let rect = bounds.to_rect(self.size());
        let pane =
            PaneRuntime::spawn(self.id, pane_id, &spec, rect.cols.max(1), rect.rows.max(1)).await?;
        let applied = self.apply_workspace(|state| {
            let window = state
                .workspace
                .windows
                .get_mut(&window_id)
                .ok_or(SessionError::WindowNotFound(window_id))?;
            window.floating.insert(
                layer_id,
                FloatingPane {
                    layer_id,
                    pane_id,
                    bounds,
                    z: window.max_z().saturating_add(1),
                    minimized: false,
                },
            );
            Ok(())
        });
        if let Err(err) = applied {
            pane.kill().await;
            return Err(err);
        }
        self.panes.lock().expect("panes").insert(pane_id, pane);
        self.placements
            .lock()
            .expect("placements")
            .insert(pane_id, PanePlacement::Floating { layer_id });
        self.resize_panes();
        self.publish_layout();
        Ok((pane_id, layer_id, self.layout(Some(pane_id))))
    }

    /// Backwards-compatible `Run` entry point: split against the anchor pane.
    pub async fn run(
        &self,
        pane_id: u64,
        spec: PaneSpec,
        axis: SplitAxis,
        ratio_bps: u16,
    ) -> Result<(u64, Layout), SessionError> {
        let window_id = self.window_id_for_pane(pane_id)?;
        self.run_in_window(window_id, Some(pane_id), Some(spec), axis, ratio_bps)
            .await
    }

    pub async fn close(&self, pane_id: u64) -> Result<Option<Layout>, SessionError> {
        let _op = self.ops.lock().await;
        let placement = self.placement(pane_id);
        let window_id = self.state().workspace.window_for_pane(pane_id);
        let next_root = if placement == PanePlacement::Tiled {
            let state = self.state();
            let window = state
                .workspace
                .windows
                .get(&window_id.ok_or(SessionError::PaneNotFound(pane_id))?)
                .ok_or(SessionError::PaneNotFound(pane_id))?;
            close_leaf(&window.root, PaneId::new(pane_id))?
        } else {
            None
        };
        let pane = {
            let mut panes = self.panes.lock().expect("panes");
            panes
                .remove(&pane_id)
                .ok_or(SessionError::PaneNotFound(pane_id))?
        };
        self.placements.lock().expect("placements").remove(&pane_id);
        self.owners.lock().expect("owners").remove(&pane_id);
        let empty = self.panes.lock().expect("panes").is_empty();
        self.apply_workspace(|state| {
            if empty {
                for window in state.workspace.windows.values_mut() {
                    window.root = LayoutNode::empty();
                    window.zoom = None;
                    window.floating.clear();
                }
            } else if let Some(next_root) = &next_root {
                let window = state
                    .workspace
                    .windows
                    .get_mut(&window_id.ok_or(SessionError::PaneNotFound(pane_id))?)
                    .ok_or(SessionError::PaneNotFound(pane_id))?;
                window.root = next_root.clone();
                if window.zoom == Some(pane_id) {
                    window.zoom = None;
                }
            } else if let PanePlacement::Floating { layer_id } = placement {
                for window in state.workspace.windows.values_mut() {
                    window.floating.remove(&layer_id);
                }
            }
            Ok(())
        })?;
        pane.kill().await;
        if empty {
            self.publish_layout();
            return Ok(None);
        }
        self.resize_panes();
        self.publish_layout();
        Ok(Some(self.layout(None)))
    }

    pub async fn set_layout(
        &self,
        root: LayoutNode,
        base_revision: u64,
        window_id: Option<u64>,
        floating: Option<Vec<FloatingPaneInfo>>,
    ) -> Result<Layout, SessionError> {
        let _op = self.ops.lock().await;
        self.check_revision(base_revision)?;
        validate_layout(&root)?;
        let requested: BTreeSet<u64> = layout_leaves(&root).into_iter().map(PaneId::get).collect();
        let state = self.state();
        let target = window_id.unwrap_or(state.workspace.active_window);
        let window = state
            .workspace
            .windows
            .get(&target)
            .ok_or(SessionError::WindowNotFound(target))?;
        let current: BTreeSet<u64> = layout_leaves(&window.root)
            .into_iter()
            .map(PaneId::get)
            .collect();
        if requested != current {
            return Err(SessionError::LayoutMismatch);
        }
        let layers = floating.map(|layers| {
            layers
                .into_iter()
                .map(|layer| {
                    (
                        layer.layer_id,
                        FloatingPane {
                            layer_id: layer.layer_id,
                            pane_id: layer.pane_id,
                            bounds: layer.bounds,
                            z: layer.z,
                            minimized: layer.minimized,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>()
        });
        if let Some(layers) = &layers {
            let requested_layers: BTreeSet<u64> = window
                .floating
                .values()
                .map(|layer| layer.layer_id)
                .collect();
            let supplied_layers: BTreeSet<u64> = layers.keys().copied().collect();
            if requested_layers != supplied_layers {
                return Err(SessionError::LayoutMismatch);
            }
            for layer in layers.values() {
                layer.bounds.validate()?;
                if !matches!(self.placement(layer.pane_id), PanePlacement::Floating { layer_id } if layer_id == layer.layer_id)
                {
                    return Err(SessionError::PaneNotTiled(layer.pane_id));
                }
            }
        }
        self.apply_workspace(|state| {
            let window = state
                .workspace
                .windows
                .get_mut(&target)
                .ok_or(SessionError::WindowNotFound(target))?;
            window.root = root.clone();
            window.zoom = None;
            if let Some(layers) = &layers {
                window.floating = layers.clone();
            }
            Ok(())
        })?;
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
        self.apply_workspace(|state| {
            state.workspace.active_mut().zoom = pane_id;
            Ok(())
        })?;
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
        let placements = self.placements.lock().expect("placements");
        let owners = self.owners.lock().expect("owners");
        let state = self.state();
        let infos: Vec<PaneInfo> = panes
            .iter()
            .map(|pane| {
                let meta = pane.meta();
                PaneInfo {
                    id: pane.id,
                    title: meta.title,
                    cwd: meta.cwd,
                    cols: meta.cols,
                    rows: meta.rows,
                    alive: pane.is_alive(),
                    placement: placements
                        .get(&pane.id)
                        .copied()
                        .unwrap_or(PanePlacement::Tiled),
                    window_id: state.workspace.window_for_pane(pane.id),
                    owner_client_id: owners.get(&pane.id).copied(),
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
            detached_pane_count: infos
                .iter()
                .filter(|info| info.placement == PanePlacement::Detached)
                .count() as u32,
            window_count: state.workspace.windows.len() as u32,
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

    pub fn get_by_id(&self, id: u64) -> Option<Arc<Session>> {
        self.sessions
            .values()
            .find(|session| session.id == id)
            .cloned()
    }

    pub fn find_by_window(&self, window_id: u64) -> Option<Arc<Session>> {
        self.sessions
            .values()
            .find(|session| session.has_window(window_id))
            .cloned()
    }

    pub fn find_by_layer(&self, layer_id: u64) -> Option<Arc<Session>> {
        self.sessions
            .values()
            .find(|session| session.has_layer(layer_id))
            .cloned()
    }

    pub fn first(&self) -> Option<Arc<Session>> {
        self.sessions.values().next().cloned()
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
        let placements = BTreeMap::from([(pane_id, PanePlacement::Tiled)]);
        let (layout_tx, _) = broadcast::channel(LAYOUT_QUEUE);
        let session = Arc::new(Session {
            id: session_id,
            name: StdMutex::new(name.to_owned()),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            created_at_unix_ms: now_unix_ms(),
            workspace: StdMutex::new(WorkspaceState {
                workspace: Workspace::new(LayoutNode::leaf(PaneId::new(pane_id))),
                revision: 1,
            }),
            panes: StdMutex::new(panes),
            placements: StdMutex::new(placements),
            owners: StdMutex::new(BTreeMap::new()),
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

    const TEST_SHELL: &str = if cfg!(windows) { "cmd.exe" } else { "sh" };

    #[tokio::test]
    async fn repeated_detach_place_close_does_not_retain_a_pane_runtime() {
        let mut registry = SessionRegistry::new();
        let session = registry
            .create("recycle", TEST_SHELL, &[], None, None, 80, 24, false)
            .await
            .unwrap();
        let pane_id = session.pane_ids()[0];
        let pane = session.pane(pane_id).expect("pane runtime");
        let weak = Arc::downgrade(&pane);
        drop(pane);

        for _ in 0..8 {
            session.detach_pane(pane_id).await.expect("detach");
            session
                .place_pane(pane_id, None, None, SplitAxis::Columns, 5000)
                .await
                .expect("place");
        }
        assert!(
            weak.upgrade().is_some(),
            "same runtime should survive recycling"
        );

        session.close(pane_id).await.expect("close");
        // The reader task owns an Arc too; aborted tasks release it once the
        // runtime gets a chance to poll their cancellation.
        for _ in 0..16 {
            tokio::task::yield_now().await;
            if weak.upgrade().is_none() {
                break;
            }
        }
        assert!(
            weak.upgrade().is_none(),
            "closed pane runtime must be released"
        );
        assert!(session.panes().is_empty());
        assert!(session.owners().is_empty());
    }

    #[tokio::test]
    async fn closing_a_window_waits_for_in_flight_pane_operations() {
        let mut registry = SessionRegistry::new();
        let session = registry
            .create("workspace", TEST_SHELL, &[], None, None, 80, 24, false)
            .await
            .unwrap();
        let window = session.create_window(None).await.unwrap();
        let operation = session.ops.lock().await;
        let close = session.close_window(window);
        tokio::pin!(close);
        tokio::select! {
            biased;
            result = &mut close => panic!("close bypassed operation lock: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        assert!(session.has_window(window));
        drop(operation);
        close.await.unwrap();

        let result = session
            .run_floating(
                window,
                PaneSpec::new(TEST_SHELL, &[], None, None),
                FloatBounds::new(0, 0, 5000, 5000),
            )
            .await;
        assert!(matches!(result, Err(SessionError::WindowNotFound(id)) if id == window));
        assert_eq!(session.pane_ids().len(), 1);
        registry.kill("workspace").await.unwrap();
    }

    #[tokio::test]
    async fn closing_the_last_pane_is_explicit() {
        let mut registry = SessionRegistry::new();
        let session = registry
            .create("demo", TEST_SHELL, &[], None, None, 80, 24, false)
            .await
            .unwrap();
        let pane_id = session.pane_ids()[0];
        assert!(session.close(pane_id).await.unwrap().is_none());
        assert!(session.pane_ids().is_empty());
        assert!(matches!(session.root(), LayoutNode::Empty));
    }
}
