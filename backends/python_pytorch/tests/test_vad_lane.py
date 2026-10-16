"""The ``vad_frames`` lane against a live runner: a thread of its own beside the main
lane, its bound, the fixture's scores, and the voice-activity state the table keeps."""

from __future__ import annotations

import array
import copy
import math
import sys
import threading
from collections.abc import Callable
from typing import Any, ClassVar

import pytest

from tensorplate_pytorch_backend import codec, jobs, protocol
from tensorplate_pytorch_backend.backends import BackendError, FixtureBackend
from tensorplate_pytorch_backend.job_objects import JobRequest, VadResult
from test_speech_jobs_golden_replay import WAIT_S, Peer, golden
from test_speech_jobs_golden_replay import connect as connect  # the fixture
from test_speech_jobs_runner import (
    ACCEPTED,
    ACKNOWLEDGED,
    FAILED,
    SESSION_RELEASED,
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

FRAME = 512


def pcm(*amplitudes: int) -> bytes:
    """One frame of each constant amplitude, as little-endian s16 samples."""
    samples = array.array("h", [amplitude for amplitude in amplitudes for _ in range(FRAME)])
    if sys.byteorder == "big":
        samples.byteswap()
    return samples.tobytes()


def vad_submit(
    job_id: int, session_key: int = 1, utterance_id: int = 1, frames: int = 3, **changes: Any
) -> dict[str, Any]:
    """The golden vad_frames submit with another identity, utterance and frame count."""
    header = copy.deepcopy(golden("vad_frames")[0])
    header.update(message_id=f"a{job_id}", job_id=job_id, session_key=session_key, **changes)
    frame_samples = header["input"]["frame_samples"]
    header["input"].update(
        utterance_id=utterance_id, frame_count=frames, payload_length=frames * frame_samples * 2
    )
    return header


def hold_vad(
    peer: Peer, backend: FixtureBackend, job_id: int, session_key: int = 2, utterance_id: int = 1
) -> None:
    """Leave VAD job ``job_id`` running, held open by the gate ``peer.hold`` gave ``backend``."""
    backend.vad_started.clear()
    assert read_after(peer, vad_submit(job_id, session_key, utterance_id)) == (ACCEPTED, job_id)
    assert backend.vad_started.wait(WAIT_S)


def test_the_fixture_scores_each_frame_by_its_peak_and_takes_only_512_sample_frames(
    connect: Callable[..., Peer],
) -> None:
    peer = connect()
    peer.enable_jobs()
    peer.send(vad_submit(1, frames=5), pcm(0, 16384, -32768, 24576, 1))
    assert digest(peer.read().header) == (ACCEPTED, 1)
    completed = peer.read().header
    assert completed["result"]["probabilities"] == [0.0, 0.5, 1.0, 0.75, 1 / 32768]
    assert digest(peer.read().header) == ended(1)[1]
    other = vad_submit(2, frames=2)
    other["input"].update(frame_samples=768, payload_length=2 * 768 * 2)
    peer.send(other)
    assert read(peer, 2) == ended(2, FAILED, "unsupported", "job_not_permitted")


def test_a_vad_job_runs_while_a_decode_job_is_held(connect: Callable[..., Peer]) -> None:
    peer = connect()
    backend, gate = hold_job(peer)
    backend.vad_gate = None
    # Held on a gate, the decode releases the GIL, as a native decode call does.
    peer.send(vad_submit(2, session_key=2))
    assert read(peer, 3) == [(ACCEPTED, 2), *ended(2)]
    assert not gate.is_set()
    gate.set()
    assert read(peer, 2) == ended(1)


def test_a_decode_job_runs_while_a_vad_job_is_held(connect: Callable[..., Peer]) -> None:
    peer = connect()
    backend = peer.enable_jobs()
    gate = peer.hold(backend)
    backend.job_gate = None
    hold_vad(peer, backend, 1)
    peer.send(submit(2))
    assert read(peer, 3) == [(ACCEPTED, 2), *ended(2)]
    gate.set()
    assert read(peer, 2) == ended(1)


def test_health_and_cancel_are_answered_while_both_lanes_are_held(
    connect: Callable[..., Peer],
) -> None:
    peer = connect()
    backend, gate = hold_job(peer)
    hold_vad(peer, backend, 2)
    assert_quiet(peer)
    assert read_after(peer, message(protocol.KIND_JOB_CANCEL, 2, session_key=2)) == (
        ACKNOWLEDGED,
        2,
    )
    assert read_after(peer, message(protocol.KIND_JOB_CANCEL, 1)) == (ACKNOWLEDGED, 1)
    gate.set()
    frames = read(peer, 4)
    assert sorted(frames) == sorted(
        [*ended(1, FAILED, "cancelled"), *ended(2, FAILED, "cancelled")]
    )


def test_each_lane_is_bounded_on_its_own_and_runs_in_submission_order(
    connect: Callable[..., Peer],
) -> None:
    peer = connect()
    backend, gate = hold_job(peer)
    hold_vad(peer, backend, 100)
    gpu = range(2, 2 + jobs.GPU_LANE_QUEUE_DEPTH)
    vad = range(101, 101 + jobs.VAD_LANE_QUEUE_DEPTH)
    for job_id in gpu:
        peer.send(submit(job_id))
    for job_id in vad:
        peer.send(vad_submit(job_id, session_key=2))
    assert read(peer, 16) == [(ACCEPTED, job_id) for job_id in [*gpu, *vad]]
    for refused in (submit(10), vad_submit(109, session_key=2)):
        peer.send(refused)
        job_id = refused["job_id"]
        assert read(peer, 2) == ended(
            job_id, FAILED, "resource_exhausted", "job_capacity_exhausted"
        )
    gate.set()
    frames = read(peer, 36)
    by_lane = [frame for frame in frames if frame[1] >= 100], [f for f in frames if f[1] < 100]
    assert by_lane == (
        [frame for job_id in [100, *vad] for frame in ended(job_id)],
        [frame for job_id in [1, *gpu] for frame in ended(job_id)],
    )


class _Scores(FixtureBackend):
    """Returns ``scores``, or raises ``failure``, in place of the fixture's scores."""

    scores: ClassVar[list[float] | None] = None
    failure: ClassVar[BackendError | None] = None

    def run_vad(self, request: JobRequest, state: object | None) -> tuple[VadResult, object]:
        result, state = super().run_vad(request, state)
        if self.failure is not None:
            raise self.failure
        return (result if self.scores is None else VadResult(self.scores)), state


@pytest.mark.parametrize(
    ("scores", "context"),
    [
        ([math.nan, 0.5, 0.5], "vad_probability_out_of_range"),
        ([math.inf, 0.5, 0.5], "vad_probability_out_of_range"),
        ([1.5, 0.5, 0.5], "vad_probability_out_of_range"),
        ([0.5, 0.5], "vad_result_count_mismatch"),
    ],
)
def test_scores_the_wire_cannot_carry_fail_the_job_once(
    connect: Callable[..., Peer], monkeypatch: pytest.MonkeyPatch, scores: list[float], context: str
) -> None:
    monkeypatch.setattr(_Scores, "scores", scores)
    peer = connect({"fixture": _Scores})
    peer.enable_jobs()
    peer.send(vad_submit(1))
    assert read(peer, 3) == [(ACCEPTED, 1), *ended(1, FAILED, "inference_failed", context)]
    assert_quiet(peer)


class _Recording(FixtureBackend):
    """Records the state each VAD job is given and returns its job id as the next state."""

    given: ClassVar[list[tuple[int, object]]] = []
    failing: ClassVar[set[int]] = set()

    def run_vad(self, request: JobRequest, state: object | None) -> tuple[VadResult, object]:
        result, _ = super().run_vad(request, state)
        job_id = request.identity.job_id
        self.given.append((job_id, state))
        if job_id in self.failing:
            raise BackendError(protocol.ERR_INFERENCE_FAILED, "the frames could not be scored")
        return result, f"after {job_id}"


def test_the_table_keeps_each_sessions_state_for_its_utterance_and_drops_it_on_any_other_end(
    connect: Callable[..., Peer], monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(_Recording, "given", [])
    monkeypatch.setattr(_Recording, "failing", {4})
    peer = connect({"fixture": _Recording})
    backend = peer.enable_jobs()

    def run(job_id: int, session_key: int = 1, utterance_id: int = 1) -> None:
        peer.send(vad_submit(job_id, session_key, utterance_id))
        assert read(peer, 3) == [(ACCEPTED, job_id), *ended(job_id)]

    run(1)
    run(2)
    run(3, utterance_id=2)  # a new utterance starts from reset state
    run(9, session_key=2, utterance_id=2)  # another session's state is its own
    peer.send(vad_submit(4, utterance_id=2))
    assert read(peer, 3) == [(ACCEPTED, 4), *ended(4, FAILED, "inference_failed")]
    run(5, utterance_id=2)
    gate = peer.hold(backend)
    hold_vad(peer, backend, 6, session_key=1, utterance_id=2)
    peer.send(message(protocol.KIND_JOB_CANCEL, 6))
    assert read(peer, 1) == [(ACKNOWLEDGED, 6)]
    gate.set()
    assert read(peer, 2) == ended(6, FAILED, "cancelled")
    run(7, utterance_id=2)
    assert read_after(peer, message(protocol.KIND_SESSION_RELEASE)) == (SESSION_RELEASED, 1)
    run(8, utterance_id=2)
    assert exchange(peer, golden("negotiation")[1])["status"] == protocol.STATUS_OK
    run(10, utterance_id=2)
    reloaded = peer.runner.state.backend
    assert isinstance(reloaded, _Recording)
    peer.hold(reloaded)
    hold_vad(peer, reloaded, 20)
    assert read_after(peer, vad_submit(11, utterance_id=2)) == (ACCEPTED, 11)
    peer.send(message(protocol.KIND_JOB_CANCEL, 11))
    assert read(peer, 3) == [(ACKNOWLEDGED, 11), *ended(11, FAILED, "cancelled")]
    peer.gates[-1].set()
    assert read(peer, 2) == ended(20)
    run(12, utterance_id=2)
    assert _Recording.given == [
        (1, None),
        (2, "after 1"),
        (3, None),
        (9, None),
        (4, "after 3"),
        (5, None),  # a failed job leaves none
        (6, "after 5"),
        (7, None),  # nor does a cancelled one
        (8, None),  # nor a released session
        (10, None),  # nor a backend loaded again
        (20, None),
        (12, None),  # nor a cancelled job that never ran
    ]


@pytest.mark.parametrize("end", ["cancelled", "refused", "refused in another session"])
def test_a_job_that_does_not_complete_beside_a_running_one_leaves_that_one_no_state(
    connect: Callable[..., Peer], monkeypatch: pytest.MonkeyPatch, end: str
) -> None:
    monkeypatch.setattr(_Recording, "given", [])
    peer = connect({"fixture": _Recording})
    backend = peer.enable_jobs()
    gate = peer.hold(backend)
    hold_vad(peer, backend, 1, session_key=1)
    if end == "cancelled":
        assert read_after(peer, vad_submit(2)) == (ACCEPTED, 2)
        peer.send(message(protocol.KIND_JOB_CANCEL, 2))
        assert read(peer, 3) == [(ACKNOWLEDGED, 2), *ended(2, FAILED, "cancelled")]
    else:
        session = 2 if end.endswith("another session") else 1
        refused = vad_submit(2, session_key=session, frames=1)
        refused["input"].update(frame_samples=768, payload_length=768 * 2)
        peer.send(refused)
        assert read(peer, 2) == ended(2, FAILED, "unsupported", "job_not_permitted")
    gate.set()
    assert read(peer, 2) == ended(1)
    peer.send(vad_submit(3))
    assert read(peer, 3) == [(ACCEPTED, 3), *ended(3)]
    carried = "after 1" if end.endswith("another session") else None
    assert _Recording.given == [(1, None), (3, carried)]


def test_an_unload_fails_waiting_vad_jobs_and_is_answered_after_the_running_one(
    connect: Callable[..., Peer],
) -> None:
    peer = connect()
    backend = peer.enable_jobs()
    gate = peer.hold(backend)
    hold_vad(peer, backend, 1)
    assert read_after(peer, vad_submit(2, session_key=2)) == (ACCEPTED, 2)
    unload = message(protocol.KIND_UNLOAD)
    peer.send(unload)
    assert read(peer, 2) == ended(2, FAILED, "unavailable", "backend_unavailable")
    gate.set()
    assert read(peer, 3) == [*ended(1), ("unload_response", unload["message_id"])]


class _Watched(threading.Condition):
    """A condition that says when a thread first waits on it."""

    def __init__(self, lock: threading.Lock) -> None:
        super().__init__(lock)
        self.waited = threading.Event()

    def wait(self, timeout: float | None = None) -> bool:
        self.waited.set()
        return super().wait(timeout)


def test_a_load_or_unload_is_not_taken_while_a_vad_job_runs() -> None:
    written: list[tuple[Any, ...]] = []
    table = jobs.JobTable(
        lambda frame, _originated: written.append(digest(frame.header)), lambda _message: None
    )
    watched = _Watched(table._lock)
    table._work = watched
    table.open(FixtureBackend(), jobs.LANE_JOB_CLASSES)
    header = vad_submit(1)
    assert table.handle(codec.SidecarFrame(header, pcm(0, 0, 0)))
    job = table.take_vad()
    assert job is not None
    unload = codec.SidecarFrame(message(protocol.KIND_UNLOAD))
    assert table.offer(unload, closing=True)
    taken: list[object] = []
    taker = threading.Thread(target=lambda: taken.append(table.take()), daemon=True)
    taker.start()
    assert watched.waited.wait(WAIT_S)
    assert not taken
    table.run(job)
    taker.join(timeout=WAIT_S)
    assert taken == [unload]
    assert written == [(ACCEPTED, 1), *ended(1)]


@pytest.mark.parametrize("drain", [False, True])
def test_a_stopped_vad_lane_drops_its_waiting_jobs_unless_it_drains(drain: bool) -> None:
    table = jobs.JobTable(lambda _frame, _originated: None, lambda _message: None)
    table.open(FixtureBackend(), jobs.LANE_JOB_CLASSES)
    assert table.handle(codec.SidecarFrame(vad_submit(1), pcm(0, 0, 0)))
    table.stop(drain=drain)
    job = table.take_vad()
    assert (job is not None) == drain
    assert table.take_vad() is None
