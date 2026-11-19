# stitch-mux Implementation Protocol

This is the **operating protocol** for turning the design in
[ARCHITECTURE.md](ARCHITECTURE.md) and the milestones in
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) into code. It is a living
document: update it in the **same commit** as the work it describes. It is not
the wire protocol; that contract lives in [PROTOCOL.md](PROTOCOL.md).

Last updated: **2026-11-19** — M0–M4 complete, M5 ready.

## 1. Standing rules

1. The design docs are the source of truth. If code needs a different shape,
   update the relevant doc and this protocol with the rationale in the same
   commit; never leave the docs behind.
2. `main` is always green and releasable. Integration happens on `dev`; `main`
   is updated by fast-forward only.
3. M0–M4 are toolkit-neutral at the product boundary. No egui, GPUI, Qt,
   tokio-free core violation, or terminal dependency enters
   `stitch-mux-core`, `stitch-mux-protocol`, `stitch-mux-server`, or
   `stitch-mux-client`.
4. The daemon owns PTYs, screens, layout, focus history, and scrollback.
   Clients are views. There is no second terminal emulator in a client.
5. Keyboard encoding goes through `stitch-pty`'s `encode_key` with the focused
   pane's active Kitty flags. Frontends send structured `InputEvent`s, never
   hand-rolled escape sequences.
6. The transport is local-only: Unix socket or Windows named pipe, owner-scoped.
   No TCP listener, no SSH, no remote or multi-user path.
7. Bounded queues, bounded frames, and coalescing are implemented with the
   feature, not retrofitted. A slow client must never block a PTY reader.
8. Detach/reattach is an M1 acceptance gate. It is never deferred behind GUI
   polish or terminal compatibility.
9. Work lands in small, reviewable commits. A commit that breaks a gate is
   fixed before the next commit is started.
10. If a rule in this document is wrong, change the rule explicitly in a docs
    commit. Do not silently work around it.

## 2. Branch and commit conventions

- `dev` is the integration branch. Feature work uses short-lived branches from
  `dev` and is rebase-merged into `dev`.
- `main` is advanced from `dev` with `git merge --ff-only` after green CI.
- No force-push to shared branches. Rewriting is allowed only on a private
  feature branch before it is merged.
- Commit subjects follow `type(scope): summary`:
  - `feat(core)` / `feat(protocol)` / `feat(server)` / `feat(client)` /
    `feat(gui-egui)` / `feat(cli)` for new capability
  - `fix(...)`, `docs:`, `test:`, `refactor:`, `chore:` as appropriate
- One logical change per commit. Mixing a protocol change, a server change, and
  unrelated docs cleanup makes bisecting impossible.
- Version bumps are their own commits. Tags and publication require explicit
  user approval; nothing is tagged or published by default.

## 3. Work item lifecycle

| State | Entry condition | Exit condition |
|---|---|---|
| Backlog | Linked to a milestone in `IMPLEMENTATION_PLAN.md` | Acceptance criteria are concrete and testable |
| Ready | Scope, files, tests, and protocol/doc impact identified | Work can start without guessing |
| In progress | Branch created from `dev` | Code + tests + docs drafted |
| Review | Local gates pass | Reviewer or self-review checklist complete |
| Done | CI green on all platforms; status table updated | Next task can start |

Every work item records:

```text
ID / milestone / goal
Files touched
Protocol or doc impact
Tests added
Gates run
Status + blocker (if any)
```

A task is not "Done" because the code compiles. It is Done when acceptance
criteria from the plan are demonstrated and the evidence is recorded in the
status table at the end of this document.

## 4. Mandatory gates

Run locally before marking Review, then again in CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check -p stitch-mux-core --no-default-features
cargo check -p stitch-mux-client --no-default-features
cargo doc --no-deps --workspace
```

Additional gates by area:

| Area | Extra gate |
|---|---|
| protocol | Golden fixtures for every kind; truncated, oversized, bad-magic, wrong-version, bad-flags, unknown-kind cases |
| client `SessionView` | Snapshot replacement, delta apply, stale delta, seq gap, resync flag, disconnect clears state |
| server / pane runtime | Real-child echo, resize, exit, capture, final screen |
| detach/reattach | Full matrix from `IMPLEMENTATION_PLAN.md` §4, including GUI kill and daemon loss |
| multi-client | Independent focus, smallest-client resize, slow-client disconnect, bounded memory |
| GUI adapter | Headless layout/paint test where practical; manual DPI/theme/CJK checks |
| terminal adapter | Alternate-screen restore, wide chars, styles, cursor placement |
| release | `cargo test --workspace` on Linux/macOS/Windows plus the v0.1 release checklist |

## 5. Change control

### Wire protocol changes

1. Edit `PROTOCOL.md` first: field, kind, capability, and versioning impact.
2. Add or update a golden fixture. A schema change without a fixture change is
   a bug.
3. Update DTOs, encoder/decoder, client view, and server projection in one
   logical series of commits.
4. Record the decision in the log below.
5. Breaking 0.x changes bump the envelope version; additive fields use
   `#[serde(default)]` and remain optional for old peers.

### Architecture changes

1. Update `ARCHITECTURE.md` and, if the idea came from a reference,
   `REFERENCES.md`.
2. Add a decision-log entry with the rejected alternative.
3. Update `IMPLEMENTATION_PLAN.md` only if the milestone's deliverables or
   acceptance criteria changed.

### Hard constraints

- No raw-byte relay as the primary attach mode.
- No client-side emulator or ANSI parsing in GUI frontends.
- No hardcoded Kitty/xterm encoding in frontends.
- No network listener, accounts, tokens, or multi-user authorization.
- No freeform/overlapping layout in v0.1; layout is server-owned.
- No session restoration claim after daemon crash; GUIs show "session lost".

## 6. Milestone procedure

For each milestone:

1. **Open** — read its section in `IMPLEMENTATION_PLAN.md`; copy acceptance
   criteria into the work item; identify protocol/doc impact.
2. **Slice** — order work so the highest-risk requirement runs first. For M1
   that means real PTY → structured snapshot → attach → detach/reattach,
   before any GUI polish.
3. **Build** — implement one vertical slice at a time; keep gates green.
4. **Prove** — run unit, real-PTY, detach, and (from M3) multi-client tests.
5. **Close** — update this protocol's status table, update README if the status
   line changed, and fast-forward `main`.

Do not start M2 until task 10 ("single-pane egui app proves detach/reattach
with a real child") is green on all three platforms.

## 7. Review checklist

Before marking a task Ready for review:

- [ ] The change matches an acceptance criterion in the plan.
- [ ] No UI toolkit dependency leaked into core/protocol/server/client.
- [ ] Protocol changes have a golden fixture and a `PROTOCOL.md` update.
- [ ] Keyboard input still uses `encode_key` on the server side.
- [ ] New loops have cancellation and bounded queues.
- [ ] Errors are typed; no `unwrap` on untrusted input.
- [ ] Docs, comments, and the status table are current.
- [ ] All local gates pass.
- [ ] Commit history is one logical change per commit.

## 8. Decision log

| Date | Decision | Context | Consequence |
|---|---|---|---|
| 2026-11-18 | Use tokio's Unix socket / Windows named pipe instead of `interprocess` | Async endpoint already in the runtime; one abstraction over both platforms | `interprocess` remains a documented fallback; no blocking bridge needed |
| 2026-11-18 | `Render` wires a 13-byte binary prefix plus raw ANSI, not MessagePack | Keeps ANSI bytes unescaped while carrying `seq`/size/full | Decoded DTO stays in `dto::Render`; golden fixture records the prefix |
| 2026-11-18 | M0 is handshake-only; no PTY or session commands | Prove endpoint, framing, and typed errors before lifecycle complexity | CLI `ls` returns "arrives in M1"; `Ping` is the smoke test |
| 2026-11-18 | `dev` integrates, `main` is fast-forward only | Keeps a green stable branch while allowing feature work | No force-push to shared branches; tags only on explicit approval |
| 2026-11-18 | `PaneRuntime` owns `HistoryScreen`, Kitty flags, and the snapshot/delta stream | `PtySession` stays transport-only; reattach needs one authoritative screen | Frontends never parse ANSI or encode keys themselves |
| 2026-11-18 | Split framed connections into reader and writer halves | A cancelled `recv_frame` in `select!` loses partially decoded bytes | Reader task owns the framed decoder; pushes and commands share one writer |
| 2026-11-18 | `ResizeEvent` carries only `cols`/`rows` | `ResizeRequest` also carries pixel dimensions; the DTO shapes differ on the wire | Server sends the event DTO, not the request DTO, to avoid decode failures |
| 2026-11-18 | Consume `stitch-pty` 0.15.0 from crates.io, not a sibling path | CI checks out only `stitch-mux`; a `../stitch-pty` path dep cannot resolve there | Local and CI builds use the same published API |
| 2026-11-18 | The daemon owns the split tree and its revision; clients send intent | A stale `SetLayout` must never corrupt geometry | Rejected requests return `Busy` plus a fresh `Layout` |
| 2026-11-18 | Broadcast layouts carry no focus; each connection adds its own focus | Focus is per client, but layout changes are session-wide | Every layout push is followed by a per-client `Focus` frame |
| 2026-11-18 | Zoom uses `visible_pane_ids`, not all session panes | Reattach while zoomed must focus the pane actually on screen | Hidden panes stay alive but receive no input |
| 2026-11-18 | GUI splitters send `SetLayout` with the current base revision | A drag is an intent, not a local mutation; stale drags must be rejected | The daemon answers with a fresh `Layout`, which `SessionView` applies |
| 2026-11-18 | Preserve decoder state and pending frames when splitting a connection | Handshake reads can coalesce `HelloAck` with the first attach frames; discarding them stalls clients | `Connection::split` hands both to `FrameReader`; regression fixture covers it |
| 2026-11-18 | The daemon keeps one client registry with view mode, size, focus, and attach time | Canonical sizing and `DetachClient` need one authoritative record, not per-connection counters | Registry entries exist from handshake through EOF; `ClientInfo` exposes them |
| 2026-11-18 | Smallest attached client determines canonical size; no-op resizes do not bump revision | Multiple viewports must agree on one PTY geometry without churn | Attach/detach publish a layout so every client sees the new `attached_clients` count |
| 2026-11-18 | Preserve a typed daemon error when the socket closes | `DetachClient`'s `Busy` frame is followed immediately by EOF | `mark_lost_preserving_error` keeps the typed error and still marks the view lost |
| 2026-11-18 | Bound each client's update queue at 64 and coalesce deltas per pane | A stalled GUI must never block a PTY reader or grow memory without limit | `try_send` + lagged-snapshot fallback; one overflow closes with typed `Busy` after allowing the error frame to drain; a deterministic unit test asserts the frame |
| 2026-11-18 | Rewrite delta sequence numbers per client after coalescing | Dropping intermediate deltas must not look like a protocol gap | `UpdateCoalescer` keeps client-visible `seq` contiguous; `Resync` remains for real gaps |
| 2026-11-18 | GUI retries a lost session with exponential backoff, capped at 8s | Daemon loss must be visible and never silently recreated | Lost overlay offers Retry, New session, and Back to picker |
| 2026-11-19 | Shutdown excludes the issuing control connection | Detaching every client at once broke the `Shutdown` response with a broken pipe | Other clients get typed close; the command client receives its response first |
| 2026-11-19 | `PanePlacement` lives in `stitch-mux-core` and the protocol re-exports it | M0 lists placement as a core type and the architecture treats it as domain state | Wire shape is `tiled` / `floating { layer_id }` / `detached` |
| 2026-11-19 | `Attach { takeover: true }` is rejected as `Unsupported` | `PROTOCOL.md` reserves takeover for a later version and v1 must not silently force an attach | Typed-error test covers the handshake path |
| 2026-11-19 | Focus is remembered per `Hello.client_instance` | M2 requires this client's focus to survive detach/reattach | Bounded 1024-entry map, validated against the current visible panes; auto-focus changes update it |
| 2026-11-19 | Command responses carry the requesting connection's focus | Applying a split/close/zoom/SetLayout response must not clear the GUI's active pane | `response_layout` keeps a visible focus and falls back to the first visible pane |
| 2026-11-19 | `CommandResponse` is an 8 MiB payload kind; capture rows cap at 1000 | A bounded `CaptureResponse` can exceed the old 1 MiB limit and kill the control connection mid-frame | Frame cap test plus `CaptureResponse`/golden fixtures; docs updated |
| 2026-11-19 | `SessionInfo.last_activity_unix_ms` is the newest pane output timestamp | A picker must not report listing time as activity | `PaneRuntime` tracks the last delta; `Session::info` takes the maximum |
| 2026-11-19 | M0–M3 acceptance audit | Several deliverable clauses were untested even though code existed | Added every-kind frame round-trips, truncated-frame buffering, old-peer optional-field decoding, Unix endpoint permissions, child-exit, optional-metadata survival, focus restore, and large-response tests |
| 2026-11-19 | The pane reader drains to EOF; the exit watcher waits for the reader | A 150 ms drain sleep truncated the final bytes of a child that exited after a large burst | The reader no longer stops on a watch signal; the watcher waits up to 5 s for `reader_done` before publishing `Exit` |
| 2026-11-19 | 10 MiB stress is a release-mode CI job | Debug terminal emulation makes a literal 10 MiB burst slow; the acceptance must still be automated | `tests/stress.rs` spawns the `flood_fixture` binary, both clients must consume deltas and the authoritative capture must retain the marker, and a stalled third client must receive typed `Busy`; the test is `#[ignore]`d in the debug matrix |
| 2026-11-19 | Slow-client disconnect is proven end to end | A queue unit test does not prove the real socket closes with `Busy` | The same stress test stops reading one attached connection until the daemon closes it with `ErrorCode::Busy` / `"client fell behind"` |
| 2026-11-19 | Terminal DA/DSR replies go through a bounded scanner | `stitch-pty` 0.15.0 exposes no settable process-input callback | DA1, DA2, DSR 5, DSR 6, and private CPR are answered; scanner unit tests run everywhere and a raw-mode Unix test proves the child receives the reply |
| 2026-11-19 | GUI backoff and lost-session labeling are pure helpers | The picker and retry behavior were only exercised by hand | `next_backoff` covers 500 ms → 8 s; `session_label` covers the lost marker, sizes, clients, working count, and deterministic activity age |
| 2026-11-19 | Terminal clients receive `Layout` plus `Render`, never cell frames | Focus switching and shared geometry need pane IDs; cell snapshots would duplicate authoritative state | `send_session_state` sends geometry to both views, then either `PaneSnapshot` or a full `Render`; PROTOCOL.md states the boundary |
| 2026-11-19 | The ANSI compositor keeps one previous grid per terminal connection | Full repaints on every keystroke would defeat terminal compatibility | `AnsiCompositor` emits bounded cursor-addressed diffs, forces full frames on attach/geometry/`Resync`, and clips to 300x100 |
| 2026-11-19 | The terminal frontend writes bytes but never encodes keys or parses ANSI | Key encoding belongs to the focused pane's `HistoryScreen`; a second emulator would break reattach fidelity | `stitch-mux-terminal` maps host keys to `InputEvent` and paints server frames in sequence; a skipped frame requests `Resync` |
| 2026-11-19 | The conformance corpus is synthetic and deterministic | vim/less/htop are not guaranteed on every CI runner | `tests/conformance.rs` drives real snapshots through the compositor for bash/zsh, vim, less, htop/top, and git log patterns; `docs/CONFORMANCE.md` records the interactive pass |
| 2026-11-19 | The DSR end-to-end assertion accepts any CPR row/column | macOS reported a different cursor position than Linux while the daemon still answered the query | `tests/queries.rs` matches `ESC [ row ; col R` by shape and keeps the screen in the timeout diagnostic |
| 2026-11-19 | The terminal test asserts output through Capture, not one Render frame | A style run can split a word across coalesced ANSI diffs on macOS | Capture proves the child output; the test asserts that terminal Render sequence advanced without requiring one frame to contain the whole marker |

## 9. Current status

| Milestone | Status | Evidence / next action |
|---|---|---|
| M0 — workspace, core, protocol, client skeleton | **Done** | `3b6bcca`; every message kind round-trips byte-by-byte, truncated frames buffer, old peers can omit optional fields, and Unix endpoints are owner-only; fmt/clippy/test/doc green; CLI `ping` verified on Windows named pipe |
| M1 — one-pane GUI session with detach/reattach | **Done** | Real-child echo, capture, resize, detach/reattach, and clean child-exit tests; the reader drains to EOF before exit, so final output is not truncated; optional cwd/foreground/shell/status metadata survives snapshots and deltas; CLI `new`/`ls`/`kill`/`gui` smoke; abrupt GUI kill leaves child alive and clears attached count |
| M2 — multi-pane GUI layout and per-client focus | **Done** | Real-child split/focus/zoom/stale-revision/close test; focus restores per client instance across detach/reattach; picker receives pane sizes and real last-activity timestamps; CLI `new`/`split`/`ls`/`zoom`/`close`/`kill` smoke; 8-second GUI smoke leaves children alive with `attached_clients: 0` |
| M3 — multiple clients, backpressure | **Done** | 65 tests on Unix / 64 on Windows plus one release-only 10 MiB stress; real-endpoint tests for two-client focus/size/detach, focus restore, registry commands and typed shutdown, a two-client delta-pump test with repeated attach/detach, bounded-queue/coalescer unit tests, a deterministic typed `Busy` frame test, and an end-to-end stalled-socket `Busy` proof; `CommandResponse` payload cap covers captures; CLI `clients`/`rename`/`shutdown` smoke; GUI survives daemon loss |
| M4 — terminal ANSI compatibility | **Done** | 82 tests on Unix / 79 on Windows plus one release-only 10 MiB stress; bounded server compositor emits full and diff ANSI frames, terminal adapter writes them without parsing or key encoding, real-endpoint test attaches terminal + structured clients and asserts identical layout geometry/output/detach, deterministic conformance corpus covers bash/zsh, vim, less, htop/top and git log patterns, and CLI `attach` replaces the M4 stub; main CI `44598420738` green on all nine jobs |
| M5 — Qt/PySide6 + optional GPUI | Backlog | Second-toolkit proof, no protocol changes allowed |
| M6 — hardening / v0.1 | Backlog | Release checklist in `IMPLEMENTATION_PLAN.md` §7 |
| M7–M12 — v0.2/v0.3 | Backlog | Only after v0.1 is published |

Blocked items must be recorded here with owner and next action; an empty
blocker cell means the task is not blocked.

## 10. Living-document rule

Any change to status, decisions, gates, or conventions belongs in the commit
that causes it. If this document and code disagree, the code is considered
out of compliance until the next commit fixes the document or the behavior.
