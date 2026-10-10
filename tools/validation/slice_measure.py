#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Measure the thin streaming slice from the caller's side and write a checked record.

Runs paced speech-to-text sessions and text-to-speech segments against a
serving worker's stream endpoint and takes a timestamp, on this process's
monotonic clock, at every boundary a caller can see. It speaks the stream
session envelope through bindings generated at start from
`protocol/proto/tensorplate/stream/v1/session.proto` with `grpcio-tools`;
it does not use or import the SDK. The record's shape and its check are in
`slice_measurement_record.py`; it holds no audio, text, transcript or address.

`run` writes the record before the first session and again after each one, so
a run that is interrupted or fails leaves a record that says so. Exit status
of `run`: the record check's, or 2 when no record could be written.
"""

from __future__ import annotations

import argparse
import contextlib
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
import signal
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any, NamedTuple

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
PROTO_PATH = REPO_ROOT / "protocol/proto/tensorplate/stream/v1/session.proto"

sys.path.insert(0, str(HERE))
import slice_measurement_record as record_mod  # noqa: E402
from candidate_audio import AudioError, pcm16_bytes, read_wav_pcm16_mono  # noqa: E402
from candidate_record import sha256_digest  # noqa: E402

MS = 1_000_000
GPU_QUERY = ("--query-gpu=index,name,utilization.gpu,memory.used,memory.total", "--format=csv")
GPU_COLUMNS = {
    "index": "index",
    "utilization.gpu [%]": "utilization_percent",
    "memory.used [MiB]": "memory_used_mib",
    "memory.total [MiB]": "memory_total_mib",
}
# The server events a session of each mode can be sent, beside those of every session.
SESSION_BODIES = ("ready", "accepted", "session_closed")
MODE_BODIES = {
    "stt": ("partial_transcript", "endpoint_detected", "final_transcript"),
    "tts": ("audio_chunk", "segment_completed", "synthesis_completed"),
}
PROTOCOL_NAME = "[ -~]{1,64}"
LABEL = "[0-9A-Za-z][0-9A-Za-z.+_-]{0,63}"


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
    """A deployment as the Open names it; a worker that reports no generation has neither."""

    endpoint: str
    deployment_id: str
    generation: int | None
    descriptor_digest: str | None

    @classmethod
    def parse(cls, endpoint: str, spec: str) -> Target:
        parts = spec.split(":", 2)
        if len(parts) == 1 and spec:
            return cls(endpoint, spec, None, None)
        if len(parts) != 3 or not parts[1].isdigit() or int(parts[1]) < 1:
            raise MeasureError(f"target {spec!r} is not DEPLOYMENT or DEPLOYMENT:GENERATION:DIGEST")
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
        rate, samples = read_wav_pcm16_mono(path)
    except AudioError as exc:
        raise MeasureError(str(exc)) from exc
    data = pcm16_bytes(samples)
    per_frame, remainder = divmod(rate * frame_ms, 1000)
    if not data or remainder:
        raise MeasureError(f"{path.name}: need audio in whole {frame_ms} ms frames")
    return {
        "frames": [data[at : at + per_frame * 2] for at in range(0, len(data), per_frame * 2)],
        "rate": rate,
        "frame_ms": frame_ms,
        "samples": len(data) // 2,
        "sha256": sha256_digest(data),
        "bytes": len(data),
    }


def parse_gpu_csv(text: str) -> list[dict[str, Any]]:
    """Rows of `nvidia-smi --query-gpu=... --format=csv`, by the header's column names."""
    lines = [line for line in text.splitlines() if line.strip()]
    header = [name.strip() for name in lines[0].split(",")] if lines else []
    if not {"name", *GPU_COLUMNS} <= set(header) or len(lines) < 2:
        raise ValueError("no reading under the expected header")
    rows = []
    for line in lines[1:]:
        cells = dict(zip(header, (cell.strip() for cell in line.split(",")), strict=True))
        row: dict[str, Any] = {"name": cells["name"]}
        for column, field in GPU_COLUMNS.items():
            number = cells[column].split()[:1]
            row[field] = int(number[0]) if number and number[0].isdigit() else None
        if row["index"] is None or not row["name"]:
            raise ValueError("a reading without a device index or name")
        rows.append(row)
    return rows


def machine_facts() -> dict[str, Any]:
    """What a latency depends on in the measuring machine, and nothing that names it."""
    model = None
    with contextlib.suppress(OSError):
        cpuinfo = Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
        found = re.search(r"^model name\s*:\s*([ -~]{1,128})", cpuinfo, re.MULTILINE)
        model = found and found.group(1).strip()
    return {
        "system": platform.system(),
        "release": platform.release(),
        "machine": platform.machine(),
        "cpu_count": os.cpu_count(),
        "cpu_model": model or None,
    }


class Run:
    """A run's clock, with one origin for every timestamp, and its transport.

    A test replaces it to make time and arrival deterministic.
    """

    def __init__(self) -> None:
        self.origin = time.monotonic_ns()

    def now(self) -> int:
        return time.monotonic_ns() - self.origin

    def outbound(self) -> Any:
        """Where a call puts the events gRPC is to send; None ends them."""
        return queue.Queue()

    def take(self, inbound: queue.Queue[tuple[int, Any]], until: int) -> tuple[int, Any] | None:
        """What a call read next and when, or None once the clock reaches `until`."""
        try:
            return inbound.get(timeout=max(0, until - self.now()) / 1e9)
        except queue.Empty:
            return None

    @contextlib.contextmanager
    def connect(
        self, bindings: Bindings, endpoint: str, timeout_ms: int
    ) -> Iterator[tuple[Any, int | None]]:
        """A stub on a ready channel and the time to it, or neither when it never was ready."""
        import grpc

        with grpc.insecure_channel(endpoint) as channel:
            started = self.now()
            try:
                grpc.channel_ready_future(channel).result(timeout=timeout_ms / 1000)
            except grpc.FutureTimeoutError:
                yield None, None
            else:
                yield bindings.rpc.SessionServiceStub(channel), self.now() - started


def sample_gpu(program: str, interval_ms: int, alive: Callable[[], bool] | None = None) -> int:
    """Print one line per device reading, and one per query that gave none, while `alive`.

    By default that is until the starting process stops or ends this one.
    Exits 1 when the first query gives no reading.
    """
    parent, printed = os.getppid(), False
    alive = alive or (lambda: os.getppid() == parent)
    while alive():
        at = time.monotonic_ns()
        try:
            done = subprocess.run(
                [program, *GPU_QUERY], capture_output=True, text=True, timeout=10, check=True
            )
            rows = parse_gpu_csv(done.stdout)
        except Exception:  # noqa: BLE001 - a query that fails in any way is counted, not fatal
            if not printed:
                return 1
            rows = [{"failed": True}]
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

    def __init__(
        self, run: Run, enabled: bool, interval_ms: int, program: str = "nvidia-smi"
    ) -> None:
        self._run, self._interval_ms = run, interval_ms
        self._process: subprocess.Popen[str] | None = None
        self._reason: str | None = None
        self._status = "unavailable" if enabled else "not_requested"
        if not enabled:
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

    def section(self, reason: str | None, lines: list[dict[str, Any]], serving: bool) -> Any:
        """The record's GPU section: what was not sampled and why, or `lines` as samples."""
        samples = [
            {key: value for key, value in line.items() if key != "name"}
            for line in lines
            if "failed" not in line
        ]
        names = {line["index"]: line["name"] for line in lines if "failed" not in line}
        sampled = bool(samples)
        status = "sampled" if sampled else self._status
        return {
            "status": status,
            "reason": None if sampled or status == "not_requested" else self._reason or reason,
            "scope": "device_wide",
            "machine": None if not sampled else "serving" if serving else "client_only",
            "interval_ms": None if status == "not_requested" else self._interval_ms,
            "failed_queries": len(lines) - len(samples),
            "devices": [{"index": index, "name": names[index]} for index in sorted(names)],
            "samples": samples,
            "summary": None,
        }

    def finish(self, serving: bool) -> dict[str, Any]:
        """End the sampling process and return the section; no sample outlives a dead sampler."""
        if self._process is None:
            return self.section(None, [], serving)
        died = self._process.poll() is not None
        self._process.terminate()
        self._process.wait()
        self._lines.seek(0)
        text = self._lines.read()
        self._lines.close()
        if not text:
            return self.section("nvidia-smi gave no reading", [], serving)
        if died:
            return self.section("the sampling process ended before the run did", [], serving)
        lines = [json.loads(line) for line in text.splitlines()]
        return self.section(
            None, [{**line, "at_ns": line["at_ns"] - self._run.origin} for line in lines], serving
        )


class Call:
    """One Session call: its events out and in, and what the session's record says of them."""

    def __init__(
        self, run: Run, bindings: Bindings, stub: Any, mode: str, index: int, timeout_ns: int
    ) -> None:
        self._run, self._pb, self._timeout_ns = run, bindings.pb, timeout_ns
        self._mode, self.now = mode, run.now
        self._requests = run.outbound()
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
            budget.used + sent + more <= budget.limit
            for budget, sent, more in zip(self._budgets, held, wanted, strict=True)
        )

    def _credit(self, credit: Any) -> None:
        """Take the budgets a Ready or an Accepted reports; no byte limit is not unlimited."""
        if not credit.bytes.limit:
            raise _Stop("credit_missing")
        self._budgets = (credit.bytes, credit.waiting_segments)

    def send_input(self, size: int, segments: int, **body: Any) -> int:
        """Send an Audio or a TextSegment once the session's credit admits it."""
        if not self._fits(size, segments):
            wanted = zip(self._budgets, (size, segments), strict=True)
            if any(more > budget.limit for budget, more in wanted):
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
            item = self._run.take(self._inbound, wake)
            if item is not None or self._run.now() >= deadline:
                return item

    def wait(self, done: Callable[[], bool]) -> None:
        """Handle server events until `done`.

        The call ending ends the wait, and so does the timeout, which is
        counted again from each event the session needs.
        """
        deadline = self._run.now() + self._timeout_ns
        while not done():
            if self.status is not None:
                raise _CallEnded
            item = self._next(deadline)
            if item is None:
                raise _Timeout
            if self._handle(*item):
                deadline = self._run.now() + self._timeout_ns

    def pace(self, until: int) -> None:
        """Handle what arrives until `until`, the time the next frame is due."""
        while (item := self._next(until)) is not None:
            self._handle(*item)
            if self.status is not None:
                raise _CallEnded

    def _handle(self, at: int, item: Any) -> bool:
        """Take one item read at `at`; whether it was an event the session needs."""
        server = self.session["server"]
        if not isinstance(item, self._pb.ServerEvent):
            self.status = item
            self.session["grpc_status"] = item.name
            return True
        server["events_received"] += 1
        if item.sequence != server["events_received"]:
            raise _Stop("sequence_gap")
        body = item.WhichOneof("body")
        if body is None:
            server["ignored_events"] += 1
            return False
        allowed = body in ("ready", "session_closed") if self.ready is None else body != "ready"
        if server["closed"] is not None or not allowed:
            raise _Stop("unexpected_event")
        if body in ("pong", "status"):
            return False
        if body not in (*SESSION_BODIES, *MODE_BODIES[self._mode]):
            raise _Stop("unexpected_event")
        getattr(self, f"_on_{body}")(at, getattr(item, body))
        return True

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
        self._credit(ready.status.input_credit)

    def _on_accepted(self, at: int, accepted: Any) -> None:
        self.session["server"]["accepted_events"] += 1
        for sequence in [s for s in self._pending if s <= accepted.accepted_sequence]:
            del self._pending[sequence]
        self._credit(accepted.input_credit)

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
        if partial.text.strip() and boundaries["first_partial"] is None:
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
        text = "".join(segment.text for segment in final.segments)
        self.session["boundaries"]["final_transcript"] = at
        server["final"] = {
            **numbers(final, "finalize_sequence", "end_sample_offset"),
            "segments": len(final.segments),
            "text_bytes": len(text.strip().encode()),
            "text_sha256": sha256_digest(text.encode()),
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
        generation=target.generation or 0,
        descriptor_digest=target.descriptor_digest or "",
    )
    opened = pb.Open(resolved=resolved, **fields)
    call.send(open=opened)
    call.wait(lambda: call.ready is not None)
    ready = call.ready
    if (ready.generation, ready.descriptor_digest) != (
        resolved.generation,
        resolved.descriptor_digest,
    ):
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
    first = due = at = 0
    for position, frame in enumerate(plan["frames"]):
        # Each frame is due at its place counted from the first, whenever the one before went.
        due = first + position * frame_ms * MS
        if position:
            call.pace(due)
        body = pb.Audio(
            data=frame, sample_count=len(frame) // 2, sample_offset=client["samples_sent"]
        )
        at = call.send_input(len(frame), 0, audio=body)
        if not position:
            first = due = boundaries["first_audio_sent"] = at
        client["max_pacing_lag_ns"] = max(client["max_pacing_lag_ns"] or 0, at - due)
        client["frames_sent"] += 1
        client["bytes_sent"] += len(frame)
        client["samples_sent"] += len(frame) // 2
    boundaries["last_audio_scheduled"], boundaries["last_audio_sent"] = due, at
    client["input_lag_ns"] = at - due
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
    client, boundaries = call.session["client"], call.session["boundaries"]
    client["segment_id"] = 1
    segment = pb.TextSegment(segment_id=1, text=plan["text"])
    due = boundaries["text_segment_scheduled"] = call.now()
    boundaries["text_segment_sent"] = call.send_input(size, 1, text_segment=segment)
    client["input_lag_ns"] = boundaries["text_segment_sent"] - due
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


def declared_input(mode: str, plan: dict[str, Any]) -> dict[str, Any]:
    """What every session of a section sends, as the record states it."""
    if mode == "stt":
        return {
            "audio_sha256": plan["sha256"],
            "audio_bytes": plan["bytes"],
            "samples": plan["samples"],
            "format": {"encoding": "pcm_s16le", "sample_rate_hz": plan["rate"], "channels": 1},
            "frame_ms": plan["frame_ms"],
            "frames": len(plan["frames"]),
            "language": plan["language"],
        }
    text = plan["text"].encode()
    return {
        "text_sha256": sha256_digest(text),
        "text_bytes": len(text),
        "language": plan["language"],
        "voice": plan["voice"],
    }


def run_section(
    run: Run, bindings: Bindings, mode: str, target: Target, plan: dict[str, Any],
    iterations: int, timeout_ms: int, record: dict[str, Any], save: Callable[[], None],
) -> None:  # fmt: skip
    """Fill `record[mode]`, saving the record after each session."""
    with run.connect(bindings, target.endpoint, timeout_ms) as (stub, connect_ns):
        sessions: list[dict[str, Any]] = []
        record[mode] = {
            "target": {
                "generation": target.generation,
                "descriptor_digest": target.descriptor_digest,
                "loopback": target.is_loopback(),
                "connect_ns": connect_ns,
            },
            "input": declared_input(mode, plan),
            "sessions": sessions,
            "summary": {},
        }
        if stub is None:
            print(
                f"{record_mod.TOOL_NAME}: {mode}: the channel never became ready", file=sys.stderr
            )
            return
        for index in range(iterations):
            sessions.append(run_session(run, bindings, stub, mode, index, target, plan, timeout_ms))
            save()


def measure(
    bindings: Bindings,
    *,
    stt: tuple[Target, dict[str, Any]] | None,
    tts: tuple[Target, dict[str, Any]] | None,
    iterations: int = 1,
    event_timeout_ms: int = 30_000,
    provenance: str = "synthetic",
    source_commit: str | None = None,
    worker_version: str | None = None,
    worker_build: str | None = None,
    gpu_sampling: str = "auto",
    gpu_interval_ms: int = 1000,
    gpu_program: str = "nvidia-smi",
    checkpoint: Callable[[dict[str, Any]], None] | None = None,
    run: Run | None = None,
) -> dict[str, Any]:
    """Run the requested sections and return the finished record.

    `checkpoint` is given the record as it stands before the first session
    and after each one. An error or an interrupt ends the run and is
    recorded; it is not raised.
    """
    run = run or Run()
    requested = {"stt": stt, "tts": tts}
    serving = all(section[0].is_loopback() for section in requested.values() if section)
    # Elsewhere than on loopback the readings are not the serving machine's: only when asked.
    sampler = GpuSampler(
        run, gpu_sampling == "on" or (gpu_sampling == "auto" and serving), gpu_interval_ms,
        gpu_program,
    )  # fmt: skip
    stated = worker_version is not None or worker_build is not None
    record: dict[str, Any] = {
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
        "worker": {
            "source": "operator_stated" if stated else "not_stated",
            "version": worker_version,
            "build": worker_build,
        },
        "client_platform": machine_facts(),
        "recorded_at_utc": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "clock": {
            "source": "caller_monotonic",
            "origin": "run_start",
            "resolution_ns": max(1, round(time.get_clock_info("monotonic").resolution * 1e9)),
        },
        "percentile_method": "nearest_rank",
        "settings": {
            "iterations": iterations,
            "event_timeout_ms": event_timeout_ms,
            "max_input_lag_ms": record_mod.MAX_INPUT_LAG_MS,
        },
        "stt": None,
        "tts": None,
        "gpu": sampler.section("the run did not end", [], serving),
        "run": {"ended_by": None, "error": None},
        "result": {},
    }

    def save() -> None:
        record_mod.finalize(record)
        if checkpoint is not None:
            checkpoint(record)

    try:
        save()
        for mode, section in requested.items():
            if section is not None:
                run_section(
                    run, bindings, mode, *section, iterations, event_timeout_ms, record, save
                )
        record["run"]["ended_by"] = "completed"
    except KeyboardInterrupt:
        record["run"]["ended_by"] = "interrupt"
    except Exception as error:  # noqa: BLE001 - whatever ended the run, the record says so
        traceback.print_exc()
        record["run"].update(ended_by="error", error=type(error).__name__)
    record["gpu"] = sampler.finish(serving)
    return record_mod.finalize(record)


def write_record(out: Path, record: dict[str, Any]) -> None:
    """Replace `out` whole, so a write that is cut short leaves the record before it."""
    partial = out.with_name(out.name + ".partial")
    partial.write_text(record_mod.dump(record), encoding="utf-8")
    os.replace(partial, out)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="measure and write the record")
    run.add_argument("--out", type=Path, required=True)
    run.add_argument(
        "--provenance",
        choices=("recorded", "synthetic"),
        help="default: recorded when --source-commit and the worker's release or build are "
        "given, synthetic otherwise",
    )
    run.add_argument("--source-commit", default=None, help="the commit this tool was run from")
    run.add_argument("--worker-version", help="the serving worker's release, as the operator knows")
    run.add_argument("--worker-build", help="the serving worker's build, as the operator knows it")
    run.add_argument("--iterations", type=int, default=1)
    run.add_argument("--event-timeout-ms", type=int, default=30_000)
    run.add_argument(
        "--gpu-sampling",
        choices=("auto", "on", "off"),
        default="auto",
        help="auto samples only when every target is a loopback address",
    )
    run.add_argument("--gpu-interval-ms", type=int, default=1000)
    for mode in record_mod.MODES:
        run.add_argument(f"--{mode}-endpoint", metavar="HOST:PORT")
        run.add_argument(f"--{mode}-target", metavar="DEPLOYMENT[:GENERATION:DIGEST]")
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
        # Ending this process ends the query in flight: `subprocess.run` kills it on the way out.
        signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
        # An interrupt reaches the whole process group; only the run that started this ends it.
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        return sample_gpu(args.program, args.interval_ms)
    try:
        if min(args.iterations, args.event_timeout_ms, args.gpu_interval_ms) < 1:
            raise MeasureError("iterations, timeouts and intervals are at least 1")
        if not 20 <= args.frame_ms <= 320:
            raise MeasureError("a frame is 20 to 320 ms, the bounds of the stream schema")
        if args.source_commit and re.fullmatch("[0-9a-f]{40}", args.source_commit) is None:
            raise MeasureError("--source-commit is not a full commit id")
        sourced = bool(args.source_commit and (args.worker_version or args.worker_build))
        provenance = args.provenance or ("recorded" if sourced else "synthetic")
        if provenance == "recorded" and not sourced:
            raise MeasureError(
                "--provenance recorded needs --source-commit and --worker-version or --worker-build"
            )
        for option, pattern in (
            ("worker_version", LABEL), ("worker_build", LABEL), ("stt_language", PROTOCOL_NAME),
            ("tts_language", PROTOCOL_NAME), ("voice", PROTOCOL_NAME),
        ):  # fmt: skip
            value = getattr(args, option)
            if value and re.fullmatch(pattern, value) is None:
                raise MeasureError(f"--{option.replace('_', '-')} does not match {pattern}")
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
                provenance=provenance,
                source_commit=args.source_commit,
                worker_version=args.worker_version or None,
                worker_build=args.worker_build or None,
                gpu_sampling=args.gpu_sampling,
                gpu_interval_ms=args.gpu_interval_ms,
                checkpoint=lambda record: write_record(args.out, record),
            )
        write_record(args.out, record)
    except (MeasureError, OSError, UnicodeDecodeError) as exc:
        print(f"{name}: cannot measure: {exc}", file=sys.stderr)
        return record_mod.EXIT_NO_VERDICT
    status, detail = record_mod.check_record(record)
    print(f"{name}: {status}: {detail}", file=sys.stdout if status == "complete" else sys.stderr)
    return record_mod.exit_status(status)


if __name__ == "__main__":
    sys.exit(main())
