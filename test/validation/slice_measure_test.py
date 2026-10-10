#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check the streaming slice measurement record and the client that writes it.

The first record is synthetic: its timestamps are written by hand, so every
derived value has a known answer, and each rule of the check is broken once
with a refusal that has to name that rule. The client then runs against a
server built in this process from the same generated bindings, which holds
the client to the stream schema and misbehaves once per client guard. What
each timestamp means, and when each frame is due, is held in virtual time:
a scripted server and a clock that moves only when the client waits. Set
TP_SLICE_MEASURE_REQUIRE_PINS=1 to require the pinned grpcio and protobuf.
"""

from __future__ import annotations

import contextlib
import copy
import hashlib
import importlib.metadata
import io
import json
import math
import os
import queue
import re
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave
from concurrent import futures
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RECORD_TOOL = ROOT / "tools/validation/slice_measurement_record.py"
CLIENT_TOOL = ROOT / "tools/validation/slice_measure.py"
SCHEMA = ROOT / "config/schemas/slice_measurement_record.json"
FRAMES_DIR = ROOT / "test/contract/fixtures/stream/v1"
PINS = ROOT / "sdk/python/constraints/speech.txt"
GPU_CAPTURE = ROOT / "test/platform/memory_observation/nvidia-smi-query-gpu.csv"
sys.path.insert(0, str(ROOT / "tools/validation"))
# This process serves gRPC and starts child processes, which gRPC's fork handlers do not allow.
os.environ["GRPC_ENABLE_FORK_SUPPORT"] = "0"
import grpc  # noqa: E402
import jsonschema  # noqa: E402
import slice_measure as client_mod  # noqa: E402
import slice_measurement_record as record_mod  # noqa: E402

MS = 1_000_000
DIGEST = "sha256:" + "0" * 64
FRAMES, FRAME_MS, RATE = 10, 20, 16000
SEGMENT_LAG = MS // 2
SAMPLES = FRAMES * FRAME_MS * RATE // 1000
TOTALS = {
    "input_audio_samples": 0,
    "input_text_bytes": 0,
    "output_audio_samples": 0,
    "utterances_completed": 0,
    "segments_completed": 0,
}


def closed(**totals: int) -> dict:
    return {
        "state": "closed",
        "reason": None,
        "code": None,
        "admission": "admitted",
        "last_accepted_sequence": 2,
        "totals": {**TOTALS, **totals},
    }


def stt_session(index: int, start: int, final_after_ms: int) -> dict:
    session = record_mod.new_session("stt", index, start)
    last_due = start + 3 * MS + (FRAMES - 1) * FRAME_MS * MS
    last_audio = last_due + MS
    final = last_due + final_after_ms * MS
    session["boundaries"].update(
        ready=start + 2 * MS,
        first_audio_sent=start + 3 * MS,
        last_audio_scheduled=last_due,
        last_audio_sent=last_audio,
        finalize_sent=last_audio + 1,
        endpoint=final - MS,
        final_transcript=final,
        session_closed=final + MS,
    )
    session["client"].update(
        events_sent=FRAMES + 2,
        frames_sent=FRAMES,
        samples_sent=SAMPLES,
        bytes_sent=SAMPLES * 2,
        finalize_sequence=FRAMES + 2,
        max_pacing_lag_ns=2 * MS,
        input_lag_ns=MS,
    )
    session["server"].update(
        events_received=5,
        accepted_events=1,
        limits={"min_frame_ms": 20, "max_frame_ms": 40, "max_utterance_ms": 30000},
        endpoint={"reason": "client_finalize", "end_sample_offset": SAMPLES},
        final={
            "finalize_sequence": FRAMES + 2,
            "end_sample_offset": SAMPLES,
            "segments": 1,
            "text_bytes": 9,
            "text_sha256": DIGEST,
        },
        closed=closed(input_audio_samples=SAMPLES, utterances_completed=1),
    )
    session.update(outcome="completed", grpc_status="OK")
    return session


def tts_session(index: int, start: int, first_audio_after_ms: int) -> dict:
    session = record_mod.new_session("tts", index, start)
    first_audio = start + 3 * MS + first_audio_after_ms * MS
    session["boundaries"].update(
        ready=start + 2 * MS,
        text_segment_scheduled=start + 3 * MS,
        text_segment_sent=start + 3 * MS + SEGMENT_LAG,
        first_audio_chunk=first_audio,
        segment_completed=first_audio + MS,
        finalize_sent=first_audio + 2 * MS,
        synthesis_completed=first_audio + 3 * MS,
        session_closed=first_audio + 4 * MS,
    )
    session["client"].update(
        events_sent=3, segment_id=1, finalize_sequence=3, input_lag_ns=SEGMENT_LAG
    )
    session["server"].update(
        events_received=7,
        accepted_events=1,
        limits={
            "max_segment_text_bytes": 512,
            "max_segment_audio_ms": 10000,
            "max_synthesis_text_bytes": 4096,
            "max_synthesis_audio_ms": 120000,
        },
        output_format={"encoding": "pcm_s16le", "sample_rate_hz": 24000, "channels": 1},
        chunks=2,
        audio_bytes=1920,
        audio_samples=960,
        audio_sha256=DIGEST,
        segment={"segment_id": 1, "total_samples": 960, "chunk_count": 2},
        synthesis={"segment_count": 1, "total_samples": 960, "finalize_sequence": 3},
        closed=closed(input_text_bytes=22, output_audio_samples=960, segments_completed=1),
    )
    session.update(outcome="completed", grpc_status="OK")
    return session


def synthetic_record() -> dict:
    target = {"generation": 7, "descriptor_digest": DIGEST, "loopback": True, "connect_ns": 5 * MS}
    record = {
        "schema_version": "0.1",
        "record_kind": "slice_measurement",
        "provenance": "synthetic",
        "tool": {
            "name": "slice-measure",
            "source_commit": None,
            "python": "3.12.4",
            "grpcio": "1.81.1",
            "protobuf": "6.33.5",
            "generator": "1.81.1",
            "stream_schema_sha256": DIGEST,
        },
        "worker": {"source": "not_stated", "version": None, "build": None},
        "client_platform": {
            "system": "Linux",
            "release": "6.8.0",
            "machine": "x86_64",
            "cpu_count": 8,
            "cpu_model": None,
        },
        "recorded_at_utc": "2026-01-01T00:00:00Z",
        "clock": {"source": "caller_monotonic", "origin": "run_start", "resolution_ns": 1},
        "percentile_method": "nearest_rank",
        "settings": {"iterations": 3, "event_timeout_ms": 30000, "max_input_lag_ms": 100},
        "stt": {
            "target": dict(target),
            "input": {
                "audio_sha256": DIGEST,
                "audio_bytes": SAMPLES * 2,
                "samples": SAMPLES,
                "format": {"encoding": "pcm_s16le", "sample_rate_hz": RATE, "channels": 1},
                "frame_ms": FRAME_MS,
                "frames": FRAMES,
                "language": "en",
            },
            "sessions": [
                stt_session(0, 10 * MS, 300),
                stt_session(1, 1000 * MS, 100),
                stt_session(2, 2000 * MS, 200),
            ],
            "summary": {},
        },
        "tts": {
            "target": dict(target),
            "input": {
                "text_sha256": DIGEST,
                "text_bytes": 22,
                "language": "en",
                "voice": VOICE,
            },
            "sessions": [
                tts_session(0, 3000 * MS, 90),
                tts_session(1, 4000 * MS, 70),
                tts_session(2, 5000 * MS, 80),
            ],
            "summary": {},
        },
        "gpu": {
            "status": "sampled",
            "reason": None,
            "scope": "device_wide",
            "machine": "serving",
            "interval_ms": 1000,
            "failed_queries": 0,
            "devices": [{"index": 0, "name": "NVIDIA L4"}],
            "samples": [
                {
                    "at_ns": at * MS,
                    "index": 0,
                    "utilization_percent": used,
                    "memory_used_mib": 256 + used,
                    "memory_total_mib": 23034,
                }
                for at, used in ((0, 0), (1000, 61), (2000, 37))
            ],
            "summary": None,
        },
        "run": {"ended_by": "completed", "error": None},
        "result": {},
    }
    return record_mod.finalize(record)


def run_check(record: dict, work: Path) -> subprocess.CompletedProcess:
    path = work / "record.json"
    path.write_text(record_mod.dump(record), encoding="utf-8")
    return subprocess.run(
        [sys.executable, str(RECORD_TOOL), "check", str(path)],
        capture_output=True,
        text=True,
        check=False,
    )


def check_nearest_rank() -> None:
    hundred = list(range(1, 101))
    assert [record_mod.nearest_rank(hundred, p) for p in (50, 95, 99)] == [50, 95, 99]
    # Twenty samples: ranks 10, 19 and 20. Interpolation would give 95.25 and 99.05.
    twenty = [n * 5 for n in range(1, 21)]
    assert [record_mod.nearest_rank(twenty, p) for p in (50, 95, 99)] == [50, 95, 100]
    assert [record_mod.nearest_rank([7, 9, 30], p) for p in (50, 95, 99)] == [9, 30, 30]
    assert record_mod.distribution([30, None, 7, 9]) == {
        "count": 3,
        "misses": 1,
        "p50_ns": 9,
        "p95_ns": 30,
        "p99_ns": 30,
        "min_ns": 7,
        "max_ns": 30,
    }
    assert record_mod.distribution([None]) == {
        "count": 0,
        "misses": 1,
        "p50_ns": None,
        "p95_ns": None,
        "p99_ns": None,
        "min_ns": None,
        "max_ns": None,
    }


def check_stage_list() -> None:
    labels = json.loads((ROOT / "protocol/schemas/metric_event.json").read_text(encoding="utf-8"))
    registered = labels["properties"]["labels"]["properties"]["stage"]["enum"]
    schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
    assert list(record_mod.SERVER_STAGES) == registered, registered
    lag_bound = schema["properties"]["settings"]["properties"]["max_input_lag_ms"]["const"]
    assert lag_bound == record_mod.MAX_INPUT_LAG_MS, lag_bound
    assert schema["definitions"]["ServerStage"]["properties"]["stage"]["enum"] == registered
    assert [entry["stage"] for entry in record_mod.server_stages()] == registered


def check_good_record(good: dict, work: Path) -> None:
    jsonschema.validate(good, json.loads(SCHEMA.read_text(encoding="utf-8")))
    assert record_mod.check_record(good) == ("complete", "every session completed")
    completed = run_check(good, work)
    assert completed.returncode == 0 and "complete: every session" in completed.stdout, completed
    assert "provenance synthetic: these numbers measure nothing" in completed.stdout

    stt, tts = good["stt"], good["tts"]
    first = stt["sessions"][0]["measurements"]
    assert first["session_initialization"] == {
        "from": "call_start",
        "to": "ready",
        "ended_by": "ready",
        "ns": 2 * MS,
    }
    # Counted from when the last frame was due, 1 ms before it was sent.
    assert first["last_audio_to_final_transcript"] == {
        "from": "last_audio_scheduled",
        "to": "final_transcript",
        "ended_by": "final_transcript",
        "ns": 300 * MS,
    }
    assert first["call_start_to_first_transcript"] == {
        "from": "call_start",
        "to": "final_transcript",
        "ended_by": "final_transcript",
        "ns": (3 + 9 * 20 + 300) * MS,
    }
    assert stt["summary"]["last_audio_to_final_transcript"] == {
        "count": 3,
        "misses": 0,
        "p50_ns": 200 * MS,
        "p95_ns": 300 * MS,
        "p99_ns": 300 * MS,
        "min_ns": 100 * MS,
        "max_ns": 300 * MS,
    }
    assert tts["summary"]["segment_to_first_audio"]["p50_ns"] == 80 * MS
    assert tts["summary"]["call_start_to_first_audio"]["max_ns"] == 93 * MS
    assert tts["summary"]["segment_to_completion"]["min_ns"] == 71 * MS
    # Counted from when the segment was due, half a millisecond before it was sent.
    assert tts["sessions"][0]["measurements"]["segment_to_first_audio"] == {
        "from": "text_segment_scheduled",
        "to": "first_audio_chunk",
        "ended_by": "audio_chunk",
        "ns": 90 * MS,
    }
    assert good["gpu"]["summary"] == {
        "samples": 3,
        "max_utilization_percent": 61,
        "max_memory_used_mib": 317,
    }
    for session in stt["sessions"] + tts["sessions"]:
        assert all(entry["ns"] is None for entry in session["server_stages"])

    # A partial that arrived ends the first-transcript measurement, and says so.
    partial = copy.deepcopy(good)
    session = partial["stt"]["sessions"][0]
    session["boundaries"]["first_partial"] = session["boundaries"]["first_audio_sent"] + 40 * MS
    session["server"].update(partials=1, events_received=6)
    record_mod.finalize(partial)
    assert session["measurements"]["call_start_to_first_transcript"] == {
        "from": "call_start",
        "to": "first_partial",
        "ended_by": "partial_transcript",
        "ns": 43 * MS,
    }
    assert record_mod.check_record(partial)[0] == "complete"


def put(path: str, value: object):
    """An edit that sets the dotted `path` of a record to `value`."""
    keys = [int(key) if key.isdigit() else key for key in path.split(".")]

    def apply(record: dict) -> None:
        node = record
        for key in keys[:-1]:
            node = node[key]
        node[keys[-1]] = value

    return apply


def fail_session(record: dict, reason: str = "oom") -> None:
    session = record["stt"]["sessions"][1]
    session["server"]["closed"].update(state="failed", reason=reason, code="unspecified")
    session["outcome"] = "session_failed"


def end_early(outcome: str, status: str | None, stop_reason: str | None = None):
    def apply(record: dict) -> None:
        session = record_mod.new_session("tts", 2, 5000 * MS)
        session.update(outcome=outcome, grpc_status=status, stop_reason=stop_reason)
        record["tts"]["sessions"][2] = session

    return apply


def closed_before_start(record: dict) -> None:
    end_early("session_failed", "OK")(record)
    session = record["tts"]["sessions"][2]
    session["boundaries"]["session_closed"] = 4999 * MS
    session["server"].update(closed={**closed(), "state": "failed"}, events_received=1)


def no_gpu(status: str, reason: str | None, interval: int | None):
    def apply(record: dict) -> None:
        record["gpu"].update(status=status, reason=reason, interval_ms=interval, samples=[])
        record["gpu"].update(summary=None, machine=None, devices=[])

    return apply


def stop_session(record: dict) -> None:
    record["stt"]["sessions"][1].update(outcome="stopped", stop_reason="unexpected_event")


def end_run(ended_by: str | None, error: str | None = None, sessions: int = 2):
    """A run that did not end by itself, with `sessions` of its three text-to-speech sessions."""

    def apply(record: dict) -> None:
        record["run"].update(ended_by=ended_by, error=error)
        del record["tts"]["sessions"][sessions:]

    return apply


def late_input(mode: str, lag: int):
    """Session 0 sends its timed input `lag` after it was due; everything else stays in order."""

    def apply(record: dict) -> None:
        session = record[mode]["sessions"][0]
        client, chain = session["client"], record_mod.CHAIN[mode]
        shift = lag - client["input_lag_ns"]
        for name in (*chain[chain.index(record_mod.INPUT[mode][1]) :], "session_closed"):
            session["boundaries"][name] += shift
        client["input_lag_ns"] = lag
        if mode == "stt":
            client["max_pacing_lag_ns"] = max(lag, client["max_pacing_lag_ns"])

    return apply


def too_many(record: dict) -> None:
    end_run("interrupt")(record)
    record["tts"]["sessions"] += [tts_session(2, 5000 * MS, 80), tts_session(3, 6000 * MS, 80)]


def puts(*edits: tuple[str, object]):
    return lambda record: [put(path, value)(record) for path, value in edits]


def no_channel(record: dict) -> None:
    record["tts"]["target"]["connect_ns"] = None
    record["tts"]["sessions"] = []


S0, T0 = "stt.sessions.0", "tts.sessions.0"

# Shapes the schema refuses: text, audio, names and addresses have no field,
# and a server stage cannot carry a duration.
SHAPE_CASES = (
    (f"{S0}.server.final.text", "synthetic words"),
    (f"{S0}.audio", "AAAA"),
    ("tts.input.voice", ""),
    ("tts.input.voice_sha256", DIGEST),
    ("stt.input.language", "\u00e9"),
    ("stt.input.language_sha256", DIGEST),
    ("record_kind", "deployment_measurement"),
    ("settings.max_input_lag_ms", 5000),
    ("run.ended_by", "killed"),
    ("run.error", "no such file: /synthetic"),
    ("worker.source", "reported"),
    ("worker.version", "synthetic/path"),
    ("client_platform.hostname", "synthetic-host"),
    ("gpu.machine", "worker"),
    ("gpu.devices.0.serial", "0000"),
    ("gpu.failed_queries", None),
    (f"{S0}.stop_reason", "credit"),
    ("stt.target.endpoint", "localhost:1"),
    ("stt.target.deployment_id", "synthetic-deployment"),
    (f"{S0}.boundaries.call_start", None),
    (f"{S0}.server_stages.4.ns", 120 * MS),
    (f"{S0}.server_stages.4.status", "measured"),
    (f"{S0}.server_stages.0.stage", "decode"),
    (f"{S0}.outcome", "passed"),
    (f"{S0}.stop_reason", "gave_up"),
    (f"{T0}.server.closed.state", "active"),
    ("percentile_method", "linear"),
    ("clock.source", "wall"),
    ("gpu.scope", "per_model"),
    ("result.status", "pass"),
    ("result.passed", True),
)

# Records of the right shape that contradict themselves: (edit, what the refusal names).
STRUCTURE_CASES = (
    (put("settings.iterations", 2), "stt: 3 sessions recorded, 2 expected"),
    (put("tool.generator", "1.81.0"), "tool: the generator is not the grpcio release"),
    (closed_before_start, "tts session 2: session_closed precedes an event read"),
    (put("stt.sessions.1.index", 2), "stt session 1: carries index 2"),
    (put("stt.sessions.1.boundaries.call_start", 400 * MS), "starts before the session ahead"),
    (put(f"{S0}.boundaries.ready", 9 * MS), "ready precedes the boundary before it"),
    (put(f"{S0}.boundaries.finalize_sent", None), "endpoint is set and finalize_sent, which"),
    (put(f"{T0}.boundaries.first_audio_chunk", 3000 * MS), "first_audio_chunk precedes"),
    (put(f"{S0}.boundaries.first_partial", 11 * MS), "first_partial is outside the utterance"),
    (put(f"{S0}.boundaries.first_partial", 900 * MS), "first_partial is outside the utterance"),
    (put(f"{S0}.boundaries.session_closed", 400 * MS), "session_closed precedes an event read"),
    (put(f"{S0}.boundaries.session_closed", 9 * MS), "session_closed precedes an event read"),
    (lambda r: r["stt"]["sessions"][0]["server_stages"].reverse(), "not the registered stage"),
    (lambda r: r["tts"]["sessions"][0]["server_stages"].pop(), "not the registered stage"),
    (put(f"{S0}.stop_reason", "sequence_gap"), "stop_reason does not agree with outcome completed"),
    (put(f"{S0}.outcome", "stopped"), "stop_reason does not agree with outcome stopped"),
    (put(f"{S0}.grpc_status", "FINE"), "'FINE' is not a gRPC status code"),
    (put(f"{S0}.grpc_status", "UNAVAILABLE"), "completed without every boundary, a clean close"),
    (put(f"{S0}.grpc_status", None), "completed without every boundary, a clean close"),
    (put(f"{S0}.server.closed.state", "failed"), "completed without every boundary, a clean"),
    (put(f"{T0}.server.closed.reason", "internal"), "completed without every boundary, a clean"),
    (put(f"{T0}.server.closed.code", "internal"), "completed without every boundary, a clean"),
    (put(f"{T0}.boundaries.synthesis_completed", None), "completed without every boundary"),
    (put(f"{T0}.server.closed.reason", "gave_up"), "'gave_up' is not a reason the protocol"),
    (put(f"{T0}.server.closed.code", "gave_up"), "'gave_up' is not a code the protocol"),
    (put(f"{S0}.server.closed", None), "SessionClosed and its timestamp do not agree"),
    (put(f"{S0}.boundaries.session_closed", None), "SessionClosed and its timestamp do not agree"),
    (put(f"{S0}.server.final", None), "completed without the answers it measures"),
    (put(f"{S0}.server.limits", None), "completed without the answers it measures"),
    (put(f"{T0}.server.synthesis", None), "completed without the answers it measures"),
    (put(f"{T0}.server.output_format", None), "completed without an output format"),
    (put(f"{S0}.outcome", "session_failed"), "session_failed without a failed SessionClosed"),
    (put(f"{S0}.outcome", "call_failed"), "call_failed needs a status that explains it"),
    (put(f"{S0}.outcome", "timeout"), "a timeout carries a status"),
    (end_early("call_failed", None), "call_failed needs a status that explains it"),
    (end_early("session_failed", "OK"), "session_failed without a failed SessionClosed"),
    (put("tts.target.connect_ns", None), "tts: 3 sessions recorded, 0 expected"),
    (put("stt.input.format.sample_rate_hz", 0), "a frame is not a whole number of samples"),
    (put("stt.input.audio_bytes", SAMPLES * 2 + 1), "audio_bytes is not samples times"),
    (put("stt.input.format.encoding", "unspecified"), "audio_bytes is not samples times"),
    (put("stt.input.format.sample_rate_hz", 11025), "a frame is not a whole number of samples"),
    (put("stt.input.frames", 9), "frames is not samples divided into frames"),
    (put(f"{S0}.client.frames_sent", 0), "frames_sent and first_audio_sent do not agree"),
    (put(f"{S0}.client.max_pacing_lag_ns", None), "max_pacing_lag_ns and first_audio_sent"),
    (put(f"{S0}.client.frames_sent", 9), "last_audio_sent is set before every frame was sent"),
    (put(f"{S0}.boundaries.first_audio_sent", 15 * MS), "is not the last frame's place in time"),
    (put(f"{S0}.boundaries.last_audio_scheduled", 195 * MS), "last_audio_sent precedes"),
    (put(f"{S0}.client.max_pacing_lag_ns", MS - 1), "is below the last frame's lag"),
    (put(f"{S0}.client.input_lag_ns", 0), "is not last_audio_sent less last_audio_scheduled"),
    (put(f"{T0}.client.input_lag_ns", None), "is not text_segment_sent less text_segment_sched"),
    (put(f"{S0}.server.events_received", 0), "events_received is below the events the record"),
    (put(f"{T0}.server.events_received", 6), "events_received is below the events the record"),
    (put("provenance", "recorded"), "provenance: recorded needs the tool's source commit"),
    (
        puts(("provenance", "recorded"), ("tool.source_commit", "0" * 40)),
        "provenance: recorded needs the worker's release or build",
    ),
    (put("stt.target.loopback", False), "gpu: machine is 'serving' and the targets make it"),
    (put("gpu.machine", "client_only"), "gpu: machine is 'client_only' and the targets make"),
    (put("gpu.devices", []), "gpu: devices are not the devices the samples name"),
    (put("gpu.devices.0.index", 1), "gpu: devices are not the devices the samples name"),
    (lambda r: r["gpu"]["devices"].append(r["gpu"]["devices"][0]), "gpu: devices are not"),
    (put("worker.source", "operator_stated"), "worker: source does not agree"),
    (put("worker.build", "b1"), "worker: source does not agree"),
    (put("run.error", "KeyError"), "run: error does not agree with ended_by"),
    (put("run.ended_by", "error"), "run: error does not agree with ended_by"),
    (put("stt.target.generation", None), "stt target: a generation without a digest"),
    (put("tts.target.descriptor_digest", None), "tts target: a generation without a digest"),
    (put("gpu.samples", []), "gpu: sampled needs samples, an interval and no reason"),
    (put("gpu.interval_ms", None), "gpu: sampled needs samples, an interval and no reason"),
    (put("gpu.reason", "it ran"), "gpu: sampled needs samples, an interval and no reason"),
    (put("gpu.status", "unavailable"), "gpu: unavailable carries samples or the wrong reason"),
    (put("gpu.status", "not_requested"), "gpu: not_requested carries samples or the wrong"),
    (no_gpu("unavailable", None, 1000), "gpu: unavailable carries samples or the wrong reason"),
    (no_gpu("not_requested", "off", None), "gpu: not_requested carries samples or the wrong"),
    (no_gpu("not_requested", None, 1000), "gpu: not_requested carries an interval"),
    (lambda r: r["gpu"]["samples"].reverse(), "gpu: samples are not in time order"),
)

# A derived field edited to say something the timestamps do not.
DERIVED_CASES = (
    (f"{S0}.measurements.last_audio_to_final_transcript.ns", 30 * MS),
    (f"{S0}.measurements.last_audio_to_final_transcript.from", "finalize_sent"),
    (f"{S0}.measurements.last_audio_to_final_transcript.ended_by", "partial_transcript"),
    (f"{T0}.measurements.segment_to_first_audio.ns", None),
    ("stt.summary.last_audio_to_final_transcript.p95_ns", 200 * MS),
    ("stt.summary.session_initialization.misses", 1),
    ("tts.summary.segment_to_first_audio.count", 4),
    ("gpu.summary.max_utilization_percent", 37),
    ("result.reasons", ["nothing to see"]),
    ("result.status", "incomplete"),
)

# Records the check trusts once their derived fields are recomputed, with the
# status they then carry: (edit, status, what the reasons name).
VERDICT_CASES = (
    (fail_session, "failed", "stt session 1: session failed (oom)"),
    (lambda r: fail_session(r, "unspecified"), "failed", "session failed (unspecified)"),
    (end_early("call_failed", "FAILED_PRECONDITION"), "failed", "tts session 2: call ended"),
    (end_early("stopped", None, "sequence_gap"), "failed", "tts session 2: stopped (sequence_gap)"),
    (end_early("timeout", None), "failed", "tts session 2: timeout"),
    (no_channel, "failed", "tts: the channel never became ready"),
    (put("tts", None), "incomplete", "tts: not run"),
    (put("stt", None), "incomplete", "stt: not run"),
    (put(f"{S0}.server.final.text_bytes", 0), "incomplete", "no sample for call_start_to_first"),
    (no_gpu("unavailable", "no sampler on this machine", 1000), "complete", ""),
    (no_gpu("not_requested", None, None), "complete", ""),
    (end_run("interrupt"), "failed", "run: interrupted"),
    (end_run("error", "KeyError", 0), "failed", "run: ended by an error (KeyError)"),
    (end_run(None), "failed", "run: did not end"),
    (late_input("stt", 100 * MS), "complete", ""),
    (late_input("stt", 100 * MS + 1), "incomplete", "no sample for last_audio_to_final_trans"),
    (late_input("tts", 100 * MS), "complete", ""),
    (late_input("tts", 100 * MS + 1), "incomplete", "no sample for segment_to_first_audio"),
    (late_input("tts", 100 * MS + 1), "incomplete", "no sample for segment_to_completion"),
    (stop_session, "failed", "stt session 1: stopped (unexpected_event)"),
    # Refused whatever the derived fields say: these are recomputed first.
    (put(f"{S0}.boundaries.first_partial", 53 * MS), "invalid", "first_partial is set and no par"),
    (
        puts((f"{T0}.server.chunks", 0), (f"{T0}.server.segment.chunk_count", 0)),
        "invalid",
        "audio is recorded and no chunk arrived",
    ),
    (too_many, "invalid", "tts: 4 sessions recorded, 3 expected"),
    (lambda r: r["stt"]["sessions"].pop(), "invalid", "stt: 2 sessions recorded, 3 expected"),
    (
        puts(("stt.target.generation", None), ("stt.target.descriptor_digest", None)),
        "complete",
        "",
    ),
    (puts(("tts.target.loopback", False), ("gpu.machine", "client_only")), "complete", ""),
    (puts(("worker.source", "operator_stated"), ("worker.version", "0.3.1")), "complete", ""),
    (
        puts(
            ("provenance", "recorded"),
            ("tool.source_commit", "0" * 40),
            ("worker.source", "operator_stated"),
            ("worker.build", "b1"),
        ),
        "complete",
        "",
    ),
    (put(f"{S0}.server.closed.admission", "not_admitted"), "failed", "admission is"),
    (put(f"{S0}.client.samples_sent", SAMPLES - 1), "failed", "samples sent is"),
    (put(f"{S0}.client.bytes_sent", SAMPLES), "failed", "bytes sent is"),
    (put(f"{S0}.server.endpoint.reason", "silence"), "failed", "endpoint reason is"),
    (put(f"{S0}.server.endpoint.end_sample_offset", 9), "failed", "endpoint end_sample_offset is"),
    (put(f"{S0}.server.final.end_sample_offset", 9), "failed", "final end_sample_offset is"),
    (put(f"{S0}.server.final.finalize_sequence", 0), "failed", "final finalize_sequence is"),
    (put(f"{S0}.server.closed.totals.input_audio_samples", 9), "failed", "total input_audio_samp"),
    (put(f"{S0}.server.closed.totals.utterances_completed", 0), "failed", "utterances_completed"),
    (put(f"{T0}.server.audio_samples", 0), "failed", "no audio sample arrived"),
    (put(f"{T0}.server.audio_bytes", 1921), "failed", "the audio is not whole samples"),
    (put(f"{T0}.server.output_format.encoding", "unspecified"), "failed", "not whole samples"),
    (put(f"{T0}.server.segment.segment_id", 2), "failed", "segment id is"),
    (put(f"{T0}.server.segment.total_samples", 480), "failed", "segment total_samples is"),
    (put(f"{T0}.server.segment.chunk_count", 1), "failed", "segment chunk_count is"),
    (put(f"{T0}.server.synthesis.segment_count", 2), "failed", "synthesis segment_count is"),
    (put(f"{T0}.server.synthesis.total_samples", 480), "failed", "synthesis total_samples is"),
    (put(f"{T0}.server.synthesis.finalize_sequence", 2), "failed", "synthesis finalize_sequence"),
    (put(f"{T0}.server.closed.totals.output_audio_samples", 9), "failed", "total output_audio_sa"),
    (put(f"{T0}.server.closed.totals.input_text_bytes", 21), "failed", "total input_text_bytes"),
    (put(f"{T0}.server.closed.totals.segments_completed", 2), "failed", "total segments_complet"),
)


def check_rejections(good: dict, work: Path) -> None:
    schema = json.loads(SCHEMA.read_text(encoding="utf-8"))

    def case(apply, status: str, needle: str, *, rederive: bool = False) -> dict:
        record = copy.deepcopy(good)
        apply(record)
        if rederive:
            record_mod.finalize(record)
        found, detail = record_mod.check_record(record)
        assert found == status and needle in detail, (needle, found, detail)
        return record

    for path, value in SHAPE_CASES:
        record = case(put(path, value), "invalid", "record does not match its schema")
        try:
            jsonschema.validate(record, schema)
        except jsonschema.ValidationError:
            continue
        raise AssertionError(f"jsonschema accepts {path} = {value!r}")
    for apply, needle in STRUCTURE_CASES:
        jsonschema.validate(case(apply, "invalid", needle), schema)
    for path, value in DERIVED_CASES:
        case(put(path, value), "invalid", "differs from the one derived from the timestamps")
    for apply, status, needle in VERDICT_CASES:
        jsonschema.validate(case(apply, status, needle, rederive=True), schema)

    # A failed session cannot be reported as anything else, and the exit status follows.
    for claimed in ("complete", "incomplete"):

        def claim(record: dict, claimed: str = claimed) -> None:
            fail_session(record)
            record_mod.finalize(record)
            record["result"] = {"status": claimed, "reasons": []}

        case(claim, "invalid", "result differs from the one derived")
    for apply, code, needle in (
        (fail_session, 1, "failed: stt session 1"),
        (put("tts", None), 2, "incomplete: tts: not run"),
    ):
        record = copy.deepcopy(good)
        apply(record)
        completed = run_check(record_mod.finalize(record), work)
        assert completed.returncode == code and needle in completed.stderr, completed
    edited = copy.deepcopy(good)
    edited["result"]["status"] = "incomplete"
    completed = run_check(edited, work)
    assert completed.returncode == 2 and "invalid: result differs" in completed.stderr, completed
    (work / "unreadable.json").write_text("{", encoding="utf-8")
    command = [sys.executable, str(RECORD_TOOL), "check", str(work / "unreadable.json")]
    completed = subprocess.run(command, capture_output=True, text=True, check=False)
    assert completed.returncode == 2 and "invalid:" in completed.stderr, completed


def check_samples(good: dict) -> None:
    """Only a session that completed with no finding gives a sample; the lag bound is exact."""
    whole = good["stt"]["summary"]["last_audio_to_final_transcript"]
    assert (whole["count"], whole["misses"], whole["min_ns"]) == (3, 0, 100 * MS), whole
    for edit, status in (
        (fail_session, "failed"),
        (stop_session, "failed"),
        (put("stt.sessions.1.server.final.finalize_sequence", 0), "failed"),
    ):
        record = copy.deepcopy(good)
        edit(record)
        record_mod.finalize(record)
        assert record_mod.check_record(record)[0] == status, record_mod.check_record(record)
        session = record["stt"]["sessions"][1]
        # The session's own timestamps still give its durations: the fastest final of the three.
        assert session["boundaries"]["final_transcript"] is not None
        assert session["measurements"]["last_audio_to_final_transcript"]["ns"] == 100 * MS
        for name, summary in record["stt"]["summary"].items():
            assert (summary["count"], summary["misses"]) == (2, 1), (name, summary)
        assert record["stt"]["summary"]["last_audio_to_final_transcript"]["min_ns"] == 200 * MS
    for mode, late in (("stt", "last_audio_to_"), ("tts", "segment_to_")):
        record = copy.deepcopy(good)
        late_input(mode, record_mod.MAX_INPUT_LAG_MS * MS + 1)(record)
        record_mod.finalize(record)
        entries = record[mode]["sessions"][0]["measurements"]
        for name, summary in record[mode]["summary"].items():
            missed = name.startswith(late)
            assert (summary["count"], summary["misses"]) == ((2, 1) if missed else (3, 0)), name
            assert (entries[name]["ns"] is None) == missed, (name, entries[name])


# --- the client, against a server built from the same generated code ----------

TARGET = ("synthetic-deployment", 7, "sha256:" + "0" * 64)
TEXT = "synthetic sentence one"
VOICE = "voice-a"
CHUNKS = (960, 960, 400)


def recorded_ready(pb, name: str):
    """The Ready of a recorded golden session, decoded with the generated bindings."""
    for line in (
        (FRAMES_DIR / f"{name[:3]}_session.frames").read_text(encoding="utf-8").splitlines()
    ):
        fields = line.split("\t")
        if fields[0] == name:
            return pb.ServerEvent.FromString(bytes.fromhex(fields[2])).ready
    raise AssertionError(f"no recorded frame {name}")


class SliceServer:
    """Answers one session as the stream schema states it, or wrong in the way `fault` names.

    It notes in `violations` everything the client sends that the schema does
    not allow, and in `seen` what it received.
    """

    def __init__(self, bindings, fault: str = "", delay_s: float = 0.0) -> None:
        self.pb, self.fault, self.delay_s = bindings.pb, fault, delay_s
        self.violations: list[str] = []
        self.seen: dict = {"opens": [], "audio_at": [], "pings": 0, "frames": []}
        self._sequence = 0
        # Set once `client` and `out`, the process the `interrupt` fault signals, are known.
        self.armed = threading.Event()

    def _emit(self, skip: int = 0, **body):
        self._sequence += 1 + skip
        return self.pb.ServerEvent(sequence=self._sequence, **body)

    def _closed(self, last: int, cause=None, **totals):
        pb = self.pb
        failed = cause is not None
        closed = pb.SessionClosed(
            state=pb.SESSION_STATE_FAILED if failed else pb.SESSION_STATE_CLOSED,
            admission=pb.ADMISSION_ADMITTED,
            last_accepted_sequence=last,
            totals=pb.SessionTotals(**totals),
        )
        if failed:
            closed.cause.CopyFrom(pb.EndCause(reason=cause[0], code=cause[1], detail="synthetic"))
        return self._emit(session_closed=closed)

    def _expect(self, condition: bool, what: str) -> None:
        if not condition:
            self.violations.append(what)

    def Session(self, requests, context):
        pb, fault = self.pb, self.fault
        self._sequence, self._context = 0, context
        first = next(requests)
        opened = first.open
        self.seen["opens"].append(opened)
        self.seen["open_at"] = time.monotonic_ns()
        self._expect(first.sequence == 1 and first.session_id == "", "Open is not event 1")
        self._expect(opened.WhichOneof("target") == "resolved", "Open names no resolved target")
        if fault == "refuse":
            context.abort(grpc.StatusCode.FAILED_PRECONDITION, "synthetic refusal")
        stt = opened.mode == pb.SESSION_MODE_STT_STREAMING
        if fault == "fail_before_ready":
            yield self._closed(
                0, (pb.FAILURE_REASON_BACKEND_UNAVAILABLE, pb.ERROR_CODE_UNAVAILABLE)
            )
            return
        if fault == "event_before_ready":
            yield self._accepted(0, 0, 0)
        ready = recorded_ready(pb, "stt_ready" if stt else "tts_ready")
        ready.generation = opened.resolved.generation + (fault == "generation")
        ready.descriptor_digest = opened.resolved.descriptor_digest
        if fault == "digest":
            ready.descriptor_digest = "sha256:" + "1" * 64
        if fault == "ready_names_no_target":
            ready.generation, ready.descriptor_digest = 0, ""
        if stt:
            ready.input_format.CopyFrom(opened.input_format)
            if fault == "format":
                ready.input_format.sample_rate_hz = 8000
            if fault == "frame_limit":
                ready.limits.speech_to_text.min_frame_ms = 40
            if fault == "frame_max":
                ready.limits.speech_to_text.max_frame_ms = 10
            if fault == "utterance_limit":
                ready.limits.speech_to_text.max_utterance_ms = 100
            if fault == "credit":
                ready.status.input_credit.bytes.limit = 1280
            if fault == "no_byte_budget":
                ready.status.input_credit.bytes.limit = 0
        else:
            if fault == "text_limit":
                ready.limits.text_to_speech.max_segment_text_bytes = 4
            if fault == "synthesis_limit":
                ready.limits.text_to_speech.max_synthesis_text_bytes = 4
            if fault == "segment_credit":
                ready.status.input_credit.waiting_segments.used = 2
            if fault == "no_segment_budget":
                ready.status.input_credit.waiting_segments.limit = 0
            if fault == "byte_credit":
                ready.status.input_credit.bytes.limit = 8
        if fault == "heartbeat":
            ready.limits.heartbeat_interval_ms = 50
        if fault == "no_ready_status":
            ready.ClearField("status")
        if fault == "slow_ready":
            time.sleep(0.08)
        yield self._emit(ready=ready)
        if fault == "ready_twice":
            yield self._emit(ready=ready)
        if fault == "drop":
            context.abort(grpc.StatusCode.UNAVAILABLE, "synthetic drop")
        if fault == "clean_early":
            yield self._closed(0)
            return
        if fault == "wrong_mode_event":
            # A well-formed event of the other mode, at a point its own mode would allow it.
            other = (
                {"audio_chunk": self._chunk(ready, 0, 0, 2)}
                if stt
                else {"partial_transcript": pb.PartialTranscript(utterance_id=1, text="s")}
            )
            yield self._emit(**other)
        if fault == "cancel_accepted":
            yield self._emit(cancel_accepted=pb.CancelAccepted())
        if fault == "unknown_body":
            yield self._emit()
        if fault == "closed_state":
            yield self._emit(session_closed=pb.SessionClosed(state=pb.SESSION_STATE_ACTIVE))
        yield from (self._stt if stt else self._tts)(requests, ready, opened)

    def _chunk(self, ready, index: int, offset: int, size: int):
        return self.pb.AudioChunk(
            segment_id=2 if self.fault == "wrong_segment" else 1,
            chunk_index=index,
            segment_sample_offset=offset,
            format=ready.output_format,
            data=bytes(size),
        )

    def _inputs(self, requests, ready):
        """The client's events after Open with when each arrived, held to sequence and session."""
        arrived: queue.Queue = queue.Queue()

        def read() -> None:
            try:
                for event in requests:
                    arrived.put((time.monotonic_ns(), event))
            except grpc.RpcError:
                pass  # the client ended the call
            finally:
                arrived.put(None)

        threading.Thread(target=read, daemon=True).start()
        for expected, (at, event) in enumerate(iter(arrived.get, None), start=2):
            self._expect(event.sequence == expected, f"event {expected} carries {event.sequence}")
            self._expect(event.session_id == ready.session_id, "an event without the session id")
            yield at, event

    def _accepted(self, sequence: int, used: int, limit: int, segments=None):
        pb = self.pb
        credit = pb.InputCredit(bytes=pb.Usage(used=used, limit=limit), waiting_segments=segments)
        if self.fault == "accepted_without_credit":
            return self._emit(accepted=pb.Accepted(accepted_sequence=sequence))
        return self._emit(accepted=pb.Accepted(accepted_sequence=sequence, input_credit=credit))

    def _stt(self, requests, ready, opened):
        pb, fault = self.pb, self.fault
        limits, rate = ready.limits.speech_to_text, opened.input_format.sample_rate_hz
        limit = ready.status.input_credit.bytes.limit
        samples = used = last = finals = finalized = returned_at = 0
        short = False
        for at, event in self._inputs(requests, ready):
            body = event.WhichOneof("body")
            if body == "ping":
                self.seen["pings"] += 1
                yield self._emit(pong=pb.Pong())
            elif body == "audio":
                audio = event.audio
                self.seen["audio_at"].append(at)
                self._expect(at >= returned_at, "audio sent before its credit returned")
                self.seen["frames"].append(audio.sample_count)
                length_ms = audio.sample_count * 1000 / rate
                self._expect(not finalized, "audio after Finalize")
                self._expect(not short, "a short frame that is not the last")
                self._expect(audio.sample_offset == samples, "sample_offset is not cumulative")
                self._expect(audio.sample_count * 2 == len(audio.data), "sample_count is wrong")
                self._expect(length_ms <= limits.max_frame_ms, "a frame above the limit")
                short = length_ms < limits.min_frame_ms
                samples, used, last = (
                    samples + audio.sample_count,
                    used + len(audio.data),
                    event.sequence,
                )
                self._expect(not limit or used <= limit, "input credit exceeded")
                if fault == "early_endpoint":
                    yield self._emit(
                        endpoint_detected=pb.EndpointDetected(
                            utterance_id=1,
                            end_sample_offset=samples,
                            reason=pb.ENDPOINT_REASON_SILENCE,
                        )
                    )
                if fault == "partial" and len(self.seen["frames"]) <= 3:
                    text = ("", " \n", "synthetic")[len(self.seen["frames"]) - 1]
                    partial = pb.PartialTranscript(utterance_id=1, revision=1, text=text)
                    yield self._emit(partial_transcript=partial)
                if len(self.seen["frames"]) % 2 == 0:
                    if fault == "credit":
                        # Both frames stay held for 60 ms; the credit then returns unasked.
                        yield self._accepted(last, used, limit)
                        time.sleep(0.06)
                        returned_at = time.monotonic_ns()
                    used = 0
                    yield self._accepted(last, used, limit)
            elif body == "finalize":
                finalized = event.sequence
                if used:
                    yield self._accepted(last, 0, limit)
                time.sleep(self.delay_s)
                if fault == "stall":
                    continue
                if fault == "interrupt" and len(self.seen["opens"]) == 2:
                    self.armed.wait()
                    self.seen["checkpoint"] = json.loads(self.out.read_text(encoding="utf-8"))
                    os.kill(self.client.pid, signal.SIGINT)
                    continue
                if fault in ("fail_after_finalize", "event_after_closed"):
                    yield self._closed(last, (pb.FAILURE_REASON_OOM, pb.ERROR_CODE_OOM_ERROR))
                    if fault == "event_after_closed":
                        yield self._emit(pong=pb.Pong())
                    return
                endpoint = pb.EndpointDetected(
                    utterance_id=1,
                    end_sample_offset=samples,
                    reason=pb.ENDPOINT_REASON_CLIENT_FINALIZE,
                )
                text = {"empty": "", "blank": " \n"}.get(fault, "synthetic words")
                final = pb.FinalTranscript(
                    utterance_id=1,
                    segments=[pb.TranscriptSegment(end_us=samples * 1_000_000 // rate, text=text)],
                    end_sample_offset=samples,
                    finalize_sequence=0 if fault == "wrong_finalize" else finalized,
                )
                if fault != "final_first":
                    yield self._emit(skip=fault == "gap", endpoint_detected=endpoint)
                if fault == "endpoint_twice":
                    yield self._emit(endpoint_detected=endpoint)
                if fault == "partial_late":
                    yield self._emit(partial_transcript=pb.PartialTranscript(utterance_id=1))
                yield self._emit(final_transcript=final)
                finals += 1
                if fault == "ok_without_closed":
                    return
            else:
                self._expect(False, f"a {body} event in a speech-to-text session")
        if fault == "trailing_utterance":
            reason = pb.ENDPOINT_REASON_HALF_CLOSE
            yield self._emit(
                endpoint_detected=pb.EndpointDetected(
                    utterance_id=2, end_sample_offset=samples, reason=reason
                )
            )
            yield self._emit(
                final_transcript=pb.FinalTranscript(utterance_id=2, end_sample_offset=samples)
            )
            finals += 1
        yield self._closed(last, input_audio_samples=samples, utterances_completed=finals)
        if fault == "abort_after_close":
            self._context.abort(grpc.StatusCode.INTERNAL, "synthetic abort")

    def _tts(self, requests, ready, opened):
        pb, fault = self.pb, self.fault
        credit = ready.status.input_credit
        text_bytes = samples = last = 0
        for _, event in self._inputs(requests, ready):
            body = event.WhichOneof("body")
            if body == "text_segment":
                segment = event.text_segment
                self.seen["text"] = segment.text
                text_bytes, last = len(segment.text.encode()), event.sequence
                self._expect(segment.segment_id == 1, "the segment id is not 1")
                self._expect(text_bytes <= credit.bytes.limit, "input credit exceeded")
                yield self._accepted(last, text_bytes, credit.bytes.limit, credit.waiting_segments)
                time.sleep(self.delay_s)
                if fault == "early_synthesis":
                    yield self._emit(synthesis_completed=pb.SynthesisCompleted(segment_count=1))
                sizes = () if fault == "no_audio" else CHUNKS
                for index, size in enumerate((1, *sizes) if fault == "odd_chunk" else sizes):
                    chunk = self._chunk(ready, index, samples, size)
                    if fault == "wrong_format":
                        chunk.format.sample_rate_hz = 16000
                    samples += size // 2
                    yield self._emit(audio_chunk=chunk)
                    if fault == "odd_chunk" and not index:
                        time.sleep(0.05)
                completed = pb.SegmentCompleted(
                    segment_id=1, total_samples=samples, chunk_count=len(CHUNKS)
                )
                yield self._emit(segment_completed=completed)
                if fault == "segment_twice":
                    yield self._emit(segment_completed=completed)
                if fault == "chunk_late":
                    yield self._emit(audio_chunk=self._chunk(ready, len(CHUNKS), samples, 2))
            elif body == "finalize":
                done = pb.SynthesisCompleted(
                    segment_count=1, total_samples=samples, finalize_sequence=event.sequence
                )
                yield self._emit(synthesis_completed=done)
                if fault == "synthesis_twice":
                    yield self._emit(synthesis_completed=done)
            elif body == "ping":
                yield self._emit(pong=pb.Pong())
            else:
                self._expect(False, f"a {body} event in a text-to-speech session")
        yield self._closed(
            last, input_text_bytes=text_bytes, output_audio_samples=samples, segments_completed=1
        )


def write_wav(path: Path, channels: int, rate: int, samples: int) -> Path:
    tone = [round(8000 * math.sin(n * 0.2)) for n in range(samples * channels)]
    with wave.open(str(path), "wb") as sink:
        sink.setnchannels(channels)
        sink.setsampwidth(2)
        sink.setframerate(rate)
        sink.writeframes(struct.pack(f"<{len(tone)}h", *tone))
    return path


class Harness:
    """A loopback gRPC server per scenario, and the inputs every scenario shares."""

    def __init__(self, bindings, work: Path) -> None:
        self.bindings, self.work = bindings, work
        self.wav = write_wav(work / "synthetic.wav", 1, 16000, 11 * 320 - 160)
        self.audio = client_mod.load_audio(self.wav, 20)
        self.audio["language"] = "en"
        self.text = {"text": TEXT, "language": "en", "voice": VOICE}

    def serve(self, servicer: SliceServer) -> tuple[grpc.Server, str]:
        server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
        self.bindings.rpc.add_SessionServiceServicer_to_server(servicer, server)
        port = server.add_insecure_port("127.0.0.1:0")
        server.start()
        return server, f"127.0.0.1:{port}"

    def run(
        self, mode: str, fault: str = "", delay_s: float = 0.0, identity=TARGET[1:], **settings
    ) -> tuple[dict, dict, SliceServer]:
        """One section against one server: the record, its first session and the server."""
        servicer = SliceServer(self.bindings, fault, delay_s)
        server, endpoint = self.serve(servicer)
        target = client_mod.Target(endpoint, TARGET[0], *identity)
        plan = self.audio if mode == "stt" else self.text
        settings = {"gpu_sampling": "off", "provenance": "synthetic", **settings}
        servicer.seen["run_at"] = time.monotonic_ns()
        try:
            record = client_mod.measure(
                self.bindings,
                stt=(target, plan) if mode == "stt" else None,
                tts=(target, plan) if mode == "tts" else None,
                **settings,
            )
        finally:
            server.stop(0)
        status, detail = record_mod.check_record(record)
        assert status == record["result"]["status"], (fault, status, detail)
        assert record["run"] == {"ended_by": "completed", "error": None}, (fault, record["run"])
        return record, record[mode]["sessions"][0], servicer


def check_bindings(bindings) -> None:
    names = {code.name for code in grpc.StatusCode}
    assert names == set(record_mod.GRPC_STATUS_NAMES), names
    assert bindings.versions["grpcio-tools"] == bindings.versions["grpcio"], bindings.versions
    pins = dict(re.findall(r"^(grpcio|protobuf)==(\S+)", PINS.read_text(), re.MULTILINE))
    installed = {name: bindings.versions[name] for name in pins}
    if os.environ.get("TP_SLICE_MEASURE_REQUIRE_PINS") == "1":
        assert installed == pins and len(pins) == 2, (installed, pins)
    elif installed != pins:
        print(f"slice measurement: running on {installed}, the pins are {pins}")
    # The measurement client is not the SDK and never loads it.
    for tool in (CLIENT_TOOL, RECORD_TOOL):
        source = tool.read_text(encoding="utf-8")
        assert re.search(r"^\s*(import|from)\s+tensorplate\b", source, re.MULTILINE) is None, tool
    assert not [name for name in sys.modules if name.split(".")[0] == "tensorplate"]
    pb = bindings.pb
    assert pb.DESCRIPTOR.package == "tensorplate.stream.v1"
    assert (
        client_mod.enum_name(pb.EndpointReason, pb.ENDPOINT_REASON_CLIENT_FINALIZE)
        == "client_finalize"
    )
    assert (
        client_mod.enum_name(pb.FailureReason, pb.FAILURE_REASON_INVALID_EVENT) == "invalid_event"
    )
    assert client_mod.enum_name(pb.ErrorCode, 999) == "unspecified"
    assert recorded_ready(pb, "stt_ready").limits.speech_to_text.max_frame_ms == 40

    # A generator of another release than grpcio is refused before anything is generated.
    real_version, client_mod._bindings = importlib.metadata.version, None
    importlib.metadata.version = lambda name: "0.0.1" if name == "grpcio-tools" else "0.0.2"
    try:
        client_mod.load_bindings(Path("unused"))
    except client_mod.MeasureError as exc:
        assert "grpcio-tools 0.0.1 does not match grpcio 0.0.2" in str(exc), exc
    else:
        raise AssertionError("a generator that does not match grpcio was accepted")
    finally:
        importlib.metadata.version, client_mod._bindings = real_version, bindings


def check_sessions(harness: Harness) -> None:
    pb = harness.bindings.pb
    delay_ns = 80 * MS

    record, session, server = harness.run("stt", delay_s=0.08)
    assert not server.violations, server.violations
    assert session["outcome"] == "completed" and session["grpc_status"] == "OK", session
    assert record["result"] == {"status": "incomplete", "reasons": ["tts: not run"]}
    opened = server.seen["opens"][0]
    assert (opened.resolved.deployment_id, opened.resolved.generation) == TARGET[:2]
    assert opened.resolved.descriptor_digest == TARGET[2] and opened.language == "en"
    assert record["stt"]["target"]["generation"] == 7 and record["stt"]["input"]["language"] == "en"
    assert opened.mode == pb.SESSION_MODE_STT_STREAMING and not opened.voice
    assert opened.input_format == pb.AudioFormat(
        encoding=pb.AUDIO_ENCODING_PCM_S16LE, sample_rate_hz=16000, channels=1
    )
    # The call start was stamped before the server could have read the Open.
    assert server.seen["open_at"] - server.seen["run_at"] >= session["boundaries"]["call_start"]
    # Eleven frames of 20 ms, the last one short, no faster than real time at either end.
    assert server.seen["frames"] == [320] * 10 + [160], server.seen["frames"]
    assert server.seen["audio_at"][-1] - server.seen["audio_at"][0] >= 180 * MS
    bounds, measured = session["boundaries"], session["measurements"]
    assert bounds["last_audio_sent"] - bounds["first_audio_sent"] >= 200 * MS
    assert measured["last_audio_to_final_transcript"]["ns"] >= delay_ns
    assert measured["call_start_to_first_transcript"]["ns"] >= 200 * MS + delay_ns
    assert measured["call_start_to_first_transcript"]["ended_by"] == "final_transcript"
    assert measured["session_initialization"]["ns"] == bounds["ready"] - bounds["call_start"] > 0
    assert session["client"] | {"max_pacing_lag_ns": 0, "input_lag_ns": 0} == {
        "events_sent": 13,
        "frames_sent": 11,
        "samples_sent": 3360,
        "bytes_sent": 6720,
        "pings_sent": 0,
        "credit_waits": 0,
        "finalize_sequence": 13,
        "max_pacing_lag_ns": 0,
        "input_lag_ns": 0,
    }
    assert bounds["last_audio_scheduled"] == bounds["first_audio_sent"] + 200 * MS
    assert (
        session["client"]["input_lag_ns"]
        == bounds["last_audio_sent"] - 200 * MS - (bounds["first_audio_sent"])
    )
    words = b"synthetic words"
    assert session["server"]["final"] == {
        "finalize_sequence": 13,
        "end_sample_offset": 3360,
        "segments": 1,
        "text_bytes": len(words),
        "text_sha256": client_mod.sha256_digest(words),
    }
    assert session["server"]["limits"] == {
        "min_frame_ms": 20,
        "max_frame_ms": 40,
        "max_utterance_ms": 30000,
    }
    assert session["server"]["closed"]["totals"]["input_audio_samples"] == 3360
    assert session["open_refusal"] is None
    stt_input = record["stt"]["input"]
    assert stt_input["audio_sha256"] == harness.audio["sha256"] and stt_input["frames"] == 11
    assert record["stt"]["target"]["loopback"] is True and record["stt"]["target"]["connect_ns"] > 0
    assert "synthetic" not in json.dumps({**record, "provenance": ""}), "a name or text is recorded"

    record, session, server = harness.run("tts", delay_s=0.08)
    assert not server.violations, server.violations
    assert session["outcome"] == "completed" and server.seen["text"] == TEXT
    opened = server.seen["opens"][0]
    assert opened.mode == pb.SESSION_MODE_TTS_STREAMING and opened.voice == VOICE
    assert record["tts"]["input"] | {"text_sha256": ""} == {
        "text_sha256": "",
        "text_bytes": len(TEXT),
        "language": "en",
        "voice": VOICE,
    }
    measured = session["measurements"]
    assert measured["segment_to_first_audio"]["ns"] >= delay_ns
    assert measured["segment_to_first_audio"]["ended_by"] == "audio_chunk"
    assert measured["segment_to_completion"]["ns"] >= measured["segment_to_first_audio"]["ns"]
    assert measured["call_start_to_first_audio"]["ns"] > measured["segment_to_first_audio"]["ns"]
    assert session["client"] | {"input_lag_ns": 0} == {
        "events_sent": 3,
        "pings_sent": 0,
        "credit_waits": 0,
        "segment_id": 1,
        "finalize_sequence": 3,
        "input_lag_ns": 0,
    }
    assert session["server"]["output_format"] == {
        "encoding": "pcm_s16le",
        "sample_rate_hz": 24000,
        "channels": 1,
    }
    assert (session["server"]["chunks"], session["server"]["audio_samples"]) == (3, 1160)
    assert session["server"]["audio_sha256"] == client_mod.sha256_digest(bytes(sum(CHUNKS)))
    assert session["server"]["synthesis"] == {
        "segment_count": 1,
        "total_samples": 1160,
        "finalize_sequence": 3,
    }
    assert record["tts"]["input"]["text_sha256"] == client_mod.sha256_digest(TEXT.encode())
    assert "synthetic" not in json.dumps({**record, "provenance": ""}), "a name or text is recorded"

    # A worker that reports no generation and no digest: the Open names none, the record has none.
    for mode in ("stt", "tts"):
        record, session, server = harness.run(mode, identity=(None, None))
        resolved = server.seen["opens"][0].resolved
        assert (resolved.deployment_id, resolved.generation, resolved.descriptor_digest) == (
            TARGET[0],
            0,
            "",
        )
        assert session["outcome"] == "completed" and not server.violations, session
        target = record[mode]["target"]
        assert (target["generation"], target["descriptor_digest"]) == (None, None), target
        assert record["record_kind"] == "slice_measurement"
    # A Ready that names a generation the Open did not is as much a mismatch as the reverse.
    session = harness.run("stt", "generation", identity=(None, None))[1]
    assert (session["outcome"], session["stop_reason"]) == ("stopped", "ready_mismatch"), session

    # Three sessions one after another: every sample is kept and none overlaps.
    record, _, server = harness.run("tts", iterations=3)
    summary = record["tts"]["summary"]["segment_to_first_audio"]
    assert (summary["count"], summary["misses"], len(server.seen["opens"])) == (3, 0, 3)
    assert [session["index"] for session in record["tts"]["sessions"]] == [0, 1, 2]


# A server fault the client ends the call for: (mode, fault, the record's stop reason).
STOP_CASES = (
    ("stt", "gap", "sequence_gap"),
    ("stt", "generation", "ready_mismatch"),
    ("tts", "digest", "ready_mismatch"),
    ("stt", "ready_names_no_target", "ready_mismatch"),
    ("stt", "format", "ready_mismatch"),
    ("stt", "frame_limit", "input_outside_limits"),
    ("stt", "frame_max", "input_outside_limits"),
    ("stt", "utterance_limit", "input_outside_limits"),
    ("tts", "text_limit", "input_outside_limits"),
    ("tts", "synthesis_limit", "input_outside_limits"),
    ("tts", "byte_credit", "input_outside_limits"),
    ("tts", "no_segment_budget", "input_outside_limits"),
    ("stt", "ready_twice", "unexpected_event"),
    ("stt", "event_before_ready", "unexpected_event"),
    ("stt", "event_after_closed", "unexpected_event"),
    ("stt", "wrong_mode_event", "unexpected_event"),
    ("tts", "wrong_mode_event", "unexpected_event"),
    ("stt", "cancel_accepted", "unexpected_event"),
    ("stt", "no_byte_budget", "credit_missing"),
    ("stt", "no_ready_status", "credit_missing"),
    ("tts", "no_ready_status", "credit_missing"),
    ("stt", "accepted_without_credit", "credit_missing"),
    ("tts", "accepted_without_credit", "credit_missing"),
    ("tts", "closed_state", "unexpected_event"),
    ("stt", "partial_late", "unexpected_event"),
    ("stt", "endpoint_twice", "unexpected_event"),
    ("tts", "chunk_late", "unexpected_event"),
    ("tts", "segment_twice", "unexpected_event"),
    ("tts", "synthesis_twice", "unexpected_event"),
    ("stt", "clean_early", "unexpected_event"),
    ("stt", "early_endpoint", "unexpected_event"),
    ("stt", "final_first", "unexpected_event"),
    ("tts", "early_synthesis", "unexpected_event"),
    ("tts", "wrong_segment", "unexpected_event"),
    ("tts", "wrong_format", "unexpected_event"),
    ("tts", "no_audio", "unexpected_event"),
)

# Every other ending: (mode, fault, outcome, status code, the record's status, what its
# reasons name).
END_CASES = (
    ("stt", "refuse", "call_failed", "FAILED_PRECONDITION", "failed", "after 0 server events"),
    ("tts", "refuse", "call_failed", "FAILED_PRECONDITION", "failed", "after 0 server events"),
    ("stt", "drop", "call_failed", "UNAVAILABLE", "failed", "after 1 server events"),
    ("stt", "abort_after_close", "call_failed", "INTERNAL", "failed", "call ended INTERNAL"),
    ("stt", "ok_without_closed", "call_failed", "OK", "failed", "call ended OK after"),
    ("stt", "slow_ready", "completed", "OK", "incomplete", "tts: not run"),
    ("stt", "fail_before_ready", "session_failed", "OK", "failed", "(backend_unavailable)"),
    ("stt", "fail_after_finalize", "session_failed", "OK", "failed", "session failed (oom)"),
    ("stt", "stall", "timeout", None, "failed", "stt session 0: timeout"),
    ("tts", "segment_credit", "timeout", None, "failed", "tts session 0: timeout"),
    ("stt", "wrong_finalize", "completed", "OK", "failed", "final finalize_sequence is 0"),
    ("tts", "odd_chunk", "completed", "OK", "failed", "the audio is not whole samples"),
    ("stt", "empty", "completed", "OK", "incomplete", "no sample for call_start_to_first"),
    ("stt", "blank", "completed", "OK", "incomplete", "no sample for call_start_to_first"),
    ("stt", "unknown_body", "completed", "OK", "incomplete", "tts: not run"),
    ("stt", "trailing_utterance", "completed", "OK", "incomplete", "tts: not run"),
    ("stt", "partial", "completed", "OK", "incomplete", "tts: not run"),
    ("stt", "credit", "completed", "OK", "incomplete", "tts: not run"),
    ("stt", "heartbeat", "completed", "OK", "incomplete", "tts: not run"),
)


def check_faults(harness: Harness) -> None:
    cases = [
        (mode, fault, "stopped", reason, None, "failed", f"stopped ({reason})")
        for mode, fault, reason in STOP_CASES
    ] + [(mode, fault, outcome, None, *rest) for mode, fault, outcome, *rest in END_CASES]
    sessions = {}
    for mode, fault, outcome, stop_reason, status, verdict, needle in cases:
        record, session, server = harness.run(mode, fault, event_timeout_ms=400)
        observed = (session["outcome"], session["stop_reason"], session["grpc_status"])
        assert observed == (outcome, stop_reason, status), (fault, observed)
        result = record["result"]
        assert (result["status"], needle in "; ".join(result["reasons"])) == (verdict, True), result
        assert not server.violations, (fault, server.violations)
        sessions[fault] = (session, server)

    assert not sessions["frame_limit"][1].seen["frames"], "audio was sent outside the limits"
    for fault in ("no_byte_budget", "no_ready_status"):
        session, server = sessions[fault]
        assert session["boundaries"]["ready"] is not None and "text" not in server.seen
        assert not server.seen["frames"], "input was sent without credit"
    # The whitespace is digested as it came and counted as no text.
    final = sessions["blank"][0]["server"]["final"]
    assert (final["text_bytes"], final["text_sha256"]) == (0, client_mod.sha256_digest(b" \n"))
    assert sessions["event_after_closed"][0]["server"]["closed"]["reason"] == "oom"
    assert sessions["drop"][0]["client"]["frames_sent"] < 11, "audio was paced into an ended call"
    assert sessions["abort_after_close"][0]["server"]["closed"]["state"] == "closed"
    assert "text" not in sessions["text_limit"][1].seen, "text was sent outside the limits"
    # No segment credit, and none returned: the client waits for it and sends nothing.
    session, server = sessions["segment_credit"]
    assert session["client"]["credit_waits"] == 1 and "text" not in server.seen, session["client"]
    assert sessions["unknown_body"][0]["server"]["ignored_events"] == 1
    assert (
        sessions["trailing_utterance"][0]["server"]["closed"]["totals"]["utterances_completed"] == 2
    )
    assert sessions["trailing_utterance"][0]["server"]["endpoint"]["reason"] == "client_finalize"
    closed = sessions["fail_after_finalize"][0]["server"]["closed"]
    assert (closed["state"], closed["reason"], closed["code"]) == ("failed", "oom", "oom_error")
    session = sessions["stall"][0]
    assert (
        session["boundaries"]["finalize_sent"] is not None
        and session["boundaries"]["endpoint"] is None
    )

    session = sessions["partial"][0]
    first = session["measurements"]["call_start_to_first_transcript"]
    assert (first["to"], first["ended_by"]) == ("first_partial", "partial_transcript"), first
    assert first["ns"] < session["measurements"]["session_initialization"]["ns"] + 200 * MS
    bounds = session["boundaries"]
    # The empty partial after the first frame and the blank one after the second are counted
    # and end nothing.
    assert session["server"]["partials"] == 3
    assert bounds["first_partial"] - bounds["first_audio_sent"] >= 35 * MS

    # The first chunk that holds whole samples ends the measurement, not the first chunk.
    session = sessions["odd_chunk"][0]
    assert session["measurements"]["segment_to_first_audio"]["ns"] >= 50 * MS
    assert session["server"]["audio_bytes"] == sum(CHUNKS) + 1

    # Two frames of credit, returned 60 ms after every second frame: each odd frame from the
    # third waits for it, and the server saw none arrive early.
    session, server = sessions["credit"]
    assert session["client"]["credit_waits"] == 5, session["client"]
    bounds = session["boundaries"]
    assert bounds["last_audio_sent"] - bounds["first_audio_sent"] >= 300 * MS
    assert session["client"]["max_pacing_lag_ns"] >= 100 * MS
    assert session["server"]["accepted_events"] == 11
    # The last frame went out 120 ms or more after it was due: no sample counted from it.
    assert session["client"]["input_lag_ns"] >= 120 * MS, session["client"]
    assert session["measurements"]["last_audio_to_final_transcript"]["ns"] is None
    assert session["measurements"]["call_start_to_first_transcript"]["ns"] is not None

    # The time to Ready is counted from before the call started.
    initialization = sessions["slow_ready"][0]["measurements"]["session_initialization"]
    assert initialization["ns"] >= 80 * MS, initialization

    session, server = sessions["heartbeat"]
    assert session["client"]["pings_sent"] == server.seen["pings"] >= 2, session["client"]
    assert session["client"]["events_sent"] == 13 + session["client"]["pings_sent"]


# --- the client's definitions, in virtual time ---------------------------------


class FedCall:
    """gRPC's side of a call: it reads what the test feeds it, and nothing once cancelled.

    `asked` counts the times the client asked for the next event.
    """

    def __init__(self) -> None:
        self.events: queue.Queue = queue.Queue()
        self.asked = threading.Semaphore(0)

    def __iter__(self):
        return self

    def __next__(self):
        self.asked.release()
        event = self.events.get()
        if event is None:
            raise StopIteration
        return event

    def Session(self, requests):
        return self

    def code(self):
        return grpc.StatusCode.CANCELLED

    def cancel(self) -> None:
        self.events.put(None)


class Script(client_mod.Run):
    """Virtual time and a scripted server, in place of the clock and gRPC.

    The clock moves only when the client waits: to the next arrival, or to
    the time it waited for. `tick` also moves it one nanosecond a reading,
    which orders a stamp against what follows it. `reply(script, name,
    event)` answers each event as the client hands it over, with pairs of a
    delay and a ServerEvent body or the call's status; they arrive in order.
    """

    def __init__(self, bindings, reply, tick: int = 0) -> None:
        self.origin = self.at = 0
        self.pb, self.reply, self.tick = bindings.pb, reply, tick
        self.owner = threading.current_thread()
        self.calls: list[int] = []
        self.handed: list[tuple[int, str]] = []

    def now(self) -> int:
        at = self.at
        if threading.current_thread() is self.owner:
            self.at += self.tick
        return at

    @contextlib.contextmanager
    def connect(self, bindings, endpoint, timeout_ms):
        yield self, 0

    def Session(self, requests):
        self.calls.append(self.at)
        return FedCall()

    def outbound(self):
        self._arriving: list = []
        self._sequence, self.state = 0, {}
        return self

    def get(self):
        raise AssertionError("nothing reads what a scripted call sends")

    def put(self, event) -> None:
        name = "half_close" if event is None else event.WhichOneof("body")
        self.handed.append((self.at, name))
        for delay, item in self.reply(self, name, event):
            if isinstance(item, dict):
                self._sequence += 1
                item = self.pb.ServerEvent(sequence=self._sequence, **item)
            self._arriving.append((self.at + delay, item))

    def take(self, inbound, until: int):
        if not self._arriving or self._arriving[0][0] > until:
            self.at = max(self.at, until)
            return None
        due, item = self._arriving.pop(0)
        self.at = max(self.at, due)
        return self.at, item

    def sent(self, name: str) -> list[int]:
        return [at for at, handed in self.handed if handed == name]


def replies(pb, mode: str, **knobs):
    """A server that answers as the schema states, moved by `knobs`.

    ready: an edit of Ready. unasked: ns after Ready at which credit returns
    unasked. hold: {input position: ns its Accepted is late}. answer_after:
    ns from a Finalize, or from the segment, to its answer. chunks and
    chunk_every: how many AudioChunks, and the ns between them. stall: no
    answer to Finalize. pongs: how many Pings are answered. on_finalize:
    called with the script as a Finalize is handed over. partials: the text
    of a PartialTranscript after each of the first frames. final_text: the
    final transcript's.
    """
    after = knobs.get("answer_after", 0)

    def returned(state, sequence: int):
        credit = pb.InputCredit()
        credit.CopyFrom(state["ready"].status.input_credit)
        credit.bytes.used = credit.waiting_segments.used = 0
        return {"accepted": pb.Accepted(accepted_sequence=sequence, input_credit=credit)}

    def reply(script, name: str, event):
        state = script.state
        if name == "finalize":
            knobs.get("on_finalize", lambda script: None)(script)
        if name == "open":
            ready = recorded_ready(pb, f"{mode}_ready")
            resolved = event.open.resolved
            ready.generation, ready.descriptor_digest = (
                resolved.generation,
                resolved.descriptor_digest,
            )
            if mode == "stt":
                ready.input_format.CopyFrom(event.open.input_format)
            knobs.get("ready", lambda ready: None)(ready)
            state.update(ready=ready, inputs=0, samples=0, last=0, pings=0, text=0)
            yield 0, {"ready": ready}
            if "unasked" in knobs:
                yield knobs["unasked"], returned(state, 0)
        elif name == "ping":
            state["pings"] += 1
            if state["pings"] <= knobs.get("pongs", state["pings"]):
                yield 0, {"pong": pb.Pong()}
        elif name in ("audio", "text_segment"):
            position, state["last"] = state["inputs"], event.sequence
            state["inputs"] += 1
            yield knobs.get("hold", {}).get(position, 0), returned(state, event.sequence)
            if name == "audio":
                state["samples"] += event.audio.sample_count
                for text in knobs.get("partials", ())[position : position + 1]:
                    partial = pb.PartialTranscript(utterance_id=1, revision=position + 1, text=text)
                    yield 0, {"partial_transcript": partial}
                return
            count, every = knobs.get("chunks", 3), knobs.get("chunk_every", 0)
            state.update(text=len(event.text_segment.text.encode()), samples=count * 480)
            for index in range(count):
                chunk = pb.AudioChunk(
                    segment_id=1,
                    chunk_index=index,
                    segment_sample_offset=index * 480,
                    format=state["ready"].output_format,
                    data=bytes(960),
                )
                yield after + index * every, {"audio_chunk": chunk}
            done = pb.SegmentCompleted(segment_id=1, total_samples=count * 480, chunk_count=count)
            yield after + count * every, {"segment_completed": done}
        elif name == "finalize" and mode == "stt" and not knobs.get("stall"):
            endpoint = pb.EndpointDetected(
                utterance_id=1,
                end_sample_offset=state["samples"],
                reason=pb.ENDPOINT_REASON_CLIENT_FINALIZE,
            )
            final = pb.FinalTranscript(
                utterance_id=1,
                segments=[pb.TranscriptSegment(text=knobs.get("final_text", "words"))],
                end_sample_offset=state["samples"],
                finalize_sequence=event.sequence,
            )
            yield after, {"endpoint_detected": endpoint}
            yield after, {"final_transcript": final}
        elif name == "finalize" and mode == "tts":
            done = pb.SynthesisCompleted(
                segment_count=1, total_samples=state["samples"], finalize_sequence=event.sequence
            )
            yield 0, {"synthesis_completed": done}
        elif name == "half_close":
            stt = mode == "stt"
            totals = pb.SessionTotals(
                input_audio_samples=state["samples"] if stt else 0,
                input_text_bytes=state["text"],
                output_audio_samples=0 if stt else state["samples"],
                utterances_completed=int(stt),
                segments_completed=int(not stt),
            )
            closed = pb.SessionClosed(
                state=pb.SESSION_STATE_CLOSED,
                admission=pb.ADMISSION_ADMITTED,
                last_accepted_sequence=state["last"],
                totals=totals,
            )
            yield 0, {"session_closed": closed}
            yield 0, grpc.StatusCode.OK

    return reply


def scripted(
    harness: Harness, mode: str, tick: int = 0, settings: dict | None = None,
    ended_by: str = "completed", **knobs,
):  # fmt: skip
    """One run against a scripted server at an address that is not loopback."""
    script = Script(harness.bindings, replies(harness.bindings.pb, mode, **knobs), tick)
    target = client_mod.Target("192.0.2.1:7", *TARGET)
    plan = harness.audio if mode == "stt" else harness.text
    record = client_mod.measure(
        harness.bindings,
        stt=(target, plan) if mode == "stt" else None,
        tts=(target, plan) if mode == "tts" else None,
        run=script,
        **{"gpu_sampling": "off", **(settings or {})},
    )
    status, detail = record_mod.check_record(record)
    assert status == record["result"]["status"], (status, detail)
    assert record["run"]["ended_by"] == ended_by, record["run"]
    sessions = record[mode]["sessions"] if record[mode] else []
    return record, sessions[0] if sessions else None, script


class HeldClock(client_mod.Run):
    """The real waits on a clock that reads what the test set."""

    def __init__(self) -> None:
        self.origin = self.at = 0

    def now(self) -> int:
        return self.at


# Each boundary a send stamps: (boundary, the event's body, which event of that body).
SEND_STAMPS = {
    "stt": (
        ("first_audio_sent", "audio", 0),
        ("last_audio_sent", "audio", -1),
        ("finalize_sent", "finalize", 0),
    ),
    "tts": (("text_segment_sent", "text_segment", 0), ("finalize_sent", "finalize", 0)),
}


def check_stamps(harness: Harness) -> None:
    """When each stamp is taken, against the action it stands for."""
    pb, bounds = harness.bindings.pb, {}
    for mode in ("stt", "tts"):
        record, session, script = scripted(harness, mode, tick=1)
        assert session["outcome"] == "completed", session
        bounds = session["boundaries"]
        # The call start is read before the call is made.
        assert bounds["call_start"] < script.calls[0], (bounds, script.calls)
        # A send is stamped before the event is handed to the transport.
        for boundary, name, which in SEND_STAMPS[mode]:
            assert bounds[boundary] < script.sent(name)[which], (boundary, bounds, script.handed)
    assert bounds["text_segment_scheduled"] < bounds["text_segment_sent"]

    # An event is stamped when it is read, however long it then waits to be handled: each is
    # read 100 ns after the one before, and all are handled at 900.
    ready = recorded_ready(pb, "tts_ready")
    chunk = pb.AudioChunk(segment_id=1, format=ready.output_format, data=bytes(960))
    for mode, sent, answer, bodies in (
        (
            "stt",
            "finalize_sent",
            "final",
            {
                "ready": recorded_ready(pb, "stt_ready"),
                "endpoint_detected": pb.EndpointDetected(utterance_id=1),
                "final_transcript": pb.FinalTranscript(utterance_id=1),
            },
        ),
        (
            "tts",
            "text_segment_sent",
            "segment",
            {
                "ready": ready,
                "audio_chunk": chunk,
                "segment_completed": pb.SegmentCompleted(segment_id=1, total_samples=480),
            },
        ),
    ):
        clock, fed = HeldClock(), FedCall()
        call = client_mod.Call(clock, harness.bindings, fed, mode, 0, 400 * MS)
        # The state the answers need: their input went out at 150, after Ready.
        call.session["boundaries"][sent] = 150
        call.session["client"]["segment_id"] = 1
        call.session["server"]["output_format"] = {"encoding": "pcm_s16le"}
        for position, (name, body) in enumerate(bodies.items(), start=1):
            fed.asked.acquire()
            clock.at = position * 100
            fed.events.put(pb.ServerEvent(sequence=position, **{name: body}))
        fed.asked.acquire()
        clock.at = 900
        call.wait(lambda call=call, answer=answer: call.session["server"][answer] is not None)
        call.close()
        read = [at for at in call.session["boundaries"].values() if at not in (None, 0, 150)]
        assert read == [100, 200, 300], call.session["boundaries"]


def check_schedule(harness: Harness) -> None:
    """Pacing, the scheduled start of each answer's measurement, and the event timeout."""
    frame, bound = 20 * MS, record_mod.MAX_INPUT_LAG_MS * MS

    def one_frame(ready) -> None:
        ready.status.input_credit.bytes.limit = 640

    # Frame 3 waits 7 ms for credit. The frames after it go at their own places, counted from
    # the first frame and not from the late one.
    record, session, script = scripted(harness, "stt", ready=one_frame, hold={2: 27 * MS})
    sent = script.sent("audio")
    assert [at - sent[0] for at in sent] == [
        n * frame + (7 * MS if n == 3 else 0) for n in range(11)
    ], sent
    client, bounds = session["client"], session["boundaries"]
    assert (client["credit_waits"], client["max_pacing_lag_ns"], client["input_lag_ns"]) == (
        1,
        7 * MS,
        0,
    ), client
    assert bounds["last_audio_scheduled"] == bounds["last_audio_sent"] == sent[0] + 10 * frame

    # Credit for the last frame comes 30 ms late and the final 50 ms after the Finalize. The
    # time to final is counted from when the frame was due: 80 ms, not the 50 after it went.
    for late, to_final in ((30 * MS, 80 * MS), (bound, bound + 50 * MS), (bound + 1, None)):
        record, session, script = scripted(
            harness, "stt", ready=one_frame, hold={9: frame + late}, answer_after=50 * MS
        )
        client, bounds = session["client"], session["boundaries"]
        assert session["outcome"] == "completed" and client["credit_waits"] == 1, session
        assert bounds["last_audio_scheduled"] == bounds["first_audio_sent"] + 10 * frame
        assert bounds["last_audio_sent"] - bounds["last_audio_scheduled"] == late
        assert client["input_lag_ns"] == late
        assert bounds["final_transcript"] - bounds["last_audio_sent"] == 50 * MS
        measured = session["measurements"]
        assert measured["last_audio_to_final_transcript"]["ns"] == to_final, measured
        assert measured["call_start_to_first_transcript"]["ns"] is not None
        summary = record["stt"]["summary"]["last_audio_to_final_transcript"]
        assert (summary["count"], summary["misses"]) == ((1, 0) if to_final else (0, 1)), summary
        missed = "stt session 0: no sample for last_audio_to_final_transcript"
        assert (missed in record["result"]["reasons"]) == (to_final is None), record["result"]
        assert record["settings"]["max_input_lag_ms"] == record_mod.MAX_INPUT_LAG_MS

    # The same for a text segment: no segment credit until it returns unasked.
    def no_segment_credit(ready) -> None:
        ready.status.input_credit.waiting_segments.used = 2

    for late, to_audio in ((30 * MS, 70 * MS), (bound, bound + 40 * MS), (bound + 1, None)):
        record, session, script = scripted(
            harness, "tts", ready=no_segment_credit, unasked=late, answer_after=40 * MS
        )
        client, bounds = session["client"], session["boundaries"]
        assert session["outcome"] == "completed" and client["credit_waits"] == 1, session
        assert bounds["text_segment_scheduled"] == bounds["ready"]
        assert client["input_lag_ns"] == late == script.sent("text_segment")[0] - bounds["ready"]
        assert bounds["first_audio_chunk"] - bounds["text_segment_sent"] == 40 * MS
        measured = session["measurements"]
        assert measured["segment_to_first_audio"]["ns"] == to_audio, measured
        assert measured["segment_to_completion"]["ns"] == to_audio, measured
        assert measured["call_start_to_first_audio"]["ns"] is not None

    # The timeout is counted from each event: ten chunks 100 ms apart outlast one of 400 ms.
    slow = {"event_timeout_ms": 400}
    record, session, script = scripted(
        harness, "tts", settings=slow, chunks=10, chunk_every=100 * MS, answer_after=100 * MS
    )
    assert session["outcome"] == "completed" and session["server"]["chunks"] == 10, session
    assert session["measurements"]["segment_to_completion"]["ns"] == 1100 * MS
    # The first of the ten chunks ends the time to first audio, not a later one.
    assert session["measurements"]["segment_to_first_audio"]["ns"] == 100 * MS

    # A Pong is not an event the session waits for: a server that answers Pings and not the
    # Finalize times out 400 ms after the Finalize, seven Pings later.
    def heartbeat(ready) -> None:
        ready.limits.heartbeat_interval_ms = 50

    record, session, script = scripted(
        harness, "stt", settings=slow, ready=heartbeat, stall=True, pongs=40
    )
    assert session["outcome"] == "timeout", session
    finalized = session["boundaries"]["finalize_sent"]
    assert script.at - finalized == 400 * MS, (script.at, finalized)
    assert len([at for at in script.sent("ping") if at > finalized]) == 7, script.sent("ping")

    # A partial or a final of nothing but whitespace is no transcript: the first partial is
    # the one read after the third frame, not the one after the fourth, and a blank final is
    # a miss.
    record, session, script = scripted(harness, "stt", partials=("", " \n", "word", "words"))
    assert session["server"]["partials"] == 4, session["server"]
    assert session["boundaries"]["first_partial"] == script.sent("audio")[2]
    record, session, script = scripted(harness, "stt", final_text=" \n")
    first = session["measurements"]["call_start_to_first_transcript"]
    assert (first["ns"], session["server"]["final"]["text_bytes"]) == (None, 0), session


def check_run_endings(harness: Harness) -> None:
    """A run that an error or an interrupt ends still gives a record, and it says so."""

    def fail_second(script) -> None:
        if len(script.calls) == 2:
            raise RuntimeError("synthetic failure")

    saved: list[dict] = []
    settings = {"iterations": 2, "checkpoint": lambda record: saved.append(copy.deepcopy(record))}
    with contextlib.redirect_stderr(io.StringIO()) as stderr:
        record, session, script = scripted(
            harness, "stt", settings=settings, ended_by="error", on_finalize=fail_second
        )
    assert "RuntimeError: synthetic failure" in stderr.getvalue()
    assert record["run"] == {"ended_by": "error", "error": "RuntimeError"}, record["run"]
    assert record["result"]["status"] == "failed", record["result"]
    assert "run: ended by an error (RuntimeError)" in record["result"]["reasons"]
    assert "synthetic failure" not in json.dumps(record)
    # Saved before the first session and after it; the second session never ended.
    assert [entry["stt"] and len(entry["stt"]["sessions"]) for entry in saved] == [None, 1]
    assert len(record["stt"]["sessions"]) == 1 and len(script.calls) == 2
    for entry in saved:
        assert entry["run"]["ended_by"] is None
        status, detail = record_mod.check_record(entry)
        assert status == "failed" and "run: did not end" in detail, (status, detail)

    # A record on disk is replaced whole: the new one is written beside it and renamed over
    # it, so a reader sees the record before or the record after.
    out, renames, rename = harness.work / "replaced.json", [], os.replace
    out.write_text("{}", encoding="utf-8")

    def renaming(source, target) -> None:
        renames.append((Path(source).name, json.loads(Path(source).read_text()), out.read_text()))
        rename(source, target)

    os.replace = renaming
    try:
        client_mod.write_record(out, record)
    finally:
        os.replace = rename
    assert renames == [("replaced.json.partial", record, "{}")], renames
    assert json.loads(out.read_text(encoding="utf-8")) == record

    interrupt = {
        "ended_by": "interrupt",
        "on_finalize": lambda _: signal.raise_signal(signal.SIGINT),
    }
    record, session, script = scripted(harness, "tts", **interrupt)
    assert record["run"] == {"ended_by": "interrupt", "error": None}, record["run"]
    assert session is None and record["result"]["reasons"][0] == "run: interrupted"


def check_arrival_order(harness: Harness) -> None:
    """An event read before the client sent what it would answer is not that answer."""
    pb = harness.bindings.pb
    for mode, sent, handler, event in (
        ("stt", "first_audio_sent", "_on_partial_transcript", pb.PartialTranscript(text="s")),
        ("tts", "text_segment_sent", "_on_audio_chunk", pb.AudioChunk(segment_id=1)),
    ):
        call = object.__new__(client_mod.Call)
        call.session = record_mod.new_session(mode, 0, 0)
        call.session["boundaries"][sent] = 50
        call.session["client"]["segment_id"] = 1
        call.session["server"]["output_format"] = {"encoding": "pcm_s16le"}
        call.ready, call._digest = pb.Ready(), hashlib.sha256()
        try:
            getattr(call, handler)(49, event)
        except client_mod._Stop as stop:
            assert stop.args == ("unexpected_event",), stop
        else:
            raise AssertionError(f"{handler} took an event read before {sent}")
    call.session = record_mod.new_session("stt", 0, 0)
    call.session["boundaries"]["first_audio_sent"] = 50
    call._on_partial_transcript(50, pb.PartialTranscript(text="s"))
    assert call.session["boundaries"]["first_partial"] == 50


def smi(work: Path, name: str, later: str) -> str:
    """A fake nvidia-smi that replays the capture once and runs `later` each time after."""
    program, mark = work / name, work / f"{name}-ran"
    script = (
        f'#!/bin/sh\nif [ -e "{mark}" ]; then\n{later}\nfi\n: > "{mark}"\ncat "{GPU_CAPTURE}"\n'
    )
    program.write_text(script)
    program.chmod(0o755)
    return str(program)


def check_sampler(work: Path, silent: str) -> None:
    """The sampling process: a query that fails is counted, and nothing outlives the run."""
    count = work / "queries"
    flaky = smi(
        work,
        "flaky-smi",
        f'echo x >> "{count}"\n'
        f'case $(wc -l < "{count}" | tr -d " ") in\n'
        f'1) head -1 "{GPU_CAPTURE}"; echo "0, NVIDIA L4"; exit 0;;\n'
        "2) exit 9;;\nesac",
    )
    printed = io.StringIO()
    with contextlib.redirect_stdout(printed):
        assert client_mod.sample_gpu(flaky, 1, iter([True] * 4 + [False]).__next__) == 0
        assert client_mod.sample_gpu(silent, 1, lambda: True) == 1
    lines = [json.loads(line) for line in printed.getvalue().splitlines()]
    assert ["failed" in line for line in lines] == [False, True, True, False], lines
    sampler = client_mod.GpuSampler(client_mod.Run(), False, 20)
    gpu = sampler.section(None, lines, False)
    assert (gpu["status"], gpu["failed_queries"], len(gpu["samples"])) == ("sampled", 2, 2), gpu
    assert gpu["devices"] == [{"index": 0, "name": "NVIDIA L4"}] and gpu["machine"] == "client_only"

    # An interrupt meant for the run does not end the sampling process: it ignores SIGINT,
    # which the query it starts inherits and reports here.
    inherited, reporting = work / "sigint", work / "reporting-smi"
    reporting.write_text(
        f"#!{sys.executable}\nimport signal\n"
        f"ignored = signal.getsignal(signal.SIGINT) == signal.SIG_IGN\n"
        f"open({str(inherited)!r}, 'w').write(str(ignored))\n"
        f"print(open({str(GPU_CAPTURE)!r}).read())\n"
    )
    reporting.chmod(0o755)
    sampler = client_mod.GpuSampler(client_mod.Run(), True, 1000, str(reporting))
    assert sampler.finish(True)["status"] == "sampled" and inherited.read_text() == "True"

    # A sampling process that died says nothing was sampled, whatever it read before.
    sampler = client_mod.GpuSampler(
        client_mod.Run(), True, 1, smi(work, "fatal-smi", "kill -9 $PPID")
    )
    sampler._process.wait()
    gpu = sampler.finish(True)
    assert (gpu["status"], gpu["samples"], gpu["machine"]) == ("unavailable", [], None), gpu
    assert gpu["reason"] == "the sampling process ended before the run did", gpu
    record = synthetic_record()
    record["gpu"] = gpu
    assert record_mod.check_record(record_mod.finalize(record))[0] == "complete"

    # A query in flight when the run ends is ended with it. It names itself through a pipe.
    pipe = work / "query-pid"
    os.mkfifo(pipe)
    hanging = smi(work, "hanging-smi", f'echo $$ > "{pipe}"\nexec sleep 30')
    sampler = client_mod.GpuSampler(client_mod.Run(), True, 1, hanging)
    query = int(pipe.read_text())
    try:
        assert sampler.finish(True)["status"] == "sampled"
        try:
            os.kill(query, 0)
        except ProcessLookupError:
            return
        raise AssertionError("a query in flight outlived the run")
    finally:
        with contextlib.suppress(ProcessLookupError):
            os.kill(query, signal.SIGKILL)


def check_inputs_and_gpu(harness: Harness) -> None:
    work = harness.work

    def refused(action, needle: str) -> None:
        try:
            action()
        except (client_mod.MeasureError, ValueError) as exc:
            assert needle in str(exc), (needle, str(exc))
            return
        raise AssertionError(f"not refused: expected {needle!r}")

    refused(lambda: client_mod.load_audio(work / "absent.wav", 20), "unreadable WAV file")
    for name, channels, rate, needle in (
        ("stereo", 2, 16000, "not a mono 16-bit PCM WAV file"),
        ("odd", 1, 11025, "need audio in whole 20 ms frames"),
    ):
        unusable = write_wav(work / f"{name}.wav", channels, rate, 640)
        refused(lambda path=unusable: client_mod.load_audio(path, 20), needle)
    refused(lambda: client_mod.load_audio(write_wav(work / "empty.wav", 1, 16000, 0), 20), "need")
    for spec in ("d:0:" + TARGET[2], "d:x:" + TARGET[2], "d:7", "d:7:sha256:00", ""):
        refused(lambda spec=spec: client_mod.Target.parse("localhost:1", spec), "target")
    target = client_mod.Target.parse("[::1]:7", "d:7:" + TARGET[2])
    assert target == ("[::1]:7", "d", 7, TARGET[2]) and target.is_loopback()
    assert client_mod.Target.parse("[::1]:7", "d") == ("[::1]:7", "d", None, None)
    assert client_mod.Target("localhost:7", *TARGET).is_loopback()
    assert not client_mod.Target("192.0.2.1:7", *TARGET).is_loopback()
    assert not client_mod.Target("speech.example:7", *TARGET).is_loopback()

    # The recorded capture of an L4 names the sampler's four columns as the sampler reads them.
    capture = GPU_CAPTURE.read_text(encoding="utf-8")
    reading = {
        "name": "NVIDIA L4",
        "index": 0,
        "utilization_percent": 0,
        "memory_used_mib": 256,
        "memory_total_mib": 23034,
    }
    assert client_mod.parse_gpu_csv(capture) == [reading]
    for cell in ("[N/A]", "", "MiB"):
        not_reported = capture.replace("256 MiB", cell)
        assert client_mod.parse_gpu_csv(not_reported) == [{**reading, "memory_used_mib": None}]
    refused(lambda: client_mod.parse_gpu_csv(capture.replace("NVIDIA L4", "")), "index or name")
    refused(lambda: client_mod.parse_gpu_csv(""), "no reading")
    refused(lambda: client_mod.parse_gpu_csv(capture.splitlines()[0]), "no reading")
    refused(
        lambda: client_mod.parse_gpu_csv(capture.replace("memory.used", "memory.spent")),
        "no reading",
    )
    refused(lambda: client_mod.parse_gpu_csv(capture.replace("\n0, ", "\n[N/A], ")), "device index")

    fake = work / "bin" / "nvidia-smi"
    fake.parent.mkdir()
    fake.write_text(f'#!/bin/sh\necho "$@" >> "{work}/gpu-arguments"\ncat "{GPU_CAPTURE}"\n')
    fake.chmod(0o755)
    record, _, _ = harness.run(
        "tts", gpu_sampling="auto", gpu_interval_ms=20, gpu_program=str(fake)
    )
    gpu = record["gpu"]
    assert gpu["status"] == "sampled" and gpu["interval_ms"] == 20 and gpu["reason"] is None, gpu
    assert gpu["summary"]["max_memory_used_mib"] == 256 and gpu["summary"]["samples"] >= 1
    # On loopback the readings are of the machine that serves, by default.
    assert (gpu["machine"], gpu["failed_queries"]) == ("serving", 0), gpu
    assert gpu["devices"] == [{"index": 0, "name": "NVIDIA L4"}] and "name" not in gpu["samples"][0]
    # The first reading precedes the first session, on the run's own clock.
    assert gpu["samples"][0]["at_ns"] <= record["tts"]["sessions"][0]["boundaries"]["call_start"]
    asked = set((work / "gpu-arguments").read_text().splitlines())
    assert asked == {" ".join(client_mod.GPU_QUERY)}, asked
    # Away from loopback they are the client machine's: taken only when asked for.
    elsewhere = {"gpu_program": str(fake), "gpu_interval_ms": 20}
    gpu = scripted(harness, "tts", settings={**elsewhere, "gpu_sampling": "auto"})[0]["gpu"]
    assert (gpu["status"], gpu["machine"], gpu["samples"]) == ("not_requested", None, []), gpu
    gpu = scripted(harness, "tts", settings={**elsewhere, "gpu_sampling": "on"})[0]["gpu"]
    assert (gpu["status"], gpu["machine"]) == ("sampled", "client_only"), gpu
    silent = work / "silent-smi"
    silent.write_text("#!/bin/sh\nexit 9\n")
    silent.chmod(0o755)
    for program, reason in (
        (str(silent), "gave no reading"),
        (str(work / "absent"), "no nvidia-smi"),
    ):
        gpu = harness.run("tts", gpu_sampling="auto", gpu_program=program)[0]["gpu"]
        assert gpu["status"] == "unavailable" and reason in gpu["reason"] and not gpu["samples"], (
            gpu
        )
    assert harness.run("tts")[0]["gpu"]["status"] == "not_requested"
    check_sampler(work, str(silent))

    # A target nothing listens on: the record says the channel never became ready.
    target = client_mod.Target("127.0.0.1:1", *TARGET)
    record = client_mod.measure(
        harness.bindings, stt=(target, harness.audio), tts=None, event_timeout_ms=300,
        gpu_sampling="off",
    )  # fmt: skip
    assert record["stt"]["target"]["connect_ns"] is None and not record["stt"]["sessions"]
    assert record_mod.check_record(record) == (
        "failed",
        "stt: the channel never became ready; tts: not run",
    )


def check_command(harness: Harness) -> None:
    """The command end to end: both modes, the GPU section, the record on disk and its check."""
    work = harness.work
    text = work / "segment.txt"
    text.write_text(TEXT + "\n", encoding="utf-8")
    out = work / "slice-record.json"
    empty, long = work / "empty.txt", work / "long.txt"
    empty.write_text("\n", encoding="utf-8")
    long.write_text("s" * 4097, encoding="utf-8")
    servers = [harness.serve(SliceServer(harness.bindings)) for _ in range(2)]
    spec = ":".join(str(part) for part in TARGET)
    stt = [
        "--stt-endpoint", servers[0][1], "--stt-target", spec, "--stt-language", "en",
        "--audio", str(harness.wav),
    ]  # fmt: skip
    command = [
        sys.executable, str(CLIENT_TOOL), "run", "--out", str(out),
        "--iterations", "2", "--gpu-interval-ms", "20", *stt,
        "--tts-endpoint", servers[1][1], "--tts-target", spec, "--tts-language", "en",
        "--text-file", str(text),
        "--voice", VOICE,
    ]  # fmt: skip
    # The command runs as an operator runs it, without this process's gRPC setting.
    environment = {**os.environ, "PATH": f"{work / 'bin'}{os.pathsep}{os.environ['PATH']}"}
    del environment["GRPC_ENABLE_FORK_SUPPORT"]
    try:
        done = subprocess.run(command, capture_output=True, text=True, check=False, env=environment)
        assert done.returncode == 0 and "complete: every session completed" in done.stdout, done
        record = json.loads(out.read_text(encoding="utf-8"))
        assert record["result"] == {"status": "complete", "reasons": []}
        # Both targets are loopback, so the GPU is sampled; no commit, so nothing is `recorded`.
        assert record["gpu"]["status"] == "sampled" and record["provenance"] == "synthetic"
        assert record["run"] == {"ended_by": "completed", "error": None}
        assert record["worker"] == {"source": "not_stated", "version": None, "build": None}
        assert set(record["client_platform"]) == {
            "system", "release", "machine", "cpu_count", "cpu_model",
        }  # fmt: skip
        assert not [path for path in work.iterdir() if path.name.endswith(".partial")]
        assert [len(record[mode]["sessions"]) for mode in ("stt", "tts")] == [2, 2]
        assert record["tool"]["stream_schema_sha256"] == harness.bindings.schema_digest
        jsonschema.validate(record, json.loads(SCHEMA.read_text(encoding="utf-8")))
        assert run_check(record, work).returncode == 0
        for extra, needle in (
            (["--iterations", "0"], "at least 1"),
            (["--source-commit", "abc"], "not a full commit id"),
            (["--stt-target", "d:7"], "is not DEPLOYMENT or DEPLOYMENT:GENERATION:DIGEST"),
            (["--audio", str(text)], "unreadable WAV file"),
            (["--provenance", "recorded"], "--provenance recorded needs --source-commit"),
            (
                ["--provenance", "recorded", "--source-commit", "0" * 40],
                "--provenance recorded needs --source-commit and --worker-version",
            ),
            (["--worker-version", "synthetic/path"], "--worker-version does not match"),
            (["--worker-build", "a b"], "--worker-build does not match"),
            (["--voice", "\u00e9"], "--voice does not match"),
            (["--tts-language", "e\tn"], "--tts-language does not match"),
            (["--out", str(work / "absent" / "record.json")], "No such file or directory"),
            (["--frame-ms", "10"], "a frame is 20 to 320 ms"),
            (["--text-file", str(harness.wav)], "codec can't decode"),
            (["--text-file", str(empty)], "segment text of 1 to 4,096 bytes"),
            (["--text-file", str(long)], "segment text of 1 to 4,096 bytes"),
            (["--frame-ms", "400"], "a frame is 20 to 320 ms"),
            (["--stt-language", ""], "--stt-endpoint needs --stt-target, --stt-language"),
            (["--voice", ""], "--tts-endpoint needs --tts-target, --tts-language"),
        ):
            done = subprocess.run([*command, *extra], capture_output=True, text=True, check=False)
            assert done.returncode == 2 and needle in done.stderr, done
        done = subprocess.run(command[:5], capture_output=True, text=True, check=False)
        assert done.returncode == 2 and "nothing to measure" in done.stderr, done

        # A commit makes the run `recorded`; the worker's release is what the operator states;
        # a target given by its deployment alone has no generation and no digest.
        stated = [
            "--iterations", "1", "--gpu-sampling", "off", "--source-commit", "0" * 40,
            "--worker-version", "0.3.1", "--worker-build", "b1", "--tts-target", TARGET[0],
        ]  # fmt: skip
        done = subprocess.run([*command, *stated], capture_output=True, text=True, check=False)
        assert done.returncode == 0, done
        record = json.loads(out.read_text(encoding="utf-8"))
        assert record["provenance"] == "recorded" and record["gpu"]["status"] == "not_requested"
        assert record["worker"] == {"source": "operator_stated", "version": "0.3.1", "build": "b1"}
        assert record["stt"]["target"]["generation"] == 7
        assert record["tts"]["target"] | {"connect_ns": 0} == {
            "generation": None,
            "descriptor_digest": None,
            "loopback": True,
            "connect_ns": 0,
        }

        # One target of two away from loopback, and no --gpu-sampling: nothing is sampled,
        # though the fake nvidia-smi is there. Neither channel is given time to become ready.
        mixed = [
            "--event-timeout-ms", "1", "--stt-endpoint", "127.0.0.1:1",
            "--tts-endpoint", "192.0.2.1:7",
        ]  # fmt: skip
        done = subprocess.run(
            [*command, *mixed], capture_output=True, text=True, check=False, env=environment
        )
        assert done.returncode == 1 and "the channel never became ready" in done.stderr, done
        record = json.loads(out.read_text(encoding="utf-8"))
        assert [record[mode]["target"]["loopback"] for mode in ("stt", "tts")] == [True, False]
        assert record["gpu"]["status"] == "not_requested", record["gpu"]

        # An interrupt in the second session: the record on disk holds the first and says so.
        # The server reads what was on disk before it sends the signal.
        servicer = SliceServer(harness.bindings, "interrupt")
        servers.append(harness.serve(servicer))
        interrupted = [*command[:5], "--iterations", "2", "--gpu-sampling", "off", *stt]
        interrupted[4] = str(work / "interrupted.json")
        interrupted[interrupted.index("--stt-endpoint") + 1] = servers[-1][1]
        servicer.out = Path(interrupted[4])
        servicer.client = subprocess.Popen(
            interrupted, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=environment
        )
        servicer.armed.set()
        _, stderr = servicer.client.communicate()
        assert servicer.client.returncode == 1 and "failed: run: interrupted" in stderr, stderr
        record = json.loads(servicer.out.read_text(encoding="utf-8"))
        assert record["run"] == {"ended_by": "interrupt", "error": None}, record["run"]
        before = servicer.seen["checkpoint"]
        assert before["run"]["ended_by"] is None and record_mod.check_record(before)[0] == "failed"
        for entry in (before, record):
            sessions = entry["stt"]["sessions"]
            assert [session["outcome"] for session in sessions] == ["completed"], sessions
    finally:
        for server, _ in servers:
            server.stop(0)


def main() -> int:
    # A shell that started this in the background left SIGINT ignored; the interrupt cases send it.
    signal.signal(signal.SIGINT, signal.default_int_handler)
    check_nearest_rank()
    check_stage_list()
    with tempfile.TemporaryDirectory() as raw_tmp:
        work = Path(raw_tmp)
        good = synthetic_record()
        check_good_record(good, work)
        check_rejections(good, work)
        check_samples(good)
        generated = work / "generated"
        generated.mkdir()
        bindings = client_mod.load_bindings(generated)
        check_bindings(bindings)
        harness = Harness(bindings, work)
        check_stamps(harness)
        check_schedule(harness)
        check_run_endings(harness)
        check_sessions(harness)
        check_faults(harness)
        check_arrival_order(harness)
        check_inputs_and_gpu(harness)
        check_command(harness)
    print("slice measurement: the record and the client check, measure and fail closed as expected")
    return 0


if __name__ == "__main__":
    sys.exit(main())
