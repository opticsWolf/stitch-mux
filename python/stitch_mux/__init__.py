"""Python control SDK for stitch-mux.

The Rust extension owns protocol state on a background thread. Qt adapters live
in :mod:`stitch_mux.qt` and are imported only when PySide6 is available.
"""

from ._native import Session, attach, capture, connect, list_sessions

__all__ = ["Session", "attach", "capture", "connect", "list_sessions"]
