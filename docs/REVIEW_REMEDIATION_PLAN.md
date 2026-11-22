# stitch-mux review remediation plan

Date: 2026-11-22  
Repository: `D:\User\Documents\Python\stitch-mux`  
Reviewed commit: `5a402ad perf(bench): optimize structured pane output path`  
Branch: `dev_imporove`  
Status: proposed; no code changes made by this document.

This plan addresses the findings from a guideline-based review of `stitch-mux`.
It uses the Microsoft Pragmatic Rust Guidelines loaded through the
`rust-guidelines` skill. Guideline IDs are cited inline, for example
`M-STATIC-VERIFICATION`.

No wire-format break is intended. Protocol changes are assertions or additive
behavior only.

---

## Table of contents

1. [Phase 0 — Restore CI first](#phase-0--restore-ci-first)
2. [Phase 1 — Correctness fixes](#phase-1--correctness-fixes)
3. [Phase 2 — Hot-path allocation and timing cleanup](#phase-2--hot-path-allocation-and-timing-cleanup)
4. [Phase 3 — API and lint hygiene](#phase-3--api-and-lint-hygiene)
5. [Phase 4 — Validation and benchmark refresh](#phase-4--validation-and-benchmark-refresh)
6. [Commit sequence](#commit-sequence)
7. [Rollback rules](#rollback-rules)

---

## Phase 0 — Restore CI first

**Guideline:** `M-STATIC-VERIFICATION`

Goal: make the pushed branch pass the same clippy commands CI runs.

### 0.1 `src/core/cell_text.rs`

- Rename inherent `CellText::from_str` to `CellText::new`.
- Implement `std::str::FromStr` for `CellText` with `Err = Infallible`,
  forwarding to `new`.
- Update internal callers:
  - `From<&str>`
  - `From<String>`
  - `From<&String>`
  - `Deserialize`
- Replace the manual `Default` impl with `#[derive(Default)]`.

Why: removes `clippy::should_implement_trait` and
`clippy::derivable_impls`, and gives callers the conventional constructor.

### 0.2 `src/protocol/pane_frame.rs`

Replace constant-size `chunks_exact*` calls with `as_chunks*`:

- encode records: `as_chunks_mut::<ROW_RECORD_LEN>().0.iter_mut()`
- encode cells: `as_chunks_mut::<CELL_LEN>().0.iter_mut()`
- decode records: `as_chunks::<ROW_RECORD_LEN>().0`
- decode cells: `as_chunks::<CELL_LEN>().0`

Why: four clippy errors, no behavior change.

### 0.3 `src/server/pane.rs`

At the test around line 1047, replace `vec![row(...), ...]` with an array
literal.

### 0.4 `benchmark/src/outer_pty.rs`

Delete unused debug helpers:

- `LineOracle::debug_text`
- `LineOracle::debug_screen`
- `OuterSession::debug_text`
- `OuterSession::debug_compact`
- `OuterSession::debug_raw`

If they must be retained for future debugging, gate them behind an explicit
`debug-util` feature instead of leaving dead code.

### 0.5 CI hardening

Update `.github/workflows/ci.yml` to also run:

```sh
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Why: the `bench-profile` feature currently escapes default clippy/test runs.

### Exit criteria

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

all pass.

---

## Phase 1 — Correctness fixes

### 1.1 Coalescer must never drop newest metadata

**Files:** `src/protocol/dto.rs`, `src/server/client_updates.rs`  
**Guidelines:** `M-STRONG-TYPES-GUARD`, `M-REGULAR-FN`

Current merge in `UpdateCoalescer::add` copies most fields but misses:

- `total_lines`
- `notification`
- `progress`

Do not just add three lines. Centralize the invariant.

Add to `PaneDelta`:

```rust
impl PaneDelta {
    pub fn overlay_state(&mut self, newer: Self) {
        debug_assert_eq!(self.pane_id, newer.pane_id);
        let Self {
            pane_id: _,
            seq,
            rows: _,
            cursor,
            modes,
            title,
            cwd,
            foreground,
            shell_marks,
            program_status,
            notification,
            progress,
            alt_screen,
            scrollback_len,
            total_lines,
            scrollback_bytes,
            bell,
        } = newer;

        self.seq = seq;
        self.cursor = cursor;
        self.modes = modes;
        self.title = title;
        self.cwd = cwd;
        self.foreground = foreground;
        self.shell_marks = shell_marks;
        self.program_status = program_status;
        self.notification = notification;
        self.progress = progress;
        self.alt_screen = alt_screen;
        self.scrollback_len = scrollback_len;
        self.total_lines = total_lines;
        self.scrollback_bytes = scrollback_bytes;
        self.bell |= bell;
    }
}
```

Then `UpdateCoalescer::add`:

```rust
for row in &delta.rows {
    pending.rows.retain(|existing| existing.row != row.row);
    pending.rows.push(row.clone());
}
pending.overlay_state((**delta).clone());
```

Exhaustive destructuring makes any future `PaneDelta` field a compile error
until explicitly handled.

#### Test

In `src/server/client_updates.rs`, add
`coalesces_total_lines_notification_and_progress`:

- Add two deltas for the same pane.
- Give the second distinct `total_lines`, `notification`, and `progress`.
- Flush.
- Decode the emitted `PaneDelta`.
- Assert all three match the second delta.

### 1.2 Ack recovery needs a real deadline timer

**Files:** `src/server/client_updates.rs`, `src/server/mod.rs`  
**Guidelines:** `M-MOCKABLE-SYSCALLS`, `M-TEST-UTIL`

Current problem: `deadline_panes()` is only called in `Event::Update`. A lost
final ack with no subsequent update never triggers a repaint.

Add to `UpdateCoalescer`:

```rust
pub fn next_deadline(&self) -> Option<Instant>
```

It should return the earliest `sent_at + self.ack_deadline` over `in_flight`.

Store the deadline in the coalescer:

```rust
pub struct UpdateCoalescer {
    ...
    ack_deadline: Duration,
}
```

Move the `STITCH_MUX_ACK_DEADLINE_MS` environment read into
`with_capabilities`, so it is parsed once. For unit tests, add a private
constructor:

```rust
#[cfg(test)]
fn with_timing(client_caps: u64, max_delta_hz: u32, ack_deadline: Duration) -> Self
```

In the connection loop in `src/server/mod.rs`:

```rust
let next_ack_deadline = coalescer.next_deadline();
let deadline = async move {
    match next_ack_deadline {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending::<()>().await,
    }
};

let event = tokio::select! {
    _ = deadline => Event::AckDeadline,
    ...
};
```

Add `Event::AckDeadline`.

Refactor the existing overdue-repaint block into a helper, e.g.:

```rust
async fn repaint_overdue_panes(...) -> Result<(), ServerError>
```

Call it from both `Event::Update` and `Event::AckDeadline`.

#### Test

Add an integration or unit test that:

1. Connects a structured-capable client.
2. Produces a delta.
3. Drops/does not send `PaneFrameAck`.
4. Sends no further updates.
5. Waits for a `PaneSnapshot` triggered by the deadline timer.

Use a short injected deadline, not a 50 ms environment variable changed
globally.

### 1.3 Pin wire kind numbers in the ledger

**File:** `src/protocol/frame.rs`  
**Guideline:** `M-DOCUMENTED-MAGIC`

Current ledger test checks coverage but not numeric stability. Add explicit
numeric assertions:

```rust
const EXPECTED_KIND_CODES: [(MessageKind, u8); 29] = [
    (MessageKind::Hello, 1),
    // ...
    (MessageKind::PaneFrameAck, 29),
];

for (kind, code) in EXPECTED_KIND_CODES {
    assert_eq!(kind.as_u8(), code);
    assert_eq!(MessageKind::try_from(code).unwrap(), kind);
}
```

Optional stronger version: add `code: u8` to `FrameKindSpec` and assert it
matches `kind.as_u8()`.

### Exit criteria

- Coalesced deltas always contain newest metadata.
- A lost ack is repaired without requiring another PTY update.
- Renumbering `MessageKind` fails tests.

---

## Phase 2 — Hot-path allocation and timing cleanup

### 2.1 Remove the per-cell `String` clone

**File:** `src/server/pane.rs:983`  
**Guidelines:** `M-HOTPATH`, `M-MEM-REUSE`

Replace:

```rust
text: cell.data.clone().into(),
```

with:

```rust
text: CellText::from_str(&cell.data),
```

or `CellText::new(&cell.data)` after Phase 0.

This avoids one heap allocation for every single-scalar cell, which is the
common case.

### 2.2 Make row hashing in-place

**Files:** `src/server/pane.rs:331`, `src/server/pane.rs:772`  
**Guidelines:** `M-MEM-REUSE`, `M-INITIAL-CAPACITY`

Current flow:

```rust
let rows = changed_rows(&state.last_row_hashes, state.screen.buffer());
...
last_row_hashes.clear();
last_row_hashes.extend(...);
```

`changed_rows` internally clones `previous.to_vec()` and recomputes every hash.

Replace with:

```rust
fn changed_rows_in_place(
    previous: &mut [u64],
    visible: &[Vec<PtyCell>],
) -> Vec<RowUpdate> {
    changed_rows_at(previous, visible, 0..visible.len())
}
```

Then in `Pane::feed`:

```rust
let PaneState {
    last_row_hashes,
    screen,
    ..
} = &mut *state;

let rows = changed_rows_in_place(last_row_hashes, screen.buffer());
```

Remove the subsequent clear/extend block.

Important: this is full in-place recomputation, **not** the previously
reverted scroll-aware partial hashing. Keep that failed experiment reverted.

#### Test

Add a parity test:

- Build several visible screens.
- Run old clone-based `changed_rows` and new `changed_rows_in_place` on copies.
- Assert equal row updates and final hash arrays.
- Include:
  - no change
  - one row change
  - resize shorter/longer
  - wide-character row
  - style-only change

### 2.3 Cache environment configuration

**File:** `src/server/client_updates.rs`  
**Guidelines:** `M-HOTPATH`, `M-MOCKABLE-SYSCALLS`

`ack_deadline()` currently reads the environment every `deadline_panes()` call.

- Store `ack_deadline: Duration` on `UpdateCoalescer`.
- Parse `STITCH_MUX_ACK_DEADLINE_MS` once in `with_capabilities`.
- Use `self.ack_deadline` in `deadline_panes` and `next_deadline`.
- Keep `configured_max_delta_hz()` as-is; it is already called once per
  construction.

### 2.4 Optional: reusable structured-frame encoder buffers

**Files:** `src/protocol/pane_frame.rs`, `src/server/client_updates.rs`  
**Guideline:** `M-MEM-REUSE`

Profile shows wire encode is only ~0.036 ms/frame, so this is lower priority.

If implemented:

- Add `PaneFrameEncoder` with reusable buffers for:
  - meta bytes
  - record bytes
  - cell bytes
  - cluster bytes
  - link bytes
  - final payload
- Store one encoder per client connection.
- Add `StructuredPaneFrame::encode_into(&mut PaneFrameEncoder)`.
- Keep `encode(self)` as the convenient allocating API.

Do not do this before 2.1 and 2.2; those target the dominant cost.

### Exit criteria

- Benchmark still reports `screen_oracle.failures = 0`.
- `terminal_input_to_visible` does not regress beyond noise.
- Allocation counters, if enabled, show fewer allocations in `cells_from_row`
  and pane diff.

---

## Phase 3 — API and lint hygiene

### 3.1 Explicit protocol re-exports

**File:** `src/protocol/mod.rs`  
**Guideline:** `M-NO-GLOB-REEXPORTS`

Replace:

```rust
pub use dto::*;
```

with an explicit list generated from public items in `dto.rs`.

Example process:

```sh
rg '^pub (struct|enum|type)' src/protocol/dto.rs
```

Then list those names explicitly.

### 3.2 Public types implement `Debug`

**Guideline:** `M-PUBLIC-DEBUG`

Add `Debug` to public types in public modules:

- `ClientUpdateQueue`
- `UpdateCoalescer`

For generic wrappers, avoid requiring `W: Debug` or `R: Debug`:

```rust
impl<W> fmt::Debug for FrameWriter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameWriter").finish_non_exhaustive()
    }
}
```

For `FrameReader<R>`, report only `pending.len()` and `decoder.buffered()`.

### 3.3 `CellText` constructor fix

Already covered in Phase 0, but keep the API result explicit:

```rust
let text = CellText::new("e\u{301}");
let parsed: CellText = "a".parse().unwrap();
```

### 3.4 Lint overrides

**Guideline:** `M-LINT-OVERRIDE-EXPECT`

Replace:

```rust
#[allow(clippy::too_many_arguments)]
```

with:

```rust
#[expect(
    clippy::too_many_arguments,
    reason = "connection handler mirrors the fixed server event contract"
)]
```

Apply at:

- `src/server/mod.rs:945`
- `src/server/session.rs:1237`

### 3.5 Unsafe rationale

**Guideline:** `M-UNSAFE`

At `src/protocol/endpoint.rs:81,116,125`, add comments:

```rust
// SAFETY: getuid/geteuid are side-effect-free libc calls with no preconditions.
```

This is documentation only.

### 3.6 Error model

**Guideline:** `M-ERRORS-CANONICAL-STRUCTS`

Longer-term, convert public error enums such as `FrameError`, `ServerError`,
`EndpointError`, and `SessionError` into canonical structs with:

- private `ErrorKind`
- optional source
- `Backtrace`
- `is_*()` helpers
- `Display` summary
- `std::error::Error`

This is API-breaking and not required for the benchmark work. Defer to a
separate release unless caller ergonomics demands it now.

### Exit criteria

- No glob re-export in `src/protocol/mod.rs`.
- All public types in touched modules implement `Debug`.
- No stale `#[allow]` overrides.
- All unsafe calls have safety comments.

---

## Phase 4 — Validation and benchmark refresh

Run after each phase, and again at the end:

```sh
cd D:/User/Documents/Python/stitch-mux

bash scripts/check-local-pty.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --release -p stitch-mux --test stress -- --ignored
```

Expected local dependency:

```text
stitch-pty v0.17.0 (D:\User\Documents\Python\stitch-pty)
```

Final benchmark:

```sh
cargo run -p stitch-mux-benchmark --release -- \
  --samples 30 --warmup 5 --burst-bytes 1048576 \
  --out benchmark/results/latest.json
```

Compare against:

```text
benchmark/results/baseline-29735ad.json
```

Acceptance:

- `screen_oracle.failures = 0`
- `cargo clippy ... -D warnings` clean
- no `terminal_input_to_visible` regression beyond noise
- `terminal_output_completion` <= current baseline within measurement noise,
  ideally improved
- `full_repaints` may be non-zero only in ack/timeout tests
- `deltas_deferred` must never leave stale final state
- no new crates.io `stitch-pty` in `cargo tree`

---

## Commit sequence

Use separate commits so each change is bisectable:

1. `fix(ci): satisfy clippy on all targets and features`
2. `fix(core): preserve newest delta metadata when coalescing`
3. `fix(server): add ack deadline repaint timer`
4. `test(protocol): pin message kind wire numbers`
5. `perf(pane): remove per-cell clone and hash row state in place`
6. `refactor(config): cache update pacing configuration`
7. `refactor(api): tidy protocol re-exports and Debug implementations`

Optional last commit if benchmark numbers change materially:

8. `test(bench): refresh headless benchmark results`

---

## Rollback rules

- If screen oracle fails after Phase 2, revert Phase 2 first; keep Phase 1
  correctness fixes.
- If clippy passes but latency regresses >5%, revert the performance phase
  independently.
- If protocol ledger tests fail, do not renumber `MessageKind`; reserve the old
  value and add a new one.
- Never reintroduce:
  - 2 ms publish throttling
  - scroll-aware partial hashing
  - naive frame coalescing
  - 2 MiB publish batching

Those previously broke the screen oracle or latency.

---

## Implementation status (2026-11-22)

Implemented on `dev_imporove`:

- Phase 0: clippy/rustfmt blockers, benchmark dead helpers, CI all-features
  clippy and tests. Applied `M-STATIC-VERIFICATION`.
- Phase 1:
  - `PaneDelta::overlay_state` centralizes the newest non-row delta state,
    including `total_lines`, `notification`, and `progress`
    (`M-STRONG-TYPES-GUARD`).
  - `UpdateCoalescer::next_deadline` plus a `tokio::select!` timer branch
    repaints overdue panes even when no PTY update arrives. The repaint path
    is shared by update-driven and timer-driven recovery.
  - `MessageKind` wire numbers are pinned by tests (`M-DOCUMENTED-MAGIC`).
- Phase 2:
  - `CellText::new(&cell.data)` removes the per-cell `String` clone
    (`M-MEM-REUSE`).
  - `changed_rows_in_place` recomputes all visible row hashes without cloning
    the previous vector; scroll-aware partial hashing remains reverted.
  - `STITCH_MUX_ACK_DEADLINE_MS` and `STITCH_MUX_MAX_DELTA_HZ` are parsed once
    per coalescer and injected into tests (`M-HOTPATH`, `M-MOCKABLE-SYSCALLS`).
- Phase 3:
  - `src/protocol/mod.rs` uses explicit `#[doc(inline)]` re-exports
    (`M-NO-GLOB-REEXPORTS`, `M-DOC-INLINE`).
  - `ClientUpdateQueue`, `UpdateCoalescer`, `FrameReader`, and `FrameWriter`
    implement `Debug` (`M-PUBLIC-DEBUG`).
  - `CellText` now has `new` plus `FromStr` (`C-CTOR`, clippy trait naming).
  - Lint overrides use `#[expect(..., reason = ...)]`
    (`M-LINT-OVERRIDE-EXPECT`).
  - Unix `libc` calls have `SAFETY` comments (`M-UNSAFE`).

Deferred as designed:

- Phase 2.4 reusable structured-frame encoder buffers: wire encode is only
  ~0.037 ms/frame, so the extra complexity is not justified yet.
- Phase 3.6 canonical error structs/backtraces: API-breaking and out of scope
  for this remediation pass.

Validation:

```text
cargo fmt --all -- --check                                      ok
cargo clippy --workspace --all-targets -- -D warnings           ok
cargo clippy --workspace --all-targets --all-features -- -D warnings  ok
cargo test --all-targets --all-features                         ok (116 lib tests)
cargo test --release -p stitch-mux --test stress -- --ignored   ok
cargo test --release -p stitch-mux --test soak -- --ignored     ok
bash scripts/check-local-pty.sh                                 ok (local stitch-pty)
```

Final headless benchmark vs `baseline-29735ad.json`:

```text
screen oracle failures:        0 (baseline 0)
terminal_output_completion p50: 295.48 ms (baseline 296.52 ms)
terminal_output_ingest_rate p50: 3.387 MiB/s (baseline 3.379 MiB/s)
terminal_input_to_visible p50:  15.47 ms (baseline 15.35 ms)
full_repaints:                  0
deltas_deferred:                0
wire_encode_ms_per_frame:       0.0367 ms (instrumented)
```
