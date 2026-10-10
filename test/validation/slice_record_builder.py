# SPDX-License-Identifier: Apache-2.0
"""A streaming slice measurement record for tests that run without a gRPC package.

No streaming run has been recorded on a machine, so every number here is made up
and the record carries only what the checker accepts. A test that asks for
`provenance="recorded"` stands in for the client that would write it.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/validation"))
import slice_measurement_record as record_mod  # noqa: E402

MS = 1_000_000
DIGEST = "sha256:" + "0" * 64
FRAMES, FRAME_MS, RATE = 10, 20, 16000
SAMPLES = FRAMES * FRAME_MS * RATE // 1000
SEGMENT_LAG = MS // 2
STT_FINAL_AFTER_MS = (300, 100, 200)
TTS_FIRST_AUDIO_AFTER_MS = (90, 70, 80)
TOTALS = {
    "input_audio_samples": 0,
    "input_text_bytes": 0,
    "output_audio_samples": 0,
    "utterances_completed": 0,
    "segments_completed": 0,
}


def closed(**totals: int) -> dict[str, Any]:
    return {
        "state": "closed",
        "reason": None,
        "code": None,
        "admission": "admitted",
        "last_accepted_sequence": 2,
        "totals": {**TOTALS, **totals},
    }


def stt_session(index: int, start: int, final_after_ms: int) -> dict[str, Any]:
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


def tts_session(index: int, start: int, first_audio_after_ms: int) -> dict[str, Any]:
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


def build_record(
    commit: str | None,
    provenance: str = "recorded",
    modes: tuple[str, ...] = ("stt", "tts"),
    stt_final_after_ms: tuple[int, ...] = STT_FINAL_AFTER_MS,
    tts_first_audio_after_ms: tuple[int, ...] = TTS_FIRST_AUDIO_AFTER_MS,
) -> dict[str, Any]:
    """Three sessions per mode in `modes`; a `recorded` record names `commit` and a worker build."""
    target = {"generation": 7, "descriptor_digest": DIGEST, "loopback": True, "connect_ns": 5 * MS}
    stated = provenance == "recorded"
    record: dict[str, Any] = {
        "schema_version": "0.1",
        "record_kind": "slice_measurement",
        "provenance": provenance,
        "tool": {
            "name": "slice-measure",
            "source_commit": commit,
            "python": "3.12.4",
            "grpcio": "1.81.1",
            "protobuf": "6.33.5",
            "generator": "1.81.1",
            "stream_schema_sha256": DIGEST,
        },
        "worker": {
            "source": "operator_stated" if stated else "not_stated",
            "version": "stub-build" if stated else None,
            "build": None,
        },
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
        "stt": None,
        "tts": None,
        "gpu": {
            "status": "not_requested",
            "reason": None,
            "scope": "device_wide",
            "machine": None,
            "interval_ms": None,
            "failed_queries": 0,
            "devices": [],
            "samples": [],
            "summary": None,
        },
        "run": {"ended_by": "completed", "error": None},
        "result": {},
    }
    if "stt" in modes:
        record["stt"] = {
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
                stt_session(n, (n + 1) * 1000 * MS, after)
                for n, after in enumerate(stt_final_after_ms)
            ],
            "summary": {},
        }
    if "tts" in modes:
        record["tts"] = {
            "target": dict(target),
            "input": {
                "text_sha256": DIGEST,
                "text_bytes": 22,
                "language": "en",
                "voice": "voice-a",
            },
            "sessions": [
                tts_session(n, (n + 10) * 1000 * MS, after)
                for n, after in enumerate(tts_first_audio_after_ms)
            ],
            "summary": {},
        }
    return record_mod.finalize(record)


def delay_input(record: dict[str, Any], mode: str, lag_ns: int) -> dict[str, Any]:
    """Every session of `mode` sends its timed input `lag_ns` after it was due."""
    chain = record_mod.CHAIN[mode]
    for session in record[mode]["sessions"]:
        client = session["client"]
        shift = lag_ns - client["input_lag_ns"]
        for name in (*chain[chain.index(record_mod.INPUT[mode][1]) :], "session_closed"):
            session["boundaries"][name] += shift
        client["input_lag_ns"] = lag_ns
    return record_mod.finalize(record)


def stop_session(record: dict[str, Any], mode: str, index: int) -> dict[str, Any]:
    """The client ended one session itself, on an event its state does not allow."""
    record[mode]["sessions"][index].update(outcome="stopped", stop_reason="unexpected_event")
    return record_mod.finalize(record)
