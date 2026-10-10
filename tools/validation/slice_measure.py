#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Measure the thin streaming slice from the caller's side and write a checked record.

Runs paced speech-to-text sessions and text-to-speech segments against a
serving worker's stream endpoint and takes a timestamp, on this process's
monotonic clock, at every boundary a caller can see. It speaks the stream
session envelope through bindings generated at start from
`protocol/proto/tensorplate/stream/v1/session.proto` with `grpcio-tools`;
it does not use or import the SDK. The record's shape and its check are in
`slice_measurement_record.py`; it holds counts, sizes, digests and times only.

Exit status of `run`: the record check's, or 2 when no record could be written.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import hashlib
import importlib
import importlib.metadata
import ipaddress
import json
import os
import platform
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import wave
from collections.abc import Callable
from pathlib import Path
from typing import Any, NamedTuple

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
PROTO_PATH = REPO_ROOT / "protocol/proto/tensorplate/stream/v1/session.proto"

sys.path.insert(0, str(HERE))
import slice_measurement_record as record_mod  # noqa: E402

MS = 1_000_000
GPU_QUERY = ("--query-gpu=index,utilization.gpu,memory.used,memory.total", "--format=csv")
GPU_COLUMNS = {
    "index": "index",
    "utilization.gpu [%]": "utilization_percent",
    "memory.used [MiB]": "memory_used_mib",
    "memory.total [MiB]": "memory_total_mib",
}


class MeasureError(Exception):
    """The run cannot start: an input, a target or the installed generator is unusable."""


class _Stop(Exception):
    """The client ends the call; the argument is the record's `stop_reason`."""


class _Timeout(Exception):
    pass


class _CallEnded(Exception):
    pass


class Bindings(NamedTuple):
    pb: Any
    rpc: Any
    versions: dict[str, str]
    schema_digest: str


_bindings: Bindings | None = None


def sha256_digest(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def load_bindings(out_dir: Path) -> Bindings:
    """Generate the stream bindings into `out_dir` and import them, once per process."""
    global _bindings
    if _bindings is not None:
        return _bindings
    try:
        from grpc_tools import protoc

        versions = {
            name: importlib.metadata.version(name)
            for name in ("grpcio", "protobuf", "grpcio-tools")
        }
    except ImportError as exc:
        raise MeasureError(f"grpcio, protobuf and grpcio-tools must be installed: {exc}") from exc
    if versions["grpcio-tools"] != versions["grpcio"]:
        raise MeasureError(
            f"grpcio-tools {versions['grpcio-tools']} does not match grpcio {versions['grpcio']}"
        )
    arguments = ["protoc", f"-I{PROTO_PATH.parent}", f"--python_out={out_dir}"]
    if protoc.main([*arguments, f"--grpc_python_out={out_dir}", PROTO_PATH.name]) != 0:
        raise MeasureError(f"protoc could not generate bindings from {PROTO_PATH.name}")
    sys.path.insert(0, str(out_dir))
    stem = PROTO_PATH.stem
    _bindings = Bindings(
        importlib.import_module(f"{stem}_pb2"),
        importlib.import_module(f"{stem}_pb2_grpc"),
        versions,
        sha256_digest(PROTO_PATH.read_bytes()),
    )
    return _bindings


def enum_name(enum: Any, value: int) -> str:
    """`ENDPOINT_REASON_SILENCE` as `silence`; a value this schema lacks reads as unspecified."""
    prefix = enum.Name(0)[: -len("UNSPECIFIED")]
    try:
        return str(enum.Name(value))[len(prefix) :].lower()
    except ValueError:
        return "unspecified"


def numbers(message: Any, *fields: str) -> dict[str, Any]:
    return {field: getattr(message, field) for field in fields}


class Target(NamedTuple):
    endpoint: str
    deployment_id: str
    generation: int
    descriptor_digest: str

    @classmethod
    def parse(cls, endpoint: str, spec: str) -> Target:
        parts = spec.split(":", 2)
        if len(parts) != 3 or not parts[1].isdigit() or int(parts[1]) < 1:
            raise MeasureError(f"target {spec!r} is not DEPLOYMENT:GENERATION:DIGEST")
        if re.fullmatch("sha256:[0-9a-f]{64}", parts[2]) is None:
            raise MeasureError(f"target {spec!r}: the digest is not sha256: and 64 hex digits")
        return cls(endpoint, parts[0], int(parts[1]), parts[2])

    def is_loopback(self) -> bool:
        host = self.endpoint.rsplit(":", 1)[0].strip("[]")
        try:
            return host == "localhost" or ipaddress.ip_address(host).is_loopback
        except ValueError:
            return False


def load_audio(path: Path, frame_ms: int) -> dict[str, Any]:
    """Mono 16-bit PCM from a WAV file, cut into frames of `frame_ms`."""
    try:
        with wave.open(str(path), "rb") as source:
            shape = (source.getnchannels(), source.getsampwidth(), source.getcomptype())
            rate, data = source.getframerate(), source.readframes(source.getnframes())
    except (OSError, EOFError, wave.Error) as exc:
        raise MeasureError(f"{path.name}: not a readable WAV file: {exc}") from exc
    per_frame, remainder = divmod(rate * frame_ms, 1000)
    if shape != (1, 2, "NONE") or not data or remainder:
        raise MeasureError(f"{path.name}: need mono 16-bit PCM in whole {frame_ms} ms frames")
    return {
        "frames": [data[at : at + per_frame * 2] for at in range(0, len(data), per_frame * 2)],
        "rate": rate,
        "frame_ms": frame_ms,
        "samples": len(data) // 2,
        "sha256": sha256_digest(data),
        "bytes": len(data),
    }


def parse_gpu_csv(text: str) -> list[dict[str, int | None]]:
    """Rows of `nvidia-smi --query-gpu=... --format=csv`, by the header's column names."""
    lines = [line for line in text.splitlines() if line.strip()]
    header = [name.strip() for name in lines[0].split(",")] if lines else []
    if not set(GPU_COLUMNS) <= set(header) or len(lines) < 2:
        raise ValueError("no reading under the expected header")
    rows = []
    for line in lines[1:]:
        cells = dict(zip(header, (cell.strip() for cell in line.split(",")), strict=True))
        row: dict[str, int | None] = {}
        for column, field in GPU_COLUMNS.items():
            number = cells[column].split()[0]
            row[field] = int(number) if number.isdigit() else None
        if row["index"] is None:
            raise ValueError("a reading without a device index")
        rows.append(row)
    return rows


class Run:
    """One clock and one origin for every timestamp of a run."""

    def __init__(self) -> None:
        self.origin = time.monotonic_ns()

    def now(self) -> int:
        return time.monotonic_ns() - self.origin


def sample_gpu(program: str, interval_ms: int) -> int:
    """Print one line per device reading until the starting process stops or ends this one.

    Exits 1 when the first query gives no reading; a later one that fails is skipped.
    """
    parent, printed = os.getppid(), False
    while os.getppid() == parent:
        at = time.monotonic_ns()
        try:
            done = subprocess.run(
                [program, *GPU_QUERY], capture_output=True, text=True, timeout=10, check=False
            )
            rows = parse_gpu_csv(done.stdout) if done.returncode == 0 else []
        except (OSError, ValueError, subprocess.TimeoutExpired):
            rows = []
        if not (rows or printed):
            return 1
        for row in rows:
            print(json.dumps({"at_ns": at, **row}), flush=True)
        printed = True
        time.sleep(interval_ms / 1000)
    return 0


class GpuSampler:
    """Device-wide readings, taken by a process of its own.

    gRPC does not tolerate a fork beside its threads, so the one process
    this starts is started before the run opens a channel, and that process
    runs `nvidia-smi`. Both read the system's monotonic clock.
    """

    def __init__(self, run: Run, mode: str, interval_ms: int, program: str = "nvidia-smi") -> None:
        self._run, self._interval_ms = run, interval_ms
        self._process: subprocess.Popen[str] | None = None
        self._reason: str | None = None
        self._status = "not_requested" if mode == "off" else "unavailable"
        if mode == "off":
            return
        if shutil.which(program) is None:
            self._reason = "no nvidia-smi on this machine"
            return
        self._lines = tempfile.TemporaryFile("w+", encoding="utf-8")
        command = [sys.executable, str(Path(__file__).resolve()), "sample-gpu"]
        self._process = subprocess.Popen(
            [*command, "--program", program, "--interval-ms", str(interval_ms)], stdout=self._lines
        )
        # The first reading is taken before the first session starts.
        while self._process.poll() is None and not os.fstat(self._lines.fileno()).st_size:
            time.sleep(0.01)

    def finish(self) -> dict[str, Any]:
        samples: list[dict[str, Any]] = []
        if self._process is not None:
            self._process.terminate()
            self._process.wait()
            self._lines.seek(0)
            for line in self._lines.read().splitlines():
                sample = json.loads(line)
                samples.append({**sample, "at_ns": sample["at_ns"] - self._run.origin})
            self._lines.close()
            if samples:
                self._status = "sampled"
            else:
                self._reason = "nvidia-smi gave no reading"
        return {
            "status": self._status,
            "reason": self._reason,
            "scope": "device_wide",
            "interval_ms": None if self._status == "not_requested" else self._interval_ms,
            "samples": samples,
            "summary": None,
        }


class Call:
    """One Session call: its events out and in, and what the session's record says of them."""

    def __init__(
        self, run: Run, bindings: Bindings, stub: Any, mode: str, index: int, timeout_ns: int
    ) -> None:
        self._run, self._pb, self._timeout_ns = run, bindings.pb, timeout_ns
        self._requests: queue.Queue[Any] = queue.Queue()
        self._inbound: queue.Queue[tuple[int, Any]] = queue.Queue()
        self._sequence = 0
        self._session_id = ""
        self._next_ping: int | None = None
        self._ping_every = 0
        self._budgets: tuple[Any, Any] = (None, None)
        self._pending: dict[int, tuple[int, int]] = {}
        self._digest = hashlib.sha256()
        self.ready: Any = None
        self.status: Any = None
        self.half_closed = False
        self.session = record_mod.new_session(mode, index, 0)
        self.session["boundaries"]["call_start"] = run.now()
        self._call = stub.Session(iter(self._requests.get, None))
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        import grpc

        try:
            for event in self._call:
                self._inbound.put((self._run.now(), event))
            code = self._call.code()
        except grpc.RpcError as error:
            code = error.code()
        self._inbound.put((self._run.now(), code))

    def send(self, **body: Any) -> tuple[int, int]:
        """Hand one event to the transport; returns when, and its sequence."""
        self._sequence += 1
        event = self._pb.ClientEvent(sequence=self._sequence, session_id=self._session_id, **body)
        at = self._run.now()
        self._requests.put(event)
        self.session["client"]["events_sent"] += 1
        return at, self._sequence

    def _fits(self, *wanted: int) -> bool:
        """Whether both budgets, bytes and waiting segments, admit `wanted` more."""
        held = [sum(sent) for sent in zip(*self._pending.values(), strict=True)] or [0, 0]
        return all(
            budget.limit == 0 or budget.used + sent + more <= budget.limit
            for budget, sent, more in zip(self._budgets, held, wanted, strict=True)
        )

    def send_input(self, size: int, segments: int, **body: Any) -> int:
        """Send an Audio or a TextSegment once the session's credit admits it."""
        if not self._fits(size, segments):
            wanted = zip(self._budgets, (size, segments), strict=True)
            if any(budget.limit and more > budget.limit for budget, more in wanted):
                raise _Stop("input_outside_limits")
            self.session["client"]["credit_waits"] += 1
            self.wait(lambda: self._fits(size, segments))
        at, sequence = self.send(**body)
        self._pending[sequence] = (size, segments)
        return at

    def half_close(self) -> None:
        self.half_closed, self._next_ping = True, None
        self._requests.put(None)

    def close(self) -> None:
        if not self.half_closed:
            self._requests.put(None)
        self._call.cancel()

    def trailing_metadata(self) -> Any:
        return self._call.trailing_metadata()

    def _next(self, deadline: int) -> tuple[int, Any] | None:
        while True:
            now = self._run.now()
            if self._next_ping is not None and now >= self._next_ping:
                self.send(ping=self._pb.Ping())
                self.session["client"]["pings_sent"] += 1
                self._next_ping += self._ping_every
                continue
            wake = deadline if self._next_ping is None else min(deadline, self._next_ping)
            try:
                return self._inbound.get(timeout=max(0, wake - now) / 1e9)
            except queue.Empty:
                if self._run.now() >= deadline:
                    return None

    def wait(self, done: Callable[[], bool], timeout_ns: int | None = None) -> None:
        """Handle server events until `done`; the call ending or the timeout ends the wait."""
        deadline = self._run.now() + (self._timeout_ns if timeout_ns is None else timeout_ns)
        while not done():
            if self.status is not None:
                raise _CallEnded
            item = self._next(deadline)
            if item is None:
                raise _Timeout
            self._handle(*item)

    def pace(self, until: int) -> None:
        """Handle what arrives until `until`, the time the next frame is due."""
        while (item := self._next(until)) is not None:
            self._handle(*item)
            if self.status is not None:
                raise _CallEnded

    def _handle(self, at: int, item: Any) -> None:
        server = self.session["server"]
        if not isinstance(item, self._pb.ServerEvent):
            self.status = item
            self.session["grpc_status"] = item.name
            return
        server["events_received"] += 1
        if item.sequence != server["events_received"]:
            raise _Stop("sequence_gap")
        body = item.WhichOneof("body")
        if body is None:
            server["ignored_events"] += 1
            return
        allowed = body in ("ready", "session_closed") if self.ready is None else body != "ready"
        if server["closed"] is not None or not allowed:
            raise _Stop("unexpected_event")
        if body in ("pong", "status"):
            return
        handler = getattr(self, f"_on_{body}", None)
        if handler is None:
            raise _Stop("unexpected_event")
        handler(at, getattr(item, body))

    def _follows(self, at: int, sent: str) -> bool:
        """Whether an event read at `at` can answer what the boundary `sent` records."""
        boundary = self.session["boundaries"][sent]
        return boundary is not None and at >= boundary

    def _on_ready(self, at: int, ready: Any) -> None:
        self.ready, self._session_id = ready, ready.session_id
        self.session["boundaries"]["ready"] = at
        if ready.limits.heartbeat_interval_ms:
            self._ping_every = ready.limits.heartbeat_interval_ms * MS
            self._next_ping = at + self._ping_every
        credit = ready.status.input_credit
        self._budgets = (credit.bytes, credit.waiting_segments)

    def _on_accepted(self, at: int, accepted: Any) -> None:
        self.session["server"]["accepted_events"] += 1
        for sequence in [s for s in self._pending if s <= accepted.accepted_sequence]:
            del self._pending[sequence]
        self._budgets = (accepted.input_credit.bytes, accepted.input_credit.waiting_segments)

    def _on_session_closed(self, at: int, closed: Any) -> None:
        pb = self._pb
        if closed.state not in (pb.SESSION_STATE_CLOSED, pb.SESSION_STATE_FAILED):
            raise _Stop("unexpected_event")
        cause = closed.cause if closed.HasField("cause") else None
        self.session["boundaries"]["session_closed"] = at
        self.session["server"]["closed"] = {
            "state": enum_name(pb.SessionState, closed.state),
            "reason": None if cause is None else enum_name(pb.FailureReason, cause.reason),
            "code": None if cause is None else enum_name(pb.ErrorCode, cause.code),
            "admission": enum_name(pb.Admission, closed.admission),
            "last_accepted_sequence": closed.last_accepted_sequence,
            "totals": numbers(
                closed.totals,
                "input_audio_samples",
                "input_text_bytes",
                "output_audio_samples",
                "utterances_completed",
                "segments_completed",
            ),
        }
        clean = closed.state == pb.SESSION_STATE_CLOSED and cause is None
        if clean and not self.half_closed:
            raise _Stop("unexpected_event")

    # --- speech-to-text ---------------------------------------------------

    def _on_partial_transcript(self, at: int, partial: Any) -> None:
        boundaries, server = self.session["boundaries"], self.session["server"]
        if not self._follows(at, "first_audio_sent") or server["endpoint"] is not None:
            raise _Stop("unexpected_event")
        server["partials"] += 1
        if partial.text and boundaries["first_partial"] is None:
            boundaries["first_partial"] = at

    def _on_endpoint_detected(self, at: int, endpoint: Any) -> None:
        boundaries, server = self.session["boundaries"], self.session["server"]
        if server["final"] is not None:
            return
        if not self._follows(at, "finalize_sent") or server["endpoint"] is not None:
            raise _Stop("unexpected_event")
        boundaries["endpoint"] = at
        server["endpoint"] = {
            "reason": enum_name(self._pb.EndpointReason, endpoint.reason),
            "end_sample_offset": endpoint.end_sample_offset,
        }

    def _on_final_transcript(self, at: int, final: Any) -> None:
        server = self.session["server"]
        if server["final"] is not None:
            return
        if server["endpoint"] is None:
            raise _Stop("unexpected_event")
        text = "".join(segment.text for segment in final.segments).encode()
        self.session["boundaries"]["final_transcript"] = at
        server["final"] = {
            **numbers(final, "finalize_sequence", "end_sample_offset"),
            "segments": len(final.segments),
            "text_bytes": len(text),
            "text_sha256": sha256_digest(text),
        }

    # --- text-to-speech ---------------------------------------------------

    def _on_audio_chunk(self, at: int, chunk: Any) -> None:
        boundaries, server = self.session["boundaries"], self.session["server"]
        if (
            not self._follows(at, "text_segment_sent")
            or server["segment"] is not None
            or chunk.segment_id != self.session["client"]["segment_id"]
            or chunk.format != self.ready.output_format
        ):
            raise _Stop("unexpected_event")
        size = record_mod.BYTES_PER_SAMPLE.get(server["output_format"]["encoding"], 0) * (
            chunk.format.channels
        )
        self._digest.update(chunk.data)
        server["chunks"] += 1
        server["audio_bytes"] += len(chunk.data)
        server["audio_sha256"] = "sha256:" + self._digest.hexdigest()
        if size:
            server["audio_samples"] = server["audio_bytes"] // size
            whole = bool(chunk.data) and len(chunk.data) % size == 0
            if whole and boundaries["first_audio_chunk"] is None:
                boundaries["first_audio_chunk"] = at

    def _on_segment_completed(self, at: int, segment: Any) -> None:
        boundaries, server = self.session["boundaries"], self.session["server"]
        if boundaries["first_audio_chunk"] is None or server["segment"] is not None:
            raise _Stop("unexpected_event")
        boundaries["segment_completed"] = at
        server["segment"] = numbers(segment, "segment_id", "total_samples", "chunk_count")

    def _on_synthesis_completed(self, at: int, synthesis: Any) -> None:
        boundaries, server = self.session["boundaries"], self.session["server"]
        if not self._follows(at, "finalize_sent") or server["synthesis"] is not None:
            raise _Stop("unexpected_event")
        boundaries["synthesis_completed"] = at
        server["synthesis"] = numbers(
            synthesis, "segment_count", "total_samples", "finalize_sequence"
        )


def decode_open_refusal(trailing_metadata: Any) -> None:
    """The typed refusal a refused Open's status carries, for the record's `open_refusal`.

    The stream schema has not fixed how the status carries it, so nothing is
    decoded yet and a refusal is read by its status code alone.
    """
    return None


def _open(call: Call, pb: Any, target: Target, **fields: Any) -> Any:
    resolved = pb.ResolvedTarget(
        deployment_id=target.deployment_id,
        generation=target.generation,
        descriptor_digest=target.descriptor_digest,
    )
    opened = pb.Open(resolved=resolved, **fields)
    call.send(open=opened)
    call.wait(lambda: call.ready is not None)
    ready = call.ready
    if (ready.generation, ready.descriptor_digest) != (target.generation, target.descriptor_digest):
        raise _Stop("ready_mismatch")
    return ready


def _finalize(call: Call, pb: Any, answer: str) -> None:
    at, sequence = call.send(finalize=pb.Finalize())
    call.session["boundaries"]["finalize_sent"] = at
    call.session["client"]["finalize_sequence"] = sequence
    call.wait(lambda: call.session["server"][answer] is not None)
    call.half_close()
    call.wait(lambda: call.status is not None)


def _stt(call: Call, pb: Any, target: Target, plan: dict[str, Any]) -> None:
    audio = pb.AudioFormat(
        encoding=pb.AUDIO_ENCODING_PCM_S16LE, sample_rate_hz=plan["rate"], channels=1
    )
    ready = _open(
        call, pb, target, mode=pb.SESSION_MODE_STT_STREAMING, language=plan["language"],
        input_format=audio,
    )  # fmt: skip
    limits = ready.limits.speech_to_text
    call.session["server"]["limits"] = numbers(
        limits, "min_frame_ms", "max_frame_ms", "max_utterance_ms"
    )
    if ready.input_format != audio:
        raise _Stop("ready_mismatch")
    frame_ms = plan["frame_ms"]
    fits = limits.min_frame_ms <= frame_ms <= limits.max_frame_ms
    if not fits or plan["samples"] * 1000 > limits.max_utterance_ms * plan["rate"]:
        raise _Stop("input_outside_limits")
    client, boundaries = call.session["client"], call.session["boundaries"]
    first = at = 0
    for position, frame in enumerate(plan["frames"]):
        if position:
            call.pace(first + position * frame_ms * MS)
        body = pb.Audio(
            data=frame, sample_count=len(frame) // 2, sample_offset=client["samples_sent"]
        )
        at = call.send_input(len(frame), 0, audio=body)
        if not position:
            first = boundaries["first_audio_sent"] = at
        lag = at - first - position * frame_ms * MS
        client["max_pacing_lag_ns"] = max(client["max_pacing_lag_ns"] or 0, lag)
        client["frames_sent"] += 1
        client["bytes_sent"] += len(frame)
        client["samples_sent"] += len(frame) // 2
    boundaries["last_audio_sent"] = at
    _finalize(call, pb, "final")


def _tts(call: Call, pb: Any, target: Target, plan: dict[str, Any]) -> None:
    ready = _open(
        call, pb, target, mode=pb.SESSION_MODE_TTS_STREAMING, language=plan["language"],
        voice=plan["voice"],
    )  # fmt: skip
    limits, server = ready.limits.text_to_speech, call.session["server"]
    server["limits"] = numbers(
        limits,
        "max_segment_text_bytes",
        "max_segment_audio_ms",
        "max_synthesis_text_bytes",
        "max_synthesis_audio_ms",
    )
    server["output_format"] = {
        "encoding": enum_name(pb.AudioEncoding, ready.output_format.encoding),
        **numbers(ready.output_format, "sample_rate_hz", "channels"),
    }
    size = len(plan["text"].encode())
    if size > min(limits.max_segment_text_bytes, limits.max_synthesis_text_bytes):
        raise _Stop("input_outside_limits")
    call.session["client"]["segment_id"] = 1
    segment = pb.TextSegment(segment_id=1, text=plan["text"])
    call.session["boundaries"]["text_segment_sent"] = call.send_input(size, 1, text_segment=segment)
    call.wait(lambda: server["segment"] is not None)
    _finalize(call, pb, "synthesis")


def run_session(
    run: Run, bindings: Bindings, stub: Any, mode: str, index: int, target: Target,
    plan: dict[str, Any], timeout_ms: int,
) -> dict[str, Any]:  # fmt: skip
    """One session from call start to its end, whatever ends it."""
    call = Call(run, bindings, stub, mode, index, timeout_ms * MS)
    session, outcome = call.session, "call_failed"
    try:
        (_stt if mode == "stt" else _tts)(call, bindings.pb, target, plan)
    except _Stop as stop:
        outcome, session["stop_reason"] = "stopped", stop.args[0]
    except _Timeout:
        outcome = "timeout"
    except _CallEnded:
        if not session["server"]["events_received"]:
            session["open_refusal"] = decode_open_refusal(call.trailing_metadata())
    finally:
        call.close()
    closed = session["server"]["closed"]
    clean = closed is not None and closed["state"] == "closed" and closed["reason"] is None
    if outcome != "stopped" and closed is not None and not clean:
        outcome = "session_failed"
    elif outcome == "call_failed" and clean and session["grpc_status"] == "OK":
        outcome = "completed"
    session["outcome"] = outcome
    return session


def run_section(
    run: Run, bindings: Bindings, mode: str, target: Target, plan: dict[str, Any],
    iterations: int, timeout_ms: int,
) -> dict[str, Any]:  # fmt: skip
    import grpc

    if mode == "stt":
        declared = {
            "audio_sha256": plan["sha256"],
            "audio_bytes": plan["bytes"],
            "samples": plan["samples"],
            "format": {"encoding": "pcm_s16le", "sample_rate_hz": plan["rate"], "channels": 1},
            "frame_ms": plan["frame_ms"],
            "frames": len(plan["frames"]),
            "language_sha256": sha256_digest(plan["language"].encode()),
        }
    else:
        text = plan["text"].encode()
        declared = {
            "text_sha256": sha256_digest(text),
            "text_bytes": len(text),
            "language_sha256": sha256_digest(plan["language"].encode()),
            "voice_sha256": sha256_digest(plan["voice"].encode()),
        }
    sessions: list[dict[str, Any]] = []
    connect_ns = None
    with grpc.insecure_channel(target.endpoint) as channel:
        started = run.now()
        try:
            grpc.channel_ready_future(channel).result(timeout=timeout_ms / 1000)
            connect_ns = run.now() - started
        except grpc.FutureTimeoutError:
            print(
                f"{record_mod.TOOL_NAME}: {mode}: the channel never became ready", file=sys.stderr
            )
        if connect_ns is not None:
            stub = bindings.rpc.SessionServiceStub(channel)
            for index in range(iterations):
                sessions.append(
                    run_session(run, bindings, stub, mode, index, target, plan, timeout_ms)
                )
    return {
        "target": {
            "generation": target.generation,
            "descriptor_digest": target.descriptor_digest,
            "loopback": target.is_loopback(),
            "connect_ns": connect_ns,
        },
        "input": declared,
        "sessions": sessions,
        "summary": {},
    }


def measure(
    bindings: Bindings,
    *,
    stt: tuple[Target, dict[str, Any]] | None,
    tts: tuple[Target, dict[str, Any]] | None,
    iterations: int = 1,
    event_timeout_ms: int = 30_000,
    provenance: str = "recorded",
    source_commit: str | None = None,
    gpu_sampling: str = "auto",
    gpu_interval_ms: int = 1000,
    gpu_program: str = "nvidia-smi",
) -> dict[str, Any]:
    """Run the requested sections and return the finished record."""
    run = Run()
    sampler = GpuSampler(run, gpu_sampling, gpu_interval_ms, gpu_program)
    sections: dict[str, Any] = {"stt": None, "tts": None}
    try:
        for mode, requested in (("stt", stt), ("tts", tts)):
            if requested is not None:
                sections[mode] = run_section(
                    run, bindings, mode, *requested, iterations, event_timeout_ms
                )
    finally:
        gpu = sampler.finish()
    record = {
        "schema_version": "0.1",
        "record_kind": record_mod.RECORD_KIND,
        "provenance": provenance,
        "tool": {
            "name": record_mod.TOOL_NAME,
            "source_commit": source_commit,
            "python": platform.python_version(),
            "grpcio": bindings.versions["grpcio"],
            "protobuf": bindings.versions["protobuf"],
            "generator": bindings.versions["grpcio-tools"],
            "stream_schema_sha256": bindings.schema_digest,
        },
        "recorded_at_utc": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "clock": {
            "source": "caller_monotonic",
            "origin": "run_start",
            "resolution_ns": max(1, round(time.get_clock_info("monotonic").resolution * 1e9)),
        },
        "percentile_method": "nearest_rank",
        "settings": {"iterations": iterations, "event_timeout_ms": event_timeout_ms},
        **sections,
        "gpu": gpu,
        "result": {},
    }
    return record_mod.finalize(record)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="measure and write the record")
    run.add_argument("--out", type=Path, required=True)
    run.add_argument("--provenance", choices=("recorded", "synthetic"), default="recorded")
    run.add_argument("--source-commit", default=None)
    run.add_argument("--iterations", type=int, default=1)
    run.add_argument("--event-timeout-ms", type=int, default=30_000)
    run.add_argument("--gpu-sampling", choices=("auto", "off"), default="auto")
    run.add_argument("--gpu-interval-ms", type=int, default=1000)
    for mode in record_mod.MODES:
        run.add_argument(f"--{mode}-endpoint", metavar="HOST:PORT")
        run.add_argument(f"--{mode}-target", metavar="DEPLOYMENT:GENERATION:DIGEST")
        run.add_argument(f"--{mode}-language", help="a language tag the deployment declares")
    run.add_argument("--audio", type=Path, help="mono 16-bit PCM WAV, one utterance")
    run.add_argument("--frame-ms", type=int, default=20)
    run.add_argument("--text-file", type=Path, help="UTF-8 text of one segment")
    run.add_argument("--voice")
    sampler = commands.add_parser("sample-gpu", help="the sampling process `run` starts")
    sampler.add_argument("--program", required=True)
    sampler.add_argument("--interval-ms", type=int, required=True)
    args = parser.parse_args(argv)
    name = record_mod.TOOL_NAME
    if args.command == "sample-gpu":
        return sample_gpu(args.program, args.interval_ms)
    try:
        if min(args.iterations, args.event_timeout_ms, args.gpu_interval_ms) < 1:
            raise MeasureError("iterations, timeouts and intervals are at least 1")
        if not 20 <= args.frame_ms <= 320:
            raise MeasureError("a frame is 20 to 320 ms, the bounds of the stream schema")
        if args.source_commit and re.fullmatch("[0-9a-f]{40}", args.source_commit) is None:
            raise MeasureError("--source-commit is not a full commit id")
        stt = tts = None
        if args.stt_endpoint:
            if not (args.stt_target and args.stt_language and args.audio):
                raise MeasureError("--stt-endpoint needs --stt-target, --stt-language and --audio")
            plan = load_audio(args.audio, args.frame_ms)
            stt = (Target.parse(args.stt_endpoint, args.stt_target), plan)
            plan["language"] = args.stt_language
        if args.tts_endpoint:
            if not (args.tts_target and args.tts_language and args.text_file and args.voice):
                raise MeasureError(
                    "--tts-endpoint needs --tts-target, --tts-language, --text-file and --voice"
                )
            text = args.text_file.read_text(encoding="utf-8").strip()
            if not 1 <= len(text.encode()) <= 4096:
                raise MeasureError(f"{args.text_file.name}: need segment text of 1 to 4,096 bytes")
            plan = {"text": text, "language": args.tts_language, "voice": args.voice}
            tts = (Target.parse(args.tts_endpoint, args.tts_target), plan)
        if stt is None and tts is None:
            raise MeasureError("nothing to measure: give --stt-endpoint or --tts-endpoint")
        with tempfile.TemporaryDirectory(prefix="slice-measure-") as generated:
            record = measure(
                load_bindings(Path(generated)),
                stt=stt,
                tts=tts,
                iterations=args.iterations,
                event_timeout_ms=args.event_timeout_ms,
                provenance=args.provenance,
                source_commit=args.source_commit,
                gpu_sampling=args.gpu_sampling,
                gpu_interval_ms=args.gpu_interval_ms,
            )
        args.out.write_text(record_mod.dump(record), encoding="utf-8")
    except (MeasureError, OSError, UnicodeDecodeError) as exc:
        print(f"{name}: cannot measure: {exc}", file=sys.stderr)
        return record_mod.EXIT_NO_VERDICT
    status, detail = record_mod.check_record(record)
    print(f"{name}: {status}: {detail}", file=sys.stdout if status == "complete" else sys.stderr)
    return record_mod.exit_status(status)


if __name__ == "__main__":
    sys.exit(main())
