# Changelog

All notable changes to stitch-mux are documented here. The project is local-only
and single-user; versions follow the workspace version in `Cargo.toml` and the
Python package version in `pyproject.toml`.

## Unreleased (v0.2 work)

### Added

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
