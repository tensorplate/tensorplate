#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check the speech corpus build: its rules, its readers and an end-to-end build.

Synthetic sources in each reader's layout are built into a corpus with labels
written as the aligner writes them. The result must pass the manifest check
with its audio rebuilt, a second build from only the labels it files must
write the same manifest, and each rule is shown withholding the file it names.
"""

from __future__ import annotations

import hashlib
import io
import json
import math
import random
import sys
import tempfile
import wave
from contextlib import redirect_stdout
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/validation"))

import candidate_audio as audio  # noqa: E402
import speech_corpus as corpus  # noqa: E402
import speech_corpus_build as build  # noqa: E402

VERSION = "synthetic-build-0"
SECOND = 16000


def burst(total: int, onset: int, end: int, seed: int) -> list[int]:
    hz = 200 + seed * 37
    return [
        round(6000 * math.sin(2 * math.pi * hz * i / SECOND)) if onset <= i < end else 0
        for i in range(total)
    ]


def write_wav(path: Path, samples: list[int], rate: int = SECOND) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(audio.wav_pcm16_mono_bytes(rate, samples))


def check_conversions() -> None:
    values = [k / 32767 for k in (0.5, 1.5, 2.5, -0.5, -1.5)] + [1.0, -1.0, 1.0001, -1.0001]
    samples, clipped = build.float_to_pcm16(values)
    assert samples == [0, 2, 2, 0, -2, 32767, -32767, 32767, -32768], samples
    assert clipped == 2
    assert build.full_scale_count([32767, -32767, -32768, 32766, -32766, 0]) == 3
    assert build.slug("yes/0a2b_nohash_0") == "yes-0a2b-nohash-0"


def check_semantic_classes() -> None:
    def classes(text: str, language: str = "en", speech: int = SECOND) -> list[str]:
        return build.semantic_classes(text, language, speech)

    assert classes("It was a fine day") == []
    assert classes("He did NOT come") == ["negation"]
    assert classes("They don't know") == ["negation"]
    assert classes("one-hundred percent") == ["numerals"]
    assert classes("In 1886 it rained") == ["numerals"]
    assert classes("very, very far") == ["repetition"]
    assert classes("a b c d a") == [], "a repeat four words back is not within three"
    assert classes("a b c a") == ["repetition"]
    assert classes("long", speech=build.WINDOW_SEAM_SAMPLES) == []
    assert classes("long", speech=build.WINDOW_SEAM_SAMPLES + 1) == ["window_seam"]
    assert classes("لم يأت", "ar") == ["negation"]
    assert classes("ولم يأت", "ar") == ["negation"]
    assert classes("فما", "ar") == ["negation"]
    assert classes("ولد", "ar") == [], "only a leading wa or fa is a prefix"
    assert classes("عندي ثلاثة كتب", "ar") == ["numerals"]
    assert classes("وثلاثة", "ar") == ["numerals"]


def candidate(key: str, **fields: Any) -> dict[str, Any]:
    base = {
        "key": key,
        "source": "s",
        "file_id": key,
        "pool": "held_out",
        "withheld": None,
        "bin": "1to5s",
        "speaker": key,
        "text_group": key,
    }
    base.update(fields)
    return base


def ordered(keys: list[str]) -> list[str]:
    return sorted(keys, key=lambda k: build.order_key(VERSION, "select", "s", k))


def check_select() -> None:
    quota = {"source": "s", "pool": "held_out", "bin": "1to5s", "count": 2}
    keys = ordered([f"f{i}" for i in range(6)])

    def run(cands: list[dict[str, Any]], **caps: Any) -> list[str]:
        q = {**quota, "speaker_cap": 1, "text_cap": None, **caps}
        return [c["key"] for c in build.select(cands, q, VERSION, set())]

    assert run([candidate(k) for k in keys]) == keys[:2]
    same_speaker = [candidate(k, speaker="x" if k in keys[:2] else k) for k in keys]
    assert run(same_speaker) == [keys[0], keys[2]]
    assert run(same_speaker, speaker_cap=2) == keys[:2]
    same_text = [candidate(k, text_group="t" if k in keys[:2] else k) for k in keys]
    assert run(same_text, text_cap=1) == [keys[0], keys[2]]
    assert run(same_text) == keys[:2]
    other_bin = [candidate(k, bin="lt1s" if k == keys[0] else "1to5s") for k in keys]
    assert run(other_bin) == keys[1:3]
    withheld = [candidate(k, withheld="rule" if k == keys[1] else None) for k in keys]
    assert run(withheld) == [keys[0], keys[2]]
    taken = [candidate(k) for k in keys]
    q = {**quota, "speaker_cap": 1, "text_cap": None}
    assert [c["key"] for c in build.select(taken, q, VERSION, {keys[0]})] == keys[1:3]
    late = [candidate(k) for k in keys]
    del late[3]["bin"]
    assert run(late) == keys[:2], "a file after the quota fills needs no label"
    early = [candidate(k) for k in keys]
    del early[1]["bin"]
    expect_error(lambda: run(early), "not labelled yet")
    expect_error(lambda: run([candidate(keys[0])]), "1 eligible of 2")


def expect_error(call: Any, reason: str) -> None:
    try:
        call()
    except build.BuildError as exc:
        assert reason in str(exc), f"expected {reason!r}, got {exc}"
    else:
        raise AssertionError(f"no error: {reason}")


def check_noise_choice() -> None:
    noise = [
        {"id": "n-a", "pool": "held_out", "excluded": False, "samples": 100},
        {"id": "n-b", "pool": "held_out", "excluded": True, "samples": 100},
        {"id": "n-c", "pool": "tuning", "excluded": False, "samples": 100},
        {"id": "n-d", "pool": "held_out", "excluded": False, "samples": 50},
    ]
    for clip in ("c1", "c2", "c3", "c4"):
        assert build.choose_noise(noise, "held_out", 60, VERSION, clip)["id"] == "n-a"
        assert build.choose_noise(noise, "tuning", 60, VERSION, clip)["id"] == "n-c"
    picks = {build.choose_noise(noise, "held_out", 50, VERSION, f"c{i}")["id"] for i in range(20)}
    assert picks == {"n-a", "n-d"}, picks
    expect_error(lambda: build.choose_noise(noise, "held_out", 101, VERSION, "c1"), "no held_out")


def first_key(
    prefix: str, below: str | None = None, above: str | None = None, *, source: str
) -> str:
    """The first `<prefix><n>` whose seeded selection order falls below or above a key."""
    for n in range(10000):
        name = f"{prefix}{n:04d}"
        key = build.order_key(VERSION, "select", source, name)
        if (below is None or key < below) and (above is None or key > above):
            return name
    raise AssertionError(f"no {prefix} name in range")


def check_fixtures() -> None:
    def entry(key: str, text: str, pool: str = "held_out", seconds: float = 2) -> dict[str, Any]:
        return {
            "key": key,
            "language": key.split("/")[0],
            "pool": pool,
            "text": text,
            "onset": 0,
            "end": int(seconds * SECOND),
        }

    def order(key: str) -> str:
        return build.order_key(VERSION, "semantic", key)

    english = [
        entry("en/neg", "not now"),
        entry("en/num", "two cats"),
        entry("en/rep", "go go"),
        entry("en/seam", "long", seconds=9),
        entry("en/tune", "not here", pool="tuning"),
    ]
    first = min(order(e["key"]) for e in english)
    both = next(k for k in (f"en/both{n}" for n in range(1000)) if order(k) < first)
    english.append(entry(both, "not two"))
    arabic = [
        entry("ar/neg", "لا"),
        entry("ar/num", "ثلاثة"),
        entry("ar/rep", "نعم نعم"),
        entry("ar/seam", "طويل", seconds=9),
    ]
    fixtures, availability = build._fixtures(
        {"semantic": {"per_class": 1}}, english + arabic, VERSION
    )
    # The file meeting two classes comes first, so negation takes it and numerals the next.
    assert sorted(fixtures) == sorted(
        [both, "en/num", "en/rep", "en/seam", "ar/neg", "ar/num", "ar/rep", "ar/seam"]
    ), sorted(fixtures)
    assert fixtures[both] == ["negation", "numerals"]
    assert availability["en"] == {"negation": 2, "numerals": 2, "repetition": 1, "window_seam": 1}
    assert availability["ar"] == {"negation": 1, "numerals": 1, "repetition": 1, "window_seam": 1}
    expect_error(
        lambda: build._fixtures({"semantic": {"per_class": 2}}, english + arabic, VERSION),
        "files left to be fixtures, not 2",
    )


def check_readers(base: Path) -> None:
    slr = base / "slr/data"
    (slr / "transcripts/train").mkdir(parents=True)
    (slr / "transcripts/train/recordings.tsv").write_text("1\tPROMPT ONE\n2\tPROMPT TWO \n")
    (slr / "transcripts/train/answers.tsv").write_text("S_7_Answers_Arabic_5\tAN ANSWER\n")
    for kind, names in (("Recordings_Arabic", ["1", "2"]), ("Answers_Arabic", ["5", "6"])):
        for speaker in ("7", "8"):
            for name in names:
                write_wav(slr / "speech/train/S" / kind / speaker / f"{name}.wav", [0] * 160)
    split = {"dir": "slr/data", "part": "train", "speakers": ["S/7"], "prompts": ["1", "2"]}
    rows = list(build.read_openslr46(base, {**split, "answers": True}))
    assert [(r[0], r[2], r[4], r[5]) for r in rows] == [
        ("train/S/Recordings_Arabic/7/1", "PROMPT ONE", "PROMPT ONE", "S/7"),
        ("train/S/Recordings_Arabic/7/2", "PROMPT TWO", "PROMPT TWO", "S/7"),
        ("train/S/Answers_Arabic/7/5", "AN ANSWER", "AN ANSWER", "S/7"),
    ], rows
    assert rows[0][1] == "slr/data/speech/train/S/Recordings_Arabic/7/1.wav"
    rows = list(build.read_openslr46(base, {**split, "prompts": ["2"], "answers": False}))
    assert [r[0] for r in rows] == ["train/S/Recordings_Arabic/7/2"], "a split reads its prompts"
    asc = base / "asc"
    asc.mkdir()
    (asc / "test.csv").write_text(
        "file,class\n"
        "dataset/yes/00000003_NO_01.wav,yes\n"
        "dataset/zoom in/00000004_NO_02.wav,zoom in\n"
        "dataset/no/00000003_NO_01.wav,no\n"
    )
    texts = {"yes": "نعم", "zoom in": "تكبير"}
    rows = list(
        build.read_arabic_speech_commands(
            base, {"list": "asc/test.csv", "dir": "asc", "texts": texts}
        )
    )
    assert rows == [
        (
            "yes/00000003_NO_01",
            "asc/dataset/yes/00000003_NO_01.wav",
            "نعم",
            "نعم",
            "00000003/yes",
            "00000003",
        ),
        (
            "zoom in/00000004_NO_02",
            "asc/dataset/zoom in/00000004_NO_02.wav",
            "تكبير",
            "تكبير",
            "00000004/zoom in",
            "00000004",
        ),
    ], rows


class Sources:
    """Synthetic sources in each reader's layout, with the speech span of every file."""

    def __init__(self, base: Path) -> None:
        self.base = base
        self.spans: dict[str, tuple[int, int]] = {}
        self.seed = 0

    def audio(self, key: str, path: Path, seconds: float, onset: float, end: float) -> None:
        self.seed += 1
        span = (int(onset * SECOND), int(end * SECOND))
        write_wav(path, burst(int(seconds * SECOND), *span, self.seed))
        self.spans[key] = span

    def librispeech(self, split: str, utterances: list[tuple[str, str, float]]) -> None:
        listings: dict[Path, list[str]] = {}
        for uid, text, seconds in utterances:
            speaker, chapter, _ = uid.split("-")
            folder = self.base / "ls" / split / speaker / chapter
            # The reader names FLAC files; the build reads a PCM WAV whatever its name.
            self.audio(f"ls-{split}/{uid}", folder / f"{uid}.flac", seconds, 0.3, seconds - 0.3)
            listings.setdefault(folder / f"{speaker}-{chapter}.trans.txt", []).append(
                f"{uid} {text}"
            )
        for path, lines in listings.items():
            path.write_text("\n".join(lines) + "\n", encoding="utf-8")


# Held-out LibriSpeech files that stay usable; the label-rule files are named to come
# before all of them in the seeded order, so the selection examines them.
USABLE = ["11-1-0000", "11-1-0001", "12-1-0000", "13-1-0002", "16-1-0000"]
FIRST_USABLE = min(build.order_key(VERSION, "select", "ls-test", u) for u in USABLE)
EDGE_OOV = first_key("14-1-", below=FIRST_USABLE, source="ls-test")
UNALIGNED = first_key("14-2-", below=FIRST_USABLE, source="ls-test")
AT_START = first_key("15-1-", below=FIRST_USABLE, source="ls-test")
AT_END = first_key("15-2-", below=FIRST_USABLE, source="ls-test")
GSC_USABLE = ["yes/aa01_nohash_0", "yes/aa01_nohash_1", "yes/aa02_nohash_0", "no/aa03_nohash_0"]
LAST_GSC = max(build.order_key(VERSION, "select", "gsc", u) for u in GSC_USABLE)
# An unaligned keyword after every usable one: the quotas fill before it.
GSC_LATE = first_key("yes/zz", above=LAST_GSC, source="gsc")


def make_sources(base: Path) -> Sources:
    s = Sources(base)
    s.librispeech(
        "test",
        [
            ("11-1-0000", "HE DID NOT COME", 3.0),
            ("11-1-0001", "THE THE CAT", 7.0),
            ("12-1-0000", "TWO BIRDS", 3.0),
            ("12-1-0001", "SHARED TEXT", 3.0),
            ("13-1-0000", "A LONG ONE", 30.0 + 1 / SECOND),
            ("13-1-0001", "A LOUD ONE", 3.0),
            ("13-1-0002", "NEARLY LOUD", 3.0),
            ("13-1-0003", "SHARED TEXT", 31.0),
            (EDGE_OOV, "EDGE WORD", 3.0),
            (UNALIGNED, "NOT ALIGNED", 3.0),
            (AT_START, "AT THE START", 3.0),
            (AT_END, "AT THE END", 3.0),
            ("16-1-0000", "ONE FRAME IN", 3.0),
        ],
    )
    s.librispeech(
        "dev",
        [
            ("21-1-0000", "SHARED TEXT", 3.0),
            ("21-1-0001", "DO NOT TUNE ME", 3.0),
            ("22-1-0000", "THIRTY SECONDS", 30.0),
            ("23-1-0000", "A LONG ONE", 3.0),
        ],
    )
    s.librispeech("extra", [("31-1-0000", "EXTRA WORDS", 3.0)])
    loud = base / "ls/test/13/1/13-1-0001.flac"
    samples = audio.read_wav_pcm16_mono(loud)[1]
    write_wav(loud, samples[:100] + [32767] * 5 + [-32767] * 5 + samples[110:])
    nearly = base / "ls/test/13/1/13-1-0002.flac"
    samples = audio.read_wav_pcm16_mono(nearly)[1]
    write_wav(nearly, samples[:100] + [32767] * 9 + samples[109:])
    rows = []
    for n, (sentence, seconds) in enumerate([("alpha", 9.0), ("alpha", 9.0), ("beta", 6.5)]):
        name = f"{900 + n}.wav"
        s.audio(f"fl/{900 + n}", base / "fl/test" / name, seconds, 0.4, seconds - 0.2)
        raw = f"Sentence {sentence}, {'not ' if sentence == 'beta' else ''}read."
        rows.append(f"{sentence}\t{name}\t{raw}\tsentence {sentence} read\tx\t0\tFEMALE")
    (base / "fl/test.tsv").write_text("\n".join(rows) + "\n", encoding="utf-8")
    lists: dict[str, list[str]] = {"held": [], "tune": []}
    for line, pool in [(u, "held") for u in GSC_USABLE + [GSC_LATE]] + [
        ("up/bb01_nohash_0", "tune"),
        ("off/bb02_nohash_0", "tune"),
    ]:
        s.audio(f"gsc/{line}", base / "gsc" / f"{line}.wav", 1.0, 0.2, 0.7)
        lists[pool].append(f"{line}.wav")
    write_wav(base / "gsc/yes/cc01_nohash_0.wav", [100] * 8000, rate=8000)
    with wave.open(str(base / "gsc/no/cc02_nohash_0.wav"), "wb") as stereo:
        stereo.setnchannels(2)
        stereo.setsampwidth(2)
        stereo.setframerate(SECOND)
        stereo.writeframes(audio.pcm16_bytes([100] * 2 * SECOND))
    lists["held"] += ["yes/cc01_nohash_0.wav", "no/cc02_nohash_0.wav"]
    for pool, entries in lists.items():
        (base / f"gsc/{pool}.txt").write_text("\n".join(entries) + "\n", encoding="utf-8")
    generator = random.Random(7)
    for n in range(8):
        noise = [generator.randint(-2000, 2000) for _ in range(int((28 + n) * SECOND))]
        write_wav(base / "noise/files" / f"n{n}.wav", noise)
    write_wav(base / "noise/bad/eight-k.wav", [0] * 8000 * 31, rate=8000)
    with wave.open(str(base / "noise/stereo.wav"), "wb") as stereo:
        stereo.setnchannels(2)
        stereo.setsampwidth(2)
        stereo.setframerate(SECOND)
        stereo.writeframes(audio.pcm16_bytes([0] * 2 * 31 * SECOND))
    return s


def source(
    source_id: str, reader: str | None, splits: list[dict[str, Any]], public: bool = True
) -> dict[str, Any]:
    entry = {
        "id": source_id,
        "dataset": source_id,
        "revision": "synthetic",
        "url": "https://example.invalid/",
        "license": "CC-BY-4.0",
        "license_statement": "synthetic",
        "attribution": "synthetic",
        "condition": "synthetic",
        "transcripts_public": public,
        "language": "en",
        "speaker_namespace": reader or "noise",
        "splits": splits,
    }
    if reader:
        entry["reader"] = reader
    return entry


def make_plan() -> dict[str, Any]:
    quotas = [
        ("held_out", "ls-test", "1to5s", 4, 1, None),
        ("held_out", "ls-test", "gt5to30s", 1, 1, None),
        ("held_out", "fl", "gt5to30s", 2, 0, 1),
        ("held_out", "gsc", "lt1s", 2, 1, 1),
        ("tuning", "ls-dev", "1to5s", 2, 1, None),
        ("tuning", "gsc", "lt1s", 2, 1, None),
    ]
    return {
        "corpus": {"name": "synthetic", "version": VERSION},
        "recipes": {
            "telephony8k": {
                "recipe": "tp-telephony8k-fir63-g711stl-v1",
                "level_dbov": -26,
                "level_method": "span_rms",
            },
            "noisy16k": {
                "snr_db": [5, 10, 15, 20],
                "snr_rule": "round-robin-sorted-clean-id-v1",
                "seed_rule": "sha256-version-noisy16k-clip-v1",
            },
        },
        "aligners": {"en": "synthetic-aligner"},
        "semantic": {"per_class": 1},
        "source_fields": [
            "id",
            "dataset",
            "revision",
            "url",
            "license",
            "license_statement",
            "attribution",
            "condition",
            "transcripts_public",
        ],
        "sources": [
            source("ls-test", "librispeech", [{"pool": "held_out", "dir": "ls/test"}]),
            source("ls-dev", "librispeech", [{"pool": "tuning", "dir": "ls/dev"}]),
            source(
                "fl",
                "fleurs",
                [{"pool": "held_out", "list": "fl/test.tsv", "dir": "fl/test"}],
                public=False,
            ),
            source("ls-extra", "librispeech", [{"pool": "tuning", "dir": "ls/extra"}]),
            source(
                "gsc",
                "speech_commands",
                [
                    {
                        "pool": "held_out",
                        "list": "gsc/held.txt",
                        "dir": "gsc",
                        "texts": {"yes": "yes", "no": "no"},
                    },
                    {
                        "pool": "tuning",
                        "list": "gsc/tune.txt",
                        "dir": "gsc",
                        "texts": {"up": "up", "off": "off"},
                    },
                ],
            ),
            source("noise-src", None, []),
        ],
        "noise": {
            "source": "noise-src",
            "dir": "noise",
            "glob": "files/*.wav",
            "tuning_one_in": 2,
            "count": 5,
            "seconds": 30,
            "excluded": {},
        },
        "quotas": [
            {"pool": p, "source": src, "bin": b, "count": n, "speaker_cap": sc, "text_cap": tc}
            for p, src, b, n, sc, tc in quotas
        ],
    }


def check_noise_step(plan: dict[str, Any], base: Path, work: Path) -> str:
    """Builds the noise pool with one exclusion and returns the excluded file's id."""
    spec = plan["noise"]
    for change, reason in (
        ({"seconds": 29}, "at least as long as the longest clip"),
        ({"seconds": 36}, "0 noise recordings are 36 s long, not 5"),
        ({"excluded": {"files/x": "r"}}, "excluded noise files not found"),
        ({"glob": "bad/*.wav"}, "bad/eight-k: not 16 kHz mono"),
        ({"glob": "stereo.wav"}, "stereo: not 16 kHz mono"),
    ):
        expect_error(
            lambda change=change: build.noise_step(
                {**plan, "noise": {**spec, **change}}, base, work
            ),
            reason,
        )
    build.noise_step(plan, base, work)
    pool_noise = json.loads((work / "noise.json").read_text(encoding="utf-8"))
    assert pool_noise["selection"] == [
        {"rule": "recording shorter than 30 s", "excluded": 2},
        {"rule": "after the first 5 in seeded order", "excluded": 1},
    ], pool_noise["selection"]
    noise = pool_noise["items"]
    # n0 and n1 are shorter than 30 s; five of the six others are taken in seeded order.
    expected = sorted(
        (f"files/n{n}" for n in range(2, 8)),
        key=lambda i: build.order_key(VERSION, "noise-select", i),
    )[:5]
    assert sorted(n["file_id"] for n in noise) == sorted(expected)
    for item in noise:
        data = (work / item["path"]).read_bytes()
        assert item["samples"] == 30 * SECOND == len(audio.pcm16_samples(data[44:]))
        assert hashlib.sha256(data).hexdigest() == item["sha256"] and item["size"] == len(data)
        tuning = int(build.order_key(VERSION, "noise-pool", item["file_id"])[:16], 16) % 2 == 0
        assert item["pool"] == ("tuning" if tuning else "held_out")
    excluded = next(n for n in noise if n["pool"] == "held_out")
    build.screen_jobs_step(work)
    jobs = json.loads((work / "screen-jobs.json").read_text(encoding="utf-8"))
    assert [(j["id"], j["start"], j["frames"]) for j in jobs] == [
        (n["id"], 0, n["samples"]) for n in noise
    ]
    decoded = [
        {"id": n["id"], "text": "AB CDE FGHIJ" if n is excluded else "ABC DEFGHI"} for n in noise
    ]
    exclusions = build.screen_exclusions(noise, decoded)
    assert list(exclusions) == [excluded["file_id"]], "ten letters exclude, nine do not"
    spec["excluded"] = exclusions
    build.noise_step(plan, base, work)
    noise = json.loads((work / "noise.json").read_text(encoding="utf-8"))["items"]
    assert [n["id"] for n in noise if n["excluded"]] == [excluded["id"]]
    return excluded["id"]


def write_labels(work: Path, s: Sources) -> list[dict[str, Any]]:
    jobs = json.loads((work / "align-jobs.json").read_text(encoding="utf-8"))
    lines = []
    for job in jobs:
        onset, end = s.spans[job["id"]]
        result: dict[str, Any] = {
            "aligned": True,
            "edge_oov": False,
            "speech_onset_sample": onset,
            "speech_end_sample": end,
            "score": 0.9,
        }
        name = job["id"].split("/", 1)[1]
        if name == EDGE_OOV:
            result["edge_oov"] = True
        if name in (UNALIGNED, GSC_LATE):
            result = {"aligned": False, "edge_oov": False}
        edges = {AT_START: (319, end), AT_END: (onset, 48000 - 319), "16-1-0000": (320, 47680)}
        if name in edges:
            result["speech_onset_sample"], result["speech_end_sample"] = edges[name]
        lines.append(json.dumps({"id": job["id"], **result}))
    (work / "labels.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return jobs


def check_sv56(tmp: Path, plan: dict[str, Any], base: Path, work: Path) -> None:
    # The fake reports the peak of the signal it is given, so the level is shown to be
    # measured on the levelled 16 kHz samples.
    fake = tmp / "sv56demo"
    fake.write_text(
        "#!/usr/bin/env python3\n"
        "import struct, sys\n"
        "assert sys.argv[1:4] == ['-q', '-sf', '16000'] and sys.argv[5] == '/dev/null'\n"
        "data = open(sys.argv[4], 'rb').read()\n"
        "peak = max(abs(v) for v in struct.unpack(f'<{len(data) // 2}h', data))\n"
        "print(f'  Active speech level: .......... -{peak / 1000:.3f} [dBov]')\n"
    )
    fake.chmod(0o755)
    silent = tmp / "sv56-silent"
    silent.write_text("#!/bin/sh\necho 'no level'\n")
    silent.chmod(0o755)
    expect_error(lambda: build._sv56_dbov(str(silent), [1, -1]), "no active speech level")
    root = tmp / "corpus-sv56"
    build.build_step(plan, base, work, root, str(fake), 2)
    manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    files = {f["id"]: f for f in manifest["files"]}
    for entry in files.values():
        if entry["codec"] != "telephony8k":
            continue
        clean = audio.read_wav_pcm16_mono(root / files[entry["derived_from"]]["path"])[1]
        gain = entry["recipe"]["level_gain_q30"]
        peak = max(abs(corpus.apply_gain(v, gain)) for v in clean)
        assert entry["recipe"]["sv56_dbov"] == float(f"-{peak / 1000:.3f}"), entry["id"]
    corpus.check_manifest(manifest, root=root, rebuild=True, frozen=True)


def check_build() -> None:
    with tempfile.TemporaryDirectory(prefix="tp-speech-corpus-build-") as tmp:
        base, work, root = Path(tmp) / "sources", Path(tmp) / "work", Path(tmp) / "corpus"
        s = make_sources(base)
        plan = make_plan()
        excluded_noise = check_noise_step(plan, base, work)
        build.candidates_step(plan, base, work, 2)
        found = {c["key"]: c for c in json.loads((work / "candidates.json").read_text())}
        withheld = {k: c["withheld"] for k, c in found.items() if c["withheld"]}
        assert withheld == {
            "ls-test/13-1-0000": build.RULE_LONG,
            "ls-test/13-1-0001": build.RULE_FULL_SCALE,
            "ls-test/12-1-0001": build.RULE_TEXT_POOLS,
            "ls-test/13-1-0003": build.RULE_LONG,
            "ls-dev/21-1-0000": build.RULE_TEXT_POOLS,
            "gsc/yes/cc01_nohash_0": build.RULE_FORMAT,
            "gsc/no/cc02_nohash_0": build.RULE_FORMAT,
        }, withheld
        jobs = write_labels(work, s)
        assert not [j for j in jobs if j["id"].startswith("ls-extra/")], "a split with no quota"
        assert {j["text"] for j in jobs if j["id"].startswith("fl/")} == {
            "sentence alpha read",
            "sentence beta read",
        }, "the aligner gets the alignment text"
        names = [(found[j["id"]]["source"], found[j["id"]]["pool"]) for j in jobs]
        # Interleaved in proportion to each source and pool's quotas: 5, 2, 2, 2 and 2 files.
        assert names[:6] == [
            ("ls-test", "held_out"),
            ("ls-test", "held_out"),
            ("fl", "held_out"),
            ("gsc", "held_out"),
            ("gsc", "tuning"),
            ("ls-dev", "tuning"),
        ], names[:6]
        for name in set(names):
            group = [found[j["id"]] for j, n in zip(jobs, names, strict=True) if n == name]
            assert group == sorted(group, key=lambda c: build.select_order(VERSION, c))
        build.build_step(plan, base, work, None, None, 2)
        build.build_step(plan, base, work, root, None, 2)
        text = (root / "manifest.json").read_text(encoding="utf-8")
        manifest = json.loads(text)
        assert text == build.dump_manifest(manifest)
        assert corpus.check_manifest(manifest, root=root, rebuild=True) == 39
        files = {f["id"]: f for f in manifest["files"]}
        clean = [f for f in files.values() if f["codec"] == "clean16k"]
        chosen = {f["source"]["file_ids"][0] for f in clean}
        assert {"13-1-0002", "16-1-0000"} <= chosen, "nine full-scale samples, a frame in"
        assert not chosen & {EDGE_OOV, UNALIGNED, AT_START, AT_END}
        fleurs = [f["transcript"] for f in clean if f["source"]["source"] == "fl"]
        assert {t["sha256"] for t in fleurs} == {
            hashlib.sha256(b"Sentence alpha, read.").hexdigest(),
            hashlib.sha256(b"Sentence beta, not read.").hexdigest(),
        }, "the text cap takes one recording per sentence"
        assert all(t["text"] is None for t in fleurs), (
            "a restricted source's text is held by digest"
        )
        selection = {x["id"]: x["selection"] for x in manifest["sources"]}
        assert selection["ls-test"] == [
            {"rule": build.RULE_FULL_SCALE, "excluded": 1},
            {"rule": build.RULE_LONG, "excluded": 2},
            {"rule": build.RULE_UNALIGNED, "excluded": 2},
            {"rule": build.RULE_EDGE, "excluded": 2},
            {"rule": build.RULE_TEXT_POOLS, "excluded": 1},
        ], selection["ls-test"]
        assert selection["fl"] == []
        assert selection["gsc"] == [{"rule": build.RULE_FORMAT, "excluded": 2}], (
            "a label rule counts only the files a quota examined"
        )
        assert selection["noise-src"][0] == {"rule": "recording shorter than 30 s", "excluded": 2}
        filed = [json.loads(line) for line in (root / "labels.jsonl").read_text().splitlines()]
        assert [x["id"] for x in filed] == sorted(x["id"] for x in filed)
        assert all(sorted(x) == sorted(("id", *build.LABEL_FIELDS)) for x in filed)
        filed_ids = {x["id"] for x in filed}
        assert {f"ls-test/{n}" for n in (EDGE_OOV, UNALIGNED, AT_START, AT_END)} <= filed_ids
        assert f"gsc/{GSC_LATE}" not in filed_ids
        for f in clean:
            speaker = f["source"]["speaker"]
            if f["source"]["source"].startswith("ls-"):
                number = f["source"]["file_ids"][0].split("-")[0]
                key = build.order_key(VERSION, "speaker", "librispeech", number)
                assert speaker == "s" + key[:12], "one key per dataset speaker, across archives"
            assert f["labels"]["tool"] == "synthetic-aligner"
        fixtures = [f for f in clean if f["semantic"]]
        assert len(fixtures) == 4 and all(f["pool"] == "held_out" for f in fixtures)
        assert {c for f in fixtures for c in f["semantic"]} == set(build.CLASSES)
        listed = {n["id"] for n in manifest["noise"]}
        mixed = {f["recipe"]["noise"] for f in files.values() if f["codec"] == "noisy16k"}
        assert excluded_noise in listed and mixed <= listed
        assert listed - mixed == {excluded_noise}, "only mixed or excluded noise is listed"
        assert sorted(p.name for p in root.iterdir()) == [
            "held_out",
            "labels.jsonl",
            "manifest.json",
            "noise",
            "tuning",
        ]
        expect_error(lambda: build.build_step(plan, base, work, root, None, 2), "is not empty")
        check_sv56(Path(tmp), plan, base, work)
        lines = (work / "labels.jsonl").read_text(encoding="utf-8").splitlines()
        (work / "labels.jsonl").write_text("\n".join(lines[:3]) + "\n", encoding="utf-8")
        expect_error(lambda: build.build_step(plan, base, work, None, None, 2), "quotas unfilled")
        again = Path(tmp) / "again"
        build.build_step(plan, base, work, again, None, 2, root / "labels.jsonl")
        assert (again / "manifest.json").read_text(encoding="utf-8") == text, (
            "the filed labels alone select the same corpus"
        )


def main() -> int:
    check_conversions()
    check_semantic_classes()
    check_select()
    check_noise_choice()
    check_fixtures()
    with tempfile.TemporaryDirectory(prefix="tp-speech-corpus-readers-") as tmp:
        check_readers(Path(tmp))
    with redirect_stdout(io.StringIO()):
        check_build()
    print("speech corpus build: rules, readers and an end-to-end build passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
