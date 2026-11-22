//! Per-pane revision coalescing for client-facing deltas.
//!
//! This is a rate limiter that never discards the newest state. The first
//! revision after an idle period is emitted immediately. Revisions observed
//! inside the rate window are collapsed to the newest one, which is emitted
//! when the window opens or when the caller forces a flush.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CoalescedRevision {
    /// Emit the observed revision now.
    pub emit: bool,
    /// A newer revision is waiting for the next window.
    pub pending: bool,
}

#[derive(Clone, Debug)]
pub struct RevisionCoalescer {
    interval: Duration,
    last_emitted: Option<(u64, Instant)>,
    pending: Option<u64>,
}

impl RevisionCoalescer {
    /// Creates a coalescer capped at `max_per_second` emissions. A rate of zero
    /// disables emission except for explicit forced flushes.
    pub fn new(max_per_second: u32) -> Self {
        let interval = if max_per_second == 0 {
            Duration::MAX
        } else {
            Duration::from_nanos(1_000_000_000 / u64::from(max_per_second))
        };
        Self {
            interval,
            last_emitted: None,
            pending: None,
        }
    }

    pub fn observe(&mut self, revision: u64, now: Instant) -> CoalescedRevision {
        if self.last_emitted.is_some_and(|(last, _)| last == revision) {
            return CoalescedRevision {
                emit: false,
                pending: self.pending.is_some(),
            };
        }
        if self.window_open(now) {
            self.record(revision, now);
            return CoalescedRevision {
                emit: true,
                pending: false,
            };
        }
        self.pending = Some(revision);
        CoalescedRevision {
            emit: false,
            pending: true,
        }
    }

    /// Promotes a pending revision when the rate window opens.
    pub fn poll(&mut self, now: Instant) -> Option<u64> {
        if !self.window_open(now) {
            return None;
        }
        let revision = self.pending.take()?;
        self.record(revision, now);
        Some(revision)
    }

    /// Emits the newest pending revision regardless of the rate window.
    pub fn force(&mut self, now: Instant) -> Option<u64> {
        let revision = self.pending.take()?;
        self.record(revision, now);
        Some(revision)
    }

    /// Resets the revision baseline after a snapshot or resync.
    pub fn seed(&mut self, revision: u64, now: Instant) {
        self.pending = None;
        self.record(revision, now);
    }

    pub fn pending(&self) -> Option<u64> {
        self.pending
    }

    fn window_open(&self, now: Instant) -> bool {
        self.last_emitted
            .map(|(_, at)| now.duration_since(at) >= self.interval)
            .unwrap_or(true)
    }

    fn record(&mut self, revision: u64, now: Instant) {
        self.last_emitted = Some((revision, now));
    }
}

#[cfg(test)]
mod tests {
    use super::RevisionCoalescer;
    use std::time::{Duration, Instant};

    #[test]
    fn first_revision_is_emitted_immediately() {
        let mut coalescer = RevisionCoalescer::new(60);
        let decision = coalescer.observe(7, Instant::now());
        assert!(decision.emit);
        assert!(!decision.pending);
    }

    #[test]
    fn only_newest_revision_survives_a_window() {
        let mut coalescer = RevisionCoalescer::new(60);
        let base = Instant::now();
        assert!(coalescer.observe(1, base).emit);
        assert!(!coalescer.observe(2, base + Duration::from_millis(1)).emit);
        assert!(!coalescer.observe(3, base + Duration::from_millis(2)).emit);
        assert_eq!(coalescer.pending(), Some(3));
        assert_eq!(coalescer.poll(base + coalescer.interval), Some(3));
    }

    #[test]
    fn force_emits_the_newest_pending_revision() {
        let mut coalescer = RevisionCoalescer::new(1);
        let base = Instant::now();
        assert!(coalescer.observe(1, base).emit);
        assert!(!coalescer.observe(2, base + Duration::from_millis(1)).emit);
        assert_eq!(coalescer.force(base + Duration::from_millis(2)), Some(2));
    }
}
