"""The golden speech-job frames, replayed against a live runner over a socketpair.

Each file under ``protocol/rust/tests/fixtures`` is one connection: the
adapter's frames are sent in file order and every frame the sidecar answers
with is compared with the file's, field for field. Two fields are the
sidecar's own: the ``message_id`` of a message it originates, checked as the
running ``s<n>``, and the wording of an error's ``message``.
"""

from __future__ import annotations

import contextlib
import copy
import json
import socket
import threading
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

from tensorplate_pytorch_backend import codec, jobs, protocol
from tensorplate_pytorch_backend.backends import Backend, FixtureBackend, SmolVLABackend
from tensorplate_pytorch_backend.runner import SidecarRunner

_FIXTURES = Path(__file__).resolve().parents[3] / "protocol" / "rust" / "tests" / "fixtures"
_PREFIX = "python_pytorch_ipc_speech_jobs_"
_REPLAYED = (
    "cancel",
    "interleaved_progress",
    "negotiation",
    "negotiation_refused",
    "refused",
    "session_release",
    "stt_decode",
    "tts_synthesis",
    "vad_frames",
)
_ADAPTER_KINDS = {
    protocol.KIND_LOAD_MODEL,
    protocol.KIND_JOB_SUBMIT,
    protocol.KIND_JOB_CANCEL,
    protocol.KIND_SESSION_RELEASE,
}
#: Bound of every wait on the runner: a socket read, an event, a thread join.
WAIT_S = 5.0


def golden(name: str) -> list[dict[str, Any]]:
    assert name in _REPLAYED
    text = (_FIXTURES / f"{_PREFIX}{name}.jsonl").read_text(encoding="utf-8")
    return [json.loads(line) for line in text.splitlines()]


class Peer:
    """The adapter's end of a socketpair whose other end a live runner serves."""

    def __init__(self, factories: dict[str, type[Backend]] | None = None) -> None:
        self.client, server = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        self.client.settimeout(WAIT_S)
        self.runner = SidecarRunner(server, backend_factories=factories)
        self.thread = threading.Thread(target=self.runner.serve_forever, daemon=True)
        self.gates: list[threading.Event] = []
        self._received = bytearray()
        self.thread.start()

    def send(self, header: dict[str, Any], payload: bytes | None = None) -> None:
        if payload is None:
            payload = bytes(header.get("input", {}).get("payload_length", 0))
        self.client.sendall(codec.encode(codec.SidecarFrame(header=header, payload=payload)))

    def read(self) -> codec.SidecarFrame:
        """The runner's next frame; the socket's timeout bounds the wait."""
        while True:
            try:
                frame, consumed = codec.decode_one(bytes(self._received))
            except codec.IncompleteFrame:
                chunk = self.client.recv(65536)
                assert chunk, "the runner closed the connection"
                self._received.extend(chunk)
                continue
            del self._received[:consumed]
            return frame

    def enable_jobs(self) -> FixtureBackend:
        """Load the golden model with ``speech_jobs_v1``; return the loaded fixture."""
        self.send(golden("negotiation")[1])
        response = self.read().header
        assert response["status"] == protocol.STATUS_OK, response
        backend = self.runner.state.backend
        assert isinstance(backend, FixtureBackend)
        return backend

    def hold(self, backend: FixtureBackend) -> threading.Event:
        """Hold the fixture's jobs open until the returned gate, or close(), is set."""
        gate = threading.Event()
        backend.job_gate = gate
        self.gates.append(gate)
        return gate

    def close(self) -> None:
        for gate in self.gates:
            gate.set()
        with contextlib.suppress(OSError):
            self.client.shutdown(socket.SHUT_RDWR)
        self.client.close()
        self.thread.join(timeout=WAIT_S)
        assert not self.thread.is_alive(), "the runner outlived its connection"


@pytest.fixture
def connect(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Callable[..., Peer]]:
    # The golden load_model names its runner entry by a relative path.
    entry = tmp_path / "models" / "speech-stt" / "entry.json"
    entry.parent.mkdir(parents=True)
    entry.write_text("{}", encoding="utf-8")
    monkeypatch.chdir(tmp_path)
    peers: list[Peer] = []

    def make(factories: dict[str, type[Backend]] | None = None) -> Peer:
        peers.append(Peer(factories))
        return peers[-1]

    try:
        yield make
    finally:
        for peer in peers:
            peer.close()


def expect_frame(peer: Peer, want: dict[str, Any], originated: int | None) -> codec.SidecarFrame:
    """Read one frame and compare it with ``want``.

    ``originated`` numbers a message the sidecar originates; a response keeps
    the ``message_id`` of its request.
    """
    frame = peer.read()
    got, want = copy.deepcopy(frame.header), copy.deepcopy(want)
    if originated is not None:
        want["message_id"] = f"s{originated}"
    for header in (got, want):
        if "error" in header:
            assert header["error"].pop("message")
    assert got == want
    assert len(frame.payload) == want.get("result", {}).get("payload_length", 0)
    return frame


def replay(
    peer: Peer, frames: list[dict[str, Any]], backend: FixtureBackend, *, held: bool = False
) -> None:
    """Send the adapter's frames in order and compare each frame of the sidecar.

    With ``held`` the fixture's job stays open from its start until the sidecar
    has acknowledged the cancellation, as in the trace the file carries.
    """
    gate = peer.hold(backend) if held else None
    originated = 0
    for want in frames:
        if want["kind"] in _ADAPTER_KINDS:
            if held and want["kind"] != protocol.KIND_JOB_SUBMIT:
                assert backend.job_started.wait(WAIT_S)
            peer.send(want)
            continue
        originated += 1
        expect_frame(peer, want, originated)
        if gate is not None and want["kind"] == protocol.KIND_JOB_CANCEL_ACKNOWLEDGED:
            gate.set()


def test_every_golden_file_on_disk_is_replayed() -> None:
    on_disk = [path.name[len(_PREFIX) : -len(".jsonl")] for path in _FIXTURES.glob(f"{_PREFIX}*")]
    assert sorted(on_disk) == sorted(_REPLAYED)


def test_negotiation(connect: Callable[..., Peer]) -> None:
    ready, load, response = golden("negotiation")
    peer = connect()
    peer.runner.announce_ready()
    expect_frame(peer, ready, 1)
    peer.send(load)
    got = peer.read().header
    # The frame's runner declares vad_frames, which no lane runs yet, so the
    # classes compared are the loaded fixture's. Replay the frame's own once
    # this fails.
    assert protocol.JOB_CLASS_VAD_FRAMES not in jobs.LANE_JOB_CLASSES
    assert protocol.JOB_CLASS_VAD_FRAMES in response.pop("job_classes")
    assert got.pop("job_classes") == list(FixtureBackend().job_classes())
    assert got == response


def test_negotiation_refused(connect: Callable[..., Peer]) -> None:
    _ready, load, response = golden("negotiation_refused")
    # A backend without the job interface, refused before it is asked to load.
    assert not hasattr(SmolVLABackend, "run_job")
    peer = connect({"fixture": SmolVLABackend})
    peer.send(load)
    got = peer.read().header
    assert got.pop("error")["code"] == response.pop("error")["code"] == protocol.ERR_UNSUPPORTED
    assert got == response
    assert peer.runner.state.backend is None


@pytest.mark.parametrize("name", ["stt_decode", "tts_synthesis"])
def test_a_job_runs_to_its_result(connect: Callable[..., Peer], name: str) -> None:
    peer = connect()
    replay(peer, golden(name), peer.enable_jobs())


def test_cancel(connect: Callable[..., Peer]) -> None:
    peer = connect()
    replay(peer, golden("cancel"), peer.enable_jobs(), held=True)


def test_refused(connect: Callable[..., Peer]) -> None:
    peer = connect()
    backend = peer.enable_jobs()
    backend.job_languages = frozenset({"ar"})
    replay(peer, golden("refused"), backend)


def test_session_release(connect: Callable[..., Peer]) -> None:
    frames = golden("session_release")
    # No lane runs vad_frames yet, so the session's job is the stt_decode
    # golden submit, which has the same identity; every other frame is the file's.
    submit = golden("stt_decode")[0]
    identity = ("job_id", "session_key", "generation")
    assert frames[0]["job_class"] == protocol.JOB_CLASS_VAD_FRAMES
    assert [frames[0][name] for name in identity] == [submit[name] for name in identity]
    peer = connect()
    replay(peer, [submit, *frames[1:]], peer.enable_jobs(), held=True)


def test_vad_frames_is_refused_until_it_has_a_lane(connect: Callable[..., Peer]) -> None:
    submit = golden("vad_frames")[0]
    peer = connect()
    peer.enable_jobs()
    peer.send(submit)
    identity = {name: submit[name] for name in ("job_id", "session_key", "generation")}
    failed = peer.read().header
    assert failed.pop("error") == {
        "schema_version": protocol.SCHEMA_VERSION,
        "code": protocol.ERR_UNSUPPORTED,
        "message": "the request is not supported",
        "context": "job_class_unsupported",
    }
    envelope = {"schema_version": protocol.SCHEMA_VERSION, **identity}
    assert failed == {**envelope, "message_id": "s1", "kind": protocol.KIND_JOB_FAILED}
    assert peer.read().header == {
        **envelope,
        "message_id": "s2",
        "kind": protocol.KIND_JOB_RELEASED,
    }


def test_interleaved_progress_submits_yield_no_progress(connect: Callable[..., Peer]) -> None:
    submits = [
        frame
        for frame in golden("interleaved_progress")
        if frame["kind"] == protocol.KIND_JOB_SUBMIT
    ]
    assert [frame["progress_limit"] for frame in submits] == [3, 3]
    peer = connect()
    peer.enable_jobs()
    for frame in submits:
        peer.send(frame)
    kinds: dict[int, list[str]] = {frame["job_id"]: [] for frame in submits}
    for number in range(1, 7):
        header = peer.read().header
        assert header["message_id"] == f"s{number}"
        kinds[header["job_id"]].append(header["kind"])
    whole_result = [
        protocol.KIND_JOB_ACCEPTED,
        protocol.KIND_JOB_COMPLETED,
        protocol.KIND_JOB_RELEASED,
    ]
    assert kinds == {1: whole_result, 2: whole_result}
