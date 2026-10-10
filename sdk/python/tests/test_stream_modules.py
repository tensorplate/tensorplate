"""The modules generated from the stream session schema, read against the recorded frames.

The frames under ``test/contract/fixtures/stream/v1`` are the ones the C++
bindings are tested against. Every test here skips where the ``speech`` extra
or the generated modules are absent. The CI job that installs a built wheel
beside the pins sets ``TENSORPLATE_REQUIRE_SPEECH_EXTRAS=1``, which turns
their absence into a failure.
"""

from __future__ import annotations

import importlib
import os
from collections.abc import Iterator
from concurrent import futures
from pathlib import Path
from typing import Any, NamedTuple

import pytest

_REPOSITORY = Path(__file__).resolve().parents[3]
_FRAMES = _REPOSITORY / "test/contract/fixtures/stream/v1"
_SCHEMA_ROOT = _REPOSITORY / "protocol/proto"
_SCHEMA = "tensorplate/stream/v1/session.proto"
_PACKAGE = "tensorplate.stream.v1"
_REQUIRED = os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1"

# The frames of extension.proto, each read by name below.
_ADDITIONS = (
    "added_client_fields",
    "added_server_fields",
    "added_client_body",
    "added_server_body",
    "added_mode",
    "added_client_capability",
    "added_server_capability",
)


class Frame(NamedTuple):
    name: str
    type: str
    wire: bytes
    text: str


def _need(module: str) -> Any:
    try:
        return importlib.import_module(module)
    except ImportError as error:
        if _REQUIRED:
            raise
        pytest.skip(f"{module} is not importable here: {error}")


@pytest.fixture(scope="module")
def pb() -> Any:
    return _need(f"{_PACKAGE}.session_pb2")


@pytest.fixture(scope="module")
def frames() -> list[Frame]:
    recorded = []
    for path in sorted(_FRAMES.glob("*.frames")):
        for line in path.read_text(encoding="utf-8").splitlines():
            if line and not line.startswith("#"):
                name, message_type, wire, text = line.split("\t")
                recorded.append(Frame(name, message_type, bytes.fromhex(wire), text))
    assert recorded, f"no frame under {_FRAMES}"
    return recorded


def _unknown(message: Any) -> list[tuple[int, object]]:
    fields = _need("google.protobuf.unknown_fields").UnknownFieldSet(message)
    return [(field.field_number, field.data) for field in fields]


def _parts(message: Any) -> Iterator[Any]:
    """The message and every message nested in it."""
    yield message
    for field, value in message.ListFields():
        if field.message_type is not None:
            for nested in value if field.is_repeated else [value]:
                yield from _parts(nested)


def _twice(message_class: Any, frame: Frame) -> Iterator[Any]:
    """The frame as decoded, then as decoded from its own re-encoding."""
    decoded = message_class.FromString(frame.wire)
    yield decoded
    yield message_class.FromString(decoded.SerializeToString())


def test_every_stream_frame_decodes_and_encodes_back_to_the_recorded_bytes(
    pb: Any, frames: list[Frame]
) -> None:
    text_format = _need("google.protobuf.text_format")
    message_factory = _need("google.protobuf.message_factory")
    checked = 0
    for frame in frames:
        if not frame.type.startswith(f"{_PACKAGE}."):
            continue
        descriptor = pb.DESCRIPTOR.pool.FindMessageTypeByName(frame.type)
        assert descriptor.file.name == _SCHEMA, frame.name
        message_class = message_factory.GetMessageClass(descriptor)
        decoded = message_class.FromString(frame.wire)
        assert [kept for part in _parts(decoded) for kept in _unknown(part)] == [], frame.name
        assert decoded == text_format.Parse(frame.text, message_class()), frame.name
        assert decoded.SerializeToString() == frame.wire, frame.name
        checked += 1
    assert checked == len(frames) - len(_ADDITIONS) > 0


def test_additions_the_schema_does_not_know_survive_a_decode_and_a_re_encode(
    pb: Any, frames: list[Frame]
) -> None:
    added = {frame.name: frame for frame in frames if not frame.type.startswith(f"{_PACKAGE}.")}
    assert sorted(added) == sorted(_ADDITIONS)

    for event in _twice(pb.ClientEvent, added["added_client_fields"]):
        assert event.sequence == 1
        assert event.open.resolved.deployment_id == "synthetic-deployment"
        assert event.open.resolved.generation == 7
        assert event.open.mode == pb.SESSION_MODE_STT_STREAMING
        assert _unknown(event) == [(4, 5)]
        assert _unknown(event.open) == [(10, b"synthetic addition")]

    for event in _twice(pb.ServerEvent, added["added_server_fields"]):
        assert event.sequence == 1
        assert event.ready.session_id == "synthetic-session-1"
        assert event.ready.generation == 7
        assert _unknown(event) == [(2, 5)]
        assert _unknown(event.ready) == [(10, b"synthetic addition")]

    # extension.proto's AddedBody, at a body number stream/v1 reserves.
    added_body = [(40, b"\x0a\x0esynthetic body")]
    for event in _twice(pb.ClientEvent, added["added_client_body"]):
        assert event.WhichOneof("body") is None
        assert (event.sequence, event.session_id) == (2, "synthetic-session-1")
        assert _unknown(event) == added_body
    for event in _twice(pb.ServerEvent, added["added_server_body"]):
        assert event.WhichOneof("body") is None
        assert event.sequence == 2
        assert _unknown(event) == added_body

    # An enum value the schema does not name stays the number sent, in its field.
    for event in _twice(pb.ClientEvent, added["added_mode"]):
        assert event.open.resolved.deployment_id == "synthetic-deployment"
        assert event.open.mode == 3
        assert 3 not in pb.SessionMode.values()
        assert _unknown(event) == _unknown(event.open) == []
    for event in _twice(pb.ClientEvent, added["added_client_capability"]):
        assert event.open.mode == pb.SESSION_MODE_TTS_STREAMING
        assert list(event.open.capabilities) == [1]
        assert 1 not in pb.Capability.values()
        assert _unknown(event) == _unknown(event.open) == []
    for event in _twice(pb.ServerEvent, added["added_server_capability"]):
        assert event.ready.session_id == "synthetic-session-1"
        assert list(event.ready.capabilities) == [1]
        assert _unknown(event) == _unknown(event.ready) == []


def test_the_modules_are_what_the_generator_makes_of_the_committed_schema(
    pb: Any, tmp_path: Path
) -> None:
    protoc = _need("grpc_tools.protoc")
    descriptor_pb2 = _need("google.protobuf.descriptor_pb2")
    compiled = tmp_path / "schema.binpb"
    arguments = [f"--proto_path={_SCHEMA_ROOT}", f"--descriptor_set_out={compiled}", _SCHEMA]
    assert protoc.main(["protoc", *arguments]) == 0

    [committed] = descriptor_pb2.FileDescriptorSet.FromString(compiled.read_bytes()).file
    # A generated module embeds its descriptor without the derived JSON names.
    pending = list(committed.message_type)
    while pending:
        message = pending.pop()
        pending.extend(message.nested_type)
        for field in message.field:
            field.ClearField("json_name")
    generated = descriptor_pb2.FileDescriptorProto.FromString(pb.DESCRIPTOR.serialized_pb)
    assert generated == committed


def test_the_service_module_carries_a_session_call_over_a_loopback_channel(pb: Any) -> None:
    grpc = _need("grpc")
    service = _need(f"{_PACKAGE}.session_pb2_grpc")

    class Echo:
        def Session(self, request_iterator: Any, context: Any) -> Any:
            for event in request_iterator:
                ready = pb.Ready(session_id=event.open.resolved.deployment_id)
                yield pb.ServerEvent(sequence=event.sequence, ready=ready)

    server = grpc.server(futures.ThreadPoolExecutor(max_workers=1))
    service.add_SessionServiceServicer_to_server(Echo(), server)
    port = server.add_insecure_port("localhost:0")
    server.start()
    try:
        with grpc.insecure_channel(f"localhost:{port}") as channel:
            target = pb.ResolvedTarget(deployment_id="synthetic-deployment", generation=7)
            opened = pb.ClientEvent(sequence=1, open=pb.Open(resolved=target))
            answers = list(service.SessionServiceStub(channel).Session(iter([opened]), timeout=30))
    finally:
        server.stop(grace=None)

    assert [(answer.sequence, answer.ready.session_id) for answer in answers] == [
        (1, "synthetic-deployment")
    ]
