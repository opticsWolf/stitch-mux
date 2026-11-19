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
from stitch_mux.qt import CellCanvas, SessionPicker, color_key, style_key  # noqa: E402


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
    canvas.keyPressEvent(
        QKeyEvent(
            QEvent.Type.KeyPress,
            ord("a"),
            Qt.KeyboardModifier.ControlModifier,
            "a",
        )
    )
    assert session.keys == [("left", 0)]
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


def test_picker_lists_sessions(app: QApplication) -> None:
    picker = SessionPicker(
        lambda: [
            {"name": "demo", "panes": [{"id": 101}], "attached_clients": 1},
        ]
    )
    picker.refresh()
    assert picker.list.count() == 1
    assert "demo" in picker.list.item(0).text()
