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
        "last_audio_sent",
        "finalize_sent",
        "endpoint",
        "final_transcript",
    ),
    "tts": (
        "call_start",
        "ready",
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

# name -> (from, to, the server event that ends it)
MEASUREMENTS = {
    "stt": {
        "session_initialization": ("call_start", "ready", "ready"),
        "call_start_to_first_transcript": ("call_start", "final_transcript", "final_transcript"),
        "last_audio_to_final_transcript": (
            "last_audio_sent",
            "final_transcript",
            "final_transcript",
        ),
    },
    "tts": {
        "session_initialization": ("call_start", "ready", "ready"),
        "call_start_to_first_audio": ("call_start", "first_audio_chunk", "audio_chunk"),
        "segment_to_first_audio": ("text_segment_sent", "first_audio_chunk", "audio_chunk"),
        "segment_to_completion": ("text_segment_sent", "segment_completed", "segment_completed"),
    },
}

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
        }
        server: dict[str, Any] = {"partials": 0, "limits": None, "endpoint": None, "final": None}
    else:
        client = {
            "events_sent": 0,
            "pings_sent": 0,
            "credit_waits": 0,
            "segment_id": None,
            "finalize_sequence": None,
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
    session["measurements"] = derive_measurements(mode, session)
    return session


def derive_measurements(mode: str, session: dict[str, Any]) -> dict[str, Any]:
    boundaries = session["boundaries"]
    derived = {}
    for name, (start, end, event) in MEASUREMENTS[mode].items():
        valid = True
        if name == "call_start_to_first_transcript":
            if boundaries["first_partial"] is not None:
                end, event = "first_partial", "partial_transcript"
            else:
                final = session["server"]["final"]
                valid = final is not None and final["text_bytes"] > 0
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


def derive_summary(mode: str, sessions: list[dict[str, Any]]) -> dict[str, Any]:
    measured = [derive_measurements(mode, session) for session in sessions]
    return {
        name: distribution([entry[name]["ns"] for entry in measured]) for name in MEASUREMENTS[mode]
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
        if totals["utterances_completed"] < 1:
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


def derive_result(record: dict[str, Any]) -> dict[str, Any]:
    """The run's status and why, from the sessions only."""
    failed: list[str] = []
    incomplete: list[str] = []
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
            session["measurements"] = derive_measurements(mode, session)
        section["summary"] = derive_summary(mode, section["sessions"])
    record["gpu"]["summary"] = derive_gpu_summary(record["gpu"]["samples"])
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
        answers = ("endpoint", "final") if mode == "stt" else ("segment", "synthesis")
        reached = all(boundaries[name] is not None for name in CHAIN[mode])
        if not (reached and ended_clean and status == "OK" and closed["code"] is None):
            raise RecordError(f"{label}: completed without every boundary, a clean close and OK")
        if any(server[name] is None for name in answers) or server["limits"] is None:
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


def _check_pacing(label: str, section: dict[str, Any], session: dict[str, Any], frame: int) -> None:
    client, boundaries = session["client"], session["boundaries"]
    first, last = boundaries["first_audio_sent"], boundaries["last_audio_sent"]
    if (first is None) != (client["frames_sent"] == 0):
        raise RecordError(f"{label}: frames_sent and first_audio_sent do not agree")
    if (first is None) != (client["max_pacing_lag_ns"] is None):
        raise RecordError(f"{label}: max_pacing_lag_ns and first_audio_sent do not agree")
    if last is None:
        return
    if client["frames_sent"] != section["input"]["frames"]:
        raise RecordError(f"{label}: last_audio_sent is set before every frame was sent")
    lag = last - first - (client["frames_sent"] - 1) * frame
    if lag < 0:
        raise RecordError(f"{label}: the audio was sent faster than real time")
    if client["max_pacing_lag_ns"] < lag:
        raise RecordError(f"{label}: max_pacing_lag_ns is below the last frame's lag")


def _check_gpu(gpu: dict[str, Any]) -> None:
    status, samples = gpu["status"], gpu["samples"]
    if status == "sampled":
        if not samples or gpu["reason"] is not None or not gpu["interval_ms"]:
            raise RecordError("gpu: sampled needs samples, an interval and no reason")
    elif samples or (gpu["reason"] is None) != (status == "not_requested"):
        raise RecordError(f"gpu: {status} carries samples or the wrong reason")
    if status == "not_requested" and gpu["interval_ms"] is not None:
        raise RecordError("gpu: not_requested carries an interval")
    for earlier, later in itertools.pairwise(samples):
        if later["at_ns"] < earlier["at_ns"]:
            raise RecordError("gpu: samples are not in time order")
    _require_derived("gpu summary", gpu["summary"], derive_gpu_summary(samples))


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
        if record["tool"]["generator"] != record["tool"]["grpcio"]:
            raise RecordError("tool: the generator is not the grpcio release")
        for mode in MODES:
            section = record[mode]
            if section is None:
                continue
            frame = _check_stt_input(section) if mode == "stt" else 0
            sessions = section["sessions"]
            expected = (
                0 if section["target"]["connect_ns"] is None else record["settings"]["iterations"]
            )
            if len(sessions) != expected:
                raise RecordError(f"{mode}: {len(sessions)} sessions recorded, {expected} expected")
            latest = 0
            for position, session in enumerate(sessions):
                label = f"{mode} session {position}"
                if session["index"] != position:
                    raise RecordError(f"{label}: carries index {session['index']}")
                latest = _check_boundaries(mode, label, session, latest)
                if [entry["stage"] for entry in session["server_stages"]] != list(SERVER_STAGES):
                    raise RecordError(f"{label}: server_stages is not the registered stage list")
                _check_outcome(mode, label, session, vocab)
                if mode == "stt":
                    _check_pacing(label, section, session, frame)
                _require_derived(
                    f"{label}: measurements",
                    session["measurements"],
                    derive_measurements(mode, session),
                )
            _require_derived(f"{mode} summary", section["summary"], derive_summary(mode, sessions))
        _check_gpu(record["gpu"])
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
