"""The runner's socket: its mode, how the runner ends a connection, and what it still
does for a connection that has ended or whose peer has half-closed."""

from __future__ import annotations

import contextlib
import errno
import itertools
import os
import socket
import threading
from collections.abc import Callable, Iterator
from typing import Any, cast

import pytest

from tensorplate_pytorch_backend import codec, jobs, protocol
from tensorplate_pytorch_backend import runner as runner_module
from tensorplate_pytorch_backend.backends import Backend, FixtureBackend, NamedTensor
from test_speech_jobs_golden_replay import WAIT_S, Peer
from test_speech_jobs_golden_replay import connect as connect  # the fixture
from test_speech_jobs_runner import ACCEPTED, message, read_after, submit


@contextlib.contextmanager
def _socketpair() -> Iterator[tuple[socket.socket, socket.socket]]:
    client, server = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(WAIT_S)
    try:
        yield client, server
    finally:
        client.close()
        server.close()


def _serve(sock: object, factories: dict[str, type[Backend]] | None = None) -> threading.Thread:
    runner = runner_module.SidecarRunner(cast(socket.socket, sock), backend_factories=factories)
    serving = threading.Thread(target=runner.serve_forever, daemon=True)
    serving.start()
    return serving


def _send(client: socket.socket, *headers: dict[str, Any]) -> None:
    client.sendall(b"".join(codec.encode(codec.SidecarFrame(header)) for header in headers))


class _Quirky:
    """The runner's socket with two quirks of a platform: SHUT_RDWR refused, as on macOS
    once the peer has half-closed, and a first recv that finds nothing to read."""

    def __init__(self, sock: socket.socket) -> None:
        self._sock = sock
        self.shutdowns: list[int] = []
        self._found_nothing = False

    def __getattr__(self, name: str) -> Any:
        return getattr(self._sock, name)

    def recv(self, size: int) -> bytes:
        if not self._found_nothing:
            self._found_nothing = True
            raise BlockingIOError(errno.EAGAIN, os.strerror(errno.EAGAIN))
        return self._sock.recv(size)

    def shutdown(self, how: int) -> None:
        self.shutdowns.append(how)
        if how == socket.SHUT_RDWR:
            raise OSError(errno.ENOTCONN, os.strerror(errno.ENOTCONN))
        self._sock.shutdown(how)


class _Unencodable(FixtureBackend):
    def infer(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        return [NamedTensor("x", {"shape": object()}, b"")]


def _answered(client: socket.socket) -> bool:
    """Whether the runner answers a health_check."""
    _send(client, message(protocol.KIND_HEALTH_CHECK))
    return bool(codec.decode_one(client.recv(65536))[0].header["status"] == protocol.STATUS_OK)


def test_the_socket_is_non_blocking_from_construction_and_stays_so() -> None:
    with _socketpair() as (client, server):
        runner = runner_module.SidecarRunner(server)
        assert not server.getblocking()
        serving = threading.Thread(target=runner.serve_forever, daemon=True)
        serving.start()
        assert _answered(client)
        assert not server.getblocking()
        client.close()
        serving.join(timeout=WAIT_S)
        assert not serving.is_alive()
        assert not server.getblocking()


def test_a_recv_that_finds_nothing_waits_for_the_peer_again() -> None:
    with _socketpair() as (client, server):
        _serve(_Quirky(server))
        assert _answered(client)


def test_a_refused_shutdown_of_both_directions_falls_back_to_the_write_side() -> None:
    with _socketpair() as (client, server):
        proxy = _Quirky(server)
        serving = _serve(proxy)
        client.shutdown(socket.SHUT_WR)
        assert client.recv(1) == b""  # the fallback's EOF
        serving.join(timeout=WAIT_S)
        assert not serving.is_alive()
        assert proxy.shutdowns == [socket.SHUT_RDWR, socket.SHUT_WR]


def test_the_reader_stops_without_a_shutdown_to_wake_it(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(runner_module, "READ_POLL_S", 0.05)
    with _socketpair() as (client, server):
        # Only the write side gets shut down, and that wakes no wait for the peer's bytes.
        serving = _serve(_Quirky(server), {"fixture": _Unencodable})
        # No frame can carry what this backend returns, so the serving thread ends the connection.
        load = message(protocol.KIND_LOAD_MODEL, model_spec={})
        _send(client, load, message(protocol.KIND_INFER))
        while client.recv(65536):
            pass
        serving.join(timeout=WAIT_S)
        assert not serving.is_alive()


_Work = jobs.Job | codec.SidecarFrame | None


def _hold_the_serving_thread(
    monkeypatch: pytest.MonkeyPatch,
) -> tuple[threading.Event, list[_Work]]:
    """Let the serving thread take the load, then hold it before its next take, past its look
    at the ended flag, until the reader is about to stop the lane; hold the reader there until
    that take has returned. Returns the event set once it is held and what it then took."""
    held, release, returned = threading.Event(), threading.Event(), threading.Event()
    take, stop, takes = jobs.JobTable.take, jobs.JobTable.stop, itertools.count()
    taken: list[_Work] = []

    def held_take(table: jobs.JobTable) -> _Work:
        if next(takes) == 0:
            return take(table)
        held.set()
        release.wait(WAIT_S)
        try:
            taken.append(take(table))
        finally:
            returned.set()
        return taken[-1]

    def held_stop(table: jobs.JobTable, *, drain: bool = False) -> None:
        release.set()
        returned.wait(WAIT_S)
        stop(table, drain=drain)

    monkeypatch.setattr(jobs.JobTable, "take", held_take)
    monkeypatch.setattr(jobs.JobTable, "stop", held_stop)
    return held, taken


def _fail_the_write_made_in(
    method: str, peer: Peer, header: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    """Send ``header`` and close the peer's end while the reader is held on its way into
    the table's ``method``, so the write made there ends the connection."""
    entered, closed = threading.Event(), threading.Event()
    original = getattr(jobs.JobTable, method)

    def held(*arguments: Any) -> Any:
        entered.set()
        closed.wait(WAIT_S)
        return original(*arguments)

    monkeypatch.setattr(jobs.JobTable, method, held)
    peer.send(header)
    assert entered.wait(WAIT_S)
    peer.client.close()
    closed.set()
    peer.thread.join(timeout=WAIT_S)
    assert not peer.thread.is_alive()


def test_a_job_queued_after_its_job_accepted_write_ended_the_connection_is_not_run(
    connect: Callable[..., Peer], monkeypatch: pytest.MonkeyPatch
) -> None:
    held, taken = _hold_the_serving_thread(monkeypatch)
    peer = connect()
    backend = peer.enable_jobs()
    assert held.wait(WAIT_S)
    _fail_the_write_made_in("handle", peer, submit(1), monkeypatch)
    assert [type(work) for work in taken] == [jobs.Job]
    assert not backend.job_started.is_set()


def test_an_unload_queued_after_a_waiting_jobs_failure_ended_the_connection_is_not_run(
    connect: Callable[..., Peer], monkeypatch: pytest.MonkeyPatch
) -> None:
    held, taken = _hold_the_serving_thread(monkeypatch)
    peer = connect()
    backend = peer.enable_jobs()
    assert held.wait(WAIT_S)
    assert read_after(peer, submit(1)) == (ACCEPTED, 1)  # and it waits: the serving thread is held
    _fail_the_write_made_in("offer", peer, message(protocol.KIND_UNLOAD), monkeypatch)
    assert [type(work) for work in taken] == [codec.SidecarFrame]
    assert peer.runner.state.backend is backend
