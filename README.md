# stitch-mux

A terminal multiplexer built on top of [`stitch-pty`](../stitch-pty).

> **Status: v0.1 candidate (`0.1.0`).** The daemon keeps a client registry,
> sizes sessions to the smallest attached client, bounds and coalesces each
> client's updates, and exposes `ListClients`, `DetachClient`, session rename,
> and typed shutdown. The reference GUI retries lost sessions with backoff and
> never silently recreates them. A 10 MiB two-client stress test, a
> stalled-client `Busy` proof, deterministic decoder fuzzing, a crash/clean-detach
> soak, and a three-client focus stress run in CI on Linux, macOS, and Windows.
> The terminal frontend writes server-composed ANSI frames and shares sessions,
> layout, and focus with the GUI. A PyO3 SDK and PySide6 adapter prove the
> frontend boundary with a second toolkit, with no protocol changes. Local
> diagnostics and a file log are built in. Publication is not performed without
> explicit approval.

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

## Repository layout

```text
stitch-mux/
├── Cargo.toml                 # the single publishable package + workspace
├── README.md
├── src/
│   ├── lib.rs                 # public Rust client API and daemon entry point
│   ├── main.rs                # stitch-mux executable
│   ├── core/                  # IDs, layout, cells, actions, status
│   ├── protocol/              # framing, DTOs, local IPC
│   ├── server/                # daemon, sessions, panes, terminal state
│   ├── client/                # attach, commands, SessionView
│   ├── terminal.rs            # ANSI compatibility frontend
│   └── gui/                   # egui reference adapter (feature = "gui")
├── docs/                      # architecture, protocol, milestones, reviews
├── agent-skill/               # assets embedded by skill-install
├── bindings/python/           # PyO3 SDK manifest (publish = false)
├── examples/gpui/             # GPUI spike (publish = false)
└── tests/                     # integration tests and PTY fixtures
```

`cargo install stitch-mux` installs the daemon, CLI, and ANSI attach. Add
`--features gui` to include the egui reference adapter behind `stitch-mux gui`.
The Python binding, GPUI spike, fixtures, and tests stay in the repository and
are excluded from the crates.io archive.

## Quickstart

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
cargo run -p stitch-mux -- attach -s demo  # ANSI terminal view
cargo run -p stitch-mux -- clients -s demo
cargo run -p stitch-mux -- close -t 102
cargo run -p stitch-mux -- rename --from demo --to work
cargo run -p stitch-mux -- kill -t work

# Local automation (no GUI needed)
cargo run -p stitch-mux -- run -s task --wait -- sh -c 'exit 7'  # exits 7
cargo run -p stitch-mux -- capture -t 101 --lines 200 --text
cargo run -p stitch-mux -- events --session task --timeout-ms 5000
cargo run -p stitch-mux -- skill-install --targets pi,claude
cargo run -p stitch-mux -- actions
cargo run -p stitch-mux -- get-keymap
cargo run -p stitch-mux -- invoke pane.split -t 101 --arg axis=columns --arg ratio_bps=5000
```

On Windows use `cmd.exe` instead of `sh`. `stitch-mux gui -s demo` skips the
picker. In the GUI, click a pane to focus it, use `Ctrl+Arrow` to move focus,
drag a splitter to change its ratio, and use each pane's title bar for
left/right (`LR`), top/bottom (`TB`), close (`X`), and zoom (`Z`/`U`). The top
status bar shows session size, revision, clients, activity, and bell state;
`Detach` leaves the session alive. `F1` or the `Diagnostics` button opens a
local panel with the endpoint in use, connection state, log path, and a fresh
session list. Ctrl-C, Alt-key, and punctuation keys are sent as semantic modified
keys, and the pane keeps repainting while output arrives. Wheel-up shows a
bounded local scrollback cache; the daemon still owns authoritative scrollback,
so a burst that outpaces the UI can leave gaps until the viewport protocol lands.
The egui adapter is a disposable reference; the protocol and client state are
toolkit-neutral.

The terminal view can be open at the same time as the GUI. It enters the
alternate screen and restores it on detach. `Ctrl-]` is the local prefix:
`Ctrl-] d` detaches without killing the child, `Ctrl-] r` requests a full
resync, and `Ctrl-]` followed by an arrow moves this client's focus. All other
keys are sent as semantic key events; the daemon's authoritative screen encodes
them for the focused pane's Kitty flags.

A Python client and Qt adapter are available through the maturin package:

```sh
uv sync --extra test
uv run python -m stitch_mux.qt --session demo
```

### Logs and diagnostics

All processes write to a local file: `$XDG_STATE_HOME/stitch-mux/stitch-mux.log`
on Unix (falling back to `$HOME/.local/state` and then the temp directory),
`%LOCALAPPDATA%/stitch-mux/stitch-mux.log` on Windows. Set `STITCH_MUX_LOG` to
override the path. No log events are sent over the network. CLI failures print
the log path; the GUI diagnostics panel shows it and the active endpoint.

### Verification

`cargo test --workspace` runs the unit and real-PTY integration suites. The
release stress job additionally runs the 10 MiB flood and the short
detach/reattach soak on three platforms. For the 30-minute acceptance soak:

```sh
cargo build --release -p stitch-mux-test-fixtures
STITCH_MUX_SOAK_MS=1800000 cargo test --release -p stitch-mux --test soak -- --ignored
```

See [Python SDK](docs/PYTHON.md), [parity](docs/PARITY.md), and the
[GPUI spike](docs/GPUI-SPIKE.md) for scope and licensing notes.

## Documentation map

- [Changelog](CHANGELOG.md) — v0.1.0 feature and hardening history.
- [Architecture](docs/ARCHITECTURE.md) — the design and the reasoning.
- [Protocol](docs/PROTOCOL.md) — the local wire protocol.
- [Frontends](docs/FRONTENDS.md) — egui, GPUI, and Qt/PySide6 adapter design.
- [Python SDK](docs/PYTHON.md) — PyO3 API, PySide6 adapter, and licensing.
- [Parity](docs/PARITY.md) — frontend capability checklist.
- [GPUI spike](docs/GPUI-SPIKE.md) — optional second-native-toolkit boundary.
- [Conformance](docs/CONFORMANCE.md) — M4 terminal corpus and interactive checks.
- [Automation](docs/AUTOMATION.md) — local CLI, event stream, and agent skill.
- [Stitch PTY integration](docs/STITCH_PTY_GAPS.md) — terminal replies, OSC metadata, hyperlinks, and exit draining.
- [Keymaps](docs/KEYMAP.md) — action names, JSON bindings, modes, and reload.
- [Terminal fidelity](docs/TERMINAL-FIDELITY.md) — capture formats, links, and
  explicit fidelity limits.
- [Implementation plan](docs/IMPLEMENTATION_PLAN.md) — milestones and order of
  work.
- [Implementation protocol](docs/IMPLEMENTATION_PROTOCOL.md) — living operating
  rules, gates, decision log, and status.
- [Reference evaluation](docs/REFERENCES.md) — what to borrow and what not to
  borrow from rmux, Zellij, WezTerm, and Rex.

## License

MIT OR Apache-2.0, matching `stitch-pty`.
