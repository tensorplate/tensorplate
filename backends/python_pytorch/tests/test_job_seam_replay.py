"""The job seam's vectors, replayed through the sidecar's job objects.

``protocol/fixtures/job_seam.json`` is shared with the C++ objects and the Rust
mirror. Each vector is rendered as the socket header the Rust suite renders
and read back, or built as the objects a runner returns and rendered.
"""

from __future__ import annotations

import copy
import json
from pathlib import Path
from typing import Any

import pytest

from tensorplate_pytorch_backend import job_objects, protocol

_REPO = Path(__file__).resolve().parents[3]
_SEAM = json.loads((_REPO / "protocol" / "fixtures" / "job_seam.json").read_text(encoding="utf-8"))
_IDENTITY = {"job_id": 1, "session_key": 1, "generation": 1}
_INPUT_FIELDS = {
    "audio_frames": ("format", "start_sample"),
    "text_segment": (),
    "vad_frames": ("format", "frame_samples", "frame_count", "utterance_id"),
}


def _vectors(section: str) -> list[dict[str, Any]]:
    return [vector for vector in _SEAM[section] if vector.get("scope", "all") == "all"]


def _text_bytes(text: dict[str, Any]) -> bytes:
    if "utf8" in text:
        return str(text["utf8"]).encode("utf-8")
    if "hex" in text:
        return bytes.fromhex(text["hex"])
    return str(text["repeat"] * text["times"] + text.get("then", "")).encode("utf-8")


def _text(text: dict[str, Any]) -> str:
    # A str carries ill-formed UTF-8 as lone surrogates, which the mirror refuses.
    return _text_bytes(text).decode("utf-8", "surrogateescape")


def _format(value: Any) -> dict[str, Any]:
    return dict(_SEAM["formats"][value] if isinstance(value, str) else value)


def _envelope(message_id: str, kind: str, identity: dict[str, int]) -> dict[str, Any]:
    return {
        "schema_version": protocol.SCHEMA_VERSION,
        "message_id": message_id,
        "kind": kind,
        **{name: identity[name] for name in ("job_id", "session_key", "generation")},
    }


def _render_request(message_id: str, request: dict[str, Any]) -> tuple[dict[str, Any], bytes]:
    source = request["payload"]
    if source["type"] == "text_segment":
        payload = _text_bytes(source["text"])
    else:
        payload = bytes(source["pcm"]["size"])
    rendered = {"type": source["type"]}
    for name in _INPUT_FIELDS[source["type"]]:
        rendered[name] = _format(source[name]) if name == "format" else source[name]
    rendered["payload_length"] = len(payload)
    header = _envelope(message_id, protocol.KIND_JOB_SUBMIT, request["identity"])
    header.update(
        job_class=request["job_class"],
        progress_limit=request["progress_limit"],
        options=request["options"],
        input=rendered,
    )
    return header, payload


def _render_result(result: dict[str, Any]) -> tuple[dict[str, Any], bytes]:
    kind = result["type"]
    if kind == "transcript":
        tokens = result["tokens"]
        if isinstance(tokens, dict):
            tokens = list(range(1, tokens["count"] + 1))
        words = result["words"]
        if isinstance(words, dict):
            text = _text(words["one_per_token"]["text"])
            words = [
                {
                    "text": text,
                    "token_begin": i,
                    "token_end": i + 1,
                    "start_sample": 512 * i,
                    "end_sample": 512 * (i + 1),
                }
                for i in range(len(tokens))
            ]
        else:
            words = [{**word, "text": _text(word["text"])} for word in words]
        rendered = {"type": kind, "text": _text(result["text"]), "tokens": tokens, "words": words}
        return rendered, b""
    if kind == "audio_chunk":
        payload = bytes(result["pcm"]["size"])
        return {
            "type": kind,
            "format": _format(result["format"]),
            "clipped_samples": result["clipped_samples"],
            "payload_length": len(payload),
        }, payload
    return {"type": kind, "probabilities": [float(p) for p in result["probabilities"]]}, b""


def _result_object(wire: dict[str, Any], payload: bytes) -> job_objects.JobResult:
    """The object a runner returns for the result ``wire`` and ``payload`` carry."""
    if wire["type"] == "transcript":
        words = tuple(job_objects.WordUnit(**word) for word in wire["words"])
        return job_objects.TranscriptResult(wire["text"], tuple(wire["tokens"]), words)
    if wire["type"] == "audio_chunk":
        return job_objects.AudioChunkResult(
            payload, wire["clipped_samples"], job_objects.AudioFormat(**wire["format"])
        )
    return job_objects.VadResult(tuple(wire["probabilities"]))


def _render_event(
    message_id: str, event: dict[str, Any], identity: dict[str, int]
) -> tuple[dict[str, Any], bytes]:
    header = _envelope(message_id, f"job_{event['kind']}", event.get("identity", identity))
    payload = b""
    if "error" in event:
        error = event["error"]
        wire = {
            "schema_version": protocol.SCHEMA_VERSION,
            "code": error["code"],
            "message": _text(error["message"]),
        }
        if "context" in error:
            wire["context"] = _text(error["context"])
        header["error"] = wire
    if "progress_sequence" in event:
        header["progress_sequence"] = event["progress_sequence"]
    if "result" in event:
        header["result"], payload = _render_result(event["result"])
    return header, payload


def _job_event(
    message_id: str, event: dict[str, Any], identity: dict[str, int]
) -> tuple[dict[str, Any], bytes]:
    """Build the event through the mirror, as the job table does, and number it."""
    header, payload = _render_event(message_id, event, identity)
    error = header.get("error")
    built, payload = job_objects.job_event(
        header["kind"],
        job_objects.JobIdentity(header["job_id"], header["session_key"], header["generation"]),
        progress_sequence=header.get("progress_sequence"),
        result=_result_object(header["result"], payload) if "result" in header else None,
        error=job_objects.error_object(error["code"], error["message"], error.get("context"))
        if error
        else None,
    )
    assert built["message_id"] == ""
    built["message_id"] = message_id
    return built, payload


def _refusal(vector: dict[str, Any]) -> str | None:
    expect = vector["expect"]
    return None if expect == "accept" else str(expect["reject"])


def test_limits_are_the_job_seam_limits() -> None:
    limits = {
        name[6:].lower(): value
        for name, value in vars(protocol).items()
        if name.startswith("LIMIT_")
    }
    assert limits == _SEAM["limits"]
    assert set(job_objects.RESULT_KIND_OF.items()) == set(
        _SEAM["names"]["result_kind_for_job_class"].items()
    )


def test_every_request_vector_replays() -> None:
    count = 0
    for vector in _vectors("requests"):
        name, source = vector["name"], vector["request"]
        header, payload = _render_request("a1", source)
        reason = _refusal(vector)
        count += 1
        if reason is not None:
            with pytest.raises(job_objects.JobRefused) as refused:
                job_objects.read_submit(header, payload)
            assert (refused.value.reason, refused.value.code) == (
                reason,
                protocol.ERR_CONFIG_INVALID,
            ), name
            continue
        request = job_objects.read_submit(header, payload)
        assert request.identity == job_objects.JobIdentity(**source["identity"]), name
        assert request.job_class == source["job_class"], name
        assert request.progress_limit == source["progress_limit"], name
        assert request.payload == payload, name
        options = source["options"]
        assert (request.language, request.voice, request.speed_milli) == (
            options.get("language", ""),
            options.get("voice", ""),
            options.get("speed_milli", 0),
        ), name
        for field in _INPUT_FIELDS[source["payload"]["type"]][1:]:
            assert getattr(request, field) == source["payload"][field], name
    assert count == 78, "scope-all request vectors"


def test_every_result_vector_replays() -> None:
    count = 0
    for vector in _vectors("results"):
        name = vector["name"]
        wire, payload = _render_result(vector["result"])
        result = _result_object(wire, payload)
        reason = _refusal(vector)
        count += 1
        if reason is None:
            assert job_objects.render_result(result) == (wire, payload), name
            continue
        with pytest.raises(job_objects.JobRefused) as refused:
            job_objects.render_result(result)
        assert refused.value.reason == reason, name
    assert count == 35, "scope-all result vectors"


def test_every_event_vector_replays() -> None:
    count = 0
    for vector in _vectors("events"):
        name, event = vector["name"], vector["event"]
        reason = _refusal(vector)
        count += 1
        if reason is None:
            assert _job_event("s1", event, _IDENTITY) == _render_event("s1", event, _IDENTITY), name
            continue
        with pytest.raises(job_objects.JobRefused) as refused:
            _job_event("s1", event, _IDENTITY)
        assert (refused.value.reason, refused.value.code) == (
            reason,
            protocol.ERR_CONFIG_INVALID,
        ), name
    assert count == 12, "scope-all event vectors"


def test_every_trace_vector_replays() -> None:
    traces = messages = 0
    for trace in _vectors("traces"):
        name, expect = trace["name"], trace["expect"]
        sequences = {}
        for index, (job, source) in enumerate(trace["jobs"].items(), start=1):
            request = job_objects.read_submit(*_render_request(f"a{index}", source))
            sequences[job] = job_objects.JobEventSequence.for_request(request)
            messages += 1
        refused_at = None if expect == "accept" else expect["refused_at"]
        for index, step in enumerate(trace["steps"]):
            sequence = sequences[step["job"]]
            messages += 1
            if "cancel" in step:
                sequence.note_cancel_requested()
                continue
            identity = trace["jobs"][step["job"]]["identity"]
            event = _job_event(f"s{index}", step, identity)
            if index != refused_at:
                sequence.observe(*event)
                assert event == _render_event(f"s{index}", step, identity), name
                continue
            before = copy.deepcopy(sequence)
            with pytest.raises(job_objects.JobRefused) as refused:
                sequence.observe(*event)
            assert (refused.value.reason, refused.value.code) == (
                expect["reason"],
                protocol.ERR_INFERENCE_FAILED,
            ), name
            assert sequence == before, f"{name}: a refused event changed the sequence"
            assert index == len(trace["steps"]) - 1, name
        if refused_at is None:
            assert all(sequence.released for sequence in sequences.values()), name
        traces += 1
    assert (traces, messages) == (46, 207), "trace vectors and their messages"


def test_structural_faults_are_malformed_not_seam_reasons() -> None:
    golden = _vectors("requests")[0]["request"]
    for pointer, value in (
        (("job_id",), None),
        (("job_id",), 2**53),
        (("generation",), "1"),
        (("progress_limit",), True),
        (("unknown",), 1),
        (("options",), ["en"]),
        (("options", "pitch"), 1),
        (("options", "language"), None),
        (("input", "format"), ["pcm_s16le", 16000, 1]),
        (("input", "format", "bits"), 16),
        (("input", "frame_count"), 1),
        (("input", "payload_length"), 4),
        (("input", "type"), "tensor"),
    ):
        header, payload = _render_request("a1", copy.deepcopy(golden))
        target = header
        for key in pointer[:-1]:
            target = target[key]
        target[pointer[-1]] = value
        with pytest.raises(job_objects.MalformedJob):
            job_objects.read_submit(header, payload)
    for missing in ("generation", "job_class", "progress_limit", "options", "input"):
        header, payload = _render_request("a1", copy.deepcopy(golden))
        del header[missing]
        with pytest.raises(job_objects.MalformedJob):
            job_objects.read_submit(header, payload)
