#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The speech evaluation corpus: its renderings and the check of its manifest.

The manifest's shape is `config/schemas/speech_corpus_manifest.json`; the audio
it names is held outside the repository. A clean 16 kHz clip is rendered noisy
by mixing a noise file at a signal-to-noise ratio measured over the labelled
speech, and rendered for telephony by levelling the speech to -26 dBov and
passing it through the qualification tool's telephony recipe. Both are integer
arithmetic on the standard library, so a rendering has the same bytes on every
machine and the check can rebuild it.

The check derives what the manifest could otherwise only claim: duration bins
from the labels, a rendering's labels from its source's, each noisy seed from
the corpus version, and the separation of the tuning and held-out pools. With
`--root` it also hashes the audio, and with `--rebuild` re-renders every
derived file and compares the bytes.

Exit status: 0 valid, 1 refused, 2 unreadable.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
SCHEMA_PATH = REPO_ROOT / "config/schemas/speech_corpus_manifest.json"

sys.path.insert(0, str(HERE))
import candidate_audio as audio  # noqa: E402
from candidate_record import RecordError, validate_record  # noqa: E402

TOOL_NAME = "speech-corpus"
# The recipe sources a manifest pins; a change to either invalidates its renderings.
TOOLS = ("tools/validation/candidate_audio.py", "tools/validation/speech_corpus.py")

RATE_16K = 16000
RATE_8K = 8000
LEVEL_DBOV = -26
FULL_SCALE = 32768
GAIN_BITS = 30
SNR_SET_DB = (5, 10, 15, 20)

# Bounds of the duration bins in 16 kHz samples of labelled speech.
ONE_SECOND = RATE_16K
FIVE_SECONDS = 5 * RATE_16K
THIRTY_SECONDS = 30 * RATE_16K

# What a build writes beside the audio; every other file under the root is listed.
BUILD_FILES = ("manifest.json", "labels.jsonl")

EXIT_VALID = 0
EXIT_REFUSED = 1
EXIT_UNREADABLE = 2


class ManifestError(Exception):
    """The manifest, or the audio it names, is not what it claims."""


def iroot(value: int, k: int) -> int:
    """The integer floor of the k-th root of a non-negative integer."""
    if value < 2:
        return value
    root = 1 << -(-value.bit_length() // k)
    while True:
        lower = ((k - 1) * root + value // root ** (k - 1)) // k
        if lower >= root:
            return root
        root = lower


def gain_q30(numerator_power: int, denominator_power: int, db: int) -> int:
    """floor(sqrt(numerator / denominator) * 10^(db / 20) * 2^30), exactly.

    Raising the gain to the 20th power clears both the square root and the
    decibel fraction, so no floating point enters a rendering.
    """
    numerator = numerator_power**10 << (20 * GAIN_BITS)
    denominator = denominator_power**10
    if db >= 0:
        numerator *= 10**db
    else:
        denominator *= 10**-db
    return iroot(numerator // denominator, 20)


def apply_gain(sample: int, gain: int) -> int:
    product = sample * gain
    half = 1 << (GAIN_BITS - 1)
    if product >= 0:
        return (product + half) >> GAIN_BITS
    return -((-product + half) >> GAIN_BITS)


def _scale_to(sample: int, top: int, peak: int) -> int:
    magnitude = (2 * abs(sample) * top + peak) // (2 * peak)
    return magnitude if sample >= 0 else -magnitude


def span_power(samples: list[int], onset: int, end: int) -> int:
    return sum(s * s for s in samples[onset:end])


def noisy_seed(corpus_version: str, clip_id: str) -> int:
    """The seed a clean clip's noisy rendering draws its noise offset from."""
    digest = hashlib.sha256(f"{corpus_version}/noisy16k/{clip_id}".encode()).digest()
    return int.from_bytes(digest[:8], "big")


def assigned_snr(files: list[dict[str, Any]]) -> dict[str, int]:
    """Each clean speech clip's noisy SNR: round-robin over the set in sorted id order,
    within each pool, language and duration bin, so every group is balanced."""
    groups: dict[tuple[str, str, str], list[str]] = {}
    for entry in files:
        if entry["codec"] == "clean16k" and entry["suite"] == "speech":
            key = (entry["pool"], entry["language"], entry["duration_bin"])
            groups.setdefault(key, []).append(entry["id"])
    return {
        clip_id: SNR_SET_DB[index % len(SNR_SET_DB)]
        for ids in groups.values()
        for index, clip_id in enumerate(sorted(ids))
    }


def noise_offset(seed: int, noise_samples: int, clip_samples: int) -> int:
    """Where the excerpt starts: the seed modulo the number of starts that fit."""
    if noise_samples < clip_samples:
        raise audio.AudioError(
            f"the noise has {noise_samples} samples, fewer than the clip's {clip_samples}"
        )
    return seed % (noise_samples - clip_samples + 1)


def render_noisy(
    clean: list[int], onset: int, end: int, noise: list[int], *, snr_db: int, seed: int
) -> tuple[list[int], dict[str, int]]:
    """Mix a seeded excerpt of `noise` into `clean` at `snr_db` over the labelled speech.

    The mixture is scaled as a whole when it would exceed 16 bits, so the ratio
    holds and nothing clips; `mixture_peak` records the peak before scaling.
    """
    offset = noise_offset(seed, len(noise), len(clean))
    excerpt = noise[offset : offset + len(clean)]
    speech = span_power(clean, onset, end)
    background = span_power(excerpt, onset, end)
    if speech == 0:
        raise audio.AudioError("the labelled speech is silent")
    if background == 0:
        raise audio.AudioError("the noise excerpt is silent under the labelled speech")
    gain = gain_q30(speech, background, -snr_db)
    mixture = [s + apply_gain(n, gain) for s, n in zip(clean, excerpt, strict=True)]
    peak = max(abs(v) for v in mixture)
    if peak > audio.INT16_MAX:
        mixture = [_scale_to(v, audio.INT16_MAX, peak) for v in mixture]
    return mixture, {"noise_offset_sample": offset, "mixture_peak": peak}


def level_dbov(samples: list[int], onset: int, end: int) -> float:
    return achieved_dbov(span_power(samples, onset, end), end - onset)


def achieved_dbov(levelled_span_power: int, span_samples: int) -> float:
    """The span RMS level a telephony rendering records as its exact integer power."""
    return 10 * math.log10(levelled_span_power / (span_samples * FULL_SCALE**2))


def render_telephony(
    clean: list[int], labels: tuple[int, int] | None
) -> tuple[bytes, dict[str, Any]]:
    """Level the labelled speech to -26 dBov, then the 8 kHz mu-law telephony recipe.

    A gain that would push a sample past 16 bits is lowered to the largest that
    does not, and the level then reads below the target. The level is recorded as
    the levelled span's integer power, so a rebuild compares it exactly. Silence is
    not levelled.
    """
    if labels is None:
        return audio.telephony_8k_ulaw_bytes(clean), {
            "level_gain_q30": None,
            "levelled_span_power": None,
        }
    onset, end = labels
    power = span_power(clean, onset, end)
    if power == 0:
        raise audio.AudioError("the labelled speech is silent")
    gain = gain_q30((end - onset) * FULL_SCALE**2, power, LEVEL_DBOV)
    peak = max(abs(s) for s in clean)
    gain = min(gain, (audio.INT16_MAX << GAIN_BITS) // peak)
    levelled = [apply_gain(s, gain) for s in clean]
    return audio.telephony_8k_ulaw_bytes(levelled), {
        "level_gain_q30": gain,
        "levelled_span_power": span_power(levelled, onset, end),
    }


def telephony_labels(onset: int, end: int) -> tuple[int, int]:
    """16 kHz speech labels at 8 kHz: the decimator is zero-phase, so sample 2k becomes k."""
    return onset // 2, (end + 1) // 2


def duration_bin(speech_samples_16k: int) -> str:
    if speech_samples_16k < ONE_SECOND:
        return "lt1s"
    if speech_samples_16k <= FIVE_SECONDS:
        return "1to5s"
    if speech_samples_16k <= THIRTY_SECONDS:
        return "gt5to30s"
    raise ManifestError(f"{speech_samples_16k} samples of speech is longer than 30 s")


def load_schema(path: Path = SCHEMA_PATH) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _unique(items: list[dict[str, Any]], key: str, what: str) -> dict[str, dict[str, Any]]:
    seen: dict[str, dict[str, Any]] = {}
    for item in items:
        if item[key] in seen:
            raise ManifestError(f"{what} {key} {item[key]!r} appears twice")
        seen[item[key]] = item
    return seen


def _check_tools(recipes: dict[str, Any], repo_root: Path) -> None:
    listed = _unique(recipes["tools"], "path", "recipe tool")
    if sorted(listed) != sorted(TOOLS):
        raise ManifestError(f"recipe tools are {sorted(listed)}, not {sorted(TOOLS)}")
    for path, tool in listed.items():
        actual = _sha256((repo_root / path).read_bytes())
        if actual != tool["sha256"]:
            raise ManifestError(
                f"{path}: rendered with digest {tool['sha256']}, this one is {actual}"
            )


def _check_shape(entry: dict[str, Any]) -> None:
    name = entry["id"]
    clean = entry["codec"] == "clean16k"
    expected = (
        ("mulaw", RATE_8K) if entry["codec"] == "telephony8k" else ("pcm_s16le_wav", RATE_16K)
    )
    if (entry["encoding"], entry["rate"]) != expected:
        raise ManifestError(f"{name}: {entry['codec']} is {expected[0]} at {expected[1]} Hz")
    if entry["encoding"] == "mulaw" and entry["size"] != entry["samples"]:
        raise ManifestError(f"{name}: headerless mu-law has one byte per sample")
    if clean != (entry["derived_from"] is None) or clean != (entry["source"] is not None):
        raise ManifestError(f"{name}: a clean file names its source, a derived file its clean file")
    kind = None if entry["recipe"] is None else entry["recipe"]["kind"]
    if kind != (None if clean else entry["codec"]):
        raise ManifestError(f"{name}: a {entry['codec']} file carries recipe {kind!r}")
    speech = entry["suite"] == "speech"
    for field in ("language", "duration_bin", "transcript", "labels"):
        if (entry[field] is not None) != speech:
            raise ManifestError(f"{name}: {field} is required for speech and absent for silence")
    if not speech and (entry["semantic"] or entry["codec"] == "noisy16k"):
        raise ManifestError(f"{name}: silence carries no semantic class and has no noisy rendering")
    if len(set(entry["semantic"])) != len(entry["semantic"]):
        raise ManifestError(f"{name}: a semantic class is listed twice")
    labels = entry["labels"]
    if labels is not None:
        onset, end = labels["speech_onset_sample"], labels["speech_end_sample"]
        if not 0 <= onset < end <= entry["samples"]:
            raise ManifestError(
                f"{name}: speech [{onset}, {end}) is outside {entry['samples']} samples"
            )
        if (labels["method"] == "inherited") == clean:
            raise ManifestError(f"{name}: only a derived file's labels are inherited")
        if clean and duration_bin(end - onset) != entry["duration_bin"]:
            raise ManifestError(
                f"{name}: {end - onset} samples of speech is bin {duration_bin(end - onset)!r}, "
                f"not {entry['duration_bin']!r}"
            )


def _check_transcript(entry: dict[str, Any], source: dict[str, Any]) -> None:
    transcript = entry["transcript"]
    if transcript is None:
        return
    text = transcript["text"]
    if (text is not None) != source["transcripts_public"]:
        raise ManifestError(
            f"{entry['id']}: source {source['id']} transcripts are "
            + (
                "public, so the text is carried"
                if source["transcripts_public"]
                else "carried by digest only"
            )
        )
    # U+02BC is a letter, so "can't" written with it stays one word and hides its negation.
    if text is not None and entry["language"] == "en" and "\u02bc" in text:
        raise ManifestError(
            f"{entry['id']}: English text holds U+02BC; write the apostrophe as U+0027 or U+2019"
        )
    if text is not None and _sha256(text.encode("utf-8")) != transcript["sha256"]:
        raise ManifestError(f"{entry['id']}: the transcript digest is not that of its text")


def _check_derived(
    entry: dict[str, Any],
    clean: dict[str, Any],
    noise: dict[str, dict[str, Any]],
    manifest: dict[str, Any],
    snr: dict[str, int],
) -> None:
    name = entry["id"]
    if clean["codec"] != "clean16k":
        raise ManifestError(f"{name}: derived from {clean['id']}, which is not a clean file")
    for field in ("suite", "pool", "language", "duration_bin", "transcript", "semantic"):
        if entry[field] != clean[field]:
            raise ManifestError(f"{name}: {field} differs from its clean file {clean['id']}")
    expected_samples = (
        clean["samples"] if entry["codec"] == "noisy16k" else -(-clean["samples"] // 2)
    )
    if entry["samples"] != expected_samples:
        raise ManifestError(
            f"{name}: {entry['samples']} samples, its recipe makes {expected_samples}"
        )
    if clean["labels"] is not None:
        onset, end = clean["labels"]["speech_onset_sample"], clean["labels"]["speech_end_sample"]
        if entry["codec"] == "telephony8k":
            onset, end = telephony_labels(onset, end)
        inherited = {
            "speech_onset_sample": onset,
            "speech_end_sample": end,
            "method": "inherited",
            "tool": None,
            "human_verified": clean["labels"]["human_verified"],
        }
        if entry["labels"] != inherited:
            raise ManifestError(f"{name}: labels are not those inherited from {clean['id']}")
    recipe = entry["recipe"]
    if entry["codec"] == "noisy16k":
        if recipe["snr_db"] != snr[clean["id"]]:
            raise ManifestError(
                f"{name}: {recipe['snr_db']} dB is not the {snr[clean['id']]} dB the corpus assigns"
            )
        if recipe["seed"] != noisy_seed(manifest["corpus"]["version"], clean["id"]):
            raise ManifestError(f"{name}: the seed is not the one the corpus version derives")
        mixed = noise.get(recipe["noise"])
        if mixed is None:
            raise ManifestError(f"{name}: names noise {recipe['noise']!r}, which is not listed")
        if mixed["excluded"]:
            raise ManifestError(f"{name}: mixes noise {mixed['id']}, which is excluded")
        if mixed["pool"] != entry["pool"]:
            raise ManifestError(f"{name}: mixes {mixed['pool']} noise into a {entry['pool']} clip")
        try:
            offset = noise_offset(recipe["seed"], mixed["samples"], entry["samples"])
        except audio.AudioError as exc:
            raise ManifestError(f"{name}: {mixed['id']}: {exc}") from exc
        if recipe["noise_offset_sample"] != offset:
            raise ManifestError(f"{name}: the noise offset is not the one the seed derives")
    else:
        speech = entry["suite"] == "speech"
        levelled = (
            recipe["level_gain_q30"] is not None,
            recipe["levelled_span_power"] is not None,
        )
        if levelled != (speech, speech) or (not speech and recipe["sv56_dbov"] is not None):
            raise ManifestError(f"{name}: speech is levelled and measured, silence is neither")


def _check_pools(files: list[dict[str, Any]], noise: list[dict[str, Any]]) -> None:
    pools: dict[tuple[str, ...], str] = {}

    def claim(key: tuple[str, ...], pool: str, name: str) -> None:
        other = pools.setdefault(key, pool)
        if other != pool:
            raise ManifestError(
                f"{name}: {key[0]} {key[1:]} is in both the tuning and held-out pools"
            )

    for item in noise:
        claim(("source file", item["source"], item["file_id"]), item["pool"], item["id"])
    for entry in files:
        if entry["source"] is not None:
            source = entry["source"]
            for file_id in source["file_ids"]:
                claim(("source file", source["source"], file_id), entry["pool"], entry["id"])
            if source["speaker"] is not None:
                claim(("speaker", source["speaker"]), entry["pool"], entry["id"])
        if entry["transcript"] is not None:
            claim(("transcript", entry["transcript"]["sha256"]), entry["pool"], entry["id"])


def _check_recordings(files: list[dict[str, Any]]) -> None:
    """Each clean file is a distinct recording; an utterance recurs only in a concatenation."""
    clean = [entry for entry in files if entry["codec"] == "clean16k"]
    _unique(clean, "pcm_sha256", "clean file")
    # Silence clips may be several excerpts of one long file.
    single = [
        entry
        for entry in clean
        if entry["suite"] == "speech" and len(entry["source"]["file_ids"]) == 1
    ]
    seen: dict[tuple[str, str], str] = {}
    for entry in single:
        key = (entry["source"]["source"], entry["source"]["file_ids"][0])
        if key in seen:
            raise ManifestError(f"{entry['id']}: source file {key} is already {seen[key]}")
        seen[key] = entry["id"]


def _contained(root: Path, relative: str) -> Path:
    path = (root / relative).resolve()
    if not path.is_relative_to(root.resolve()):
        raise ManifestError(f"{relative} resolves outside the corpus root")
    return path


def read_audio(root: Path, path: str, encoding: str, rate: int) -> tuple[bytes, list[int] | None]:
    """A file's bytes, and its samples when it is PCM; refuses one its entry does not describe."""
    resolved = _contained(root, path)
    try:
        data = resolved.read_bytes()
    except OSError as exc:
        raise ManifestError(f"{path}: unreadable ({exc.__class__.__name__})") from exc
    if encoding == "mulaw":
        return data, None
    try:
        actual_rate, samples = audio.read_wav_pcm16_mono(resolved)
    except audio.AudioError as exc:
        raise ManifestError(str(exc)) from exc
    if actual_rate != rate:
        raise ManifestError(f"{path}: {actual_rate} Hz, not {rate}")
    return data, samples


def _check_unlisted(root: Path, listed: set[str]) -> None:
    for path in sorted(root.rglob("*")):
        if path.is_dir() and not path.is_symlink():
            continue
        relative = path.relative_to(root).as_posix()
        if relative not in listed and relative not in BUILD_FILES:
            raise ManifestError(f"{relative} is under the corpus root but not in the manifest")


def _check_observed(entry: dict[str, Any], data: bytes, pcm: bytes, samples: int) -> None:
    observed = {"size": len(data), "samples": samples, "sha256": _sha256(data)}
    if "pcm_sha256" in entry:
        observed["pcm_sha256"] = _sha256(pcm)
    for field, value in observed.items():
        if entry[field] != value:
            raise ManifestError(
                f"{entry['path']}: {field} is {value!r}, the manifest says {entry[field]!r}"
            )


def _check_file_audio(root: Path, entry: dict[str, Any]) -> list[int] | None:
    data, samples = read_audio(root, entry["path"], entry["encoding"], entry["rate"])
    if samples is None:
        _check_observed(entry, data, data, len(data))
    else:
        _check_observed(entry, data, audio.pcm16_bytes(samples), len(samples))
    return samples


def _check_noise_audio(root: Path, item: dict[str, Any]) -> list[int]:
    data, samples = read_audio(root, item["path"], "pcm_s16le_wav", RATE_16K)
    assert samples is not None
    _check_observed(item, data, b"", len(samples))
    return samples


def _rebuild(
    entry: dict[str, Any],
    clean: dict[str, Any],
    clean_samples: list[int],
    noise_samples: Callable[[str], list[int]],
) -> None:
    labels = clean["labels"]
    span = None if labels is None else (labels["speech_onset_sample"], labels["speech_end_sample"])
    recipe = entry["recipe"]
    if entry["codec"] == "noisy16k":
        assert span is not None
        samples, made = render_noisy(
            clean_samples,
            *span,
            noise_samples(recipe["noise"]),
            snr_db=recipe["snr_db"],
            seed=recipe["seed"],
        )
        data = audio.wav_pcm16_mono_bytes(RATE_16K, samples)
    else:
        data, made = render_telephony(clean_samples, span)
    for field, value in made.items():
        if recipe[field] != value:
            raise ManifestError(
                f"{entry['id']}: rebuilt {field} is {value!r}, the manifest says {recipe[field]!r}"
            )
    if _sha256(data) != entry["sha256"]:
        raise ManifestError(f"{entry['id']}: the rebuilt rendering differs from the file")


def check_manifest(
    manifest: Any,
    *,
    root: Path | None = None,
    rebuild: bool = False,
    frozen: bool = False,
    repo_root: Path = REPO_ROOT,
) -> int:
    """Refuse a manifest that disagrees with what it derives; return its file count."""
    try:
        validate_record(manifest, load_schema())
    except RecordError as exc:
        raise ManifestError(f"the manifest does not match its schema: {exc}") from exc
    _check_tools(manifest["recipes"], repo_root)
    sources = _unique(manifest["sources"], "id", "source")
    noise = _unique(manifest["noise"], "id", "noise")
    for item in noise.values():
        if item["source"] not in sources:
            raise ManifestError(f"noise {item['id']}: source {item['source']!r} is not listed")
        if item["excluded"] != (item["exclusion_reason"] is not None):
            raise ManifestError(
                f"noise {item['id']}: an exclusion has a reason and only an exclusion"
            )
    files = manifest["files"]
    by_id = _unique(files, "id", "file")
    snr = assigned_snr(files)
    _unique(files + manifest["noise"], "path", "audio")
    for entry in files:
        _check_shape(entry)
        if entry["source"] is not None:
            source = sources.get(entry["source"]["source"])
            if source is None:
                raise ManifestError(
                    f"{entry['id']}: source {entry['source']['source']!r} is not listed"
                )
            _check_transcript(entry, source)
        else:
            clean = by_id.get(entry["derived_from"])
            if clean is None:
                raise ManifestError(
                    f"{entry['id']}: derived from {entry['derived_from']!r}, which is not listed"
                )
            _check_derived(entry, clean, noise, manifest, snr)
            if (
                frozen
                and entry["codec"] == "telephony8k"
                and entry["suite"] == "speech"
                and entry["recipe"]["sv56_dbov"] is None
            ):
                raise ManifestError(f"{entry['id']}: a frozen corpus records the sv56demo level")
    _check_pools(files, manifest["noise"])
    _check_recordings(files)
    if root is None:
        if rebuild:
            raise ManifestError("a rebuild needs the corpus root")
        return len(files)
    for item in noise.values():
        _check_noise_audio(root, item)
    derived: dict[str, list[dict[str, Any]]] = {}
    for entry in files:
        if entry["derived_from"] is not None:
            derived.setdefault(entry["derived_from"], []).append(entry)
    # A real corpus does not fit in memory as samples: hold one clean file and one noise file.
    loaded: dict[str, list[int]] = {}

    def noise_samples(noise_id: str) -> list[int]:
        if noise_id not in loaded:
            loaded.clear()
            loaded[noise_id] = _check_noise_audio(root, noise[noise_id])
        return loaded[noise_id]

    for entry in files:
        if entry["derived_from"] is not None:
            continue
        clean_samples = _check_file_audio(root, entry)
        assert clean_samples is not None
        for child in derived.get(entry["id"], []):
            _check_file_audio(root, child)
            if rebuild:
                _rebuild(child, entry, clean_samples, noise_samples)
    _check_unlisted(root, {item["path"] for item in files + manifest["noise"]})
    return len(files)


def summarize(manifest: dict[str, Any]) -> dict[str, Any]:
    """Counts per stratum, distinct references per language and pool, and noisy files per SNR."""
    strata: dict[str, int] = {}
    references: dict[str, set[str]] = {}
    per_source: dict[str, int] = {}
    per_snr: dict[str, int] = {}
    files = manifest["files"]
    by_id = {entry["id"]: entry for entry in files}
    for entry in files:
        key = "/".join(
            str(entry[field]) for field in ("pool", "suite", "language", "codec", "duration_bin")
        )
        strata[key] = strata.get(key, 0) + 1
        clean = entry if entry["derived_from"] is None else by_id[entry["derived_from"]]
        source = clean["source"]["source"]
        per_source[source] = per_source.get(source, 0) + 1
        if entry["transcript"] is not None:
            references.setdefault(f"{entry['pool']}/{entry['language']}", set()).add(
                entry["transcript"]["sha256"]
            )
        if entry["codec"] == "noisy16k":
            snr = f"{entry['pool']}/{entry['recipe']['snr_db']}"
            per_snr[snr] = per_snr.get(snr, 0) + 1
    return {
        "files": len(files),
        "strata": dict(sorted(strata.items())),
        "distinct_references": {k: len(v) for k, v in sorted(references.items())},
        "files_per_source": dict(sorted(per_source.items())),
        "noisy_per_snr_db": dict(sorted(per_snr.items())),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check", help="verify a manifest and, with --root, its audio")
    check.add_argument("manifest", type=Path)
    check.add_argument("--root", type=Path, help="the directory the manifest's paths are under")
    check.add_argument("--rebuild", action="store_true", help="re-render every derived file")
    check.add_argument("--frozen", action="store_true", help="require every recorded cross-check")
    summary = commands.add_parser("summary", help="print a valid manifest's counts as JSON")
    summary.add_argument("manifest", type=Path)
    args = parser.parse_args(argv)
    try:
        manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        print(f"{TOOL_NAME}: unreadable: {args.manifest}: {exc}", file=sys.stderr)
        return EXIT_UNREADABLE
    try:
        if args.command == "check":
            count = check_manifest(
                manifest, root=args.root, rebuild=args.rebuild, frozen=args.frozen
            )
        else:
            check_manifest(manifest)
    except (ManifestError, audio.AudioError) as exc:
        print(f"{TOOL_NAME}: refused: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    if args.command == "check":
        checked = (
            "audio rebuilt" if args.rebuild else "audio hashed" if args.root else "audio not read"
        )
        print(f"{TOOL_NAME}: valid: {count} files, {checked}")
    else:
        print(json.dumps(summarize(manifest), indent=2))
    return EXIT_VALID


if __name__ == "__main__":
    sys.exit(main())
