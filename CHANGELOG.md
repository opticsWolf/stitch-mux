# Changelog

All notable changes to stitch-mux are documented here. The project is local-only
and single-user; versions follow the workspace version in `Cargo.toml` and the
Python package version in `pyproject.toml`.

## Unreleased (v0.2 work)

### Added

- **M12 workspace controls:** GUI tab strip with window create/focus controls,
  a Windows panel for rename/reorder/close and floating raise/lower/minimize/
  tile, CLI and agent-skill commands for all window/layer operations, and
  daemon-owned `window.*`, `pane.float`/`pane.tile`, and `floating.*` actions.
- **M12 workspace core:** daemon-owned ordered windows with independent tiled
  trees, bounded fixed-point floating layers with z-order/minimize, window-aware
  `Run`/`SetLayout`, typed window/layer commands and events, and an active-window
  projection for terminal clients. Real-PTY tests cover two windows, floating
  close/tile, z-order changes, and atomic rejection of invalid bounds.
- **M11 workspace controls:** CLI `detach-pane`/`place-pane`/`claim-pane`/
  `release-pane`, a GUI background-pane window with detach/place/close actions,
  explicit close for detached panes, and a recycled-pane runtime test that
  verifies no `Arc<PaneRuntime>` survives close.
- **M11 detached panes (server slice):** session-owned placement maps, an
  explicit empty layout root, `DetachPane`/`PlacePane`, `ClaimPane`/
  `ReleasePane` size-owner policy, placement/owner events, and capability
  gating. Detached panes keep running and remain capturable/waitable; real-PTY
  tests cover detach/place, detached exit, and owner-based sizing.

- **M9 core foundation:** typed action registry with argument validation,
  canonical key names, JSON keymap merging, and a bounded mode/sequence
  dispatcher with which-key hints and double-prefix passthrough. The daemon
  now serves `ListActions`, `GetKeymap`, `InvokeAction`, and `ReloadKeymap` with
  capability checks, typed argument errors, and local change events.
  The CLI, terminal frontend, and egui frontend share the daemon's keymap; the
  GUI adds a command palette and which-key/mode indicators.

- **M10 capture/export:** `capture --format vt` emits SGR-styled text,
  `--format html` emits escaped styled rows, and scrollback is bounded by
  `STITCH_MUX_SCROLLBACK` (default 2000 history rows).

- **M10 shell integration:** bounded OSC 133/1337 scans publish prompt/command
  marks and cwd in pane snapshots, and `total_lines` lets clients map absolute
  marks to scrollback offsets. The GUI adds `prompt.previous`/`prompt.next`
  (`C-b [`/`C-b ]`) and ‹P/P› status-bar buttons; no screen text is parsed.
- **M10 smooth scrolling:** egui accumulates fractional wheel deltas and paints
  one extra clipped row, with pointer/selection mapping using the same shift.
  A real-PTY alternate-screen test now asserts shell-screen restoration.
- **M10 notifications/progress:** bounded OSC 9/777 and OSC 9;4 scans publish
  current-value notification and progress metadata in snapshots/deltas; the egui
  status bar shows the focused pane's latest record without any daemon-side
  desktop side effect.
- **M10 selection/copy:** the egui frontend selects cells across its bounded
  scrollback overlay, copies with `Ctrl+C` or copy-on-drag, supports
  `Ctrl+Shift+A` select-all and `Esc` clear, and publishes PRIMARY selection
  where supported. With no selection, `Ctrl+C` still reaches the child.
- **M10 hyperlinks:** bounded OSC 8 scans attach URIs to authoritative cells.
  Web/mail links open directly; custom schemes require an explicit
  confirmation dialog. A new [terminal fidelity](docs/TERMINAL-FIDELITY.md)
  document records the supported surface and its limits.
- **M7 program status and activity:** bounded mux-side OSC 7501 scanner,
  current-status propagation to panes/sessions/GUI, textual attention badges,
  activity filtering and local unread state in the reference GUI, a sourceable
  shell helper, and a bounded local event stream with typed backpressure.

### Fixed

- Explicit `KillSession` now publishes `pane_exited` instead of racing the exit
  watcher with task cancellation.

- **M8 local automation:** `run`, `wait`, `send`, `send-keys`, `capture`,
  `events --json`, and `skill-install`. `run --wait`/`wait` adopt the child's
  exit status; daemon `Run` (split placement) and `Wait` (pane/session with
  timeout) return authoritative state. `pane_created`, `pane_exited`,
  `program_status_changed`, and `activity_changed` stream to local subscribers,
  and unscoped subscribers always receive the initial `session_list`. The
  portable agent skill installs idempotently into Claude Code, Codex, OpenCode,
  and Pi skill directories.

## 0.1.0 (v0.1 candidate, 2026-11-20)

Not published to crates.io or PyPI. Publication requires explicit approval.

### Added

- Owner-scoped local IPC: Unix sockets (`0700` directory, `0600` socket) and
  Windows named pipes with a per-user name. No TCP listener and no accounts.
- Daemon-authoritative sessions, panes, tiled layout, per-client focus, zoom,
  bounded snapshot/delta streams, captures, and typed errors.
- Detach/reattach: children and pane contents survive GUI close, GUI process
  kill, and client crashes. Lost sessions are never silently recreated.
- Structured toolkit-neutral client (`stitch-mux-client`) plus a disposable egui
  reference GUI, an ANSI terminal client, and a PyO3/PySide6 adapter.
- Server-composed ANSI projection for terminal clients; clients never parse the
  screen or encode keys.
- GUI diagnostics (F1 or status-bar button): endpoint, connection state, log
  path, and a fresh daemon session list. A shared file log is available to every
  process via `STITCH_MUX_LOG` or the platform state directory.
- Deterministic protocol fuzz smoke, golden frames, concurrent layout/pane-ID
  tests, a 10 MiB two-client stress with a stalled-client `Busy` proof, a
  12-cycle crash/clean-detach soak, and a three-client focus stress. All run in
  three-platform CI.

### Fixed during hardening

- Concurrent splits no longer drop subtrees; pane IDs are process-wide.
- Named-pipe bursts no longer fail with `ERROR_PIPE_BUSY`.
- Control resizes defer to attached clients; pane input, frame queues, and
  layout decoding are bounded.
- Coalescer snapshots cannot roll back newer state; viewport diffs and
  per-client delta sequences survive scroll and reattach.
- Modified keys, ANSI colors, nested split axes, pane buttons, status geometry,
  and live repaint are correct in the egui and Qt adapters.
- Client abort closes both socket halves, so a killed frontend detaches instead
  of leaving a stale registry entry.

### Known limitations

- The GUI's wheel history is a bounded local cache; a burst that outpaces the UI
  can leave gaps until a server-side viewport/scrollback protocol lands.
- Session restoration after a daemon crash is explicitly out of scope.
- The 30-minute soak is opt-in with `STITCH_MUX_SOAK_MS=1800000`; CI runs the
  short form on every platform.
