---
name: stitch-mux
description: Drive a local stitch-mux daemon through its CLI to run commands, capture pane output, wait for exit codes, and follow program-status events. Use for terminal/automation tasks involving stitch-mux sessions or panes; never for remote shell access.
compatibility: Requires the `stitch-mux` CLI on PATH and a reachable local daemon (`stitch-mux ping`). No network account, token, or privileged hook.
---

# stitch-mux local automation

Use only the local `stitch-mux` CLI and its local socket/named pipe. There is no
remote API, no MCP server, and no multi-user path; do not invent one.

Before doing anything else, read `references/commands.md` from this skill
directory and run `stitch-mux ping`. Exit code 4 means the daemon is not
reachable; report that instead of starting a second daemon implicitly.

## Recipes

Run a short command in a new pane and adopt its exit status:

```sh
stitch-mux run -s task --wait -- sh -c 'exit 7'
# process exit status: 7
```

Create a durable session, send input, capture, then close the pane:

```sh
stitch-mux new -s build -- /bin/sh
PANE=$(stitch-mux ls | python -c 'import json,sys; print(json.load(sys.stdin)["sessions"][0]["panes"][0]["id"])')
stitch-mux send -t "$PANE" 'make test
'
stitch-mux send-keys -t "$PANE" ctrl+c
stitch-mux capture -t "$PANE" --lines 200 --text
stitch-mux close -t "$PANE"
```

Follow only the changes that matter, with a bounded run:

```sh
stitch-mux events --session build --timeout-ms 30000
```

## Rules

- Pane IDs are daemon-stable integers; never parse ANSI to infer state. Use
  `ls`, `capture`, and `events` JSON.
- `run --wait` and `wait` return the child's exit code. A timeout returns 1
  with `timed_out: true`; do not treat it as a child failure.
- `events` has no replay. Subscribe before starting the work you want to watch.
  A lagging subscriber receives a typed `busy` error; reconnect and resync from
  `ls`/`capture` rather than asking for history.
- `capture --text` prints plain rows; without it the command prints a stable
  JSON envelope. `--lines` is capped by the daemon (1000 rows).
- Detached panes keep running and can be placed back with `place-pane`;
  `close` is the explicit destructive operation.
- Windows and floating layers are daemon-owned; use `window-*` and
  `floating`/`float-pane`/`tile-pane` rather than local GUI geometry.
- New-window placement uses a default split; floating bounds are percentages
  placement for v0.1/v0.2.
- Keep logs local. `STITCH_MUX_LOG` overrides the local log path; never ship
  logs or endpoints to a remote sink.
