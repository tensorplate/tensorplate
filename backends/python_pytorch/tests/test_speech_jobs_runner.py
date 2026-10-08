"""Job and session messages against a live runner: admission, the lane, cancellation,
session release, unload, and what the two threads may do while a job runs."""

from __future__ import annotations

import contextlib
import copy
import itertools
import json
import logging
import socket
import threading
import time
from collections.abc import Callable
from typing import Any, ClassVar

import pytest

from tensorplate_pytorch_backend import codec, job_objects, jobs, protocol, sanitize
from tensorplate_pytorch_backend import runner as runner_module
from tensorplate_pytorch_backend.backends import BackendError, FixtureBackend
from test_speech_jobs_golden_replay import WAIT_S, Peer, golden
from test_speech_jobs_golden_replay import connect as connect  # the fixture

ACCEPTED, COMPLETED, FAILED, RELEASED, ACKNOWLEDGED, SESSION_RELEASED, ERROR_EVENT = (
    protocol.KIND_JOB_ACCEPTED,
    protocol.KIND_JOB_COMPLETED,
    protocol.KIND_JOB_FAILED,
    protocol.KIND_JOB_RELEASED,
    protocol.KIND_JOB_CANCEL_ACKNOWLEDGED,
    protocol.KIND_SESSION_RELEASED,
    protocol.KIND_ERROR_EVENT,
)
CANARY = "canary-7f3a9c"
_numbers = itertools.count(1)


def submit(job_id: int, session_key: int = 1, **changes: Any) -> dict[str, Any]:
    """The golden stt_decode submit with another identity; its message_id is ``a<job_id>``."""
    header = copy.deepcopy(golden("stt_decode")[0])
    header.update(message_id=f"a{job_id}", job_id=job_id, session_key=session_key, **changes)
    return header


def message(
    kind: str, job_id: int | None = None, session_key: int = 1, **more: Any
) -> dict[str, Any]:
    header = {"schema_version": "0.1", "message_id": f"m{next(_numbers)}", "kind": kind, **more}
    if kind.startswith(("job_", "session_")):
        header.update(session_key=session_key, generation=1)
    if job_id is not None:
        header["job_id"] = job_id
    return header


def digest(header: dict[str, Any]) -> tuple[Any, ...]:
    """A frame as (kind, job or session or answered message, error code, error context)."""
    error = header.get("error", {})
    subject = (
        header["message_id"]
        if "status" in header
        else header.get("job_id", header.get("session_key"))
    )
    parts = (header["kind"], subject, error.get("code"), error.get("context"))
    return tuple(part for part in parts if part is not None)


def read(peer: Peer, count: int) -> list[tuple[Any, ...]]:
    return [digest(peer.read().header) for _ in range(count)]


def exchange(peer: Peer, header: dict[str, Any], payload: bytes | None = None) -> dict[str, Any]:
    peer.send(header, payload)
    return peer.read().header


def assert_quiet(peer: Peer) -> None:
    """Nothing is on its way from the reader thread: a health_check is answered next."""
    request = message(protocol.KIND_HEALTH_CHECK)
    assert digest(exchange(peer, request)) == ("health_check_response", request["message_id"])


def hold_job(peer: Peer, job_id: int = 1) -> tuple[FixtureBackend, threading.Event]:
    """Enable jobs and leave job ``job_id`` running, held open by the returned gate."""
    backend = peer.enable_jobs()
    gate = peer.hold(backend)
    assert read_after(peer, submit(job_id)) == (ACCEPTED, job_id)
    assert backend.job_started.wait(WAIT_S)
    return backend, gate


def read_after(peer: Peer, header: dict[str, Any], payload: bytes | None = None) -> tuple[Any, ...]:
    return digest(exchange(peer, header, payload))


def ended(job_id: int, kind: str = COMPLETED, *error: str) -> list[tuple[Any, ...]]:
    return [(kind, job_id, *error), (RELEASED, job_id)]


def test_health_and_cancel_do_not_wait_for_the_backend(connect: Callable[..., Peer]) -> None:
    peer = connect()
    _backend, gate = hold_job(peer)
    started = time.monotonic()
    assert_quiet(peer)
    assert read_after(peer, message(protocol.KIND_JOB_CANCEL, 1)) == (ACKNOWLEDGED, 1)
    assert time.monotonic() - started < 1.0
    peer.send(message(protocol.KIND_JOB_CANCEL, 1))  # a repeat says nothing
    assert_quiet(peer)
    gate.set()
    assert read(peer, 2) == ended(1, FAILED, "cancelled")
    peer.send(message(protocol.KIND_JOB_CANCEL, 1))  # nor does one after the terminal message
    peer.send(message(protocol.KIND_JOB_CANCEL, 99))  # or for a job never submitted
    assert_quiet(peer)


def test_the_lane_is_bounded_and_runs_in_submission_order(connect: Callable[..., Peer]) -> None:
    peer = connect()
    _backend, gate = hold_job(peer)
    for job_id in range(2, 2 + jobs.GPU_LANE_QUEUE_DEPTH):
        peer.send(submit(job_id))
    assert read(peer, 8) == [(ACCEPTED, job_id) for job_id in range(2, 10)]
    peer.send(submit(10))
    assert read(peer, 2) == ended(10, FAILED, "resource_exhausted", "job_capacity_exhausted")
    # The id of an unreleased job: refused without touching that job's stream.
    assert read_after(peer, submit(5)) == (ERROR_EVENT, "a5", "config_invalid", "duplicate_job_id")
    mismatched = message(protocol.KIND_JOB_CANCEL, 6, session_key=2)
    assert read_after(peer, mismatched) == (
        ERROR_EVENT,
        mismatched["message_id"],
        "config_invalid",
        "identity_mismatch",
    )
    peer.send(message(protocol.KIND_JOB_CANCEL, 5))  # a waiting job ends at once
    assert read(peer, 3) == [(ACKNOWLEDGED, 5), *ended(5, FAILED, "cancelled")]
    assert read_after(peer, submit(10)) == (ACCEPTED, 10)  # room again, and the id is free
    gate.set()
    order = [1, 2, 3, 4, 6, 7, 8, 9, 10]
    assert read(peer, 18) == [frame for job_id in order for frame in ended(job_id)]


def test_session_release_cancels_its_jobs_and_is_then_forgotten(
    connect: Callable[..., Peer],
) -> None:
    peer = connect()
    _backend, gate = hold_job(peer)
    peer.send(submit(2))
    peer.send(submit(3, session_key=2))
    assert read(peer, 2) == [(ACCEPTED, 2), (ACCEPTED, 3)]
    peer.send(message(protocol.KIND_SESSION_RELEASE))
    assert read(peer, 4) == [(ACKNOWLEDGED, 1), (ACKNOWLEDGED, 2), *ended(2, FAILED, "cancelled")]
    peer.send(message(protocol.KIND_SESSION_RELEASE))  # a repeat while pending is ignored
    peer.send(submit(4))
    assert read(peer, 2) == ended(4, FAILED, "not_ready", "session_releasing")
    # A session with no job is released at once.
    assert read_after(peer, message(protocol.KIND_SESSION_RELEASE, session_key=9)) == (
        SESSION_RELEASED,
        9,
    )
    gate.set()
    assert read(peer, 5) == [*ended(1, FAILED, "cancelled"), (SESSION_RELEASED, 1), *ended(3)]
    peer.send(submit(5))
    assert read(peer, 3) == [(ACCEPTED, 5), *ended(5)]


def test_unload_fails_waiting_jobs_and_disables_job_messages(connect: Callable[..., Peer]) -> None:
    peer = connect()
    _backend, gate = hold_job(peer)
    assert read_after(peer, submit(2)) == (ACCEPTED, 2)
    unload = message(protocol.KIND_UNLOAD)
    peer.send(unload)
    assert read(peer, 2) == ended(2, FAILED, "unavailable", "backend_unavailable")
    peer.send(submit(3))
    gate.set()
    assert read(peer, 4) == [
        *ended(1),
        ("unload_response", unload["message_id"]),
        (ERROR_EVENT, "a3", "unsupported"),
    ]
    peer.enable_jobs()
    peer.send(submit(4))
    assert read(peer, 3) == [(ACCEPTED, 4), *ended(4)]
    # A load that an unload already waits behind enables nothing.
    peer.send(golden("negotiation")[1])
    peer.send(message(protocol.KIND_UNLOAD))
    peer.send(submit(5))
    assert [frame[0] for frame in read(peer, 3)] == [
        "load_model_response",
        "unload_response",
        ERROR_EVENT,
    ]


def test_unary_requests_wait_behind_the_job_and_are_bounded(connect: Callable[..., Peer]) -> None:
    peer = connect()
    backend = peer.enable_jobs()
    assert exchange(peer, message(protocol.KIND_PRIME))["status"] == protocol.STATUS_OK
    gate = peer.hold(backend)
    assert read_after(peer, submit(1)) == (ACCEPTED, 1)
    assert backend.job_started.wait(WAIT_S)
    tensor = {
        "name": "x",
        "tensor": {"dtype": "uint8", "shape": [1]},
        "payload_offset": 0,
        "payload_length": 1,
    }
    infers = [
        message(protocol.KIND_INFER, tensors=[tensor]) for _ in range(jobs.UNARY_QUEUE_DEPTH + 1)
    ]
    for infer in infers:
        peer.send(infer, b"\0")
    assert read(peer, 1) == [("infer_response", infers[-1]["message_id"], "resource_exhausted")]
    assert_quiet(peer)
    gate.set()
    answered = [("infer_response", infer["message_id"]) for infer in infers[:-1]]
    assert read(peer, 10) == [*ended(1), *answered]
    assert not backend.overlapped


def test_a_peer_that_stops_reading_ends_the_runner(
    connect: Callable[..., Peer], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(runner_module, "WRITE_STALL_TIMEOUT_S", 0.2)
    peer = connect()
    flood = codec.encode(codec.SidecarFrame(message(protocol.KIND_HEALTH_CHECK))) * 20_000
    with contextlib.suppress(OSError):  # the runner shuts the socket down under the flood
        peer.client.sendall(flood)
    peer.thread.join(timeout=WAIT_S)
    assert not peer.thread.is_alive()


class _NoClasses(FixtureBackend):
    unloaded: ClassVar[int] = 0

    def job_classes(self) -> tuple[str, ...]:
        return ()

    def unload(self) -> None:
        type(self).unloaded += 1


def test_a_load_enables_jobs_only_for_a_listed_capability_and_a_runner_with_a_lane(
    connect: Callable[..., Peer],
) -> None:
    load = golden("negotiation")[1]
    peer = connect({"fixture": _NoClasses})
    assert read_after(peer, load) == ("load_model_response", "a1", "unsupported")
    assert (_NoClasses.unloaded, peer.runner.state.backend) == (1, None)
    peer = connect()
    for capabilities in (["speech_jobs_v1", "speech_jobs_v2"], "speech_jobs_v1"):
        refused = {**load, "capabilities": capabilities}
        assert read_after(peer, refused) == ("load_model_response", "a1", "unsupported")
    plain = {name: value for name, value in load.items() if name != "capabilities"}
    assert exchange(peer, plain) == {
        "schema_version": "0.1",
        "message_id": "a1",
        "kind": "load_model_response",
        "status": "ok",
    }
    assert read_after(peer, submit(1)) == (ERROR_EVENT, "a1", "unsupported")


def test_messages_the_table_cannot_act_on(connect: Callable[..., Peer]) -> None:
    peer = connect()
    backend = peer.enable_jobs()
    assert read_after(peer, submit(1, extra=1)) == (ERROR_EVENT, "a1", "config_invalid")
    assert read_after(peer, submit(0)) == (ERROR_EVENT, "a0", "config_invalid", "job_id_zero")
    stray = message(protocol.KIND_JOB_CANCEL, 1, extra=1)
    assert read_after(peer, stray) == (ERROR_EVENT, stray["message_id"], "config_invalid")
    stray = message(protocol.KIND_SESSION_RELEASE, session_key=0)
    assert read_after(peer, stray) == (
        ERROR_EVENT,
        stray["message_id"],
        "config_invalid",
        "session_key_zero",
    )
    # A request the seam refuses for a reason is a job that fails.
    peer.send(submit(1, options={"language": "e"}))
    assert read(peer, 2) == ended(1, FAILED, "config_invalid", "language_invalid")
    backend.job_languages = frozenset({"ar"})
    peer.send(submit(1, job_class="tts_synthesis"))  # the class mismatch comes before the permit
    assert read(peer, 2) == ended(1, FAILED, "config_invalid", "payload_class_mismatch")


class _Returning(FixtureBackend):
    outcome: ClassVar[Any] = None

    def run_job(self, request: job_objects.JobRequest) -> job_objects.JobResult:
        if isinstance(self.outcome, BaseException):
            raise self.outcome
        return self.outcome  # type: ignore[no-any-return]


def _chained() -> BackendError:
    try:
        raise ValueError(CANARY)
    except ValueError as cause:
        error = BackendError(protocol.ERR_INFERENCE_FAILED, f"could not read {CANARY}")
        error.__cause__ = cause
        return error


_INTERNAL = sanitize.MESSAGES[protocol.ERR_INTERNAL]
_TOKENS = tuple(range(protocol.LIMIT_TRANSCRIPT_TOKENS_MAX + 1))


# Each case: what run_job raises or returns, the job's failure, and what health keeps.
@pytest.mark.parametrize(
    ("outcome", "failure", "last_error"),
    [
        (RuntimeError(CANARY), ("internal",), _INTERNAL),
        (MemoryError(CANARY), ("oom_error",), sanitize.MESSAGES[protocol.ERR_OOM_ERROR]),
        (_chained(), ("inference_failed",), sanitize.MESSAGES[protocol.ERR_INFERENCE_FAILED]),
        (BackendError(protocol.ERR_TIMEOUT, "m" * 513), ("timeout",), "m" * 513),
        (job_objects.VadResult((0.5,)), ("inference_failed", "result_kind_mismatch"), None),
        (
            job_objects.TranscriptResult(CANARY, _TOKENS),
            ("inference_failed", "token_count_exceeded"),
            None,
        ),
        (job_objects.TranscriptResult(CANARY, (1.5,)), ("internal",), _INTERNAL),  # type: ignore[arg-type]
        (CANARY, ("internal",), _INTERNAL),
    ],
)
def test_a_failed_job_says_nothing_of_the_request_or_the_exception(
    connect: Callable[..., Peer],
    caplog: pytest.LogCaptureFixture,
    outcome: Any,
    failure: tuple[str, ...],
    last_error: str | None,
) -> None:
    _Returning.outcome = outcome
    peer = connect({"fixture": _Returning})
    peer.enable_jobs()
    with caplog.at_level(logging.DEBUG):
        accepted = exchange(peer, submit(1, options={"language": "zz-canary"}))
        failed, released = peer.read().header, peer.read().header
        health = exchange(peer, message(protocol.KIND_HEALTH_CHECK))
    assert [digest(frame) for frame in (accepted, failed, released)] == [
        (ACCEPTED, 1),
        *ended(1, FAILED, *failure),
    ]
    assert failed["error"]["message"] == sanitize.MESSAGES[failure[0]]
    assert health["health"]["last_error"] == last_error
    said = json.dumps([failed, released, health]) + caplog.text
    assert "canary" not in said.lower()


def test_an_authored_backend_error_reaches_the_job_and_health(connect: Callable[..., Peer]) -> None:
    peer = connect()
    backend = peer.enable_jobs()
    backend.fail_job = (protocol.ERR_SHAPE_MISMATCH, "the window is too long")
    peer.send(submit(1))
    failed = [peer.read().header for _ in range(3)][1]
    assert failed["error"] == {
        "schema_version": "0.1",
        "code": "shape_mismatch",
        "message": "the window is too long",
    }
    health = exchange(peer, message(protocol.KIND_HEALTH_CHECK))["health"]
    assert health["last_error"] == "the window is too long"


def test_max_iterations_answers_what_was_read_and_returns() -> None:
    client, server = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        client.settimeout(WAIT_S)
        for _ in range(3):
            client.sendall(codec.encode(codec.SidecarFrame(message(protocol.KIND_PRIME))))
        serving = threading.Thread(
            target=runner_module.SidecarRunner(server).serve_forever,
            kwargs={"max_iterations": 2},
            daemon=True,
        )
        serving.start()
        serving.join(timeout=WAIT_S)
        assert not serving.is_alive()
        received = client.recv(65536)
        first, consumed = codec.decode_one(received)
        second, _ = codec.decode_one(received[consumed:])
        assert [frame.header["error"]["code"] for frame in (first, second)] == ["not_ready"] * 2
        client.setblocking(False)
        with pytest.raises(BlockingIOError):
            client.recv(1)
    finally:
        client.close()
        server.close()
