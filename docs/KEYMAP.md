# Keymaps and actions

All input surfaces use the same daemon-owned vocabulary: an action name from
`stitch-mux actions`, a key sequence from `stitch-mux get-keymap`, and the
built-in dispatcher in `stitch-mux-core`. No frontend hard-codes what a
shortcut means, and there is no Lua or plugin hook.

## Built-in base layer

| Keys | Action | Arguments |
|---|---|---|
| `C-b` | internal prefix | — |
| `C-b %` | `pane.split` | `axis=columns`, `ratio_bps=5000` |
| `C-b "` | `pane.split` | `axis=rows`, `ratio_bps=5000` |
| `C-b Left/Right/Up/Down` | `pane.focus-*` | — |
| `C-b x` | `pane.close` | — |
| `C-b z` | `pane.zoom-toggle` | — |
| `C-b d` | `client.detach` | handled by the frontend |
| `C-b ?` | `help.which-key` | handled by the frontend |
| `C-b r` | `mode.resize` | — |

`resize` is an exclusive mode: `Left/Right/Up/Down` map to `pane.resize-*`, and
`Esc` or `C-b` runs `mode.exit`. Pressing the prefix twice sends the literal
`C-b` bytes through to the child instead of leaving a stuck sequence.

## Config format

Set `STITCH_MUX_KEYMAP=/path/to/keymap.json` before starting the daemon, or
call `stitch-mux reload-keymap --path /path/to/keymap.json` later. The file is
plain JSON:

```json
{
  "modes": [
    { "name": "resize", "exclusive": true }
  ],
  "bindings": [
    {
      "keys": ["C-b", "%"],
      "action": "pane.focus-left",
      "mode": null,
      "args": null
    },
    {
      "keys": ["C-b", "n"],
      "action": "pane.split",
      "args": { "axis": "rows", "ratio_bps": 4000 }
    }
  ]
}
```

Rules:

- A user binding with the same mode and sequence replaces the built-in entry;
  its `source` becomes `user` in `get-keymap`.
- `mode: null` is the base layer. A named mode must be listed in `modes`.
- Arguments are validated against the action's schema before the binding is
  installed. Invalid files are logged locally and the built-in table is kept.
- Key names are canonical: `ctrl-b`, `Control-b`, and `C-b` mean the same key;
  `Up`, `Escape`/`Esc`, `F1`–`F12`, and literal single characters are accepted.
- Hints are bounded (16 entries) and report the binding's source.

## Events and reload

`keymap_changed` and `actions_changed` are local events with monotonic `seq`.
Subscribers refetch `get-keymap`/`actions`; there is no replay. Reload never
restarts the daemon or drops a session.
