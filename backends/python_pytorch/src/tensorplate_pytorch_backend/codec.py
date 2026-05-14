"""Sidecar IPC frame codec (Python side).

Mirror of ``runtime/src/ipc/sidecar_codec.cpp``. Each frame on the wire
is::

    +-----+-----+-----+------+--------------+-----------------+
    | u32 | u32 | u32 |  u32 | header_bytes | payload_bytes   |
    | mag | ver | hdr | pld  | (JSON body)  | (raw tensors)   |
    +-----+-----+-----+------+--------------+-----------------+

All four ``u32`` fields are big-endian. The schema for the JSON header
lives in ``protocol/schemas/python_pytorch_ipc.json``; this module is
deliberately schema-agnostic so the runner can stay alive even if a
peer sends a JSON header with extra fields.

The codec is small enough (and the message shapes well-defined enough)
that it has no third-party dependency.
"""

from __future__ import annotations

import json
import struct
from dataclasses import dataclass
from typing import Any

PROTOCOL_WIRE_VERSION: int = 1
FRAME_MAGIC: int = 0x54505343  # 'TPSC'
FRAME_PREFIX_BYTES: int = 16
MAX_HEADER_LEN: int = 1 << 20
MAX_PAYLOAD_LEN: int = 256 << 20

_PREFIX_FMT = "!IIII"


class FrameError(ValueError):
    """Raised when an incoming frame violates the wire protocol.

    Distinct from ``IncompleteFrame`` so callers can decide whether to
    drop the connection or wait for more bytes.
    """


class IncompleteFrame(ValueError):
    """Raised when the byte buffer is a strict prefix of a valid frame.

    Callers should read more bytes from the socket and retry.
    """


@dataclass(slots=True)
class SidecarFrame:
    header: dict[str, Any]
    payload: bytes = b""


def encode(frame: SidecarFrame) -> bytes:
    """Encode ``frame`` into the on-wire byte representation."""
    header_bytes = json.dumps(frame.header, separators=(",", ":"), ensure_ascii=False).encode(
        "utf-8"
    )
    if len(header_bytes) > MAX_HEADER_LEN:
        raise FrameError(f"sidecar frame header exceeds {MAX_HEADER_LEN}-byte limit")
    if len(frame.payload) > MAX_PAYLOAD_LEN:
        raise FrameError(f"sidecar frame payload exceeds {MAX_PAYLOAD_LEN}-byte limit")
    prefix = struct.pack(
        _PREFIX_FMT,
        FRAME_MAGIC,
        PROTOCOL_WIRE_VERSION,
        len(header_bytes),
        len(frame.payload),
    )
    return prefix + header_bytes + frame.payload


def decode_one(buf: bytes) -> tuple[SidecarFrame, int]:
    """Decode the first frame from ``buf`` and return ``(frame, bytes_consumed)``.

    Raises:
        IncompleteFrame: ``buf`` is a strict prefix of a valid frame.
        FrameError: ``buf`` is malformed (bad magic / bad version /
            field exceeds maximum).
    """
    if len(buf) < FRAME_PREFIX_BYTES:
        raise IncompleteFrame("waiting on prefix")
    magic, version, hdr_len, pld_len = struct.unpack_from(_PREFIX_FMT, buf, 0)
    if magic != FRAME_MAGIC:
        raise FrameError(f"sidecar frame magic 0x{magic:08x} != expected 0x{FRAME_MAGIC:08x}")
    if version != PROTOCOL_WIRE_VERSION:
        raise FrameError(
            f"sidecar protocol wire version {version} != supported {PROTOCOL_WIRE_VERSION}"
        )
    if hdr_len > MAX_HEADER_LEN:
        raise FrameError(f"sidecar frame header_len={hdr_len} exceeds {MAX_HEADER_LEN}")
    if pld_len > MAX_PAYLOAD_LEN:
        raise FrameError(f"sidecar frame payload_len={pld_len} exceeds {MAX_PAYLOAD_LEN}")

    total = FRAME_PREFIX_BYTES + hdr_len + pld_len
    if len(buf) < total:
        raise IncompleteFrame("waiting on body")

    header_bytes = buf[FRAME_PREFIX_BYTES : FRAME_PREFIX_BYTES + hdr_len]
    payload_bytes = buf[FRAME_PREFIX_BYTES + hdr_len : total]

    try:
        header = json.loads(header_bytes.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise FrameError(f"sidecar frame header is not valid UTF-8 JSON: {exc}") from exc
    if not isinstance(header, dict):
        raise FrameError("sidecar frame header must be a JSON object")
    return SidecarFrame(header=header, payload=bytes(payload_bytes)), total


__all__ = [
    "FRAME_MAGIC",
    "FRAME_PREFIX_BYTES",
    "MAX_HEADER_LEN",
    "MAX_PAYLOAD_LEN",
    "PROTOCOL_WIRE_VERSION",
    "FrameError",
    "IncompleteFrame",
    "SidecarFrame",
    "decode_one",
    "encode",
]
