"""Standalone PySide6 demo for stitch-mux.

Run from the repository root with `uv run python examples/python/qtdemo.py`.

The rendering and interaction model follows stitch-pty's
``terminal_emulator_advanced.py`` example: a custom QPainter cell grid,
themes, cursor shapes, selection, search, and a scrollbar. The important
difference is the data source. This demo never sees PTY bytes: the daemon owns
the emulator and sends structured ``SessionView`` snapshots. Host keys become
semantic ``InputEvent`` values through ``Session.key`` / ``Session.input_text``;
no ANSI parsing or terminal key encoding happens in Python.
"""

from __future__ import annotations

import argparse
import platform
import sys
import time
from dataclasses import dataclass, field
from typing import Any

from PySide6.QtCore import QObject, QPoint, QRect, Qt, QThread, QTimer, QUrl, Signal
from PySide6.QtGui import (
    QClipboard,
    QColor,
    QDesktopServices,
    QFont,
    QFontMetricsF,
    QKeyEvent,
    QPainter,
    QPen,
)
from PySide6.QtWidgets import (
    QApplication,
    QComboBox,
    QHBoxLayout,
    QInputDialog,
    QLabel,
    QListWidget,
    QMainWindow,
    QMessageBox,
    QPushButton,
    QScrollBar,
    QTabWidget,
    QToolBar,
    QVBoxLayout,
    QWidget,
)
from stitch_mux import attach, list_sessions

IS_MAC = platform.system() == "Darwin"

# Attribute bits from stitch-mux-core::cell::Attrs.
A_BOLD, A_DIM, A_ITALIC, A_UNDERLINE = 1, 2, 4, 8
A_BLINK, A_REVERSE, A_HIDDEN, A_STRIKE = 16, 32, 64, 128
_HEX = set("0123456789abcdefABCDEF")


@dataclass(frozen=True)
class Theme:
    name: str
    bg: str
    fg: str
    cursor: str
    sel_bg: str
    match_bg: str
    match_cur_bg: str
    ansi: dict[str, str]


def _theme(name: str, bg: str, fg: str, cursor: str, sel: str, palette: list[str]) -> Theme:
    keys = [
        "black", "red", "green", "brown", "blue", "magenta", "cyan", "white",
        "brightblack", "brightred", "brightgreen", "brightbrown",
        "brightblue", "brightmagenta", "brightcyan", "brightwhite",
    ]
    ansi = dict(zip(keys, palette, strict=True))
    ansi["yellow"] = ansi["brown"]
    ansi["brightyellow"] = ansi["brightbrown"]
    return Theme(name, bg, fg, cursor, sel, "#5a4a00", "#a07800", ansi)


THEMES: dict[str, Theme] = {
    "Default Dark": _theme(
        "Default Dark", "#11131a", "#d7dae0", "#7ee0ff", "#264f78",
        ["#11131a", "#cd3131", "#0dbc79", "#d7b45b", "#2472c8", "#bc3fbc",
         "#11a8cd", "#e5e5e5", "#555866", "#f14c4c", "#23d18b", "#f5d76e",
         "#6cb6ff", "#d670d6", "#4fd6e8", "#ffffff"],
    ),
    "Nord": _theme(
        "Nord", "#2e3440", "#d8dee9", "#88c0d0", "#434c5e",
        ["#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead",
         "#88c0d0", "#e5e9f0", "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b",
         "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4"],
    ),
    "Light": _theme(
        "Light", "#fafafa", "#383a42", "#526fff", "#bfceff",
        ["#fafafa", "#e45649", "#50a14f", "#c18401", "#4078f2", "#a626a4",
         "#0184bc", "#383a42", "#a0a1a7", "#e45649", "#50a14f", "#986801",
         "#4078f2", "#a626a4", "#0184bc", "#090a0b"],
    ),
}
DEFAULT_THEME = "Default Dark"


@dataclass
class Config:
    font_family: str = "Cascadia Mono, JetBrains Mono, Menlo, Consolas, monospace"
    font_size: float = 11.0
    theme_name: str = DEFAULT_THEME
    cursor_blink: bool = True

    @property
    def theme(self) -> Theme:
        return THEMES.get(self.theme_name, THEMES[DEFAULT_THEME])


# A painted cell: text, fg, bg, attrs, link, continuation.
Cell = tuple[str, str, str, int, str | None, bool]


@dataclass(frozen=True)
class Frame:
    cols: int
    rows: list[list[Cell]]
    text_rows: list[str]
    window_start: int
    total: int
    history: int
    title: str
    cursor_x: int
    cursor_y: int
    cursor_visible: bool
    cursor_shape: str
    cursor_blink: bool
    alt_screen: bool
    links: dict[int, list[tuple[int, int, str]]] = field(default_factory=dict)


def color_name(value: Any) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        if "rgb" in value:
            r, g, b = (int(part) & 0xFF for part in value["rgb"])
            return f"#{r:02x}{g:02x}{b:02x}"
        if "indexed" in value:
            index = int(value["indexed"]) & 0xFF
            palette = list(THEMES[DEFAULT_THEME].ansi.values())
            return palette[index % len(palette)]
    return "default"


def resolve_color(value: str, theme: Theme, *, is_bg: bool) -> QColor:
    if value in ("", "default"):
        return QColor(theme.bg if is_bg else theme.fg)
    if value in theme.ansi:
        return QColor(theme.ansi[value])
    if len(value) == 6 and all(ch in _HEX for ch in value):
        return QColor("#" + value)
    if value.startswith("#"):
        return QColor(value)
    return QColor(theme.bg if is_bg else theme.fg)


def dim_color(color: QColor) -> QColor:
    return QColor(int(color.red() * 0.6), int(color.green() * 0.6), int(color.blue() * 0.6))


def _convert_row(row: dict[str, Any], cols: int) -> list[Cell]:
    cells: list[Cell] = []
    for raw in row.get("cells") or []:
        width = max(1, min(4, int(raw.get("width") or 1)))
        text = raw.get("text") or " "
        fg = color_name(raw.get("fg"))
        bg = color_name(raw.get("bg"))
        attrs = int(raw.get("attrs") or 0)
        link = raw.get("link")
        cells.append((text, fg, bg, attrs, link, False))
        for _ in range(width - 1):
            cells.append(("", fg, bg, attrs, link, True))
        if len(cells) >= cols:
            break
    while len(cells) < cols:
        cells.append((" ", "default", "default", 0, None, False))
    return cells[:cols]


def _row_text(cells: list[Cell]) -> str:
    return "".join((" " if cont else text) or " " for text, _, _, _, _, cont in cells)


class FrameBuilder:
    """Turns authoritative snapshots into a bounded local scroll view."""

    MAX_LOCAL_HISTORY = 2000

    def __init__(self) -> None:
        self.seq = -1
        self.scrollback = 0
        self.history: list[list[Cell]] = []
        self.visible: list[list[Cell]] = []

    def build(self, snapshot: dict[str, Any]) -> Frame:
        cells = snapshot.get("cells") or []
        cols = max(1, int(snapshot.get("cols") or 80))
        seq = int(snapshot.get("seq") or 0)
        scrollback = max(0, int(snapshot.get("scrollback_len") or 0))

        if seq < self.seq or scrollback < self.scrollback:
            self.history.clear()
            self.visible.clear()
        else:
            shift = min(max(0, scrollback - self.scrollback), len(self.visible))
            if shift:
                self.history.extend(self.visible[:shift])
                del self.history[:-self.MAX_LOCAL_HISTORY]

        self.visible = [_convert_row(row, cols) for row in cells]
        self.seq = seq
        self.scrollback = scrollback

        rows = self.history + self.visible
        window_start = max(0, scrollback - len(self.history))
        text_rows = [_row_text(row) for row in rows]
        links: dict[int, list[tuple[int, int, str]]] = {}
        for index, painted in enumerate(rows):
            ranges: list[tuple[int, int, str]] = []
            start = 0
            current: str | None = None
            for col in range(cols + 1):
                url = painted[col][4] if col < cols else None
                if url != current:
                    if current:
                        ranges.append((start, col, current))
                    current = url
                    start = col
            if ranges:
                links[window_start + index] = ranges

        cursor = snapshot.get("cursor") or {}
        modes = snapshot.get("modes") or {}
        shape = str(cursor.get("shape") or "block")
        return Frame(
            cols=cols,
            rows=rows,
            text_rows=text_rows,
            window_start=window_start,
            total=scrollback + len(self.visible),
            history=scrollback,
            title=str(snapshot.get("title") or ""),
            cursor_x=int(cursor.get("x") or 0),
            cursor_y=int(cursor.get("y") or 0),
            cursor_visible=bool(cursor.get("visible", True) and modes.get("cursor_visible", True)),
            cursor_shape=shape.rsplit(".", 1)[-1].lower(),
            cursor_blink=bool(cursor.get("blink", True)),
            alt_screen=bool(modes.get("alt_screen") or snapshot.get("alt_screen")),
            links=links,
        )


_QT_SPECIAL = {
    Qt.Key.Key_Return: "enter",
    Qt.Key.Key_Enter: "enter",
    Qt.Key.Key_Tab: "tab",
    Qt.Key.Key_Backtab: "tab",
    Qt.Key.Key_Backspace: "backspace",
    Qt.Key.Key_Escape: "esc",
    Qt.Key.Key_Insert: "insert",
    Qt.Key.Key_Delete: "delete",
    Qt.Key.Key_Left: "left",
    Qt.Key.Key_Right: "right",
    Qt.Key.Key_Up: "up",
    Qt.Key.Key_Down: "down",
    Qt.Key.Key_PageUp: "pageup",
    Qt.Key.Key_PageDown: "pagedown",
    Qt.Key.Key_Home: "home",
    Qt.Key.Key_End: "end",
}
_QT_PUNCTUATION = {
    Qt.Key.Key_Comma: ",", Qt.Key.Key_Period: ".", Qt.Key.Key_Slash: "/",
    Qt.Key.Key_Semicolon: ";", Qt.Key.Key_Apostrophe: "'",
    Qt.Key.Key_BracketLeft: "[", Qt.Key.Key_BracketRight: "]",
    Qt.Key.Key_Backslash: "\\", Qt.Key.Key_Minus: "-",
    Qt.Key.Key_Equal: "=", Qt.Key.Key_QuoteLeft: "`",
}


def _key_name(event: QKeyEvent) -> str | None:
    key = event.key()
    if key in _QT_SPECIAL:
        return _QT_SPECIAL[key]
    if Qt.Key.Key_F1 <= key <= Qt.Key.Key_F35:
        return f"f{int(key) - int(Qt.Key.Key_F1) + 1}"
    if Qt.Key.Key_A <= key <= Qt.Key.Key_Z:
        return chr(ord("a") + int(key) - int(Qt.Key.Key_A))
    if Qt.Key.Key_0 <= key <= Qt.Key.Key_9:
        digit = chr(ord("0") + int(key) - int(Qt.Key.Key_0))
        return f"kp_{digit}" if event.modifiers() & Qt.KeyboardModifier.KeypadModifier else digit
    if key in _QT_PUNCTUATION:
        return _QT_PUNCTUATION[key]
    text = event.text()
    if text and len(text) == 1 and ord(text) >= 0x20:
        return text.lower() if text.isalpha() else text
    return None


def _modifiers(event: QKeyEvent) -> int:
    bits = 0
    if event.modifiers() & Qt.KeyboardModifier.ShiftModifier:
        bits |= 1
    if event.modifiers() & Qt.KeyboardModifier.AltModifier:
        bits |= 2
    if event.modifiers() & Qt.KeyboardModifier.ControlModifier:
        bits |= 4
    if event.modifiers() & Qt.KeyboardModifier.MetaModifier:
        bits |= 8
    return bits


class TerminalView(QWidget):
    """Custom-painted cell grid for one pane.

    Signals carry semantic commands only. The owning session tab sends them to
    the daemon through the Python SDK; this widget never owns a terminal.
    """

    key_event = Signal(str, int, object)  # code, modifiers, text
    text_event = Signal(str)
    pane_clicked = Signal(int)
    resized = Signal()

    def __init__(self, config: Config, pane_id: int, scrollbar: QScrollBar,
                 parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.cfg = config
        self.pane_id = pane_id
        self.vbar = scrollbar
        self.frame: Frame | None = None
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.setMouseTracking(True)
        self.setCursor(Qt.CursorShape.IBeamCursor)
        self.setAttribute(Qt.WidgetAttribute.WA_OpaquePaintEvent, True)
        self.vbar.valueChanged.connect(self._on_scrollbar)
        self._follow = True
        self._sel_anchor: tuple[int, int] | None = None
        self._sel_head: tuple[int, int] | None = None
        self._sel_mode = "char"
        self._selecting = False
        self._click_count = 0
        self._last_click_t = 0.0
        self._last_click_cell: tuple[int, int] | None = None
        self._hover_link: tuple[int, int, int, str] | None = None
        self._search = ""
        self._matches: list[tuple[int, int, int]] = []
        self._match_idx = -1
        self._blink_on = True
        self._blink = QTimer(self)
        self._blink.setInterval(530)
        self._blink.timeout.connect(self._toggle_blink)
        self._apply_font()

    def _apply_font(self) -> None:
        font = QFont(self.cfg.font_family)
        font.setStyleHint(QFont.StyleHint.Monospace)
        font.setFixedPitch(True)
        font.setPointSizeF(self.cfg.font_size)
        self._font = font
        self._font_bold = QFont(font)
        self._font_bold.setBold(True)
        self._font_italic = QFont(font)
        self._font_italic.setItalic(True)
        self._font_bi = QFont(font)
        self._font_bi.setBold(True)
        self._font_bi.setItalic(True)
        metrics = QFontMetricsF(font)
        self.cell_w = max(1.0, metrics.horizontalAdvance("M"))
        self.cell_h = max(1.0, metrics.height())
        self.baseline = metrics.ascent()
        self.update()

    def refresh_config(self) -> None:
        self._apply_font()
        self.update()

    # -- frame and scroll -------------------------------------------------

    def set_frame(self, frame: Frame) -> None:
        self.frame = frame
        rows_visible = max(1, int(self.height() / self.cell_h))
        # Only the rows we actually hold locally can be scrolled. The daemon
        # owns the authoritative scrollback; the demo keeps a bounded window.
        max_top = max(0, frame.window_start + len(frame.rows) - rows_visible)
        self.vbar.setRange(0, max_top)
        self.vbar.setPageStep(rows_visible)
        if self._follow:
            self.vbar.setValue(max_top)
        if self._search:
            self._refresh_matches()
        self.update()

    def _on_scrollbar(self, _value: int) -> None:
        self._follow = self.vbar.value() >= self.vbar.maximum()
        self.update()

    @property
    def top_row(self) -> int:
        return self.vbar.value()

    def scroll_lines(self, delta: int) -> None:
        self.vbar.setValue(self.vbar.value() + delta)

    def scroll_to_bottom(self) -> None:
        self._follow = True
        self.vbar.setValue(self.vbar.maximum())

    def _toggle_blink(self) -> None:
        self._blink_on = not self._blink_on
        self.update()

    def focusInEvent(self, event: Any) -> None:
        super().focusInEvent(event)
        if self.cfg.cursor_blink and self._frame_blinks():
            self._blink.start()
        self.update()

    def focusOutEvent(self, event: Any) -> None:
        super().focusOutEvent(event)
        self._blink.stop()
        self.update()

    def _frame_blinks(self) -> bool:
        return bool(self.frame and self.frame.cursor_blink and self.frame.cursor_visible)

    # -- painting ---------------------------------------------------------

    def paintEvent(self, _event: Any) -> None:
        painter = QPainter(self)
        try:
            theme = self.cfg.theme
            painter.fillRect(self.rect(), QColor(theme.bg))
            frame = self.frame
            if frame is None:
                return
            rows_visible = max(1, int(self.height() / self.cell_h))
            cols_visible = max(1, int(self.width() / self.cell_w))
            sel = self._selection()
            for screen_row in range(rows_visible):
                absolute = self.top_row + screen_row
                index = absolute - frame.window_start
                if 0 <= index < len(frame.rows):
                    self._paint_row(painter, frame.rows[index], screen_row, cols_visible,
                                    absolute, theme, sel)
            self._paint_cursor(painter, frame, theme, cols_visible)
        finally:
            painter.end()

    def _paint_row(self, painter: QPainter, cells: list[Cell], screen_row: int,
                   cols: int, absolute: int, theme: Theme, sel: Any) -> None:
        y = screen_row * self.cell_h
        runs: list[list[Any]] = []

        def add(col: int, cell: Cell, fg: QColor, bg: QColor, font: QFont, flags: int) -> None:
            if runs and runs[-1][4] == (fg.rgb(), bg.rgb(), flags, font.bold(), font.italic()):
                runs[-1][1].append(cell)
                return
            runs.append([col, [cell], fg, bg, (fg.rgb(), bg.rgb(), flags,
                                              font.bold(), font.italic()), font, flags])

        for col in range(min(cols, len(cells))):
            text, fg_name, bg_name, attrs, _link, cont = cells[col]
            fg = resolve_color(fg_name, theme, is_bg=False)
            bg = resolve_color(bg_name, theme, is_bg=True)
            if attrs & A_DIM:
                fg = dim_color(fg)
            if attrs & A_REVERSE:
                fg, bg = bg, fg
            if attrs & A_HIDDEN:
                fg = bg
            if self._is_selected(absolute, col, sel):
                bg = QColor(theme.sel_bg)
            if self._is_match(absolute, col):
                bg = QColor(theme.match_cur_bg if self._current_match(absolute, col)
                            else theme.match_bg)
                fg = QColor("#000000")
            if self._hover_link and self._hover_link[0] == absolute \
                    and self._hover_link[1] <= col < self._hover_link[2]:
                attrs |= A_UNDERLINE
            font = (self._font_bi if attrs & A_BOLD and attrs & A_ITALIC else
                    self._font_bold if attrs & A_BOLD else
                    self._font_italic if attrs & A_ITALIC else self._font)
            flags = (1 if attrs & A_UNDERLINE else 0) | (2 if attrs & A_STRIKE else 0)
            add(col, (text, fg_name, bg_name, attrs, _link, cont), fg, bg, font, flags)

        for start, group, fg, bg, _key, font, flags in runs:
            width = len(group)
            rect = QRect(int(start * self.cell_w), int(y),
                         int(width * self.cell_w), int(self.cell_h) + 1)
            if bg.rgb() != QColor(theme.bg).rgb():
                painter.fillRect(rect, bg)
            text = "".join((" " if cont or not glyph else glyph)
                           for glyph, _, _, _, _, cont in group)
            if text.strip():
                painter.setFont(font)
                painter.setPen(QPen(fg))
                painter.drawText(QPoint(int(start * self.cell_w), int(y + self.baseline)), text)
            if flags & 1:
                painter.drawLine(int(start * self.cell_w), int(y + self.cell_h - 1),
                                 int((start + width) * self.cell_w), int(y + self.cell_h - 1))
            if flags & 2:
                painter.drawLine(int(start * self.cell_w), int(y + self.cell_h * 0.55),
                                 int((start + width) * self.cell_w), int(y + self.cell_h * 0.55))

    def _paint_cursor(self, painter: QPainter, frame: Frame, theme: Theme, cols: int) -> None:
        if not frame.cursor_visible or not (0 <= frame.cursor_x < cols):
            return
        screen_row = frame.cursor_y - self.top_row
        rows = max(1, int(self.height() / self.cell_h))
        if not 0 <= screen_row < rows:
            return
        x, y = frame.cursor_x * self.cell_w, screen_row * self.cell_h
        color = QColor(theme.cursor)
        painter.setPen(QPen(color))
        if not self.hasFocus():
            painter.setBrush(Qt.BrushStyle.NoBrush)
            painter.drawRect(QRect(int(x), int(y), int(self.cell_w), int(self.cell_h - 1)))
            return
        if frame.cursor_blink and self.cfg.cursor_blink and not self._blink_on:
            return
        shape = frame.cursor_shape
        if shape == "bar":
            painter.fillRect(QRect(int(x), int(y), 2, int(self.cell_h)), color)
        elif shape == "underline":
            painter.fillRect(QRect(int(x), int(y + self.cell_h - 2), int(self.cell_w), 2), color)
        else:
            rect = QRect(int(x), int(y), int(self.cell_w), int(self.cell_h))
            painter.fillRect(rect, color)
            index = frame.cursor_y - frame.window_start
            if 0 <= index < len(frame.rows):
                cell = frame.rows[index][frame.cursor_x]
                if cell[0] and not cell[5]:
                    painter.setPen(QPen(QColor(theme.bg)))
                    painter.drawText(QPoint(int(x), int(y + self.baseline)), cell[0])

    # -- selection, search, clipboard -------------------------------------

    def _selection(self) -> tuple[tuple[int, int], tuple[int, int], str] | None:
        if self._sel_anchor is None or self._sel_head is None:
            return None
        first, last = self._sel_anchor, self._sel_head
        return (first, last, self._sel_mode) if first <= last else (last, first, self._sel_mode)

    def _is_selected(self, row: int, col: int, sel: Any) -> bool:
        if not sel:
            return False
        (r0, c0), (r1, c1), mode = sel
        if mode == "block":
            return r0 <= row <= r1 and min(c0, c1) <= col <= max(c0, c1)
        if row < r0 or row > r1:
            return False
        if r0 == r1:
            return c0 <= col <= c1
        if row == r0:
            return col >= c0
        if row == r1:
            return col <= c1
        return True

    def has_selection(self) -> bool:
        sel = self._selection()
        return bool(sel and sel[0] != sel[1])

    def clear_selection(self) -> None:
        self._sel_anchor = self._sel_head = None
        self.update()

    def select_all(self) -> None:
        if not self.frame or not self.frame.text_rows:
            return
        self._sel_mode = "char"
        self._sel_anchor = (0, 0)
        self._sel_head = (len(self.frame.text_rows) - 1, self.frame.cols - 1)
        self.update()

    def selected_text(self) -> str:
        sel = self._selection()
        if not sel or not self.frame:
            return ""
        (r0, c0), (r1, c1), mode = sel
        out: list[str] = []
        for row in range(r0, r1 + 1):
            index = row - self.frame.window_start
            text = self.frame.text_rows[index] if 0 <= index < len(self.frame.text_rows) else ""
            if mode == "block":
                out.append(text[min(c0, c1):max(c0, c1) + 1].rstrip())
            else:
                start = c0 if row == r0 else 0
                end = c1 + 1 if row == r1 else len(text)
                out.append(text[start:end].rstrip())
        return "\n".join(out)

    def _word_bounds(self, row: int, col: int) -> tuple[int, int]:
        index = row - (self.frame.window_start if self.frame else 0)
        text = ""
        if self.frame and 0 <= index < len(self.frame.text_rows):
            text = self.frame.text_rows[index]
        wordish = lambda ch: ch.isalnum() or ch in "_-./~:@"  # noqa: E731
        if not text or col >= len(text) or not wordish(text[col]):
            return col, col
        start = col
        while start > 0 and wordish(text[start - 1]):
            start -= 1
        end = col
        while end < len(text) - 1 and wordish(text[end + 1]):
            end += 1
        return start, end

    def copy(self, mode: QClipboard.Mode = QClipboard.Mode.Clipboard) -> None:
        clipboard = QApplication.clipboard()
        if mode == QClipboard.Mode.Selection and not clipboard.supportsSelection():
            return
        text = self.selected_text()
        if text:
            clipboard.setText(text, mode)

    def paste(self) -> None:
        text = QApplication.clipboard().text()
        if text:
            self.text_event.emit(text.replace("\r\n", "\n").replace("\r", "\n"))
            self.scroll_to_bottom()

    def set_search(self, term: str) -> None:
        self._search = term
        self._matches = []
        if term and self.frame:
            needle = term.lower()
            for row, text in enumerate(self.frame.text_rows):
                start = 0
                while (index := text.lower().find(needle, start)) >= 0:
                    self._matches.append((self.frame.window_start + row, index, len(term)))
                    start = index + 1
        self._match_idx = 0 if self._matches else -1
        self.update()

    def find_next(self, forward: bool = True) -> None:
        if not self._matches:
            return
        step = 1 if forward else -1
        self._match_idx = (self._match_idx + step) % len(self._matches)
        row, _, _ = self._matches[self._match_idx]
        self.vbar.setValue(max(0, row - self.vbar.pageStep() // 2))
        self.update()

    def _is_match(self, row: int, col: int) -> bool:
        return any(r == row and c <= col < c + length for r, c, length in self._matches)

    def _current_match(self, row: int, col: int) -> bool:
        if not (0 <= self._match_idx < len(self._matches)):
            return False
        r, c, length = self._matches[self._match_idx]
        return r == row and c <= col < c + length

    # -- input ------------------------------------------------------------

    def _cell_at(self, pos: QPoint) -> tuple[int, int]:
        col = max(0, min(self.frame.cols - 1 if self.frame else 80,
                         int(pos.x() / self.cell_w)))
        row = self.top_row + int(pos.y() / self.cell_h)
        if self.frame and self.frame.text_rows:
            row = max(0, min(len(self.frame.text_rows) - 1, row))
        return row, col

    def _link_at(self, row: int, col: int) -> str | None:
        if not self.frame:
            return None
        for start, end, url in self.frame.links.get(row, []):
            if start <= col < end:
                return url
        return None

    def mousePressEvent(self, event: Any) -> None:
        self.setFocus()
        pos = event.position().toPoint()
        row, col = self._cell_at(pos)
        button = event.button()
        if button == Qt.MouseButton.LeftButton and (
                event.modifiers() & Qt.KeyboardModifier.ControlModifier):
            url = self._link_at(row, col)
            if url:
                if not url.lower().startswith(("http://", "https://", "mailto:")):
                    if QMessageBox.question(
                        self, "Open custom link",
                        f"Open this URI with the desktop handler?\n\n{url}",
                    ) != QMessageBox.StandardButton.Yes:
                        return
                QDesktopServices.openUrl(QUrl(url))
                return
        if button == Qt.MouseButton.MiddleButton:
            self.paste()
            return
        if button != Qt.MouseButton.LeftButton:
            return
        self.pane_clicked.emit(self.pane_id)
        now = time.monotonic()
        if now - self._last_click_t < 0.4 and self._last_click_cell == (row, col):
            self._click_count += 1
        else:
            self._click_count = 1
        self._last_click_t, self._last_click_cell = now, (row, col)
        self._selecting = True
        shift = bool(event.modifiers() & Qt.KeyboardModifier.ShiftModifier)
        if event.modifiers() & Qt.KeyboardModifier.AltModifier:
            self._sel_mode = "block"
            self._sel_anchor = self._sel_head = (row, col)
        elif self._click_count >= 3:
            self._sel_mode = "line"
            last_col = (self.frame.cols - 1) if self.frame else 0
            self._sel_anchor, self._sel_head = (row, 0), (row, last_col)
        elif self._click_count == 2:
            self._sel_mode = "word"
            start, end = self._word_bounds(row, col)
            self._sel_anchor, self._sel_head = (row, start), (row, end)
        elif shift and self._sel_anchor is not None:
            self._sel_head = (row, col)
        else:
            self._sel_mode = "char"
            self._sel_anchor = self._sel_head = (row, col)
        self.update()

    def mouseMoveEvent(self, event: Any) -> None:
        pos = event.position().toPoint()
        if self._selecting and event.buttons() & Qt.MouseButton.LeftButton:
            row, col = self._cell_at(pos)
            if self._sel_mode == "word":
                start, end = self._word_bounds(row, col)
                col = end if (row, col) >= (self._sel_anchor or (0, 0)) else start
            elif self._sel_mode == "line":
                last_col = (self.frame.cols - 1) if self.frame else 0
                col = last_col if row >= (self._sel_anchor or (0, 0))[0] else 0
            self._sel_head = (row, col)
            self.update()
            return
        row, col = self._cell_at(pos)
        url = self._link_at(row, col)
        hover = (row, col, col + 1, url) if url else None
        if hover != self._hover_link:
            self._hover_link = hover
            self.update()
        self.setCursor(Qt.CursorShape.PointingHandCursor if url else Qt.CursorShape.IBeamCursor)

    def mouseReleaseEvent(self, event: Any) -> None:
        if self._selecting:
            self._selecting = False
            if self.has_selection():
                self.copy(QClipboard.Mode.Selection)
                self.copy()

    def wheelEvent(self, event: Any) -> None:
        delta = event.angleDelta().y()
        if not delta:
            return
        lines = int(delta / 120 * 3) or (1 if delta > 0 else -1)
        if self.frame and self.frame.alt_screen:
            code = "up" if lines > 0 else "down"
            for _ in range(abs(lines)):
                self.key_event.emit(code, 0, None)
        else:
            self.scroll_lines(-lines)
        event.accept()

    def keyPressEvent(self, event: QKeyEvent) -> None:
        modifiers = event.modifiers()
        if modifiers & Qt.KeyboardModifier.ShiftModifier:
            if event.key() == Qt.Key.Key_PageUp:
                self.scroll_lines(-self.vbar.pageStep())
                return
            if event.key() == Qt.Key.Key_PageDown:
                self.scroll_lines(self.vbar.pageStep())
                return
        if (modifiers & Qt.KeyboardModifier.ControlModifier
                and modifiers & Qt.KeyboardModifier.ShiftModifier
                and event.key() == Qt.Key.Key_C):
            self.copy()
            return
        if (modifiers & Qt.KeyboardModifier.ControlModifier
                and modifiers & Qt.KeyboardModifier.ShiftModifier
                and event.key() == Qt.Key.Key_V):
            self.paste()
            return
        name = _key_name(event)
        if name is None:
            event.ignore()
            return
        self.scroll_to_bottom()
        if self.has_selection() and not modifiers & Qt.KeyboardModifier.ShiftModifier:
            self.clear_selection()
        text = event.text()
        if not text or any(ord(ch) < 0x20 for ch in text):
            text = None
        self.key_event.emit(name, _modifiers(event), text)
        event.accept()


class PaneWidget(QWidget):
    """A titled pane: header, cell view, and its local scrollbar."""

    def __init__(self, pane_id: int, config: Config, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.pane_id = pane_id
        self.builder = FrameBuilder()
        self.scrollbar = QScrollBar(Qt.Orientation.Vertical, self)
        self.view = TerminalView(config, pane_id, self.scrollbar, self)
        self.title = QLabel(f"pane {pane_id}", self)
        self.title.setObjectName("paneTitle")
        header = QHBoxLayout()
        header.setContentsMargins(4, 2, 4, 2)
        header.addWidget(self.title)
        body = QHBoxLayout()
        body.setContentsMargins(0, 0, 0, 0)
        body.addWidget(self.view, 1)
        body.addWidget(self.scrollbar)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(1, 1, 1, 1)
        layout.addLayout(header)
        layout.addLayout(body, 1)
        self.setStyleSheet(
            "#paneTitle { color: #8ab4f8; font-size: 11px; }\n"
            "QScrollBar:vertical { width: 12px; }"
        )

    def refresh_config(self) -> None:
        self.view.refresh_config()


class ViewWorker(QThread):
    """Polls the Rust session off the GUI thread and emits plain dicts."""

    view_updated = Signal(dict)
    failed = Signal(str)

    def __init__(self, session: Any, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self.session = session

    def run(self) -> None:  # noqa: D102 - Qt thread entry point
        while not self.isInterruptionRequested():
            try:
                view = self.session.view()
            except RuntimeError as err:
                self.failed.emit(str(err))
                return
            self.view_updated.emit(view)
            if view.get("connection") == "Lost":
                self.failed.emit(str((view.get("error") or {}).get("message") or "connection lost"))
                return
            self.msleep(33)

    def stop(self) -> None:
        self.requestInterruption()
        if not self.wait(1500):
            self.terminate()
            self.wait()


class SessionTab(QWidget):
    """One attached stitch-mux session, with a daemon-owned pane grid."""

    status_changed = Signal(str)
    lost = Signal(str)

    def __init__(self, name: str, socket: str | None, config: Config,
                 parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.name = name
        self.socket = socket
        self.cfg = config
        self.session = attach(name, cols=100, rows=30, socket=socket)
        self.panes: dict[int, PaneWidget] = {}
        self.focus = 0
        self.view: dict[str, Any] = {}
        self.worker = ViewWorker(self.session, self)
        self.worker.view_updated.connect(self.apply_view)
        self.worker.failed.connect(self.lost.emit)
        self.worker.start()

    def _pane(self, pane_id: int) -> PaneWidget:
        if pane_id not in self.panes:
            pane = PaneWidget(pane_id, self.cfg, self)
            pane.view.key_event.connect(self._send_key)
            pane.view.text_event.connect(self.session.input_text)
            pane.view.pane_clicked.connect(self.focus_pane)
            self.panes[pane_id] = pane
        return self.panes[pane_id]

    def _send_key(self, code: str, modifiers: int, text: object) -> None:
        try:
            self.session.key(code, modifiers, text if isinstance(text, str) else None)
        except RuntimeError:
            pass

    def focus_pane(self, pane_id: int) -> None:
        self.focus = pane_id
        try:
            self.session.focus(pane_id)
        except RuntimeError:
            pass

    def apply_view(self, view: dict[str, Any]) -> None:
        self.view = view
        layout = view.get("layout") or {}
        panes = view.get("panes") or {}
        active = {int(pane_id) for pane_id in panes}
        for pane_id in list(self.panes):
            if pane_id not in active:
                self.panes.pop(pane_id).deleteLater()

        cell_w = self.panes[next(iter(self.panes))].view.cell_w if self.panes else 8.0
        cell_h = self.panes[next(iter(self.panes))].view.cell_h if self.panes else 16.0
        for geometry in layout.get("panes") or []:
            pane_id = int(geometry.get("pane_id") or 0)
            pane = self._pane(pane_id)
            snapshot = (panes.get(str(pane_id)) or {}).get("snapshot")
            if snapshot:
                pane.view.set_frame(pane.builder.build(snapshot))
                pane.title.setText(snapshot.get("title") or f"pane {pane_id}")
            outer = geometry.get("outer") or {}
            header_h = pane.title.sizeHint().height() + 4
            pane.setGeometry(
                int(float(outer.get("x", 0)) * cell_w),
                header_h + int(float(outer.get("y", 0)) * cell_h),
                max(1, int(float(outer.get("cols", 1)) * cell_w)),
                max(1, int(float(outer.get("rows", 1)) * cell_h)),
            )
            pane.show()

        focus = int(layout.get("focus") or view.get("focus") or 0)
        if focus in self.panes:
            self.focus = focus
        activity = view.get("activity") or {}
        status = (
            f"{view.get('name', '')} · {view.get('connection', '')} · "
            f"focus {self.focus or '-'} · {len(self.panes)} pane(s) · "
            f"working {activity.get('working', 0)} · blocked {activity.get('blocked', 0)}"
        )
        self.status_changed.emit(status)

    def resizeEvent(self, event: Any) -> None:
        super().resizeEvent(event)
        cell_w = 8.0
        cell_h = 16.0
        if self.panes:
            view = next(iter(self.panes.values())).view
            cell_w, cell_h = view.cell_w, view.cell_h
        cols = max(20, int(self.width() / cell_w))
        rows = max(4, int(self.height() / cell_h))
        if (cols, rows) != getattr(self, "_last_grid", (0, 0)):
            self._last_grid = (cols, rows)
            try:
                self.session.resize(cols, rows)
            except RuntimeError:
                pass

    def refresh_config(self) -> None:
        for pane in self.panes.values():
            pane.refresh_config()

    def find(self, term: str) -> None:
        for pane in self.panes.values():
            pane.view.set_search(term)

    def find_next(self, forward: bool = True) -> None:
        pane = self.panes.get(self.focus)
        if pane:
            pane.view.find_next(forward)

    def close(self) -> None:  # noqa: D102 - Qt lifecycle helper
        self.worker.stop()
        try:
            self.session.detach()
        except RuntimeError:
            pass
        super().close()


class ConnectDialog(QWidget):
    session_selected = Signal(str)

    def __init__(self, socket: str | None, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle("Attach to a stitch-mux session")
        self.resize(460, 320)
        self.list = QListWidget()
        self.refresh = QPushButton("Refresh")
        self.connect_button = QPushButton("Attach")
        self.socket = socket
        self.refresh.clicked.connect(self.reload)
        self.connect_button.clicked.connect(self._accept)
        buttons = QHBoxLayout()
        buttons.addWidget(self.refresh)
        buttons.addWidget(self.connect_button)
        layout = QVBoxLayout(self)
        layout.addWidget(self.list)
        layout.addLayout(buttons)
        self.reload()

    def reload(self) -> None:
        self.list.clear()
        for session in list_sessions(self.socket):
            name = session.get("name", "")
            panes = len(session.get("panes") or [])
            clients = session.get("attached_clients", 0)
            self.list.addItem(f"{name} — {panes} pane(s), {clients} client(s)")

    def _accept(self) -> None:
        item = self.list.currentItem()
        if item:
            self.session_selected.emit(item.text().split(" — ", 1)[0])
            self.close()


class MainWindow(QMainWindow):
    def __init__(self, socket: str | None, session: str | None) -> None:
        super().__init__()
        self.socket = socket
        self.cfg = Config()
        self.setWindowTitle("stitch-mux — Qt demo")
        self.resize(1100, 700)
        self.tabs = QTabWidget()
        self.tabs.setTabsClosable(True)
        self.tabs.tabCloseRequested.connect(self._close_tab)
        self.setCentralWidget(self.tabs)
        self.status = QLabel("no session attached")
        self.statusBar().addPermanentWidget(self.status)

        toolbar = QToolBar("Session")
        self.addToolBar(toolbar)
        toolbar.addAction("Attach", self.attach_dialog)
        toolbar.addAction("Detach", self.detach_current)
        toolbar.addAction("Copy", self.copy_current)
        toolbar.addAction("Paste", self.paste_current)
        toolbar.addAction("Find", self.find_current)
        toolbar.addWidget(QLabel("Theme"))
        self.theme_box = QComboBox()
        self.theme_box.addItems(THEMES)
        self.theme_box.currentTextChanged.connect(self._set_theme)
        toolbar.addWidget(self.theme_box)
        toolbar.addAction("Font +", lambda: self._zoom(1.0))
        toolbar.addAction("Font -", lambda: self._zoom(-1.0))

        self.dialog: ConnectDialog | None = None
        if session:
            self.attach(session)

    @property
    def current(self) -> SessionTab | None:
        return self.tabs.currentWidget()

    def attach_dialog(self) -> None:
        self.dialog = ConnectDialog(self.socket, self)
        self.dialog.session_selected.connect(self.attach)
        self.dialog.show()

    def attach(self, name: str) -> None:
        tab = SessionTab(name, self.socket, self.cfg, self)
        tab.status_changed.connect(self.status.setText)
        tab.lost.connect(self.status.setText)
        index = self.tabs.addTab(tab, name)
        self.tabs.setCurrentIndex(index)

    def _close_tab(self, index: int) -> None:
        tab = self.tabs.widget(index)
        if isinstance(tab, SessionTab):
            tab.close()
        self.tabs.removeTab(index)

    def detach_current(self) -> None:
        tab = self.current
        if tab:
            self._close_tab(self.tabs.currentIndex())

    def _active_view(self) -> TerminalView | None:
        tab = self.current
        pane = tab.panes.get(tab.focus) if tab else None
        return pane.view if pane else None

    def copy_current(self) -> None:
        if view := self._active_view():
            view.copy()

    def paste_current(self) -> None:
        if view := self._active_view():
            view.paste()

    def find_current(self) -> None:
        tab = self.current
        if not tab:
            return
        term, ok = QInputDialog.getText(self, "Find in scrollback", "Search:")
        if ok:
            tab.find(term)

    def _set_theme(self, name: str) -> None:
        self.cfg.theme_name = name
        if tab := self.current:
            tab.refresh_config()

    def _zoom(self, delta: float) -> None:
        self.cfg.font_size = max(7.0, min(22.0, self.cfg.font_size + delta))
        if tab := self.current:
            tab.refresh_config()

    def closeEvent(self, event: Any) -> None:
        for index in reversed(range(self.tabs.count())):
            self._close_tab(index)
        event.accept()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="stitch-mux PySide6 demo")
    parser.add_argument("--socket", default=None, help="override the local endpoint")
    parser.add_argument("--session", help="attach directly instead of showing the picker")
    args = parser.parse_args(argv)
    app = QApplication(sys.argv)
    window = MainWindow(args.socket, args.session)
    window.show()
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
