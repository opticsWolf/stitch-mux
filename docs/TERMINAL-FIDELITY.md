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
- **Smooth scrolling:** the egui frontend accumulates sub-row wheel deltas and
  paints with a fractional pixel shift plus one extra clipped row. Selection
  and pointer hit-testing use the same shift, so clicks stay aligned.
- **Selection/copy:** drag in the egui pane to select cells across the local
  scrollback overlay. `Ctrl+C` copies a non-empty selection (otherwise it is
  sent to the child), `Ctrl+Shift+A` selects all retained rows, `Esc` clears the
  selection, and the status-bar `Copy: drag` toggle controls copy-on-selection.
  Copying also publishes the PRIMARY selection on platforms that support it.
- **OSC 8 links:** Stitch PTY attaches hyperlinks to cells as it draws them.
  Snapshots, row deltas, and cell captures carry the same URI. The egui
  reference client makes linked cells clickable.
- **Shell integration:** OSC 133 prompt/command marks use the parser's row at
  dispatch, with rows rebased when scrollback is evicted. Alternate-screen
  marks are ignored. OSC 1337 `CurrentDir=` also contributes cwd metadata.
  `prompt.previous` and `prompt.next` use retained row numbers.
- **Program status:** OSC 7501 records feed pane/session activity summaries and
  a bounded local event stream. OSC 9/777 notifications and OSC 9;4 progress
  become current-value metadata; the egui status bar shows the focused pane's
  latest notification and progress. The daemon never raises a desktop
  notification itself.

## Link safety

`http://`, `https://`, and `mailto:` open directly. Any child-controlled custom
scheme opens only after an explicit confirmation dialog that shows the full
URI. The parser rejects control characters in an OSC 8 URI. Link metadata
moves with terminal cells through scrolling and reflow and is removed when
unlinked text overwrites those cells.

## Limits

- No sixel, Kitty image protocol, or other graphics support is implied.
- Underline styles are mapped to a simple attribute bit, not full conformance
  with every underline variant.
- Width changes reflow the primary screen and scrollback. Pane prompt marks
  are cleared when reflow changes their coordinate space. Resize publishes a
  fresh snapshot.
- There is no copy mode or buffer search in v0.2. Selection is a direct drag
  over the visible cells plus the frontend's bounded local scrollback, not a
  daemon-side mode.
- tmux control-mode compatibility is out of scope.
- Notification/progress records are current values, not an audit trail; a
  frontend that needs history must keep its own bounded projection. The
  daemon performs no desktop notification or other side effect.

Alternate-screen programs (vim, less, htop) remain covered by the M4 conformance
corpus and by `tests/alternate_screen.rs`, which drives real OSC 1049
transitions through a PTY and asserts that the shell screen is restored without
leaking alternate-screen rows.
