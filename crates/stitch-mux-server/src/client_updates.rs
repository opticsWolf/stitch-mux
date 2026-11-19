//! Bounded, coalescing update queue for one attached client.
//!
//! Pane readers must never block on a slow GUI. Updates are offered with
//! `try_send`; if the queue stays full, the connection is closed with a typed
//! `Busy` error instead of growing memory without bound.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use stitch_mux_protocol::{FrameError, FrameWriter, PaneDelta};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;

use crate::pane::PaneUpdate;

pub const CLIENT_UPDATE_QUEUE: usize = 64;
pub const MAX_CLIENT_OVERFLOWS: u32 = 1;
const MAX_DRAIN_PER_WAKE: usize = 256;

pub struct ClientUpdateQueue {
    tx: mpsc::Sender<Arc<PaneUpdate>>,
    rx: mpsc::Receiver<Arc<PaneUpdate>>,
    overflow: Arc<AtomicU32>,
}

impl Default for ClientUpdateQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientUpdateQueue {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(CLIENT_UPDATE_QUEUE);
        Self {
            tx,
            rx,
            overflow: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn sender(&self) -> mpsc::Sender<Arc<PaneUpdate>> {
        self.tx.clone()
    }

    pub fn overflow_counter(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.overflow)
    }

    pub async fn recv(&mut self) -> Option<Arc<PaneUpdate>> {
        self.rx.recv().await
    }

    /// Drain a bounded batch so one busy client cannot starve control frames.
    pub fn drain_batch(&mut self) -> Vec<Arc<PaneUpdate>> {
        let mut batch = Vec::new();
        while batch.len() < MAX_DRAIN_PER_WAKE {
            match self.rx.try_recv() {
                Ok(update) => batch.push(update),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
        batch
    }

    pub fn overflows(&self) -> u32 {
        self.overflow.load(Ordering::Relaxed)
    }
}

/// Offer an update without ever blocking a PTY reader.
pub fn offer_update(
    tx: &mpsc::Sender<Arc<PaneUpdate>>,
    overflow: &AtomicU32,
    update: Arc<PaneUpdate>,
) {
    match tx.try_send(update) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            overflow.fetch_add(1, Ordering::Relaxed);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

/// Coalesces pending deltas per pane while preserving the latest cursor,
/// modes, title, metadata, and newest row for each changed row.
#[derive(Default)]
pub struct UpdateCoalescer {
    ordered: Vec<Arc<PaneUpdate>>,
    deltas: BTreeMap<u64, PaneDelta>,
    last_sent: BTreeMap<u64, u64>,
}

impl UpdateCoalescer {
    pub fn add(&mut self, update: Arc<PaneUpdate>) {
        match update.as_ref() {
            PaneUpdate::Delta(delta) => {
                match self.deltas.get_mut(&delta.pane_id) {
                    Some(pending) => {
                        for row in &delta.rows {
                            pending.rows.retain(|existing| existing.row != row.row);
                            pending.rows.push(row.clone());
                        }
                        pending.seq = delta.seq;
                        pending.cursor = delta.cursor;
                        pending.modes = delta.modes;
                        pending.title = delta.title.clone();
                        pending.cwd = delta.cwd.clone();
                        pending.foreground = delta.foreground.clone();
                        pending.shell_marks = delta.shell_marks.clone();
                        pending.program_status = delta.program_status.clone();
                        pending.alt_screen = delta.alt_screen;
                        pending.scrollback_len = delta.scrollback_len;
                        pending.scrollback_bytes = delta.scrollback_bytes;
                        pending.bell |= delta.bell;
                    }
                    None => {
                        self.deltas.insert(delta.pane_id, (*delta).clone());
                    }
                }
                self.ordered.push(update);
            }
            PaneUpdate::Snapshot(snapshot) => {
                self.last_sent.insert(snapshot.pane_id, snapshot.seq);
                self.deltas.remove(&snapshot.pane_id);
                self.ordered.push(update);
            }
            PaneUpdate::Exit(_) => self.ordered.push(update),
        }
    }

    /// Send all pending updates. Delta sequence numbers are made contiguous for
    /// this client so coalescing never looks like a protocol gap.
    pub async fn flush<W>(&mut self, writer: &mut FrameWriter<W>) -> Result<(), FrameError>
    where
        W: AsyncWrite + Unpin,
    {
        use stitch_mux_protocol::MessageKind;

        for update in self.ordered.drain(..) {
            match update.as_ref() {
                PaneUpdate::Snapshot(snapshot) => {
                    self.last_sent.insert(snapshot.pane_id, snapshot.seq);
                    writer.send(MessageKind::PaneSnapshot, snapshot).await?;
                }
                PaneUpdate::Delta(delta) => {
                    let Some(mut pending) = self.deltas.remove(&delta.pane_id) else {
                        continue;
                    };
                    let next = self.last_sent.entry(delta.pane_id).or_insert(0);
                    *next = next.saturating_add(1);
                    pending.seq = *next;
                    writer.send(MessageKind::PaneDelta, &pending).await?;
                }
                PaneUpdate::Exit(exit) => {
                    writer.send(MessageKind::Exit, exit).await?;
                }
            }
        }
        self.deltas.clear();
        Ok(())
    }

    pub fn reset(&mut self) {
        self.ordered.clear();
        self.deltas.clear();
        self.last_sent.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stitch_mux_core::{Cursor, Modes, Row};
    use stitch_mux_protocol::{PaneSnapshot, RowUpdate};

    fn delta(pane_id: u64, seq: u64, row: u16) -> Arc<PaneUpdate> {
        Arc::new(PaneUpdate::Delta(PaneDelta {
            pane_id,
            seq,
            rows: vec![RowUpdate {
                row,
                cells: Vec::new(),
                wrapped: false,
            }],
            cursor: Cursor {
                x: seq as u16,
                ..Cursor::default()
            },
            modes: Modes {
                alt_screen: seq.is_multiple_of(2),
                ..Modes::default()
            },
            title: format!("title-{seq}"),
            cwd: None,
            foreground: None,
            shell_marks: None,
            program_status: None,
            alt_screen: false,
            scrollback_len: 0,
            scrollback_bytes: 0,
            bell: false,
        }))
    }

    #[tokio::test]
    async fn coalesces_rows_per_pane_and_keeps_latest_metadata() {
        let (mut tx, mut rx) = tokio::io::duplex(16 * 1024);
        let mut coalescer = UpdateCoalescer::default();
        coalescer.add(delta(1, 1, 0));
        coalescer.add(delta(1, 2, 1));
        coalescer.add(delta(2, 1, 0));
        {
            let mut writer = FrameWriter::new(&mut tx);
            coalescer.flush(&mut writer).await.unwrap();
        }

        let mut reader = stitch_mux_protocol::FrameReader::new(&mut rx);
        let first = reader.recv_frame().await.unwrap();
        let second = reader.recv_frame().await.unwrap();
        let first: PaneDelta = stitch_mux_protocol::decode_message(&first).unwrap();
        let second: PaneDelta = stitch_mux_protocol::decode_message(&second).unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(first.title, "title-2");
        assert_eq!(first.cursor.x, 2);
        assert!(first.modes.alt_screen);
        assert_eq!(first.rows.len(), 2);
        assert_eq!(second.seq, 1);
        assert_eq!(second.rows.len(), 1);
    }

    #[tokio::test]
    async fn queue_never_blocks_and_reports_overflow() {
        let mut queue = ClientUpdateQueue::new();
        let tx = queue.sender();
        let overflow = queue.overflow_counter();
        let snapshot = Arc::new(PaneUpdate::Snapshot(PaneSnapshot {
            pane_id: 1,
            seq: 1,
            cols: 1,
            rows: 1,
            cells: vec![Row {
                cells: Vec::new(),
                wrapped: false,
            }],
            cursor: Cursor::default(),
            modes: Modes::default(),
            title: "sh".into(),
            cwd: None,
            foreground: None,
            shell_marks: None,
            program_status: None,
            alt_screen: false,
            scrollback_len: 0,
            scrollback_bytes: 0,
        }));
        for _ in 0..CLIENT_UPDATE_QUEUE + 3 {
            offer_update(&tx, &overflow, Arc::clone(&snapshot));
        }
        assert_eq!(queue.overflows(), 3);
        assert_eq!(queue.drain_batch().len(), CLIENT_UPDATE_QUEUE);
    }
}
