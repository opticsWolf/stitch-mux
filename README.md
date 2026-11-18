# stitch-mux

A terminal multiplexer built on top of [`stitch-pty`](../stitch-pty).

> **Status: M3 multi-client session work in progress.** The daemon now keeps a
> client registry, sizes sessions to the smallest attached client, and supports
> `ListClients`, `DetachClient`, session rename, and typed shutdown. Bounded
> client queues and GUI reconnect are next.

## Why another multiplexer?

`stitch-pty` already provides the hard, platform-specific parts: native PTYs on
Unix, ConPTY on Windows, async I/O, child lifecycle, resize, terminal
emulation, scrollback, and Kitty keyboard handling. What it deliberately does
not provide is a **session/daemon model**, **pane layout**, **attach/detach**,
**IPC**, or a **compositor**.

`stitch-mux` adds those layers while keeping `stitch-pty` transport-only. It is
not a fork of Zellij, WezTerm, rmux, or Rex; those projects are architectural
references and a source of proven ideas.

## Goals

- Native Linux, macOS, and Windows.
- **Detach and reattach sessions without killing child processes**; this is a
  core requirement, not a later convenience.
- **A structured GUI view as the primary frontend**: per-pane cell snapshots,
  cursors, modes, and layout geometry that a GUI can draw into its own widgets.
- **GUI toolkit independence**: egui, GPUI, and Qt/PySide6 adapters consume the
  same toolkit-agnostic client state.
- Server-authoritative terminal state, so reattach is faithful and multiple
  clients can observe the same session.
- Tiled panes, per-client focus, resize, zoom, and a minimal status bar.
- **Program status and activity**: panes can report idle/working/done/blocked/
  error through OSC 7501, and the GUI can surface which pane needs attention.
- **A local automation surface**: a scriptable CLI, JSON output, a bounded JSON
  event stream, `run --wait`, `capture`, and an agent skill. This is for local
  tools and coding agents, not a remote API.
- **Terminal fidelity where it matters**: OSC 8 hyperlinks, shell-integration
  metadata, selection, scrollback, and safe link handling.
- **Explicit actions and keymaps**: a server-owned action registry with
  configurable bindings, key sequences, modes, and a GUI command palette.
- A terminal ANSI client as a compatibility frontend, not the architectural
  center.
- Rust-first internals, with a Python control SDK and PySide6 reference adapter
  added after the core is stable.
- Reuse `stitch-pty` for PTY, screen, resize, and Kitty key encoding.
- Explicit, versioned, local-only IPC with bounded memory and backpressure.

## Non-goals for the first release

- tmux command-line or configuration compatibility.
- Plugins, WASM, user Lua scripting, or a web client. The local CLI/event API is
  in scope; an embedded scripting language is not.
- Copy mode/search in the first release. Native GUI selection, copy/paste, and
  bounded scrollback capture are in scope.
- Freeform/overlapping pane geometry in the first release; server-owned tiled
  layout comes first. Multi-window and floating layers are later workspace
  features.
- Session restoration after the daemon itself restarts.
- Sixel, Kitty graphics, or inline images.
- **Remote, network, or multi-user access.** No TCP listener, SSH client,
  Tailscale integration, login server, shared server, port forwarding, or
  remote host management.
- Full terminal conformance beyond the fidelity items listed above; advanced
  underline styles and graphics protocols remain follow-up work.

Plugin/terminal capabilities may be revisited later, but they must not distort
the first architecture. Remote/multi-user access is a scope boundary, not a
backlog item; it requires a separate product decision and threat model.

## Priorities after the v0.1 GUI core

These are the features adopted from the Rex evaluation, filtered to stitch-mux's
local, single-user scope:

1. **Program status and activity** — OSC 7501 state, per-pane activity, session
   summaries, unread/attention UI, and agent status reporting.
2. **Local automation and agents** — JSON CLI, `run --wait`, `wait`, `capture`
   formats, an NDJSON event stream, stable IDs, and a small agent skill.
3. **Detached panes and ownership** — pane lifetime independent of layout
   placement, plus an explicit size-owner policy for local multi-client use.
4. **Terminal fidelity** — OSC 8 hyperlinks, OSC 133 shell integration,
   foreground process/cwd metadata, desktop notifications, and prompt jumps.
5. **Actions, keymaps, and command palette** — a server-owned action registry
   with sequences, modes, which-key hints, and configurable bindings.
6. **Multi-window and floating layers** — multiple windows/tabs per session and
   overlay panes, after the detached-pane model is stable.
7. **Large scrollback and export** — an explicit scrollback budget and text,
   JSON, VT, and HTML capture formats.

Remote/multi-user features from Rex are deliberately excluded: SSH, Tailscale,
VPN/HTTP listeners, personal/shared/login server modes, PAM/accounts, remote
hosts, port forwarding, the web client, and the iOS client.

## Planned repository layout

```text
stitch-mux/
├── Cargo.toml                 # workspace
├── README.md
├── docs/
│   ├── ARCHITECTURE.md        # components, data flow, decisions
│   ├── PROTOCOL.md            # wire format and attach messages
│   ├── FRONTENDS.md           # egui/GPUI/Qt-PySide6 adapter evaluation
│   ├── AUTOMATION.md          # local CLI, events, and agent interface
│   ├── IMPLEMENTATION_PLAN.md # milestones, tasks, acceptance criteria
│   ├── IMPLEMENTATION_PROTOCOL.md # living execution rules and decision log
│   └── REFERENCES.md          # rmux/Zellij/WezTerm/Rex evaluation
├── agent-skill/               # local agent instructions and hooks (later)
└── crates/
    ├── stitch-mux-core/       # IDs, layout, frame/cell model
    ├── stitch-mux-protocol/   # framing, messages, capabilities
    ├── stitch-mux-server/     # daemon, session actor, pane runtime
    ├── stitch-mux-client/     # toolkit-agnostic protocol/state library
    ├── stitch-mux-gui-egui/   # reference egui/eframe GUI
    ├── stitch-mux-gui-gpui/   # optional GPUI adapter
    ├── stitch-mux-terminal/   # ANSI terminal compatibility client
    ├── stitch-mux/            # CLI binary
    └── stitch-mux-py/         # PyO3 control SDK + PySide6 adapter (later)
```

## Quickstart (M2)

```sh
# Terminal 1: local daemon
cargo run -p stitch-mux -- server --foreground

# Terminal 2: a detached child that survives client exits
cargo run -p stitch-mux -- new -s demo -- sh
cargo run -p stitch-mux -- ls         # shows pane IDs
cargo run -p stitch-mux -- split -t 101 --axis columns
cargo run -p stitch-mux -- zoom -s demo -t 101
cargo run -p stitch-mux -- zoom -s demo   # unzoom
cargo run -p stitch-mux -- gui        # pick "demo"
cargo run -p stitch-mux -- close -t 102
cargo run -p stitch-mux -- kill -t demo
```

On Windows use `cmd.exe` instead of `sh`. `stitch-mux gui -s demo` skips the
picker. In the GUI, click a pane to focus it, use `Ctrl+Arrow` to move focus,
drag a splitter to change its ratio, and use the status bar to split, close,
zoom, and detach. The egui adapter is a disposable reference; the protocol and
client state are toolkit-neutral.

## Documentation map

- [Architecture](docs/ARCHITECTURE.md) — the design and the reasoning.
- [Protocol](docs/PROTOCOL.md) — the local wire protocol.
- [Frontends](docs/FRONTENDS.md) — egui, GPUI, and Qt/PySide6 adapter design.
- [Automation](docs/AUTOMATION.md) — local CLI, event stream, and agent skill.
- [Implementation plan](docs/IMPLEMENTATION_PLAN.md) — milestones and order of
  work.
- [Implementation protocol](docs/IMPLEMENTATION_PROTOCOL.md) — living operating
  rules, gates, decision log, and status.
- [Reference evaluation](docs/REFERENCES.md) — what to borrow and what not to
  borrow from rmux, Zellij, WezTerm, and Rex.

## License

MIT OR Apache-2.0, matching `stitch-pty`.
