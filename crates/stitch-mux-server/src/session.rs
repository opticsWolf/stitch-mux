//! Session lifecycle. A session owns one pane in M1 and stays alive when
//! every client detaches.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use stitch_mux_core::{ActivitySummary, LayoutNode, PaneId, Rect};
use stitch_mux_protocol::{
    Attached, Layout, PaneGeometry, PaneInfo, PanePlacement, PlacementKind, SessionInfo,
};
use thiserror::Error;

use crate::pane::{PaneError, PaneRuntime};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Pane(#[from] PaneError),
    #[error("session {0} already exists")]
    AlreadyExists(String),
    #[error("session {0} not found")]
    NotFound(String),
}

pub struct Session {
    pub id: u64,
    pub name: String,
    cols: AtomicU16,
    rows: AtomicU16,
    pub created_at_unix_ms: u64,
    pub pane: Arc<PaneRuntime>,
}

impl Session {
    pub fn attached(&self, clients: u32) -> Attached {
        Attached {
            session_id: self.id,
            session_name: self.name.clone(),
            view_mode: stitch_mux_protocol::ViewMode::Structured,
            cols: self.cols.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            status_rows: 0,
            attached_clients: clients,
            layout_revision: 1,
            activity: ActivitySummary::default(),
        }
    }

    /// M1 layout: the one pane fills the client viewport.
    pub fn layout(&self) -> Layout {
        Layout {
            session_id: self.id,
            revision: 1,
            cols: self.cols.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            status_rows: 0,
            focus: Some(self.pane.id),
            panes: vec![PaneGeometry {
                pane_id: self.pane.id,
                outer: Rect::new(
                    0,
                    0,
                    self.cols.load(Ordering::Relaxed),
                    self.rows.load(Ordering::Relaxed),
                ),
                content: Rect::new(
                    0,
                    0,
                    self.cols.load(Ordering::Relaxed),
                    self.rows.load(Ordering::Relaxed),
                ),
                title: self.pane.snapshot().title,
            }],
        }
    }

    pub fn root_layout(&self) -> LayoutNode {
        LayoutNode::leaf(PaneId(self.pane.id))
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.pane.resize(cols, rows);
        self.cols.store(cols, Ordering::Relaxed);
        self.rows.store(rows, Ordering::Relaxed);
    }

    pub fn info(&self, attached_clients: u32) -> SessionInfo {
        let snapshot = self.pane.snapshot();
        SessionInfo {
            id: self.id,
            name: self.name.clone(),
            cols: self.cols.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            pane_count: 1,
            detached_pane_count: 0,
            window_count: 1,
            attached_clients,
            created_at_unix_ms: self.created_at_unix_ms,
            last_activity_unix_ms: now_unix_ms(),
            activity: ActivitySummary::default(),
            panes: vec![PaneInfo {
                id: self.pane.id,
                title: snapshot.title,
                cwd: snapshot.cwd,
                alive: self.pane.is_alive(),
                placement: PanePlacement {
                    kind: PlacementKind::Tiled,
                    layer_id: None,
                },
                window_id: Some(1),
                owner_client_id: None,
                program_status: snapshot.program_status,
            }],
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
        let pane_id = session_id * 100 + 1;
        let pane =
            PaneRuntime::spawn(session_id, pane_id, program, args, cwd, env, cols, rows).await?;
        let session = Arc::new(Session {
            id: session_id,
            name: name.to_owned(),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            created_at_unix_ms: now_unix_ms(),
            pane,
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
        session.pane.kill().await;
        Ok(())
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
