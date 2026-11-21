"""Offscreen checks for the proper stitch-mux Qt demo.

The demo renders daemon snapshots, never PTY bytes, so the tests feed a plain
``SessionView``-shaped dict through the same conversion path as the live app.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from PySide6.QtCore import QEvent, Qt  # noqa: E402
from PySide6.QtGui import QKeyEvent  # noqa: E402
from PySide6.QtWidgets import QApplication, QScrollBar  # noqa: E402

from examples.python import qtdemo  # noqa: E402


def snapshot(seq: int, scrollback_len: int, text: str) -> dict:
    return {
        "seq": seq,
        "cols": 8,
        "rows": 1,
        "scrollback_len": scrollback_len,
        "title": "demo",
        "cells": [
            {
                "cells": [
                    {
                        "text": ch,
                        "fg": "red" if ch == "x" else "default",
                        "bg": {"rgb": [1, 2, 3]},
                        "attrs": 1,
                        "width": 1,
                        "link": None,
                    }
                    for ch in row
                ],
                "wrapped": False,
            }
            for row in [text.ljust(8)[:8]]
        ],
        "cursor": {"x": 0, "y": 0, "visible": True, "shape": "Block", "blink": False},
        "modes": {"alt_screen": False, "cursor_visible": True},
    }


def test_snapshot_colors_are_daemon_metadata() -> None:
    assert qtdemo.color_name("green") == "green"
    assert qtdemo.color_name({"rgb": [1, 2, 3]}) == "#010203"


def test_frame_builder_keeps_bounded_scroll_context() -> None:
    builder = qtdemo.FrameBuilder()
    first = builder.build(snapshot(1, 0, "oldline"))
    assert first.text_rows == ["oldline "]
    assert first.window_start == 0

    second = builder.build(snapshot(2, 1, "newline"))
    assert second.history == 1
    assert second.text_rows == ["oldline ", "newline "]
    assert second.window_start == 0

    third = builder.build(snapshot(3, 2, "latest!"))
    assert third.text_rows == ["oldline ", "newline ", "latest! "]
    # The visible row is authoritative even while local history lags behind.
    assert third.text_rows[-1].startswith("latest")


def test_terminal_view_paints_and_reports_semantic_keys() -> None:
    app = QApplication.instance() or QApplication([])
    bar = QScrollBar(Qt.Orientation.Vertical)
    view = qtdemo.TerminalView(qtdemo.Config(), 42, bar)
    view.resize(400, 120)
    keys: list[tuple[str, int, str | None]] = []
    view.key_event.connect(lambda code, mods, text: keys.append((code, mods, text)))
    view.set_frame(qtdemo.FrameBuilder().build(snapshot(1, 0, "hello   ")))
    app.processEvents()
    assert not view.grab().isNull()

    view.keyPressEvent(
        QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Left, Qt.KeyboardModifier.NoModifier)
    )
    view.keyPressEvent(
        QKeyEvent(
            QEvent.Type.KeyPress,
            Qt.Key.Key_A,
            Qt.KeyboardModifier.ControlModifier,
            "",
        )
    )
    view.keyPressEvent(
        QKeyEvent(QEvent.Type.KeyPress, ord("b"), Qt.KeyboardModifier.NoModifier, "b")
    )
    assert keys == [("left", 0, None), ("a", 4, None), ("b", 0, "b")]


def test_key_names_stay_toolkit_neutral() -> None:
    event = QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_F2, Qt.KeyboardModifier.NoModifier)
    assert qtdemo._key_name(event) == "f2"


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__]))
