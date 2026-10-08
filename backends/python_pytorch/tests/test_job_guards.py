"""Guards of the job objects and the job table that no seam vector or golden frame reaches."""

from __future__ import annotations

import copy
import json
import logging
import socket
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any, ClassVar

import pytest

from tensorplate_pytorch_backend import codec, job_objects, jobs, protocol
from tensorplate_pytorch_backend.backends import BackendError, FixtureBackend, NamedTensor
from test_speech_jobs_golden_replay import WAIT_S, Peer, golden
from test_speech_jobs_golden_replay import connect as connect  # the fixture
from test_speech_jobs_runner import (
    ACCEPTED,
    ACKNOWLEDGED,
    ERROR_EVENT,
    FAILED,
    assert_quiet,
    digest,
    ended,
    exchange,
    hold_job,
    message,
    read,
    read_after,
    submit,
)


@pytest.mark.parametrize(
    "changes",
    [
        {"job_id": -1},
        {"job_class": 7},
        {"options": 5},
        {"options": {"voice": 5}},
        {"input": 5},
        {"input": ["audio_frames", 32000]},
    ],
)
def test_an_ill_typed_submit_field_is_malformed(changes: dict[str, Any]) -> None:
    header = {**copy.deepcopy(golden("stt_decode")[0]), **changes}
    with pytest.raises(job_objects.MalformedJob):
        job_objects.read_submit(header, bytes(32000))


@pytest.mark.parametrize(
    "result",
    [
        "transcript",
        None,
        job_objects.VadResult((True,)),
        job_objects.VadResult(("0.5",)),  # type: ignore[arg-type]
        job_objects.AudioChunkResult(bytes(4), clipped_samples=-1),
    ],
)
def test_a_result_json_could_not_carry_is_malformed(result: object) -> None:
    with pytest.raises(job_objects.MalformedJob):
        job_objects.render_result(result)


def test_a_boolean_is_not_a_wire_integer_where_one_is_rendered() -> None:
    flagged = job_objects.AudioFormat(protocol.AUDIO_ENCODING_PCM_S16LE, 24000, True)
    with pytest.raises(job_objects.MalformedJob):
        job_objects.render_result(job_objects.AudioChunkResult(bytes(4), 0, flagged))
    with pytest.raises(job_objects.MalformedJob):
        job_objects.job_event(protocol.KIND_JOB_ACCEPTED, job_objects.JobIdentity(True, 1, 1))


@pytest.mark.parametrize(
    ("name", "option"),
    [("stt_decode", "voice"), ("stt_decode", "speed_milli"), ("vad_frames", "language")],
)
def test_an_option_its_class_does_not_take_is_refused_though_empty_or_zero(
    name: str, option: str
) -> None:
    header = copy.deepcopy(golden(name)[0])
    payload = bytes(header["input"]["payload_length"])
    assert job_objects.read_submit(header, payload).job_class == name
    header["options"][option] = 0 if option == "speed_milli" else ""
    with pytest.raises(job_objects.JobRefused) as refused:
        job_objects.read_submit(header, payload)
    assert refused.value.reason == "option_not_applicable"


def test_a_failed_message_needs_its_error() -> None:
    identity = job_objects.JobIdentity(1, 1, 1)
    with pytest.raises(job_objects.MalformedJob):
        job_objects.job_event(protocol.KIND_JOB_FAILED, identity)
    with pytest.raises(job_objects.MalformedJob):
        job_objects.job_event(protocol.KIND_JOB_PROGRESS, identity)


@pytest.mark.parametrize("unknown", ["job_id", "extra"])
def test_a_session_release_with_a_field_it_does_not_have_is_malformed(unknown: str) -> None:
    header = {**message(protocol.KIND_SESSION_RELEASE), unknown: 1}
    with pytest.raises(job_objects.MalformedJob):
        job_objects.read_session_release(header)


def test_capabilities_that_are_not_a_list_are_refused(connect: Callable[..., Peer]) -> None:
    peer = connect()
    # An object keyed by the capability's name would pass a membership check alone.
    capabilities = {protocol.CAPABILITY_SPEECH_JOBS_V1: True}
    load = {**golden("negotiation")[1], "capabilities": capabilities}
    assert read_after(peer, load) == ("load_model_response", "a1", "unsupported")


def test_an_empty_capability_list_is_refused(connect: Callable[..., Peer]) -> None:
    peer = connect()
    load = {**golden("negotiation")[1], "capabilities": []}
    assert read_after(peer, load) == ("load_model_response", "a1", "config_invalid")
    assert peer.runner.state.backend is None


class _DeclaresVad(FixtureBackend):
    def job_classes(self) -> tuple[str, ...]:
        return (protocol.JOB_CLASS_VAD_FRAMES, *reversed(super().job_classes()))


def test_a_declared_class_without_a_lane_is_neither_advertised_nor_run(
    connect: Callable[..., Peer],
) -> None:
    peer = connect({"fixture": _DeclaresVad})
    assert exchange(peer, golden("negotiation")[1])["job_classes"] == list(jobs.LANE_JOB_CLASSES)
    vad = golden("vad_frames")[0]
    peer.send(vad)
    assert read(peer, 2) == ended(vad["job_id"], FAILED, "unsupported", "job_class_unsupported")


def test_a_job_message_with_a_bad_envelope_does_not_reach_the_table(
    connect: Callable[..., Peer],
) -> None:
    peer = connect()
    peer.enable_jobs()
    refused = submit(1, schema_version="0.2")
    assert read_after(peer, refused) == (ERROR_EVENT, "a1", "unsupported")
    assert_quiet(peer)


class _Unencodable(FixtureBackend):
    def infer(self, inputs: list[NamedTensor]) -> list[NamedTensor]:
        return [NamedTensor("x", {"shape": object()}, b"")]


def test_an_exception_on_the_serving_thread_ends_the_connection(
    connect: Callable[..., Peer],
) -> None:
    peer = connect({"fixture": _Unencodable})
    peer.enable_jobs()
    peer.send(message(protocol.KIND_INFER))  # no frame can carry what this backend returns
    assert peer.client.recv(1) == b""
    peer.thread.join(timeout=WAIT_S)
    assert not peer.thread.is_alive()


class _SlowLoad(FixtureBackend):
    started: ClassVar[threading.Event]
    gate: ClassVar[threading.Event]

    def load(self, model_spec: dict[str, Any]) -> None:
        self.started.set()
        self.gate.wait(WAIT_S)
        super().load(model_spec)


def test_a_load_that_an_unload_already_waits_behind_enables_nothing(
    connect: Callable[..., Peer],
) -> None:
    _SlowLoad.started, _SlowLoad.gate = threading.Event(), threading.Event()
    peer = connect({"fixture": _SlowLoad})
    try:
        peer.send(golden("negotiation")[1])
        assert _SlowLoad.started.wait(WAIT_S)
        peer.send(message(protocol.KIND_UNLOAD))
        assert_quiet(peer)  # the reader has queued the unload behind the load
    finally:
        _SlowLoad.gate.set()
    assert [frame[0] for frame in read(peer, 2)] == ["load_model_response", "unload_response"]
    assert read_after(peer, submit(1)) == (ERROR_EVENT, "a1", "unsupported")


def test_work_waiting_when_the_peer_leaves_is_dropped_unanswered(
    connect: Callable[..., Peer],
) -> None:
    peer = connect()
    backend, gate = hold_job(peer)
    peer.send(message(protocol.KIND_UNLOAD))
    assert_quiet(peer)  # the reader has queued the unload behind the running job
    peer.client.shutdown(socket.SHUT_WR)
    assert peer.client.recv(1) == b""  # the runner shut the socket down in turn
    gate.set()
    peer.thread.join(timeout=WAIT_S)
    assert not peer.thread.is_alive()
    assert peer.runner.state.backend is backend  # the unload never ran


def test_a_job_that_ends_after_the_peer_left_is_not_written(
    connect: Callable[..., Peer], caplog: pytest.LogCaptureFixture
) -> None:
    peer = connect()
    _backend, gate = hold_job(peer)
    peer.client.shutdown(socket.SHUT_WR)
    assert peer.client.recv(1) == b""  # the runner shut the socket down in turn
    with caplog.at_level(logging.WARNING):
        gate.set()
        peer.thread.join(timeout=WAIT_S)
    assert not peer.thread.is_alive()
    # A write to the socket that was shut down would fail, and the failure be logged.
    assert not caplog.records


def test_a_job_cancelled_between_its_take_and_its_backend_call_is_not_run() -> None:
    written: list[tuple[Any, ...]] = []
    table = jobs.JobTable(
        lambda frame, _originated: written.append(digest(frame.header)), lambda _message: None
    )
    backend = FixtureBackend()
    table.open(backend, jobs.LANE_JOB_CLASSES)
    header = submit(1)
    assert table.handle(codec.SidecarFrame(header, bytes(header["input"]["payload_length"])))
    table.stop(drain=True)  # a take of a stopped lane does not wait when nothing was queued
    job = table.take()
    assert isinstance(job, jobs.Job)
    assert table.handle(codec.SidecarFrame(message(protocol.KIND_JOB_CANCEL, 1)))
    table.run(job)
    assert not backend.job_started.is_set()
    assert written == [(ACCEPTED, 1), (ACKNOWLEDGED, 1), *ended(1, FAILED, "cancelled")]


def test_the_fixture_records_a_call_made_while_another_runs() -> None:
    backend = FixtureBackend()
    backend.load({})
    backend.prime()
    backend.job_gate = threading.Event()
    request = job_objects.read_submit(golden("stt_decode")[0], bytes(32000))
    job = threading.Thread(target=backend.run_job, args=(request,), daemon=True)
    job.start()
    try:
        assert backend.job_started.wait(WAIT_S)
        assert not backend.overlapped
        backend.infer([NamedTensor("x", {"dtype": "uint8", "shape": [1]}, b"\0")])
        assert backend.overlapped
    finally:
        backend.job_gate.set()
        job.join(timeout=WAIT_S)
    assert not job.is_alive()


@pytest.mark.parametrize("delay", [True, -1, 60_001, "5"])
def test_the_fixture_refuses_a_job_delay_outside_its_range(tmp_path: Path, delay: object) -> None:
    entry = tmp_path / "entry.json"
    entry.write_text(json.dumps({"job_delay_ms": delay}), encoding="utf-8")
    with pytest.raises(BackendError) as refused:
        FixtureBackend().load({"artifact_path": str(entry)})
    assert refused.value.code == protocol.ERR_CONFIG_INVALID


def test_the_fixture_holds_a_job_open_for_its_entry_delay(tmp_path: Path) -> None:
    entry = tmp_path / "entry.json"
    entry.write_text('{"job_delay_ms": 80}', encoding="utf-8")
    backend = FixtureBackend()
    backend.load({"artifact_path": str(entry)})
    request = job_objects.read_submit(golden("stt_decode")[0], bytes(32000))
    started = time.monotonic()
    assert isinstance(backend.run_job(request), job_objects.TranscriptResult)
    assert time.monotonic() - started >= 0.08
