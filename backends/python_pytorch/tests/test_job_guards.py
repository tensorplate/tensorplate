"""Guards of the job objects and the job table that no seam vector or golden frame reaches."""

from __future__ import annotations

import copy
import json
import socket
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any, ClassVar

import pytest

from tensorplate_pytorch_backend import job_objects, protocol
from tensorplate_pytorch_backend.backends import BackendError, FixtureBackend
from test_speech_jobs_golden_replay import WAIT_S, Peer, golden
from test_speech_jobs_golden_replay import connect as connect  # the fixture
from test_speech_jobs_runner import (
    ERROR_EVENT,
    assert_quiet,
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


def test_a_failed_message_needs_its_error() -> None:
    identity = job_objects.JobIdentity(1, 1, 1)
    with pytest.raises(job_objects.MalformedJob):
        job_objects.job_event(protocol.KIND_JOB_FAILED, identity)
    with pytest.raises(job_objects.MalformedJob):
        job_objects.job_event(protocol.KIND_JOB_PROGRESS, identity)


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
