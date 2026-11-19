# Frontend parity checklist

All adapters consume the same protocol and `SessionView`; none parses ANSI or
owns PTY state. This table is the v0.1 baseline, not a claim of full tmux
parity.

| Capability | egui reference | ANSI terminal | PySide6 | GPUI spike |
|---|---|---|---|---|
| Structured pane cells and layout | Yes | Geometry only | Yes | `SpikeFrame` |
| Server-composed ANSI frames | n/a | Yes | n/a | n/a |
| Per-client focus | Yes | Yes (`Ctrl-]` arrows) | Yes (click/focus API) | Geometry only |
| Attach/detach/reattach | Yes | Yes | Yes | n/a (spike) |
| Resize via smallest-client policy | Yes | Yes | Yes | n/a |
| Activity/status hooks | Status bar | Status in composed border | Status label | n/a |
| Command palette | Status-bar buttons | `Ctrl-]` prefixes | `Ctrl+P` stub | n/a |
| Native selection/copy | Planned v0.2 | n/a | Planned v0.2 | n/a |
| OSC 8 links | Planned v0.2 | Planned v0.2 | Planned v0.2 | n/a |
| Scrollback/search | Capture only | n/a | Capture only | n/a |

## Baseline checks

- [x] Cells are drawn from style runs; continuation cells are skipped.
- [x] Bold/italic/underline, indexed, and truecolor styles survive the
      structured and ANSI projections.
- [x] Wide glyphs occupy two cells in the server compositor and Qt painter.
- [x] Focused cursor comes from the authoritative snapshot.
- [x] Alternate-screen changes do not leak into the GUI cell model.
- [x] Lost sessions are shown explicitly; no adapter silently recreates them.
- [x] Second toolkit (PySide6) required no protocol changes.
- [ ] Native mouse selection, OSC 8 links, and prompt jumps (v0.2).
- [ ] Accessibility/IME audit and production Qt theming (v0.2).

GPUI scope and the gate for a real dependency are in
[GPUI-SPIKE.md](GPUI-SPIKE.md).
