"""PySide6 reference adapter for stitch-mux.

The Qt side never sees PTY bytes. Rust publishes a structured ``SessionView``
dict; this module paints its cells and forwards native events as semantic
commands. Importing this module requires PySide6 (``pip install stitch-mux[qt]``).
"""

from __future__ import annotations

import argparse
import sys
from typing import Any, Protocol

from PySide6.QtCore import QObject, QPointF, QRectF, Qt, QThread, Signal, Slot
from PySide6.QtGui import QColor, QFont, QFontMetricsF, QKeyEvent, QPainter, QPaintEvent, QPen
from PySide6.QtWidgets import (
    QApplication,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from . import attach, list_sessions

#: Rows of local scrollback kept per pane for the wheel. The daemon owns the
#: authoritative screen; this adapter only replays what it already received.
MAX_SCROLLBACK = 1000

_ANSI_COLORS = [
    "#000000",
    "#cd3131",
    "#0dbc79",
    "#e5e510",
    "#2472c8",
    "#bc3fbc",
    "#11a8cd",
    "#e5e5e5",
    "#555555",
    "#f14c4c",
    "#23d18b",
    "#f5f543",
    "#3b8eea",
    "#d670d6",
    "#29b8db",
    "#ffffff",
]


class SessionProtocol(Protocol):
    """The subset of the Rust session used by the Qt adapter."""

    def view(self) -> dict[str, Any]: ...
    def input_text(self, text: str) -> None: ...
    def input_bytes(self, data: bytes) -> None: ...
    def key(self, code: str, modifiers: int = 0, text: str | None = None) -> None: ...
    def resize(self, cols: int, rows: int) -> None: ...
    def focus(self, pane_id: int) -> None: ...
    def detach(self) -> None: ...


def color_key(value: Any) -> tuple[Any, ...]:
    """Normalize a protocol Color into a hashable style-run key."""
    if isinstance(value, str):
        return ("name", value)
    if isinstance(value, dict):
        if "indexed" in value:
            return ("indexed", int(value["indexed"]))
        if "rgb" in value:
            red, green, blue = value["rgb"]
            return ("rgb", int(red), int(green), int(blue))
    return ("name", "default")


def color_from_key(key: tuple[Any, ...], fallback: str) -> QColor:
    kind = key[0]
    if kind == "indexed" and 0 <= int(key[1]) < len(_ANSI_COLORS):
        return QColor(_ANSI_COLORS[int(key[1])])
    if kind == "rgb":
        return QColor(int(key[1]), int(key[2]), int(key[3]))
    if kind == "name" and key[1] != "default":
        value = str(key[1])
        return QColor(value if value.startswith("#") else f"#{value}")
    return QColor(fallback)


def style_key(cell: dict[str, Any]) -> tuple[Any, ...]:
    attrs = cell.get("attrs", 0)
    if isinstance(attrs, dict):
        attrs = int(attrs.get("bits", 0))
    return (color_key(cell.get("fg", "default")), color_key(cell.get("bg", "default")), int(attrs))


def apply_style(
    painter: QPainter,
    key: tuple[Any, ...],
    fallback_fg: str,
    fallback_bg: str,
) -> None:
    fg, bg, attrs = key
    painter.setPen(QPen(color_from_key(fg, fallback_fg)))
    painter.setBackground(color_from_key(bg, fallback_bg))
    font = painter.font()
    font.setBold(bool(attrs & 1))
    font.setItalic(bool(attrs & 4))
    font.setUnderline(bool(attrs & 8))
    painter.setFont(font)


class ViewSignaler(QThread):
    """Poll the Rust snapshot and deliver one batched update to Qt."""

    view_updated = Signal(dict)
    connection_lost = Signal(str)

    def __init__(
        self,
        session: SessionProtocol,
        interval_ms: int = 33,
        parent: QObject | None = None,
    ):
        super().__init__(parent)
        self._session = session
        self._interval_ms = max(1, interval_ms)
        self._reported_lost = False

    def run(self) -> None:  # noqa: D102 - Qt thread entry point
        while not self.isInterruptionRequested():
            try:
                view = self._session.view()
            except RuntimeError as err:
                self.connection_lost.emit(str(err))
                return
            self.view_updated.emit(view)
            if view.get("connection") == "Lost":
                message = view.get("error") or "connection lost"
                if not self._reported_lost:
                    self._reported_lost = True
                    self.connection_lost.emit(str(message))
                return
            self.msleep(self._interval_ms)


def modified_key_code(key: int) -> str | None:
    """Return the semantic key for a Ctrl/Alt chord.

    Qt suppresses ``QKeyEvent.text()`` while Control/Alt/Meta is held, so
    ``Ctrl-C`` and ``Alt-b`` never reached the pane. The daemon owns key
    encoding, so forward the key plus its modifiers instead of raw bytes.
    """
    if Qt.Key.Key_A <= key <= Qt.Key.Key_Z:
        return chr(ord("a") + int(key) - int(Qt.Key.Key_A))
    if Qt.Key.Key_0 <= key <= Qt.Key.Key_9:
        return chr(ord("0") + int(key) - int(Qt.Key.Key_0))
    # Some key synthesizers (and QTest) use the lowercase ASCII code.
    if ord("a") <= key <= ord("z") or ord("0") <= key <= ord("9"):
        return chr(key)
    punctuation = {
        Qt.Key.Key_Space: " ",
        Qt.Key.Key_Minus: "-",
        Qt.Key.Key_Equal: "=",
        Qt.Key.Key_BracketLeft: "[",
        Qt.Key.Key_BracketRight: "]",
        Qt.Key.Key_Backslash: "\\",
        Qt.Key.Key_Semicolon: ";",
        Qt.Key.Key_Apostrophe: "'",
        Qt.Key.Key_Comma: ",",
        Qt.Key.Key_Period: ".",
        Qt.Key.Key_Slash: "/",
        Qt.Key.Key_QuoteLeft: "`",
    }
    return punctuation.get(key)


class CellCanvas(QWidget):
    """Paints rows and style runs from a structured SessionView."""

    resized = Signal(int, int)
    command_requested = Signal()

    def __init__(self, parent: QWidget | None = None):
        super().__init__(parent)
        self.view: dict[str, Any] = {}
        self.session: SessionProtocol | None = None
        self.cell_width = 9.0
        self.cell_height = 18.0
        self._last_size = (0, 0)
        self._scroll: dict[int, dict[str, Any]] = {}
        font = QFont("Cascadia Mono, DejaVu Sans Mono, Menlo, Consolas, monospace")
        font.setStyleHint(QFont.StyleHint.TypeWriter)
        font.setPointSize(12)
        self.setFont(font)
        self.setAutoFillBackground(True)
        self.setAttribute(Qt.WidgetAttribute.WA_InputMethodEnabled, True)
        metrics = QFontMetricsF(font)
        self.cell_width = max(1.0, metrics.horizontalAdvance("M"))
        self.cell_height = max(1.0, metrics.height())
        self.setMinimumSize(int(self.cell_width * 8), int(self.cell_height * 4))

    def set_session(self, session: SessionProtocol) -> None:
        self.session = session

    @Slot(dict)
    def set_view(self, view: dict[str, Any]) -> None:
        self.view = view
        self._sync_scroll(view)
        self.update()

    def _sync_scroll(self, view: dict[str, Any]) -> None:
        """Keep a bounded history of rows that left each pane's viewport."""
        panes = view.get("panes") or {}
        live = {int(key) for key in panes}
        self._scroll = {
            pane_id: state for pane_id, state in self._scroll.items() if pane_id in live
        }
        for key, pane in panes.items():
            pane_id = int(key)
            snapshot = pane.get("snapshot") or {}
            cells = snapshot.get("cells") or []
            if not cells:
                continue
            seq = int(snapshot.get("seq") or 0)
            scrollback = int(snapshot.get("scrollback_len") or 0)
            state = self._scroll.get(pane_id)
            if (
                state is None
                or seq < state["seq"]
                or scrollback < state["scrollback_len"]
            ):
                self._scroll[pane_id] = {
                    "seq": seq,
                    "scrollback_len": scrollback,
                    "visible": cells,
                    "history": [],
                    "offset": 0,
                }
                continue
            if seq == state["seq"]:
                continue
            scrolled = max(0, scrollback - state["scrollback_len"])
            state["history"].extend(state["visible"][:scrolled])
            del state["history"][:-MAX_SCROLLBACK]
            state["seq"] = seq
            state["scrollback_len"] = scrollback
            state["visible"] = cells
            state["offset"] = min(state["offset"], len(state["history"]))

    def window_for(self, pane_id: int, snapshot: dict[str, Any]) -> list[dict[str, Any]]:
        """Rows to paint: history above the viewport when scrolled up."""
        rows = snapshot.get("cells") or []
        height = len(rows)
        state = self._scroll.get(pane_id)
        if state is None or state["offset"] == 0:
            return rows
        offset = min(state["offset"], len(state["history"]))
        combined = list(state["history"][-offset:]) + list(rows)
        end = max(0, len(combined) - offset)
        return combined[max(0, end - height) : end]

    def _pane_at(self, position: QPointF) -> int | None:
        x = position.x() / self.cell_width
        y = position.y() / self.cell_height
        for geometry, _pane in self._pane_views():
            outer = geometry.get("outer") or {}
            left = float(outer.get("x", 0))
            top = float(outer.get("y", 0))
            if (
                left <= x < left + float(outer.get("cols", 0))
                and top <= y < top + float(outer.get("rows", 0))
            ):
                return int(geometry["pane_id"])
        return None

    def _scroll_pane(self, pane_id: int, direction: int) -> bool:
        state = self._scroll.get(pane_id)
        if state is None or not state["history"]:
            return False
        limit = len(state["history"])
        # Three rows per wheel notch, matching common terminal defaults.
        step = 3 if direction > 0 else -3
        state["offset"] = max(0, min(limit, state["offset"] + step))
        self.update()
        return True

    def _pane_views(self) -> list[tuple[dict[str, Any], dict[str, Any]]]:
        layout = self.view.get("layout") or {}
        panes = self.view.get("panes") or {}
        result = []
        for geometry in layout.get("panes") or []:
            pane_id = str(geometry.get("pane_id"))
            if pane_id in panes:
                result.append((geometry, panes[pane_id]))
        return result

    def paintEvent(self, _event: QPaintEvent) -> None:  # noqa: N802 - Qt API
        painter = QPainter(self)
        painter.fillRect(self.rect(), QColor("#0b1020"))
        focus = self.view.get("focus")
        for geometry, pane in self._pane_views():
            outer = geometry.get("outer") or {}
            content = geometry.get("content") or {}
            active = geometry.get("pane_id") == focus
            x = float(outer.get("x", 0)) * self.cell_width
            y = float(outer.get("y", 0)) * self.cell_height
            width = float(outer.get("cols", 0)) * self.cell_width
            height = float(outer.get("rows", 0)) * self.cell_height
            painter.setPen(QPen(QColor("#5ccfe6" if active else "#33405a"), 2 if active else 1))
            painter.drawRect(int(x), int(y), int(width), int(height))
            self._paint_rows(painter, geometry, pane, content, active)

        connection = self.view.get("connection", "Disconnected")
        if connection != "Connected":
            painter.setPen(QColor("#ffcc66"))
            painter.drawText(8, self.height() - 6, f"connection: {connection}")
        painter.end()

    def _paint_rows(
        self,
        painter: QPainter,
        geometry: dict[str, Any],
        pane: dict[str, Any],
        content: dict[str, Any],
        active: bool,
    ) -> None:
        snapshot = pane.get("snapshot") or {}
        origin_x = float(content.get("x", 0)) * self.cell_width
        origin_y = float(content.get("y", 0)) * self.cell_height
        clip = QRectF(
            origin_x,
            origin_y,
            float(content.get("cols", 0)) * self.cell_width,
            float(content.get("rows", 0)) * self.cell_height,
        )
        painter.save()
        painter.setClipRect(clip)
        pane_id = int(geometry.get("pane_id") or 0)
        for row_index, row in enumerate(self.window_for(pane_id, snapshot)):
            cells = row.get("cells") or []
            runs: list[tuple[tuple[Any, ...], str, int]] = []
            run_x = 0
            current_key: tuple[Any, ...] | None = None
            text = ""
            for col_index, cell in enumerate(cells):
                if cell.get("width") == 0:
                    continue
                key = style_key(cell)
                glyph = cell.get("text") or " "
                if current_key is None:
                    current_key = key
                    run_x = col_index
                    text = glyph
                elif key == current_key:
                    text += glyph
                else:
                    runs.append((current_key, text, run_x))
                    current_key = key
                    run_x = col_index
                    text = glyph
            if current_key is not None:
                runs.append((current_key, text, run_x))
            y = origin_y + row_index * self.cell_height + self.cell_height * 0.8
            for key, run_text, col in runs:
                apply_style(painter, key, "#d7dae0", "#0b1020")
                painter.drawText(int(origin_x + col * self.cell_width), int(y), run_text)

        painter.restore()
        if not active or self._scroll.get(pane_id, {}).get("offset", 0) > 0:
            # While scrolled up the cursor position no longer matches the
            # painted rows, so hide it instead of drawing at a stale spot.
            return
        cursor = snapshot.get("cursor") or {}
        if not cursor.get("visible", False):
            return
        x = origin_x + float(cursor.get("x", 0)) * self.cell_width
        y = origin_y + float(cursor.get("y", 0)) * self.cell_height
        painter.fillRect(
            int(x),
            int(y),
            max(2, int(self.cell_width)),
            max(2, int(self.cell_height)),
            QColor("#7ee0ff"),
        )

    def keyPressEvent(self, event: QKeyEvent) -> None:  # noqa: N802 - Qt API
        modifiers = event.modifiers()
        if (
            event.key() == Qt.Key.Key_P
            and modifiers & Qt.KeyboardModifier.ControlModifier
        ):
            self.command_requested.emit()
            return
        if self.session is None:
            return
        bits = 0
        if modifiers & Qt.KeyboardModifier.ShiftModifier:
            bits |= 1
        if modifiers & Qt.KeyboardModifier.AltModifier:
            bits |= 2
        if modifiers & Qt.KeyboardModifier.ControlModifier:
            bits |= 4
        if modifiers & Qt.KeyboardModifier.MetaModifier:
            bits |= 8
        key = event.key()
        special = {
            Qt.Key.Key_Return: "enter",
            Qt.Key.Key_Enter: "enter",
            Qt.Key.Key_Tab: "tab",
            Qt.Key.Key_Backtab: "tab",
            Qt.Key.Key_Backspace: "backspace",
            Qt.Key.Key_Escape: "esc",
            Qt.Key.Key_Left: "left",
            Qt.Key.Key_Right: "right",
            Qt.Key.Key_Up: "up",
            Qt.Key.Key_Down: "down",
            Qt.Key.Key_Home: "home",
            Qt.Key.Key_End: "end",
            Qt.Key.Key_PageUp: "pageup",
            Qt.Key.Key_PageDown: "pagedown",
            Qt.Key.Key_Insert: "insert",
            Qt.Key.Key_Delete: "delete",
        }
        if key in special:
            if key == Qt.Key.Key_Backtab:
                bits |= 1
            self.session.key(special[key], bits)
            return
        if Qt.Key.Key_F1 <= key <= Qt.Key.Key_F35:
            self.session.key(f"f{key - Qt.Key.Key_F1 + 1}", bits)
            return
        reserved = (
            Qt.KeyboardModifier.ControlModifier
            | Qt.KeyboardModifier.AltModifier
            | Qt.KeyboardModifier.MetaModifier
        )
        if modifiers & reserved:
            code = modified_key_code(key)
            if code is not None:
                self.session.key(code, bits)
            return
        text = event.text()
        if text:
            self.session.input_text(text)

    def inputMethodEvent(self, event: Any) -> None:  # noqa: N802 - Qt API
        if self.session is not None and event.commitString():
            self.session.input_text(event.commitString())

    def mousePressEvent(self, event: Any) -> None:  # noqa: N802 - Qt API
        if self.session is None:
            return
        pane_id = self._pane_at(event.position())
        if pane_id is not None:
            self.session.focus(pane_id)

    def wheelEvent(self, event: Any) -> None:  # noqa: N802 - Qt API
        if self.session is None:
            return
        delta = event.angleDelta().y()
        if delta == 0:
            return
        pane_id = self._pane_at(event.position())
        if pane_id is None:
            return
        self._scroll_pane(pane_id, 1 if delta > 0 else -1)
        event.accept()

    def resizeEvent(self, event: Any) -> None:  # noqa: N802 - Qt API
        cols = max(1, int(event.size().width() / self.cell_width))
        rows = max(1, int(event.size().height() / self.cell_height))
        if (cols, rows) != self._last_size:
            self._last_size = (cols, rows)
            self.resized.emit(cols, rows)


class SessionPicker(QWidget):
    """Simple session list using the same control SDK as the CLI."""

    session_selected = Signal(str)

    def __init__(self, lister: Any = None, parent: QWidget | None = None):
        super().__init__(parent)
        self._lister = lister or list_sessions
        self.windows: list[QWidget] = []
        self.list = QListWidget()
        connect = QPushButton("Attach")
        refresh = QPushButton("Refresh")
        connect.clicked.connect(self._emit_selected)
        refresh.clicked.connect(self.refresh)
        layout = QVBoxLayout(self)
        layout.addWidget(self.list)
        buttons = QHBoxLayout()
        buttons.addWidget(refresh)
        buttons.addWidget(connect)
        layout.addLayout(buttons)

    @Slot()
    def refresh(self) -> None:
        self.list.clear()
        for session in self._lister():
            name = session.get("name", "")
            panes = len(session.get("panes") or [])
            clients = session.get("attached_clients", 0)
            self.list.addItem(f"{name} — {panes} pane(s), {clients} client(s)")

    @Slot()
    def _emit_selected(self) -> None:
        item = self.list.currentItem()
        if item is not None:
            self.session_selected.emit(item.text().split(" — ", 1)[0])


class CommandPalette(QWidget):
    """Stub command palette: named intents over the existing client API."""

    command_selected = Signal(str)

    ACTIONS = ("Focus first pane", "Detach session", "Refresh view")

    def __init__(self, parent: QWidget | None = None):
        super().__init__(parent)
        self.search = QLineEdit()
        self.search.setPlaceholderText("Run a command…")
        self.results = QListWidget()
        self._populate("")
        self.search.textChanged.connect(self._populate)
        self.results.itemActivated.connect(self._activate)
        layout = QVBoxLayout(self)
        layout.addWidget(self.search)
        layout.addWidget(self.results)
        self.setWindowTitle("stitch-mux commands")
        self.resize(320, 220)

    def _populate(self, query: str) -> None:
        self.results.clear()
        for action in self.ACTIONS:
            if query.lower() in action.lower():
                self.results.addItem(action)

    def _activate(self, item: Any) -> None:
        self.command_selected.emit(item.text())
        self.close()


class SessionWindow(QWidget):
    """Attaches, paints, reports loss, and never silently recreates a session."""

    def __init__(self, session_name: str, socket: str | None = None, parent: QWidget | None = None):
        super().__init__(parent)
        self.session_name = session_name
        self.socket = socket
        self.session: SessionProtocol | None = None
        self.worker: ViewSignaler | None = None
        self.canvas = CellCanvas()
        self.palette = CommandPalette(self)
        self.status = QLabel("connecting…")
        self.lost = QLabel("Session lost — the daemon did not restore it.")
        self.lost.setStyleSheet("color: #ffcc66;")
        self.lost.hide()
        retry = QPushButton("Retry attach")
        back = QPushButton("Back to picker")
        retry.clicked.connect(self.retry)
        back.clicked.connect(self.close)
        lost_layout = QHBoxLayout()
        lost_layout.addWidget(self.lost)
        lost_layout.addWidget(retry)
        lost_layout.addWidget(back)
        layout = QVBoxLayout(self)
        layout.addWidget(self.canvas)
        layout.addLayout(lost_layout)
        layout.addWidget(self.status)
        self.setWindowTitle(f"stitch-mux — {session_name}")
        self.resize(800, 500)
        self.canvas.resized.connect(self._on_resize)
        self.canvas.command_requested.connect(self.palette.show)
        self.palette.command_selected.connect(self._run_command)
        self.attach()

    def attach(self) -> None:
        if self.worker is not None:
            self.worker.requestInterruption()
            self.worker.wait(500)
            self.worker = None
        self.session = attach(self.session_name, socket=self.socket)
        self.canvas.set_session(self.session)
        self.worker = ViewSignaler(self.session, parent=self)
        self.worker.view_updated.connect(self.on_view)
        self.worker.connection_lost.connect(self.on_lost)
        self.worker.start()
        self.lost.hide()

    @Slot(dict)
    def on_view(self, view: dict[str, Any]) -> None:
        self.canvas.set_view(view)
        activity = view.get("activity") or {}
        focus = view.get("focus")
        self.status.setText(
            f"{view.get('name', '')} · {view.get('attached_clients', 0)} client(s) · "
            f"focus {focus} · working {activity.get('working', 0)} · "
            f"blocked {activity.get('blocked', 0)}"
        )

    @Slot(str)
    def on_lost(self, message: str) -> None:
        self.lost.setText(f"Session lost: {message}. The daemon did not restore it.")
        self.lost.show()

    @Slot()
    def retry(self) -> None:
        self.attach()

    @Slot(str)
    def _run_command(self, command: str) -> None:
        if self.session is None:
            return
        if command == "Detach session":
            self.session.detach()
            self.status.setText("detached; session is still alive")
            return
        if command == "Focus first pane":
            layout = self.canvas.view.get("layout") or {}
            panes = layout.get("panes") or []
            if panes:
                self.session.focus(int(panes[0]["pane_id"]))
            return
        if command == "Refresh view":
            self.canvas.set_view(self.session.view())

    @Slot(int, int)
    def _on_resize(self, cols: int, rows: int) -> None:
        if self.session is not None:
            self.session.resize(cols, rows)

    def closeEvent(self, event: Any) -> None:  # noqa: N802 - Qt API
        if self.worker is not None:
            self.worker.requestInterruption()
            self.worker.wait(500)
        if self.session is not None:
            self.session.detach()
        event.accept()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="stitch-mux PySide6 adapter")
    parser.add_argument("--socket", default=None, help="override the local endpoint")
    parser.add_argument("--session", help="attach directly instead of showing the picker")
    args = parser.parse_args(argv)
    app = QApplication(sys.argv)
    window: QWidget
    if args.session:
        window = SessionWindow(args.session, args.socket)
    else:
        picker = SessionPicker(lambda: list_sessions(args.socket))
        window = picker

        def open_session(name: str) -> None:
            child = SessionWindow(name, args.socket)
            picker.windows.append(child)
            child.show()

        picker.session_selected.connect(open_session)
    window.show()
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
