# SPDX-License-Identifier: Apache-2.0
"""Pinned, integer-exact audio derivations for the candidate qualification tool.

Every function here is deterministic across machines: the FIR taps are a
literal Q15 table, the noise comes from a seeded integer generator, and the
G.711 mu-law transcode is integer arithmetic. The derived fixtures therefore
have stable digests the inventory can pin.
"""

from __future__ import annotations

import io
import math
import random
import struct
import wave
from pathlib import Path

RESAMPLER_ID = "tp-fir63-blackman-q15-v1"

# 63-tap windowed sinc, cutoff 0.23 of the 16 kHz rate (3,680 Hz), Blackman
# window, quantized to Q15 with the centre tap absorbing the rounding so the
# taps sum to exactly 32768. `fir_taps_from_formula` regenerates it; the test
# suite pins the table against that formula.
FIR_TAPS_Q15 = [
    0,
    0,
    -1,
    1,
    6,
    -1,
    -16,
    -3,
    31,
    16,
    -52,
    -46,
    74,
    100,
    -88,
    -186,
    79,
    307,
    -24,
    -462,
    -105,
    644,
    348,
    -837,
    -771,
    1021,
    1518,
    -1174,
    -3112,
    1275,
    10305,
    15074,
    10305,
    1275,
    -3112,
    -1174,
    1518,
    1021,
    -771,
    -837,
    348,
    644,
    -105,
    -462,
    -24,
    307,
    79,
    -186,
    -88,
    100,
    74,
    -46,
    -52,
    16,
    31,
    -3,
    -16,
    -1,
    6,
    1,
    -1,
    0,
    0,
]
FIR_TAP_COUNT = 63
FIR_CUTOFF = 0.23
FIR_DELAY = (FIR_TAP_COUNT - 1) // 2

INT16_MIN = -32768
INT16_MAX = 32767


class AudioError(Exception):
    """A clip cannot be read or derived as the inventory describes."""


def fir_taps_from_formula(tap_count: int = FIR_TAP_COUNT, cutoff: float = FIR_CUTOFF) -> list[int]:
    taps = []
    for n in range(tap_count):
        m = n - (tap_count - 1) / 2
        sinc = 2 * cutoff if m == 0 else math.sin(2 * math.pi * cutoff * m) / (math.pi * m)
        window = (
            0.42
            - 0.5 * math.cos(2 * math.pi * n / (tap_count - 1))
            + 0.08 * math.cos(4 * math.pi * n / (tap_count - 1))
        )
        taps.append(sinc * window)
    total = sum(taps)
    quantized = [round(t / total * 32768) for t in taps]
    quantized[(tap_count - 1) // 2] += 32768 - sum(quantized)
    return quantized


def clamp16(value: int) -> int:
    return INT16_MIN if value < INT16_MIN else INT16_MAX if value > INT16_MAX else value


def _round_q15(accumulator: int) -> int:
    # Round half away from zero on the Q15 accumulator.
    if accumulator >= 0:
        return (accumulator + 16384) >> 15
    return -((-accumulator + 16384) >> 15)


def _fir(samples: list[int], stride_out: int, upsample: int) -> list[int]:
    """Filter `samples` (zero-stuffed by `upsample`) and keep every `stride_out`-th output."""
    taps = FIR_TAPS_Q15
    stuffed_len = len(samples) * upsample
    output = []
    for n in range(0, stuffed_len, stride_out):
        centre = n + FIR_DELAY
        accumulator = 0
        for k, tap in enumerate(taps):
            index = centre - k
            if index < 0 or index >= stuffed_len or index % upsample:
                continue
            accumulator += tap * samples[index // upsample]
        output.append(clamp16(_round_q15(accumulator * upsample)))
    return output


def decimate_2x(samples: list[int]) -> list[int]:
    """16 kHz to 8 kHz: low-pass at 3,680 Hz, keep every second sample."""
    return _fir(samples, stride_out=2, upsample=1)


def interpolate_2x(samples: list[int]) -> list[int]:
    """8 kHz to 16 kHz: zero-stuff, low-pass at 3,680 Hz with gain two."""
    return _fir(samples, stride_out=1, upsample=2)


_ULAW_BIAS = 0x84
_ULAW_CLIP_14BIT = 8159
_ULAW_SEGMENT_ENDS = (0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF, 0x1FFF)


def ulaw_encode(sample: int) -> int:
    """G.711 mu-law as the Sun g711.c reference encodes it (the 14-bit domain, bias 0x21)."""
    value = sample >> 2
    if value < 0:
        value = -value
        mask = 0x7F
    else:
        mask = 0xFF
    value = min(value, _ULAW_CLIP_14BIT) + (_ULAW_BIAS >> 2)
    segment = next((i for i, end in enumerate(_ULAW_SEGMENT_ENDS) if value <= end), 8)
    if segment >= 8:
        return 0x7F ^ mask
    return ((segment << 4) | ((value >> (segment + 1)) & 0x0F)) ^ mask


def ulaw_decode(byte: int) -> int:
    value = ~byte & 0xFF
    sign = value & 0x80
    exponent = (value >> 4) & 0x07
    mantissa = value & 0x0F
    magnitude = ((mantissa << 3) + _ULAW_BIAS) << exponent
    magnitude -= _ULAW_BIAS
    return -magnitude if sign else magnitude


def ulaw_roundtrip(samples: list[int]) -> list[int]:
    return [ulaw_decode(ulaw_encode(s)) for s in samples]


def telephony_8k_ulaw(samples_16k: list[int]) -> list[int]:
    """The 8 kHz mu-law telephony derivation: decimate, then transcode through G.711."""
    return ulaw_roundtrip(decimate_2x(samples_16k))


def integer_rms(samples: list[int]) -> int:
    if not samples:
        return 0
    return math.isqrt(sum(s * s for s in samples) // len(samples))


def seeded_noise_mix(samples: list[int], *, seed: int, snr_db: int) -> list[int]:
    """Add seeded, integer-generated noise at `snr_db` below the clip's RMS."""
    if not samples:
        raise AudioError("cannot mix noise into an empty clip")
    clean_rms = integer_rms(samples)
    if clean_rms == 0:
        raise AudioError("cannot set a signal-to-noise ratio on a silent clip")
    generator = random.Random(seed)
    # Sum of twelve 16-bit uniforms: near-Gaussian, integer-exact, platform-stable.
    raw = [sum(generator.getrandbits(16) for _ in range(12)) - 12 * 32768 for _ in samples]
    raw_rms = integer_rms(raw)
    # Target RMS in thousandths to keep the scale integer: rms / 10^(snr/20).
    target_milli = round(clean_rms * 1000 / (10 ** (snr_db / 20)))
    return [
        clamp16(s + (n * target_milli) // (raw_rms * 1000))
        for s, n in zip(samples, raw, strict=True)
    ]


def read_wav_pcm16_mono(path: Path) -> tuple[int, list[int]]:
    try:
        with wave.open(str(path), "rb") as handle:
            if (
                handle.getnchannels() != 1
                or handle.getsampwidth() != 2
                or handle.getcomptype() != "NONE"
            ):
                raise AudioError(f"{path.name}: not a mono 16-bit PCM WAV file")
            rate = handle.getframerate()
            frames = handle.readframes(handle.getnframes())
    except (wave.Error, EOFError, OSError) as exc:
        raise AudioError(f"{path.name}: unreadable WAV file ({exc.__class__.__name__})") from exc
    return rate, list(struct.unpack(f"<{len(frames) // 2}h", frames))


def wav_pcm16_mono_bytes(rate: int, samples: list[int]) -> bytes:
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(rate)
        handle.writeframes(pcm16_bytes(samples))
    return buffer.getvalue()


def pcm16_bytes(samples: list[int]) -> bytes:
    return struct.pack(f"<{len(samples)}h", *samples)


def pcm16_samples(data: bytes) -> list[int]:
    if len(data) % 2:
        raise AudioError("PCM16 payload has an odd byte count")
    return list(struct.unpack(f"<{len(data) // 2}h", data))


def duration_us(sample_count: int, rate: int) -> int:
    return -(-sample_count * 1_000_000 // rate)
