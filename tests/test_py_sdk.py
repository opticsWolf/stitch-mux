"""Python SDK tests against a real local daemon.

Set STITCH_MUX_BIN to the built CLI executable to run the end-to-end test. The
pure checks below always run.
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import time
from pathlib import Path
from uuid import uuid4

import pytest
import stitch_mux


def test_connect_reports_bad_endpoint_without_a_daemon() -> None:
    endpoint = str(Path(tempfile.gettempdir()) / f"m5py{uuid4().hex[:6]}.sock")
    with pytest.raises(RuntimeError):
        stitch_mux.connect(endpoint)


def test_capture_format_is_validated_after_extension_load() -> None:
    with pytest.raises(ValueError):
        stitch_mux.capture(1, format="svg")


def _wait_for(predicate, timeout: float = 15.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except RuntimeError:
            pass
        time.sleep(0.05)
    raise AssertionError("condition not met before timeout")


def test_attach_input_resize_detach_and_reattach() -> None:
    binary = os.environ.get("STITCH_MUX_BIN")
    if not binary:
        pytest.skip("set STITCH_MUX_BIN to a built stitch-mux executable")

    unique = f"m5py{uuid4().hex[:8]}"
    socket = unique if os.name == "nt" else str(Path(tempfile.gettempdir()) / f"{unique}.sock")
    env = {**os.environ, "STITCH_MUX_SOCKET": socket}
    daemon = subprocess.Popen([binary, "--socket", socket, "server", "--foreground"], env=env)
    try:
        _wait_for(lambda: bool(stitch_mux.connect(socket)))

        program = "cmd.exe" if os.name == "nt" else "sh"
        created = subprocess.run(
            [binary, "--socket", socket, "new", "-s", "demo", "--", program],
            env=env,
            capture_output=True,
            text=True,
            check=True,
            timeout=15,
        )
        pane_id = int(json.loads(created.stdout)["sessions"][0]["panes"][0]["id"])

        session = stitch_mux.attach("demo", cols=80, rows=24, socket=socket)
        try:
            _wait_for(lambda: session.view()["connection"] == "Connected")
            _wait_for(lambda: str(pane_id) in session.view()["panes"])

            session.input_text("echo M5-PYTHON\n")
            _wait_for(
                lambda: "M5-PYTHON"
                in (stitch_mux.capture(pane_id, socket=socket).get("text") or "")
            )

            session.resize(96, 32)
            _wait_for(lambda: session.view()["size"]["cols"] == 96)
            session.focus(pane_id)
        finally:
            session.detach()

        reattached = stitch_mux.attach("demo", cols=80, rows=24, socket=socket)
        try:
            _wait_for(lambda: reattached.view()["connection"] == "Connected")
            assert "M5-PYTHON" in (stitch_mux.capture(pane_id, socket=socket).get("text") or "")
        finally:
            reattached.detach()

        sessions = stitch_mux.list_sessions(socket)
        assert any(item["name"] == "demo" for item in sessions)
    finally:
        subprocess.run(
            [binary, "--socket", socket, "shutdown"],
            env=env,
            capture_output=True,
            timeout=15,
        )
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
