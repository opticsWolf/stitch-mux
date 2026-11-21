# GPUI spike (optional, not a v0.1 gate)

`examples/gpui/` proves the frontend boundary without making GPUI a
release dependency:

```rust
let frame = stitch_mux_gui_gpui::from_view(&session_view);
```

`SpikeFrame` contains only cell geometry and focus. The daemon, protocol, and
toolkit-independent client are unchanged; this is the same `SessionView` egui
and PySide6 consume. Enable the `gpui` feature for the feature-gated
`window_title` hook:

```sh
cargo check -p stitch-mux-gpui --features gpui
```

## Why no GPUI dependency yet

GPUI 0.2.x is pre-1.0 with frequent API churn and sparse standalone
documentation. Pinning it now would add a large dependency to the default
workspace and turn an experimental adapter into a release blocker. The M5 goal
is to prove that a second toolkit can attach to one session without protocol
changes; the Qt/PySide6 adapter provides that proof with a stable dependency.

## Gate for a real GPUI window

Add a pinned `gpui` dependency and a window/pane renderer only when:

1. a standalone GPUI app builds on all three target platforms;
2. it consumes `SessionView` directly and never parses ANSI or talks to PTYs;
3. the pinned version has an upgrade owner and a documented migration path;
4. the adapter stays out of `main` behind a feature flag until v0.2.

Until then this crate is a boundary spike, not a supported frontend.
