# stitch-mux improvement plan

Branch: `dev_imporove`  
Base commit: `29735ad test(bench): profile the output pipeline by phase`  
Date: 2026-11-22

## Execution status

Implemented in this branch:

- Phase 0 baseline artifact, phase counters, and `scripts/check-local-pty.sh`.
- Phase 1a compact `CellText` with inline 4-byte storage and serde round-trip.
- Phase 1c flat `StructuredPaneFrame` with fixed cells, row records, cluster/link
  interning, capability negotiation, and legacy `PaneDelta` fallback.
- Phase 2 revision coalescing, client frame acknowledgements, deadline full
  repaint hook, and forced final flush when the update queue is empty.
- Phase 3 stateful outer-PTY DSR bootstrap that handles split request prefixes.
- Phase 6 stable wire ledger with direction/status metadata and conformance test.
- Phase 8 report provenance, host/profile fields, and omitted-lane-aware report
  structure.

Still deferred as separate high-risk changes:

- Phase 1b numeric `LinkId` in the in-memory `Cell` (wire frames already intern
  links; the core type still uses `Option<String>`).
- Phase 2c explicit row-shift records (current safe behavior recomputes hashes
  for all viewport rows).
- Phase 4 dedicated PTY writer thread and bounded output ring.
- Phase 5 style-run render encoder and synchronized output.
- Phase 7 Windows job-object lifecycle policy.

Final headless benchmark after the implemented changes:

```text
screen oracle failures: 0
terminal_output_completion p50: ~296.7 ms
terminal_output_ingest_rate p50: ~3.40 MiB/s
terminal_input_to_visible p50:  ~15.7 ms
server frames: ~7113
wire encode: ~0.036 ms/frame (instrumented)
```

Against the frozen baseline this is throughput-neutral to slightly better with
zero correctness regressions; the remaining serial cost is still pane row
diffing and conversion. That is the justification for the deferred phases.

This plan turns the parser/screen/mux benchmark work and the RMUX / Zellij / wtmux
code review into an implementation roadmap. It is deliberately conservative:
every change is capability-gated, benchmarked against the local `stitch-pty`, and
must pass the existing screen oracle before it can be kept.

## 0. Evidence we are optimizing against

### Current measurements

Latest valid local-linked mux p50 values:

| Metric | p50 |
|---|---:|
| `terminal_output_ingest_rate` | ~3.38 MiB/s |
| `terminal_output_completion` | ~296 ms |
| `terminal_input_to_visible` | ~15.35 ms |
| `output_trigger_ipc_ms` | ~0.287 ms |
| `raw_pty_ascii_mb_s` | ~20.8–23.4 MB/s |
| `raw_pty_cursor_mb_s` | ~22–29 MB/s |

With the opt-in `bench-profile` feature, the serial output path attributes
approximately:

| Phase | ms/sample |
|---|---:|
| pane row diff + wire-cell conversion | ~205 |
| screen feed | ~77 |
| server frame write | ~32 |
| client apply | ~63 |
| publish | ~1 |

Timers perturb absolutes, so these are attribution signals, not SLA numbers.

### Failed experiments that must not be repeated blindly

- **2 ms publish throttling** broke the screen oracle and doubled input-to-visible
  latency. The safe replacement is revision coalescing plus a final full snapshot.
- **Scroll-aware partial hashing** broke the oracle because skipped rows kept stale
  hashes. Any partial-diff scheme must update hashes for every row affected by a
  scroll or carry explicit row-shift records.
- **2 MiB publish batching** did not help and risked dropped deltas.
- Naive frame coalescing without sequence rewriting causes client sequence gaps.

### Review conclusions

- **RMUX** has the most directly useful data-plane ideas: lazy plain-ASCII rows,
  inline cell text, fixed parameter arrays, revision coalescing, bounded output
  rings, DSR bootstrap, and guarded Windows job objects.
- **Zellij** has the strongest wire/render protocol: fixed-layout `WireCell`,
  row records, cluster/link interning, row shifts, ack-based frame pacing, paused
  parser for host queries, and a dedicated PTY writer thread.
- **wtmux** has useful terminal-side rendering robustness: style-run batching,
  grid-column reanchoring, risky-glyph handling, dirty-line rendering, and
  synchronized output. It is not a detach/attach mux reference.

## 1. Rules for all phases

1. Keep the headless default lane headless. No GUI/TUI requirement.
2. Always verify the local engine:

   ```sh
   cargo tree -p stitch-mux | grep stitch-pty
   ```

   Expected:

   ```text
   stitch-pty v0.16.0 (D:\User\Documents\Python\stitch-pty)
   ```

3. Never claim “faster/slower than tmux/RMUX/Zellij” without same-outer-PTY
   numbers from the same run, same geometry, and same payload.
4. Do not claim pixel or keyboard-to-screen latency without a real renderer and
   hardware measurement.
5. Every protocol change is append-only within v1 and negotiated by capability.
6. Every correctness-sensitive change must pass:

   ```sh
   cargo test -p stitch-pty --lib
   cargo test -p stitch-mux --lib
   cargo run -p stitch-mux-benchmark --release -- \
     --samples 30 --warmup 5 --burst-bytes 1048576 \
     --out benchmark/results/latest.json
   ```

   and must report zero screen-oracle failures.

## 2. Phase map

| Phase | Priority | Outcome | Main risk | Primary metric |
|---|---:|---|---|---|
| 0. Baseline guardrails | P0 | Honest, reproducible attribution | Stale local dependency | `git sha`, `cargo tree` |
| 1. Compact cell + fixed wire frame | P0 | Remove per-glyph `String`/MessagePack cost | Protocol compatibility | pane diff/wire `ns/sample` |
| 2. Revision coalescing + ack/deadline | P0 | Safe pacing without missed deltas | Oracle regressions | `terminal_output_completion` |
| 3. Query-pause + DSR bootstrap | P1 | Reliable ANSI/attach path | Parser-stream ordering | oracle failures, burst cap |
| 4. Writer threads + bounded output ring | P1 | No writer/reader deadlock or unbounded memory | Backpressure semantics | `server_send_ns`, overflows |
| 5. Style-run render encoding | P1 | Fewer escape bytes, safer wide glyphs | Cursor drift | render bytes/sample |
| 6. Protocol ledger | P2 | Stable wire identity | Over-engineering | conformance tests |
| 7. Windows job-object lifecycle | P2 | No orphaned children, clear errors | Launch compatibility | lifecycle tests |
| 8. Benchmark methodology | P2 | Same-lane, auditable comparisons | Environmental variance | report completeness |

## 3. Detailed phases

---

## Phase 0 — Baseline guardrails

### Goal

Freeze the current state before changing the hot path.

### Work

1. Keep `benchmark/results/latest.json` as the pre-change baseline.
2. Add a second artifact `benchmark/results/baseline-29735ad.json` copied from
   the current result, so later comparisons are explicit.
3. Record the local `stitch-pty` revision in the report:

   ```rust
   #[derive(Serialize)]
   struct DependencyProvenance {
       stitch_pty_source: &'static str,
       stitch_pty_path: Option<String>,
   }

   fn local_pty_provenance() -> DependencyProvenance {
       // Parse `cargo metadata --format-version 1` once at startup.
       // Fail the run if stitch-pty resolves to the registry.
       DependencyProvenance {
           stitch_pty_source: "path",
           stitch_pty_path: Some(
               std::env::var("CARGO_MANIFEST_DIR")
                   .map(|_| "../stitch-pty".to_owned())
                   .unwrap_or_default(),
           ),
       }
   }
   ```

   A simpler initial guard is enough: run `cargo tree` in CI and fail on
   `registry+https://github.com/rust-lang/crates.io-index`.

4. Add phase counters that the upcoming changes will use:

   ```rust
   // src/bench_profile.rs
   pub(crate) static WIRE_ENCODE_NS: AtomicU64 = AtomicU64::new(0);
   pub(crate) static INTERN_NS: AtomicU64 = AtomicU64::new(0);
   pub(crate) static ACK_DEFERRED: AtomicU64 = AtomicU64::new(0);
   pub(crate) static FULL_REPAINTS: AtomicU64 = AtomicU64::new(0);
   ```

   Add corresponding fields to `Snapshot`, `reset()`, and `snapshot()`.

### Acceptance

- Report contains commit, host, profile, local dependency path, p50/p95, and
  raw samples.
- A registry-linked `stitch-pty` causes a hard failure.

---

## Phase 1 — Compact cell representation and fixed wire frame

This is the highest-value change. The profile says pane row diff plus wire-cell
conversion is roughly 2.6× the screen feed cost.

### 1a. Compact in-memory cell text

Current `src/core/cell.rs` uses:

```rust
pub struct Cell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub width: u8,
    pub link: Option<String>,
}
```

Every ASCII glyph therefore owns a heap allocation most of the time, and
`link: Option<String>` adds another. RMUX solves this with inline 4-byte text
and a boxed fallback. Adapt it:

```rust
// src/core/cell_text.rs  (new)
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct CellText {
    inline: [u8; 4],
    inline_len: u8,
    extended: Option<Box<CompactString>>,
}

impl CellText {
    pub fn from_char(ch: char) -> Self {
        let mut inline = [0_u8; 4];
        let inline_len = ch.encode_utf8(&mut inline).len() as u8;
        Self { inline, inline_len, extended: None }
    }

    pub fn from_str(text: &str) -> Self {
        if text.chars().count() == 1 {
            return Self::from_char(text.chars().next().expect("one char"));
        }
        Self {
            inline: [0; 4],
            inline_len: 0,
            extended: Some(Box::new(CompactString::new(text))),
        }
    }

    pub fn as_str(&self) -> &str {
        if let Some(text) = &self.extended {
            return text.as_str();
        }
        std::str::from_utf8(&self.inline[..usize::from(self.inline_len)])
            .expect("inline text is valid utf-8")
    }

    pub fn is_empty(&self) -> bool {
        self.extended.is_none() && self.inline_len == 0
    }
}
```

Add to `stitch-mux/Cargo.toml`:

```toml
compact_str = { version = "0.9", features = ["serde"] }
```

Then change `Cell`:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub text: CellText,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub width: u8,
    pub link: LinkId,
}
```

`LinkId` is an interned numeric ID:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LinkId(pub u32);

impl LinkId {
    pub const NONE: Self = Self(0);
}
```

Preserve ergonomics with accessors:

```rust
impl Cell {
    pub fn text(&self) -> &str { self.text.as_str() }
    pub fn blank() -> Self {
        Self {
            text: CellText::from_char(' '),
            ..Self::default()
        }
    }
}
```

Update all `Cell { text: "...".into(), .. }` construction sites to use
`CellText::from_str`. Add unit tests for:

- ASCII, CJK, combining mark, ZWJ sequence
- serde round-trip
- blank/continuation cells
- hyperlink interning and durability across resize

### 1b. Numeric wire colors

Keep `Color` as the public API but encode it into a fixed `u32` on the wire:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct WireColor(u32);

impl WireColor {
    pub const DEFAULT: Self = Self(0); // sentinel, not black
    pub const fn indexed(index: u8) -> Self { Self(1 | (index as u32) << 8) }
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self(2 | ((r as u32) << 8) | ((g as u32) << 16) | ((b as u32) << 24))
    }
    pub const fn from_color(color: Color) -> Self {
        match color {
            Color::Default => Self::DEFAULT,
            Color::Indexed(i) => Self::indexed(i),
            Color::Rgb(r, g, b) => Self::rgb(r, g, b),
        }
    }
    pub fn to_color(self) -> Color {
        match self.0 & 0xff {
            1 => Color::Indexed((self.0 >> 8) as u8),
            2 => Color::Rgb(
                (self.0 >> 8) as u8,
                (self.0 >> 16) as u8,
                (self.0 >> 24) as u8,
            ),
            _ => Color::Default,
        }
    }
}
```

### 1c. Fixed-layout structured pane frame

Create `src/protocol/pane_frame.rs`. Model it on Zellij's `WireCell` but keep
stitch-mux semantics and v1 framing.

```rust
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaneWireCell {
    pub ch: u32,          // codepoint, or cluster-table index with high bit
    pub fg: u32,          // WireColor
    pub bg: u32,
    pub attrs: u16,
    pub width: u8,
    pub link: u32,        // LinkId
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct PaneRowRecord {
    pub row: u16,
    pub x0: u16,
    pub len: u16,
    pub flags: u16,       // wrapped, continuation reset, etc.
}

#[derive(Clone, Debug, Default)]
pub struct StructuredPaneFrame {
    pub pane_id: u64,
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    pub cursor: Cursor,
    pub modes: Modes,
    pub clusters: Vec<String>,
    pub links: Vec<String>,
    pub records: Vec<PaneRowRecord>,
    pub cells: Vec<u8>,   // flat PaneWireCell bytes
}
```

Builder:

```rust
impl StructuredPaneFrame {
    pub fn push_row(&mut self, row: u16, x0: u16, cells: &[PaneWireCell], wrapped: bool) {
        let room = usize::from(self.cols.saturating_sub(x0));
        let cells = &cells[..cells.len().min(room)];
        if cells.is_empty() {
            return;
        }
        self.records.push(PaneRowRecord {
            row,
            x0,
            len: cells.len() as u16,
            flags: if wrapped { 1 } else { 0 },
        });
        let start = self.cells.len();
        self.cells.resize(start + size_of::<PaneWireCell>() * cells.len(), 0);
        for (slot, cell) in self.cells[start..]
            .chunks_exact_mut(size_of::<PaneWireCell>())
            .zip(cells)
        {
            slot.copy_from_slice(bytemuck::bytes_of(cell));
        }
    }
}
```

Wire it into the existing frame layer as an append-only kind:

```rust
pub enum MessageKind {
    // ...
    StructuredPaneFrame = 28,
    PaneFrameAck = 29,
}
```

Negotiate in `Hello`/`HelloAck`:

```rust
pub const CAP_STRUCTURED_PANE_FRAME_V1: u64 = 1 << 3;

fn client_wants_structured(caps: u64) -> bool {
    caps & CAP_STRUCTURED_PANE_FRAME_V1 != 0
}
```

Server selection:

```rust
if client_caps & CAP_STRUCTURED_PANE_FRAME_V1 != 0 {
    writer.send(MessageKind::StructuredPaneFrame, &frame).await?;
} else {
    writer.send(MessageKind::PaneDelta, &legacy_delta).await?;
}
```

Client decoding should mutate a `PaneView` directly from borrowed cells, without
materializing a `Vec<Cell>` for unchanged rows.

### Acceptance

- Old clients continue to receive `PaneDelta`.
- New clients pass the same screen oracle and resize/capture tests.
- Benchmark reports a new metric:

  ```text
  wire_encode_ns_per_sample
  wire_bytes_per_sample
  cells_interned
  ```

- Pane diff/wire conversion drops materially from the ~205 ms/sample baseline.
- Zero new oracle failures.

### Rollback

Gate the new path behind capability negotiation. If the oracle fails, disable
the capability bit without changing the legacy path.

---

## Phase 2 — Safe coalescing, ack/deadline, and row shifts

### Why this replaces the failed throttle

RMUX's `SnapshotCoalescer` does not simply sleep. It:

1. emits the first revision immediately;
2. suppresses duplicate revisions;
3. keeps only the newest pending revision;
4. emits that pending revision when the rate window opens.

Zellij adds frame sequence acknowledgements and a deadline that forces a full
repaint if the client does not ack. Together these avoid both missed deltas and
unbounded latency.

### 2a. Revision coalescer

Add `src/core/revision_coalescer.rs`:

```rust
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
struct EmittedRevision {
    revision: u64,
    at: Instant,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CoalescedRevision {
    pub emit: bool,
    pub pending: bool,
}

#[derive(Clone, Debug)]
pub struct RevisionCoalescer {
    interval: Duration,
    last_emitted: Option<EmittedRevision>,
    pending: Option<u64>,
}

impl RevisionCoalescer {
    pub fn new(max_per_second: u32) -> Self {
        let nanos = if max_per_second == 0 {
            u64::MAX
        } else {
            1_000_000_000 / u64::from(max_per_second)
        };
        Self {
            interval: Duration::from_nanos(nanos),
            last_emitted: None,
            pending: None,
        }
    }

    pub fn observe(&mut self, revision: u64, now: Instant) -> CoalescedRevision {
        if self
            .last_emitted
            .is_some_and(|last| last.revision == revision)
        {
            return CoalescedRevision {
                emit: false,
                pending: self.pending.is_some(),
            };
        }

        let eligible = self
            .last_emitted
            .map(|last| now.duration_since(last.at) >= self.interval)
            .unwrap_or(true);

        if eligible {
            self.last_emitted = Some(EmittedRevision { revision, at: now });
            self.pending = None;
            CoalescedRevision { emit: true, pending: false }
        } else {
            self.pending = Some(revision); // newest wins
            CoalescedRevision { emit: false, pending: true }
        }
    }

    pub fn poll(&mut self, now: Instant) -> Option<u64> {
        let eligible = self
            .last_emitted
            .map(|last| now.duration_since(last.at) >= self.interval)
            .unwrap_or(true);
        if !eligible {
            return None;
        }
        let revision = self.pending.take()?;
        self.last_emitted = Some(EmittedRevision { revision, at: now });
        Some(revision)
    }
}
```

Use it in `server/client_updates.rs`, not directly in `PaneRuntime::ingest`.
Pane readers should keep producing authoritative updates; only the per-client
emission is paced.

```rust
pub struct UpdateCoalescer {
    ordered: Vec<Arc<PaneUpdate>>,
    deltas: BTreeMap<u64, PaneDelta>,
    last_sent: BTreeMap<u64, u64>,
    emitter: RevisionCoalescer,
    pending_flush: Option<Instant>,
}
```

Change `flush()` so it keeps the newest `PaneDelta` per pane until the gate
opens:

```rust
pub async fn flush<W>(&mut self, writer: &mut FrameWriter<W>) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let now = Instant::now();
    let mut ready: BTreeMap<u64, PaneDelta> = BTreeMap::new();

    for (pane_id, delta) in std::mem::take(&mut self.deltas) {
        let decision = self.emitter.observe(delta.seq, now);
        if decision.emit {
            ready.insert(pane_id, delta);
        } else {
            // Keep the newest pending delta for this pane.
            self.deltas.insert(pane_id, delta);
        }
    }

    for update in self.ordered.drain(..) {
        match update.as_ref() {
            PaneUpdate::Delta(_) => {
                let Some(mut delta) = ready.remove(&update_pane_id(&update)) else {
                    continue;
                };
                let next = self.last_sent.entry(delta.pane_id).or_insert(0);
                *next = next.saturating_add(1);
                delta.seq = *next;
                writer.send(MessageKind::PaneDelta, &delta).await?;
            }
            PaneUpdate::Snapshot(snapshot) => {
                // Snapshots always bypass pacing and reseed the gate.
                self.last_sent.insert(snapshot.pane_id, snapshot.seq);
                writer.send(MessageKind::PaneSnapshot, snapshot).await?;
            }
            PaneUpdate::Exit(exit) => {
                writer.send(MessageKind::Exit, exit).await?;
            }
        }
    }
    Ok(())
}
```

`update_pane_id()` is a small helper that extracts the pane id from a
`PaneUpdate`. Snapshots must always bypass the gate and call
`emitter.observe(snapshot.seq, now)` to reset the revision baseline.

### 2b. Frame acknowledgement and deadline

Add a protocol frame:

```rust
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PaneFrameAck {
    /// Highest contiguous pane delta sequence applied by the client.
    pub seq: u64,
}
```

Client side, after `PaneView::apply_delta` succeeds:

```rust
if view.apply_delta(delta).is_ok() {
    writer
        .send(
            MessageKind::PaneFrameAck,
            &PaneFrameAck { seq: view.pane(pane_id).last_seq },
        )
        .await?;
} else {
    writer.send(MessageKind::Resync, &Resync { reason: "sequence gap".into() }).await?;
}
```

Server side, track in-flight state per client:

```rust
#[derive(Clone, Copy, Debug)]
struct ClientFrameState {
    in_flight: Option<u64>,
    sent_at: Option<Instant>,
}

impl ClientFrameState {
    fn arm(&mut self, seq: u64) {
        self.in_flight = Some(seq);
        self.sent_at = Some(Instant::now());
    }

    fn disarm(&mut self) {
        self.in_flight = None;
        self.sent_at = None;
    }

    fn deadline_expired(&self, deadline: Duration) -> bool {
        match (self.in_flight, self.sent_at) {
            (Some(_), Some(at)) => at.elapsed() >= deadline,
            (Some(_), None) => true,
            _ => false,
        }
    }
}
```

On `PaneFrameAck`, disarm only if the ack matches the in-flight sequence. On
deadline, send a full `PaneSnapshot` and disarm. This is the critical safety net:
a lost or delayed delta can never leave the view permanently stale.

Make the deadline configurable but default conservatively:

```rust
fn ack_deadline() -> Duration {
    let ms = std::env::var("STITCH_MUX_ACK_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(50);
    Duration::from_millis(ms.min(5_000))
}
```

### 2c. Row shifts

Do not repeat the partial-hash bug. When the screen scrolls, either:

- recompute all row hashes (current safe behavior), or
- emit an explicit shift record and update hashes for affected rows.

Preferred structured-frame record:

```rust
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct PaneRowShift {
    pub pane_id: u64,
    pub from_row: u16,
    pub to_row: u16,
    pub count: u16,
    pub flags: u16,
}
```

Client application order must be:

1. apply shifts;
2. apply row records;
3. apply cursor/modes/metadata.

If shifts are not implemented in a phase, keep full-viewport hashing. Never
hash only a subset without updating the hash slots of shifted rows.

### Acceptance

- Re-run the exact benchmark that failed before: no oracle failures.
- `terminal_output_completion` improves or stays neutral.
- `terminal_input_to_visible` does not regress beyond noise.
- Add tests:

  ```text
  coalescer_keeps_newest_pending_revision
  coalescer_first_emit_is_immediate
  ack_mismatch_is_ignored
  deadline_forces_full_snapshot
  scroll_shift_does_not_leave_stale_hash
  ```

---

## Phase 3 — Query-pause and DSR bootstrap

`stitch-pty` already has `set_write_process_input`, so responses can be generated
at query dispatch. What is missing is stream-order preservation and robust
handling of split DSR prefixes.

### 3a. Parser-level query pause

Add an opt-in API that reports how many bytes were consumed before a
forward-bound query:

```rust
// stitch-pty/src/terminal/history.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryKind {
    DeviceStatusReport,
    PrimaryDeviceAttributes,
    SecondaryDeviceAttributes,
    KittyKeyboardFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedOutcome {
    Complete,
    Paused {
        consumed: usize,
        query: QueryKind,
    },
}

impl HistoryScreen {
    pub fn feed_until_query(&mut self, data: &[u8]) -> FeedOutcome {
        // Existing `feed()` becomes a wrapper that drains paused tails.
        // New state machine records the byte offset at query dispatch and
        // returns without consuming the remainder.
    }
}
```

Server-side usage:

```rust
struct PaneState {
    // ...
    pending_pty_input: VecDeque<u8>,
    forward_paused: bool,
    forward_paused_at: Option<Instant>,
}

fn ingest(&self, bytes: &[u8]) {
    let mut state = self.lock_state();
    let pending = state.pending_pty_input.get_mut();
    pending.extend_from_slice(bytes);

    if state.forward_paused {
        return; // drained when the reply lands or the fallback fires
    }

    match state.screen.feed_until_query(pending) {
        FeedOutcome::Complete => pending.clear(),
        FeedOutcome::Paused { consumed, .. } => {
            pending.drain(..consumed);
            state.forward_paused = true;
            state.forward_paused_at = Some(Instant::now());
        }
    }
}
```

A timer or the response path clears `forward_paused` and drains the remaining
bytes. Use a bounded fallback:

```rust
const QUERY_FALLBACK: Duration = Duration::from_millis(500);

fn maybe_resume_query(&mut self) {
    if !self.forward_paused {
        return;
    }
    if self.forward_paused_at.is_some_and(|at| at.elapsed() >= QUERY_FALLBACK) {
        self.forward_paused = false;
        self.forward_paused_at = None;
        // Drain through the normal feed path, answering locally where possible.
    }
}
```

### 3b. Outer-PTY DSR bootstrap

Replace the current per-chunk `respond_to_dsr()` in
`benchmark/src/outer_pty.rs` with a stateful filter modeled on RMUX:

```rust
const DSR_REQUEST: &[u8] = b"\x1b[6n";
const DSR_RESPONSE: &[u8] = b"\x1b[1;1R";

#[derive(Debug)]
struct DsrBootstrap {
    deadline: Instant,
    completed: bool,
    pending: Vec<u8>,
    deferred: Vec<u8>,
}

impl DsrBootstrap {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + Duration::from_millis(2_000),
            completed: false,
            pending: Vec::new(),
            deferred: Vec::new(),
        }
    }

    fn filter(&mut self, bytes: &[u8]) -> (Vec<u8>, usize) {
        if self.completed || Instant::now() > self.deadline {
            self.completed = true;
            let mut out = std::mem::take(&mut self.pending);
            out.extend_from_slice(bytes);
            return (out, 0);
        }

        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(bytes);

        if let Some(offset) = find_subslice(&combined, DSR_REQUEST) {
            self.completed = true;
            let mut out = Vec::with_capacity(combined.len() - DSR_REQUEST.len());
            out.extend_from_slice(&combined[..offset]);
            out.extend_from_slice(&combined[offset + DSR_REQUEST.len()..]);
            return (out, 1); // one response
        }

        let prefix = partial_prefix_len(&combined, DSR_REQUEST);
        let keep = combined.len() - prefix;
        self.pending.extend_from_slice(&combined[keep..]);
        (combined[..keep].to_vec(), 0)
    }
}
```

In `OuterSession`:

```rust
Ok(bytes) => {
    let (visible, responses) = self.dsr.filter(&bytes);
    for _ in 0..responses {
        let _ = self.writer.write_all(DSR_RESPONSE);
        let _ = self.writer.flush();
    }
    self.raw.extend_from_slice(&visible);
    self.oracle.feed(&visible);
}
```

### Acceptance

- Byte-at-a-time DSR split tests pass.
- Large ANSI bursts no longer need the 256-byte cap.
- Zero screen-oracle failures on the outer lane with a real tmux reference.
- If a reference mux is unavailable, report it as omitted, not failed or estimated.

---

## Phase 4 — Dedicated writer threads and bounded output ring

### 4a. PTY input writer

Zellij separates stdout reads from stdin writes because some programs deadlock
when both happen in the same task. Add a dedicated writer:

```rust
enum PtyWriteCommand {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Exit,
}

struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
}
```

```rust
const MAX_PENDING_BYTES: usize = 10 * 1024 * 1024;
const PENDING_DRAIN_TIMEOUT: Duration = Duration::from_millis(10);

fn writer_loop(rx: Receiver<PtyWriteCommand>, pty: PtyWriter) {
    let mut pending: VecDeque<PendingWrite> = VecDeque::new();

    loop {
        let command = if pending.is_empty() {
            match rx.recv() {
                Ok(command) => command,
                Err(_) => break,
            }
        } else {
            match rx.recv_timeout(PENDING_DRAIN_TIMEOUT) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => PtyWriteCommand::Resize {
                    cols: 0,
                    rows: 0, // signal: drain only
                },
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };

        match command {
            PtyWriteCommand::Write(bytes) => {
                let queued: usize = pending.iter().map(|w| w.bytes.len() - w.offset).sum();
                if queued + bytes.len() > MAX_PENDING_BYTES {
                    pending.clear();
                    dropped_writes.fetch_add(1, Ordering::Relaxed);
                } else {
                    pending.push_back(PendingWrite { bytes, offset: 0 });
                }
            }
            PtyWriteCommand::Resize { cols, rows } => {
                pty.resize(cols, rows);
            }
            PtyWriteCommand::Exit => break,
        }

        // Nonblocking partial drain; keep leftovers for the next wake.
        while let Some(front) = pending.front_mut() {
            match pty.try_write(&front.bytes[front.offset..]) {
                Ok(0) => break,
                Ok(n) => front.offset += n,
                Err(WouldBlock) => break,
                Err(_) => break,
            }
            if pending.front().is_some_and(|w| w.offset == w.bytes.len()) {
                pending.pop_front();
            }
        }
    }
}
```

Add counters:

```text
pty_write_queue_depth
pty_write_dropped_bytes
pty_write_partial_drains
```

### 4b. Output ring

RMUX's bounded ring stores `Arc<[u8]>` events with sequence numbers. Adapt it
for raw pane output and side effects:

```rust
use std::sync::Arc;

pub const OUTPUT_RING_CAPACITY: usize = 2048;

#[derive(Clone, Debug)]
pub struct OutputEvent {
    sequence: u64,
    bytes: Arc<[u8]>,
}

impl OutputEvent {
    pub fn from_vec(sequence: u64, bytes: Vec<u8>) -> Self {
        Self { sequence, bytes: bytes.into() }
    }

    pub fn bytes(&self) -> &[u8] { &self.bytes }
}

#[derive(Debug)]
pub struct OutputRing {
    events: VecDeque<OutputEvent>,
    next_sequence: u64,
}

impl OutputRing {
    pub fn push(&mut self, bytes: Vec<u8>) -> u64 {
        let seq = self.next_sequence;
        self.next_sequence += 1;
        self.events.push_back(OutputEvent::from_vec(seq, bytes));
        if self.events.len() > OUTPUT_RING_CAPACITY {
            self.events.pop_front();
        }
        seq
    }

    pub fn since(&self, cursor: u64) -> impl Iterator<Item = &OutputEvent> {
        self.events
            .iter()
            .filter(move |event| event.sequence >= cursor)
    }
}
```

Clients keep a cursor; gaps are explicit instead of silently replaying stale
data.

### 4c. Attach output worker

Use RMUX's bounded output queue pattern for terminal writes:

```rust
enum OutputCommand {
    Frame(Vec<u8>),
    Fence(AttachFence),
}

const OUTPUT_QUEUE_CAPACITY: usize = 64;

fn output_worker(output: impl Write, rx: Receiver<OutputCommand>) {
    let mut output = BufWriter::with_capacity(64 * 1024, output);
    for command in rx {
        match command {
            OutputCommand::Frame(bytes) => {
                if output.write_all(&bytes).and_then(|_| output.flush()).is_err() {
                    break;
                }
            }
            OutputCommand::Fence(fence) => {
                let _ = output.flush();
                fence.complete();
            }
        }
    }
}
```

### Acceptance

- No deadlock in the stress test that writes to stdin while reading stdout.
- Queue overflow is counted and bounded.
- Server write phase does not grow with client count beyond the configured cap.

---

## Phase 5 — Style-run render encoding

This phase reduces the ANSI bytes in `Render` frames and makes wide glyphs safer.

### Work

1. Add `src/render/row_stream.rs`, modeled on wtmux.
2. Keep a reusable `String` buffer and a reusable style template.
3. Reanchor at run boundaries using absolute grid columns:

   ```rust
   fn flush_run<W: Write, S, F>(
       out: &mut W,
       run: &mut String,
       style: &S,
       origin: Option<(u16, u16)>,
       start_col: usize,
       apply_style: &mut F,
   ) -> std::io::Result<()>
   where
       F: FnMut(&mut W, &S) -> std::io::Result<()>,
   {
       if run.is_empty() {
           return Ok(());
       }
       if let Some((x, y)) = origin {
           write!(out, "\x1b[{};{}H", y as u32 + 1, x as u32 + start_col as u32 + 1)?;
       }
       apply_style(out, style)?;
       out.write_all(run.as_bytes())?;
       run.clear();
       Ok(())
   }
   ```

4. Reanchor around risky glyphs:

   ```rust
   fn is_risky(cell: &Cell) -> bool {
       cell.width != 1 || cell.text().chars().count() != 1
   }
   ```

5. Wrap full frames in synchronized output:

   ```rust
   write!(out, "\x1b[?2026h")?;
   // render rows
   write!(out, "\x1b[?2026l")?;
   out.flush()?;
   ```

6. Clear tail cells per row with `EL 0` instead of emitting spaces.

### Acceptance

- Existing render snapshots unchanged for ASCII-only and styled runs.
- Wide/CJK/emoji tests do not bleed into neighboring cells.
- New benchmark metrics:

  ```text
  render_bytes_per_sample
  render_runs_per_row
  render_reanchors_per_row
  ```

---

## Phase 6 — Protocol ledger and capability bits

RMUX pins stable `u16` wire identities and reserves removed values. Our
`MessageKind::ALL` already helps, but it lacks explicit reserved-status and
direction metadata.

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameStatus {
    Active,
    Reserved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameDirection {
    ClientToServer,
    ServerToClient,
    Bidirectional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameKindSpec {
    pub kind: MessageKind,
    pub status: FrameStatus,
    pub direction: FrameDirection,
    pub feature: &'static str,
}

pub const WIRE_LEDGER_V1: &[FrameKindSpec] = &[
    FrameKindSpec {
        kind: MessageKind::Hello,
        status: FrameStatus::Active,
        direction: FrameDirection::ClientToServer,
        feature: "handshake",
    },
    // ...
];
```

Add a conformance test that:

- every `MessageKind` has exactly one ledger entry;
- no two active entries share a tag;
- reserved tags are never reused;
- direction matches the endpoint that emits the frame.

Capability bits:

```rust
pub const CAP_STRUCTURED_PANE_FRAME_V1: u64 = 1 << 3;
pub const CAP_PANE_FRAME_ACK_V1: u64 = 1 << 4;
pub const CAP_ROW_SHIFT_V1: u64 = 1 << 5;
pub const CAP_SYNCHRONIZED_RENDER_V1: u64 = 1 << 6;
```

`HelloAck` should return only the intersection of client and server bits.

---

## Phase 7 — Windows daemon and job-object lifecycle

Adopt RMUX's policy rather than retrying unsafely.

```rust
#[cfg(windows)]
fn hidden_daemon_creation_flags(allow_breakaway: bool) -> u32 {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let base = DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;
    if allow_breakaway {
        base | CREATE_BREAKAWAY_FROM_JOB
    } else {
        base
    }
}
```

Rules:

- child shells are placed in a job with `KILL_ON_JOB_CLOSE`;
- explicit breakaway is allowed only for the daemon/helper;
- a failed breakaway produces an actionable error and does not silently retry
  inside the caller's job;
- Ctrl+C delivery must keep the host and hosted console processes in the same
  process group, as RMUX documents.

Error text should explain both options:

```text
Windows refused to launch an independent daemon: <os error>.
Allow child-process breakaway in the enclosing job, or start the mux
outside that job. Retrying inside the job would kill the daemon with
the launcher.
```

This does not require changing the benchmark default lane. It only matters for
future native Windows reference lanes.

---

## Phase 8 — Benchmark methodology and reference lanes

### Adopt RMUX's reporting discipline

Every report should contain:

```json
{
  "schema": 1,
  "commit": "29735ad",
  "host": { "os": "windows", "arch": "x86_64", "cpus": 32 },
  "tool_versions": { "stitch-mux": "...", "stitch-pty": "...", "tmux": null },
  "profile": "release",
  "samples": { "n": 30, "warmup": 5 },
  "metrics": {},
  "reference_lanes": [],
  "omitted_lanes": [
    { "tool": "tmux", "reason": "not found on PATH" }
  ]
}
```

Rules:

- p50 and p95 plus raw samples.
- Missing tools are omitted, never estimated.
- Report the noise band; do not call 0.80×–1.25× a win.
- Clear inherited selectors before spawning servers:

  ```sh
  unset STITCH_MUX_SOCKET TMUX RMUX TERM_PROGRAM
  ```

- Use one neutral config, private socket, and fixed geometry for every CLI mux.
- Use the same outer PTY implementation for both sides.

### Reference lane priority

1. `tmux` — primary CLI baseline.
2. `rmux` — native Windows daemon mux; useful but job-object sensitive.
3. `zellij` — secondary; TUI/plugin cost must be labelled.
4. `wtmux` — not a same-lane attach reference until detach/attach exists.

### Keep benchmark lanes separate

Do not merge these into one number:

- parser-only
- screen-state floor
- structured mux path
- ANSI outer PTY
- render-side output

### Image parser lane caveat

Native `stitch-pty` skips APC string bytes while `vte` delivers them. Keep the
lane labelled as not apples-to-apples.

## 4. Validation matrix

After each phase, run:

```sh
cargo tree -p stitch-mux | grep stitch-pty
cargo test -p stitch-pty --lib
cargo test -p stitch-mux --lib
cargo run -p stitch-mux-benchmark --release -- \
  --samples 30 --warmup 5 --burst-bytes 1048576 \
  --out benchmark/results/latest.json
```

Additional gates:

| Phase | Required gate |
|---|---|
| 1 | Legacy `PaneDelta` conformance + oracle = 0 failures |
| 2 | Oracle = 0 failures; acks and deadline tests pass |
| 3 | Split-DSR tests; outer-lane oracle = 0 failures |
| 4 | Input-writer deadlock stress test; bounded queue test |
| 5 | Wide glyph/emoji render snapshots |
| 6 | Wire ledger conformance |
| 7 | Windows job lifecycle tests |
| 8 | Report includes provenance and omitted lanes |

Performance acceptance is relative to the frozen baseline, not to published
numbers from other projects:

- pane diff/wire conversion must decrease;
- `terminal_output_completion` must not regress;
- `terminal_input_to_visible` must stay near ~15 ms unless a deliberate,
  explained change occurs;
- oracle failures must remain zero.

## 5. Risks and rollback

| Risk | Mitigation |
|---|---|
| Protocol change breaks old clients | capability negotiation; legacy `PaneDelta` remains |
| Coalescing hides final delta | newest-revision-only + deadline full snapshot |
| Scroll diff goes stale | explicit row shifts or full-row hash recompute |
| Parser pause deadlocks | 500 ms fallback + tests for split sequences |
| Writer queue drops input under burst | bounded drop counter + stress test |
| Wide glyph drift | reanchor around risky cells + snapshot tests |
| Stale local dependency | hard `cargo tree` check |
| Invalid cross-tool claim | same-lane requirement and omitted-lane metadata |

Each phase should be one commit or a small stack of commits with a feature flag
or capability bit, so a regression can be disabled without reverting unrelated
work.

## 6. References

- RMUX repository: https://github.com/Helvesec/rmux
- RMUX benchmark methodology: https://github.com/Helvesec/rmux/blob/main/docs/benchmarks.md
- RMUX grid cell: https://github.com/Helvesec/rmux/blob/main/crates/rmux-core/src/grid/cell.rs
- RMUX revision coalescing: https://github.com/Helvesec/rmux/blob/main/crates/rmux-core/src/events/coalescing.rs
- RMUX output ring: https://github.com/Helvesec/rmux/blob/main/crates/rmux-core/src/events/ring.rs
- RMUX wire ledger: https://github.com/Helvesec/rmux/blob/main/crates/rmux-proto/src/frame_kind.rs
- RMUX Windows daemon handling: https://github.com/Helvesec/rmux/blob/main/crates/rmux-os/src/daemon.rs
- Zellij repository: https://github.com/zellij-org/zellij
- Zellij structured render: https://github.com/zellij-org/zellij/blob/main/zellij-utils/src/structured_render.rs
- Zellij pane mirror: https://github.com/zellij-org/zellij/blob/main/zellij-utils/src/pane_mirror.rs
- Zellij ack pacing: https://github.com/zellij-org/zellij/blob/main/zellij-server/src/screen.rs
- Zellij paused parser: https://github.com/zellij-org/zellij/blob/main/zellij-server/src/panes/terminal_pane.rs
- Zellij PTY writer: https://github.com/zellij-org/zellij/blob/main/zellij-server/src/pty_writer.rs
- wtmux repository: https://github.com/fukuyori/wtmux
- wtmux row stream: https://github.com/fukuyori/wtmux/blob/main/src/ui/row_stream.rs
- wtmux renderer: https://github.com/fukuyori/wtmux/blob/main/src/ui/renderer.rs
