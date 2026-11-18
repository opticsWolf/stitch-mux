# Reference Evaluation

Survey date: **2026-11-15**. Versions observed during the survey:

| Project | Version / source |
|---|---|
| `stitch-pty` | 0.15.0, local `../stitch-pty` |
| rmux | 0.10.0 |
| Zellij | 0.46.0 |
| WezTerm | workspace `main`, surveyed 2026-11-15 |
| Rex | public beta docs, surveyed 2026-11-15 |

These projects are architectural references. We do not vendor or depend on their
mux engines. rmux, Zellij, and WezTerm are open-source projects with licenses
compatible with a MIT OR Apache-2.0 project. Rex is a beta product used only as a
product/API/UX reference; no Rex code is copied. The design notes below are
about structure, not code copying.

## 1. rmux

Repository: <https://github.com/Helvesec/rmux>
Architecture notes: <https://github.com/Helvesec/rmux/blob/main/docs/ARCHITECTURE.md>

### Architecture

rmux is the closest existing engine to what we want:

```text
CLI ── local IPC ── daemon
                      ├── sessions / windows / panes / layouts
                      ├── rmux-pty (Unix PTY, Windows ConPTY)
                      ├── rmux-core (screen, input, formats, hooks)
                      ├── rmux-server (tokio request dispatcher, renderer)
                      ├── rmux-proto (versioned bincode RPC + attach stream)
                      ├── rmux-ipc (Unix socket / Windows named pipe)
                      └── rmux-sdk (typed daemon-backed Rust SDK)

rmux-render-core ── pure pane snapshot + ratatui projection (WASM-capable)
ratatui-rmux     ── ratatui widget over snapshots
librmux          ── Python SDK that drives the rmux executable
```

The daemon is the authority for terminal state. The CLI attaches, and the server
renders the composed screen as ANSI (`renderer.rs` returns `Vec<u8>`). Pane
snapshots are also exposed to the SDK and the ratatui widget.

### What is worth borrowing

- **Crate seams.** PTY, OS boundary, protocol, server, client, SDK, and render
  core are separate crates. This keeps platform code and protocol code testable
  in isolation. Our planned `core` / `protocol` / `server` / `client` split is a
  smaller version of the same idea.
- **Versioned envelope.** rmux uses a magic byte, wire-version varint, and a
  length-prefixed payload. Our v1 uses a fixed header, but the same principles:
  explicit magic, version, bounded length, incremental decoder.
- **Typed identities.** `SessionId`, `WindowId`, and `PaneId` are stable types,
  separate from user-facing names.
- **Capabilities and attach stream.** The attach path has its own data frames,
  not a generic RPC stream. Resize, keystrokes, and render data have explicit
  tags.
- **Snapshots plus rendering split.** Snapshots are pure data; the ratatui
  widget is a deterministic projection. This is the right shape for our future
  Python/native/web clients.
- **Per-pane screen facade.** `TerminalScreen` exposes `feed`, `resize`,
  `take_replies`, `pending_bytes`, and recovery state while keeping the parser
  private. Our pane runtime should similarly own the screen and publish a
  compact snapshot rather than exposing parser internals.
- **Local IPC abstraction.** Unix sockets and Windows named pipes are behind one
  endpoint abstraction. We will use `interprocess` rather than reinvent it.

### What not to borrow

- **Do not depend on `rmux-core`/`rmux-server`.** The server directly uses
  `rmux-pty`; there is no public “bring your own PTY” backend. Replacing it with
  `stitch-pty` would be a fork of rmux, not an integration.
- **Do not adopt the full tmux command surface.** rmux implements 90+ commands,
  formats, hooks, options, control mode, and web sharing. That is a different
  product scope.
- **Do not copy the private-API crates.** `rmux-core`, `rmux-client`, and
  `rmux-pty` explicitly say their semver is not stable. They are published for
  workspace consumers, not as a stable platform.
- **Do not assume the Python SDK is a native binding.** `librmux` drives the
  `rmux` executable; the survey found it targeting rmux 0.6.1 while the core was
  0.10.0. This is a control SDK, not a PyO3 embedding layer.

## 2. Zellij

Repository: <https://github.com/zellij-org/zellij>
Published pieces: `zellij-server`, `zellij-client`, `zellij-utils`

### Architecture

Zellij uses a client/server model with an explicit thread-and-channel bus:

```text
client
  ├── stdin handler / input handler / keyboard parser
  ├── terminal setup and restore
  └── IPC (protobuf client/server contract)

server
  ├── route thread
  ├── PTY thread       (spawn, resize, close, signal)
  ├── screen thread    (tabs, panes, layout, render)
  ├── plugin thread    (WASM plugin host)
  ├── background jobs
  └── server instruction loop

zellij-utils
  ├── input actions, keybinds, layouts, options
  ├── IPC/protobuf types
  ├── pane geometry and sizes
  └── session metadata/serialization
```

The server owns pane grids, tabs, layouts, focus, input modes, and rendering.
`TiledPanes` and `PaneGeom` handle splitting, resizing, floating panes, stacked
panes, and hit testing. The client has its own stdin parsing, input handler, and
terminal setup/teardown. Zellij also has a plugin host, web client, session
serialization, and resurrection; those are outside our v1 scope.

### What is worth borrowing

- **Explicit server/client responsibilities.** The client does terminal setup,
  input capture, and rendering; the server does PTY, layout, screen, and mode
  state. Our split is the same, just smaller.
- **Pane geometry and layout tree.** Zellij's `PaneGeom`/split logic is a good
  reference for minimum sizes, focus navigation, and resize behavior. We use a
  simpler binary tree but keep the same separation between logical layout and
  resolved rectangles.
- **Per-client identity and focus.** Zellij distinguishes clients, active tabs,
  active panes, and input modes. V0.1 keeps focus per client, so model the
  client explicitly rather than assuming one global focus.
- **Input mode state machine.** “Normal”, prefix, copy, and resize modes are a
  proven way to avoid ad-hoc key handling. v0.1 needs only normal/prefix/help,
  but the state machine should be explicit.
- **Attach handshake and session metadata.** Session names, socket discovery,
  client attributes (size, style), and attach/detach messages are directly
  applicable.
- **Protobuf as an alternative.** Zellij's `client_server_contract` uses
  protobuf. We choose MessagePack for smaller setup cost, but protobuf is the
  fallback if the protocol becomes large or multi-language clients demand schema
  tooling.

### What not to borrow

- **Do not depend on `zellij-server` as a library.** `start_server` daemonizes on
  Unix, takes over the process, and expects the Zellij client protocol,
  configuration, plugins, and assets. It is the whole application, not an
  embeddable pane engine.
- **Do not copy the thread-per-subsystem bus.** Zellij's channel bus is mature
  but heavyweight. Tokio tasks with per-pane actors and bounded channels are a
  better fit for a new, smaller server.
- **Do not start with plugins or session resurrection.** They add a lot of
  protocol and state surface before the basic attach path is proven.
- **Do not copy the full pane model.** Floating panes, stacked panes, swap
  layouts, and plugin panes are valuable but out of scope for v0.1.

## 3. WezTerm

Repository: <https://github.com/wezterm/wezterm>
Relevant crates: `mux`, `wezterm-client`, `wezterm-mux-server`,
`wezterm-mux-server-impl`, `term`, `termwiz`, `pty`

### Architecture

WezTerm's mux is a separate daemon/client subsystem:

```text
Mux
  ├── tabs, panes, windows, domains
  ├── subscribers / MuxNotification
  ├── local panes (PTY + TerminalState)
  └── remote/client panes

wezterm-mux-server
  ├── daemonize / pid-file lock
  ├── SessionHandler per client
  ├── PduSender
  └── per-pane dirty-line computation

wezterm-client
  ├── client domain
  ├── client pane / renderable proxy
  └── PDU decode/encode

termwiz
  ├── Surface + Change deltas
  ├── SequenceNo
  └── escape/input parsing
```

A `Pane` trait abstracts local and remote panes. The server computes
`GetPaneRenderChangesResponse` per pane: cursor, dimensions, title, mouse grab,
alternate-screen state, dirty line ranges, and “bonus” lines. `termwiz::Surface`
tracks changes with `SequenceNo`; lines can be compressed for scrollback. The
client applies dirty lines to its own renderable proxy. The standalone mux
server has a daemonize/pid-file path, while `portable-pty` provides the PTY
abstraction.

### What is worth borrowing

- **Pane abstraction.** A `Pane` trait (or enum) that hides whether a pane is
  local or remote is a good long-term shape. v0.1 can have only local panes, but
  the session model should not assume that forever.
- **Dirty-line replication.** WezTerm's per-pane dirty ranges + cursor/title/mode
  metadata is the best reference for our structured/delta protocol. Stitch's
  `HistoryScreen` exposes dirty rows; adding a sequence number and absolute row
  coordinates is the natural next step.
- **Compressed scrollback rows.** WezTerm compresses rows for scrollback before
  sending. Our `CaptureResponse` should send only the requested range and avoid
  repeating style data where possible.
- **Sequence numbers.** A monotonic per-pane/per-session sequence lets clients
  detect dropped frames and request a resync. Our `Render.seq` and `Resync`
  message are the v1 version of this idea.
- **Daemonize/pid lock.** WezTerm's Unix daemonize and pid-file lock are a good
  reference for single-instance behavior. We can use a simpler socket-bind lock
  plus a detached child process.
- **Surface change deltas.** `termwiz::surface::Change` is a proven model for
  “apply only what changed.” If we add a structured client, evaluate using
  `termwiz`'s Surface rather than inventing a cell-diff format from scratch.

### What not to borrow

- **Do not depend on WezTerm's mux.** `wezterm-mux-server`/`wezterm-client` are
  not published as standalone crates; they live in the WezTerm workspace and
  carry its config, domain, GUI, and crypto assumptions.
- **Do not pull in `termwiz` just for emulation.** Stitch already has a screen.
  `termwiz` is large and explicitly subject to sweeping changes. Use it only if
  we need its Surface/delta model or its escape/input facilities.
- **Do not copy the full domain model.** SSH, TLS, WSL, serial, exec domains, and
  GUI windows are out of scope. A local `Pane` and `Session` are enough.
- **Do not use `portable-pty` as the primary PTY.** Stitch already provides the
  cross-platform PTY we need. `portable-pty` remains a reference and a fallback.

## 4. Rex

Rex is the terminal multiplexer from Superlogical (Mitchell Hashimoto's team).
It entered public testing on **2026-10-05**. At the time of this survey the
public beta was a macOS client plus Linux/FreeBSD servers; the Windows server was
limited, and the iOS/Linux/Windows clients were not yet part of public testing.
Some macOS features were marked Bleeding Edge, not Preview.

Rex is relevant because it is a modern, GUI-first mux with a strong activity and
automation model. It is **not** a dependency and its remote/multi-user features
are out of scope for stitch-mux.

### Architecture

```text
Rex.app (macOS; iOS/web/Linux/Windows clients in progress)
  ├── local Rex server, or remote Rex server
  ├── sessions / windows / blocks / clients
  ├── tiled layouts + floating layers
  ├── block ownership (size preference per client)
  ├── native selection, scrollback, themes, command palette
  └── OSC 7501 program status + activity UI

Rex server
  ├── PTYs and terminal blocks (today terminals; block creators are extensible)
  ├── local Unix socket / Windows named pipe
  ├── SSH / Tailscale / HTTP listeners (out of scope for us)
  ├── CLI, Lua, self-describing server API, WebSocket data tickets
  └── personal / shared / login server modes (out of scope for us)
```

Rex's docs describe a **block** as one application (today a terminal), a
**window** as a tab holding a layout, and a **session** as durable work. Blocks
can be detached from a window, floated in layers, and owned by a client for size
purposes. The server keeps sessions alive when clients detach.

### What is worth borrowing

- **OSC 7501 program status.** A generic, structured status protocol with
  `idle`/`working`/`done`/`blocked`/`error`, optional `permission`/`question`/
  `auth` kinds, an app name, and a message. This is the right shape for our
  activity model and agent reporting. See [PROTOCOL.md](PROTOCOL.md) and
  [AUTOMATION.md](AUTOMATION.md).
- **Activity as state, not toast notifications.** Session/tab indicators,
  activity filters, Dock badges, sounds, and unread state all derive from the
  same structured record. We should copy that separation, not the exact UI.
- **Pane lifetime independent of placement.** Detached blocks, `place`/`move`,
  and detached capture are a narrow, useful part of Rex's block model. We adopt
  the lifecycle idea in v0.3, not the block/plugin platform.
- **Per-pane ownership for size preferences.** Rex lets a block follow its owner
  when clients want different sizes. We keep smallest-client as the v0.1 default
  and treat ownership as a v0.3 option; it is a local size policy, not auth.
- **Local CLI/JSON/events.** `rex ls --json`, `run --wait`, `wait`, `capture`,
  `send`, `send-key`, and `events --json` are a strong automation surface. We
  adopt the local subset; no remote API.
- **Agent skill.** Rex teaches Claude Code/Codex/OpenCode/Pi to use its CLI,
  open panes, read screens, and report status. We adopt a plain local skill over
  our CLI, without a privileged daemon hook.
- **Native UX details.** Command palette, key sequences, which-key hints, key
  modes, structured selection/scrollback, OSC 8 links, shell integration, and
  prompt jumps are good local GUI targets. Cursor shaders, Display P3, and
  animation polish are adapter-level, not architecture.
- **Multi-window/floating layers.** Useful as a v0.3 reference after the
  detached-pane model. We do not put floating layers in v0.1.
- **Self-describing API.** Rex's `rex api list/describe/call` with JSON Schema is
  excellent for agents, but a full dynamic API is overkill for our local v1. We
  keep typed protocol + stable JSON CLI instead.

### What not to borrow

- **Remote/multi-user.** SSH, Tailscale, VPN/HTTP listeners, personal/shared/
  login server modes, PAM/accounts, port forwarding, remote hosts, web client,
  and iOS client are explicitly out of scope. They change the threat model and
  product surface.
- **Lua/plugin/block creators.** A dynamic scripting surface and a plugin block
  platform would delay the core. Our automation is a CLI/event API, not an
  embedded language.
- **Native app implementation.** Rex's native macOS/iOS UI is not a reusable
  cross-platform engine. Our GUI adapters remain egui/GPUI/Qt over structured
  cells.
- **Full session/service management.** Systemd units, shared login servers,
  upgrade orchestration, and remote host management are out.
- **The whole "multiplexer for all work" abstraction.** Rex is intentionally
  broader than a terminal mux. stitch-mux stays a local terminal multiplexer
  with a strong GUI and local automation.

### What to verify later

- How Rex resolves divergent client sizes in practice; its ownership model is a
  reference, not a spec.
- Whether OSC 7501 becomes a de facto standard or needs adaptation in our
  parser; keep unknown fields/records optional.
- How much of the native UX (selection, scrollback, prompt jumps) can be matched
  in egui versus Qt/GPUI.

## 5. Suitable building blocks

| Need | Candidate | Decision |
|---|---|---|
| PTY, child, resize | `stitch-pty` | **Use now** |
| Terminal screen | `stitch-pty::HistoryScreen` | **Use now** |
| Alternative screen | `avt` | Evaluate if stitch fidelity is insufficient |
| Alternative screen | `alacritty_terminal` | Evaluate for full Alacritty-grade grid |
| Surface/deltas | `termwiz` | Evaluate for structured delta protocol |
| Layout | own binary split tree | **Build**; Zellij/rmux as references |
| Local IPC | tokio Unix socket / Windows named pipe | **Use now** (interprocess is a fallback) |
| Control encoding | `rmp-serde` (MessagePack) | **Use now** |
| High-speed binary frames | `bincode` | Evaluate later for render deltas |
| Schema-first protocol | `prost` / protobuf | Evaluate if multi-language clients grow |
| Client terminal control | `crossterm` | **Use now** |
| Native client widgets | `ratatui` | Later, after structured frames |
| Terminal widget | `tui-term` | Only if using `vt100`; not a fit for stitch cells |
| Routing/tiling helpers | none needed | Keep the layout tree small |
| Daemonization | std + `libc`/`windows-sys` | **Build** a small platform shim |
| Logging | `tracing` | **Use now** |
| CLI | `clap` | **Use now** |

### Why `stitch-pty` over `portable-pty`

`stitch-pty` already has:

- native Unix PTY and Windows ConPTY;
- async read/write and `reader()`/`writer()` handles;
- child lifecycle, `wait`, terminate, resize, winsize;
- a terminal screen with scrollback, alternate screen, modes, dirty rows,
  title/cwd, bells, and Kitty keyboard parsing/encoding.

Using `portable-pty` would duplicate the transport layer and give us a second
PTY implementation to maintain. It is useful as a reference and a fallback if a
platform bug appears.

### Why `avt`/`alacritty_terminal` are not immediate dependencies

Stitch's screen is already integrated and has the Kitty work. `avt` has an
excellent composable observer/dirty-row model and is `no_std`-friendly;
`alacritty_terminal` has a battle-tested grid and selection model. But adding
either now creates a second screen model and a translation layer before we know
stitch's gaps matter. Start with stitch; add an `Emulator` adapter only if
conformance tests show a real failure.

### Why not `tui-term`

`tui-term` is a ratatui widget built around `vt100`. It is useful for a
single-process terminal widget, but it does not render stitch's `HistoryScreen`
cells and would force a second emulator into the client. If we add a ratatui
client, build a small `PaneWidget` over our structured frame instead.

## 6. Architecture conclusions

1. **The server must own screen state.** All four references do this; it is what
   makes reattach, activity, and multi-client behavior coherent.
2. **Use structured cells for the primary GUI.** rmux's snapshot/render split,
   WezTerm's client-pane/dirty-line model, and Rex's structured session/activity
   API all support this direction. Keep ANSI as a second projection for terminal
   compatibility.
3. **Borrow the envelope and dirty-line ideas, not the engines.** rmux's
   versioned protocol and WezTerm's dirty-line/sequence model are the most
   reusable pieces. Zellij's layout and input-mode model is the best reference
   for panes and focus. Rex's OSC 7501/activity model is the best reference for
   local GUI attention state.
4. **Keep the v1 scope smaller than all four.** One window, binary splits,
   per-client focus, no plugins, no web, no graphics, no session resurrection,
   and no remote/multi-user features.
5. **Do not take a dependency on a mux engine.** The point of `stitch-mux` is to
   make `stitch-pty` the foundation, not to wrap another multiplexer.
6. **Adopt local automation and activity, reject remote/multi-user.** Rex shows
   the value of a scriptable mux and structured activity; its remote and
   multi-user layers are a different product.

## 7. Source map

Useful starting points for future work:

- rmux architecture: <https://github.com/Helvesec/rmux/blob/main/docs/ARCHITECTURE.md>
- rmux protocol: `crates/rmux-proto/src/envelope.rs`, `codec.rs`, `attach.rs`
- rmux server/render: `crates/rmux-server/src/daemon.rs`,
  `pane_io.rs`, `renderer.rs`
- rmux pure render core: `crates/rmux-render-core/src/`
- Zellij server: `zellij-server/src/route.rs`, `thread_bus.rs`, `pty.rs`,
  `screen.rs`, `panes/tiled_panes/`
- Zellij client: `zellij-client/src/input_handler.rs`, `stdin_handler.rs`,
  `server_launch.rs`
- Zellij IPC: `zellij-utils/src/ipc.rs`, `client_server_contract/`
- WezTerm mux: `mux/src/lib.rs`, `pane.rs`, `domain.rs`
- WezTerm server/client: `wezterm-mux-server-impl/src/sessionhandler.rs`,
  `dispatch.rs`; `wezterm-client/src/pane/`
- WezTerm surface: `termwiz/src/surface/`
- stitch-pty: `../stitch-pty/src/session.rs`,
  `../stitch-pty/src/terminal/history.rs`,
  `../stitch-pty/src/terminal_session.rs`
- Rex public testing post: <https://www.superlogical.com/updates/public-testing-beginning>
- Rex docs: <https://www.superlogical.com/rex/docs>
- Rex CLI: <https://www.superlogical.com/rex/docs/automate/cli>
- Rex server API: <https://www.superlogical.com/rex/docs/automate/server-api>
- Rex Program Status Protocol: <https://www.superlogical.com/rex/docs/build/program-status>
- Rex agents: <https://www.superlogical.com/rex/docs/automate/agent-skill>
- Rex events: <https://www.superlogical.com/rex/docs/reference/events>
- Rex remote access (reference only, out of scope):
  <https://www.superlogical.com/rex/docs/linux/remote-access>
