# stitch-mux Implementation Plan

This is the execution plan for the architecture in
[ARCHITECTURE.md](ARCHITECTURE.md), the wire contract in [PROTOCOL.md](PROTOCOL.md),
the toolkit adapter design in [FRONTENDS.md](FRONTENDS.md), and the local
automation/agent interface in [AUTOMATION.md](AUTOMATION.md).

Execution follows the living [IMPLEMENTATION_PROTOCOL.md](IMPLEMENTATION_PROTOCOL.md):
branch rules, mandatory gates, decision log, and current status are updated in
the same commit as the work.

The priorities are explicit:

1. **GUI is the primary frontend.**
2. **Detach/reattach is a core requirement, not a later feature.**
3. **The server is authoritative; the GUI is a renderer and input surface.**
4. **No UI toolkit enters the daemon, core, or protocol client.**
5. **A terminal ANSI client remains as a compatibility projection.**

### Toolkit neutrality across M1–M4

M1–M4 are **toolkit-neutral at the product boundary**:

- `stitch-mux-core`, `stitch-mux-protocol`, `stitch-mux-server`, and
  `stitch-mux-client` have no egui/GPUI/Qt dependency.
- M1–M3 use **egui as a disposable reference adapter** to prove the structured
  protocol, layout, focus, and detach/reattach. It is not a product commitment.
- M2/M3 acceptance can also be exercised with protocol-level/headless GUI tests.
- M4 is the ANSI terminal compatibility path; it has no GUI toolkit at all.
- M5 adds the Qt/PySide6 adapter and optional GPUI adapter.

If a different toolkit is the primary target, swap the M1 reference adapter and
keep the protocol/server/client-state work unchanged. Do not start by porting
the mux into a toolkit-specific architecture.

Estimates are rough engineer-days for one experienced Rust developer, not
calendar promises. They assume the `stitch-pty` public API remains stable.

### Local scope and Rex-derived priorities

The Rex evaluation is filtered to local, single-user features. Remote access,
SSH/Tailscale, login/shared servers, PAM, multi-user accounts, port forwarding,
web clients, and iOS clients are explicitly out of scope.

| Priority | Feature | Release | Why it matters |
|---:|---|---|---|
| 1 | Program status/activity (OSC 7501) | v0.2 | The GUI needs a structured “needs attention” signal |
| 2 | Local automation and agent CLI | v0.2 | Agents can drive panes without screen scraping |
| 3 | Detached panes and ownership | v0.3 | Background work and multi-client size policy |
| 4 | Terminal fidelity (OSC 8, shell marks) | v0.2 | Normal terminal navigation and links |
| 5 | Actions, keymaps, command palette | v0.2 | GUI-first interaction without hard-coded keys |
| 6 | Multi-window and floating layers | v0.3 | Workspace model after placement is stable |
| 7 | Large scrollback and export | v0.2 | Capture, logs, and agent context |

v0.1 stays focused on the GUI core and detach/reattach. These priorities are
sequenced after it so they do not delay the first vertical slice.

| Release | Milestones | Rough estimate |
|---|---|---:|
| v0.1 | M0-M6 | 37-54 engineer-days |
| v0.2 | M7-M10 | 18-26 engineer-days |
| v0.3 | M11-M12 | 14-21 engineer-days |

## 1. Strategy

Build the smallest complete GUI session loop first:

```text
spawn shell → feed screen → structured layout/snapshot → GUI panes
     ↑                                                │
     └──────────── detach / reattach ─────────────────┘
```

The first vertical slice must include **detach and reattach**, because that is
the requirement most likely to expose architectural mistakes. Do not postpone it
until after terminal compatibility or cosmetic GUI work.

Order:

1. Protocol and `SessionView` state machine.
2. Server session/pane lifecycle.
3. One-pane egui GUI.
4. Detach, reconnect, full-state restore.
5. Multi-pane layout and per-client focus.
6. Multi-client/backpressure.
7. Terminal ANSI compatibility.
8. Qt/PySide6 and optional GPUI adapters.
9. Hardening and release.

After v0.1:

10. Program status, activity, and the GUI attention model.
11. Local automation, JSON events, and the agent skill.
12. Actions, keymaps, and command palette.
13. Terminal fidelity and large scrollback/export.
14. Detached panes and ownership.
15. Multi-window and floating layers.

## 2. Milestones

### M0 — Workspace, core, and structured protocol

**Goal:** a compiling workspace with the domain, protocol, and frontend state
contract, no PTY or GUI behavior yet.

**Estimate:** 3–4 engineer-days.

Deliverables:

- `Cargo.toml` workspace and initial crates:
  `stitch-mux-core`, `stitch-mux-protocol`, `stitch-mux-server`,
  `stitch-mux-client`, `stitch-mux`, plus stub GUI/terminal crates.
- `stitch-mux-core`: IDs, `Size`, `Rect`, `LayoutNode`, `PanePlacement`,
  `Cell`, `Color`, `Attrs`, `Cursor`, `Modes`, `ProgramStatus`,
  `ActivitySummary`, `CaptureFormat`, layout resolution and tests.
- `stitch-mux-protocol`: envelope, incremental decoder, MessagePack DTOs,
  `Layout`/`PaneGeometry`/`PaneSnapshot`/`PaneDelta`/`Input`, optional
  status/shell/link fields, capabilities, typed errors, golden fixtures.
- `stitch-mux-client`: `SessionView`/`PaneView`, apply-layout/snapshot/delta
  logic, activity state, sequence-gap detection, connection state machine
  skeleton.
- Endpoint abstraction for Unix sockets and Windows named pipes.
- CI: fmt, clippy, tests on Linux/macOS/Windows; core/client builds with no GUI
  dependency.
- `docs/` linked from README.

Acceptance:

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` pass.
- Frame encoder/decoder round-trips every message kind.
- Truncated, oversized, bad-magic, and wrong-version frames fail with typed
  errors and no allocation blow-up.
- `SessionView` applies a layout + two pane snapshots + deltas and reports a
  sequence gap correctly.
- Status/shell/link optional fields round-trip and older peers can omit them.
- Action/keymap DTO slots exist even if the registry is populated in M9.
- A client can connect to `server --foreground`, exchange `Hello`/`HelloAck`,
  and disconnect cleanly.
- Endpoint permissions are correct on Unix; named pipe is user-scoped on
  Windows.

Out of scope: PTYs, sessions, GUI, terminal rendering.

### M1 — One-pane GUI session with detach/reattach

**Goal:** a real GUI multiplexer loop for one pane, including the core
detach/reattach path.

**Estimate:** 7–10 engineer-days.

Deliverables:

- `PaneRuntime`: `Arc<PtySession>` + `HistoryScreen`; reader task; writer/query
  queue; structured snapshots/deltas; resize; child exit.
- Session registry and lifecycle: `NewSession`, `EnsureSession`, `ListSessions`,
  `Attach`, `Detach`, `KillSession`; detached sessions stay alive.
- Structured server path: `Attached` → `Layout` → full `PaneSnapshot`(s) →
  `PaneDelta`; `Focus`, `Input`, `ResizeRequest`/`ResizeEvent`, `Resync`.
- Optional status/shell/link fields are carried even before the GUI renders
  them; no parser work is required for this milestone.
- `stitch-mux-client` connection actor and `SessionView`.
- `stitch-mux-gui-egui`: single-pane reference window (swappable), cell-grid
  painter, cursor, input, resize, connection status, session picker/reattach.
- CLI: `new`, `ls`, `kill`, `gui` (launch GUI), `attach` (terminal later may be
  stub or minimal).
- Structured input mapping for text, keys, paste, focus; Kitty-aware key
  encoding through the server.

Acceptance:

```sh
stitch-mux new -s demo -- sh
stitch-mux gui            # pick "demo"
# type: echo hello
# close the GUI window (or kill the GUI process)
stitch-mux gui            # reattach "demo"
# "hello" is still visible; the shell is alive
stitch-mux ls
stitch-mux kill -t demo
```

- The child survives GUI close, normal detach, and abrupt GUI process kill.
- Reattach receives a full layout + snapshot; no stale local screen is shown
  before that state arrives.
- Typing, paste, focus, and resize work in the GUI.
- Cursor position/style and alternate-screen state are restored.
- Child exit becomes a clean `Exit`, not a daemon crash.
- `capture` returns visible rows and bounded scrollback.
- Integration tests cover echo, resize, detach/reattach, and child exit.
- Optional status/cwd/link fields survive snapshot replacement and deltas.

Out of scope: multiple panes, multiple clients, terminal ANSI, Qt, GPUI.

### M2 — Multi-pane GUI layout and per-client focus

**Goal:** a usable tiled GUI workspace.

**Estimate:** 8–12 engineer-days.

Deliverables:

- Server-owned binary split layout: `SetLayout`, split/close/focus commands,
  minimum sizes, ratio changes, zoom.
- GUI layout chrome in the reference adapter: pane borders, titles, active-pane
  state, status bar, draggable splitters (converted to `SetLayout`/ratio
  requests). The server/client-state work remains toolkit-neutral.
- Per-client focus: clicking a GUI pane changes only that client's focus;
  terminal clients later keep their own.
- Pane create/close/resize with correct PTY sizes.
- GUI session picker shows pane titles, sizes, attached clients, last activity.
- Layout revision handling; stale `SetLayout` returns a fresh `Layout`.
- Structured `PaneDelta` batching and row updates.
- `SessionInfo.activity` is present (zeros until status ingestion in M7).

Acceptance:

- Split columns/rows, focus in four directions, close a pane, zoom/unzoom.
- Each pane's child sees the expected `stty size`/equivalent geometry.
- Closing the focused pane transfers focus deterministically.
- Layout survives detach/reattach exactly (geometry and this client's focus).
- A stale layout request cannot corrupt geometry.
- Layout/compositor tests cover odd sizes, minimums, nested splits, hit testing.
- No panics on zero/one-cell panes; impossible splits are rejected cleanly.

Out of scope: floating/overlapping panes, per-client windows, copy mode.

### M3 — Multiple clients, session management, and backpressure

**Goal:** a GUI and another client can share a session without corruption, and a
lost connection is recoverable.

**Estimate:** 5–7 engineer-days.

**Status: Done.** Version `0.1.0` is ready as a candidate; publication needs
explicit approval. The 30-minute soak is available via `STITCH_MUX_SOAK_MS` and
remains an operator gate; CI runs the short form on every platform.

Deliverables:

- Client registry with view mode, size, focus, and attach time.
- Multiple structured GUI clients; each with per-client focus, shared layout.
- Canonical size recomputation on attach/detach/resize (smallest-client policy).
- Bounded per-client queues; coalesce `PaneDelta` per pane; preserve latest
  cursor/modes/title; slow-client disconnect policy.
- `seq`/`layout_revision` gap detection and `Resync`.
- Reconnect with exponential backoff; session picker marks disconnected/lost.
- `ListClients`, `DetachClient`, session rename.
- Daemon shutdown/lost-session UX: show “session lost,” offer a new session;
  never silently recreate and imply restoration.

Acceptance:

- Two GUI clients attach; both see the same panes; each has independent focus.
- Detaching one leaves the other attached and input works.
- A smaller client resizes the session; larger clients get the new layout.
- A slow client cannot block PTY readers; it is disconnected with a typed error.
- Kill/restart the GUI at any point; reattach restores full state.
- Kill the daemon; GUI shows a clear lost-session state.
- Stress: 10 MiB output to two clients, repeated attach/detach, bounded memory.

Out of scope: per-client windows, collaborative editing, remote clients.

### M4 — Terminal ANSI compatibility

**Goal:** keep a terminal client for CI and users who prefer terminals, sharing
the same sessions and layout as the GUI.

**Estimate:** 4–6 engineer-days.

Deliverables:

- Server ANSI compositor: layout, borders, status bar, cursor, full frame.
- `stitch-mux-terminal`: raw mode, alternate screen, frame pump, input/resize,
  detach, terminal restore.
- Terminal and structured clients attach simultaneously; shared layout and PTYs.
- Terminal frame coalescing and `Resync`.
- ANSI encoder tests (styles, colors, wide chars, cursor placement).
- Conformance corpus: bash/zsh, vim, less, htop/top, git log.
- Hyperlink/shell metadata is preserved through the structured path; terminal
  rendering of links is a v0.2 fidelity task.

Acceptance:

- `stitch-mux attach -t demo` works from a normal terminal while the GUI is open.
- Layout and pane sizes are consistent between both clients.
- Terminal attach/detach does not disturb the GUI.
- Vim/less alternate-screen transitions restore the shell screen.
- Wide characters and styles survive the ANSI round trip.
- No terminal-specific assumptions leak into the structured path.

Out of scope: copy mode/search, graphics protocols, full tmux compatibility.

### M5 — Qt/PySide6 adapter and optional GPUI spike

**Goal:** prove the frontend boundary with a second toolkit, prioritizing the
Python/Qt path.

**Estimate:** 5–8 engineer-days.

Deliverables:

- `stitch-mux-py`: PyO3 client exposing `SessionView`, `connect`, `attach`,
  `detach`, `input`, `resize`, `set_layout`, `capture`, `list_sessions`.
- PySide6 `QWidget` adapter:
  - `paintEvent` draws row/style runs, backgrounds, cursor;
  - signals deliver batched snapshots/deltas to the Qt thread;
  - key/mouse/IME mapping;
  - session picker and reconnect state.
- Offscreen Qt tests (`QT_QPA_PLATFORM=offscreen`).
- Optional GPUI spike behind a feature flag, using the same `stitch-mux-client`
  state; do not make it a release blocker.
- Frontend parity checklist from [FRONTENDS.md](FRONTENDS.md).
- Activity/status hooks and command-palette stubs so v0.2 UI work does not
  require a protocol rewrite.

Acceptance:

- A PySide6 window attaches to a running session and draws panes.
- Typing, resize, detach, and reattach work.
- A second GUI toolkit does not require protocol changes.
- Rust core tests still pass without Python or Qt.
- Qt packaging/licensing notes documented.

Out of scope: production-quality Qt theming, full accessibility audit, GPUI
release support.

### M6 — Hardening and v0.1

**Goal:** release-quality GUI-first first version.

**Estimate:** 5–7 engineer-days.

Deliverables:

- Windows/macOS/Linux GUI builds and smoke tests.
- Protocol fuzzing (frame decoder, pane/capture payloads).
- Detach/reattach soak: close/crash GUI repeatedly, daemon stays stable.
- Multi-client/per-client focus stress.
- Bounded memory/frame checks; slow client tests.
- GUI diagnostics: connection state, session list, daemon start failure, log
  path.
- README, architecture/protocol/frontend/automation docs, changelog, license
  files.
- Version `0.1.0`; publication only after explicit approval.

Acceptance:

- All CI jobs green on three platforms.
- GUI starts, creates/attaches, splits, focuses, detaches, and reattaches.
- Terminal compatibility path passes its smoke tests.
- v0.1 does not depend on status/automation; those are v0.2 release gates.
- Short soak with repeated output/attach/detach runs in CI; the 30-minute form
  is available on demand and remains an operator gate.
- No known P0/P1 bug in detach/reattach, per-client focus, input encoding, or
  child cleanup.
- Release checklist in §7 complete.

Out of scope: freeform layout, floating layers, copy mode, plugins, web,
session restoration, graphics protocols, and every remote/multi-user feature.

**v0.2 — local activity, automation, and fidelity**

v0.2 is M7-M10. It is deliberately local and single-user; no remote transport or
multi-user feature enters these milestones.

### M7 — Program status and activity

**Goal:** turn pane activity into structured state the GUI and agents can use.

**Estimate:** 4-6 engineer-days.

**Status: Done.** OSC 7501 ingestion, current-status propagation, activity
aggregation, GUI attention UI, the shell helper, and the bounded event stream
are implemented with unit, real-PTY, and event tests.

Deliverables:

- OSC 7501 status ingestion (state, kind, app, message) alongside the terminal
  screen feed. If `stitch-pty` does not expose the record, add a mux-side byte
  scanner that observes the same stream; do not fork the emulator.
- `ProgramStatus` and `ActivitySummary` in `PaneSnapshot`/`PaneDelta` and
  `SessionInfo`.
- Current-status replacement semantics; no unbounded status queue.
- `program_status_changed` events and activity counts in `ListSessions`.
- GUI: per-pane indicator, tab/session badges, activity filter/list, and
  unread/seen state.
- Terminal client: optional simple indicator or ignore.
- Shell function and docs for program authors.

Acceptance:

- A program emitting OSC 7501 updates its pane status within one frame.
- A later status replaces an earlier one; a pane with no report has no status.
- Blocked/done/error are distinguishable in the GUI without color alone.
- Activity counts match pane statuses after attach, detach, and reattach.
- A slow client does not receive an unbounded backlog of status transitions.
- OSC 7501 parse is covered by unit tests and a real-PTY integration test.

Out of scope: mobile push, system notification policy beyond a basic hook,
priority inbox, and multi-user aggregation.

### M8 — Local automation and agent CLI

**Goal:** make the mux scriptable and agent-friendly without a remote API.

**Status: Done.** Daemon-side `Run`/`Wait`, the stable CLI automation surface,
bounded NDJSON events, capture formats, and the portable agent skill are
implemented. New-window placement and named actions remain later milestones.

**Estimate:** 5-7 engineer-days.

Deliverables:

- CLI `run`, `wait`, `send`, `send-keys`, `capture`, `ls --json`, and
  `events --json`.
- `run --wait` returns the child status; `wait` works on existing panes and
  sessions with an optional timeout. `run` places the pane in the active window;
  new-window placement waits for M12.
- Stable IDs and stable JSON schemas for session/pane/result objects.
- Bounded NDJSON event subscription; sequence numbers and disconnect-on-lag.
- Capture formats `text` and `json`; `cells` for tooling.
- Agent skill in `agent-skill/` for Claude Code/Codex/OpenCode/Pi-compatible
  skill layouts; no privileged hook required.
- Docs: [AUTOMATION.md](AUTOMATION.md), command examples, exit statuses.

Acceptance:

- `run --wait -- sh -c 'exit 7'` returns 7.
- A script can create a pane, capture its output, and close it without attaching
  a GUI.
- `events --json` emits `pane_exited`, `program_status_changed`, and
  `activity_changed` with monotonic `seq`.
- A slow subscriber is disconnected or resynced; PTY readers never block.
- The agent skill can be installed idempotently and uses only the local CLI.
- No network listener or remote account is introduced.

Out of scope: MCP server, remote API, Lua/embedded scripting, and multi-user
authorization. A local MCP wrapper may be added later over the same CLI.

### M9 — Actions, keymaps, and command palette

**Goal:** replace ad-hoc shortcuts with a named action system shared by GUI,
terminal, and CLI.

**Status: Done.** The core registry/dispatcher, daemon commands and events,
CLI, terminal and egui frontends, JSON keymap reload, command palette, and
which-key/mode indicators are implemented with unit and integration tests.

**Estimate:** 4-6 engineer-days.

Deliverables:

- `ActionRegistry` in core/server with typed action names and arguments.
- Key tables: base layer, named modes, sequences, double-prefix passthrough,
  validation, source reporting, and reload.
- Protocol: `ListActions`, `GetKeymap`, `InvokeAction`; keymap/action change
  events.
- GUI: command palette, which-key hints, active-mode indicator, and action
  search; all behavior comes from the registry.
- CLI: `actions`, `get-keymap`, `invoke`.
- Config file format for bindings and modes (no Lua).

Acceptance:

- `Ctrl-b`, `%`, `"`, arrows, `x`, `z`, `d`, and `?` are registry entries with
  the same semantics in GUI and terminal clients.
- A `resize` mode can rebind arrows and exit on Escape.
- A multi-key sequence works; a double prefix sends the literal prefix.
- Invalid action args are rejected before execution, with a useful message.
- Command palette and key hints match the daemon's effective keymap.
- Reloading bindings does not require restarting the daemon or GUI.

Out of scope: per-client GUI actions, Lua, plugins, and remote control of a
client.

### M10 — Terminal fidelity and scrollback/export

**Goal:** cover the normal terminal behaviors a GUI user expects, without
chasing graphics protocols.

**Status: Done.** Styled `vt`/`html` capture, bounded scrollback,
shell-integration metadata (OSC 133 prompt/command marks and OSC 1337 cwd), GUI
prompt jumps, OSC 8 link metadata with safe click handling, egui
selection/copy across scrollback, current OSC 9/777 notification / OSC 9;4
progress metadata, and fractional smooth scrolling are implemented. A real-PTY
integration test covers alternate-screen entry/exit and shell-screen restore.
Reflow is explicitly out of scope: the daemon owns fixed canonical geometry and
the reference GUI does not rewrap historical rows.

**Estimate:** 5-7 engineer-days.

Deliverables:

- OSC 8 hyperlink metadata through the screen and structured view; safe link
  handling in the GUI.
- Shell integration: cwd, foreground process, prompt/command marks, and prompt
  jumps.
- OSC 9/777 desktop notifications and OSC 9;4 progress as optional metadata.
- Native selection/copy/paste, copy-on-selection option, select-all including
  scrollback.
- Bounded scrollback budget and `capture` formats `vt` and `html`.
- Smooth scrolling/reflow where the toolkit supports it.
- Docs for terminal fidelity limits and link safety.

Acceptance:

- A program emitting OSC 8 links produces visible, targetable cells with
  confirmation for custom schemes.
- Prompt jump moves to the previous/next prompt without parsing screen text.
- Selection/copy works across scrollback without corrupting styles.
- Scrollback stays bounded under continuous output; capture ranges are enforced.
- Vim/less/htop alternate-screen transitions still restore the shell screen.
- No sixel/Kitty graphics support is implied.

Out of scope: copy mode/search, graphics protocols, full underline conformance,
and tmux compatibility.

**v0.3 — workspace model**

v0.3 is M11-M12. It extends the session model; it does not add remote access.

### M11 — Detached panes and ownership

**Goal:** decouple pane lifetime from window placement and resolve local
multi-client size preferences.

**Status: Done.** Session-owned placements, an explicit empty layout,
`DetachPane`/`PlacePane`, `ClaimPane`/`ReleasePane` size ownership, placement/
owner events, capability gating, CLI controls, and GUI background-pane actions
are in place. Real-PTY tests cover detach/place, detached exit, explicit close,
and owner failover. The 30-minute soak remains an operator gate.

**Estimate:** 6-9 engineer-days.

Deliverables:

- `PanePlacement` (`Tiled`, `Floating`, `Detached`) and session-owned
  `PaneRuntime` map; windows reference pane IDs.
- `DetachPane` and `PlacePane`; detached panes run, capture, wait, and report
  activity.
- Background-pane list in the GUI; attach/place/close actions.
- Optional `ClaimPane`/`ReleasePane` size-owner policy; smallest-client remains
  the default when no owner exists.
- Owner changes reflected in `PaneInfo` and the v0.2 event stream.
- Tests for detach/reattach, pane exit while detached, and owner failover.

Acceptance:

- A pane can be detached, left running for minutes, captured, then placed into a
  different split without restarting its child.
- Detaching the last pane leaves an empty window; closing the pane is explicit.
- A detached pane's scrollback and activity survive.
- Two local clients with different sizes get deterministic PTY sizing under the
  chosen policy.
- No pane runtime leaks after repeated detach/place/close.

Out of scope: floating geometry (M12), remote clients, and collaborative
ownership.

### M12 — Multi-window and floating layers

**Goal:** add the workspace features that make a GUI multiplexer feel larger than
one tiled window, after the placement model is stable.

**Estimate:** 8-12 engineer-days.

Deliverables:

- Multiple windows/tabs per session; `WindowId` in layout/commands and GUI tabs.
- Window create/close/move/rename/focus-next/previous.
- Floating layers with fractional `[x, y, w, h]` bounds, z-order, raise/lower,
  resize, and close.
- Float a tiled pane, create a new floating pane, minimize/restore floating
  windows, and return focus correctly.
- Layout revisions for tiled + floating structures; `SetLayout` extended with
  window/layer identity.
- GUI tab bar/sidebar and window picker; terminal fallback can show a single
  active window.

Acceptance:

- Two windows with independent tiled layouts share one session and PTYs.
- A floating pane can overlap tiled panes, be raised/lowered, resized, and
  closed without corrupting the tiled layout.
- Floating bounds survive detach/reattach.
- Window/layer operations are rejected atomically when they would violate
  minimum sizes or limits.
- Terminal clients see a deterministic active-window projection; GUI clients
  see the full workspace.

Out of scope: freeform GUI-owned layout, arbitrary overlapping tiled panes,
remote/multi-user sessions, and plugin panes.

## 3. First ten tasks

1. Create workspace, crate manifests, license files, and CI.
2. Define IDs, layout, `PanePlacement`, `Cell`/`PaneSnapshot`/`PaneDelta`,
   `ProgramStatus`/`ActivitySummary`, and layout tests.
3. Implement the frame envelope and incremental decoder with golden/fuzz tests.
4. Implement structured `Layout`/`PaneSnapshot`/`PaneDelta`/`Input` DTOs and
   reserve optional status/shell/link/action fields.
5. Implement endpoint discovery/binding (Unix socket, Windows pipe).
6. Implement `PaneRuntime` with `PtySession` + `HistoryScreen`.
7. Implement session registry/lifecycle (`new`, `ensure`, `list`, `attach`,
   `detach`, `kill`).
8. Implement the structured server projection and per-client focus.
9. Implement `stitch-mux-client` `SessionView` and sequence/resync handling.
10. Build the single-pane egui app and prove detach/reattach with a real child.

Do not start M2 until task 10 is green on all three platforms.

## 4. Testing strategy

### Unit tests

- Layout: nested splits, minimums, odd dimensions, zero-size inputs.
- Pane geometry: outer/content rects, border/title space, PTY sizing.
- `SessionView`: snapshot replacement, delta apply, seq gap, resize/reset,
  alternate-screen reset, focus changes, activity state.
- Frame/ANSI encoder: style runs, color fallback, cursor placement, wide chars.
- Protocol: every message round-trip, truncated/oversized/bad-version frames,
  optional status/shell/link fields, old-peer omission.
- Key mapping: key codes/modifiers, prefix state, double-prefix, Kitty flags,
  action lookup, key modes, invalid action args.
- Capture: absolute rows, bounds, wrapped lines, style preservation, text/json/
  vt/html formats, alternate-screen behavior.
- Program status: OSC 7501 states/kinds/message, replacement semantics, activity
  aggregation, malformed records.

### Real-PTY integration tests

Run against actual children:

- echo/printf output appears in a pane snapshot;
- cursor position and styles are preserved;
- resize changes `stty size`/`tput cols` (or Windows equivalent);
- child exit yields `Exit` and a stable final screen;
- detach/reattach returns the same visible screen and child state;
- terminal query produces a reply at the child;
- Unicode/wide characters survive the round trip;
- alternate-screen enter/exit restores the primary screen;
- a real child emits OSC 7501 and the pane reports the expected status;
- a shell-integration child reports cwd/prompt marks;
- a hyperlink cell preserves its OSC 8 URI;
- `run --wait` returns the child's exit status;
- a detached pane can be captured and placed without restarting the child.

### Detach/reattach matrix

These are mandatory, not optional:

| Scenario | Expected |
|---|---|
| GUI sends `Detach` | Session alive; client removed; reattach full state |
| GUI window closes without detach | Socket EOF treated as detach; session alive |
| GUI process killed | Same as EOF; session alive |
| GUI reconnect | New handshake, `Layout` + full snapshots, no stale frame |
| Daemon unavailable | GUI shows reconnecting/lost state, no fake session |
| Two GUI clients | Shared layout, independent focus, both restore |
| GUI + terminal | Shared layout; both restore independently |
| Pane exits while detached | Reattach shows final screen and exit status |

### GUI tests

- Protocol/state tests cover the GUI's data path without a window.
- egui: headless layout/painter tests where practical; screenshot/manual visual
  checks for cell rendering.
- Qt: offscreen `QWidget` tests with `QT_QPA_PLATFORM=offscreen`.
- GPUI: optional smoke test only until it is a release target.
- Manual visual matrix: 100%/125%/150% DPI, light/dark themes, CJK/emoji,
  selection, cursor shapes, IME.

### Protocol/fuzz tests

- Golden fixtures for every message.
- `cargo fuzz` targets for frame header, MessagePack payload, pane rows, capture.
- Malformed/oversized frames must return typed errors, not panic or allocate
  unbounded memory.

### CI matrix

| Job | What |
|---|---|
| fmt | `cargo fmt --check` |
| clippy | host + Linux/macOS/Windows targets, `-D warnings` |
| test-linux | `cargo test --workspace` |
| test-macos | `cargo test --workspace` |
| test-windows | `cargo test --workspace` |
| gui-egui | build/check egui adapter |
| gui-gpui (optional) | feature-gated check, not required for v0.1 |
| qt-python (later) | maturin build, offscreen pytest, ruff, mypy |
| docs | `cargo doc --no-deps` |

Keep `stitch-mux-client` and `stitch-mux-core` buildable without GUI features:
`cargo check -p stitch-mux-client --no-default-features`.

## 5. Performance budgets

Targets for a local 200×50 session, release mode:

| Metric | Target |
|---|---|
| First structured frame after attach | < 100 ms |
| Key-to-echo latency | < 20 ms typical |
| GUI paint for 200×50 | < 8 ms on a normal desktop |
| Render update rate | 30–60 Hz coalesced, idle when no output |
| Idle CPU (GUI attached) | < 2% |
| 10 MiB output to one client | no dropped input, bounded memory |
| Memory, 10 panes + 1,000 scrollback rows | bounded; record baseline |
| Slow client | server stays responsive; client disconnected if queue overflows |
| Status update latency | visible in the GUI within one coalesced frame |
| Event stream lag | bounded; slow subscriber disconnected, not unqueued |
| Detached pane overhead | no measurable reader/writer regression |

Record actual numbers. GUI paint time and text shaping are the likely
bottlenecks, not PTY reads.

## 6. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Protocol designed around ANSI by accident | GUI needs a rewrite | Structured view is M0/M1; ANSI is M4 compatibility |
| Detach/reattach bolted on later | Lost sessions, stale UI | Make detach/reattach the M1 acceptance test |
| GUI toolkit leakage into core | Cannot support egui/GPUI/Qt together | `stitch-mux-client` has no UI deps; adapters are separate crates |
| egui text shaping/performance | Poor CJK/emoji or slow large grids | Test early; batch style runs; keep GPUI/Qt options open |
| GPUI API churn | Constant breakage | Feature-gate, pin version, optional until stable |
| Python paint overhead | Low frame rate in Qt | Row runs, QImage/glyph atlas, Rust-side renderer; offscreen benchmarks |
| Qt licensing surprise | Distribution problem | Document LGPL/commercial obligations early |
| Text shaping/IME differences | Wrong cursor/composition | Per-toolkit IME tests; cursor cell from server |
| Multi-client focus races | Input goes to wrong pane | Per-client focus state, server-owned layout |
| Slow GUI blocks daemon | Stalled PTYs | Bounded queues, delta coalescing, disconnect policy |
| Wide chars/combining marks | Misaligned grid | Explicit width/continuation tests; draw runs, not cells |
| Daemon crash loses sessions | User surprise | Document v0.1; clear lost-session UI; restoration later |
| OSC 7501 parser incompatibility | Wrong activity/GUI confusion | Generic protocol docs; unknown fields ignored; status optional |
| Agent automation security | Local tool can run arbitrary user commands | Same local permissions; no network; no privileged hook |
| Detached pane leak | Memory/PTY exhaustion | Session-owned runtime, explicit close, soak tests |
| Owner size thrash | Resize storms with two clients | Debounce, deterministic ownership, smallest-client fallback |
| Multi-window/floating scope creep | Delays v0.1/v0.2 | Keep M11-M12 in v0.3; no freeform GUI layout |
| Scope creep | Never ships | Enforce non-goals; remote/multi-user explicitly out |

## 7. Release checklist (v0.1)

- [x] `cargo fmt --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo test --workspace` on Windows, Linux, macOS
- [x] `cargo check -p stitch-mux-client --no-default-features`
- [x] egui adapter builds on all three platforms
- [x] detach/reattach demo (normal close, process kill, reattach) passes —
      `tests/soak.rs` alternates `detach()` with abrupt `abort()` for 12 cycles
- [x] two-client/per-client-focus demo passes — three clients switch focus for
      20 rounds in `tests/soak.rs`
- [x] terminal compatibility smoke test passes — `tests/terminal.rs` and
      `tests/conformance.rs`
- [x] Protocol golden tests and fuzz smoke test pass
- [x] Endpoint permissions verified — Unix mode test in
      `tests/endpoint.rs`; Windows uses a per-user pipe name and the OS default
      named-pipe DACL (creator, SYSTEM, Administrators), never a TCP listener
- [x] No known P0/P1 bugs
- [x] README, ARCHITECTURE, PROTOCOL, FRONTENDS, AUTOMATION,
      IMPLEMENTATION_PLAN, REFERENCES current
- [x] CHANGELOG and version bumped to `0.1.0`
- [x] `stitch-pty` dependency pinned to released `0.15.0` from crates.io
- [ ] Crates.io/PyPI publication explicitly approved (do not publish by default)
- [ ] 30-minute soak run with `STITCH_MUX_SOAK_MS=1800000` (operator gate)

## 8. Remaining backlog after v0.3

The feature milestones above (M7-M12) are the v0.2/v0.3 roadmap. Remaining
follow-up work:

1. **Freeform/overlapping layout** — explicit GUI-owned layout capability,
   separate session mode; keep tiled server layout for shared sessions.
2. **Copy mode and search** — server-side capture first; then server-side or
   client-local selection/search; yank/paste integration.
3. **GPUI production adapter** — only if egui/Qt performance or UX is
   insufficient; pin and isolate.
4. **Full Qt/PySide6 polish** — accessibility, IME, theming, packaging.
5. **Full terminal conformance** — advanced underline styles, graphics
   protocols, broader corpus; OSC 8/shell integration are already M10.
6. **Session restoration** — opt-in layout/scrollback restoration; clearly
   distinguish from live process restoration.
7. **Windows hardening** — pipe ACLs, console input, ConPTY resize.
8. **Theme/status-bar/scrollback configuration polish.**

Remote/web transport and multi-user servers are **not** backlog items. They are
out of scope for stitch-mux and must not be reintroduced as an “optional later”
feature without a separate product decision and threat model.

## 9. Definition of done for v0.1

A user can:

1. start a named session with a shell or command;
2. open the GUI, split into at least two panes, focus them, resize the window,
   and type in both;
3. close the GUI, reopen it, and find the session, panes, screen contents, and
   child processes intact;
4. repeat that flow after killing the GUI process, not just closing its window;
5. attach a terminal client to the same session and see the same layout;
6. run a full-screen TUI in a pane without obvious corruption;
7. use the same core workflow on Linux, macOS, and Windows.

And the maintainers can point to green CI, real-PTY tests, a detach/reattach
matrix, a documented structured protocol, and no known data-loss path on
detach/reattach.

## 10. Definition of done for v0.2 and v0.3

**v0.2:** status/activity is visible and bounded; the local CLI can run, wait,
capture, send, and stream events; the agent skill uses only the local CLI;
actions/keymaps drive GUI, terminal, and CLI consistently; OSC 8 and shell marks
work without graphics scope creep.

**v0.3:** detached panes survive placement changes and can be captured/waited
on; local size ownership is deterministic or the smallest-client fallback is
documented; multiple windows and floating layers share a session without
corrupting tiled geometry; all of it remains local and single-user.
