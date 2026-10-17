"""Record what the pinned telephony reference gives for the C++ resampler's cases.

Usage, from the repository root, with a CPython that still has audioop (3.12):
    python3 test/unit/fixtures/telephony_resampler/record.py \
        test/unit/fixtures/telephony_resampler/tp_fir63_blackman_q15_v1.json

The reference is tools/validation/candidate_audio.py: its G.711 decoder, its
literal Q15 taps and interpolate_2x over each whole input. The decoder is
checked against audioop.ulaw2lin over all 256 bytes, and the taps against the
formula they were quantized from, before anything is written.
"""

from __future__ import annotations

import json
import math
import platform
import random
import struct
import sys
import warnings
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(ROOT / "tools" / "validation"))

import candidate_audio as audio  # noqa: E402

with warnings.catch_warnings():
    warnings.simplefilter("ignore", DeprecationWarning)
    import audioop  # removed in Python 3.13

_RATE = 8000
_SAMPLES = 2000
_FLUSH_AFTER = 1000


def _pcm_hex(samples: list[int]) -> str:
    return b"".join(struct.pack("<h", s) for s in samples).hex()


def _resampled(codes: bytes) -> list[int]:
    resampled: list[int] = audio.interpolate_2x([audio.ulaw_decode(c) for c in codes])
    return resampled


def _gained_accumulators(samples: list[int]) -> list[int]:
    """The reference filter's accumulator times its gain, before rounding, for each output."""
    stuffed = 2 * len(samples)
    gained = []
    for n in range(stuffed):
        accumulator = 0
        for k, tap in enumerate(audio.FIR_TAPS_Q15):
            index = n + audio.FIR_DELAY - k
            if 0 <= index < stuffed and index % 2 == 0:
                accumulator += tap * samples[index // 2]
        gained.append(accumulator * 2)
    return gained


def _noise() -> bytes:
    rng = random.Random(8000)
    return bytes(rng.randrange(256) for _ in range(_SAMPLES))


def _tones() -> bytes:
    def sample(n: int) -> int:
        t = n / _RATE
        return round(
            12000 * math.sin(2 * math.pi * 300 * t)
            + 8000 * math.sin(2 * math.pi * 1000 * t)
            + 4000 * math.sin(2 * math.pi * 3400 * t)
        )

    return bytes(audio.ulaw_encode(sample(n)) for n in range(_SAMPLES))


def _case(name: str, codes: bytes) -> dict[str, object]:
    pcm = _resampled(codes)
    gained = _gained_accumulators([audio.ulaw_decode(c) for c in codes])
    rounded = [-((-g + 16384) >> 15) if g < 0 else (g + 16384) >> 15 for g in gained]
    if [audio.clamp16(r) for r in rounded] != pcm:
        sys.exit("the accumulator replica differs from the reference")
    ties = [g for g in gained if g % 32768 == 16384]
    return {
        "name": name,
        "mulaw_hex": codes.hex(),
        "pcm_s16le_hex": _pcm_hex(pcm),
        "rounding_ties": {
            "positive": sum(g > 0 for g in ties),
            "negative": sum(g < 0 for g in ties),
        },
        "clamped_outputs": {
            "above": sum(r > audio.INT16_MAX for r in rounded),
            "below": sum(r < audio.INT16_MIN for r in rounded),
        },
    }


def main() -> None:
    table = [audio.ulaw_decode(c) for c in range(256)]
    if table != list(struct.unpack("<256h", audioop.ulaw2lin(bytes(range(256)), 2))):
        sys.exit("the reference mu-law decoder differs from audioop.ulaw2lin")
    if audio.fir_taps_from_formula() != audio.FIR_TAPS_Q15 or sum(audio.FIR_TAPS_Q15) != 32768:
        sys.exit("the reference taps differ from their formula")
    noise = _noise()
    cases = [_case("noise", noise), _case("tones", _tones()), _case("short", noise[:7])]
    ties, clamped = cases[0]["rounding_ties"], cases[0]["clamped_outputs"]
    assert isinstance(ties, dict)
    assert isinstance(clamped, dict)
    if 0 in (*ties.values(), *clamped.values()):
        sys.exit("the noise case misses a rounding tie or a clamp of one sign")
    record = {
        "resampler_id": audio.RESAMPLER_ID,
        "taps_q15": audio.FIR_TAPS_Q15,
        "mulaw_to_linear": table,
        "cases": cases,
        "flushed_prefix": {
            "case": "noise",
            "inputs": _FLUSH_AFTER,
            "pcm_s16le_hex": _pcm_hex(_resampled(noise[:_FLUSH_AFTER])),
        },
        "recorded_with": {
            "python": platform.python_version(),
            "reference": "tools/validation/candidate_audio.py",
        },
    }
    Path(sys.argv[1]).write_text(json.dumps(record, indent=1) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
