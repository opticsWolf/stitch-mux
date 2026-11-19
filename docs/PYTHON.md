# Python SDK and PySide6 adapter

The Python package is a maturin project. Rust owns the protocol connection on
one background thread per `Session`; Python receives immutable view dicts and
sends semantic commands. No Python code parses ANSI or encodes keys.

## Build and run

```sh
uv sync --extra test
uv run python -m stitch_mux.qt --session demo
```

For development against a checkout:

```sh
maturin develop --features pyo3/extension-module
QT_QPA_PLATFORM=offscreen pytest -q
```

The extension uses `abi3-py311`, so one wheel works on CPython 3.11+. PySide6
is an optional extra, not a dependency of the core SDK:

```sh
pip install stitch-mux[qt]
```

## API

```python
import stitch_mux

print(stitch_mux.connect())
print(stitch_mux.list_sessions())

session = stitch_mux.attach("demo", cols=100, rows=30)
try:
    view = session.view()
    for pane_id, pane in view["panes"].items():
        print(pane_id, pane["snapshot"]["title"])
    session.input_text("echo hello\n")
    session.key("left")
    session.resize(90, 28)
    print(session.capture(101, lines=200)["text"])
finally:
    session.detach()
```

`view()` returns the current `SessionView`: layout geometry, panes, cells,
styles, cursor, modes, activity, focus, connection state, and typed errors.
The Qt adapter polls it at a bounded rate and emits one batched signal per
update; fast bursts coalesce without blocking the Qt event loop.

## Qt adapter

`python/stitch_mux/qt.py` provides `CellCanvas`, `SessionPicker`,
`SessionWindow`, and a `Ctrl+P` command palette stub. It maps native keys to
the same `InputEvent` codes as egui and the terminal client. Closing or
detaching a window leaves the daemon's children alive.

## Packaging and licensing

- The Rust crates are `MIT OR Apache-2.0`, matching `stitch-pty`.
- PySide6 is **LGPL-3.0 / GPL / commercial**. The wheel does not vendor Qt;
  users install their own PySide6 distribution via the `qt` extra.
- If you redistribute a bundled Qt application, use dynamic linking and meet
  the LGPL obligations (notice, source availability for Qt modifications, and
  relinking), or obtain a commercial Qt license. Do not treat the Rust crate's
  dual license as covering Qt.
- Built extension artifacts (`*.pyd`, `*.so`, `*.dylib`) and `.venv/` are
  ignored by Git.
