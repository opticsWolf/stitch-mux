# stitch-mux Protocol v1

This document defines the local client/daemon protocol. v1 is **GUI-first**:
the primary attach mode carries structured pane cells and layout geometry, not
ANSI. A terminal ANSI mode remains available as a compatibility projection over
the same session state.

The protocol is **local-only and single-user**. There is no network listener,
remote authentication, multi-user authorization, SSH/Tailscale transport, or
web client transport. The local socket/pipe permissions are the trust boundary.

Program status, terminal fidelity metadata, activity, and local automation are
first-class parts of the design. Some automation and detached-pane messages are
marked **v0.2**; they use reserved kinds/capabilities and are not required for a
v0.1 GUI client.

The Rust DTOs live in `stitch-mux-protocol`; this file is the human-readable
contract.

## 1. Transport and endpoints

Local only:

- **Unix:** Unix domain socket at
  `$XDG_RUNTIME_DIR/stitch-mux/<uid>.sock`, falling back to
  `/tmp/stitch-mux-<uid>/default.sock`. Directory `0700`, socket `0600`.
- **Windows:** named pipe `\\.\pipe\stitch-mux-<user-scope-hash>`, with an ACL
  restricted to the current user.

There is no TCP listener in v1. `--socket` overrides the endpoint for tests and
advanced users.

The transport is a byte stream. One connection carries one logical conversation.
Attach connections are long-lived; control connections are short-lived.

## 2. Frame envelope

Every message is a single frame:

```text
offset  size  field
0       2     magic: 0x53 0x4D ("SM")
2       2     version: u16 little-endian (v1 = 1)
4       1     kind: u8
5       1     flags: u8 (reserved, must be 0 in v1)
6       4     payload length: u32 little-endian
10      N     payload
```

Rules:

- Maximum payload: **8 MiB** for `Render`, `PaneSnapshot`, and
  `CaptureResponse`; **1 MiB** for all other kinds. Reject larger frames before
  allocating.
- A decoder must be incremental: socket reads may split or coalesce frames.
- Unknown `kind` values are a protocol error in v1. A future version may add a
  capability-negotiated message.
- `flags` is reserved; decoders must reject non-zero flags in v1.
- Payload strings are UTF-8. Binary payloads are explicitly typed
  (`Render.ansi`).

## 3. Payload encoding

| Kind | Payload encoding |
|---|---|
| Handshake, control, layout, pane cells, capture | MessagePack (`rmp-serde`) |
| `Render` | 13-byte binary prefix (`seq` u64 LE, `cols` u16 LE, `rows` u16 LE, `full` u8) followed by raw ANSI bytes; no MessagePack wrapper |

MessagePack is used because control and pane schemas will evolve during 0.x.
Use `#[serde(default)]` for additive fields and keep enum variants append-only.
A breaking change bumps the envelope version and the handshake rejects old peers
with a clear error.

## 4. Message kinds

| Value | Name | Direction | Purpose |
|---:|---|---|---|
| 1 | `Hello` | client → server | Version, client identity, view mode, size, capabilities |
| 2 | `HelloAck` | server → client | Accepted version, server identity, capabilities |
| 3 | `Attach` | client → server | Attach to a session |
| 4 | `Attached` | server → client | Session metadata and canonical size |
| 5 | `Layout` | server → client | Pane geometry, revision, and this client's focus |
| 6 | `PaneSnapshot` | server → client | Full visible pane cells and metadata |
| 7 | `PaneDelta` | server → client | Changed pane rows/cursor/modes/title |
| 8 | `Focus` | server → client | This client's active pane changed |
| 9 | `Render` | server → client | Full ANSI frame for terminal view |
| 10 | `Input` | client → server | Key/text/paste/mouse/focus input |
| 11 | `ResizeRequest` | client → server | Client viewport size changed |
| 12 | `ResizeEvent` | server → client | Canonical session size changed |
| 13 | `FocusRequest` | client → server | Set this client's focused pane |
| 14 | `SetLayout` | client → server | Request a server-validated tiled layout |
| 15 | `Detach` | client → server | Leave the session without killing it |
| 16 | `Clipboard` | server → client | Clipboard/OSC 52 relay (reserved) |
| 17 | `Exit` | server → client | Session or pane ended |
| 18 | `Error` | both | Typed protocol/operation error |
| 19 | `Ping` | both | Liveness check |
| 20 | `Pong` | both | Liveness reply |
| 21 | `Resync` | client → server | Request a full structured or ANSI state |
| 22 | `CaptureRequest` | client → server | Fetch bounded scrollback cells |
| 23 | `CaptureResponse` | server → client | Bounded scrollback rows/export |
| 24 | `CommandRequest` | client → server | Session/CLI control operation |
| 25 | `CommandResponse` | server → client | Control result |
| 26 | `SubscribeEvents` | client → server | v0.2 local automation event subscription |
| 27 | `Event` | server → client | v0.2 automation event envelope |
| 28-31 | reserved | — | Future local-only extensions |

`view_mode` determines whether the server sends `Layout`/`PaneSnapshot`/
`PaneDelta` (structured) or `Render` (terminal). It never sends both for one
attach connection. Kinds 26-27 require the `EVENTS` capability; a v1 peer that
does not advertise it treats them as unknown kinds. Kinds 28-31 are reserved
and must not be sent without a future capability negotiation.

## 5. Handshake

### 5.1 `Hello`

```rust
enum ViewMode { Structured, Terminal }

struct Hello {
    protocol_version: u16,       // must equal the envelope version
    client_name: String,         // "stitch-mux-gui-egui 0.1.0"
    client_instance: String,     // stable per GUI/terminal process; for logs
    view_mode: ViewMode,
    cols: u16,
    rows: u16,
    pixel_width: u16,            // 0 = unknown
    pixel_height: u16,           // 0 = unknown
    capabilities: u64,           // bitmask, see §10
}
```

The server checks `protocol_version` and `view_mode` before decoding further
messages. A mismatch returns `Error { code: ProtocolVersion }` or
`Error { code: Unsupported }` and closes the connection.

### 5.2 `HelloAck`

```rust
struct HelloAck {
    protocol_version: u16,
    server_version: String,
    capabilities: u64,
    sessions: u32,               // convenience for a GUI session picker
}
```

The handshake does not attach. A client may open a control connection, perform
several `CommandRequest`s, and close without ever attaching.

## 6. Structured view (primary)

### 6.1 `Attach`

```rust
struct Attach {
    session_name: String,
    takeover: bool,              // v1: false; reserved for forced attach
}
```

If the session does not exist, the server returns
`Error { code: NotFound }`. `takeover` is reserved; v1 rejects `true` as
unsupported.

### 6.2 `Attached`

```rust
struct Attached {
    session_id: u64,
    session_name: String,
    view_mode: ViewMode,
    cols: u16,                   // canonical session size
    rows: u16,
    status_rows: u16,            // rows reserved for status/chrome
    attached_clients: u32,
    layout_revision: u64,
    activity: ActivitySummary,
}
```

The daemon recomputes the canonical size before sending `Attached`. For a
structured client, the next messages are:

1. one `Layout`;
2. one full `PaneSnapshot` for every pane in the layout.

No stale pane content is rendered before that first full state.

### 6.3 `Layout` and `PaneGeometry`

```rust
struct Layout {
    session_id: u64,
    revision: u64,               // monotonic per session
    cols: u16,
    rows: u16,
    status_rows: u16,
    focus: Option<PaneId>,       // focus for THIS client
    root: LayoutNode,            // server-owned binary split tree
    panes: Vec<PaneGeometry>,
}

struct PaneGeometry {
    pane_id: u64,
    outer: Rect,                 // border/title area
    content: Rect,               // PTY cell area
    title: String,
}

struct Rect {
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
}
```

`content` is the PTY size. `outer` is what a GUI can use for borders, padding,
and hit testing. The terminal compositor uses `outer` and draws the border/title
inside it. The GUI may draw its own chrome within `outer`.

`root` is the authoritative binary split tree. Clients may request a new tree
with `SetLayout`, but only the daemon applies it; `revision` increments on every
accepted change. `focus` is **per client**. Two attached clients can have
different active panes while sharing the same layout.

### 6.4 `PaneSnapshot`

```rust
struct PaneSnapshot {
    pane_id: u64,
    seq: u64,                    // monotonic per pane
    cols: u16,
    rows: u16,
    cells: Vec<Row>,             // exactly rows entries
    cursor: Cursor,
    modes: Modes,
    title: String,
    cwd: Option<String>,
    foreground: Option<ProcessInfo>,
    shell_marks: Option<ShellMarks>,
    program_status: Option<ProgramStatus>,
    alt_screen: bool,
    scrollback_len: u64,
    scrollback_bytes: u64,
}

struct Row {
    cells: Vec<Cell>,            // exactly cols cells, including continuations
    wrapped: bool,
}

struct Cell {
    text: String,                // grapheme cluster or single cell
    fg: Color,
    bg: Color,
    attrs: u32,                  // Attrs bitset
    width: u8,                   // 1 = normal, 2 = wide, 0 = continuation
    link: Option<String>,        // OSC 8 URI; optional in v1
}

struct ProcessInfo {
    pid: Option<u32>,
    name: String,
    cwd: Option<String>,
}

struct ShellMarks {
    prompt_rows: Vec<u64>,       // absolute scrollback rows
    command_rows: Vec<u64>,
}

enum ProgramState { Idle, Working, Done, Blocked, Error }
enum ProgramStatusKind { Permission, Question, Auth }

struct ProgramStatus {
    id: Option<String>,          // OSC 7501 record id, when present
    state: ProgramState,
    kind: Option<ProgramStatusKind>,
    app: Option<String>,
    message: Option<String>,
    updated_at_unix_ms: u64,
}

// Wire values are lowercase: idle, working, done, blocked, error;
// permission, question, auth.

struct ActivitySummary {
    idle: u32,
    working: u32,
    blocked: u32,
    done: u32,
    error: u32,
}

enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

struct Cursor {
    x: u16,
    y: u16,
    visible: bool,
    shape: CursorShape,          // Block, Bar, Underline
    blink: bool,
}

struct Modes {
    alt_screen: bool,
    app_cursor: bool,
    bracketed_paste: bool,
    mouse_proto: u16,            // 0, 9, 1000, 1002, 1003...
    sgr_mouse: bool,
    cursor_visible: bool,
    keyboard_flags: u16,         // Kitty flags
}
```

`Cell.width == 0` is a continuation cell for the preceding wide character; a
renderer must skip it. The server still counts it as a grid column so pane
geometry and PTY sizes agree.

### 6.5 `PaneDelta`

```rust
struct PaneDelta {
    pane_id: u64,
    seq: u64,                    // must be > the last applied seq
    rows: Vec<RowUpdate>,        // changed rows only
    cursor: Cursor,
    modes: Modes,
    title: String,
    cwd: Option<String>,
    foreground: Option<ProcessInfo>,
    shell_marks: Option<ShellMarks>,
    program_status: Option<ProgramStatus>,
    alt_screen: bool,
    scrollback_len: u64,
    scrollback_bytes: u64,
    bell: bool,                  // edge-triggered
}

struct RowUpdate {
    row: u16,                    // 0-based visible row
    cells: Vec<Cell>,
    wrapped: bool,
}
```

A delta may contain no rows when only the cursor, title, bell, modes,
activity, or shell metadata changed. Clients must reset their pane buffer on an
alternate-screen transition; the server sends a full `PaneSnapshot` in that case
rather than a partial delta.

`program_status` is the **current** status, not an event queue. A later value
replaces an earlier one. Activity aggregation is derived from the current pane
statuses; the GUI keeps unread/seen state locally. A pane with no OSC 7501
report has `None` status.

`scrollback_bytes` lets a client show memory pressure and lets the daemon enforce
a bounded scrollback budget. `shell_marks` are absolute scrollback rows; a GUI
can use them for prompt/command navigation without reparsing the screen.

### 6.6 `FocusRequest` / `Focus`

```rust
struct FocusRequest {
    pane_id: u64,
}

struct Focus {
    pane_id: u64,
}
```

The client sends `FocusRequest` when a GUI pane is clicked or a terminal key
binding moves focus. The server validates the pane, updates **this client's**
focus, and returns `Focus`. `Layout` is sent only if geometry changed. Focus is
never global: attaching a second GUI does not steal the first client's focus.

`InputEvent::Focus(bool)` in §6.7 means the outer terminal/window gained or lost
OS focus; it is not pane focus.

### 6.7 `Input`

```rust
enum InputEvent {
    Key(KeyEvent),
    Text(String),
    Paste(String),
    Mouse(MouseEvent),
    Focus(bool),
    Bytes(Vec<u8>),
}

struct KeyEvent {
    code: KeyCode,
    modifiers: u16,
    kind: KeyEventKind,
    text: Option<String>,        // composed/IME text when available
}

enum KeyCode {
    Char(char),
    Enter, Tab, Backspace, Esc,
    Up, Down, Left, Right,
    Home, End, PageUp, PageDown, Insert, Delete,
    F(u8),
    Null,
}

enum KeyEventKind { Press, Repeat, Release }

struct MouseEvent {
    x: u16,                      // pane-local cell coordinates
    y: u16,
    kind: MouseKind,             // Down, Up, Drag, Move, ScrollUp, ScrollDown
    button: u8,                  // 0 = left, 1 = middle, 2 = right
    modifiers: u16,
}
```

Modifier bits:

| Bit | Modifier |
|---:|---|
| 0 | Shift |
| 1 | Control |
| 2 | Alt |
| 3 | Super |
| 4 | Hyper |
| 5 | Meta |
| 6 | Caps Lock |
| 7 | Num Lock |

The server maps `KeyEvent` to `stitch-pty`'s `KeyEvent` and calls the focused
pane's `encode_key` with its current Kitty flags. `Text`/`Paste` are written
directly (with bracketed-paste wrapping when the pane has enabled it). `Bytes`
is a fallback for frontends that cannot provide structured events.

Mouse coordinates are pane-local; the GUI adapter converts window pixels to the
pane's cell grid before sending. The server encodes SGR/legacy mouse reports
based on the focused pane's modes.

### 6.8 `ResizeRequest` / `ResizeEvent`

```rust
struct ResizeRequest {
    cols: u16,                   // outer UI viewport, in cells
    rows: u16,
    pixel_width: u16,            // 0 = unknown
    pixel_height: u16,           // 0 = unknown
}

struct ResizeEvent {
    cols: u16,                   // canonical session size after recomputation
    rows: u16,
}
```

The server recomputes the canonical size, re-resolves the layout, resizes every
PTY, and updates every `HistoryScreen`. A structured client receives a new
`Layout` and full `PaneSnapshot`s; a terminal client receives a full `Render`.
Clients larger than the canonical size leave unused space; smaller clients crop
from the top-left in v1.

### 6.9 `SetLayout`

```rust
struct SetLayout {
    base_revision: u64,
    root: LayoutNode,
}

enum LayoutNode {
    Leaf { pane_id: u64 },
    Split {
        axis: SplitAxis,         // Columns = side-by-side, Rows = stacked
        ratio_bps: u16,          // 1..=9999 basis points
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

enum SplitAxis { Columns, Rows }
```

The daemon validates:

- every pane id exists exactly once;
- no unknown panes;
- every content rect meets the minimum size;
- `base_revision` matches the current layout revision (otherwise return
  `Error { code: Busy }` and a fresh `Layout`).

The server remains the layout authority. A GUI may request a layout, but the
daemon resolves it, resizes PTYs, increments `revision`, and broadcasts the
result. Terminal clients attached to the same session see the same layout,
recomposed as ANSI.

Freeform/overlapping geometry is not representable in v1. It will require an
explicit session capability and a different layout model.

### 6.10 `Detach`

```rust
struct Detach {
    reason: String,              // "user", "window_closed", "eof", "error"
}
```

The server removes the client but leaves all panes, PTYs, layout, focus history,
and scrollback intact. A socket EOF is equivalent to a detach. A GUI closing its
window must send `Detach` when possible; the daemon also handles abrupt
disconnects.

### 6.11 `Resync`

```rust
struct Resync {
    reason: String,              // "seq_gap", "reconnect", "layout_gap"
}
```

For a structured client, the server replies with a fresh `Layout` and full
`PaneSnapshot`s. For a terminal client, it replies with a full `Render`. A client
that detects a `PaneDelta.seq` or `Layout.revision` gap must send `Resync` rather
than guess.

### 6.12 `Clipboard`, `Exit`, `Error`

```rust
struct Clipboard {
    pane_id: u64,
    selection: ClipboardSelection,
    text: Option<String>,
}

struct Exit {
    session_id: u64,
    pane_id: Option<u64>,
    reason: String,
}

struct Error {
    code: ErrorCode,
    message: String,
}

enum ErrorCode {
    ProtocolVersion, BadFrame, UnknownKind, TooLarge,
    NotFound, Unsupported, Busy, Internal,
}
```

`Clipboard` is reserved in v1. `Exit` means a pane or the session ended; the
client may stay connected until the daemon closes the stream. `Error` may be
sent before a close.

### 6.13 `CaptureRequest` / `CaptureResponse`

Kinds 22 and 23 carry the same fields as `Command::Capture` and
`CaptureResponse`:

```rust
struct CaptureRequest {
    pane_id: u64,
    start: u64,
    lines: u32,
    format: CaptureFormat,
}
```

The control-command path is preferred for one-shot CLI use. A dedicated capture
frame is useful for a frontend that wants to fetch scrollback without opening a
separate control connection. Both paths enforce the same line/frame limits.

## 7. Terminal ANSI view (compatibility)

A client that requests `ViewMode::Terminal` receives:

```rust
struct Render {
    seq: u64,                    // monotonic per session
    cols: u16,
    rows: u16,
    full: bool,                  // true in v1
    ansi: Vec<u8>,               // raw bytes for stdout
}
```

On the wire, `Render` is not MessagePack: it is the 13-byte binary prefix
followed by raw ANSI bytes shown above. The struct is the decoded form used by
`stitch-mux-client`. The server composes the session layout, borders, status
bar, and focused cursor, then encodes ANSI. The terminal client enters the
alternate screen, enables bracketed paste/focus reporting, writes frames,
forwards input/resize, and restores the terminal on detach. It does not parse
pane output.

The terminal view is a compatibility projection, not a separate session model.
Both views share PTYs, screens, layout, and per-client focus. The server may
coalesce terminal frames; a client that falls behind sends `Resync` for the
latest full frame.

## 8. Control commands and session management

Control commands use their own short-lived connection. The client sends
`Hello`, receives `HelloAck`, then sends one or more `CommandRequest` frames.

```rust
enum Command {
    NewSession {
        name: String,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>, // None = inherit, Some([]) = empty
        cols: u16,
        rows: u16,
    },
    EnsureSession {
        name: String,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        cols: u16,
        rows: u16,
    },
    ListSessions,
    ListClients { session: String },
    ListPanes { session: String },
    KillSession { name: String },
    RenameSession { name: String, new_name: String },
    DetachClient { client_id: u64 },
    SplitPane { pane_id: u64, axis: SplitAxis, ratio_bps: u16 },
    ClosePane { pane_id: u64 },
    ZoomPane { session: String, pane_id: Option<u64> }, // None unzooms
    ResizeSession { session: String, cols: u16, rows: u16 },
    SendText { pane_id: u64, text: String },
    SendKeys { pane_id: u64, bytes: Vec<u8> },
    Run {
        session: String,
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        placement: RunPlacement,
        label: Option<String>,
        wait: bool,
        keep_open: bool,
    },
    Wait {
        target: WaitTarget,
        timeout_ms: Option<u64>,
    },
    Capture {
        pane_id: u64,
        start: u64,              // absolute row; 0 = oldest retained
        lines: u32,
        format: CaptureFormat,
    },
    DetachPane { pane_id: u64 },       // v0.2
    PlacePane {                       // v0.2
        pane_id: u64,
        window_id: Option<u64>,       // None = active window
        anchor_pane_id: Option<u64>,
        axis: SplitAxis,
        ratio_bps: u16,
    },
    ClaimPane { pane_id: u64, mode: ClaimMode }, // v0.2
    ReleasePane { pane_id: u64 },                // v0.2
    ListActions,
    GetKeymap,
    InvokeAction { action: String, args: Option<ActionArgs> },
    Shutdown,
}

enum RunPlacement {
    NewWindow,                   // v0.3; v0.2 uses the active window
    Split { pane_id: u64, axis: SplitAxis, ratio_bps: u16 },
}

enum WaitTarget {
    Session { name: String },
    Pane { pane_id: u64 },
}

enum ClaimMode { Take, Request }

enum CaptureFormat { Cells, Text, Json, Vt, Html }

struct ActionArgs {
    // MessagePack value validated against the action schema.
}

struct CommandResponse {
    ok: bool,
    error: Option<Error>,
    sessions: Vec<SessionInfo>,
    clients: Vec<ClientInfo>,
    panes: Vec<PaneInfo>,
    capture: Option<CaptureResponse>,
    layout: Option<Layout>,      // fresh layout when a SetLayout base was stale
    run: Option<RunResult>,
    wait: Option<WaitResult>,
    actions: Vec<ActionInfo>,
    keymap: Option<Keymap>,
}

struct RunResult {
    pane_id: u64,
    window_id: u64,
    exit_code: Option<i32>,
    timed_out: bool,
}

struct WaitResult {
    target: WaitTarget,
    exit_code: Option<i32>,
    timed_out: bool,
}

struct ActionInfo {
    name: String,
    title: String,
    description: Option<String>,
    category: Option<String>,
    args_schema: Option<String>, // JSON Schema serialized as a string
}

struct Keymap {
    bindings: Vec<KeyBinding>,
    modes: Vec<KeyMode>,
}

struct KeyBinding {
    keys: Vec<String>,           // e.g. ["ctrl+b", "x"]
    action: String,
    args: Option<ActionArgs>,
    source: String,              // defaults, config, override
}

struct KeyMode {
    name: String,
    exclusive: bool,
}

struct SessionInfo {
    id: u64,
    name: String,
    cols: u16,
    rows: u16,
    pane_count: u32,
    detached_pane_count: u32,
    window_count: u32,
    attached_clients: u32,
    created_at_unix_ms: u64,
    last_activity_unix_ms: u64,
    activity: ActivitySummary,
    panes: Vec<PaneInfo>,
}

struct PaneInfo {
    id: u64,
    title: String,
    cwd: Option<String>,
    alive: bool,
    placement: PanePlacement,
    window_id: Option<u64>,
    owner_client_id: Option<u64>,
    program_status: Option<ProgramStatus>,
}

enum PanePlacement {
    Tiled,
    Floating { layer_id: u64 },      // v0.3
    Detached,                        // v0.2
}

struct ClientInfo {
    id: u64,
    name: String,
    view_mode: ViewMode,
    cols: u16,
    rows: u16,
    attached_at_unix_ms: u64,
}
```

`EnsureSession` is the GUI startup primitive: reuse by name or create it. `NewSession`
is explicit and fails if the name exists. `ListSessions` powers the GUI session
picker and must include enough metadata to show a useful recent-session list
without attaching, including activity counts and detached panes.

`Run`, `Wait`, `SendText`, `ListPanes`, and the action methods are the v0.2
automation/agent surface. They use the same local connection and trust model as
the GUI; there is no remote variant. `DetachPane`, `PlacePane`, `ClaimPane`, and
`ReleasePane` are reserved for the v0.2 detached-pane/ownership model. A v1
server that does not advertise the relevant capability returns
`Error { code: Unsupported }` for these variants.

`CaptureResponse` is bounded by `lines <= 1000` and the 8 MiB frame limit.
`format` selects the payload; unused payload fields are empty:

```rust
struct CaptureResponse {
    pane_id: u64,
    start: u64,
    total_lines: u64,
    format: CaptureFormat,
    rows: Vec<CapturedRow>,      // Cells / Json
    text: Option<String>,        // Text
    vt: Option<Vec<u8>>,         // Vt
    html: Option<String>,        // Html
}

struct CapturedRow {
    cells: Vec<Cell>,
    wrapped: bool,
}
```

Capture is read-only and does not change scrollback or focus. `Text` may trim or
unwrap; `Json` includes cells plus cursor/modes/status metadata; `Vt` preserves
control sequences for replay; `Html` preserves styled text for logs and reports.

### 8.1 Session lifecycle

- A session exists independently of clients.
- `Detach`/EOF removes a client; it never kills panes.
- A GUI restart lists sessions and reattaches with a full snapshot.
- `KillSession` terminates panes with the shared `stitch-pty` grace policy.
- Closing the last pane may end the session according to session policy.
- The daemon keeps detached sessions alive until explicitly killed.
- v1 does not restore sessions after a daemon crash; live PTYs cannot be
  recovered from a dead daemon without a separate supervisor. The GUI must show
  a clear “session lost” state rather than silently recreating it.

### 8.2 Detached panes and ownership (v0.2)

`DetachPane` removes a pane from its window without closing the PTY. The pane
keeps its ID, screen, scrollback, activity, and child process. It appears in
`PaneInfo` with `placement = Detached` and can be captured, waited on, or placed
later with `PlacePane`.

`PlacePane` inserts the pane into a window at an anchor. The daemon validates
the resulting layout and resizes the PTY only if the content rect changes.
Detaching the last tiled pane leaves an empty window; closing the pane is the
explicit destructive operation.

`ClaimPane`/`ReleasePane` implement the optional size-owner policy. `Take` makes
the caller the owner immediately; `Request` only claims unowned panes and waits
in line otherwise. Ownership changes emit a `PaneOwnerChanged` event in the v0.2
event stream. Ownership is a local size preference, not authentication.

### 8.3 Automation and agent commands (v0.2)

The local CLI and agent skill use these control commands:

- `Run` creates a pane in a split or the active window, optionally waits for the
  child, and returns the child's exit status. New-window placement is v0.3.
- `Wait` waits for a pane or session to exit, with an optional timeout.
- `SendText`/`SendKeys` write text or semantic keys to a pane.
- `Capture` returns cells, text, JSON metadata, VT sequences, or HTML.
- `ListActions`/`GetKeymap` expose the action registry and effective bindings.
- `InvokeAction` runs a named layout/focus/session action with validated
  arguments.

`SubscribeEvents` (kind 26) starts an NDJSON-style local event stream. The
server sends `Event` (kind 27) frames with a monotonic `seq` and a typed payload:
session/window/pane created or closed, pane exit, program-status change,
activity summary change, layout change, and owner change. Events are bounded and
coalesced; a subscriber that falls behind is disconnected or told to resync
from a fresh `ListSessions`/`Capture`. There is no replay log.

The event stream is local-only. It is intended for scripts and coding agents,
not for a remote control plane or a multi-user audit system.

```rust
struct SubscribeEvents {
    session: Option<String>,     // None = all sessions
    all: bool,                   // include client/keymap/host events
    since_seq: Option<u64>,      // None = current state only
}

struct Event {
    seq: u64,                    // monotonic per subscription
    session_id: Option<u64>,
    kind: String,                // pane_exited, program_status_changed, ...
    payload: ActionArgs,         // typed by kind; MessagePack value
}
```

`SubscribeEvents` (kind 26) and `Event` (kind 27) require the `EVENTS`
capability. `since_seq` is a best-effort catch-up hint, not a replay guarantee;
the server may disconnect a subscriber that has fallen outside its bounded
buffer. Clients should refresh state after any gap.

## 9. Error and backpressure semantics

- Protocol errors close the connection after sending `Error` when possible.
- A slow structured client: coalesce `PaneDelta`s per pane, keep at most a
  bounded number of pending updates per pane, and drop intermediate rows while
  preserving the latest cursor/modes/title. If the client still cannot drain,
  close with `Error { code: Busy }`.
- A slow terminal client: keep only the newest ANSI frame.
- Input and control messages are never silently dropped.
- A serialization/backpressure gap must be recoverable with `Resync`; a client
  must not assume deltas are complete after a dropped frame.
- A daemon-side pane error becomes a failed pane (`Exit`/status), not a daemon
  panic.
- **Activity is coalesced.** Current `program_status` replaces the previous
  value; a slow client may miss intermediate transitions but must receive the
  latest state. Activity never grows an unbounded queue.
- **Event subscribers are bounded.** A slow `SubscribeEvents` connection is
  disconnected or told to resync; it cannot block pane readers or the GUI.
- Bounded queues and frame sizes are mandatory; never allocate based on an
  untrusted length before checking the maximum.

## 10. Capabilities

Client capabilities (requested):

| Bit | Name | Meaning |
|---:|---|---|
| 0 | `STRUCTURED_VIEW` | Can consume `Layout`/`PaneSnapshot`/`PaneDelta` |
| 1 | `ANSI` | Can consume composed ANSI `Render` frames |
| 2 | `TRUECOLOR` | Renderer supports 24-bit color |
| 3 | `SYNC_OUTPUT` | Terminal supports synchronized output |
| 4 | `KITTY_KEYS` | Can send structured Kitty-aware key events |
| 5 | `MOUSE` | Can send mouse events |
| 6 | `FOCUS` | Can report focus changes |
| 7 | `BRACKETED_PASTE` | Can send paste events |
| 8 | `LAYOUT_CONTROL` | May send `SetLayout` |
| 9 | `CAPTURE` | Supports `CaptureRequest` |
| 10 | `RESYNC` | Supports `Resync` |
| 11 | `PROGRAM_STATUS` | Can display OSC 7501 status/activity |
| 12 | `SHELL_INTEGRATION` | Can display cwd/foreground/prompt marks |
| 13 | `HYPERLINKS` | Can render OSC 8 link metadata |
| 14 | `EVENTS` | Can consume the v0.2 event stream |
| 15 | `ACTIONS` | Can list/invoke named actions |

Server capabilities (advertised):

| Bit | Name | Meaning |
|---:|---|---|
| 0 | `STRUCTURED_VIEW` | Structured attach stream |
| 1 | `ANSI` | ANSI terminal attach stream |
| 2 | `TRUECOLOR` | Can emit 24-bit cell colors |
| 3 | `STRUCTURED_DELTA` | Supports `PaneDelta` (v1 yes) |
| 4 | `CAPTURE` | Supports `CaptureRequest` |
| 5 | `RESYNC` | Supports `Resync` |
| 6 | `LAYOUT_CONTROL` | Accepts `SetLayout` |
| 7 | `PROGRAM_STATUS` | Emits OSC 7501 activity in pane state |
| 8 | `SHELL_INTEGRATION` | Emits cwd/foreground/prompt marks |
| 9 | `HYPERLINKS` | Emits OSC 8 link metadata |
| 10 | `EVENTS` | Supports `SubscribeEvents`/`Event` |
| 11 | `ACTIONS` | Supports `ListActions`/`InvokeAction` |
| 12 | `DETACHED_PANES` | Supports detach/place/claim (v0.2) |

A structured client must request and be granted `STRUCTURED_VIEW`; a terminal
client must request `ANSI`. `SetLayout` requires `LAYOUT_CONTROL`; the daemon
still validates and resolves every request. A client that does not advertise
`PROGRAM_STATUS`, `SHELL_INTEGRATION`, or `HYPERLINKS` may still receive the
optional fields, but must ignore what it cannot render.

## 11. Versioning policy

- The envelope version and `Hello.protocol_version` must match exactly in 0.x.
- Adding an optional field to a MessagePack struct is compatible.
- Adding a new `kind` is compatible only when both peers advertise the relevant
  capability; otherwise it is an unknown kind. Kinds 26-27 are the v0.2
  `EVENTS` extension; 28-31 remain reserved.
- Changing an existing message's semantics or required fields requires a
  version bump.
- Keep golden encoded fixtures for every message so accidental wire changes are
  caught in CI.
- The v1 schema is frozen at the start of M1; v0.2 additions are
  capability-gated and follow this policy rather than ad-hoc edits.

## 12. Frontend contract

A frontend adapter consumes `SessionView` from `stitch-mux-client`:

- `Layout` creates/updates pane widgets and their pixel rects;
- `PaneSnapshot` replaces a pane buffer;
- `PaneDelta` applies changed rows and metadata if `seq` is newer;
- `Focus` updates this client's active pane;
- `program_status`/`activity` drive badges, filters, and unread state; they are
  state, not transient notifications;
- `shell_marks`/`foreground`/`cwd` drive prompt jumps, path display, and
  context without reparsing the screen;
- `Cell.link` is optional OSC 8 metadata; the adapter applies safe link
  handling;
- `Bell`/title changes trigger UI state, not pane resets;
- a missing sequence triggers `Resync`.

The adapter owns:

- window/event loop and repaint scheduling;
- font metrics, glyph atlas, text shaping, colors, and chrome;
- native selection, scrollback presentation, and copy/paste;
- command palette, action titles/categories, and which-key hints;
- mapping native key/mouse/IME events to `InputEvent`;
- converting a GUI viewport to `ResizeRequest` and pane pixel geometry to
  `SetLayout`/mouse cell coordinates.

The adapter does not own terminal state, PTYs, or scrollback. A GUI can be
restarted at any time and reconstruct its view from the daemon.
