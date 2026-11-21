"""Offscreen Qt adapter tests.

They use a fake Rust session so the widget contract can be checked without a
daemon or a display server. CI sets QT_QPA_PLATFORM=offscreen.
"""

from __future__ import annotations

import os

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

from PySide6.QtCore import QEvent, Qt  # noqa: E402
from PySide6.QtGui import QKeyEvent  # noqa: E402
from PySide6.QtWidgets import QApplication  # noqa: E402
from stitch_mux.qt import (  # noqa: E402
    CellCanvas,
    CommandPalette,
    SessionPicker,
    SessionWindow,
    color_key,
    style_key,
)


class FakeSession:
    def __init__(self) -> None:
        self.text: list[str] = []
        self.keys: list[tuple[str, int]] = []
        self.resize_calls: list[tuple[int, int]] = []
        self.focus_calls: list[int] = []

    def view(self) -> dict:
        return {
            "connection": "Connected",
            "focus": 101,
            "layout": {
                "panes": [
                    {
                        "pane_id": 101,
                        "outer": {"x": 0, "y": 0, "cols": 20, "rows": 6},
                        "content": {"x": 1, "y": 1, "cols": 18, "rows": 4},
                    }
                ]
            },
            "panes": {
                "101": {
                    "snapshot": {
                        "cells": [
                            {
                                "cells": [
                                    {
                                        "text": "A",
                                        "fg": {"rgb": [1, 2, 3]},
                                        "bg": "default",
                                        "attrs": 1,
                                        "width": 1,
                                    }
                                ],
                                "wrapped": False,
                            }
                        ],
                        "cursor": {"x": 0, "y": 0, "visible": True},
                        "title": "sh",
                    }
                }
            },
        }

    def input_text(self, text: str) -> None:
        self.text.append(text)

    def input_bytes(self, data: bytes) -> None:
        self.text.append(data.decode())

    def key(self, code: str, modifiers: int = 0, text: str | None = None) -> None:
        self.keys.append((code, modifiers))

    def resize(self, cols: int, rows: int) -> None:
        self.resize_calls.append((cols, rows))

    def focus(self, pane_id: int) -> None:
        self.focus_calls.append(pane_id)

    def detach(self) -> None:
        return None


@pytest.fixture(scope="module")
def app() -> QApplication:
    return QApplication.instance() or QApplication([])


def test_style_keys_distinguish_truecolor_and_attributes() -> None:
    plain = {"fg": "default", "bg": "default", "attrs": 0}
    red = {"fg": {"rgb": [255, 0, 0]}, "bg": "default", "attrs": 0}
    bold = {"fg": "default", "bg": "default", "attrs": 1}
    assert color_key(plain) == ("name", "default")
    assert style_key(red)[0] == ("rgb", 255, 0, 0)
    assert len({style_key(plain), style_key(red), style_key(bold)}) == 3


def test_canvas_paints_a_structured_view(app: QApplication) -> None:
    canvas = CellCanvas()
    canvas.set_view(FakeSession().view())
    canvas.resize(400, 200)
    app.processEvents()
    pixmap = canvas.grab()
    assert not pixmap.isNull()
    assert pixmap.width() == canvas.width()
    assert "connection" not in canvas.view or canvas.view["connection"] == "Connected"


def test_canvas_maps_keys_and_focus(app: QApplication) -> None:
    session = FakeSession()
    canvas = CellCanvas()
    canvas.set_session(session)
    canvas.set_view(session.view())
    canvas.keyPressEvent(
        QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Left, Qt.KeyboardModifier.NoModifier)
    )
    # Ctrl chords carry no text event in Qt; they must reach the pane as a
    # semantic key so the daemon can encode them for the child.
    canvas.keyPressEvent(
        QKeyEvent(
            QEvent.Type.KeyPress,
            Qt.Key.Key_A,
            Qt.KeyboardModifier.ControlModifier,
            "",
        )
    )
    canvas.keyPressEvent(
        QKeyEvent(
            QEvent.Type.KeyPress,
            Qt.Key.Key_B,
            Qt.KeyboardModifier.AltModifier,
            "",
        )
    )
    assert session.keys == [("left", 0), ("a", 4), ("b", 2)]
    assert session.text == []

    plain = QKeyEvent(QEvent.Type.KeyPress, ord("b"), Qt.KeyboardModifier.NoModifier, "b")
    canvas.keyPressEvent(plain)
    assert session.text == ["b"]

    class Position:
        def x(self) -> float:
            return 10.0

        def y(self) -> float:
            return 10.0

    class MouseEvent:
        def position(self) -> Position:
            return Position()

    canvas.mousePressEvent(MouseEvent())
    assert session.focus_calls == [101]


def test_canvas_emits_bounded_resize(app: QApplication) -> None:
    session = FakeSession()
    canvas = CellCanvas()
    canvas.set_session(session)
    canvas.show()
    sizes: list[tuple[int, int]] = []
    canvas.resized.connect(lambda cols, rows: sizes.append((cols, rows)))
    canvas.resize(180, 90)
    app.processEvents()
    assert sizes and sizes[-1][0] > 0
    assert session.resize_calls == []


def test_command_palette_filters_named_intents(app: QApplication) -> None:
    palette = CommandPalette()
    palette._populate("detach")
    assert palette.results.count() == 1
    selected: list[str] = []
    palette.command_selected.connect(selected.append)
    palette._activate(palette.results.item(0))
    assert selected == ["Detach session"]


def test_canvas_requests_palette_on_ctrl_p(app: QApplication) -> None:
    canvas = CellCanvas()
    requests: list[bool] = []
    canvas.command_requested.connect(lambda: requests.append(True))
    canvas.keyPressEvent(
        QKeyEvent(
            QEvent.Type.KeyPress,
            ord("P"),
            Qt.KeyboardModifier.ControlModifier,
            "",
        )
    )
    assert requests == [True]


def test_session_window_detach_is_idempotent(
    app: QApplication, monkeypatch: pytest.MonkeyPatch
) -> None:
    class StoppedSession(FakeSession):
        def detach(self) -> None:
            raise RuntimeError("session thread already stopped")

    monkeypatch.setattr("stitch_mux.qt.attach", lambda *a, **k: StoppedSession())
    window = SessionWindow("demo")
    try:
        window._detach_session()
        window._detach_session()
        assert window.session is None
    finally:
        window.close()


def test_picker_lists_sessions(app: QApplication) -> None:
    picker = SessionPicker(
        lambda: [
            {"name": "demo", "panes": [{"id": 101}], "attached_clients": 1},
        ]
    )
    picker.refresh()
    assert picker.list.count() == 1
    assert "demo" in picker.list.item(0).text()


def test_canvas_scrolls_local_history_with_the_wheel(app: QApplication) -> None:
    canvas = CellCanvas()
    canvas._scroll[101] = {
        "seq": 4,
        "scrollback_len": 12,
        "visible": [{"cells": []}],
        "history": [{"cells": []}] * 6,
        "offset": 0,
    }
    assert canvas._scroll_pane(101, 1) is True
    assert canvas._scroll[101]["offset"] == 3
    assert canvas._scroll_pane(101, 1) is True
    assert canvas._scroll[101]["offset"] == 6
    assert canvas._scroll_pane(101, -1) is True
    assert canvas._scroll[101]["offset"] == 3

    snapshot = {"cells": [{"cells": []}, {"cells": []}], "seq": 4}
    window = canvas.window_for(101, snapshot)
    assert len(window) == 2

    # A pane without history stays at the bottom instead of scrolling.
    canvas._scroll[202] = {
        "seq": 1,
        "scrollback_len": 0,
        "visible": [],
        "history": [],
        "offset": 0,
    }
    assert canvas._scroll_pane(202, 1) is False
    assert canvas._scroll[202]["offset"] == 0
