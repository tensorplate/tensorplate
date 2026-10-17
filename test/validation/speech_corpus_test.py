#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check the speech corpus renderings and the manifest check.

The renderings are measured from outside: the signal-to-noise ratio and the
telephony level are recomputed from the output samples, and the decimator's
timing from an impulse. A small corpus is then synthesized in a temporary
directory, rendered with the tool itself, and each rule of the manifest check
is broken once with a refusal that has to name that rule.
"""

from __future__ import annotations

import copy
import hashlib
import io
import json
import math
import os
import random
import subprocess
import sys
import tempfile
from collections.abc import Callable
from contextlib import redirect_stdout
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools/validation/speech_corpus.py"
sys.path.insert(0, str(ROOT / "tools/validation"))

import candidate_audio as audio  # noqa: E402
import jsonschema  # noqa: E402
import speech_corpus as corpus  # noqa: E402

VERSION = "synthetic-0"


def sine_burst(total: int, onset: int, end: int, amplitude: int, hz: int) -> list[int]:
    return [
        round(amplitude * math.sin(2 * math.pi * hz * i / 16000)) if onset <= i < end else 0
        for i in range(total)
    ]


def seeded_noise(count: int, seed: int, amplitude: int) -> list[int]:
    generator = random.Random(seed)
    return [generator.randint(-amplitude, amplitude) for _ in range(count)]


def snr_db(clean: list[int], mixed: list[int], onset: int, end: int) -> float:
    noise = [m - c for c, m in zip(clean, mixed, strict=True)]
    return 10 * math.log10(
        corpus.span_power(clean, onset, end) / corpus.span_power(noise, onset, end)
    )


def check_arithmetic() -> None:
    assert corpus.iroot(10**40, 20) == 100 and corpus.iroot(10**40 - 1, 20) == 99
    generator = random.Random(1)
    for _ in range(200):
        value, k = generator.getrandbits(generator.randint(1, 900)), generator.randint(2, 20)
        root = corpus.iroot(value, k)
        assert root**k <= value < (root + 1) ** k, (value, k, root)
    assert corpus.gain_q30(1, 1, 0) == 1 << 30
    assert corpus.gain_q30(4, 1, 0) == 1 << 31
    assert corpus.gain_q30(1, 1, -20) == (1 << 30) // 10
    assert corpus.gain_q30(1, 1, 20) == 10 << 30
    for db in (-26, -15, -5, 5, 15):
        exact = 10 ** (db / 20) * (1 << 30)
        assert abs(corpus.gain_q30(7, 3, db) - math.sqrt(7 / 3) * exact) <= 1, db
    assert [corpus.apply_gain(v, 3 << 29) for v in (1, -1, 3, -3)] == [2, -2, 5, -5]
    assert corpus.noise_offset(2**64 - 1, 52000, 48000) == (2**64 - 1) % 4001
    rule = hashlib.sha256(b"v1/noisy16k/en-a.clean16k").digest()[:8]
    assert corpus.noisy_seed("v1", "en-a.clean16k") == int.from_bytes(rule, "big")


def check_snr_assignment() -> None:
    def clip(clip_id: str, pool: str, language: str, bin_: str, **kw: str) -> dict[str, Any]:
        entry = {"id": clip_id, "pool": pool, "language": language, "duration_bin": bin_}
        entry.update(codec="clean16k", suite="speech")
        entry.update(kw)
        return entry

    files = [
        clip("e", "held_out", "en", "1to5s"),
        clip("b", "held_out", "en", "1to5s"),
        clip("d", "held_out", "en", "1to5s"),
        clip("a", "held_out", "en", "1to5s"),
        clip("c", "held_out", "en", "1to5s"),
        clip("f", "held_out", "en", "lt1s"),
        clip("g", "held_out", "ar", "1to5s"),
        clip("h", "tuning", "en", "1to5s"),
        clip("s", "held_out", "en", "1to5s", suite="silence"),
        clip("n", "held_out", "en", "1to5s", codec="noisy16k"),
    ]
    assert corpus.assigned_snr(files) == {
        "a": 5,
        "b": 10,
        "c": 15,
        "d": 20,
        "e": 5,
        "f": 5,
        "g": 5,
        "h": 5,
    }


def check_bins_and_labels() -> None:
    for samples, expected in (
        (15999, "lt1s"),
        (16000, "1to5s"),
        (80000, "1to5s"),
        (80001, "gt5to30s"),
        (480000, "gt5to30s"),
    ):
        assert corpus.duration_bin(samples) == expected, samples
    try:
        corpus.duration_bin(480001)
    except corpus.ManifestError as exc:
        assert "longer than 30 s" in str(exc)
    else:
        raise AssertionError("31 s of speech has a bin")
    assert corpus.telephony_labels(3, 7) == (1, 4)
    assert corpus.telephony_labels(4, 8) == (2, 4)
    # The decimator is zero-phase: an impulse at 16 kHz sample 2k peaks at 8 kHz sample k,
    # and one between two output samples lands on both equally.
    for position in (64, 1000, 1001):
        impulse = [0] * 2048
        impulse[position] = 20000
        out = audio.decimate_2x(impulse)
        assert len(out) == 1024
        peak = max(range(len(out)), key=lambda i: abs(out[i]))
        if position % 2 == 0:
            assert peak == position // 2, (position, peak)
        else:
            assert out[position // 2] == out[position // 2 + 1], position


def check_noisy() -> None:
    onset, end = 24000, 40000
    # Not silent outside the labels, so speech measured over the whole clip would miss.
    hum = seeded_noise(48000, 6, 2000)
    clean = [
        s if onset <= i < end else n
        for i, (s, n) in enumerate(zip(sine_burst(48000, onset, end, 8000, 440), hum, strict=True))
    ]
    noise = seeded_noise(100000, 3, 3000)
    for snr in (5, 10, 15, 20):
        mixed, made = corpus.render_noisy(clean, onset, end, noise, snr_db=snr, seed=11)
        assert abs(snr_db(clean, mixed, onset, end) - snr) < 0.01, snr
        assert made["mixture_peak"] <= audio.INT16_MAX
    again, made_again = corpus.render_noisy(clean, onset, end, noise, snr_db=20, seed=11)
    assert (again, made_again) == (mixed, made)
    _, other = corpus.render_noisy(clean, onset, end, noise, snr_db=20, seed=12)
    assert other["noise_offset_sample"] != made["noise_offset_sample"]
    _, fitted = corpus.render_noisy(clean, onset, end, noise[:48000], snr_db=20, seed=12)
    assert fitted["noise_offset_sample"] == 0

    # A mixture past 16 bits is scaled as a whole, never clipped.
    loud = sine_burst(48000, onset, end, 32000, 440)
    scaled, made = corpus.render_noisy(loud, onset, end, noise, snr_db=5, seed=11)
    peak = made["mixture_peak"]
    assert peak > audio.INT16_MAX and max(abs(v) for v in scaled) == audio.INT16_MAX
    excerpt = noise[made["noise_offset_sample"] : made["noise_offset_sample"] + len(loud)]
    gain = corpus.gain_q30(
        corpus.span_power(loud, onset, end), corpus.span_power(excerpt, onset, end), -5
    )
    unscaled = [s + corpus.apply_gain(n, gain) for s, n in zip(loud, excerpt, strict=True)]
    assert max(abs(v) for v in unscaled) == peak
    assert all(
        abs(out * peak - raw * audio.INT16_MAX) <= peak
        for out, raw in zip(scaled, unscaled, strict=True)
    )

    for args, reason in (
        ((clean, onset, end, noise[:47999]), "fewer than the clip's"),
        ((clean, onset, end, [0] * 48000), "silent under the labelled speech"),
        (([0] * 48000, onset, end, noise), "the labelled speech is silent"),
    ):
        try:
            corpus.render_noisy(*args, snr_db=10, seed=1)
        except audio.AudioError as exc:
            assert reason in str(exc), (reason, exc)
        else:
            raise AssertionError(f"rendered: {reason}")


def check_telephony() -> None:
    onset, end = 16001, 40001
    # Loud enough outside the labels that levelling the whole clip would miss the target.
    hum = seeded_noise(48000, 4, 600)
    clean = [
        s if onset <= i < end else n
        for i, (s, n) in enumerate(zip(sine_burst(48000, onset, end, 1000, 400), hum, strict=True))
    ]
    data, made = corpus.render_telephony(clean, (onset, end))
    level = corpus.achieved_dbov(made["levelled_span_power"], end - onset)
    assert len(data) == 24000 and abs(level - corpus.LEVEL_DBOV) < 0.02
    decoded = [audio.ulaw_decode(b) for b in data]
    low, high = corpus.telephony_labels(onset, end)
    # Measured on the wire: a 400 Hz tone passes the decimator and mu-law nearly unchanged.
    assert abs(corpus.level_dbov(decoded, low, high) - corpus.LEVEL_DBOV) < 0.3
    assert data == audio.telephony_8k_ulaw_bytes(
        [corpus.apply_gain(s, made["level_gain_q30"]) for s in clean]
    )

    # A gain that would push a sample past 16 bits is lowered to fit, whether the
    # largest sample is inside the labels or, as here, outside them.
    spiky = sine_burst(48000, onset, end, 300, 400)
    spiky[30000] = 12000
    spiky[5000] = -20000
    data, made = corpus.render_telephony(spiky, (onset, end))
    assert made["level_gain_q30"] == (audio.INT16_MAX << corpus.GAIN_BITS) // 20000
    assert corpus.achieved_dbov(made["levelled_span_power"], end - onset) < corpus.LEVEL_DBOV

    hiss = seeded_noise(16000, 5, 50)
    data, made = corpus.render_telephony(hiss, None)
    assert made == {"level_gain_q30": None, "levelled_span_power": None}
    assert data == audio.telephony_8k_ulaw_bytes(hiss)
    try:
        corpus.render_telephony([0] * 16000, (0, 16000))
    except audio.AudioError as exc:
        assert "silent" in str(exc)
    else:
        raise AssertionError("levelled silent speech")


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Corpus:
    """A small synthesized corpus on disk and the manifest that describes it."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.files: list[dict[str, Any]] = []
        self.noise: list[dict[str, Any]] = []
        self.samples: dict[str, list[int]] = {}
        for noise_id, pool, excluded, seed, length in (
            ("noise-tuning-1", "tuning", False, 21, 120000),
            ("noise-held-1", "held_out", False, 22, 120000),
            ("noise-held-2", "held_out", True, 23, 120000),
            # Exactly as long as en-a, so its excerpt starts at 0 and ends at the last sample.
            ("noise-held-exact", "held_out", False, 24, 48000),
        ):
            samples = seeded_noise(length, seed, 2000)
            data = audio.wav_pcm16_mono_bytes(16000, samples)
            path = f"noise/{noise_id}.wav"
            self.write(path, data)
            self.samples[noise_id] = samples
            self.noise.append(
                {
                    "id": noise_id,
                    "source": "noise-src",
                    "file_id": f"{noise_id}.wav",
                    "path": path,
                    "sha256": sha(data),
                    "size": len(data),
                    "samples": len(samples),
                    "pool": pool,
                    "excluded": excluded,
                    "exclusion_reason": "speech heard by a listener" if excluded else None,
                }
            )
        self.clean("en-a", "held_out", "en", "read", (8000, 40000), 48000, "s1", "one two")
        self.clean("ar-b", "held_out", "ar", "crowd", (4000, 12000), 16000, None, None)
        self.clean("en-c", "tuning", "en", "read", (3001, 99001), 104000, "s2", "three four")
        self.clean("sil-d", "held_out", None, "read", None, 20001, None, None)
        self.clean("en-e", "held_out", "en", "read", (8000, 36000), 48000, "s3", "five six")
        snr = corpus.assigned_snr(self.files)
        assert corpus.assigned_snr(list(reversed(self.files))) == snr
        # en-a and en-e share a pool, language and bin, so they take the first two SNRs.
        assert (snr["en-a.clean16k"], snr["en-e.clean16k"], snr["en-c.clean16k"]) == (5, 10, 5)
        for clip_id, noise_id in (
            ("en-a", "noise-held-exact"),
            ("ar-b", "noise-held-1"),
            ("en-c", "noise-tuning-1"),
            ("en-e", "noise-held-1"),
        ):
            self.noisy(clip_id, snr[f"{clip_id}.clean16k"], noise_id)
        for clip_id in ("en-a", "ar-b", "en-c", "sil-d"):
            self.telephony(clip_id)

    def write(self, path: str, data: bytes) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)

    def by_id(self, clip_id: str) -> dict[str, Any]:
        return next(f for f in self.files if f["id"] == clip_id)

    def clean(
        self,
        clip_id: str,
        pool: str,
        language: str | None,
        source: str,
        span: tuple[int, int] | None,
        total: int,
        speaker: str | None,
        text: str | None,
    ) -> None:
        if span is None:
            samples = seeded_noise(total, 9, 40)
        else:
            samples = sine_burst(total, *span, 6000, 300)
        data = audio.wav_pcm16_mono_bytes(16000, samples)
        name = f"{clip_id}.clean16k"
        path = f"{pool}/{name}.wav"
        self.write(path, data)
        self.samples[name] = samples
        reference = text if text is not None else f"private reference of {clip_id}"
        self.files.append(
            {
                "id": name,
                "path": path,
                "sha256": sha(data),
                "pcm_sha256": sha(audio.pcm16_bytes(samples)),
                "size": len(data),
                "samples": len(samples),
                "rate": 16000,
                "encoding": "pcm_s16le_wav",
                "suite": "speech" if span else "silence",
                "pool": pool,
                "codec": "clean16k",
                "language": language,
                "duration_bin": None if span is None else corpus.duration_bin(span[1] - span[0]),
                "source": {"source": source, "file_ids": [f"{clip_id}.src"], "speaker": speaker},
                "derived_from": None,
                "transcript": None
                if span is None
                else {"sha256": sha(reference.encode()), "text": text},
                "labels": None
                if span is None
                else {
                    "speech_onset_sample": span[0],
                    "speech_end_sample": span[1],
                    "method": "synthetic",
                    "tool": "speech_corpus_test.py",
                    "human_verified": False,
                },
                "recipe": None,
                "semantic": ["numerals"] if text else [],
            }
        )

    def derived(self, clip_id: str, codec: str, data: bytes, samples: int) -> dict[str, Any]:
        clean = self.by_id(f"{clip_id}.clean16k")
        name = f"{clip_id}.{codec}"
        path = f"{clean['pool']}/{name}." + ("ul" if codec == "telephony8k" else "wav")
        self.write(path, data)
        entry = copy.deepcopy(clean)
        entry.update(
            id=name,
            path=path,
            sha256=sha(data),
            size=len(data),
            samples=samples,
            codec=codec,
            source=None,
            derived_from=clean["id"],
        )
        if clean["labels"] is not None:
            onset = clean["labels"]["speech_onset_sample"]
            end = clean["labels"]["speech_end_sample"]
            if codec == "telephony8k":
                onset, end = corpus.telephony_labels(onset, end)
            entry["labels"] = {
                "speech_onset_sample": onset,
                "speech_end_sample": end,
                "method": "inherited",
                "tool": None,
                "human_verified": False,
            }
        self.files.append(entry)
        return entry

    def noisy(self, clip_id: str, snr: int, noise_id: str) -> None:
        clean = self.by_id(f"{clip_id}.clean16k")
        labels = clean["labels"]
        seed = corpus.noisy_seed(VERSION, clean["id"])
        mixed, made = corpus.render_noisy(
            self.samples[clean["id"]],
            labels["speech_onset_sample"],
            labels["speech_end_sample"],
            self.samples[noise_id],
            snr_db=snr,
            seed=seed,
        )
        entry = self.derived(
            clip_id, "noisy16k", audio.wav_pcm16_mono_bytes(16000, mixed), len(mixed)
        )
        entry["pcm_sha256"] = sha(audio.pcm16_bytes(mixed))
        entry["recipe"] = {"kind": "noisy16k", "noise": noise_id, "snr_db": snr, "seed": seed}
        entry["recipe"].update(made)

    def telephony(self, clip_id: str) -> None:
        clean = self.by_id(f"{clip_id}.clean16k")
        labels = clean["labels"]
        span = (
            None if labels is None else (labels["speech_onset_sample"], labels["speech_end_sample"])
        )
        data, made = corpus.render_telephony(self.samples[clean["id"]], span)
        entry = self.derived(clip_id, "telephony8k", data, len(data))
        entry.update(rate=8000, encoding="mulaw", pcm_sha256=sha(data))
        entry["recipe"] = {"kind": "telephony8k", "sv56_dbov": None}
        entry["recipe"].update(made)

    def manifest(self) -> dict[str, Any]:
        return {
            "schema_version": "0.1",
            "corpus": {"name": "synthetic", "version": VERSION},
            "recipes": {
                "telephony8k": {
                    "recipe": audio.TELEPHONY_RECIPE_ID,
                    "level_dbov": -26,
                    "level_method": "span_rms",
                },
                "noisy16k": {
                    "snr_db": [5, 10, 15, 20],
                    "snr_rule": "round-robin-sorted-clean-id-v1",
                    "seed_rule": "sha256-version-noisy16k-clip-v1",
                },
                "tools": [
                    {"path": path, "sha256": sha((ROOT / path).read_bytes())}
                    for path in corpus.TOOLS
                ],
            },
            "sources": [
                self.source("read", True),
                self.source("crowd", False),
                self.source("noise-src", True),
            ],
            "noise": copy.deepcopy(self.noise),
            "files": copy.deepcopy(self.files),
        }

    @staticmethod
    def source(source_id: str, public: bool) -> dict[str, Any]:
        return {
            "id": source_id,
            "dataset": f"synthetic {source_id}",
            "revision": "0",
            "url": "https://example.invalid/synthetic",
            "license": "CC0-1.0",
            "attribution": "generated by the test",
            "condition": "synthetic tones",
            "transcripts_public": public,
        }


Mutation = Callable[[dict[str, Any]], None]


def file_entry(manifest: dict[str, Any], file_id: str) -> dict[str, Any]:
    return next(f for f in manifest["files"] if f["id"] == file_id)


def set_field(file_id: str, *path_and_value: Any) -> Mutation:
    *path, value = path_and_value

    def mutate(manifest: dict[str, Any]) -> None:
        node = file_entry(manifest, file_id)
        for key in path[:-1]:
            node = node[key]
        node[path[-1]] = value

    return mutate


def refused(
    good: dict[str, Any], mutate: Mutation, reason: str, *, root: Path | None = None, **kw: Any
) -> None:
    broken = copy.deepcopy(good)
    mutate(broken)
    try:
        corpus.check_manifest(broken, root=root, **kw)
    except corpus.ManifestError as exc:
        assert reason in str(exc), f"refused for another reason: expected {reason!r}, got {exc}"
    else:
        raise AssertionError(f"accepted a manifest that should be refused: {reason}")


def check_rules(good: dict[str, Any], root: Path) -> None:
    def drop_tool(m: dict[str, Any]) -> None:
        m["recipes"]["tools"].pop()

    def stale_tool(m: dict[str, Any]) -> None:
        m["recipes"]["tools"][0]["sha256"] = "0" * 64

    def duplicate_id(m: dict[str, Any]) -> None:
        m["files"][1]["id"] = m["files"][0]["id"]

    def duplicate_path(m: dict[str, Any]) -> None:
        m["files"][0]["path"] = m["noise"][0]["path"]

    def unknown_source(m: dict[str, Any]) -> None:
        file_entry(m, "en-a.clean16k")["source"]["source"] = "absent"

    def unknown_noise_source(m: dict[str, Any]) -> None:
        m["noise"][0]["source"] = "absent"

    def reasonless_exclusion(m: dict[str, Any]) -> None:
        m["noise"][2]["exclusion_reason"] = None

    def shared_source_file(m: dict[str, Any]) -> None:
        file_entry(m, "en-c.clean16k")["source"]["file_ids"] = ["en-a.src"]

    def shared_speaker(m: dict[str, Any]) -> None:
        file_entry(m, "ar-b.clean16k")["source"]["speaker"] = "s2"

    def shared_noise_file(m: dict[str, Any]) -> None:
        m["noise"][0]["file_id"] = m["noise"][1]["file_id"]

    def silence_from_tuning_noise(m: dict[str, Any]) -> None:
        file_entry(m, "sil-d.clean16k")["source"] = {
            "source": "noise-src",
            "file_ids": ["noise-tuning-1.wav"],
            "speaker": None,
        }

    def shorten_exact_noise(m: dict[str, Any]) -> None:
        next(n for n in m["noise"] if n["id"] == "noise-held-exact")["samples"] = 47999

    def same_samples(m: dict[str, Any]) -> None:
        file_entry(m, "ar-b.clean16k")["pcm_sha256"] = file_entry(m, "en-a.clean16k")["pcm_sha256"]

    def shared_transcript(m: dict[str, Any]) -> None:
        for name in ("en-c.clean16k", "en-c.noisy16k", "en-c.telephony8k"):
            file_entry(m, name)["transcript"] = file_entry(m, "en-a.clean16k")["transcript"]

    long_labels = {
        "speech_onset_sample": 0,
        "speech_end_sample": 480001,
        "method": "synthetic",
        "tool": "t",
        "human_verified": False,
    }
    cases: list[tuple[Mutation, str]] = [
        (drop_tool, "recipe tools are"),
        (stale_tool, "rendered with digest"),
        (duplicate_id, "appears twice"),
        (duplicate_path, "path"),
        (unknown_source, "source 'absent' is not listed"),
        (unknown_noise_source, "source 'absent' is not listed"),
        (reasonless_exclusion, "an exclusion has a reason"),
        (set_field("en-a.telephony8k", "encoding", "pcm_s16le_wav"), "is mulaw at 8000 Hz"),
        (set_field("en-a.noisy16k", "rate", 8000), "is pcm_s16le_wav at 16000 Hz"),
        (set_field("en-a.noisy16k", "derived_from", None), "a clean file names its source"),
        (set_field("en-a.clean16k", "derived_from", "ar-b.clean16k"), "a clean file names"),
        (
            set_field("en-a.noisy16k", "source", good["files"][0]["source"]),
            "a clean file names its source",
        ),
        (
            set_field("en-a.clean16k", "recipe", file_entry(good, "en-a.noisy16k")["recipe"]),
            "carries recipe",
        ),
        (set_field("en-a.clean16k", "labels", None), "labels is required for speech"),
        (set_field("sil-d.clean16k", "language", "en"), "language is required for speech"),
        (set_field("sil-d.clean16k", "semantic", ["negation"]), "silence carries no semantic"),
        (set_field("en-a.clean16k", "labels", "speech_end_sample", 48001), "is outside"),
        (set_field("en-a.clean16k", "labels", "speech_end_sample", 8000), "is outside"),
        (set_field("en-a.telephony8k", "size", 24001), "one byte per sample"),
        (set_field("en-a.clean16k", "labels", "method", "inherited"), "only a derived file's"),
        (set_field("en-a.noisy16k", "labels", "method", "human"), "only a derived file's"),
        (set_field("en-a.clean16k", "duration_bin", "gt5to30s"), "is bin '1to5s'"),
        (set_field("ar-b.clean16k", "transcript", "text", "visible"), "carried by digest only"),
        (set_field("en-a.clean16k", "transcript", "text", None), "public, so the text"),
        (set_field("en-a.clean16k", "transcript", "text", "one three"), "not that of its text"),
        (set_field("en-a.noisy16k", "derived_from", "absent"), "'absent', which is not listed"),
        (set_field("en-a.noisy16k", "derived_from", "ar-b.noisy16k"), "is not a clean file"),
        (set_field("en-a.noisy16k", "pool", "tuning"), "pool differs from its clean file"),
        (set_field("en-a.noisy16k", "semantic", []), "semantic differs from its clean file"),
        (set_field("en-a.noisy16k", "samples", 47999), "its recipe makes 48000"),
        (
            lambda m: file_entry(m, "en-a.telephony8k").update(samples=48000, size=48000),
            "its recipe makes 24000",
        ),
        (set_field("en-c.telephony8k", "labels", "speech_end_sample", 49500), "inherited"),
        (set_field("en-c.telephony8k", "labels", "human_verified", True), "inherited"),
        (set_field("en-a.noisy16k", "labels", "speech_onset_sample", 8001), "inherited"),
        (set_field("en-e.noisy16k", "recipe", "snr_db", 5), "5 dB is not the 10 dB the corpus"),
        (set_field("en-a.noisy16k", "recipe", "snr_db", 20), "20 dB is not the 5 dB the corpus"),
        (set_field("en-a.noisy16k", "recipe", "seed", 1), "the seed is not"),
        (set_field("en-a.noisy16k", "recipe", "noise", "absent"), "noise 'absent'"),
        (set_field("en-a.noisy16k", "recipe", "noise", "noise-held-2"), "which is excluded"),
        (set_field("en-a.noisy16k", "recipe", "noise", "noise-tuning-1"), "tuning noise"),
        (set_field("en-a.noisy16k", "recipe", "noise_offset_sample", 1), "offset is not the one"),
        (shorten_exact_noise, "noise-held-exact: the noise has 47999 samples, fewer than"),
        (set_field("ar-b.noisy16k", "recipe", "noise_offset_sample", 0), "offset is not the one"),
        (set_field("sil-d.telephony8k", "recipe", "level_gain_q30", 5), "silence is neither"),
        (set_field("sil-d.telephony8k", "recipe", "sv56_dbov", -26.0), "silence is neither"),
        (
            set_field("en-a.telephony8k", "recipe", "levelled_span_power", None),
            "silence is neither",
        ),
        (set_field("en-a.telephony8k", "recipe", "level_gain_q30", None), "silence is neither"),
        (shared_source_file, "source file ('read', 'en-a.src') is in both"),
        (shared_speaker, "speaker ('s2',) is in both"),
        (shared_noise_file, "source file ('noise-src', 'noise-held-1.wav') is in both"),
        (silence_from_tuning_noise, "source file ('noise-src', 'noise-tuning-1.wav') is in both"),
        (same_samples, "clean file pcm_sha256"),
        (shared_transcript, "is in both the tuning and held-out pools"),
    ]
    for mutate, reason in cases:
        refused(good, mutate, reason)

    def again(clip_id: str, file_ids: list[str]) -> Mutation:
        def mutate(m: dict[str, Any]) -> None:
            copy_of = copy.deepcopy(file_entry(m, f"{clip_id}.clean16k"))
            copy_of.update(id=f"{clip_id}-again.clean16k", path=f"{clip_id}-again.wav")
            copy_of.update(pcm_sha256="2" * 64)
            copy_of["source"]["file_ids"] = file_ids
            if copy_of["labels"] is not None:
                # A bin of its own, so the SNRs its original's group assigns do not move.
                onset = copy_of["labels"]["speech_onset_sample"]
                copy_of["labels"]["speech_end_sample"] = onset + 500
                copy_of["duration_bin"] = "lt1s"
            m["files"].append(copy_of)

        return mutate

    refused(good, again("en-a", ["en-a.src"]), "source file ('read', 'en-a.src') is already")
    for clip_id, file_ids in (("en-a", ["en-a.src", "other.src"]), ("sil-d", ["sil-d.src"])):
        accepted = copy.deepcopy(good)
        again(clip_id, file_ids)(accepted)
        assert corpus.check_manifest(accepted) == len(good["files"]) + 1, clip_id

    def long_speech(m: dict[str, Any]) -> None:
        file_entry(m, "en-a.clean16k").update(samples=480001, labels=long_labels)

    refused(good, long_speech, "longer than 30 s")
    refused(good, lambda m: None, "records the sv56demo level", frozen=True)

    def record_sv56(m: dict[str, Any]) -> None:
        for entry in m["files"]:
            if entry["codec"] == "telephony8k" and entry["suite"] == "speech":
                entry["recipe"]["sv56_dbov"] = -26.1

    frozen = copy.deepcopy(good)
    record_sv56(frozen)
    assert corpus.check_manifest(frozen, frozen=True) == len(good["files"])

    refused(good, lambda m: None, "needs the corpus root", rebuild=True)
    refused(good, set_field("en-a.clean16k", "sha256", "1" * 64), "sha256 is", root=root)
    refused(good, set_field("en-a.clean16k", "pcm_sha256", "1" * 64), "pcm_sha256", root=root)
    refused(good, set_field("en-a.clean16k", "size", 1), "size is", root=root)
    refused(
        good, lambda m: m["noise"][0].update(size=1), "noise/noise-tuning-1.wav: size is", root=root
    )
    refused(good, set_field("en-a.clean16k", "path", "absent.wav"), "unreadable", root=root)
    outside = root.parent / "outside.wav"
    outside.write_bytes((root / "held_out/en-a.clean16k.wav").read_bytes())
    (root / "escape.wav").symlink_to(outside)
    refused(good, set_field("en-a.clean16k", "path", "escape.wav"), "outside", root=root)
    (root / "eight-k.wav").write_bytes(audio.wav_pcm16_mono_bytes(8000, [0] * 48000))
    refused(good, set_field("en-a.clean16k", "path", "eight-k.wav"), "8000 Hz", root=root)

    # A rendering replaced consistently, digests and all, is caught only by rebuilding it.
    tampered_data = audio.telephony_8k_ulaw_bytes(sine_burst(48000, 8000, 40000, 5000, 300))
    (root / "tampered.ul").write_bytes(tampered_data)

    def tamper(m: dict[str, Any]) -> None:
        entry = file_entry(m, "en-a.telephony8k")
        entry.update(path="tampered.ul", sha256=sha(tampered_data), pcm_sha256=sha(tampered_data))

    broken = copy.deepcopy(good)
    tamper(broken)
    assert corpus.check_manifest(broken, root=root) == len(good["files"])
    refused(good, tamper, "the rebuilt rendering differs", root=root, rebuild=True)
    refused(
        good,
        set_field("en-a.noisy16k", "recipe", "mixture_peak", 1),
        "rebuilt mixture_peak",
        root=root,
        rebuild=True,
    )
    refused(
        good,
        set_field("en-a.telephony8k", "recipe", "levelled_span_power", 1),
        "rebuilt levelled_span_power",
        root=root,
        rebuild=True,
    )


def check_schema(good: dict[str, Any]) -> None:
    schema = corpus.load_schema()
    jsonschema.Draft7Validator.check_schema(schema)
    jsonschema.validate(good, schema)
    for mutate, where in (
        (lambda m: m.update(extra=1), "root"),
        (set_field("en-a.clean16k", "codec", "clean8k"), "codec"),
        (set_field("en-a.clean16k", "sha256", "A" * 64), "sha256"),
        (set_field("en-a.clean16k", "path", "../x.wav"), "path"),
        (set_field("en-a.clean16k", "source", "speaker", "Name With Spaces"), "speaker"),
        (set_field("en-a.noisy16k", "recipe", "snr_db", 7), "snr"),
        (set_field("en-a.noisy16k", "recipe", "kind", "telephony8k"), "recipe"),
        (lambda m: m["recipes"]["telephony8k"].update(recipe="tp-fir63-blackman-q15-v1"), "id"),
        (lambda m: m["recipes"]["noisy16k"].update(snr_db=[10, 20]), "snr set"),
    ):
        broken = copy.deepcopy(good)
        mutate(broken)
        try:
            jsonschema.validate(broken, schema)
        except jsonschema.ValidationError:
            pass
        else:
            raise AssertionError(f"jsonschema accepts a broken {where}")
        refused(good, mutate, "does not match its schema")


def check_cli(good: dict[str, Any], root: Path) -> None:
    path = root.parent / "manifest.json"
    path.write_text(json.dumps(good, indent=2))
    run = [sys.executable, str(TOOL)]
    completed = subprocess.run(
        [*run, "check", str(path), "--root", str(root), "--rebuild"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == f"speech-corpus: valid: {len(good['files'])} files, audio rebuilt\n"
    completed = subprocess.run(
        [*run, "check", str(path), "--frozen"], capture_output=True, text=True, check=False
    )
    assert completed.returncode == 1 and "speech-corpus: refused: " in completed.stderr
    path.with_name("broken.json").write_text("{")
    completed = subprocess.run(
        [*run, "check", str(path.with_name("broken.json"))],
        capture_output=True,
        text=True,
        check=False,
    )
    assert completed.returncode == 2 and "unreadable" in completed.stderr
    out = io.StringIO()
    with redirect_stdout(out):
        assert corpus.main(["summary", str(path)]) == 0
    summary = json.loads(out.getvalue())
    assert summary == {
        "files": 13,
        "strata": {
            "held_out/silence/None/clean16k/None": 1,
            "held_out/silence/None/telephony8k/None": 1,
            "held_out/speech/ar/clean16k/lt1s": 1,
            "held_out/speech/ar/noisy16k/lt1s": 1,
            "held_out/speech/ar/telephony8k/lt1s": 1,
            "held_out/speech/en/clean16k/1to5s": 2,
            "held_out/speech/en/noisy16k/1to5s": 2,
            "held_out/speech/en/telephony8k/1to5s": 1,
            "tuning/speech/en/clean16k/gt5to30s": 1,
            "tuning/speech/en/noisy16k/gt5to30s": 1,
            "tuning/speech/en/telephony8k/gt5to30s": 1,
        },
        "distinct_references": {"held_out/ar": 1, "held_out/en": 2, "tuning/en": 1},
        "files_per_source": {"crowd": 3, "read": 10},
        "noisy_per_snr_db": {"held_out/10": 1, "held_out/5": 2, "tuning/5": 1},
    }, summary


def main() -> int:
    check_arithmetic()
    check_snr_assignment()
    check_bins_and_labels()
    check_noisy()
    check_telephony()
    with tempfile.TemporaryDirectory(prefix="tp-speech-corpus-") as tmp:
        root = Path(tmp) / "corpus"
        built = Corpus(root)
        good = built.manifest()
        assert corpus.check_manifest(good) == 13
        assert corpus.check_manifest(good, root=root, rebuild=True) == 13
        check_schema(good)
        check_rules(good, root)
        check_cli(good, root)
        assert os.listdir(root)
    print("speech corpus: renderings, manifest rules and the command line passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
