# Terminal fidelity and link safety

The daemon owns the authoritative screen model. Frontends receive styled cells,
bounded scrollback, and metadata; they never parse ANSI or encode keys. This
document records what the v0.2 fidelity work supports and where its boundaries
are.

## Supported

- **Styles and geometry:** SGR colors/attributes, wide characters, cursor shape,
  alternate-screen transitions, and bounded viewport diffs.
- **Captures:** `text`, `cells`, `json`, `vt` (SGR-styled text), and `html`
  (escaped, inline-styled `<pre>` rows). `start` is absolute and `lines` is
  capped at 1000. `total_lines` gives clients an absolute row anchor.
- **Scrollback:** `STITCH_MUX_SCROLLBACK` bounds daemon history rows
  (100..20000, default 2000). Frontends keep their own bounded local overlay.
- **Selection/copy:** drag in the egui pane to select cells across the local
  scrollback overlay. `Ctrl+C` copies a non-empty selection (otherwise it is
  sent to the child), `Ctrl+Shift+A` selects all retained rows, `Esc` clears the
  selection, and the status-bar `Copy: drag` toggle controls copy-on-selection.
  Copying also publishes the PRIMARY selection on platforms that support it.
- **OSC 8 links:** a bounded mux-side scanner records the active URI for
  printable cells. Link changes publish a full snapshot so clients see the
  metadata immediately. The egui reference client makes linked cells clickable.
- **Shell integration:** OSC 133 prompt/command marks and OSC 1337 `CurrentDir=`
  become `ShellMarks` and cwd in snapshot/delta metadata. `prompt.previous` and
  `prompt.next` jump by absolute row, never by parsing screen text.
- **Program status:** OSC 7501 records feed pane/session activity summaries and
  a bounded local event stream. OSC 9/777 notifications and OSC 9;4 progress
  become current-value metadata; the egui status bar shows the focused pane's
  latest notification and progress. The daemon never raises a desktop
  notification itself.

## Link safety

`http://`, `https://`, and `mailto:` open directly. Any child-controlled custom
scheme opens only after an explicit confirmation dialog that shows the full
URI. Control characters in an OSC 8 URI are rejected. A resize clears the
absolute-cell link map because cell indices depend on the old width.

OSC 8 attribution tracks printable characters and simple newline/carriage-return
movement. If a read contains an unmodelled cursor-control sequence, link
assignment stops for the rest of that read rather than guessing a cell
position. This is deliberately conservative: no link should point at unrelated
text.

## Limits

- No sixel, Kitty image protocol, or other graphics support is implied.
- Underline styles are mapped to a simple attribute bit, not full conformance
  with every underline variant.
- There is no copy mode or buffer search in v0.2. Selection is a direct drag
  over the visible cells plus the frontend's bounded local scrollback, not a
  daemon-side mode.
- tmux control-mode compatibility is out of scope.
- Notification/progress records are current values, not an audit trail; a
  frontend that needs history must keep its own bounded projection. The
  daemon performs no desktop notification or other side effect.

Alternate-screen programs (vim, less, htop) remain covered by the M4 conformance
corpus: the daemon sends a fresh `PaneSnapshot` on an alternate-screen
transition and restores the shell screen on exit.
