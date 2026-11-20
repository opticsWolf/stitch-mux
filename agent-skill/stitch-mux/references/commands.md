# stitch-mux CLI quick reference

Every control command writes one stable JSON envelope to stdout unless noted.
`--json` is accepted globally and makes that explicit. Use `--socket` or
`STITCH_MUX_SOCKET` to target a local endpoint.

## Exit statuses

| Code | Meaning |
|---|---|
| 0 | Command succeeded. For `run --wait`/`wait`, the child's exit code (0–255). |
| 1 | CLI/protocol error, typed daemon error, or a timed-out wait. |
| 2 | Invalid arguments (clap). |
| 4 | Local daemon not reachable. |

## Commands

| Command | Purpose |
|---|---|
| `ping` | Print `HelloAck`; verify the local daemon before automation. |
| `ls --json` | Session/pane IDs, activity, geometry, and program status. |
| `new -s NAME -- PROGRAM [ARGS...]` | Create a durable session; `--ensure`, `--cwd`, `--env KEY=VALUE`. |
| `run -s NAME [--target PANE] --wait -- PROGRAM` | Spawn a pane in a split; `--wait` adopts the child status, `--keep-open` keeps the pane after exit. New-window placement is unsupported. |
| `wait [--target PANE\|--session NAME] [--timeout-ms N]` | Wait for pane/session exit; returns the child's code. |
| `send -t PANE TEXT` | Write literal text to a pane. |
| `send-keys -t PANE [--hex HEX] [KEY...]` | Encode named keys through `stitch-pty` (`enter`, `ctrl+c`, `f1`), or send raw bytes. |
| `capture -t PANE [--start N] [--lines N] [--format text\|json\|cells] [--text]` | Bounded scrollback; `--text` prints rows only. Cap: 1000 rows. |
| `split`, `close`, `zoom`, `rename`, `kill`, `clients`, `detach-client`, `shutdown` | Layout/client/session control. |
| `window-new [-s NAME] [--name NAME]`, `window-close -w ID`, `window-rename -w ID --name NAME`, `window-move -w ID --index N`, `window-focus [--window ID|--next|--previous]` | Window/tab control. |
| `float-pane -t PANE [--bounds X,Y,W,H]`, `tile-pane -t PANE -w WINDOW [--anchor PANE]`, `floating -l LAYER [--bounds X,Y,W,H] [--z N] [--minimize|--restore]` | Bounded floating-layer control. |
| `run -s NAME --window ID [--floating] [--bounds X,Y,W,H] -- PROGRAM` | Spawn into a window or floating layer. |
| `detach-pane -t PANE` | Remove a pane from the window without stopping its child; it stays in `ls` as `detached`. |
| `place-pane -t PANE [--anchor PANE] [--axis columns\|rows] [--ratio N]` | Put a detached pane back into the window. |
| `claim-pane -t PANE [--mode take\|request]`, `release-pane -t PANE` | Local size-owner preference; the smallest attached client remains the default. |
| `events [--session NAME] [--all] [--timeout-ms N]` | NDJSON stream of typed local events with monotonic `seq`. |

## Event kinds shipped in v0.2

- `session_list` — initial snapshot; always delivered, even to unscoped
  subscribers.
- `program_status_changed` — latest OSC 7501 record for a pane.
- `activity_changed` — aggregate working/blocked/done/error counts.
- `pane_created`, `pane_exited`, `pane_placement_changed`,
  `pane_owner_changed` — pane lifecycle and local ownership.
- `window_created`, `window_closed`, `window_renamed`, `window_moved`,
  `window_focused`, `pane_floated`, `pane_tiled`, `floating_changed` —
  workspace/layer changes.
- `event_gap` — the pane's status stream dropped records locally.

There is no replay log. Reconnect and rebuild state from `ls` and `capture`.
