//! Bounded, coalescing update queue for one attached client.
//!
//! Pane readers must never block on a slow GUI. Updates are offered with
//! `try_send`; if the queue stays full, the connection is closed with a typed
//! `Busy` error instead of growing memory without bound.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::core::RevisionCoalescer;

#[cfg(feature = "bench-profile")]
use crate::bench_profile;
use crate::protocol::{
    FrameError, FrameWriter, PaneDelta, StructuredPaneFrame, client_caps,
};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;

use crate::server::pane::PaneUpdate;

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

    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
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
    emitters: BTreeMap<u64, RevisionCoalescer>,
    in_flight: BTreeMap<u64, (u64, Instant)>,
    max_delta_hz: u32,
    client_caps: u64,
}

const DEFAULT_MAX_DELTA_HZ: u32 = 60;

fn configured_max_delta_hz() -> u32 {
    std::env::var("STITCH_MUX_MAX_DELTA_HZ")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_DELTA_HZ)
        .clamp(1, 1_000)
}

fn ack_deadline() -> Duration {
    let ms = std::env::var("STITCH_MUX_ACK_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(50)
        .clamp(1, 5_000);
    Duration::from_millis(ms)
}

impl UpdateCoalescer {
    pub fn with_capabilities(client_caps: u64) -> Self {
        Self {
            max_delta_hz: configured_max_delta_hz(),
            client_caps,
            ..Self::default()
        }
    }

    /// Records an acknowledgement. Stale or mismatched acks are ignored.
    pub fn ack(&mut self, pane_id: u64, seq: u64) {
        if self.in_flight.get(&pane_id).is_some_and(|(pending, _)| *pending == seq) {
            self.in_flight.remove(&pane_id);
        }
    }

    /// Panes whose last frame was not acknowledged within the deadline.
    pub fn deadline_panes(&self) -> Vec<u64> {
        let deadline = ack_deadline();
        self.in_flight
            .iter()
            .filter(|(_, (_, sent_at))| sent_at.elapsed() >= deadline)
            .map(|(pane_id, _)| *pane_id)
            .collect()
    }

    /// Clears in-flight state after a full snapshot has been sent.
    pub fn clear_in_flight(&mut self, pane_id: u64) {
        self.in_flight.remove(&pane_id);
    }

    fn emitter(&mut self, pane_id: u64) -> &mut RevisionCoalescer {
        let rate = self.max_delta_hz;
        self.emitters
            .entry(pane_id)
            .or_insert_with(|| RevisionCoalescer::new(rate))
    }

    fn arm(&mut self, pane_id: u64, seq: u64) {
        self.in_flight.insert(pane_id, (seq, Instant::now()));
    }

    pub fn set_capabilities(&mut self, client_caps: u64) {
        self.client_caps = client_caps;
    }

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

    /// Record the sequence of a snapshot sent outside `flush` (attach, resync,
    /// layout change). Without this the rewritten delta sequence restarts at 1
    /// while the client's `last_seq` is the snapshot's, so it would discard
    /// every delta until the two spaces meet again.
    pub fn seed(&mut self, pane_id: u64, seq: u64) {
        self.last_sent.insert(pane_id, seq);
        self.emitter(pane_id).seed(seq, Instant::now());
        self.in_flight.remove(&pane_id);
        // The snapshot supersedes any delta queued before it; `ordered` still
        // holds those entries, and `flush` skips a delta without a pending row.
        self.deltas.remove(&pane_id);
    }

    /// Send all pending updates without rate limiting. Used by tests and by
    /// paths that must deliver the final state immediately.
    pub async fn flush<W>(&mut self, writer: &mut FrameWriter<W>) -> Result<(), FrameError>
    where
        W: AsyncWrite + Unpin,
    {
        self.flush_inner(writer, true).await
    }

    /// Send pending updates with revision coalescing. `force` bypasses the rate
    /// window, which is required when the update queue is empty so the final
    /// authoritative state is never stranded.
    pub async fn flush_paced<W>(
        &mut self,
        writer: &mut FrameWriter<W>,
        force: bool,
    ) -> Result<(), FrameError>
    where
        W: AsyncWrite + Unpin,
    {
        self.flush_inner(writer, force).await
    }

    async fn flush_inner<W>(
        &mut self,
        writer: &mut FrameWriter<W>,
        force: bool,
    ) -> Result<(), FrameError>
    where
        W: AsyncWrite + Unpin,
    {
        use crate::protocol::MessageKind;

        let now = Instant::now();
        let mut ready = BTreeMap::new();
        for (pane_id, delta) in std::mem::take(&mut self.deltas) {
            let decision = self.emitter(pane_id).observe(delta.seq, now);
            let polled = self.emitter(pane_id).poll(now).is_some();
            if force || decision.emit || polled {
                ready.insert(pane_id, delta);
            } else {
                #[cfg(feature = "bench-profile")]
                bench_profile::add(&bench_profile::DELTAS_DEFERRED, 1);
                self.deltas.insert(pane_id, delta);
            }
        }

        let mut exits = Vec::new();
        for update in std::mem::take(&mut self.ordered) {
            match update.as_ref() {
                PaneUpdate::Snapshot(snapshot) => {
                    // A newer snapshot (attach, resync, layout change) may have
                    // been written while this one waited in the queue. Sending
                    // the older screen would roll the client back and leave the
                    // following deltas relative to the wrong base.
                    if snapshot.seq < self.last_sent.get(&snapshot.pane_id).copied().unwrap_or(0) {
                        continue;
                    }
                    self.last_sent.insert(snapshot.pane_id, snapshot.seq);
                    self.emitter(snapshot.pane_id).seed(snapshot.seq, now);
                    self.in_flight.remove(&snapshot.pane_id);
                    #[cfg(feature = "bench-profile")]
                    let send_start = bench_profile::begin();
                    let sent = writer.send(MessageKind::PaneSnapshot, snapshot).await;
                    #[cfg(feature = "bench-profile")]
                    {
                        bench_profile::record(&bench_profile::SERVER_SEND_NS, send_start);
                        bench_profile::add(&bench_profile::SERVER_FRAMES, 1);
                    }
                    sent?;
                }
                PaneUpdate::Delta(_) => {
                    // Deltas are emitted from `ready` below so a deferred pane
                    // cannot be duplicated by a later queue entry.
                }
                PaneUpdate::Exit(exit) => exits.push(exit.clone()),
            }
        }

        for (pane_id, mut pending) in ready {
            let next = self.last_sent.entry(pane_id).or_insert(0);
            *next = next.saturating_add(1);
            pending.seq = *next;
            #[cfg(feature = "bench-profile")]
            let send_start = bench_profile::begin();
            let sent = if self.client_caps & client_caps::STRUCTURED_PANE_FRAME != 0 {
                #[cfg(feature = "bench-profile")]
                let encode_start = bench_profile::begin();
                let encoded = StructuredPaneFrame::from_delta(&pending).encode();
                #[cfg(feature = "bench-profile")]
                bench_profile::record(&bench_profile::WIRE_ENCODE_NS, encode_start);
                match encoded {
                    Ok(frame) => writer.send_frame(&frame).await,
                    Err(error) => Err(error),
                }
            } else {
                writer.send(MessageKind::PaneDelta, &pending).await
            };
            #[cfg(feature = "bench-profile")]
            {
                bench_profile::record(&bench_profile::SERVER_SEND_NS, send_start);
                bench_profile::add(&bench_profile::SERVER_FRAMES, 1);
            }
            sent?;
            self.arm(pane_id, pending.seq);
        }

        for exit in exits {
            #[cfg(feature = "bench-profile")]
            let send_start = bench_profile::begin();
            let sent = writer.send(MessageKind::Exit, &exit).await;
            #[cfg(feature = "bench-profile")]
            {
                bench_profile::record(&bench_profile::SERVER_SEND_NS, send_start);
                bench_profile::add(&bench_profile::SERVER_FRAMES, 1);
            }
            sent?;
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        self.ordered.clear();
        self.deltas.clear();
        self.last_sent.clear();
        self.emitters.clear();
        self.in_flight.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Cursor, Modes, Row};
    use crate::protocol::{PaneSnapshot, RowUpdate};

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
            notification: None,
            progress: None,

            alt_screen: false,
            scrollback_len: 0,
            total_lines: 0,
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

        let mut reader = crate::protocol::FrameReader::new(&mut rx);
        let first = reader.recv_frame().await.unwrap();
        let second = reader.recv_frame().await.unwrap();
        let first: PaneDelta = crate::protocol::decode_message(&first).unwrap();
        let second: PaneDelta = crate::protocol::decode_message(&second).unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(first.title, "title-2");
        assert_eq!(first.cursor.x, 2);
        assert!(first.modes.alt_screen);
        assert_eq!(first.rows.len(), 2);
        assert_eq!(second.seq, 1);
        assert_eq!(second.rows.len(), 1);
    }

    #[tokio::test]
    async fn seeding_after_a_snapshot_keeps_the_client_sequence_contiguous() {
        let (mut tx, mut rx) = tokio::io::duplex(16 * 1024);
        let mut coalescer = UpdateCoalescer::default();
        coalescer.add(delta(1, 30, 0));
        coalescer.seed(1, 22);
        coalescer.add(delta(1, 31, 1));
        {
            let mut writer = FrameWriter::new(&mut tx);
            coalescer.flush(&mut writer).await.unwrap();
        }
        let mut reader = crate::protocol::FrameReader::new(&mut rx);
        let frame = reader.recv_frame().await.unwrap();
        let delta: PaneDelta = crate::protocol::decode_message(&frame).unwrap();
        assert_eq!(
            delta.seq, 23,
            "delta must follow the attach snapshot sequence"
        );
        assert_eq!(delta.title, "title-31");
        assert_eq!(delta.rows.len(), 1);
    }

    #[tokio::test]
    async fn a_queued_snapshot_never_rolls_back_a_newer_one() {
        let (mut tx, mut rx) = tokio::io::duplex(16 * 1024);
        let mut coalescer = UpdateCoalescer::default();
        let stale = Arc::new(PaneUpdate::Snapshot(PaneSnapshot {
            pane_id: 1,
            seq: 11,
            cols: 1,
            rows: 1,
            cells: vec![Row {
                cells: Vec::new(),
                wrapped: false,
            }],
            cursor: Cursor::default(),
            modes: Modes::default(),
            title: "stale".into(),
            cwd: None,
            foreground: None,
            shell_marks: None,
            program_status: None,
            notification: None,
            progress: None,

            alt_screen: false,
            scrollback_len: 0,
            total_lines: 0,
            scrollback_bytes: 0,
        }));
        coalescer.add(Arc::clone(&stale));
        // A newer snapshot (seq 13) was written directly by send_session_state.
        coalescer.seed(1, 13);
        coalescer.add(delta(1, 14, 0));
        {
            let mut writer = FrameWriter::new(&mut tx);
            coalescer.flush(&mut writer).await.unwrap();
        }
        let mut reader = crate::protocol::FrameReader::new(&mut rx);
        let frame = reader.recv_frame().await.unwrap();
        let delta: PaneDelta = crate::protocol::decode_message(&frame).unwrap();
        assert_eq!(delta.seq, 14, "the stale snapshot must be dropped");
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
            notification: None,
            progress: None,

            alt_screen: false,
            scrollback_len: 0,
            total_lines: 0,
            scrollback_bytes: 0,
        }));
        for _ in 0..CLIENT_UPDATE_QUEUE + 3 {
            offer_update(&tx, &overflow, Arc::clone(&snapshot));
        }
        assert_eq!(queue.overflows(), 3);
        assert_eq!(queue.drain_batch().len(), CLIENT_UPDATE_QUEUE);
    }
}
