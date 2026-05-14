"""V01-E05-F04 codec round-trip tests."""

from __future__ import annotations

import pytest

from tensorplate_pytorch_backend import codec


def test_round_trip_no_payload() -> None:
    frame = codec.SidecarFrame(header={"schema_version": "0.1", "kind": "prime", "message_id": "m"})
    blob = codec.encode(frame)
    decoded, consumed = codec.decode_one(blob)
    assert consumed == len(blob)
    assert decoded.header == frame.header
    assert decoded.payload == b""


def test_round_trip_with_payload() -> None:
    payload = b"\x00\x01\x02\x03"
    frame = codec.SidecarFrame(
        header={"schema_version": "0.1", "kind": "infer", "message_id": "m"}, payload=payload
    )
    blob = codec.encode(frame)
    decoded, _ = codec.decode_one(blob)
    assert decoded.payload == payload


def test_incomplete_prefix() -> None:
    with pytest.raises(codec.IncompleteFrame):
        codec.decode_one(b"\x00")


def test_incomplete_body() -> None:
    frame = codec.SidecarFrame(
        header={"schema_version": "0.1", "kind": "prime", "message_id": "m"}, payload=b"abcd"
    )
    blob = codec.encode(frame)
    with pytest.raises(codec.IncompleteFrame):
        codec.decode_one(blob[:-1])


def test_bad_magic_raises_frame_error() -> None:
    blob = b"\x00\x00\x00\x00" + b"\x00\x00\x00\x01" + b"\x00\x00\x00\x00" + b"\x00\x00\x00\x00"
    with pytest.raises(codec.FrameError):
        codec.decode_one(blob)


def test_unsupported_version_raises_frame_error() -> None:
    frame = codec.SidecarFrame(header={"schema_version": "0.1", "kind": "prime", "message_id": "m"})
    blob = bytearray(codec.encode(frame))
    blob[7] = 0x99  # bump wire version
    with pytest.raises(codec.FrameError):
        codec.decode_one(bytes(blob))


def test_header_size_limit_rejected() -> None:
    huge = "x" * (codec.MAX_HEADER_LEN + 1)
    frame = codec.SidecarFrame(
        header={"k": huge, "schema_version": "0.1", "kind": "prime", "message_id": "m"}
    )
    with pytest.raises(codec.FrameError):
        codec.encode(frame)


def test_multi_frame_pipeline() -> None:
    f1 = codec.encode(
        codec.SidecarFrame(header={"schema_version": "0.1", "kind": "prime", "message_id": "1"})
    )
    f2 = codec.encode(
        codec.SidecarFrame(header={"schema_version": "0.1", "kind": "prime", "message_id": "2"})
    )
    blob = f1 + f2
    decoded1, consumed1 = codec.decode_one(blob)
    assert decoded1.header["message_id"] == "1"
    decoded2, consumed2 = codec.decode_one(blob[consumed1:])
    assert decoded2.header["message_id"] == "2"
    assert consumed1 + consumed2 == len(blob)
