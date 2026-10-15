# SPDX-License-Identifier: Apache-2.0
"""The runner ends itself once the serving worker that started it is gone."""

from __future__ import annotations

import contextlib
import inspect
import logging
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

from tensorplate_pytorch_backend import runner, sanitize

PATIENCE_S = 10.0

_GONE = "the serving worker that started this runner is gone"

# The runner's own logging, and a parent that changes after the first look.
_ADOPTED_RUNNER = """
import os
import threading
from tensorplate_pytorch_backend import runner, sanitize

sanitize.configure_process_logging("WARNING")
{prelude}
first = os.getppid()
parents = iter([first])
runner.watch_parent(poll_s=0.001, getppid=lambda: next(parents, first + 1)).join()
"""

# A process at its thread limit: the line's thread cannot start.
_NO_LINE_THREAD = """
start = threading.Thread.start

def refuse(thread):
    if thread.name == "sidecar-parent-gone-line":
        raise RuntimeError("can't start new thread")
    start(thread)

threading.Thread.start = refuse
"""


class Parents:
    """``os.getppid`` for a runner whose parent is ``first`` until ``adopt()``."""

    def __init__(self, first: int) -> None:
        self._lock = threading.Lock()
        self._parent = first
        self.calls = 0
        self.checked_twice = threading.Event()

    def __call__(self) -> int:
        with self._lock:
            self.calls += 1
            if self.calls >= 3:
                self.checked_twice.set()
            return self._parent

    def adopt(self) -> None:
        with self._lock:
            self._parent += 1


class Sleeps:
    """``time.sleep`` that records each interval and waits a millisecond."""

    def __init__(self) -> None:
        self.intervals: list[float] = []

    def __call__(self, seconds: float) -> None:
        self.intervals.append(seconds)
        time.sleep(0.001)


class Exits:
    """``os._exit`` that records the status instead of ending the test."""

    def __init__(self) -> None:
        self.statuses: list[int] = []
        self.called = threading.Event()

    def __call__(self, status: int) -> None:
        self.statuses.append(status)
        self.called.set()


class Joins:
    """``Thread.join`` that records each wait and whether the exit came first.

    It then waits as long as the test does, not for the recorded time.
    """

    def __init__(self, exits: Exits) -> None:
        self._exits = exits
        self.waits: list[tuple[float, bool]] = []

    def __call__(self, thread: threading.Thread, timeout: float) -> None:
        self.waits.append((timeout, self._exits.called.is_set()))
        thread.join(PATIENCE_S)


def test_a_runner_whose_parent_changes_logs_once_and_exits_with_its_own_status(
    caplog: pytest.LogCaptureFixture,
) -> None:
    parents = Parents(4242)
    sleeps = Sleeps()
    exits = Exits()
    joins = Joins(exits)
    caplog.set_level(logging.ERROR, logger="tensorplate.sidecar")

    thread = runner.watch_parent(getppid=parents, sleep=sleeps, join=joins, exit_process=exits)
    assert parents.checked_twice.wait(PATIENCE_S)
    assert not exits.called.is_set()
    parents.adopt()

    assert exits.called.wait(PATIENCE_S)
    thread.join(PATIENCE_S)
    assert exits.statuses == [runner.EXIT_PARENT_GONE]
    assert runner.EXIT_PARENT_GONE not in (0, 1, 2, 3)
    gone = [r for r in caplog.records if _GONE in r.getMessage()]
    assert [r.levelno for r in gone] == [logging.ERROR]
    assert thread.daemon
    # The pages promise a look about once a second and a second for the line.
    assert sleeps.intervals
    assert all(0 < interval <= 1.0 for interval in sleeps.intervals)
    assert joins.waits == [(1.0, False)]


def test_the_watch_uses_the_process_and_its_clock_by_default() -> None:
    defaults = {
        name: parameter.default
        for name, parameter in inspect.signature(runner.watch_parent).parameters.items()
    }

    assert defaults == {
        "poll_s": runner.PARENT_POLL_S,
        "getppid": os.getppid,
        "sleep": time.sleep,
        "join": threading.Thread.join,
        "exit_process": os._exit,
    }


def _fill(fd: int) -> None:
    os.set_blocking(fd, False)
    for size in (65536, 1):
        with contextlib.suppress(BlockingIOError):
            while True:
                os.write(fd, b"x" * size)
    os.set_blocking(fd, True)


@pytest.mark.parametrize(
    ("reader", "prelude"),
    [("open", ""), ("gone", ""), ("full", ""), ("open", _NO_LINE_THREAD)],
    ids=["open", "gone", "full", "no-line-thread"],
)
def test_an_adopted_runner_exits_whatever_its_stderr_does(reader: str, prelude: str) -> None:
    read_end, write_end = os.pipe()
    if reader == "gone":
        os.close(read_end)
    elif reader == "full":
        _fill(write_end)
    try:
        process = subprocess.Popen(
            [sys.executable, "-c", _ADOPTED_RUNNER.format(prelude=prelude)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=write_end,
        )
    finally:
        os.close(write_end)
    try:
        status = process.wait(PATIENCE_S)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        if reader != "gone":
            with os.fdopen(read_end, "rb") as stderr:
                written = stderr.read()
    assert status == runner.EXIT_PARENT_GONE
    if reader == "open" and not prelude:
        lines = written.decode().splitlines()
        assert len(lines) == 1
        assert _GONE in lines[0]
    if reader == "full":
        assert _GONE.encode() not in written


def test_the_runner_starts_watching_before_it_connects(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    order: list[str] = []

    def connect(path: str, *, timeout_s: float) -> None:
        order.append("connect")
        raise ConnectionRefusedError

    monkeypatch.setattr(runner, "watch_parent", lambda: order.append("watch"))
    monkeypatch.setattr(runner, "_connect_socket", connect)
    monkeypatch.setattr(sanitize, "configure_process_logging", lambda level: None)

    status = runner.main(["--socket", str(tmp_path / "absent.sock")])

    assert status == 2
    assert order == ["watch", "connect"]
