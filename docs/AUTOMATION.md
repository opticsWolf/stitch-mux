# Local Automation and Agents

`stitch-mux` is a local, single-user multiplexer. Its automation surface exists
so local scripts and coding agents can drive the same sessions a human uses in
the GUI. It is **not** a remote API, an SSH gateway, a multi-user control plane,
or a hosted agent service.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the daemon model,
[PROTOCOL.md](PROTOCOL.md) for the control messages, and
[STITCH_PTY_GAPS.md](STITCH_PTY_GAPS.md) for upstream API gaps found while
implementing status and automation, and [KEYMAP.md](KEYMAP.md) for bindings.

**v0.1 status:** the CLI/control-command subset and file log are implemented.
**v0.2 work landed:** program status, the bounded event stream, `run`/`wait`/
`send`/`send-keys`/`capture`/`events --json`, the local agent skill, and the
daemon-owned action/keymap system. No privileged hook is required.

## 1. What the automation surface is for

- Start a command in a new pane or split and wait for its exit status.
- Read a pane's visible screen or bounded scrollback.
- Send text or semantic keys to a pane.
- List sessions, panes, and activity without attaching a GUI.
- Subscribe to a bounded local event stream.
- Invoke named layout/focus/session actions.
- Give a coding agent a small, stable CLI instead of screen-scraping the GUI.

The model is deliberately **session-first**, not window-first: a command can
create a pane, detach, and let the agent or human inspect the result later. This
is the same property that makes GUI detach/reattach work.

## 2. Interface layers

```text
human / script / agent
        │
        ├─ stitch-mux CLI ── control connection ── daemon
        │                         (short-lived)
        │
        ├─ stitch-mux events ── event subscription ── daemon
        │                         (long-lived, bounded)
        │
        └─ agent skill ── instructions + CLI commands
```

All layers use the same owner-scoped Unix socket or Windows named pipe. There is
no HTTP endpoint, token exchange, remote host, or account database.

## 3. CLI surface

The initial automation-oriented commands are:

```sh
stitch-mux new -s work -- bash
PANE=$(stitch-mux ls | python -c 'import json,sys; print(json.load(sys.stdin)["sessions"][0]["panes"][0]["id"])')
stitch-mux run -s work --wait -- cargo test
stitch-mux run -s work --target "$PANE" --axis columns -- htop
stitch-mux wait --target "$PANE" --timeout-ms 10000
stitch-mux capture --target "$PANE" --format text --lines 200
stitch-mux capture --target "$PANE" --format json
stitch-mux send --target "$PANE" 'make test'
stitch-mux send-keys --target "$PANE" ctrl+c enter
stitch-mux ls --json
stitch-mux events --session work --timeout-ms 30000
stitch-mux actions
stitch-mux get-keymap
stitch-mux invoke pane.split --target "$PANE" --arg axis=columns --arg ratio_bps=5000
stitch-mux reload-keymap --path ./keymap.json
```

### 3.1 `run` and `wait`

The local CLI also exposes `detach-pane -t PANE`, `place-pane -t PANE
[--anchor PANE] [--axis columns|rows] [--ratio N]`, `claim-pane -t PANE
[--mode take|request]` and `release-pane -t PANE`. Detached panes keep their
process and scrollback; `close -t PANE` is the explicit destructive operation.

`run` creates a pane in the active window (split; new-window placement arrives
with multi-window support), starts a program, and optionally waits for the child
to exit:

```sh
stitch-mux run -s build --wait -- go test ./...
```

- `--wait` returns the child's exit status.
- `--keep-open` leaves the pane visible after exit.
- `--axis columns|rows` and `--ratio 0..10000` choose the split direction and
  proportion; `--target PANE_ID` selects the pane to split (default: first pane).
- `--label` gives the pane a human-readable name.
- Without `--wait`, the command returns after the pane is created.

`wait` works on an existing pane or session and returns when it exits. A timeout
returns a distinct status and does not kill the pane.

### 3.2 `capture`

`capture` is read-only:

| Format | Content |
|---|---|
| `text` | Plain text, optionally trimmed/unwrapped |
| `cells` | Structured rows for tooling |
| `json` | Rows plus cursor, modes, title, activity, and cwd |
| `vt` | SGR-styled terminal text for replay/export |
| `html` | Escaped, styled `<pre>` rows for logs and reports |

The daemon enforces a maximum range (1000 rows) and frame size. Scrollback
history is bounded by `STITCH_MUX_SCROLLBACK` rows (default 2000, clamped to
100..20000); capture `total_lines` includes the live viewport. Full-screen/alternate-screen
programs have no scrollback; capture returns the visible screen.

### 3.3 `send` and `send-keys`

`send` writes literal text. `send-keys` sends semantic keys (`enter`, `tab`,
`escape`, arrows, function keys, modified keys) so the daemon can encode them
with the pane's Kitty flags. Do not send raw escape sequences when a semantic
key is available.

### 3.4 JSON and exit status

Every command that returns data supports `--json`. Results include stable IDs,
not just labels. Labels are convenient but can be renamed or duplicated.

Exit statuses are stable:

| Status | Meaning |
|---:|---|
| 0 | Success |
| 1 | CLI/protocol error, typed daemon error, or timed-out wait |
| 2 | Invalid arguments |
| 4 | Daemon unavailable |

`run --wait` and `wait` return the child's 0–255 exit code when it exits. A
timeout returns 1 with `timed_out: true` and does not kill the pane.

## 4. Event stream

`stitch-mux events --json` writes one JSON object per line. It is a **bounded
local event stream**, not a replay log.

```json
{"seq":41,"session_id":1,"kind":"program_status_changed","payload":{"pane_id":7,"status":{"state":"blocked","kind":"permission","app":"cargo","message":"allow network access?"}}}
{"seq":42,"session_id":1,"kind":"pane_exited","payload":{"pane_id":7,"reason":"exited with 0"}}
{"seq":43,"session_id":1,"kind":"activity_changed","payload":{"activity":{"idle":2,"working":1,"blocked":0,"done":1,"error":0}}}
```

Event categories:

- session created/destroyed/renamed;
- window created/closed/label changed;
- pane created/closed/detached/placed/exited/owner-changed;
- program status changed/removed;
- activity summary changed;
- layout revision changed;
- pane owner changed (v0.2);
- keymap/action registry changed.

Rules:

- `seq` is monotonic per subscription; gaps mean the subscriber fell behind.
- On a gap, reconnect and fetch current state with `ls --json`, `inspect`, or
  `capture`; do not assume the missed events are recoverable.
- The daemon never blocks pane readers on a slow subscriber. A subscriber that
  cannot drain is disconnected.
- Events contain IDs and structured fields, not raw terminal output. Use
  `capture` for screen contents.

The stream is local-only. It is intended for scripts, editors, and agents on the
same machine.

## 5. Program status and agent reporting

Programs can report status with **OSC 7501**:

```sh
. ./scripts/stitch-mux-status.sh
stitch_status working "Running tests"
cargo test && stitch_status done "Tests passed" || stitch_status error "Tests failed"
```

States: `idle`, `working`, `done`, `blocked`, `error`. Kinds: `permission`,
`question`, `auth`. A coding agent can use the same sequence to mark itself
blocked on approval or done with a task.

The daemon exposes the current status in `PaneSnapshot`/`PaneDelta` and as
`program_status_changed` events. The GUI derives unread/attention state; the
agent does not need a special notification API.

## 6. Agent skill

The agent interface is a small skill plus the CLI:

- teach the agent `ls`, `run`, `wait`, `capture`, `send`, `send-keys`, `events`,
  and `invoke`;
- explain stable IDs and JSON output;
- explain that `run --wait` returns the child's exit status;
- explain that `capture` is bounded and alternate-screen programs have no
  scrollback;
- explain that the agent must not kill a session to "clean up" unless asked;
- explain that sessions survive detach, so opening a pane is not a transient
  side effect.

The portable skill lives at `agent-skill/stitch-mux/SKILL.md`. Install it with:

```sh
stitch-mux skill-install --targets claude,codex,opencode,pi
# or: sh agent-skill/stitch-mux/scripts/install.sh
# STITCH_MUX_SKILL_TARGETS / STITCH_MUX_SKILL_ROOT override the script defaults
# skill-install supports --root DIR and --targets LIST and skips unchanged files
```

The installer is idempotent and copies only local documentation. Pi also
discovers `~/.agents/skills/` and project `.agents/skills/`; the same directory
layout works for the other agent targets. No privileged daemon hook is required.

A session started by an agent should have a clear label, for example the task or
repository name. An optional later feature can let the agent rename its own
session, but it is not required for the first automation slice.

## 7. Security and trust

- The local socket/pipe permissions and the current OS user are the trust
  boundary.
- There is no network listener, remote account, token, or multi-user
  authorization.
- The CLI must not log raw terminal input or output by default.
- `capture` and event logs can contain sensitive text; treat them as user data.
- An agent has the same local privileges as the user running it. Do not build a
  "sandbox" by hiding CLI commands; use OS-level isolation if that is required.
- Do not let an agent silently kill sessions or close panes. Destructive
  operations should be explicit in the command or confirmed by policy.

Remote/multi-user features are out of scope: SSH, Tailscale, HTTP/WebSocket
control planes, shared/login servers, PAM, port forwarding, and remote hosts do
not belong in this automation layer.

## 8. Staging

### v0.1: GUI and session basics

- session create/list/attach/detach/kill;
- `capture` for visible rows and bounded scrollback;
- no agent-oriented automation yet; the GUI is the primary interface.

### v0.2: events, activity, agents

- `run --wait`, `wait`, `send`, `send-keys`, and `ls --json`;
- `events --json` with bounded subscriptions;
- `program_status` in snapshots/deltas and activity summaries;
- `actions`/`get-keymap`/`invoke`;
- agent skill for the CLI;
- capture formats `text`, `json`, `vt`, `html` as time permits.

### Later

- Detached panes/ownership operations in the CLI.
- A local MCP wrapper over the CLI, if agents need structured tool calls rather
  than shell commands. It must remain local and use the same trust boundary.
- No remote automation API.

## 9. Testing

- CLI integration tests against a real daemon and real PTY.
- `run --wait` returns the exact child status on Unix and Windows.
- `capture` bounds: line limit, byte limit, alternate screen, wide chars,
  wrapped lines, styles.
- Event stream: monotonic `seq`, no blocking, disconnect on slow subscriber,
  reconnect + state refresh.
- Program status: OSC 7501 parse, replacement semantics, activity counts.
- Agent skill: dry-run install, idempotent update, no changes outside its own
  skill/hook files.
- Security: no remote listener; socket/pipe permissions; no raw input/output in
  default logs.
