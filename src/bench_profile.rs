//! Opt-in phase counters for the headless benchmark harness.
//!
//! Enabled only by the `bench-profile` feature. Normal builds do no timing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) static PANE_FEED_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static PANE_DIFF_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static PANE_PUBLISH_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static PANE_INGESTS: AtomicU64 = AtomicU64::new(0);
pub(crate) static PANE_ROWS_CHANGED: AtomicU64 = AtomicU64::new(0);
pub(crate) static SERVER_SEND_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static SERVER_FRAMES: AtomicU64 = AtomicU64::new(0);
pub(crate) static CLIENT_APPLY_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static CLIENT_FRAMES: AtomicU64 = AtomicU64::new(0);
pub(crate) static WIRE_ENCODE_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static DELTAS_DEFERRED: AtomicU64 = AtomicU64::new(0);
pub(crate) static FULL_REPAINTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub pane_feed_ns: u64,
    pub pane_diff_ns: u64,
    pub pane_publish_ns: u64,
    pub pane_ingests: u64,
    pub pane_rows_changed: u64,
    pub server_send_ns: u64,
    pub server_frames: u64,
    pub client_apply_ns: u64,
    pub client_frames: u64,
    pub wire_encode_ns: u64,
    pub deltas_deferred: u64,
    pub full_repaints: u64,
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn reset() {
    PANE_FEED_NS.store(0, Ordering::Relaxed);
    PANE_DIFF_NS.store(0, Ordering::Relaxed);
    PANE_PUBLISH_NS.store(0, Ordering::Relaxed);
    PANE_INGESTS.store(0, Ordering::Relaxed);
    PANE_ROWS_CHANGED.store(0, Ordering::Relaxed);
    SERVER_SEND_NS.store(0, Ordering::Relaxed);
    SERVER_FRAMES.store(0, Ordering::Relaxed);
    CLIENT_APPLY_NS.store(0, Ordering::Relaxed);
    CLIENT_FRAMES.store(0, Ordering::Relaxed);
    WIRE_ENCODE_NS.store(0, Ordering::Relaxed);
    DELTAS_DEFERRED.store(0, Ordering::Relaxed);
    FULL_REPAINTS.store(0, Ordering::Relaxed);
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        pane_feed_ns: PANE_FEED_NS.load(Ordering::Relaxed),
        pane_diff_ns: PANE_DIFF_NS.load(Ordering::Relaxed),
        pane_publish_ns: PANE_PUBLISH_NS.load(Ordering::Relaxed),
        pane_ingests: PANE_INGESTS.load(Ordering::Relaxed),
        pane_rows_changed: PANE_ROWS_CHANGED.load(Ordering::Relaxed),
        server_send_ns: SERVER_SEND_NS.load(Ordering::Relaxed),
        server_frames: SERVER_FRAMES.load(Ordering::Relaxed),
        client_apply_ns: CLIENT_APPLY_NS.load(Ordering::Relaxed),
        client_frames: CLIENT_FRAMES.load(Ordering::Relaxed),
        wire_encode_ns: WIRE_ENCODE_NS.load(Ordering::Relaxed),
        deltas_deferred: DELTAS_DEFERRED.load(Ordering::Relaxed),
        full_repaints: FULL_REPAINTS.load(Ordering::Relaxed),
    }
}

pub(crate) fn begin() -> Option<Instant> {
    ENABLED.load(Ordering::Relaxed).then(Instant::now)
}

pub(crate) fn record(slot: &AtomicU64, start: Option<Instant>) {
    if let Some(start) = start {
        slot.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

pub(crate) fn add(slot: &AtomicU64, value: u64) {
    if ENABLED.load(Ordering::Relaxed) {
        slot.fetch_add(value, Ordering::Relaxed);
    }
}
