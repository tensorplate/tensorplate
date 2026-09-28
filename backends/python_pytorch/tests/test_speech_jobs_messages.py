"""The sidecar's job and session literals and golden frames against the IPC schema."""

from __future__ import annotations

import contextlib
import json
import socket
import struct
import threading
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

from tensorplate_pytorch_backend import codec, protocol
from tensorplate_pytorch_backend.runner import SidecarRunner

_REPO = Path(__file__).resolve().parents[3]
_SCHEMA = json.loads(
    (_REPO / "protocol" / "schemas" / "python_pytorch_ipc.json").read_text(encoding="utf-8")
)
_SEAM = json.loads((_REPO / "protocol" / "fixtures" / "job_seam.json").read_text(encoding="utf-8"))
_GOLDEN = sorted(
    (_REPO / "protocol" / "rust" / "tests" / "fixtures").glob(
        "python_pytorch_ipc_speech_jobs_*.jsonl"
    )
)
_ADAPTER_KINDS = {protocol.KIND_JOB_SUBMIT, protocol.KIND_JOB_CANCEL, protocol.KIND_SESSION_RELEASE}


def _constants(prefix: str) -> list[str]:
    # Module attributes keep definition order, so this is source order.
    return [value for name, value in vars(protocol).items() if name.startswith(prefix)]


def _golden_headers() -> list[dict[str, Any]]:
    return [
        json.loads(line)
        for path in _GOLDEN
        for line in path.read_text(encoding="utf-8").splitlines()
    ]


def _payload_length(header: dict[str, Any]) -> int:
    for part in (header.get("input"), header.get("result")):
        if isinstance(part, dict) and "payload_length" in part:
            return int(part["payload_length"])
    return 0


def test_kind_constants_are_the_schema_kinds() -> None:
    assert sorted(_constants("KIND_")) == sorted(_SCHEMA["properties"]["kind"]["enum"])


def test_job_constants_match_the_schema_and_the_job_seam() -> None:
    definitions = _SCHEMA["definitions"]
    names = _SEAM["names"]
    assert _constants("JOB_CLASS_") == definitions["JobClass"]["enum"] == names["job_class"]
    assert _constants("JOB_INPUT_") == definitions["JobInput"]["properties"]["type"]["enum"]
    assert (
        _constants("JOB_RESULT_")
        == definitions["JobResult"]["properties"]["type"]["enum"]
        == names["result_kind"]
    )
    assert _constants("AUDIO_ENCODING_") == names["audio_encoding"]
    for audio_format in ("JobInputAudioFormat", "JobOutputAudioFormat"):
        encodings = definitions[audio_format]["properties"]["encoding"]["enum"]
        assert encodings == names["audio_encoding"]
    sidecar_job_kinds = [
        kind for kind in definitions["JobMessageKind"]["enum"] if kind not in _ADAPTER_KINDS
    ]
    assert sidecar_job_kinds == [f"job_{kind}" for kind in names["job_event_kind"]]


def test_new_constants_are_exported() -> None:
    prefixes = ("KIND_", "JOB_CLASS_", "JOB_INPUT_", "JOB_RESULT_", "AUDIO_ENCODING_")
    names = [name for name in vars(protocol) if name.startswith(prefixes)]
    assert set(names) | {"CAPABILITY_SPEECH_JOBS_V1"} <= set(protocol.__all__)


def test_golden_frames_cover_every_job_and_session_kind() -> None:
    definitions = _SCHEMA["definitions"]
    kinds = {header["kind"] for header in _golden_headers()}
    expected = set(definitions["JobMessageKind"]["enum"]) | set(
        definitions["SessionMessageKind"]["enum"]
    )
    assert expected <= kinds
    ready = [h for h in _golden_headers() if h["kind"] == protocol.KIND_READY_EVENT]
    assert ready
    assert all(protocol.CAPABILITY_SPEECH_JOBS_V1 in h["capabilities"] for h in ready)


@pytest.mark.parametrize("path", _GOLDEN, ids=lambda path: path.stem)
def test_golden_frames_round_trip_through_the_codec(path: Path) -> None:
    lines = path.read_text(encoding="utf-8").splitlines()
    assert lines
    for line in lines:
        header = json.loads(line)
        payload = bytes(_payload_length(header))
        blob = codec.encode(codec.SidecarFrame(header=header, payload=payload))
        _magic, _version, header_length, payload_length = struct.unpack_from("!IIII", blob)
        # For these frames the codec writes the header bytes the Rust mirror writes.
        start = codec.FRAME_PREFIX_BYTES
        assert blob[start : start + header_length] == line.encode("utf-8")
        assert payload_length == len(payload)
        decoded, consumed = codec.decode_one(blob)
        assert consumed == len(blob)
        assert decoded.header == header
        assert decoded.payload == payload


@pytest.fixture
def runner_client() -> Iterator[socket.socket]:
    client, server = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    runner = SidecarRunner(server)
    thread = threading.Thread(target=runner.serve_forever, daemon=True)
    thread.start()
    yield client
    with contextlib.suppress(OSError):
        client.shutdown(socket.SHUT_RDWR)
    client.close()
    thread.join(timeout=5.0)


def _exchange(client: socket.socket, frame: codec.SidecarFrame) -> codec.SidecarFrame:
    client.sendall(codec.encode(frame))
    received = bytearray()
    while True:
        try:
            decoded, _consumed = codec.decode_one(bytes(received))
        except codec.IncompleteFrame:
            chunk = client.recv(65536)
            if not chunk:
                raise AssertionError("runner closed before answering") from None
            received.extend(chunk)
            continue
        return decoded


def test_a_runner_that_enabled_no_capability_refuses_job_messages(
    runner_client: socket.socket,
) -> None:
    sent = [h for h in _golden_headers() if h["kind"] in _ADAPTER_KINDS]
    assert {h["kind"] for h in sent} == _ADAPTER_KINDS
    for header in sent:
        frame = codec.SidecarFrame(header=header, payload=bytes(_payload_length(header)))
        reply = _exchange(runner_client, frame)
        assert reply.header["kind"] == protocol.KIND_ERROR_EVENT
        assert reply.header["message_id"] == header["message_id"]
        assert reply.header["status"] == protocol.STATUS_ERROR
        assert reply.header["error"]["code"] == protocol.ERR_UNSUPPORTED
        assert reply.header["error"]["schema_version"] == protocol.SCHEMA_VERSION
