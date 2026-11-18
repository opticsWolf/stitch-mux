# stitch-mux Architecture

## 1. Scope

`stitch-mux` is a **terminal multiplexer** in the tmux/Zellij/WezTerm sense: a
long-lived daemon owns PTYs and terminal state, and local clients attach to a
session, render its panes, and send input. It is not a network multiplexer, an
SSH client, a terminal emulator, or a multi-user service.

The design target is a reliable single-user, local-first multiplexer with:

- one daemon per user (or per explicit socket);
- named sessions that survive client detach;
- windows and tiled panes;
- faithful screen restoration on reattach;
- native Unix and Windows behavior;
- structured program status and activity;
- a local CLI/event surface that humans and coding agents can drive;
- a small protocol surface that can grow without breaking old clients.

Remote access, SSH, Tailscale, port forwarding, shared/login servers, PAM,
multi-user accounts, and a web client are explicitly out of scope. That keeps
the trust model local and lets the core focus on GUI fidelity, detach/reattach,
and local automation.

## 2. Design principles

1. **Server-authoritative terminal state.** The daemon owns every PTY and every
   pane's terminal screen. Clients are views, not the source of truth.
2. **`stitch-pty` stays transport-only.** The mux owns session, layout, and
   composition. It uses `PtySession` plus `HistoryScreen`, not a modified
   `stitch-pty`.
3. **No second terminal emulator in the client.** Clients receive prepared
   cells (structured view) or composed ANSI (terminal view), never raw PTY bytes
   by default. This keeps reattach fidelity high and GUI rendering deterministic.
4. **Local-only by default.** Unix domain sockets and Windows named pipes, with
   owner-scoped endpoints. No TCP listener in the first release.
5. **Bounded state and backpressure.** Slow clients must never block PTY readers
   or grow server memory without limit.
6. **Versioned protocol.** A client/server version mismatch produces a clear
   error, not silent corruption.
7. **Compose, don't fork.** Reuse proven crates and published patterns; do not
   vendor Zellij, WezTerm, or rmux internals.
8. **GUI-first, frontend-agnostic.** The structured view is the primary v1
   frontend contract. egui, GPUI, and Qt/PySide6 adapters consume the same
   toolkit-independent client state; no UI toolkit enters the server or core.
9. **Small first release.** One window per session, server-owned tiled layout,
   and per-client focus are enough for v0.1. Freeform layout, copy mode, plugins,
   and graphics come later.
10. **Activity is first-class state.** Program status is not a notification
    afterthought. The daemon keeps structured activity per pane and exposes it
    in snapshots, session summaries, and the local event stream.
11. **Pane lifetime is independent of placement.** A pane can be detached from
    a window without killing its PTY. Detached panes remain addressable by ID,
    capturable, and available to reattach or place later.
12. **Actions are explicit.** Keyboard shortcuts, the GUI command palette, and
    the local CLI invoke named actions. Bindings are configuration; actions are
    the stable contract.

## 3. System context

```text
┌────────────────────────────────────────────────────────────────────────┐
│ stitch-mux daemon                                                      │
│                                                                        │
│  Session ── Window ── Layout tree ── PaneRuntime                       │
│                                      ├─ Arc<PtySession>                │
│                                      ├─ HistoryScreen                  │
│                                      ├─ reader task                    │
│                                      └─ writer/query channel           │
│                                                                        │
│  SessionView state                                                     │
│    ├─ structured projection: Layout + PaneSnapshot/PaneDelta           │
│    ├─ activity projection:  ProgramStatus + ActivitySummary            │
│    └─ terminal projection:  composed Frame ── ANSI encoder             │
│                                                                        │
│  client fan-out (bounded queues; per-view-mode messages)               │
└────────────────────────────────────────────────────────────────────────┘
                 │                                  │
                 │ Unix socket / named pipe         │
                 ▼                                  ▼
┌───────────────────────────────┐   ┌───────────────────────────────────┐
│ GUI frontend                  │   │ terminal frontend                 │
│ egui / GPUI / Qt-PySide6      │   │ thin ANSI attach client           │
│ draws pane cells + own chrome │   │ writes composed frames            │
│ activity + command palette    │   │                                   │
└───────────────────────────────┘   └───────────────────────────────────┘
                 ▲
                 │ local CLI / JSON events / agent skill
┌────────────────────────────────────────────────────────────────────────┐
│ scripts and coding agents                                              │
└────────────────────────────────────────────────────────────────────────┘
```

Control commands (`new`, `ls`, `kill`, `capture`, `run`, `wait`, `events`) use
short-lived local connections, except event subscriptions. Attach connections
are long-lived and carry only the attach stream. All of these paths use the same
owner-scoped local socket or named pipe; there is no network listener.

## 4. Components

### 4.1 CLI: `stitch-mux`

The user-facing binary:

```text
stitch-mux new [-s NAME] [--detached] [-- PROGRAM ARGS...]
stitch-mux gui [-t NAME]          # primary GUI frontend
stitch-mux attach [-t NAME]       # ANSI terminal compatibility
stitch-mux ls [--json]
stitch-mux kill [-t NAME]
stitch-mux split [-t NAME] [-p PANE] [-- PROGRAM ARGS...]
stitch-mux run [-t NAME] [--wait] [--keep-open] -- PROGRAM ARGS...
stitch-mux wait [-t NAME] [-p PANE] [--timeout DURATION]
stitch-mux capture [-t NAME] [-p PANE] [--start N] [--lines N]
                   [--format text|cells|json|vt|html]
stitch-mux send [-t NAME] [-p PANE] -- TEXT...
stitch-mux send-keys [-t NAME] [-p PANE] -- KEYS...
stitch-mux events [-t NAME] [--json] [--all]  # local event stream
stitch-mux actions [-t NAME]                   # list named actions
stitch-mux invoke [-t NAME] ACTION [KEY=VALUE...]
stitch-mux server [--foreground] [--socket PATH]
```

The CLI ensures a daemon exists, then either attaches or sends a control
request. `run --wait` creates a pane, waits for its child to exit, and returns
that exit status; `wait` does the same for an existing pane or session.
`capture` serializes the visible screen or scrollback. `events` streams local
session, pane, activity, and exit events as JSON Lines. `server --foreground`
is the debugging entry point. See [AUTOMATION.md](AUTOMATION.md).

### 4.2 Daemon: `stitch-mux-server`

One process holds all sessions and PTYs. It is normally started detached, but
the same code path runs in the foreground for tests and debugging. It owns:

- endpoint binding and stale-endpoint cleanup;
- session registry and lifecycle;
- per-session command loop;
- per-pane reader/writer tasks;
- compositor and render fan-out;
- signal handling and shutdown;
- protocol decode/encode and backpressure;
- local automation commands and event subscriptions;
- action registry and key-table resolution.

The daemon is not a remote service. It has no TCP listener, SSH/Tailscale
integration, user database, or multi-tenant authorization model.

### 4.3 Core: `stitch-mux-core`

Pure domain types with no tokio, no filesystem, and no PTY dependency:

- `SessionId`, `WindowId`, `PaneId`, `ClientId` newtypes;
- `Size`, `Rect`, `SplitAxis`, `LayoutNode`, `PanePlacement`;
- `Cell`, `Color`, `Attrs`, `Frame`, `Cursor`, `Modes`;
- `ProgramStatus`, `ProgramState`, `ActivitySummary`;
- `CaptureFormat`, `ActionId`, `ActionArgs`;
- layout arithmetic and hit testing;
- frame diff helpers.

Keeping this crate pure makes geometry, frame encoding, and protocol tests fast
and deterministic. It can also compile to WASM later for a browser renderer.

### 4.4 Protocol: `stitch-mux-protocol`

The wire contract, separate from the domain model where practical:

- frame envelope and incremental decoder;
- control request/response DTOs;
- attach hello/render/input/resize/detach messages;
- capability bits and protocol version;
- encode/decode tests and golden fixtures.

See [PROTOCOL.md](PROTOCOL.md).

### 4.5 Toolkit-agnostic client: `stitch-mux-client`

This crate owns protocol/state only, with no UI toolkit dependency:

- connect, handshake, attach, detach, reconnect;
- maintain `SessionView` (layout, panes, snapshots, focus, activity);
- apply `PaneSnapshot`/`PaneDelta` or terminal `Render` frames;
- expose commands (`attach`, `input`, `resize`, `set_layout`, `focus`,
  `capture`, `invoke_action`);
- track per-session/per-pane unread activity in the GUI view model;
- surface events to a frontend through a channel/waker.

It must compile without egui, GPUI, Qt, or a terminal library. This is the
boundary that lets one daemon serve multiple frontends.

### 4.6 Frontend adapters

- `stitch-mux-gui-egui`: reference GUI, using `eframe`/`egui`.
- `stitch-mux-gui-gpui`: optional performance adapter for GPUI.
- `stitch-mux-py` + Qt/PySide6 example: Python control and GUI path.
- `stitch-mux-terminal`: ANSI terminal compatibility client.

Adapters map native events to `InputEvent`, convert pane cell rects to pixels,
draw cells, render activity indicators, and expose the action palette. They do
not own terminal state or talk to PTYs.

### 4.7 Session manager

The daemon exposes named sessions independently of any attached frontend:
create, list, attach, detach, rename, kill, and client introspection. Detach is
a socket disconnect; it must never kill the session. A GUI restarting or losing
its connection reattaches and receives a fresh `Layout` plus full pane
snapshots.

## 5. Domain model

### 5.1 Identifiers

Use monotonically increasing integer newtypes, not raw `u32`/`String`:

```rust
pub struct SessionId(u64);
pub struct WindowId(u64);
pub struct PaneId(u64);
```

IDs are stable for the lifetime of the daemon. Names are user-facing and may be
renamed; internal references always use IDs. This is the lesson from Zellij's
tab ID/position distinction and rmux's typed identities.

### 5.2 Session and window

```text
Session
  id, name, created_at, last_activity
  windows: Vec<Window>
  active_window: WindowId
  panes: HashMap<PaneId, PaneRuntime>  // every live pane, placed or detached
  detached: BTreeSet<PaneId>
  clients: Vec<ClientInfo>   // id, name, view_mode, size, attached_at
  size: Size                 // canonical size, see §5.5
  key_table: KeyTable
  actions: ActionRegistry
  activity: ActivitySummary

Window
  id, name
  layout: LayoutNode
  panes: Vec<PaneId>         // membership; the runtime lives in Session.panes
  // focus is per client, not global: ClientId -> PaneId
  focus: HashMap<ClientId, PaneId>
  floating: Vec<FloatingLayer> // v0.3; empty in v0.1/v0.2

PaneRuntime
  ...
  placement: PanePlacement  // Tiled, Floating(layer), Detached
  owner: Option<ClientId>   // optional size owner; see §5.3
  status: Option<ProgramStatus>
```

The session owns every `PaneRuntime`; a window owns only membership and
placement. This is what lets a pane be detached without killing its PTY and
moved later without rebuilding the screen. It is a narrow borrow from Rex's
block lifecycle, not its block/plugin model.

v0.1 has one window per session, no floating layers, and no detached panes.
The model still allows more than one window, stored focus per client, and
detached panes so that v0.2/v0.3 do not require a data-model rewrite.

### 5.3 Pane placement and ownership

`PanePlacement` is explicit:

```rust
pub enum PanePlacement {
    Tiled,
    Floating(LayerId),
    Detached,
}
```

- **Tiled:** a leaf in a window's split tree.
- **Floating:** a leaf in a floating layer with fractional bounds and z-order
  (v0.3; out of scope for the first GUI release).
- **Detached:** not shown in any window. The pane runs, captures, and emits
  activity/exit events; it can be placed into a window later.

A detached pane is still a full session pane. It has a PTY, a screen, a
scrollback buffer, and a stable ID. Detaching is *not* closing: `DetachPane`
keeps the child alive, `ClosePane` terminates it.

For local multi-client use, each pane has an optional `owner: ClientId`. The
owner is a **size preference**, not an access-control boundary. When clients
want different terminal sizes for the same pane, the owner's size wins; if there
is no owner or the owner is disconnected, the session's canonical size applies.
Ownership is implemented only if M11 measurements show that the simpler
smallest-client policy is not enough. It is deliberately deferred from v0.1.

### 5.4 Layout tree

```rust
pub enum LayoutNode {
    Leaf(PaneId),
    Split {
        axis: SplitAxis,     // Columns = side-by-side, Rows = stacked
        ratio_bps: u16,      // 1..=9999, basis points; no NaN
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}
```

Layout is resolved to `Rect`s relative to the session's content area (the area
above the status bar). Each pane has an outer rect (border/title) and a content
rect; the content rect is the PTY size. The structured view sends both rects so
a GUI can draw its own chrome around the same content area. Minimum pane size is
configurable; splits that cannot satisfy it are rejected.

The layout model is intentionally simpler than Zellij's stacked/floating panes
and rmux's full tmux geometry. It is a binary split tree, which covers the common
tiling cases and is easy to serialize and test. Floating layers and multiple
windows are later workspace features; they extend the model rather than replace
it.

### 5.5 Canonical size and multi-client policy

The daemon has one canonical terminal size per session:

- with no clients: keep the last size;
- with clients: `min(cols)` × `min(rows)` across attached clients;
- clamp to a sensible minimum (`20 × 5`);
- on attach/detach/resize, recompute and resize every pane.

All clients share the same session size and layout. A client larger than the
canonical frame has unused space; a client smaller than the frame crops from the
top-left in v0.1. Per-client viewports/freeform geometry are later features.

The smallest-client rule is the v0.1 default because it is deterministic and
keeps every client usable. A per-pane owner (§5.3) is the fallback when local
multi-client measurements show that one client should control a pane's size. It
does not make the session multi-user; it only resolves a size preference.

The layout is **server-owned** even when a GUI requests it: a GUI may send a new
split tree or ratio, the daemon validates it, resizes PTYs, and broadcasts the
new geometry. This is what keeps GUI and terminal clients consistent. A GUI-only
freeform layout mode is future work and must be an explicit session capability,
because two clients cannot independently resize the same PTY.

This policy avoids divergent screen states per client, which would complicate
scrollback, copy mode, and input focus.

## 6. Pane runtime

### 6.1 Runtime ownership

Each pane is an actor-like task owning its screen:

```text
PaneRuntime
  session: Arc<PtySession>
  reader: PtyReader
  writer_tx: mpsc::UnboundedSender<WriteCommand>
  screen: HistoryScreen
  snapshot: Arc<PaneSnapshot>
  state: PaneState
  placement: PanePlacement
  owner: Option<ClientId>
  status: Option<ProgramStatus>
  shell: ShellState
```

`PtySession` is shared through `Arc` so the reader, writer, resize, and wait
paths observe the same child/PTY state. `HistoryScreen` is owned by the pane
task and never shared mutably across tasks. `placement` is changed by the
session, not by the reader; moving a pane never restarts or resizes the PTY
unless its new content rect differs.

Why not use `TerminalSession` directly? `TerminalSession` is the right
convenience for a single owner, but a mux needs concurrent reads, writes,
resizes, captures, and query replies. `PtySession` plus an owned
`HistoryScreen` gives that split without changing `stitch-pty`.

### 6.2 Reader task

The reader loop reads fixed-size chunks (64 KiB target), feeds them into the
screen exactly once, and publishes state changes:

```text
loop:
  n = reader.read_timeout(buf, 50ms)
  if n > 0:
      screen.feed(&buf[..n])
      publish dirty rows + metadata (coalesced)
  if n == 0 or session not alive:
      publish Exited
      break
  drain pending pane commands
```

The 50 ms timeout is a cancellation point, not a polling delay for normal
output. Pane commands (resize, capture, close, write) are handled between reads.

### 6.3 Writer and terminal replies

Writes from clients and replies generated by the terminal parser go through a
single writer task or a serialized write queue:

```text
client input ─┐
              ├─> writer queue ─> PtySession::write_all
parser reply ─┘
```

`HistoryScreen` exposes `write_process_input()` for device-attribute/status and
keyboard-mode query replies. The pane installs a closure that sends those
replies to the writer queue (using a non-blocking `try_send`; the parser must
never block). This is essential: a multiplexer must answer terminal queries on
behalf of the pane, even when no client is attached.

### 6.4 Screen snapshot

The pane publishes a `PaneSnapshot` after a dirty batch:

- visible rows as `Vec<Vec<Cell>>`, including optional OSC 8 hyperlinks;
- cursor position, visibility, shape, blink;
- title, icon name, cwd;
- foreground process metadata when known;
- modes: alternate screen, application cursor keys, bracketed paste, mouse
  protocol, SGR mouse, cursor visibility;
- Kitty keyboard flags;
- program status (`idle`/`working`/`done`/`blocked`/`error`) and message;
- shell-integration metadata (prompt/command marks, cwd);
- bell and other edge-triggered events;
- scrollback length, scrollback bytes, and absolute cursor row.

The snapshot is pane-local. The session compositor places it into the layout.
Scrollback is not copied into every snapshot; `Capture` requests fetch bounded
ranges from `HistoryScreen::styled_range`. Detached panes publish snapshots and
activity too, but have no geometry until placed.

### 6.5 Program status and activity

The daemon recognizes **OSC 7501**, Rex's generic program-status protocol. A
program reports a state (`idle`, `working`, `done`, `blocked`, `error`), an
optional kind (`permission`, `question`, `auth`), an app name, and a message.
The pane keeps the latest record as `ProgramStatus` and updates it in place.

This is state, not a notification stream:

- `PaneSnapshot`/`PaneDelta` carry the current status; the pane `seq` identifies
  the update;
- `SessionInfo.activity` summarizes working/blocked/done/error counts for the
  GUI session picker;
- the GUI derives unread/attention state locally;
- a local event stream can emit `program_status_changed` for scripts and agents;
- terminal clients can ignore the metadata or draw a simple indicator.

OSC 7501 is deliberately generic. We do not make it Superlogical-specific, and
we do not require programs to adopt it. A pane with no reports has `None`
status. Later terminal-fidelity work adds shell-integration events, OSC 9/777
desktop notifications, and OSC 9;4 progress, but those do not replace the
structured status record.

### 6.6 Scrollback and capture

Each pane has a bounded scrollback budget (target: configurable, default in the
same order as Rex's ~64 MiB, measured in cells/bytes rather than lines). The
daemon never lets a detached pane grow without bound; oldest rows are dropped
when the budget is exceeded.

`Capture` is read-only and supports:

- `cells`: structured rows, the native GUI representation;
- `text`: plain text with optional trim/unwrap;
- `json`: cells plus cursor/modes/status metadata;
- `vt`: terminal control sequences for replay/export;
- `html`: styled text for logs and reports.

The capture range is limited by the frame size and a maximum line count. Full
screen/TUI alternates have no scrollback; capture returns the visible screen
while the alternate screen is active.

### 6.7 Pane lifecycle

- `Spawn`: default shell or explicit program; environment inherited or
  explicit; initial size from layout, or a default size when detached.
- `Running`: reader/writer active.
- `Exited`: child exited; keep the final screen visible until the pane is
  closed or the session is killed.
- `Closed`: remove from layout/session, drop screen, free PTY handles.
- `Failed`: spawn/IO error; show an error pane rather than killing the session.
- `Detached`: running but not placed in a window; capturable and addressable.
- `Reattached`: placed into a window without restarting the child.

Child exit is a normal event, not a daemon error. Closing a live pane sends the
shared `stitch-pty` terminate policy with a grace period, then kills. Detaching
a pane never terminates it; the session keeps the runtime until an explicit
close or session kill.

## 7. Rendering

### 7.1 Why server-side composition

The daemon owns the screen; clients never own authoritative terminal state.
There are two projections of that state:

- **Structured (GUI, primary):** `Layout` plus per-pane `PaneSnapshot`/
  `PaneDelta` cells, cursor, modes, and title. The GUI draws panes into its own
  widgets and adds custom chrome.
- **ANSI (terminal compatibility):** the daemon composes a `Frame` and encodes
  it as ANSI for a thin terminal client.

The same PTYs and `HistoryScreen`s feed both projections. A GUI restart or a
terminal detach does not change pane state. Structured clients can request a
full snapshot after a sequence gap; terminal clients request a full ANSI frame.

The v1 structured path is not an afterthought: it is the first frontend contract
and the gate for egui/GPUI/Qt support. The ANSI path stays available for SSH,
CI, and users who prefer a terminal.

### 7.2 Frame model

```rust
pub struct Frame {
    pub size: Size,
    pub rows: Vec<Vec<Cell>>,
    pub cursor: Cursor,
    pub seq: u64,
}

pub struct Cell {
    pub text: String,      // may be a grapheme cluster
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub width: u8,         // 1 or 2; 0 for continuation cells
}
```

`Color` is typed (`Default`, `Indexed(u8)`, `Rgb`). `Attrs` is a bitset for bold,
dim, italic, underline, blink, reverse, hidden, and strikethrough. Stitch's
`Char` stores colors as strings; the adapter parses ANSI names and `#rrggbb`
into `Color`. Unknown colors fall back to `Default` rather than failing a frame.

Wide characters are copied as one cell with `width = 2`; the following
continuation cell is skipped by the encoder. This must be tested with CJK text,
emoji, and combining marks.

### 7.3 Compositor

Inputs:

- session size and layout rects;
- pane snapshots;
- active pane and focus;
- session/window names;
- status bar state (mode, key hint, clock optional).

Output: one `Frame`.

Painting order:

1. fill background;
2. paint pane borders and titles;
3. paint each pane's visible cells clipped to its inner rect;
4. paint status bar;
5. place the cursor in the active pane if visible.

The compositor is deterministic and pure; it can be unit-tested without a PTY.
This is the same separation rmux uses in `rmux-render-core` (pure data +
deterministic renderer), simplified for our scope.

### 7.4 ANSI encoding (terminal compatibility)

The terminal-view full-frame encoder:

- hide cursor;
- home and clear each row as needed;
- emit style runs (SGR) and text, resetting at row end;
- position and show the cursor last;
- optionally wrap the frame in synchronized-output markers
  (`CSI ? 2026 h/l`) when the client advertises support.

The client enters the alternate screen, enables bracketed paste and focus
reporting, and restores all of that on detach. Those terminal modes are client
state, not pane state.

Later optimization:

- compare the new frame to the previous frame and emit only changed spans;
- coalesce render ticks at ~30–60 Hz;
- keep the ANSI encoder as a compatibility projection while structured cells
  remain the primary GUI contract.

### 7.5 Known rendering limits and fidelity work

Stitch's screen model currently has limited hyperlink and underline-style
coverage, and no sixel/Kitty images. We do not inherit sixel/graphics in the
first release, and full underline conformance remains follow-up work.

Two fidelity items are in scope because they affect normal terminal use:

- **OSC 8 hyperlinks.** `Cell.link` carries the URI; the GUI decides how to
  underline and how to open it safely. Terminal clients can ignore the field or
  render a plain underline.
- **Shell integration.** OSC 133 prompt/command marks and cwd/foreground
  metadata feed prompt jumps, path display, and agent context. The daemon stores
  the marks; the GUI decides how to navigate them.

If screen fidelity becomes the bottleneck beyond these, put an emulator adapter
behind a trait and evaluate `avt` or `alacritty_terminal`; do not rewrite the
parser inside `stitch-mux`.

### 7.6 GUI and structured views

The structured view is the **primary v1 frontend contract**. A GUI must not
parse ANSI or embed a terminal emulator to get pane contents; it receives cells
and geometry directly:

```text
Terminal view:  server-composed Frame → ANSI → terminal
GUI view:       session topology + per-pane PaneSnapshot/Delta → GUI compositor
```

A structured client receives:

- session/window/pane topology and each pane's resolved outer/content `Rect`;
- per-pane visible cells, styles, cursor, modes, title, and alternate-screen
  state;
- program status, cwd, foreground process, and shell-integration marks;
- OSC 8 hyperlink URIs on cells;
- dirty-row/`PaneDelta` updates instead of a full frame each tick;
- input, focus, resize, layout, and action commands in the other direction.

The GUI draws panes, borders, tabs, status bars, overlays, animations, and
font/color themes however it wants. It also owns the activity presentation:
badges, unread state, filters, notifications, and the command palette. The
terminal content remains a cell grid; the GUI can add pixel-level chrome around
it, but cannot give a PTY a sub-cell size.

The same daemon serves both projections. A session can have a GUI client and a
terminal client attached at once; each client has its own view mode and focus,
while the layout and PTYs are shared.

#### Layout authority

There is one PTY size per pane, so two clients cannot independently choose
different pane geometries for the same live session. V1 uses **server-owned
layout**:

1. A GUI (or terminal client) sends a split-tree/ratio or pane-size request.
2. The daemon validates it, resizes the PTYs, and updates every pane's screen.
3. The daemon broadcasts the new `Layout` to structured clients and recomposes
   ANSI frames for terminal clients.

A GUI-only freeform/overlapping layout is future work and must be an explicit
session capability, because two clients cannot independently resize the same
PTY. For v1, a GUI can customize chrome, tabs, colors, and status UI without
changing pane geometry.

#### Frontend independence

The mux core and `stitch-mux-client` must not depend on egui, GPUI, Qt, or any
other toolkit. A frontend adapter:

- owns the toolkit event loop and window;
- converts pane cell rects to pixels and draws cell runs;
- maps native key/mouse/IME events to `InputEvent`;
- forwards resize/layout/focus commands;
- wakes its UI loop when `SessionView` changes.

This is what lets egui, GPUI, and PySide6 share one protocol and one session
model. See [FRONTENDS.md](FRONTENDS.md).

#### Python GUI note

A PySide6/Qt GUI should consume batched `PaneSnapshot`/`PaneDelta` events through
a PyO3 subscription, not poll `capture` at frame rate. The Qt adapter should
paint row/style runs rather than issuing one `drawText` call per cell. For 60 Hz
rendering at large sizes, a Rust-side cell renderer or a Qt `QImage`/glyph-atlas
path is preferable; Python remains a good control and chrome layer.

## 8. Attach and input

### 8.1 Attach sequence

```text
structured client                       daemon
  | ---- Hello(view=structured) --------> |
  | <--- HelloAck ----------------------- |
  | ---- Attach(session) ---------------> |
  | <--- Attached(canonical size) ------- |
  | <--- Layout(panes, focus) ----------- |
  | <--- PaneSnapshot(pane, full) ------- |
  | <--- PaneDelta(pane, rows) ---------- |
  | ---- Input / Resize / SetLayout ----> |
  | ---- Detach ------------------------> |

terminal client                         daemon
  | ---- Hello(view=terminal) ----------> |
  | <--- HelloAck ----------------------- |
  | ---- Attach(session) ---------------> |
  | <--- Resize(canonical size) --------- |
  | <--- Render(full ANSI frame) -------- |
  | <--- Render(...) -------------------- |
  | ---- Input / Resize / Detach -------> |
```

On attach, the daemon recomputes the canonical size and resizes panes. A
structured client gets a `Layout` followed by a full `PaneSnapshot` for every
pane; a terminal client gets a full `Render`. The client renders nothing from a
stale cache before that first full state.

Focus is per client. Attaching a new GUI does not steal focus from a terminal
client; each client's `Layout.focus` and `Focus` updates describe its own active
pane. Layout changes are session-wide.

### 8.2 Input model

The client sends structured input, not just bytes:

```rust
pub enum InputEvent {
    Key(KeyEvent),
    Text(String),        // IME/composed text
    Paste(String),
    Mouse(MouseEvent),
    Focus(bool),
    Bytes(Vec<u8>),      // raw fallback
}
```

Why structured: the server must decide whether a key is a mux binding or pane
input, and must encode pane input using the focused pane's current Kitty flags.
`stitch-pty` already exposes `encode_key` for that. A client that sends only raw
bytes cannot correctly implement Shift+Enter/Kitty semantics.

Key handling order:

1. If a prefix/leader is active, resolve the mux binding.
2. Otherwise, if the focused pane is in mouse-tracking mode, route mouse events.
3. Otherwise, encode the key using the pane's screen flags and write it to the
   pane.
4. Double-prefix sends the literal prefix byte.

v0.1 defaults: `Ctrl-b` prefix; `%` split columns; `"` split rows; arrow keys
focus; `x` close pane; `z` zoom; `d` detach; `?` show key help. These defaults
are entries in a minimal action table; the full configurable registry, modes,
and command palette arrive in v0.2 (M9).

### 8.3 Actions, keymaps, and command palette (v0.2)

An **action** is a named operation such as `pane.split`, `pane.focus`,
`pane.resize`, `window.new`, `pane.detach`, `session.switch`, or a user-defined
layout. Actions are the stable contract; keys, the GUI command palette, and the
CLI are all entry points.

The daemon owns the action registry and key-table resolution so that a GUI and
terminal client sharing a session resolve the same bindings. A GUI
sends key events as `Input`; the daemon checks the key table first, then encodes
unhandled keys for the pane. The GUI asks for `ListActions`/`GetKeymap` to build
its command palette and which-key hints.

Key-table features:

- a base layer plus named modes (for example `resize`), with an exclusive flag;
- multi-key sequences (`ctrl+b`, then `x`) and double-prefix passthrough;
- configurable bindings with defaults, user overrides, and source reporting;
- action arguments validated against a schema;
- a `KeymapChanged` notification after reload.

The GUI command palette lists the same actions with titles, categories, and
search keywords. The local CLI exposes `actions` and `invoke`. There is no Lua
or plugin action language in v1; user-defined actions are layout/session
operations declared in configuration and validated by the daemon.

Server ownership is deliberate for a shared local session. Per-client keymaps
and client-local actions are a later option if GUI-specific actions need them.

### 8.4 Resize

The client sends `Resize { cols, rows, pixel_width, pixel_height }`. The daemon
recomputes the canonical size, resizes the layout, calls `PtySession::resize`
for every pane, updates each `HistoryScreen`, and broadcasts a new `Layout` plus
full pane snapshots to structured clients and a full ANSI frame to terminal
clients.

Pixel dimensions are forwarded when known; `0` means unknown, matching
`stitch-pty`.

### 8.5 Detach and reconnect

Disconnecting an attach connection does not close PTYs or remove panes. The
session remains alive until:

- the last pane exits and the session is configured to close on exit;
- the user runs `kill`;
- the daemon shuts down.

On reconnect:

- a structured client gets a fresh `Layout` plus a full `PaneSnapshot` for every
  pane, then resumes `PaneDelta`s;
- a terminal client gets a full ANSI `Render`, then resumes frames;
- the client discards stale local pane buffers after any sequence/layout gap.

The daemon keeps a bounded scrollback per pane, so reconnect does not depend on
a client's local history. `ListSessions` provides the GUI session picker with
names, sizes, pane counts, attached clients, and last activity. An abrupt GUI
crash is an EOF and therefore a detach; it must not kill the session.

## 9. Daemon lifecycle

### 9.1 Endpoint discovery

Unix:

- `$XDG_RUNTIME_DIR/stitch-mux/<uid>.sock` when available;
- fallback `/tmp/stitch-mux-<uid>/default.sock`;
- directory mode `0700`, socket mode `0600`;
- `--socket PATH` overrides.

Windows:

- `\\.\pipe\stitch-mux-<user-scope-hash>`;
- per-user name plus an ACL that grants only the current user;
- `--socket` overrides with a named-pipe path.

`STITCH_MUX_SOCKET` and `STITCH_MUX_SESSION` provide script-friendly overrides.

### 9.2 Start and single-instance behavior

`ensure_daemon()`:

1. try to connect;
2. if connection fails, spawn `current_exe() server --foreground` detached
   (Unix `setsid`; Windows `DETACHED_PROCESS | CREATE_NO_WINDOW`);
3. wait for the endpoint with a bounded timeout;
4. retry the connection.

The listening socket/pipe is the single-instance lock. On Unix, remove a stale
socket only after a failed connect and only if it is owned by the current user.
Do not fork after a tokio runtime has started; detach before runtime creation or
spawn a fresh process.

### 9.3 Shutdown

- `SIGINT`/`SIGTERM` (Unix) and console-close events (Windows) request shutdown.
- `kill` requests session shutdown.
- On shutdown: stop accepting clients, terminate panes with a grace period,
  close PTYs, remove the socket, exit.
- On daemon crash, the OS closes PTYs; v0.1 does not promise child survival.
  Session restoration is explicitly out of scope.

### 9.4 Logging

`tracing` to stderr in foreground mode and to a per-user log file in daemon
mode. Log connection lifecycle, pane spawn/exit, protocol errors, and slow
clients. Never log raw terminal input or output by default.

### 9.5 Session registry and reattach guarantees

Sessions are named, client-independent records in the daemon. The GUI does not
own a session; it attaches to one. This is the core of detach/reattach:

- `EnsureSession` creates or reuses a session by name for GUI startup.
- `Attach` adds a client and never resets panes or scrollback.
- `Detach`/EOF removes only that client.
- The layout, pane screens, PTY sizes, and per-client focus history remain in
  the daemon while detached.
- `ListSessions` and `ListClients` expose enough metadata for a GUI session
  picker and a “who is attached?” view.
- `KillSession` is explicit; closing a GUI window is not a kill.

v0.1 guarantees reattach while the daemon lives. If the daemon itself crashes,
the OS closes its PTYs and child processes; live sessions cannot be recovered.
The GUI must surface that as “session lost,” not pretend a new session is the
old one. A future supervisor/session-restoration design is separate.

### 9.6 Local automation and agents

The daemon exposes a local automation surface over the same socket/pipe:

- JSON output for `ls`, `inspect`, `capture`, `run`, and `wait`;
- NDJSON event subscription for session/pane/activity/exit events;
- `run --wait` and `wait` with the child's exit status;
- stable pane/session IDs in every result;
- `send` and `send-key` for text and semantic keys;
- action invocation for layout/focus/zoom operations.

This is intentionally not a remote API. There is no HTTP listener, account
system, token exchange, or multi-user authorization. The socket permissions and
the local OS user are the trust boundary. An agent skill can teach a coding
agent the CLI, but the CLI itself is the interface: no privileged side channel.

A compact, bounded event stream is enough for agents and scripts. It is not a
replayable message bus; if a subscriber falls behind, it reconnects and fetches
the current state rather than assuming it saw every delta. See
[AUTOMATION.md](AUTOMATION.md).

## 10. Reliability and security

- **Backpressure:** each client has a bounded queue per message class. Coalesce
  `PaneDelta`s per pane and keep only the newest terminal `Render`; never drop
  input/control messages. A client that still cannot drain is disconnected with
  a typed error and must `Resync` on reconnect.
- **Bounded frames:** maximum frame size (8 MiB target) and maximum capture
  request; reject oversized payloads before allocating.
- **Activity is coalesced, not queued forever.** A pane's current status
  replaces its previous status. A slow client may miss intermediate transitions
  but must receive the latest state; activity events are never allowed to grow
  an unbounded queue.
- **Event subscriptions are bounded.** A slow automation subscriber is
  disconnected or told to resync from a fresh snapshot; it cannot block pane
  readers or the GUI.
- **Protocol errors:** close the connection with a typed error; never panic on
  malformed input.
- **Crash isolation:** a pane error must not take down the daemon; surface it as
  a failed pane.
- **Local trust:** v0.1 trusts the local user. Other local users must not be able
  to connect; verify peer credentials on Unix and enforce a per-user ACL on
  Windows.
- **No network listener** in v0.1.
- **Child environment:** inherit by default; explicit environment is passed
  through `stitch-pty`'s option contract, not rebuilt in the mux.

## 11. Decisions and alternatives

| Decision | Chosen | Why | Rejected alternative |
|---|---|---|---|
| Screen ownership | Server | Faithful reattach, multi-client, thin clients | Raw relay with client-side emulation |
| Attach rendering | Structured cells first; ANSI compatibility | GUI is the primary target; terminal still works | ANSI-only attach stream |
| Screen model | Stitch `HistoryScreen` first | No second emulator; Kitty-aware | `avt`/`alacritty_terminal` immediately |
| Layout | Binary split tree, server-owned | Simple, serializable, consistent PTY sizes | Freeform/client-owned layout in v0.1 |
| Focus | Per client, shared layout | GUI and terminal clients do not steal focus | Global focus |
| Protocol encoding | MessagePack control + typed pane cells | Evolvable DTOs, GUI-friendly | bincode-only (brittle schema), Protobuf (codegen overhead) |
| IPC | tokio Unix socket / Windows named pipe | Async endpoint behind one abstraction, already in the runtime | Separate blocking implementations; `interprocess` remains a fallback |
| Reference GUI | egui/eframe | Pure Rust, fast to iterate, MIT/Apache | GPUI/Qt from day one |
| Other GUIs | GPUI and PySide6 adapters | Same client state, different renderers | One hard-coded toolkit |
| Daemon | Custom, small | Full control over lifecycle and protocol | Embed `zellij-server`, depend on `rmux` |
| Python | PyO3 control SDK + PySide6 adapter | Python GUI with Rust daemon | Python mux server |
| Activity | OSC 7501 state in pane snapshots | Structured, generic, GUI/agent friendly | Title scraping or ad-hoc notifications |
| Pane lifetime | Session-owned; placement separate | Detached/background panes without losing state | Pane dies with its window |
| Size arbitration | Smallest client first; optional owner later | Simple and deterministic for v0.1 | Per-pane owner as the default |
| Actions | Server-owned registry + key tables | Shared bindings and command palette | Hard-coded key handling |
| Automation | Local CLI/JSON/events/agent skill | Local tools and agents, no network | Remote RPC, Lua, or web API |
| Multi-window | Deferred; model keeps WindowId | Avoid a v0.3 rewrite | Multiple windows in v0.1 |
| Floating layers | Deferred to v0.3 | GUI workspace without v0.1 scope creep | Floating panes in v0.1 |
| Terminal fidelity | OSC 8 + shell integration in scope | Normal terminal use, not graphics | Graphics protocols in v0.1 |

### Why not raw relay?

A raw relay is simpler, but reattach depends on replaying a byte ring buffer.
Long-running output truncates the buffer, parser state may start mid-sequence,
and multiple clients cannot share copy mode/search. Since stitch already has a
screen, server-side state is the better fit.

### Why structured frames first?

The primary target is a GUI, not a terminal. Structured cells let egui, GPUI, and
Qt draw pane contents directly, without parsing ANSI or embedding an emulator.
The ANSI terminal path remains a second projection over the same pane state, so
we do not lose terminal compatibility. The extra protocol work is justified
because detach/reattach and a faithful GUI are core requirements.

### Why not depend on rmux or Zellij?

`rmux` is a full mux engine, not a pluggable library for a custom PTY; adopting
it would make `stitch-pty` peripheral. `zellij-server` is published but is the
whole Zellij server (it daemonizes and expects Zellij's protocol). Both are
better as references than dependencies. See [REFERENCES.md](REFERENCES.md).

## 12. Open questions

1. Should the canonical size stay smallest-client, or should a per-pane owner
   control size in local multi-client use? v0.1 starts with smallest; measure
   before adding ownership.
2. Should focus/window selection be per client or per session? v0.1 uses
   per-client focus and one shared window.
3. Should copy mode/search run in the daemon (tmux-like) or only in a future
   native client? v0.1 provides native GUI selection and bounded capture; copy
   mode/search is post-v0.1.
4. Do we need a per-pane sequence number for delta rendering, or is full-frame
   render fast enough for local sockets? v1 structured panes use per-pane
   `seq`; measure batching and delta encoding in M2.
5. When do we add detached panes, multi-window, and floating layers? Detached
   panes are the next model change; multi-window and floating follow only after
   the placement/ownership model is stable.
6. When do we add session restoration? Only after v0.1, and only if the pane
   model can be serialized safely.
7. Which GUI toolkit is the reference implementation? egui/eframe first; GPUI
   and PySide6 adapters follow. The protocol is toolkit-agnostic.
8. How much terminal fidelity is worth carrying in the mux versus an emulator
   adapter? OSC 8 and shell integration are in scope; graphics and full
   underline conformance are not.
9. Should actions be server-owned forever, or should per-client GUI actions be
   added later? v1 keeps the registry server-side for shared bindings.
10. How much tmux compatibility is worthwhile? v0.1 intentionally has none.

## 13. First-release shape

The first usable release should support:

```sh
stitch-mux new -s demo -- bash
# open the GUI client; split, focus, type in both panes
# close the GUI window: session stays alive
stitch-mux gui -t demo
# panes and screen restored; children still running
stitch-mux ls
stitch-mux kill -t demo
```

The terminal client is also available for SSH/CI and as a compatibility check,
but the GUI attach/detach path is the primary acceptance scenario.

v0.1 is GUI core plus detach/reattach, multi-pane, local multi-client, terminal
compatibility, a second toolkit, and hardening. Program status/activity, local
automation/agents, terminal fidelity, and actions/keymaps follow in v0.2;
detached panes/ownership and multi-window/floating layers follow in v0.3.

The release ships with Linux, macOS, and Windows CI, real-PTY integration tests,
and no known data-loss path on detach. Everything else is follow-up.
