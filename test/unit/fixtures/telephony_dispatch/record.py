"""Record the 8 kHz telephony derivation of the speech dispatch's 16 kHz pattern.

Usage, from the repository root:
    python3 test/unit/fixtures/telephony_dispatch/record.py \
        test/unit/fixtures/telephony_dispatch/speech_pattern.json

The pattern is integer triangle tones between quiet runs, which
test/unit/speech_telephony_input_test.cpp builds again from the segments
recorded here; the FNV-1a digest of its 16-bit little-endian bytes lets the
case check that it built the same samples. The derivation is the telephony
recipe of tools/validation/candidate_audio.py, as the wire carries it.
"""

from __future__ import annotations

import json
import platform
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(ROOT / "tools" / "validation"))

import candidate_audio as audio  # noqa: E402

_AMPLITUDE = 24000
_QUIET_AMPLITUDE = 64
_QUIET_PERIOD = 50
# (tone period, samples) at 16 kHz; a period of 0 is a quiet run. Odd lengths
# put the edges between samples of the 8 kHz derivation.
_SEGMENTS = [
    (0, 6000),
    (40, 8000),
    (0, 3203),
    (64, 7777),
    (0, 9000),
    (20, 12345),
    (0, 7001),
    (32, 4321),
    (0, 1111),
]


def _triangle(n: int, period: int, amplitude: int) -> int:
    phase = n % period
    if phase < period // 2:
        return -amplitude + 4 * amplitude * phase // period
    return 3 * amplitude - 4 * amplitude * phase // period


def _pattern() -> list[int]:
    samples: list[int] = []
    for period, count in _SEGMENTS:
        if period == 0:
            samples.extend(_triangle(n, _QUIET_PERIOD, _QUIET_AMPLITUDE) for n in range(count))
        else:
            samples.extend(_triangle(n, period, _AMPLITUDE) for n in range(count))
    return samples


def _fnv1a64(data: bytes) -> str:
    digest = 0xCBF29CE484222325
    for byte in data:
        digest = ((digest ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{digest:016x}"


def main() -> None:
    pcm = _pattern()
    if len(pcm) % 2 != 0:
        sys.exit("the pattern must cover a whole number of 8 kHz samples")
    if max(abs(s) for s in pcm) > audio.INT16_MAX:
        sys.exit("the pattern leaves the 16-bit range")
    record = {
        "rate_hz": 16000,
        "amplitude": _AMPLITUDE,
        "quiet_amplitude": _QUIET_AMPLITUDE,
        "quiet_period": _QUIET_PERIOD,
        "segments": [{"tone_period": period, "samples": count} for period, count in _SEGMENTS],
        "pcm_s16le_fnv1a64": _fnv1a64(b"".join(struct.pack("<h", s) for s in pcm)),
        "telephony": {
            "recipe_id": audio.TELEPHONY_RECIPE_ID,
            "mulaw_hex": audio.telephony_8k_ulaw_bytes(pcm).hex(),
        },
        "recorded_with": {
            "python": platform.python_version(),
            "reference": "tools/validation/candidate_audio.py",
        },
    }
    Path(sys.argv[1]).write_text(json.dumps(record, indent=1) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
