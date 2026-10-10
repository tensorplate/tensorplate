#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check the streaming slice measurement record, then break each rule once.

The record is synthetic: its timestamps are written by hand, so every
derived value has a known answer. Each rule of the check is then broken
once, and the refusal has to name that rule.
"""

from __future__ import annotations

import copy
import json
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RECORD_TOOL = ROOT / "tools/validation/slice_measurement_record.py"
SCHEMA = ROOT / "config/schemas/slice_measurement_record.json"
sys.path.insert(0, str(ROOT / "tools/validation"))
import jsonschema  # noqa: E402
import slice_measurement_record as record_mod  # noqa: E402

MS = 1_000_000
DIGEST = "sha256:" + "0" * 64
FRAMES, FRAME_MS, RATE = 10, 20, 16000
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
    last_audio = start + 3 * MS + (FRAMES - 1) * FRAME_MS * MS + MS
    final = last_audio + final_after_ms * MS
    session["boundaries"].update(
        ready=start + 2 * MS,
        first_audio_sent=start + 3 * MS,
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
        text_segment_sent=start + 3 * MS,
        first_audio_chunk=first_audio,
        segment_completed=first_audio + MS,
        finalize_sent=first_audio + 2 * MS,
        synthesis_completed=first_audio + 3 * MS,
        session_closed=first_audio + 4 * MS,
    )
    session["client"].update(events_sent=3, segment_id=1, finalize_sequence=3)
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
        "recorded_at_utc": "2026-01-01T00:00:00Z",
        "clock": {"source": "caller_monotonic", "origin": "run_start", "resolution_ns": 1},
        "percentile_method": "nearest_rank",
        "settings": {"iterations": 3, "event_timeout_ms": 30000},
        "stt": {
            "target": dict(target),
            "input": {
                "audio_sha256": DIGEST,
                "audio_bytes": SAMPLES * 2,
                "samples": SAMPLES,
                "format": {"encoding": "pcm_s16le", "sample_rate_hz": RATE, "channels": 1},
                "frame_ms": FRAME_MS,
                "frames": FRAMES,
                "language_sha256": DIGEST,
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
                "language_sha256": DIGEST,
                "voice_sha256": DIGEST,
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
            "interval_ms": 1000,
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
    assert first["last_audio_to_final_transcript"]["ns"] == 300 * MS
    assert first["call_start_to_first_transcript"] == {
        "from": "call_start",
        "to": "final_transcript",
        "ended_by": "final_transcript",
        "ns": (3 + 9 * 20 + 1 + 300) * MS,
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
    assert tts["sessions"][0]["measurements"]["segment_to_first_audio"]["ended_by"] == "audio_chunk"
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
    session["server"]["partials"] = 1
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
        session = record["tts"]["sessions"][2]
        session["boundaries"] = {**dict.fromkeys(session["boundaries"]), "call_start": 5000 * MS}
        session["server"].update(closed=None, events_received=0)
        session.update(outcome=outcome, grpc_status=status, stop_reason=stop_reason)

    return apply


def closed_before_start(record: dict) -> None:
    end_early("session_failed", "OK")(record)
    session = record["tts"]["sessions"][2]
    session["boundaries"]["session_closed"] = 4999 * MS
    session["server"]["closed"] = {**closed(), "state": "failed"}


def no_gpu(status: str, reason: str | None, interval: int | None):
    def apply(record: dict) -> None:
        record["gpu"].update(status=status, reason=reason, interval_ms=interval, samples=[])
        record["gpu"]["summary"] = None

    return apply


def no_channel(record: dict) -> None:
    record["tts"]["target"]["connect_ns"] = None
    record["tts"]["sessions"] = []


S0, T0 = "stt.sessions.0", "tts.sessions.0"

# Shapes the schema refuses: text, audio, names and addresses have no field,
# and a server stage cannot carry a duration.
SHAPE_CASES = (
    (f"{S0}.server.final.text", "synthetic words"),
    (f"{S0}.audio", "AAAA"),
    ("tts.input.voice", "synthetic_voice"),
    ("tts.input.voice_sha256", "synthetic_voice"),
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
    (lambda r: r["stt"]["sessions"].pop(), "stt: 2 sessions recorded, 3 expected"),
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
    (put(f"{S0}.boundaries.first_audio_sent", 15 * MS), "sent faster than real time"),
    (put(f"{S0}.client.max_pacing_lag_ns", MS - 1), "is below the last frame's lag"),
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


def main() -> int:
    check_nearest_rank()
    check_stage_list()
    with tempfile.TemporaryDirectory() as raw_tmp:
        work = Path(raw_tmp)
        good = synthetic_record()
        check_good_record(good, work)
        check_rejections(good, work)
    print("slice measurement: the record checks, derives and fails closed as expected")
    return 0


if __name__ == "__main__":
    sys.exit(main())
