"""The runner's socket: its mode, how the runner ends a connection, and what it still
does for a connection that has ended or whose peer has half-closed."""

from __future__ import annotations

import contextlib
import errno
import os
import socket
import threading
from collections.abc import Iterator
from typing import Any, cast

import pytest

from tensorplate_pytorch_backend import codec, protocol
from tensorplate_pytorch_backend import runner as runner_module
from tensorplate_pytorch_backend.backends import Backend, FixtureBackend, NamedTensor
from test_speech_jobs_golden_replay import WAIT_S
from test_speech_jobs_runner import message


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
