#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The streaming slice measurement record: its derived fields and the check.

The record's shape is `config/schemas/slice_measurement_record.json`. What a
run observes is timestamps and counts; every duration, distribution and the
result are derived from them here, and the check recomputes each one and
compares it with what the record claims, so a record cannot say more than
its timestamps support. Percentiles are nearest rank on the raw samples.

Exit status of `check`: 0 complete, 1 failed (a session did not complete or
an answer contradicts what was sent), 2 no verdict (the record is incomplete
or cannot be trusted).
"""

from __future__ import annotations

import argparse
import itertools
import json
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
SCHEMA_PATH = REPO_ROOT / "config/schemas/slice_measurement_record.json"
FAILURE_REASON_SCHEMA_PATH = REPO_ROOT / "protocol/schemas/failure_reason.json"
ERROR_SCHEMA_PATH = REPO_ROOT / "protocol/schemas/error.json"

sys.path.insert(0, str(HERE))
from candidate_record import RecordError, validate_record  # noqa: E402

TOOL_NAME = "slice-measure"
RECORD_KIND = "slice_measurement"
SCHEMA_VERSION = "0.3"
MODES = ("stt", "tts")

# The registered values of the `stage` metric label, in pipeline order.
SERVER_STAGES = ("ingress", "queue", "vad", "preprocessing", "backend", "postprocessing", "egress")

GRPC_STATUS_NAMES = (
    "OK",
    "CANCELLED",
    "UNKNOWN",
    "INVALID_ARGUMENT",
    "DEADLINE_EXCEEDED",
    "NOT_FOUND",
    "ALREADY_EXISTS",
    "PERMISSION_DENIED",
    "RESOURCE_EXHAUSTED",
    "FAILED_PRECONDITION",
    "ABORTED",
    "OUT_OF_RANGE",
    "UNIMPLEMENTED",
    "INTERNAL",
    "UNAVAILABLE",
    "DATA_LOSS",
    "UNAUTHENTICATED",
)

# Each mode's boundaries in the order a session reaches them. `session_closed`
# is apart: a session can be closed at any point.
CHAIN = {
    "stt": (
        "call_start",
        "ready",
        "first_audio_sent",
        "last_audio_scheduled",
        "last_audio_sent",
        "finalize_sent",
        "endpoint",
        "final_transcript",
    ),
    "tts": (
        "call_start",
        "ready",
        "text_segment_scheduled",
        "text_segment_sent",
        "first_audio_chunk",
        "segment_completed",
        "finalize_sent",
        "synthesis_completed",
    ),
}

# The boundaries stamped as an event is read. The others are stamped as one is
# sent, which can be after a SessionClosed the client had not handled yet.
RECEIVED = (
    "ready",
    "first_partial",
    "endpoint",
    "final_transcript",
    "first_audio_chunk",
    "segment_completed",
    "synthesis_completed",
)

# The input a mode's answer is timed from: when it was due, and when it was sent.
INPUT = {
    "stt": ("last_audio_scheduled", "last_audio_sent"),
    "tts": ("text_segment_scheduled", "text_segment_sent"),
}

# A session whose timed input was sent later than this after it was due gives
# no sample for a measurement that starts there.
MAX_INPUT_LAG_MS = 100

# name -> (from, to, the server event that ends it)
MEASUREMENTS = {
    "stt": {
        "session_initialization": ("call_start", "ready", "ready"),
        "call_start_to_first_transcript": ("call_start", "final_transcript", "final_transcript"),
        "last_audio_to_final_transcript": (
            "last_audio_scheduled",
            "final_transcript",
            "final_transcript",
        ),
    },
    "tts": {
        "session_initialization": ("call_start", "ready", "ready"),
        "call_start_to_first_audio": ("call_start", "first_audio_chunk", "audio_chunk"),
        "segment_to_first_audio": ("text_segment_scheduled", "first_audio_chunk", "audio_chunk"),
        "segment_to_completion": (
            "text_segment_scheduled",
            "segment_completed",
            "segment_completed",
        ),
    },
}

# The answers a completed session holds, each a single server event.
ANSWERS = {"stt": ("endpoint", "final"), "tts": ("segment", "synthesis")}

BYTES_PER_SAMPLE = {"pcm_s16le": 2, "mulaw": 1}

EXIT_COMPLETE = 0
EXIT_FAILED = 1
EXIT_NO_VERDICT = 2


def load_schema(path: Path = SCHEMA_PATH) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def _enum(path: Path, field: str) -> frozenset[str]:
    return frozenset(json.loads(path.read_text(encoding="utf-8"))["properties"][field]["enum"])


def server_stages() -> list[dict[str, Any]]:
    return [{"stage": stage, "status": "not_measured", "ns": None} for stage in SERVER_STAGES]


def new_session(mode: str, index: int, call_start_ns: int) -> dict[str, Any]:
    """A session that has reached nothing but its call start."""
    boundaries: dict[str, Any] = {**dict.fromkeys(CHAIN[mode]), "call_start": call_start_ns}
    if mode == "stt":
        boundaries["first_partial"] = None
        client: dict[str, Any] = {
            "events_sent": 0,
            "frames_sent": 0,
            "samples_sent": 0,
            "bytes_sent": 0,
            "pings_sent": 0,
            "credit_waits": 0,
            "finalize_sequence": None,
            "max_pacing_lag_ns": None,
            "input_lag_ns": None,
        }
        server: dict[str, Any] = {"partials": 0, "limits": None, "endpoint": None, "final": None}
    else:
        client = {
            "events_sent": 0,
            "pings_sent": 0,
            "credit_waits": 0,
            "segment_id": None,
            "finalize_sequence": None,
            "input_lag_ns": None,
        }
        server = {
            "limits": None,
            "output_format": None,
            "chunks": 0,
            "audio_bytes": 0,
            "audio_samples": 0,
            "audio_sha256": None,
            "segment": None,
            "synthesis": None,
        }
    boundaries["session_closed"] = None
    server.update({"events_received": 0, "ignored_events": 0, "accepted_events": 0, "closed": None})
    session = {
        "index": index,
        "outcome": "timeout",
        "stop_reason": None,
        "grpc_status": None,
        "open_refusal": None,
        "boundaries": boundaries,
        "measurements": {},
        "client": client,
        "server": server,
        "server_stages": server_stages(),
    }
    if mode == "stt":
        session["utterances"] = []
        client["audio_frames"] = []
    session["measurements"] = derive_measurements(mode, session)
    return session


def new_utterance(utterance_id: int, start: int) -> dict[str, Any]:
    return {
        "utterance_id": utterance_id,
        "start_sample_offset": start,
        "endpoint": None,
        "final": None,
        "partials": [],
        "boundaries": dict.fromkeys(
            (
                "first_audio_sent",
                "last_audio_sent",
                "first_partial",
                "endpoint",
                "final_transcript",
            )
        ),
        "measurements": {},
        "cadence": {},
    }


def utterance_audio(session: dict[str, Any], utterance: dict[str, Any]) -> list[dict[str, Any]]:
    """Frames overlapping the utterance's half-open input interval, even at a split frame."""
    endpoint = utterance["endpoint"]
    end = endpoint["end_sample_offset"] if endpoint else session["client"]["samples_sent"]
    start = utterance["start_sample_offset"]
    return [
        frame
        for frame in session["client"]["audio_frames"]
        if start < end and frame["end_sample_offset"] > start and frame["start_sample_offset"] < end
    ]


def derive_utterance_boundaries(
    session: dict[str, Any], utterance: dict[str, Any]
) -> dict[str, Any]:
    frames = utterance_audio(session, utterance)
    return {
        **utterance["boundaries"],
        "first_audio_sent": frames[0]["sent_ns"] if frames else None,
        "last_audio_sent": frames[-1]["sent_ns"] if frames and utterance["endpoint"] else None,
        "first_partial": utterance["partials"][0]["at_ns"] if utterance["partials"] else None,
    }


def derive_utterance_measurements(utterance: dict[str, Any]) -> dict[str, Any]:
    boundaries = utterance["boundaries"]
    result = {}
    for name, start, end, event in (
        ("endpoint_to_final", "endpoint", "final_transcript", "final_transcript"),
        ("last_audio_to_endpoint", "last_audio_sent", "endpoint", "endpoint_detected"),
        (
            "last_audio_to_final",
            "last_audio_sent",
            "final_transcript",
            "final_transcript",
        ),
        (
            "first_audio_to_first_partial",
            "first_audio_sent",
            "first_partial",
            "partial_transcript",
        ),
    ):
        a, b = boundaries[start], boundaries[end]
        result[name] = {
            "from": start,
            "to": end,
            "ended_by": event,
            "ns": None if a is None or b is None else b - a,
        }
    return result


def derive_cadence(utterance: dict[str, Any]) -> dict[str, Any]:
    partials = utterance["partials"]
    revisions = [0, *(p["revision"] for p in partials)]
    return {
        "partial_count": len(partials),
        "revision_gaps": [max(0, b - a - 1) for a, b in itertools.pairwise(revisions)],
        "intervals_ns": [b["at_ns"] - a["at_ns"] for a, b in itertools.pairwise(partials)],
    }


def derive_measurements(mode: str, session: dict[str, Any]) -> dict[str, Any]:
    boundaries = session["boundaries"]
    due, sent = (boundaries[name] for name in INPUT[mode])
    late = sent is not None and sent - due > MAX_INPUT_LAG_MS * 1_000_000
    derived = {}
    for name, (start, end, event) in MEASUREMENTS[mode].items():
        valid = not (late and start == INPUT[mode][0])
        if name == "call_start_to_first_transcript":
            if boundaries["first_partial"] is not None:
                end, event = "first_partial", "partial_transcript"
            else:
                final = session["server"]["final"]
                valid = final is not None and final["text_bytes"] > 0
        if name == "call_start_to_first_transcript" and "utterances" in session:
            candidates = []
            for index, utterance in enumerate(session["utterances"]):
                prefix = f"utterances/{index}"
                for position, partial in enumerate(utterance["partials"]):
                    if partial["text_bytes"]:
                        candidates.append(
                            (
                                partial["at_ns"],
                                "first_partial",
                                f"{prefix}/partials/{position}/at_ns",
                                "partial_transcript",
                            )
                        )
                final = utterance["final"]
                if final and final["text_bytes"]:
                    candidates.append(
                        (
                            utterance["boundaries"]["final_transcript"],
                            "final_transcript",
                            f"{prefix}/boundaries/final_transcript",
                            "final_transcript",
                        )
                    )
            if candidates:
                at, key, path, event = min(candidates)
                # Keep the original boundary name when it already names this event.
                end = key if boundaries[key] == at else path
                derived[name] = {
                    "from": start,
                    "to": end,
                    "ended_by": event,
                    "ns": at - boundaries[start],
                }
                continue
            valid = False
        ns = None
        if valid and boundaries[start] is not None and boundaries[end] is not None:
            ns = boundaries[end] - boundaries[start]
        derived[name] = {"from": start, "to": end, "ended_by": event, "ns": ns}
    return derived


def nearest_rank(ordered: list[int], percent: int) -> int:
    """The sample of rank ceil(percent * n / 100), counted from 1."""
    return ordered[-(-percent * len(ordered) // 100) - 1]


def distribution(samples: list[int | None]) -> dict[str, Any]:
    ordered = sorted(sample for sample in samples if sample is not None)
    summary: dict[str, Any] = {"count": len(ordered), "misses": len(samples) - len(ordered)}
    for key, percent in (("p50_ns", 50), ("p95_ns", 95), ("p99_ns", 99)):
        summary[key] = nearest_rank(ordered, percent) if ordered else None
    summary["min_ns"] = ordered[0] if ordered else None
    summary["max_ns"] = ordered[-1] if ordered else None
    return summary


def derive_summary(mode: str, section: dict[str, Any]) -> dict[str, Any]:
    """Each measurement over the sessions that completed with no finding; the rest are misses."""
    samples: dict[str, list[int | None]] = {name: [] for name in MEASUREMENTS[mode]}
    for session in section["sessions"]:
        sound = session["outcome"] == "completed" and not session_findings(mode, section, session)
        for name, entry in derive_measurements(mode, session).items():
            samples[name].append(entry["ns"] if sound else None)
    return {name: distribution(values) for name, values in samples.items()}


def failure_cause(mode: str, section: dict[str, Any], session: dict[str, Any]) -> str | None:
    """A stable cause without server detail text or endpoint identity."""
    outcome = session["outcome"]
    if outcome == "completed":
        return "invalid_response" if session_findings(mode, section, session) else None
    if outcome == "stopped":
        return "stopped:" + session["stop_reason"]
    if outcome == "session_failed":
        closed = session["server"]["closed"]
        return (
            "session_failed:"
            + (closed["reason"] or closed["state"])
            + ":"
            + (closed["code"] or "none")
        )
    if outcome == "call_failed":
        return "grpc:" + session["grpc_status"]
    if "connection" in session and session["connection"]["connect_ns"] is None:
        return "channel_timeout"
    return "event_timeout"


def derive_aggregate(record: dict[str, Any]) -> dict[str, Any]:
    """Pool raw latency samples, never per-session percentiles."""
    recorded = opened = clean = 0
    failures: dict[str, int] = {}
    measurements = {}
    for mode in MODES:
        section = record[mode]
        if section is None:
            measurements[mode] = None
            continue
        samples: dict[str, list[int | None]] = {name: [] for name in MEASUREMENTS[mode]}
        if mode == "stt":
            samples.update(
                {name: [] for name in derive_utterance_measurements(new_utterance(1, 0))}
            )
            samples["partial_interval"] = []
        for session in section["sessions"]:
            recorded += 1
            opened += session["client"]["events_sent"] > 0
            cause = failure_cause(mode, section, session)
            if cause is None:
                clean += 1
            else:
                failures[cause] = failures.get(cause, 0) + 1
            for name, entry in derive_measurements(mode, session).items():
                samples[name].append(entry["ns"] if cause is None else None)
            for utterance in session.get("utterances", []):
                for name, entry in derive_utterance_measurements(utterance).items():
                    samples[name].append(entry["ns"] if cause is None else None)
                samples["partial_interval"].extend(
                    ns if cause is None else None
                    for ns in derive_cadence(utterance)["intervals_ns"]
                )
        measurements[mode] = {name: distribution(values) for name, values in samples.items()}
    return {
        "sessions_recorded": recorded,
        "sessions_opened": opened,
        "sessions_clean": clean,
        "sessions_failed": recorded - clean,
        "failed_by_cause": [
            {"cause": cause, "count": failures[cause]} for cause in sorted(failures)
        ],
        "wall_time_ns": record["run"]["finished_ns"],
        "measurements": measurements,
    }


def derive_gpu_summary(samples: list[dict[str, Any]]) -> dict[str, Any] | None:
    if not samples:
        return None

    def largest(key: str) -> int | None:
        readings = [sample[key] for sample in samples if sample[key] is not None]
        return max(readings) if readings else None

    return {
        "samples": len(samples),
        "max_utilization_percent": largest("utilization_percent"),
        "max_memory_used_mib": largest("memory_used_mib"),
    }


def session_findings(mode: str, section: dict[str, Any], session: dict[str, Any]) -> list[str]:
    """Where a completed session's answers contradict what the client sent."""
    client, server = session["client"], session["server"]
    totals = server["closed"]["totals"]
    found: list[str] = []

    def expect(what: str, observed: Any, expected: Any) -> None:
        if observed != expected:
            found.append(f"{what} is {observed!r}, expected {expected!r}")

    expect("admission", server["closed"]["admission"], "admitted")
    if mode == "stt":
        samples = section["input"]["samples"]
        expect("samples sent", client["samples_sent"], samples)
        expect("bytes sent", client["bytes_sent"], section["input"]["audio_bytes"])
        expect("endpoint reason", server["endpoint"]["reason"], "client_finalize")
        expect("endpoint end_sample_offset", server["endpoint"]["end_sample_offset"], samples)
        expect("final end_sample_offset", server["final"]["end_sample_offset"], samples)
        expect(
            "final finalize_sequence",
            server["final"]["finalize_sequence"],
            client["finalize_sequence"],
        )
        expect("total input_audio_samples", totals["input_audio_samples"], samples)
        if "utterances" in session:
            utterances = session["utterances"]
            expect(
                "total utterances_completed",
                totals["utterances_completed"],
                len(utterances),
            )
            for utterance in utterances:
                endpoint, final = utterance["endpoint"], utterance["final"]
                if endpoint is None or final is None:
                    found.append("utterance has no endpoint or final")
                    continue
                expect(
                    "utterance final end_sample_offset",
                    final["end_sample_offset"],
                    endpoint["end_sample_offset"],
                )
                sequence = (
                    client["finalize_sequence"] if endpoint["reason"] == "client_finalize" else 0
                )
                expect(
                    "utterance final finalize_sequence",
                    final["finalize_sequence"],
                    sequence,
                )
        elif totals["utterances_completed"] < 1:
            found.append("total utterances_completed is 0")
    else:
        produced = server["audio_samples"]
        if produced < 1:
            found.append("no audio sample arrived")
        size = (
            BYTES_PER_SAMPLE.get(server["output_format"]["encoding"], 0)
            * (server["output_format"]["channels"])
        )
        if server["audio_bytes"] != produced * size:
            found.append("the audio is not whole samples of the output format")
        expect("segment id", server["segment"]["segment_id"], client["segment_id"])
        expect("segment total_samples", server["segment"]["total_samples"], produced)
        expect("segment chunk_count", server["segment"]["chunk_count"], server["chunks"])
        expect("synthesis segment_count", server["synthesis"]["segment_count"], 1)
        expect("synthesis total_samples", server["synthesis"]["total_samples"], produced)
        expect(
            "synthesis finalize_sequence",
            server["synthesis"]["finalize_sequence"],
            client["finalize_sequence"],
        )
        expect("total output_audio_samples", totals["output_audio_samples"], produced)
        expect("total input_text_bytes", totals["input_text_bytes"], section["input"]["text_bytes"])
        expect("total segments_completed", totals["segments_completed"], 1)
    return found


def _describe(session: dict[str, Any]) -> str:
    outcome = session["outcome"]
    if outcome == "stopped":
        return f"stopped ({session['stop_reason']})"
    if outcome == "session_failed":
        closed = session["server"]["closed"]
        return f"session failed ({closed['reason'] or closed['state']})"
    if outcome == "call_failed":
        events = session["server"]["events_received"]
        return f"call ended {session['grpc_status']} after {events} server events"
    return outcome


_RUN_ENDS = {None: "did not end", "interrupt": "interrupted", "error": "ended by an error ({})"}


def derive_result(record: dict[str, Any]) -> dict[str, Any]:
    """The run's status and why, from how it ended and from the sessions."""
    failed: list[str] = []
    incomplete: list[str] = []
    ended = record["run"]["ended_by"]
    if ended != "completed":
        failed.append(f"run: {_RUN_ENDS[ended].format(record['run']['error'])}")
    for mode in MODES:
        section = record[mode]
        if section is None:
            incomplete.append(f"{mode}: not run")
            continue
        if section["target"]["connect_ns"] is None:
            failed.append(f"{mode}: the channel never became ready")
        for session in section["sessions"]:
            label = f"{mode} session {session['index']}"
            if session["outcome"] != "completed":
                failed.append(f"{label}: {_describe(session)}")
                continue
            failed.extend(
                f"{label}: {finding}" for finding in session_findings(mode, section, session)
            )
            for name, entry in derive_measurements(mode, session).items():
                if entry["ns"] is None:
                    incomplete.append(f"{label}: no sample for {name}")
    status = "failed" if failed else "incomplete" if incomplete else "complete"
    return {"status": status, "reasons": failed + incomplete}


def finalize(record: dict[str, Any]) -> dict[str, Any]:
    """Fill every derived field from the record's timestamps and counts."""
    for mode in MODES:
        section = record[mode]
        if section is None:
            continue
        for session in section["sessions"]:
            for utterance in session.get("utterances", []):
                utterance["boundaries"] = derive_utterance_boundaries(session, utterance)
                utterance["measurements"] = derive_utterance_measurements(utterance)
                utterance["cadence"] = derive_cadence(utterance)
            session["measurements"] = derive_measurements(mode, session)
        section["summary"] = derive_summary(mode, section)
    record["gpu"]["summary"] = derive_gpu_summary(record["gpu"]["samples"])
    if record["schema_version"] == "0.3":
        record["aggregate"] = derive_aggregate(record)
    record["result"] = derive_result(record)
    return record


# --- the check ---------------------------------------------------------------


def _first_difference(claimed: Any, derived: Any, pointer: str = "") -> str:
    if isinstance(claimed, dict) and isinstance(derived, dict):
        for key in sorted(set(claimed) | set(derived)):
            if key not in claimed or key not in derived:
                return f"{pointer}/{key}: present on one side only"
            found = _first_difference(claimed[key], derived[key], f"{pointer}/{key}")
            if found:
                return found
        return ""
    if claimed != derived:
        return f"{pointer or '/'}: claimed {claimed!r}, derived {derived!r}"
    return ""


def _require_derived(what: str, claimed: Any, derived: Any) -> None:
    if claimed != derived:
        found = _first_difference(claimed, derived)
        raise RecordError(f"{what} differs from the one derived from the timestamps: {found}")


def _check_boundaries(mode: str, label: str, session: dict[str, Any], not_before: int) -> int:
    """Hold the session's timestamps to their order; returns its latest one."""
    boundaries = session["boundaries"]
    latest = boundaries["call_start"]
    if latest < not_before:
        raise RecordError(f"{label}: starts before the session ahead of it ended")
    missing = None
    for name in CHAIN[mode]:
        at = boundaries[name]
        if at is None:
            missing = missing or name
            continue
        if missing is not None:
            raise RecordError(f"{label}: {name} is set and {missing}, which precedes it, is not")
        if at < latest:
            raise RecordError(f"{label}: {name} precedes the boundary before it")
        latest = at
    partial = boundaries.get("first_partial")
    if partial is not None:
        start, final = boundaries["first_audio_sent"], boundaries["final_transcript"]
        if start is None or partial < start or (final is not None and partial > final):
            raise RecordError(f"{label}: first_partial is outside the utterance")
        latest = max(latest, partial)
    closed = boundaries["session_closed"]
    if closed is not None:
        read = [boundaries[name] for name in RECEIVED if boundaries.get(name) is not None]
        if closed < max(read, default=boundaries["call_start"]):
            raise RecordError(f"{label}: session_closed precedes an event read before it")
        latest = max(latest, closed)
    for utterance in session.get("utterances", []):
        times = [at for at in utterance["boundaries"].values() if at is not None]
        times.extend(p["at_ns"] for p in utterance["partials"])
        latest = max(latest, max(times, default=latest))
    return latest


def _check_outcome(mode: str, label: str, session: dict[str, Any], vocab: dict[str, Any]) -> None:
    outcome, server, boundaries = session["outcome"], session["server"], session["boundaries"]
    closed, status = server["closed"], session["grpc_status"]
    if (session["stop_reason"] is not None) != (outcome == "stopped"):
        raise RecordError(f"{label}: stop_reason does not agree with outcome {outcome}")
    if status is not None and status not in GRPC_STATUS_NAMES:
        raise RecordError(f"{label}: {status!r} is not a gRPC status code")
    if (closed is None) != (boundaries["session_closed"] is None):
        raise RecordError(f"{label}: SessionClosed and its timestamp do not agree")
    if closed is not None:
        for field, known in (("reason", vocab["reasons"]), ("code", vocab["codes"])):
            if closed[field] is not None and closed[field] not in known:
                raise RecordError(
                    f"{label}: {closed[field]!r} is not a {field} the protocol defines"
                )
    ended_clean = closed is not None and closed["state"] == "closed" and closed["reason"] is None
    if outcome == "completed":
        reached = all(boundaries[name] is not None for name in CHAIN[mode])
        if not (reached and ended_clean and status == "OK" and closed["code"] is None):
            raise RecordError(f"{label}: completed without every boundary, a clean close and OK")
        if any(server[name] is None for name in ANSWERS[mode]) or server["limits"] is None:
            raise RecordError(f"{label}: completed without the answers it measures")
        if mode == "tts" and server["output_format"] is None:
            raise RecordError(f"{label}: completed without an output format")
    elif outcome == "session_failed":
        if closed is None or ended_clean:
            raise RecordError(f"{label}: session_failed without a failed SessionClosed")
    elif outcome == "call_failed":
        if status is None or (ended_clean and status == "OK"):
            raise RecordError(f"{label}: call_failed needs a status that explains it")
    elif outcome == "timeout" and status is not None:
        raise RecordError(f"{label}: a timeout carries a status")


def _check_stt_input(section: dict[str, Any]) -> int:
    """Hold the declared audio to its own arithmetic; returns one frame in ns."""
    declared, audio = section["input"], section["input"]["format"]
    size = BYTES_PER_SAMPLE.get(audio["encoding"], 0) * audio["channels"]
    if declared["audio_bytes"] != declared["samples"] * size:
        raise RecordError("stt input: audio_bytes is not samples times the sample size")
    per_frame, remainder = divmod(audio["sample_rate_hz"] * declared["frame_ms"], 1000)
    if per_frame < 1 or remainder:
        raise RecordError("stt input: a frame is not a whole number of samples")
    if declared["frames"] != -(-declared["samples"] // per_frame):
        raise RecordError("stt input: frames is not samples divided into frames")
    return declared["frame_ms"] * 1_000_000


def _check_counts(mode: str, label: str, session: dict[str, Any]) -> None:
    """Hold the session's counts to the events its other fields record."""
    server, boundaries = session["server"], session["boundaries"]
    single = sum(server[name] is not None for name in (*ANSWERS[mode], "closed"))
    if mode == "stt" and "utterances" in session:
        single = int(server["closed"] is not None) + sum(
            int(u["endpoint"] is not None) + int(u["final"] is not None)
            for u in session["utterances"]
        )
    many = server["partials"] if mode == "stt" else server["chunks"]
    counted = server["accepted_events"] + server["ignored_events"] + many + single
    if server["events_received"] < counted + (boundaries["ready"] is not None):
        raise RecordError(f"{label}: events_received is below the events the record counts")
    if mode == "stt":
        if boundaries["first_partial"] is not None and not server["partials"]:
            raise RecordError(f"{label}: first_partial is set and no partial arrived")
        return
    audio = (server["audio_bytes"], server["audio_samples"], server["audio_sha256"])
    if not server["chunks"] and (*audio, boundaries["first_audio_chunk"]) != (0, 0, None, None):
        raise RecordError(f"{label}: audio is recorded and no chunk arrived")


def _check_input(
    mode: str, label: str, section: dict[str, Any], session: dict[str, Any], frame: int
) -> None:
    client, boundaries = session["client"], session["boundaries"]
    due, sent = (boundaries[name] for name in INPUT[mode])
    lag = None if sent is None else sent - due
    if client["input_lag_ns"] != lag:
        raise RecordError(f"{label}: input_lag_ns is not {INPUT[mode][1]} less {INPUT[mode][0]}")
    if mode == "tts":
        return
    first = boundaries["first_audio_sent"]
    if (first is None) != (client["frames_sent"] == 0):
        raise RecordError(f"{label}: frames_sent and first_audio_sent do not agree")
    if (first is None) != (client["max_pacing_lag_ns"] is None):
        raise RecordError(f"{label}: max_pacing_lag_ns and first_audio_sent do not agree")
    if lag is None:
        return
    if client["frames_sent"] != section["input"]["frames"]:
        raise RecordError(f"{label}: last_audio_sent is set before every frame was sent")
    if due != first + (client["frames_sent"] - 1) * frame:
        raise RecordError(f"{label}: last_audio_scheduled is not the last frame's place in time")
    if client["max_pacing_lag_ns"] < lag:
        raise RecordError(f"{label}: max_pacing_lag_ns is below the last frame's lag")


def _check_utterances(label: str, section: dict[str, Any], session: dict[str, Any]) -> None:
    client, server, bounds = session["client"], session["server"], session["boundaries"]
    frames = client["audio_frames"]
    per_frame = section["input"]["format"]["sample_rate_hz"] * section["input"]["frame_ms"] // 1000
    offset, previous = 0, bounds["ready"]
    for index, frame in enumerate(frames):
        end = min(offset + per_frame, section["input"]["samples"])
        due = bounds["first_audio_sent"] + index * section["input"]["frame_ms"] * 1_000_000
        if (
            frame["start_sample_offset"] != offset
            or frame["end_sample_offset"] != end
            or end <= offset
            or frame["scheduled_ns"] != due
            or frame["sent_ns"] < max(due, previous)
        ):
            raise RecordError(f"{label}: audio frame offsets or timestamps disagree with input")
        offset, previous = end, frame["sent_ns"]
    if (
        len(frames) != client["frames_sent"]
        or offset != client["samples_sent"]
        or offset * 2 != client["bytes_sent"]
    ):
        raise RecordError(f"{label}: audio frames disagree with client counts")
    if frames:
        if frames[0]["sent_ns"] != bounds["first_audio_sent"]:
            raise RecordError(f"{label}: first audio frame timestamp disagrees")
        if (
            bounds["last_audio_sent"] is not None
            and frames[-1]["sent_ns"] != bounds["last_audio_sent"]
        ):
            raise RecordError(f"{label}: last audio frame timestamp disagrees")
        lag = max(f["sent_ns"] - f["scheduled_ns"] for f in frames)
        if lag != client["max_pacing_lag_ns"]:
            raise RecordError(f"{label}: audio frames disagree with max_pacing_lag_ns")
    ids, start, previous_endpoint, partial_count = set(), 0, None, 0
    finalize_utterance = None
    first_partial = None
    for utterance in session["utterances"]:
        uid = utterance["utterance_id"]
        if uid in ids or utterance["start_sample_offset"] != start:
            raise RecordError(f"{label}: utterance identity or start offset disagrees")
        if ids and previous_endpoint is None:
            raise RecordError(f"{label}: utterance starts without the preceding endpoint")
        ids.add(uid)
        ub, endpoint, final = (
            utterance["boundaries"],
            utterance["endpoint"],
            utterance["final"],
        )
        _require_derived(
            f"{label}: utterance boundaries",
            ub,
            derive_utterance_boundaries(session, utterance),
        )
        for key, answer in (("endpoint", endpoint), ("final_transcript", final)):
            at = ub[key]
            if (at is None) != (answer is None):
                raise RecordError(f"{label}: utterance {key} and timestamp disagree")
            if at is not None and (
                bounds["ready"] is None
                or at < bounds["ready"]
                or (bounds["session_closed"] is not None and at > bounds["session_closed"])
            ):
                raise RecordError(f"{label}: utterance event outside the session")
        if endpoint is not None:
            end = endpoint["end_sample_offset"]
            available = max(
                (f["end_sample_offset"] for f in frames if f["sent_ns"] <= ub["endpoint"]),
                default=0,
            )
            if end < start or end > available:
                raise RecordError(f"{label}: utterance endpoint offset outside sent audio")
            if previous_endpoint is not None and ub["endpoint"] < previous_endpoint:
                raise RecordError(f"{label}: utterance endpoints are out of order")
            if endpoint["reason"] == "client_finalize":
                if (
                    finalize_utterance is not None
                    or bounds["finalize_sent"] is None
                    or ub["endpoint"] < bounds["finalize_sent"]
                ):
                    raise RecordError(f"{label}: utterance endpoint does not follow Finalize")
                finalize_utterance = utterance
            elif endpoint["reason"] not in ("silence", "duration_limit", "half_close"):
                raise RecordError(f"{label}: unknown utterance endpoint reason")
            start = end
        if final is not None and (endpoint is None or ub["final_transcript"] < ub["endpoint"]):
            raise RecordError(f"{label}: utterance final without its endpoint")
        revision, at = -1, ub["first_audio_sent"]
        for partial in utterance["partials"]:
            if (
                at is None
                or partial["at_ns"] < at
                or partial["revision"] <= revision
                or (previous_endpoint is not None and partial["at_ns"] < previous_endpoint)
                or (
                    bounds["session_closed"] is not None
                    and partial["at_ns"] > bounds["session_closed"]
                )
                or (ub["endpoint"] is not None and partial["at_ns"] > ub["endpoint"])
            ):
                raise RecordError(f"{label}: partial outside its utterance or revision order")
            if partial["text_bytes"] and first_partial is None:
                first_partial = partial["at_ns"]
            revision, at = partial["revision"], partial["at_ns"]
            partial_count += 1
        _require_derived(
            f"{label}: utterance measurements",
            utterance["measurements"],
            derive_utterance_measurements(utterance),
        )
        _require_derived(
            f"{label}: partial cadence", utterance["cadence"], derive_cadence(utterance)
        )
        previous_endpoint = ub["endpoint"]
    if partial_count != server["partials"] or first_partial != bounds["first_partial"]:
        raise RecordError(f"{label}: utterance partials disagree with session")
    for key, boundary in (("endpoint", "endpoint"), ("final", "final_transcript")):
        expected = finalize_utterance[key] if finalize_utterance else None
        at = finalize_utterance["boundaries"][boundary] if finalize_utterance else None
        if server[key] != expected or bounds[boundary] != at:
            raise RecordError(f"{label}: session {key} disagrees with Finalize utterance")


def _check_gpu(gpu: dict[str, Any], loopback: bool) -> None:
    status, samples = gpu["status"], gpu["samples"]
    if status == "sampled":
        if not samples or gpu["reason"] is not None or not gpu["interval_ms"]:
            raise RecordError("gpu: sampled needs samples, an interval and no reason")
    elif samples or (gpu["reason"] is None) != (status == "not_requested"):
        raise RecordError(f"gpu: {status} carries samples or the wrong reason")
    if status == "not_requested" and gpu["interval_ms"] is not None:
        raise RecordError("gpu: not_requested carries an interval")
    whose = None if status != "sampled" else "serving" if loopback else "client_only"
    if gpu["machine"] != whose:
        raise RecordError(f"gpu: machine is {gpu['machine']!r} and the targets make it {whose!r}")
    named = [device["index"] for device in gpu["devices"]]
    if sorted(set(named)) != named or set(named) != {sample["index"] for sample in samples}:
        raise RecordError("gpu: devices are not the devices the samples name, each once in order")
    for earlier, later in itertools.pairwise(samples):
        if later["at_ns"] < earlier["at_ns"]:
            raise RecordError("gpu: samples are not in time order")
    _require_derived("gpu summary", gpu["summary"], derive_gpu_summary(samples))


def _check_claims(record: dict[str, Any]) -> None:
    """Hold what the record says of itself to what it carries."""
    tool, worker, run = record["tool"], record["worker"], record["run"]
    if tool["generator"] != tool["grpcio"]:
        raise RecordError("tool: the generator is not the grpcio release")
    if record["provenance"] == "recorded" and tool["source_commit"] is None:
        raise RecordError("provenance: recorded needs the tool's source commit")
    stated = worker["version"] is not None or worker["build"] is not None
    if stated != (worker["source"] == "operator_stated"):
        raise RecordError("worker: source does not agree with the version and build")
    if record["provenance"] == "recorded" and not stated:
        raise RecordError("provenance: recorded needs the worker's release or build")
    if (run["error"] is None) != (run["ended_by"] != "error"):
        raise RecordError("run: error does not agree with ended_by")
    if record["schema_version"] == "0.3":
        settings = record["settings"]
        if not 1 <= settings["sessions"] <= 64:
            raise RecordError("settings.sessions must be 1 to 64")
        if (
            settings["expect_test_count"] is not None
            and not 1 <= settings["expect_test_count"] <= 2048
        ):
            raise RecordError("settings.expect_test_count must be 1 to 2048")
        if (run["finished_ns"] is None) != (run["ended_by"] is None):
            raise RecordError("run: finished_ns does not agree with ended_by")
    elif "aggregate" in record or "sessions" in record["settings"] or "finished_ns" in run:
        raise RecordError("concurrent settings and aggregates require schema version 0.3")


def _check_connections(section: dict[str, Any], count: int, stagger_ns: int) -> None:
    previous = None
    batch_ends: dict[int, int] = {}
    for session in section["sessions"]:
        connection = session.get("connection")
        if connection is None:
            raise RecordError("concurrent sessions require connection observations")
        index = session["index"]
        start, duration, end = (connection[k] for k in ("started_ns", "connect_ns", "ended_ns"))
        call_start = session["boundaries"]["call_start"]
        latest = _check_boundaries(
            "stt" if "utterances" in session else "tts", "connection", session, 0
        )
        if (
            start > call_start
            or end < latest
            or (duration is not None and start + duration > call_start)
        ):
            raise RecordError("connection timestamps do not enclose the session")
        if duration is None and (
            session["client"]["events_sent"] or session["outcome"] not in ("timeout", "stopped")
        ):
            raise RecordError("an unconnected session cannot send events or complete")
        batch = index // count
        if start < max((at for b, at in batch_ends.items() if b < batch), default=0):
            raise RecordError("a batch starts before the previous batch ended")
        batch_ends[batch] = max(batch_ends.get(batch, 0), end)
        if previous is not None and previous[0] // count == batch:
            if start < previous[1] + (index - previous[0]) * stagger_ns:
                raise RecordError("session launches do not respect stagger_ms")
        previous = index, start
    connected = [s["connection"]["connect_ns"] for s in section["sessions"]]
    if section["target"]["connect_ns"] != next((n for n in connected if n is not None), None):
        raise RecordError("target connect_ns is not the first successful connection")


def check_record(record: Any) -> tuple[str, str]:
    """The record's status and why, or `invalid` when the record cannot be trusted."""
    try:
        validate_record(record, load_schema())
    except RecordError as exc:
        return "invalid", f"record does not match its schema: {exc}"
    vocab = {
        "reasons": _enum(FAILURE_REASON_SCHEMA_PATH, "reason") | {"unspecified"},
        "codes": _enum(ERROR_SCHEMA_PATH, "code") | {"unspecified"},
    }
    try:
        _check_claims(record)
        ended = record["run"]["ended_by"] == "completed"
        count = record["settings"].get("sessions", 1)
        finished = record["run"].get("finished_ns")
        loopback = True
        for mode in MODES:
            section = record[mode]
            if section is None:
                continue
            target = section["target"]
            loopback = loopback and target["loopback"]
            if (target["generation"] is None) != (target["descriptor_digest"] is None):
                raise RecordError(f"{mode} target: a generation without a digest, or the reverse")
            frame = _check_stt_input(section) if mode == "stt" else 0
            sessions = section["sessions"]
            expected = record["settings"]["iterations"] * count
            if count == 1 and target["connect_ns"] is None:
                expected = 0
            if len(sessions) > expected or (ended and len(sessions) < expected):
                raise RecordError(f"{mode}: {len(sessions)} sessions recorded, {expected} expected")
            latest = 0
            indices = [s["index"] for s in sessions]
            if count > 1:
                if indices != sorted(set(indices)) or any(i >= expected for i in indices):
                    raise RecordError(
                        f"{mode}: session indices must be unique, ordered and in range"
                    )
                _check_connections(section, count, record["settings"]["stagger_ms"] * 1_000_000)
            for position, session in enumerate(sessions):
                label = f"{mode} session {position}"
                if count == 1 and session["index"] != position:
                    raise RecordError(f"{label}: carries index {session['index']}")
                latest = _check_boundaries(mode, label, session, latest if count == 1 else 0)
                if [entry["stage"] for entry in session["server_stages"]] != list(SERVER_STAGES):
                    raise RecordError(f"{label}: server_stages is not the registered stage list")
                _check_outcome(mode, label, session, vocab)
                _check_counts(mode, label, session)
                _check_input(mode, label, section, session, frame)
                if mode == "stt":
                    has_utterances = "utterances" in session
                    has_frames = "audio_frames" in session["client"]
                    if has_utterances != has_frames or (
                        record["schema_version"] in ("0.2", "0.3") and not has_frames
                    ):
                        raise RecordError(
                            f"{label}: per-utterance records need utterances and audio_frames"
                        )
                    if has_utterances:
                        _check_utterances(label, section, session)
                if (
                    finished is not None
                    and max(latest, session.get("connection", {}).get("ended_ns", 0)) > finished
                ):
                    raise RecordError(f"{label}: ends after the run finished")
                _require_derived(
                    f"{label}: measurements",
                    session["measurements"],
                    derive_measurements(mode, session),
                )
            _require_derived(f"{mode} summary", section["summary"], derive_summary(mode, section))
        _check_gpu(record["gpu"], loopback)
        if record["schema_version"] == "0.3":
            _require_derived("aggregate", record["aggregate"], derive_aggregate(record))
        _require_derived("result", record["result"], derive_result(record))
    except RecordError as exc:
        return "invalid", str(exc)
    result = record["result"]
    return result["status"], "; ".join(result["reasons"]) or "every session completed"


def exit_status(status: str) -> int:
    if status == "complete":
        return EXIT_COMPLETE
    if status == "failed":
        return EXIT_FAILED
    return EXIT_NO_VERDICT


def dump(record: dict[str, Any]) -> str:
    return json.dumps(record, indent=2, sort_keys=False) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check", help="verify a record and print its status")
    check.add_argument("record", type=Path)
    args = parser.parse_args(argv)
    try:
        record = json.loads(args.record.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        print(f"{TOOL_NAME}: invalid: {args.record}: {exc}", file=sys.stderr)
        return EXIT_NO_VERDICT
    status, detail = check_record(record)
    stream = sys.stdout if status == "complete" else sys.stderr
    if status != "invalid" and record.get("provenance") == "synthetic":
        print(f"{TOOL_NAME}: provenance synthetic: these numbers measure nothing", file=stream)
    print(f"{TOOL_NAME}: {status}: {detail}", file=stream)
    return exit_status(status)


if __name__ == "__main__":
    sys.exit(main())
