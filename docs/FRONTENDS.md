# Frontend Adapters: egui, GPUI, and Qt/PySide6

`stitch-mux` is deliberately **frontend-agnostic**. The daemon and
`stitch-mux-client` know about cells, layout, input, and sessions; they know
nothing about egui, GPUI, Qt, or any other toolkit. Each GUI is an adapter over
the same structured protocol.

This is what allows a GUI, a terminal client, and a Python/Qt tool to share one
session and one set of PTYs.

## 1. The adapter boundary

```text
stitch-mux-client (toolkit-independent)
  ├─ protocol connection and reconnect
  ├─ SessionView: layout + panes + focus + seq state
  ├─ PaneView: cells, cursor, modes, title, links, status
  ├─ commands: attach, input, resize, set_layout, focus, capture, invoke_action
  └─ events: Layout, PaneSnapshot, PaneDelta, Focus, Exit, Disconnected

frontend adapter (one per toolkit)
  ├─ window/event loop
  ├─ cell metrics and font/glyph rendering
  ├─ native input -> InputEvent
  ├─ pixel rects -> cell coordinates
  └─ repaint scheduling / accessibility / IME
```

`stitch-mux-client` must compile without any UI dependency. The reference
adapters are separate crates:

```text
crates/
  stitch-mux-client/       # protocol/state only
  stitch-mux-gui-egui/     # egui/eframe reference
  stitch-mux-gui-gpui/     # optional GPUI adapter
  stitch-mux-py/           # PyO3 client + PySide6 adapter
  stitch-mux-terminal/     # ANSI terminal compatibility
```

## 2. Shared rendering model

The server sends cell geometry, not pixels:

- pane geometry in columns/rows (`outer` and `content` rects);
- rows of `Cell { text, fg, bg, attrs, width, link }`;
- cursor, modes, title, alternate-screen state;
- cwd, foreground process, and shell-integration marks;
- program status (`idle`/`working`/`done`/`blocked`/`error`) and activity;
- dirty rows in `PaneDelta`.

Each adapter computes its own cell metrics from its font and DPI:

```rust
struct CellMetrics {
    cell_width: f32,
    cell_height: f32,
    baseline: f32,
    padding: f32,
}
```

Rules for all adapters:

1. Draw backgrounds as rectangles, then text.
2. Draw **style runs per row**, not one text call per cell.
3. Skip `Cell.width == 0` continuation cells; draw the preceding wide glyph
   once.
4. Handle combining marks as part of the cell's text, not as separate cells.
5. Clip pane content to the content rect; draw chrome in the outer rect.
6. Repaint only on `SessionView` changes, resize, focus, or input; do not spin at
   60 Hz when idle.
7. Apply `PaneDelta` only if `seq` is newer. On a sequence gap, request `Resync`
   instead of guessing.

The server does not send a font, glyph atlas, or pixel dimensions. This keeps
the protocol portable across CPU/GPU renderers and DPI settings.

### 2.1 Program status and activity

The server sends structured activity, not a stream of toast notifications:

```text
PaneSnapshot.program_status = {
  state: working | blocked | done | error | idle,
  kind: permission | question | auth,
  app, message, updated_at
}
SessionView.activity = counts for the session picker
```

Each adapter should:

- show a compact per-pane indicator (dot, spinner, or badge) and an activity
  filter/list;
- derive unread/seen state locally when the user views a pane;
- never rely on color alone; pair color with a shape or text label;
- coalesce updates and repaint at the UI frame rate, not once per status event;
- keep the terminal content readable when activity chrome is hidden.

Do not make status a blocking modal. A blocked program should be discoverable,
not interrupt typing in another pane.

### 2.2 Selection, scrollback, links, and prompt jumps

The structured view is a grid, so selection and scrolling are adapter
responsibilities:

- native mouse selection over rows, including scrollback;
- copy/paste with the platform clipboard; copy-on-selection is optional;
- select-all including scrollback where the buffer is available;
- smooth scrolling by pixels if the toolkit supports it; line scrolling is
  acceptable;
- OSC 8 links: show the URI on hover, underline according to the cell attribute,
  and ask before launching an app for custom schemes;
- prompt/command jumps use `shell_marks` and `cwd` rather than screen scraping;
- fetch older scrollback with `Capture`, bounded by the server's scrollback
  budget; do not mirror the entire history on every frame.

The GUI may keep a local selection model, but the daemon remains authoritative
for the screen and scrollback contents. Copy mode/search is still post-v0.1;
native selection is part of the GUI baseline.

### 2.3 Actions, keymaps, and command palette

Rex's client-side action model is a useful UX reference, but stitch-mux keeps
the action registry and key-table resolution in the daemon for shared local
sessions. The adapter's job is presentation:

- list actions with title, category, description, and key hints;
- provide a searchable command palette;
- show which-key hints after a prefix;
- show active key modes (e.g. resize) without blocking the terminal;
- invoke an action through the client API, not by hard-coding behavior in the
  adapter.

Key events still go to the daemon as `Input`. The daemon resolves the key table,
runs the action, or encodes the key for the pane. This keeps the GUI and terminal
client consistent. The adapter may cache `ListActions`/`GetKeymap` and refresh
on `KeymapChanged`.

### 2.4 Terminal projection

`stitch-mux-terminal` is not a cell painter. The daemon composes the session
layout, borders, status bar, styles, cursor, and dirty-cell diffs into `Render`
frames; the adapter writes those bytes to stdout in sequence. It receives
`Layout` for pane IDs and per-client focus, but never `PaneSnapshot`/
`PaneDelta`, and it never parses ANSI or owns a screen model.

Host key events become semantic `InputEvent::Key` values with Kitty-compatible
modifier bits. The focused pane's `HistoryScreen` decides the actual bytes,
using the same path as every GUI adapter. The local prefix is `Ctrl-]`: `d`
detaches, `r` requests a full `Resync`, and arrows move this client's focus.
If a watch update skips frames, the adapter requests a full frame instead of
writing a delta whose predecessor was never painted.

## 3. Toolkit comparison

| Toolkit | Version surveyed | License | Strengths | Cautions | Role |
|---|---|---|---|---|---|
| **egui / eframe** | 0.37.0 | MIT OR Apache-2.0 | Pure Rust, simple custom painting, fast iteration, cross-platform, easy `eframe` app | Immediate mode; complex-script shaping/accessibility less complete than Qt/GPUI; per-cell text calls need batching | **Reference GUI first** |
| **GPUI** | 0.2.2 | Apache-2.0 | GPU-accelerated, Zed-grade text/editor performance, retained+immediate model | Pre-1.0, API churn, sparse docs, smaller standalone ecosystem; pin exact version | Optional high-performance adapter |
| **Qt / PySide6** | 6.12.0 | LGPL-3.0 / GPL / commercial | Mature widgets, text shaping, IME, accessibility, native look, Python-friendly | Python paint path can be slow per cell; Qt packaging/licensing; not pure Rust | Python/Qt product adapter |
| **iced** | 0.14.0 | MIT | Rust, wgpu, Elm-style architecture, good custom canvas | Smaller terminal/editor ecosystem; text shaping depends on chosen renderer | Alternative Rust adapter |
| **Slint** | 1.18.1 | GPL / royalty-free / commercial | Fast native UI, good tooling | License constraints; less natural for a cell-grid terminal | Not a default |
| **ratatui** | 0.30.2 | MIT | Excellent TUI widgets | TUI, not a GUI; needs a terminal backend | Terminal client, not GUI |

### egui

Best first reference because it is pure Rust, cross-platform, permissively
licensed, and easy to paint a custom cell grid. `eframe` gives a window and event
loop without a large framework commitment.

Watch for:

- immediate-mode repaint cost at large pane counts;
- text shaping/fallback for CJK, emoji, and combining marks;
- accessibility and IME behavior compared with native Qt/GPUI;
- per-cell `Painter::text` calls. Batch by style run and row; cache galleys.

A reference egui adapter is also the fastest way to validate the structured
protocol before committing to a heavier toolkit.

### GPUI

GPUI is attractive if the GUI is expected to be a high-performance,
Zed-style workspace with many panes, smooth scrolling, and custom GPU chrome.
It is Apache-2.0 and designed for exactly this class of application.

The risk is API stability: GPUI is pre-1.0 and explicitly warns about breaking
changes. Treat it as an optional adapter behind a feature flag, pin a known
version/commit, and keep the entire dependency in `stitch-mux-gui-gpui`. Do not
let GPUI types leak into `stitch-mux-client`.

If GPUI is chosen as the primary UI, budget time for API migrations and for
platform-specific window/input testing (especially Windows and Linux).

### Qt / PySide6

Qt is the strongest choice when the product needs mature text shaping, IME,
accessibility, native dialogs, and a Python integration path. PySide6 exposes
`QPainter`, `QWidget`, `QFontMetricsF`, and signals; a custom `QWidget` can draw
the pane cell grid.

The main performance rule: **do not call `drawText` once per cell**. Group runs
by row/style and draw text runs, backgrounds as rects, and the cursor last. For
large grids or 60 Hz updates, render into a `QImage`/glyph atlas or move the hot
cell renderer to Rust and hand Qt a buffer.

Python-specific design:

- run the protocol client in a Rust worker / background thread;
- deliver batched `PaneSnapshot`/`PaneDelta` updates to the Qt thread via signals;
- keep a `SessionView` on the Qt side, but treat the daemon as authoritative;
- test with `QT_QPA_PLATFORM=offscreen` in CI;
- avoid polling `capture` at frame rate.

Licensing: PySide6 is LGPL/GPL/commercial. If the product is proprietary, use
dynamic linking and comply with LGPL requirements, or obtain a commercial Qt
license. Do not assume “Python bindings” means “no Qt licensing obligations.”

Existing Qt terminal widgets (`qtermwidget`, `termqt`) are not a good fit here:
they run their own emulator and carry GPL or separate widget assumptions. We
already have the authoritative screen in the daemon.

### iced / Slint

`iced` is a reasonable MIT-licensed Rust alternative with a wgpu renderer and a
clear architecture. It is worth a small spike if egui’s immediate-mode text path
is insufficient but GPUI’s instability is unacceptable.

Slint is fast and polished, but its licensing model is a product decision. It is
not the default for a permissively licensed library.

## 4. Event loop and threading

Every adapter uses the same shape:

```text
UI thread                         client task (tokio/background)
   │  command channel  ──────────►  connect / input / resize / set_layout
   │                                protocol decode
   │  event channel     ◄──────────  Layout / Snapshot / Delta / Focus / Exit
   │
   ├─ apply to SessionView
   ├─ request repaint / notify
   └─ draw cells
```

- UI never blocks on the socket.
- The client task owns the protocol state machine and reconnection.
- `SessionView` is the UI-side view model; the daemon remains authoritative.
- A dropped connection emits `Disconnected`; the adapter shows a session
  reconnecting state and retries with backoff.
- On reconnect, the adapter clears stale pane buffers and waits for the full
  `Layout` + `PaneSnapshot` set before drawing.

For egui, call `ctx.request_repaint()` when a new event arrives. For GPUI, call
the appropriate `cx.notify()`. For Qt, emit a signal and call `update()`.

## 5. Detach/reattach behavior

Detach/reattach is a first-class UI flow, not an error path:

- Closing a GUI window sends `Detach` (if possible) and leaves the session alive.
- A GUI crash/EOF is treated as a detach by the daemon.
- On startup, the GUI shows `ListSessions` with names, sizes, pane counts,
  attached clients, and last activity.
- Reattach receives full layout and pane snapshots; the GUI does not replay a
  local byte log.
- A dead daemon means the sessions are gone in v1; show a clear “session lost”
  state and offer a new session. Never silently recreate a session with the same
  name and imply it was restored.
- A future session-restoration feature must be opt-in and clearly distinguish
  restored layout/scrollback from restored live processes.

## 6. Input mapping details

Adapters are responsible for:

- mapping native key codes to `KeyCode`/`Modifiers`;
- preserving `text` for IME/composed input;
- key press/repeat/release where the toolkit exposes it;
- converting paste to `InputEvent::Paste` so the daemon can apply bracketed
  paste;
- converting window pixels to pane-local cell coordinates for mouse events;
- reporting focus changes;
- handling dead keys/composition without sending partial garbage;
- presenting action titles, categories, key hints, and which-key overlays;
- sending `invoke_action` for command-palette selections.

The daemon handles:

- prefix, key-sequence, and key-mode state;
- action lookup/validation and key-table resolution;
- Kitty key encoding via the focused pane’s `HistoryScreen`;
- mouse protocol encoding based on pane modes;
- focus routing per client;
- layout changes and PTY resize.

The adapter must not independently implement mux shortcuts that the daemon also
handles; duplicated key semantics inevitably diverge between GUI and terminal
clients.

## 7. Rendering quality checklist

- [ ] Wide CJK characters occupy two cells; continuation cells are skipped.
- [ ] Emoji and variation selectors render without shifting the grid.
- [ ] Combining marks stay attached to their base cell.
- [ ] Bold/dim/italic/underline/reverse/hidden/strikethrough are mapped.
- [ ] 16/256/truecolor backgrounds and foregrounds are correct.
- [ ] Cursor shape, blink, and visibility match the pane mode.
- [ ] Alternate-screen enter/exit clears the correct pane buffer.
- [ ] Selection/highlight does not corrupt cell styles.
- [ ] Scrolling only repaints affected rows.
- [ ] OSC 8 links are underlined/targetable and custom schemes ask first.
- [ ] Activity indicators are not color-only and do not steal focus.
- [ ] Blocked/done/error states are distinguishable in tabs and the session
      picker.
- [ ] Command palette actions match the daemon's action list and key hints.
- [ ] Prompt jumps use shell marks, not screen scraping.
- [ ] DPI changes recompute cell metrics and pane pixel rects.
- [ ] IME candidate window is positioned at the cursor.
- [ ] Detach restores the outer application state; reattach rebuilds panes from
      snapshots.

## 8. Recommendation

1. **Protocol first:** implement `stitch-mux-client` and the structured protocol
   before any GUI-specific code.
2. **egui reference:** build a small egui adapter to validate cells, layout,
   input, and reattach. Keep it intentionally plain.
3. **GPUI only if needed:** add it behind a feature flag when GPU performance or
   a Zed-like workspace is a real requirement. Pin versions.
4. **Program status/activity before heavy polish:** add the status fields and a
   simple activity filter/list early; they are cheap and validate the structured
   metadata path.
5. **Qt/PySide6 for Python:** expose `stitch-mux-py`, then a `QWidget` adapter
   that paints row/style runs. Use Qt for mature IME/accessibility when Python
   is the product surface.
6. **Actions and terminal fidelity next:** command palette/key hints, OSC 8,
   shell marks, selection, and scrollback. These are local UX, not remote
   features.
7. **Terminal client as compatibility:** keep the ANSI path for SSH/CI and as a
   protocol sanity check; it should not drive the GUI architecture.

The adapter boundary is the contract. If a toolkit needs a change in the
protocol, the protocol is probably wrong; if it only needs a different cell
painter, the architecture is working.

## 9. Implemented adapter status

- egui: reference structured GUI from M1–M3. M6 adds F1/status-bar diagnostics
  (endpoint, connection state, log path, session list) and a shared file log.
  M7 adds textual per-pane status badges, all activity counts in the status bar,
  a session-picker filter with a needs-attention/new toggle, and local
  unread/seen state. The adapter still consumes only `SessionView` and the
  toolkit-neutral client.
- ANSI terminal: compatibility projection from M4; server-composed frames.
- PySide6: second-toolkit proof from M5. The Rust SDK publishes batched
  `SessionView` dicts to Qt; see [PYTHON.md](PYTHON.md).
- GPUI: boundary-only spike behind the `gpui` feature; see
  [GPUI-SPIKE.md](GPUI-SPIKE.md).

The cross-adapter capability matrix and the remaining v0.2 fidelity work are
tracked in [PARITY.md](PARITY.md).
