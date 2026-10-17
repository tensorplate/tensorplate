#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Build the speech evaluation corpus from its downloaded sources.

A plan (JSON) names the sources, their pools, the selection quotas, the noise
pool and its exclusions. The aligner runs between the steps:

    speech_corpus_build.py noise PLAN --sources DIR --work DIR
    speech_corpus_build.py screen-jobs PLAN --sources DIR --work DIR
    speech_corpus_align.py WORK/screen-jobs.json WORK/screen.jsonl --screen
    speech_corpus_build.py screen-exclusions PLAN --sources DIR --work DIR
    speech_corpus_build.py candidates PLAN --sources DIR --work DIR
    speech_corpus_align.py WORK/align-jobs.json WORK/labels.jsonl --arabic-model DIR
    speech_corpus_build.py build PLAN --sources DIR --work DIR --root CORPUS [--sv56 BIN]

`noise` takes the noise pool; the screen steps decode it and print the
exclusions the plan records. `candidates` lists every source file a pool may
use, applies the rules that need only the audio and the transcript, and
writes the aligner's jobs. `build` applies the rules that need the labels,
selects each quota in a seeded order under the speaker and text caps, writes
the clean, noisy and telephony files, the manifest and the labels selection
read under the empty directory CORPUS, and records what every rule withheld
per source. `speech_corpus.py check --root --rebuild` verifies the result.
Reading FLAC or float WAV sources needs the soundfile package.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import re
import subprocess
import sys
import tempfile
import unicodedata
import wave
from collections.abc import Iterator
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))

import candidate_audio as audio  # noqa: E402
import speech_corpus as corpus  # noqa: E402

FULL_SCALE_LIMIT = 10
MAX_FILE_SAMPLES = corpus.THIRTY_SECONDS
WINDOW_SEAM_SAMPLES = 8 * corpus.RATE_16K
# One aligner frame: speech this close to a file's edge may have been cut off by it.
EDGE_SAMPLES = 320

RULE_LONG = "file longer than 30 s"
RULE_FORMAT = "not 16 kHz mono"
RULE_FULL_SCALE = f"{FULL_SCALE_LIMIT} or more samples at 16-bit full scale"
RULE_TEXT_POOLS = "reference text used in the other pool"
RULE_UNALIGNED = "first or last word not aligned"
RULE_EDGE = "labelled speech within 20 ms of the file's start or end"
LABEL_RULES = (RULE_UNALIGNED, RULE_EDGE)
CLASSES = ("negation", "numerals", "repetition", "window_seam")
SCREEN_LETTERS = 10
LABEL_FIELDS = ("aligned", "edge_oov", "speech_onset_sample", "speech_end_sample")

NEGATION = {
    "en": {
        "no",
        "not",
        "never",
        "nothing",
        "none",
        "nobody",
        "nowhere",
        "neither",
        "nor",
        "cannot",
    },
    "ar": {"لا", "لم", "لن", "ليس", "ليست", "ما"},
}
NUMBER_WORDS = {
    "en": set(
        "zero one two three four five six seven eight nine ten eleven twelve thirteen fourteen "
        "fifteen sixteen seventeen eighteen nineteen twenty thirty forty fifty sixty seventy "
        "eighty ninety hundred thousand million billion".split()
    ),
    "ar": set(
        "صفر واحد واحدة اثنان اثنين اثنتان ثلاث ثلاثة أربع أربعة خمس خمسة ست ستة سبع سبعة "
        "ثمان ثماني ثمانية تسع تسعة عشر عشرة عشرون عشرين ثلاثون ثلاثين أربعون أربعين خمسون "
        "خمسين ستون ستين سبعون سبعين ثمانون ثمانين تسعون تسعين مئة مائة مئتان مئتين ألف ألفان "
        "آلاف مليون ملايين".split()
    ),
}


class BuildError(Exception):
    """A plan or source that cannot be built."""


def order_key(version: str, *parts: str) -> str:
    """The seeded order quotas and choices draw in; stable when other files come and go."""
    return hashlib.sha256("/".join((version, *parts)).encode("utf-8")).hexdigest()


def float_to_pcm16(values: list[float]) -> tuple[list[int], int]:
    """Float samples to 16 bits by round-half-even(x * 32767), clipped; returns the clip count."""
    out, clipped = [], 0
    for value in values:
        scaled = round(value * audio.INT16_MAX)
        if scaled > audio.INT16_MAX or scaled < audio.INT16_MIN:
            clipped += 1
            scaled = max(audio.INT16_MIN, min(audio.INT16_MAX, scaled))
        out.append(scaled)
    return out, clipped


def full_scale_count(samples: list[int]) -> int:
    return sum(1 for s in samples if s >= audio.INT16_MAX or s <= -audio.INT16_MAX)


def words(text: str) -> list[str]:
    """Casefolded words with punctuation removed, as the semantic rules read them."""
    text = unicodedata.normalize("NFC", text).casefold().replace("-", " ")
    return [w for w in (re.sub(r"[^\w']", "", t) for t in text.split()) if w]


def _forms(word: str, language: str) -> set[str]:
    """A word as written and, in Arabic, without a leading conjunction wa or fa."""
    if language == "ar" and word[:1] in ("و", "ف"):
        return {word, word[1:]}
    return {word}


def semantic_classes(text: str, language: str, speech_samples: int) -> list[str]:
    """The fixture classes a reference qualifies for: negation, numerals, repetition, seams."""
    found = []
    tokens = words(text)
    forms = [_forms(w, language) for w in tokens]
    if any(
        f & NEGATION[language] or (language == "en" and w.endswith("n't"))
        for w, f in zip(tokens, forms, strict=True)
    ):
        found.append("negation")
    if any(
        any(ch.isdigit() for ch in w) or f & NUMBER_WORDS[language]
        for w, f in zip(tokens, forms, strict=True)
    ):
        found.append("numerals")
    if any(w in tokens[max(0, i - 3) : i] for i, w in enumerate(tokens)):
        found.append("repetition")
    if speech_samples > WINDOW_SEAM_SAMPLES:
        found.append("window_seam")
    return found


def select_order(version: str, candidate: dict[str, Any]) -> str:
    return order_key(version, "select", candidate["source"], candidate["file_id"])


def select(
    candidates: list[dict[str, Any]], quota: dict[str, Any], version: str, taken: set[str]
) -> list[dict[str, Any]]:
    """The quota's first files in seeded order, at most `speaker_cap` per speaker and
    `text_cap` per reference text. Labels are needed only up to the last file chosen, so
    an unlabelled file before the quota fills stops the selection rather than being skipped:
    skipping it could choose differently from labelling every file."""
    group = sorted(
        (
            c
            for c in candidates
            if c["source"] == quota["source"]
            and c["pool"] == quota["pool"]
            and c["withheld"] is None
            and c["key"] not in taken
        ),
        key=lambda c: select_order(version, c),
    )
    speakers: dict[str, int] = {}
    texts: dict[str, int] = {}
    chosen = []
    for c in group:
        if len(chosen) == quota["count"]:
            break
        if "bin" not in c:
            raise BuildError(
                f"{quota['source']} {quota['pool']} {quota['bin']}: {len(chosen)} chosen before "
                f"{c['key']}, which is not labelled yet; align further"
            )
        if c["bin"] != quota["bin"]:
            continue
        speaker, text = c["speaker"], c["text_group"]
        if speaker is not None and speakers.get(speaker, 0) >= quota["speaker_cap"]:
            continue
        if quota["text_cap"] is not None and texts.get(text, 0) >= quota["text_cap"]:
            continue
        speakers[speaker] = speakers.get(speaker, 0) + 1
        texts[text] = texts.get(text, 0) + 1
        chosen.append(c)
    if len(chosen) < quota["count"]:
        raise BuildError(
            f"{quota['source']} {quota['pool']} {quota['bin']}: "
            f"{len(chosen)} eligible of {quota['count']}"
        )
    return chosen


def text_pool_conflicts(candidates: list[dict[str, Any]]) -> set[str]:
    """Reference texts that candidates of both pools carry, by digest of the exact text."""
    pools: dict[str, set[str]] = {}
    for c in candidates:
        pools.setdefault(c["text_sha256"], set()).add(c["pool"])
    return {digest for digest, seen in pools.items() if len(seen) > 1}


def choose_noise(
    noise: list[dict[str, Any]], pool: str, clip_samples: int, version: str, clip_id: str
) -> dict[str, Any]:
    """A seeded choice among the pool's included noise files long enough for the clip."""
    eligible = sorted(
        (
            n
            for n in noise
            if n["pool"] == pool and not n["excluded"] and n["samples"] >= clip_samples
        ),
        key=lambda n: n["id"],
    )
    if not eligible:
        raise BuildError(f"{clip_id}: no {pool} noise file has {clip_samples} samples")
    index = int(order_key(version, "noise", clip_id)[:16], 16) % len(eligible)
    return eligible[index]


def slug(value: str) -> str:
    return re.sub(r"[^a-z0-9.-]+", "-", value.lower()).strip("-.")


# Source readers: each yields (file_id, relative path, text, text for alignment,
# text group, speaker) for one split directory of the plan.


def read_librispeech(base: Path, split: dict[str, Any]) -> Iterator[tuple[str, ...]]:
    for listing in sorted((base / split["dir"]).glob("*/*/*.trans.txt")):
        for line in listing.read_text(encoding="utf-8").splitlines():
            uid, text = line.split(" ", 1)
            path = listing.parent / f"{uid}.flac"
            yield uid, str(path.relative_to(base)), text, text, text, uid.split("-")[0]


def read_fleurs(base: Path, split: dict[str, Any]) -> Iterator[tuple[str, ...]]:
    with (base / split["list"]).open(encoding="utf-8", newline="") as handle:
        for row in csv.reader(handle, delimiter="\t", quoting=csv.QUOTE_NONE):
            sentence, wav, raw, normalized = row[0], row[1], row[2], row[3]
            path = Path(split["dir"]) / wav
            yield wav.removesuffix(".wav"), str(path), raw, normalized, sentence, None


def read_speech_commands(base: Path, split: dict[str, Any]) -> Iterator[tuple[str, ...]]:
    texts = split["texts"]
    for line in (base / split["list"]).read_text(encoding="utf-8").splitlines():
        word, name = line.split("/")
        if word not in texts:
            continue
        path = Path(split["dir"]) / line
        text = texts[word]
        yield line.removesuffix(".wav"), str(path), text, text, text, name.split("_")[0]


def read_arabic_speech_commands(base: Path, split: dict[str, Any]) -> Iterator[tuple[str, ...]]:
    texts = split["texts"]
    with (base / split["list"]).open(encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle):
            word, relative = row["class"], row["file"]
            if word not in texts:
                continue
            speaker = Path(relative).name[:8]
            file_id = relative.removeprefix("dataset/").removesuffix(".wav")
            path = Path(split["dir"]) / relative
            # One take per speaker and word: a speaker's ten takes are near duplicates.
            yield file_id, str(path), texts[word], texts[word], f"{speaker}/{word}", speaker


def read_openslr46(base: Path, split: dict[str, Any]) -> Iterator[tuple[str, ...]]:
    """Tunisian_MSA: prompts read by every speaker, and answers to prompted questions."""
    root, part = base / split["dir"], split["part"]
    prompts = dict(
        line.split("\t", 1)
        for line in (root / "transcripts" / part / "recordings.tsv")
        .read_text(encoding="utf-8")
        .splitlines()
    )
    answers = {}
    if split["answers"]:
        for line in (
            (root / "transcripts" / part / "answers.tsv").read_text(encoding="utf-8").splitlines()
        ):
            key, text = line.split("\t", 1)
            answers[key] = text
    allowed = set(split["prompts"])
    for speaker in split["speakers"]:
        session, number = speaker.split("/")
        for kind, table in (("Recordings_Arabic", prompts), ("Answers_Arabic", answers)):
            folder = root / "speech" / part / session / kind / number
            for path in sorted(folder.glob("*.wav")) if table else []:
                n = path.stem
                if kind == "Recordings_Arabic":
                    if n not in allowed:
                        continue
                    text = table.get(n)
                else:
                    text = table.get(f"{session}_{number}_{kind}_{n}")
                if text is None:
                    continue
                text = text.strip()
                file_id = f"{part}/{session}/{kind}/{number}/{n}"
                yield file_id, str(path.relative_to(base)), text, text, text, speaker


READERS = {
    "librispeech": read_librispeech,
    "fleurs": read_fleurs,
    "speech_commands": read_speech_commands,
    "arabic_speech_commands": read_arabic_speech_commands,
    "openslr46": read_openslr46,
}


def read_source_audio(path: Path) -> tuple[int, list[int], int]:
    """(rate, 16-bit samples, clip count) of a mono source file; float sources are converted.
    No samples means the file is not mono 16-bit or float."""
    try:
        with wave.open(str(path), "rb") as handle:
            rate = handle.getframerate()
            if handle.getnchannels() != 1 or handle.getsampwidth() != 2:
                return rate, [], 0
            return rate, audio.pcm16_samples(handle.readframes(handle.getnframes())), 0
    except (wave.Error, EOFError):
        pass
    import soundfile

    info = soundfile.info(str(path))
    if info.channels != 1:
        return info.samplerate, [], 0
    if info.subtype in ("FLOAT", "DOUBLE"):
        data, rate = soundfile.read(str(path), dtype="float64")
        samples, clipped = float_to_pcm16(data.tolist())
        return rate, samples, clipped
    data, rate = soundfile.read(str(path), dtype="int16")
    return rate, data.tolist(), 0


def source_frames(path: Path) -> tuple[int, int]:
    """(rate, frames) of a mono source from its header; no frames when it is not mono."""
    try:
        with wave.open(str(path), "rb") as handle:
            mono = handle.getnchannels() == 1 and handle.getsampwidth() == 2
            return handle.getframerate(), handle.getnframes() if mono else 0
    except (wave.Error, EOFError):
        pass
    import soundfile

    info = soundfile.info(str(path))
    return info.samplerate, info.frames if info.channels == 1 else 0


def _scan(args: tuple[str, str]) -> dict[str, Any]:
    base, relative = args
    rate, samples, clipped = read_source_audio(Path(base) / relative)
    return {
        "rate": rate,
        "samples": len(samples),
        "full_scale": full_scale_count(samples),
        "clipped": clipped,
    }


def candidates_step(plan: dict[str, Any], sources: Path, work: Path, workers: int) -> None:
    version = plan["corpus"]["version"]
    found: list[dict[str, Any]] = []
    for source in plan["sources"]:
        for split in source.get("splits", []):
            for file_id, path, text, align_text, group, speaker in READERS[source["reader"]](
                sources, split
            ):
                found.append(
                    {
                        "key": f"{source['id']}/{file_id}",
                        "source": source["id"],
                        "file_id": file_id,
                        "path": path,
                        "pool": split["pool"],
                        "language": source["language"],
                        "text": text,
                        "align_text": align_text,
                        "text_group": group,
                        "text_sha256": hashlib.sha256(text.encode("utf-8")).hexdigest(),
                        "speaker": speaker,
                    }
                )
    with ProcessPoolExecutor(workers) as pool:
        scans = list(pool.map(_scan, [(str(sources), c["path"]) for c in found], chunksize=64))
    for c, scan in zip(found, scans, strict=True):
        c.update(scan)
        c["withheld"] = (
            RULE_FORMAT
            if c["rate"] != corpus.RATE_16K or c["samples"] == 0
            else RULE_LONG
            if c["samples"] > MAX_FILE_SAMPLES
            else RULE_FULL_SCALE
            if c["full_scale"] >= FULL_SCALE_LIMIT
            else None
        )
    conflicts = text_pool_conflicts([c for c in found if c["withheld"] is None])
    for c in found:
        if c["withheld"] is None and c["text_sha256"] in conflicts:
            c["withheld"] = RULE_TEXT_POOLS
    work.mkdir(parents=True, exist_ok=True)
    (work / "candidates.json").write_text(json.dumps(found, ensure_ascii=False), encoding="utf-8")
    wanted: dict[tuple[str, str], int] = {}
    for quota in plan["quotas"]:
        name = (quota["source"], quota["pool"])
        wanted[name] = wanted.get(name, 0) + quota["count"]
    groups: dict[tuple[str, str], list[dict[str, Any]]] = {}
    for c in found:
        if c["withheld"] is None and (c["source"], c["pool"]) in wanted:
            groups.setdefault((c["source"], c["pool"]), []).append(c)
    ranked = []
    for name, group in groups.items():
        group.sort(key=lambda c: select_order(version, c))
        ranked.extend(((rank + 1) / wanted[name], name, c) for rank, c in enumerate(group))
    ranked.sort(key=lambda item: item[:2])
    # Each source and pool's files in selection order, interleaved in proportion to its
    # quotas, so any prefix of the jobs labels a prefix of every group.
    jobs = [
        {
            "id": c["key"],
            "path": str(sources / c["path"]),
            "language": c["language"],
            "text": c["align_text"],
        }
        for _, _, c in ranked
    ]
    (work / "align-jobs.json").write_text(json.dumps(jobs, ensure_ascii=False), encoding="utf-8")
    print(json.dumps({"candidates": len(found), "align_jobs": len(jobs)}))


def _file_entry(
    clean_id: str, path: str, data: bytes, pcm: bytes, samples: int, rate: int, encoding: str
) -> dict[str, Any]:
    return {
        "id": clean_id,
        "path": path,
        "sha256": hashlib.sha256(data).hexdigest(),
        "pcm_sha256": hashlib.sha256(pcm).hexdigest(),
        "size": len(data),
        "samples": samples,
        "rate": rate,
        "encoding": encoding,
    }


def _sv56_dbov(binary: str, samples: list[int]) -> float:
    with tempfile.NamedTemporaryFile(suffix=".raw") as raw:
        raw.write(audio.pcm16_bytes(samples))
        raw.flush()
        out = subprocess.run(
            [binary, "-q", "-sf", "16000", raw.name, "/dev/null"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    match = re.search(r"Active speech level: \.* *(-?[0-9.]+) \[dBov\]", out)
    if match is None:
        raise BuildError("sv56demo printed no active speech level")
    return float(match.group(1))


def _render(job: dict[str, Any]) -> list[dict[str, Any]]:
    """Write one clean clip and its renderings; return their manifest entries."""
    root = Path(job["root"])
    rate, samples, _ = read_source_audio(Path(job["source_path"]))
    clean_id, pool, language = job["id"], job["pool"], job["language"]
    onset, end = job["onset"], job["end"]
    common = {
        "suite": "speech",
        "pool": pool,
        "language": language,
        "duration_bin": job["bin"],
        "transcript": job["transcript"],
        "semantic": job["semantic"],
    }
    entries = []
    clean_path = f"{pool}/{language}/clean16k/{clean_id}.wav"
    clean_bytes = audio.wav_pcm16_mono_bytes(rate, samples)
    (root / clean_path).parent.mkdir(parents=True, exist_ok=True)
    (root / clean_path).write_bytes(clean_bytes)
    entries.append(
        {
            **_file_entry(
                clean_id,
                clean_path,
                clean_bytes,
                audio.pcm16_bytes(samples),
                len(samples),
                rate,
                "pcm_s16le_wav",
            ),
            "codec": "clean16k",
            **common,
            "source": job["source"],
            "derived_from": None,
            "labels": {
                "speech_onset_sample": onset,
                "speech_end_sample": end,
                "method": "forced_alignment",
                "tool": job["label_tool"],
                "human_verified": False,
            },
            "recipe": None,
        }
    )
    noise = job["noise"]
    offset = corpus.noise_offset(job["seed"], noise["samples"], len(samples))
    with wave.open(str(root / noise["path"]), "rb") as handle:
        handle.setpos(offset)
        excerpt = audio.pcm16_samples(handle.readframes(len(samples)))
    mixture, made = corpus.render_noisy(
        samples, onset, end, excerpt, snr_db=job["snr_db"], seed=job["seed"]
    )
    noisy_path = f"{pool}/{language}/noisy16k/{clean_id}.wav"
    noisy_bytes = audio.wav_pcm16_mono_bytes(rate, mixture)
    (root / noisy_path).parent.mkdir(parents=True, exist_ok=True)
    (root / noisy_path).write_bytes(noisy_bytes)
    entries.append(
        {
            **_file_entry(
                f"{clean_id}.noisy16k",
                noisy_path,
                noisy_bytes,
                audio.pcm16_bytes(mixture),
                len(mixture),
                rate,
                "pcm_s16le_wav",
            ),
            "codec": "noisy16k",
            **common,
            "source": None,
            "derived_from": clean_id,
            "labels": {**entries[0]["labels"], "method": "inherited", "tool": None},
            "recipe": {
                "kind": "noisy16k",
                "noise": noise["id"],
                "noise_offset_sample": offset,
                "snr_db": job["snr_db"],
                "seed": job["seed"],
                "mixture_peak": made["mixture_peak"],
            },
        }
    )
    tele, made = corpus.render_telephony(samples, (onset, end))
    sv56 = None
    if job["sv56"]:
        levelled = [corpus.apply_gain(s, made["level_gain_q30"]) for s in samples]
        try:
            sv56 = _sv56_dbov(job["sv56"], levelled)
        except BuildError as exc:
            raise BuildError(f"{clean_id}: {exc}") from exc
    tele_path = f"{pool}/{language}/telephony8k/{clean_id}.ul"
    (root / tele_path).parent.mkdir(parents=True, exist_ok=True)
    (root / tele_path).write_bytes(tele)
    t_onset, t_end = corpus.telephony_labels(onset, end)
    entries.append(
        {
            **_file_entry(
                f"{clean_id}.telephony8k", tele_path, tele, tele, len(tele), corpus.RATE_8K, "mulaw"
            ),
            "codec": "telephony8k",
            **common,
            "source": None,
            "derived_from": clean_id,
            "labels": {
                "speech_onset_sample": t_onset,
                "speech_end_sample": t_end,
                "method": "inherited",
                "tool": None,
                "human_verified": False,
            },
            "recipe": {"kind": "telephony8k", **made, "sv56_dbov": sv56},
        }
    )
    return entries


def _tool_digests() -> list[dict[str, str]]:
    return [
        {"path": path, "sha256": hashlib.sha256((REPO_ROOT / path).read_bytes()).hexdigest()}
        for path in corpus.TOOLS
    ]


def build_step(
    plan: dict[str, Any],
    sources: Path,
    work: Path,
    root: Path | None,
    sv56: str | None,
    workers: int,
    labels_path: Path | None = None,
) -> None:
    version = plan["corpus"]["version"]
    if root is not None and root.exists() and any(root.iterdir()):
        raise BuildError(f"{root} is not empty; a build writes a whole corpus")
    found = json.loads((work / "candidates.json").read_text(encoding="utf-8"))
    labels = {}
    for line in (labels_path or work / "labels.jsonl").read_text(encoding="utf-8").splitlines():
        result = json.loads(line)
        labels[result["id"]] = result
    for c in found:
        if c["withheld"] is not None:
            continue
        result = labels.get(c["key"])
        if result is None:
            continue
        if not result["aligned"] or result["edge_oov"]:
            c["withheld"] = RULE_UNALIGNED
            continue
        c["onset"], c["end"] = result["speech_onset_sample"], result["speech_end_sample"]
        if c["onset"] < EDGE_SAMPLES or c["samples"] - c["end"] < EDGE_SAMPLES:
            c["withheld"] = RULE_EDGE
            continue
        c["bin"] = corpus.duration_bin(c["end"] - c["onset"])
    taken: set[str] = set()
    chosen: list[dict[str, Any]] = []
    unfilled = []
    # The last file each source and pool's quotas examined: label rules are counted, and
    # labels filed, up to it, which is the same however far the aligner ran past it.
    stops: dict[tuple[str, str], str] = {}
    for quota in plan["quotas"]:
        try:
            picked = select(found, quota, version, taken)
        except BuildError as exc:
            unfilled.append(str(exc))
            continue
        taken.update(c["key"] for c in picked)
        chosen.extend(picked)
        if picked:
            name = (quota["source"], quota["pool"])
            stops[name] = max(stops.get(name, ""), select_order(version, picked[-1]))
    if unfilled or root is None:
        print(json.dumps({"chosen": len(chosen), "unfilled": unfilled}, indent=1))
        if unfilled:
            raise BuildError(f"{len(unfilled)} quotas unfilled")
        return
    pool_noise = json.loads((work / "noise.json").read_text(encoding="utf-8"))
    noise = pool_noise["items"]
    sources_by_id = {s["id"]: s for s in plan["sources"]}
    clean_ids = {}
    for c in chosen:
        clean_ids[c["key"]] = f"{c['language']}-{c['source']}-{slug(c['file_id'])}"
    snr = corpus.assigned_snr(
        [
            {
                "id": clean_ids[c["key"]],
                "codec": "clean16k",
                "suite": "speech",
                "pool": c["pool"],
                "language": c["language"],
                "duration_bin": c["bin"],
            }
            for c in chosen
        ]
    )
    fixtures, availability = _fixtures(plan, chosen, version)
    chosen_noise = {}
    jobs = []
    for c in chosen:
        clean_id = clean_ids[c["key"]]
        source = sources_by_id[c["source"]]
        jobs.append(
            {
                "root": str(root),
                "source_path": str(sources / c["path"]),
                "id": clean_id,
                "pool": c["pool"],
                "language": c["language"],
                "bin": c["bin"],
                "onset": c["onset"],
                "end": c["end"],
                "transcript": {
                    "sha256": c["text_sha256"],
                    "text": c["text"] if source["transcripts_public"] else None,
                },
                "semantic": fixtures.get(c["key"], []),
                "source": {
                    "source": c["source"],
                    "file_ids": [c["file_id"]],
                    "speaker": None
                    if c["speaker"] is None
                    else "s"
                    + order_key(version, "speaker", source["speaker_namespace"], c["speaker"])[:12],
                },
                "label_tool": plan["aligners"][c["language"]],
                "noise": chosen_noise.setdefault(
                    clean_id, choose_noise(noise, c["pool"], c["samples"], version, clean_id)
                ),
                "seed": corpus.noisy_seed(version, clean_id),
                "snr_db": snr[clean_id],
                "sv56": sv56,
            }
        )
    listed = [n for n in noise if n["excluded"] or n in chosen_noise.values()]
    for item in listed:
        (root / item["path"]).parent.mkdir(parents=True, exist_ok=True)
        (root / item["path"]).write_bytes((work / item["path"]).read_bytes())
    with ProcessPoolExecutor(workers) as pool:
        entries = [e for result in pool.map(_render, jobs, chunksize=4) for e in result]

    def examined(c: dict[str, Any]) -> bool:
        stop = stops.get((c["source"], c["pool"]))
        return stop is not None and select_order(version, c) <= stop

    withheld: dict[str, dict[str, int]] = {}
    for c in found:
        if c["withheld"] is not None and (c["withheld"] not in LABEL_RULES or examined(c)):
            counts = withheld.setdefault(c["source"], {})
            counts[c["withheld"]] = counts.get(c["withheld"], 0) + 1
    manifest_sources = []
    for source in plan["sources"]:
        entry = {k: source[k] for k in plan["source_fields"]}
        entry["selection"] = [
            {"rule": rule, "excluded": count}
            for rule, count in sorted(withheld.get(source["id"], {}).items())
        ]
        if source["id"] == plan["noise"]["source"]:
            entry["selection"] = pool_noise["selection"]
        manifest_sources.append(entry)
    filed = [
        {"id": key, **{k: labels[key].get(k) for k in LABEL_FIELDS}}
        for key in sorted(c["key"] for c in found if c["key"] in labels and examined(c))
    ]
    (root / "labels.jsonl").write_text(
        "".join(json.dumps(line, sort_keys=True) + "\n" for line in filed), encoding="utf-8"
    )
    manifest = {
        "schema_version": "0.1",
        "corpus": plan["corpus"],
        "recipes": {**plan["recipes"], "tools": _tool_digests()},
        "sources": manifest_sources,
        "noise": sorted(listed, key=lambda n: n["id"]),
        "files": sorted(entries, key=lambda e: e["id"]),
    }
    (root / "manifest.json").write_text(dump_manifest(manifest), encoding="utf-8")
    print(json.dumps({"fixtures": availability}, sort_keys=True))
    print(json.dumps(corpus.summarize(manifest), sort_keys=True))


def _fixtures(
    plan: dict[str, Any], chosen: list[dict[str, Any]], version: str
) -> tuple[dict[str, list[str]], dict[str, dict[str, int]]]:
    """Per held-out file, the semantic classes it is a fixture for, and per language how
    many files qualify for each class. Each class takes its first `per_class` qualifying
    files per language in seeded order that no class took before it, so a language has
    four times `per_class` fixtures; a fixture lists every class it meets."""
    per_class = plan["semantic"]["per_class"]
    qualifies = {}
    for c in chosen:
        if c["pool"] == "held_out":
            qualifies[c["key"]] = semantic_classes(c["text"], c["language"], c["end"] - c["onset"])
    picked: set[str] = set()
    availability: dict[str, dict[str, int]] = {}
    for language in sorted({c["language"] for c in chosen if c["key"] in qualifies}):
        ordered = sorted(
            (c["key"] for c in chosen if c["key"] in qualifies and c["language"] == language),
            key=lambda key: order_key(version, "semantic", key),
        )
        availability[language] = {}
        for name in CLASSES:
            hits = [key for key in ordered if name in qualifies[key] and key not in picked]
            availability[language][name] = sum(name in qualifies[key] for key in ordered)
            if len(hits) < per_class:
                raise BuildError(
                    f"{language} {name}: {len(hits)} files left to be fixtures, not {per_class}"
                )
            picked.update(hits[:per_class])
    return {key: qualifies[key] for key in picked}, availability


def noise_step(plan: dict[str, Any], sources: Path, work: Path) -> None:
    """The noise pool: the first `seconds` of the first `count` recordings at least that
    long, in seeded order, so every clip fits every noise file and the whole pool can be
    screened before anything is mixed. Each is a canonical WAV under WORK/noise with its
    pool by seeded order and the plan's exclusions; the build copies them into the corpus."""
    version = plan["corpus"]["version"]
    spec = plan["noise"]
    excluded = spec["excluded"]
    length = spec["seconds"] * corpus.RATE_16K
    if length < MAX_FILE_SAMPLES:
        raise BuildError("noise excerpts must be at least as long as the longest clip")
    found = []
    short = 0
    for path in sorted((sources / spec["dir"]).glob(spec["glob"])):
        file_id = str(path.relative_to(sources / spec["dir"]).with_suffix(""))
        rate, frames = source_frames(path)
        if rate != corpus.RATE_16K or not frames:
            raise BuildError(f"{file_id}: not 16 kHz mono")
        if frames >= length:
            found.append((order_key(version, "noise-select", file_id), file_id, path))
        else:
            short += 1
    found.sort()
    if len(found) < spec["count"]:
        raise BuildError(
            f"{len(found)} noise recordings are {spec['seconds']} s long, not {spec['count']}"
        )
    items = []
    for _, file_id, path in found[: spec["count"]]:
        samples = read_source_audio(path)[1][:length]
        pool = (
            "tuning"
            if int(order_key(version, "noise-pool", file_id)[:16], 16) % spec["tuning_one_in"] == 0
            else "held_out"
        )
        noise_id = f"{spec['source']}-{slug(file_id)}"
        relative = f"noise/{pool}/{noise_id}.wav"
        data = audio.wav_pcm16_mono_bytes(corpus.RATE_16K, samples)
        (work / relative).parent.mkdir(parents=True, exist_ok=True)
        (work / relative).write_bytes(data)
        items.append(
            {
                "id": noise_id,
                "source": spec["source"],
                "file_id": file_id,
                "path": relative,
                "sha256": hashlib.sha256(data).hexdigest(),
                "size": len(data),
                "samples": len(samples),
                "pool": pool,
                "excluded": file_id in excluded,
                "exclusion_reason": excluded.get(file_id),
            }
        )
    items.sort(key=lambda item: item["id"])
    unknown = set(excluded) - {item["file_id"] for item in items}
    if unknown:
        raise BuildError(f"excluded noise files not found: {sorted(unknown)}")
    selection = [
        {"rule": f"recording shorter than {spec['seconds']} s", "excluded": short},
        {
            "rule": f"after the first {spec['count']} in seeded order",
            "excluded": len(found) - spec["count"],
        },
    ]
    pool_noise = {"selection": selection, "items": items}
    (work / "noise.json").write_text(json.dumps(pool_noise), encoding="utf-8")
    print(json.dumps({"noise": len(items), "excluded": len(excluded)}))


def screen_jobs_step(work: Path) -> None:
    """The aligner's --screen jobs: every noise file of the pool, whole."""
    items = json.loads((work / "noise.json").read_text(encoding="utf-8"))["items"]
    jobs = [
        {
            "id": item["id"],
            "path": str(work / item["path"]),
            "language": "en",
            "text": "",
            "start": 0,
            "frames": item["samples"],
        }
        for item in items
    ]
    (work / "screen-jobs.json").write_text(json.dumps(jobs), encoding="utf-8")


def screen_exclusions(items: list[dict[str, Any]], decoded: list[dict[str, Any]]) -> dict[str, str]:
    """The plan's noise exclusions: a file whose greedy decode has SCREEN_LETTERS or more
    letters may hold speech, and nobody listened to tell."""
    file_ids = {item["id"]: item["file_id"] for item in items}
    excluded = {}
    for result in decoded:
        letters = len(result["text"].replace(" ", ""))
        if letters >= SCREEN_LETTERS:
            excluded[file_ids[result["id"]]] = (
                f"screen: the English aligner's greedy decode gives {letters} letters "
                f"({SCREEN_LETTERS} or more excludes)"
            )
    return dict(sorted(excluded.items()))


def dump_manifest(manifest: dict[str, Any]) -> str:
    """Canonical text: sorted keys, one noise item or file per line."""
    parts = []
    for key in sorted(manifest):
        value = manifest[key]
        if key in ("files", "noise") and value:
            rows = ",\n".join(
                "  " + json.dumps(x, ensure_ascii=False, sort_keys=True) for x in value
            )
            text = "[\n" + rows + "\n ]"
        else:
            text = json.dumps(value, ensure_ascii=False, indent=1, sort_keys=True)
            text = text.replace("\n", "\n ")
        parts.append(f" {json.dumps(key)}: {text}")
    return "{\n" + ",\n".join(parts) + "\n}\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "step", choices=("noise", "screen-jobs", "screen-exclusions", "candidates", "build")
    )
    parser.add_argument("plan", type=Path)
    parser.add_argument("--sources", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument(
        "--root", type=Path, help="where to write the corpus; without it, only select"
    )
    parser.add_argument("--sv56", help="sv56demo binary for the telephony level cross-check")
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--labels", type=Path, help="the labels to select from (WORK/labels.jsonl)")
    args = parser.parse_args(argv)
    plan = json.loads(args.plan.read_text(encoding="utf-8"))
    try:
        if args.step == "noise":
            noise_step(plan, args.sources, args.work)
        elif args.step == "screen-jobs":
            screen_jobs_step(args.work)
        elif args.step == "screen-exclusions":
            items = json.loads((args.work / "noise.json").read_text(encoding="utf-8"))["items"]
            decoded = [
                json.loads(line)
                for line in (args.work / "screen.jsonl").read_text(encoding="utf-8").splitlines()
            ]
            print(json.dumps(screen_exclusions(items, decoded), indent=1, ensure_ascii=False))
        elif args.step == "candidates":
            candidates_step(plan, args.sources, args.work, args.workers)
        else:
            build_step(
                plan, args.sources, args.work, args.root, args.sv56, args.workers, args.labels
            )
    except BuildError as exc:
        print(f"speech-corpus-build: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
