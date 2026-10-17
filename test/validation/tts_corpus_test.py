#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check the text-to-speech corpus check, its counting and its reference recorder.

The filed corpus must pass. A synthetic corpus then breaks each rule of the
check once, with a refusal that has to name that rule. Counting and the
reference arm run against fakes of kokoro and torch that hold the calls they
receive, so the tests see the runner's phonemizer construction, the seed set
before each clip and the clips' bytes without the heavy dependencies.
"""

from __future__ import annotations

import copy
import hashlib
import importlib.metadata
import io
import json
import math
import shutil
import subprocess
import sys
import tarfile
import tempfile
import types
import wave
from collections.abc import Callable
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools/validation/tts_corpus.py"
FILED = ROOT / "tools/validation/corpus/tts-text-en-us.json"
sys.path.insert(0, str(ROOT / "tools/validation"))

import jsonschema  # noqa: E402
import tts_corpus as tts  # noqa: E402

LOCK_BYTES = (ROOT / "packaging/speech-runtime/lock/kokoro.txt").read_bytes()
G2P_PINS = (
    "addict",
    "en-core-web-sm",
    "espeakng-loader",
    "kokoro",
    "misaki",
    "num2words",
    "phonemizer-fork",
    "regex",
    "spacy",
)


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def lock_lines(lock: bytes) -> dict[str, str]:
    return {line.split("==")[0]: line for line in lock.decode().splitlines() if "==" in line}


def pins_sha(lock: bytes) -> str:
    """The g2p pins digest, written out: the named lines, whitespace collapsed, in order."""
    lines = lock_lines(lock)
    return sha("".join(" ".join(lines[name].split()) + "\n" for name in G2P_PINS).encode())


def relock(path: Path, name: str, line: str | None) -> None:
    """Replace one pin's line in a lock file, or drop it."""
    kept = []
    for old in path.read_text(encoding="utf-8").splitlines():
        if old.split("==")[0] != name:
            kept.append(old)
        elif line is not None:
            kept.append(line)
    path.write_text("".join(f"{k}\n" for k in kept), encoding="utf-8")


def rehash(line: str) -> str:
    return line.split("--hash=sha256:")[0] + "--hash=sha256:" + "0" * 64


LOCK_VERSIONS = {
    name: line.split("==")[1].split()[0] for name, line in lock_lines(LOCK_BYTES).items()
}


def word(index: int) -> str:
    letters = ""
    index += 1
    while index:
        index, rest = divmod(index - 1, 26)
        letters = "abcdefghijklmnopqrstuvwxyz"[rest] + letters
    return letters


def fake_phonemes(count: int) -> str:
    """A phoneme string of `count` symbols with spaces and punctuation, all of them counted."""
    return ("h a, o. " * count)[:count]


FAKE_VOCAB = {symbol: index for index, symbol in enumerate("h a,o.", 1)}


def synthetic() -> dict[str, Any]:
    sentences = []
    for index in range(330):
        group, position = divmod(index, 110)
        low, span = ((1, 20), (21, 80), (101, 100))[group]
        tokens = low + position % span
        text = f"Synthetic {word(index)} sentence."
        if position < 12:
            text = f"Do not {word(index)} it."
        elif position < 24:
            text = f"Room {index} {word(index)}."
        source, source_id = (
            ("authored", None),
            ("cmu-arctic", f"arctic_a{index:04d}"),
            ("libritts-r", f"1_2_3_{index}"),
        )[group]
        sentences.append(
            {
                "id": f"s-{index}",
                "text": text,
                "source": source,
                "source_id": source_id,
                "phoneme_tokens": tokens,
                "stratum": tts.stratum(tokens),
                "critical": tts.critical_classes(text),
            }
        )
    listening = [
        f"s-{group * 110 + offset}"
        for group in range(3)
        for offset in (0, 12, 30, 31, 32, 33, 34, 35, 36, 37)
    ]
    queued = [
        {"id": f"q-{i}", "segments": [f"s-{220 + i}", f"s-{i}"] + ([f"s-{i + 1}"] if i % 2 else [])}
        for i in range(100)
    ]
    return {
        "schema_version": "0.1",
        "corpus": {"name": "synthetic", "version": "synthetic-0"},
        "language": "en-US",
        "voice": "af_heart",
        "tokenizer": {
            "rule": "kokoro-kpipeline-g2p-length-v1",
            "lock": "packaging/speech-runtime/lock/kokoro.txt",
            "g2p_pins_sha256": pins_sha(LOCK_BYTES),
            "kokoro": "0.9.4",
            "misaki": "0.9.4",
            "spacy": "3.8.16",
            "en_core_web_sm": "3.8.0",
            "espeak_ng": "1.51+dfsg-12build1",
        },
        "sources": [
            {
                "id": "cmu-arctic",
                "title": "a",
                "license": "CMU-ARCTIC",
                "attribution": "a, tools/validation/corpus/cmu-arctic-COPYING.txt",
                "url": "http://example.invalid/a",
                "sha256": "1" * 64,
            },
            {
                "id": "libritts-r",
                "title": "l",
                "license": "CC-BY-4.0",
                "attribution": "l",
                "url": "http://example.invalid/l",
                "sha256": "2" * 64,
                "text_form": "original",
            },
            {
                "id": "authored",
                "title": "o",
                "license": "CC0-1.0",
                "attribution": "o",
                "url": None,
                "sha256": None,
            },
        ],
        "sentences": sentences,
        "listening": listening,
        "queued_sequences": queued,
    }


def sentence(c: dict[str, Any], name: str) -> dict[str, Any]:
    return next(s for s in c["sentences"] if s["id"] == name)


def edit(name: str, **fields: Any) -> Callable[[dict[str, Any]], None]:
    return lambda c: sentence(c, name).update(fields)


def recount(name: str, tokens: int) -> Callable[[dict[str, Any]], None]:
    return edit(name, phoneme_tokens=tokens, stratum=tts.stratum(tokens))


def retext(name: str, text: str) -> Callable[[dict[str, Any]], None]:
    return edit(name, text=text, critical=tts.critical_classes(text))


def source(name: str, **fields: Any) -> Callable[[dict[str, Any]], None]:
    return lambda c: next(s for s in c["sources"] if s["id"] == name).update(fields)


def drop(names: set[str]) -> Callable[[dict[str, Any]], None]:
    def mutate(c: dict[str, Any]) -> None:
        c["sentences"] = [s for s in c["sentences"] if s["id"] not in names]
        c["listening"] = [n for n in c["listening"] if n not in names]
        c["queued_sequences"] = [
            q for q in c["queued_sequences"] if not names.intersection(q["segments"])
        ]

    return mutate


def refused(
    good: dict[str, Any], mutate: Callable[[dict[str, Any]], None], expect: str, **kwargs: Any
) -> None:
    broken = copy.deepcopy(good)
    mutate(broken)
    try:
        tts.check_corpus(broken, **kwargs)
    except tts.CorpusError as exc:
        assert expect in str(exc), f"expected {expect!r}, got {exc}"
    else:
        raise AssertionError(f"accepted a corpus that should fail with {expect!r}")


def accepted(good: dict[str, Any], mutate: Callable[[dict[str, Any]], None]) -> None:
    changed = copy.deepcopy(good)
    mutate(changed)
    assert tts.check_corpus(changed) == len(changed["sentences"])


def check_filed() -> None:
    filed = json.loads(FILED.read_bytes())
    assert tts.check_corpus(filed) == len(filed["sentences"])
    summary = tts.summarize(filed)
    assert all(count >= 100 for count in summary["per_stratum"].values()), summary
    assert set(summary["per_stratum"]) == {"t1to20", "t21to100", "t101to200"}


def check_derivations() -> None:
    assert [tts.stratum(t) for t in (1, 20, 21, 100, 101, 200)] == [
        "t1to20",
        "t1to20",
        "t21to100",
        "t21to100",
        "t101to200",
        "t101to200",
    ]
    for tokens in (0, 201):
        try:
            tts.stratum(tokens)
        except tts.CorpusError:
            continue
        raise AssertionError(f"{tokens} tokens has a stratum")
    assert tts.critical_classes("I can\u2019t go.") == ["negation"]
    assert tts.critical_classes("Nothing, at 9.") == ["negation", "numeral"]
    assert tts.critical_classes("Notice the knot, at nine.") == []
    assert tts.critical_classes("Gate \u0662.") == ["numeral"]
    assert tts.critical_classes("Square \u00b2 and \u2460.") == []
    for negation in (
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
    ):
        assert tts.critical_classes(f"{negation.capitalize()} at all.") == ["negation"], negation
    assert tts.reference_seed("v", "a") == int.from_bytes(
        hashlib.sha256(b"v/reference/a").digest()[:8], "big"
    )
    pcm, clipped = tts.pcm16(
        [1.5, -1.5, 1.0, -1.0, 0.5, -0.25, 1 / 65536, 3 / 65536, -1 / 65536, 1e308, -1e308]
    )
    values = [int.from_bytes(pcm[i : i + 2], "little", signed=True) for i in range(0, len(pcm), 2)]
    assert values == [32767, -32768, 32767, -32768, 16384, -8192, 0, 2, 0, 32767, -32768], values
    assert clipped == 4
    for bad in (math.nan, math.inf):
        try:
            tts.pcm16([0.0, bad])
        except tts.CorpusError:
            continue
        raise AssertionError("nonfinite audio was converted")


def check_rules(good: dict[str, Any]) -> None:
    assert tts.check_corpus(good) == 330
    refused(good, lambda c: c.update(extra=1), "does not match its schema")
    assert tts.G2P_PINS == G2P_PINS
    refused(
        good, lambda c: c["tokenizer"].update(g2p_pins_sha256="0" * 64), "counted under g2p pins"
    )
    refused(
        good,
        lambda c: c["tokenizer"].update(g2p_pins_sha256="0" * 64),
        f"{tts.LOCK}'s {', '.join(G2P_PINS)} pins are {pins_sha(LOCK_BYTES)}; re-record the"
        " counts with `tts_corpus.py count CORPUS --model-config MODEL/config.json --write"
        " CORPUS` in the environment built from that lock (docs/validation/tts-text-corpus.md)",
    )
    # The digest of the whole lock file is not the digest of its g2p pins.
    refused(good, lambda c: c["tokenizer"].update(g2p_pins_sha256=sha(LOCK_BYTES)), "counted under")
    for field in ("kokoro", "misaki", "spacy", "en_core_web_sm"):
        refused(good, lambda c, f=field: c["tokenizer"].update({f: "0.0.1"}), f"tokenizer {field}")
        # Another locked package's version is still not this package's pin.
        other = "2.4.0" if field != "kokoro" else "3.8.16"
        assert f"=={other} " in LOCK_BYTES.decode() and good["tokenizer"][field] != other
        refused(
            good, lambda c, f=field, v=other: c["tokenizer"].update({f: v}), f"tokenizer {field}"
        )
    refused(good, source("cmu-arctic", license="CC-BY-4.0"), "license is CMU-ARCTIC")
    refused(
        good,
        source("cmu-arctic", attribution="CMU ARCTIC databases."),
        "the attribution does not name tools/validation/corpus/cmu-arctic-COPYING.txt",
    )
    refused(
        good,
        lambda c: c["sources"][1].pop("text_form"),
        "source libritts-r: text_form is original",
    )
    refused(good, source("authored", url="http://example.invalid/o"), "url and digest")
    refused(good, source("libritts-r", sha256=None), "url and digest")
    refused(
        good, source("authored", url="http://example.invalid/o", sha256="3" * 64), "url and digest"
    )
    refused(good, source("cmu-arctic", url=None, sha256=None), "url and digest")
    refused(good, lambda c: c["sources"].append(copy.deepcopy(c["sources"][0])), "appears twice")
    refused(good, lambda c: c["sources"].pop(0), "source 'cmu-arctic' is not listed")
    refused(good, edit("s-0", source_id="arctic_a0001"), "is not a authored identifier")
    refused(good, edit("s-110", source_id=None), "is not a cmu-arctic identifier")
    refused(good, edit("s-110", source_id="arctic_c0001"), "is not a cmu-arctic identifier")
    refused(good, edit("s-110", source_id="arctic_a0110x"), "is not a cmu-arctic identifier")
    refused(good, edit("s-220", source_id="1_2_3_220_5"), "is not a libritts-r identifier")
    refused(good, edit("s-220", source_id="1_2_3"), "is not a libritts-r identifier")
    refused(good, edit("s-111", source_id="arctic_a0110"), "is used twice")
    refused(
        good, lambda c: c["sentences"].append(copy.deepcopy(c["sentences"][40])), "appears twice"
    )
    refused(good, retext("s-40", "Synthetic x sentence. "), "stripped NFC")
    refused(good, retext("s-40", "Cafe\u0301 au lait."), "stripped NFC")
    refused(good, retext("s-40", "A\u200bB."), "control or format")
    refused(good, retext("s-40", "I can\u02bct go."), "s-40: text holds U+02BC")
    assert (
        tts.check_corpus(
            json.loads(json.dumps(good).replace("Do not a it.", "I can\\u2019t a it."))
        )
        == 330
    )
    refused(good, retext("s-40", "Line\none."), "control or format")
    refused(good, retext("s-40", "\u00e9" * 2049), "over 4096 UTF-8 bytes")
    refused(good, retext("s-40", "\u00e9" * 2048 + "a"), "over 4096 UTF-8 bytes")
    accepted(good, retext("s-40", "\u00e9" * 2048))
    assert (
        tts.check_corpus(json.loads(json.dumps(good).replace("Synthetic an ", "Synthetic \u00e9 ")))
        == 330
    )
    refused(
        good, retext("s-40", f"SYNTHETIC   {word(39).upper()} sentence."), "the same text as s-39"
    )
    refused(good, edit("s-40", phoneme_tokens=201), "outside every stratum")
    refused(good, edit("s-40", phoneme_tokens=30), "30 tokens is stratum t21to100")
    refused(good, edit("s-40", critical=["negation"]), "critical classes are []")
    refused(good, edit("s-0", critical=[]), "critical classes are ['negation']")
    refused(
        good, edit("s-12", critical=["negation", "numeral"]), "critical classes are ['numeral']"
    )
    refused(
        good,
        edit("s-40", text="Do not go at 9.", critical=["numeral", "negation"]),
        "critical classes are ['negation', 'numeral']",
    )
    refused(
        good, edit("s-0", critical=["negation", "negation"]), "critical classes are ['negation']"
    )
    refused(good, drop({f"s-{i}" for i in range(99, 110)}), "stratum t1to20 holds 99")
    refused(good, drop({f"s-{i}" for i in range(209, 220)}), "stratum t21to100 holds 99")
    refused(good, drop({f"s-{i}" for i in range(319, 330)}), "stratum t101to200 holds 99")
    for edge, names in (
        (20, {"s-19", "s-39", "s-59", "s-79", "s-99"}),
        (21, {"s-110", "s-190"}),
        (100, {"s-189"}),
        (101, {"s-220", "s-320"}),
        (200, {"s-319"}),
    ):
        holders = [s["id"] for s in good["sentences"] if s["phoneme_tokens"] == edge]
        assert set(holders) == names, (edge, holders)

        def move(c: dict[str, Any], holders: list[str] = holders, edge: int = edge) -> None:
            for name in holders:
                tokens = (
                    edge - 1 if edge in (21, 101, 200) else edge + 1 if edge == 100 else edge - 1
                )
                recount(name, tokens)(c)

        refused(good, move, f"exactly {edge} phoneme tokens")

    def thin_long(c: dict[str, Any]) -> None:
        for s in c["sentences"]:
            if 181 <= s["phoneme_tokens"] < 200:
                recount(s["id"], 150)(c)
            elif s["phoneme_tokens"] == 200 and s["id"] != "s-319":
                recount(s["id"], 150)(c)

    long_or_boundary = sum(
        1
        for s in good["sentences"]
        if s["phoneme_tokens"] >= 181 or s["phoneme_tokens"] in (20, 21, 100, 101, 200)
    )
    assert long_or_boundary > 20
    refused(good, thin_long, "long or boundary sentences, under 20")
    almost = copy.deepcopy(good)
    thin_long(almost)
    for name in ("s-300", "s-301", "s-302", "s-303", "s-304", "s-305", "s-306", "s-307", "s-308"):
        recount(name, 181)(almost)
    assert (
        sum(
            1
            for s in almost["sentences"]
            if s["phoneme_tokens"] >= 181 or s["phoneme_tokens"] in (20, 21, 100, 101, 200)
        )
        == 20
    )
    assert tts.check_corpus(almost) == 330
    refused(almost, recount("s-308", 180), "19 long or boundary")

    def keep_critical(kind: str, keep: int) -> Callable[[dict[str, Any]], None]:
        def mutate(c: dict[str, Any]) -> None:
            holders = [s["id"] for s in c["sentences"] if kind in s["critical"]]
            for name in holders[keep:]:
                retext(name, f"Plain {word(int(name[2:]))} words.")(c)

        return mutate

    for kind in ("negation", "numeral"):
        refused(good, keep_critical(kind, 9), f"9 {kind} sentences, under 10")
        assert tts.check_corpus(copy.deepcopy(good) | {}) == 330

    refused(good, lambda c: c["listening"].append(c["listening"][0]), "names a sentence twice")
    refused(good, lambda c: c["listening"].append("absent"), "unlisted sentences ['absent']")
    refused(
        good,
        lambda c: c.update(
            listening=c["listening"][:9] + c["listening"][10:19] + c["listening"][20:29]
        ),
        "holds 27, under 30",
    )
    refused(good, lambda c: c["listening"].__setitem__(1, "s-150"), "not balanced across strata")

    def twelve_nine_nine(c: dict[str, Any]) -> None:
        c["listening"] = [n for n in c["listening"] if n not in ("s-147", "s-257")]
        c["listening"] += ["s-38", "s-39"]

    refused(
        good,
        twelve_nine_nine,
        "not balanced across strata: {'t1to20': 12, 't21to100': 9, 't101to200': 9}",
    )
    assert (
        tts.check_corpus(
            {**copy.deepcopy(good), "listening": good["listening"] + ["s-60", "s-170", "s-280"]}
        )
        == 330
    )

    def no_listening_negation(c: dict[str, Any]) -> None:
        c["listening"] = [
            n if n not in ("s-0", "s-110") else ("s-40" if n == "s-0" else "s-150")
            for n in c["listening"]
        ]

    refused(good, no_listening_negation, "listening set holds 1 negation")
    refused(good, lambda c: c["listening"].__setitem__(0, "s-40"), "listening set holds 2 negation")

    def no_listening_numeral(c: dict[str, Any]) -> None:
        c["listening"] = [
            n if n not in ("s-12", "s-122") else ("s-41" if n == "s-12" else "s-151")
            for n in c["listening"]
        ]

    refused(good, no_listening_numeral, "listening set holds 1 numeral")

    def segments(index: int, value: list[str]) -> Callable[[dict[str, Any]], None]:
        return lambda c: c["queued_sequences"][index].update(segments=value)

    refused(good, segments(1, ["s-221", "s-1", "s-2", "s-3"]), "two or three segments")
    refused(good, segments(1, ["s-221", "absent"]), "unlisted sentences ['absent']")
    refused(good, segments(1, ["s-150", "s-1"]), "not ['t21to100', 't1to20']")
    refused(good, segments(1, ["s-221", "s-150"]), "not ['t101to200', 't21to100']")
    refused(good, segments(1, ["s-221", "s-1", "s-150"]), "not ['t101to200', 't1to20', 't21to100']")
    refused(good, segments(1, ["s-220", "s-0"]), "the same segments as an earlier sequence")
    refused(
        good,
        lambda c: c["queued_sequences"][1].update(id="q-0"),
        "queued sequence id 'q-0' appears twice",
    )
    refused(good, lambda c: c["queued_sequences"].pop(), "99 queued sequences, under 100")
    refused(
        good,
        lambda c: [
            q["segments"].append("s-90") for q in c["queued_sequences"] if len(q["segments"]) == 2
        ],
        "both a single follower and two followers",
    )
    refused(
        good,
        lambda c: [q["segments"].pop() for q in c["queued_sequences"] if len(q["segments"]) == 3],
        "both a single follower and two followers",
    )


def check_source_texts(
    good: dict[str, Any], scratch: Path
) -> tuple[dict[str, Any], dict[str, Path]]:
    arctic = scratch / "cmuarctic.data"
    lines = [f'( arctic_a{i:04d} "{sentence(good, f"s-{i}")["text"]}" )' for i in range(110, 220)]
    arctic.write_bytes(("\n".join(lines) + "\n").encode())
    archive = scratch / "test_clean.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        for directory in ("LibriTTS_R", "LibriTTS_R/test-clean/1", "LibriTTS_R/test-clean/1/2"):
            info = tarfile.TarInfo(directory)
            info.type = tarfile.DIRTYPE
            tar.addfile(info)
        for i in range(220, 330):
            for form, text in (
                ("original", sentence(good, f"s-{i}")["text"]),
                ("normalized", "other"),
            ):
                data = f"{text}\n".encode()
                info = tarfile.TarInfo(f"LibriTTS_R/test-clean/1/2/1_2_3_{i}.{form}.txt")
                info.size = len(data)
                tar.addfile(info, io.BytesIO(data))
    pinned = copy.deepcopy(good)
    source("cmu-arctic", sha256=sha(arctic.read_bytes()))(pinned)
    source("libritts-r", sha256=sha(archive.read_bytes()))(pinned)
    files = {"cmu-arctic": arctic, "libritts-r": archive}
    assert tts.check_corpus(pinned, source_files=files) == 330
    refused(good, lambda c: None, "the corpus pins " + "1" * 64, source_files=files)
    for given, missing in (("cmu-arctic", "libritts-r"), ("libritts-r", "cmu-arctic")):
        refused(
            pinned,
            lambda c: None,
            f"not given: ['{missing}']",
            source_files={given: files[given]},
        )
    refused(
        pinned,
        retext("s-150", "Changed text."),
        "s-150: text differs from cmu-arctic arctic_a0150",
        source_files=files,
    )
    refused(
        pinned,
        retext("s-250", "Changed text."),
        "s-250: text differs from libritts-r 1_2_3_250",
        source_files=files,
    )
    for name in ("authored", "harvard"):
        refused(
            pinned,
            lambda c: None,
            f"{name}: the corpus lists no fetched source",
            source_files={name: arctic},
        )
    malformed = scratch / "malformed.data"
    malformed.write_bytes(arctic.read_bytes() + b"(arctic_a9999 unquoted)\n")
    refused(
        pinned,
        source("cmu-arctic", sha256=sha(malformed.read_bytes())),
        "an unreadable prompt line",
        source_files={**files, "cmu-arctic": malformed},
    )
    trailing = scratch / "trailing.data"
    trailing.write_bytes(arctic.read_bytes() + b'( arctic_a9998 "x" ) junk\n')
    refused(
        pinned,
        source("cmu-arctic", sha256=sha(trailing.read_bytes())),
        "an unreadable prompt line",
        source_files={**files, "cmu-arctic": trailing},
    )
    sparse = scratch / "sparse.data"
    sparse.write_bytes(("\n".join(lines[:40] + lines[41:]) + "\n").encode())
    refused(
        pinned,
        source("cmu-arctic", sha256=sha(sparse.read_bytes())),
        "s-150: text differs from cmu-arctic arctic_a0150",
        source_files={**files, "cmu-arctic": sparse},
    )
    return pinned, files


def check_schemas(good: dict[str, Any]) -> None:
    schema = tts.load_schema()
    jsonschema.Draft7Validator.check_schema(schema)
    jsonschema.validate(good, schema)
    for mutate, where in (
        (lambda c: c.update(extra=1), "root"),
        (lambda c: c["tokenizer"].update(rule="other"), "rule"),
        (lambda c: c["sources"][0].update(id="harvard"), "source id"),
        (edit("s-0", phoneme_tokens=0), "zero tokens"),
        (edit("s-0", stratum="t0"), "stratum"),
        (edit("s-0", critical=["other"]), "critical"),
        (edit("s-0", id="S 0"), "sentence id"),
        (source("libritts-r", text_form="normalized"), "text_form"),
        (lambda c: c["queued_sequences"][0].update(segments=["s-220"]), "one segment"),
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
    for mutate, where in (
        (edit("s-0", phoneme_tokens=201), "201 tokens"),
        (
            lambda c: c["queued_sequences"][0].update(segments=["s-220", "s-0", "s-1", "s-2"]),
            "four segments",
        ),
    ):
        broken = copy.deepcopy(good)
        mutate(broken)
        try:
            jsonschema.validate(broken, schema)
        except jsonschema.ValidationError:
            continue
        raise AssertionError(f"jsonschema accepts {where}")
    jsonschema.Draft7Validator.check_schema(tts.load_schema(tts.RECORD_SCHEMA_PATH))


class FakeTensor:
    def __init__(self, values: list[float], dtype: object, ndim: int = 1) -> None:
        self.values, self.dtype, self.ndim, self.shape = values, dtype, ndim, (len(values),)

    def detach(self) -> FakeTensor:
        return self

    def cpu(self) -> FakeTensor:
        return self

    def tolist(self) -> list[float]:
        return list(self.values)


NO_ATTRIBUTE = object()


class Fakes:
    """kokoro and torch as modules, holding every call they receive."""

    def __init__(self, tokens: dict[str, int]) -> None:
        self.tokens = tokens
        self.calls: list[tuple[Any, ...]] = []
        # NO_ATTRIBUTE leaves the g2p without a `fallback` attribute at all.
        self.fallback: object | None = object()
        self.g2p_phonemes: Callable[[str], str] | None = None
        self.pipeline_phonemes: Callable[[str], str] | None = None
        self.chunks = 1
        self.dtype: object = None
        self.ndim = 1
        self.empty = False
        self.gain = 1.0
        self.max_threads: int | None = None
        self.model: object = None
        self.during: Callable[[], None] | None = None
        self.versions = {name: LOCK_VERSIONS[name] for name in G2P_PINS}
        fakes = self
        torch = types.ModuleType("torch")
        torch.float32 = object()  # type: ignore[attr-defined]
        torch.float64 = object()  # type: ignore[attr-defined]
        torch.__version__ = "0.0.0+fake"  # type: ignore[attr-defined]
        self.threads = 0
        torch.manual_seed = lambda seed: fakes.calls.append(("seed", seed))  # type: ignore[attr-defined]
        torch.set_num_threads = lambda n: setattr(fakes, "threads", n)  # type: ignore[attr-defined]

        def get_num_threads() -> int:
            if fakes.max_threads is None:
                return fakes.threads
            return min(fakes.threads, fakes.max_threads)

        torch.get_num_threads = get_num_threads  # type: ignore[attr-defined]
        self.torch = torch

        class G2P:
            def __init__(self) -> None:
                if fakes.fallback is not NO_ATTRIBUTE:
                    self.fallback = fakes.fallback

            def __call__(self, text: str) -> tuple[str, list[object]]:
                return (fakes.g2p_phonemes or (lambda t: fake_phonemes(fakes.tokens[t])))(text), []

        class Result:
            def __init__(self, text: str) -> None:
                self.phonemes = (
                    fakes.pipeline_phonemes or (lambda t: fake_phonemes(fakes.tokens[t]))
                )(text)
                count = fakes.tokens[text]
                self.audio = FakeTensor(
                    []
                    if fakes.empty
                    else [fakes.gain * (i % 7 - 3) / 4 for i in range(count * 10)],
                    fakes.dtype or torch.float32,
                    fakes.ndim,
                )  # type: ignore[attr-defined]

        class KPipeline:
            def __init__(self, lang_code: str, model: object, trf: bool) -> None:
                fakes.calls.append(("pipeline", lang_code, model, trf))
                self.g2p = G2P()

            def __call__(self, text: str, voice: str, speed: float) -> Any:
                fakes.calls.append(("synthesize", text, voice, speed))
                if fakes.during is not None:
                    fakes.during()
                return iter([Result(text) for _ in range(fakes.chunks)])

        class KModel:
            def __init__(self, config: dict[str, Any], model: str) -> None:
                fakes.calls.append(("model", config, model))
                fakes.model = self
                self.steps: list[str] = []

            def float(self) -> KModel:
                fakes.calls.append(("float",))
                return self

            def to(self, device: str) -> KModel:
                fakes.calls.append(("to", device))
                return self

            def eval(self) -> KModel:
                fakes.calls.append(("eval",))
                return self

        kokoro = types.ModuleType("kokoro")
        kokoro.KPipeline = KPipeline  # type: ignore[attr-defined]
        kokoro.KModel = KModel  # type: ignore[attr-defined]
        self.kokoro = kokoro

    def __enter__(self) -> Fakes:
        self.saved = {name: sys.modules.get(name) for name in ("torch", "kokoro")}
        sys.modules["torch"], sys.modules["kokoro"] = self.torch, self.kokoro
        self.version = importlib.metadata.version
        importlib.metadata.version = lambda name: self.versions[name]  # type: ignore[assignment]
        return self

    def __exit__(self, *exc: object) -> None:
        importlib.metadata.version = self.version  # type: ignore[assignment]
        for name, module in self.saved.items():
            if module is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = module


def fake_repo(
    scratch: Path, model: Path, voices: tuple[str, ...] = ("af_heart",), kokoro: bool = True
) -> Path:
    repo = scratch / "repo"
    (repo / "packaging/speech-runtime/lock").mkdir(parents=True)
    (repo / "packaging/provisioning").mkdir(parents=True)
    shutil.copy(ROOT / tts.LOCK, repo / tts.LOCK)
    (repo / tts.ARCTIC_NOTICE).parent.mkdir(parents=True)
    shutil.copy(ROOT / tts.ARCTIC_NOTICE, repo / tts.ARCTIC_NOTICE)
    # A bundle file outside model/ whose name a model file shares must not pin it.
    files = [{"path": "config.json", "sha256": "0" * 64}] + [
        {"path": f"model/{p}", "sha256": sha((model / p).read_bytes())}
        for p in ("config.json", "kokoro-v1_0.pth", *(f"voices/{v}.pt" for v in voices))
    ]
    bundles = [{"name": "other", "files": []}]
    if kokoro:
        bundles.append({"name": tts.KOKORO_BUNDLE, "files": files})
    manifest = {"bundles": bundles}
    (repo / tts.PROVISIONING).write_text(json.dumps(manifest))
    return repo


def expect_error(call: Callable[[], Any], expect: str) -> None:
    try:
        call()
    except tts.CorpusError as exc:
        assert expect in str(exc), f"expected {expect!r}, got {exc}"
    else:
        raise AssertionError(f"no refusal {expect!r}")


def check_count(good: dict[str, Any], scratch: Path) -> None:
    model = scratch / "model"
    (model / "voices").mkdir(parents=True)
    (model / "config.json").write_text(json.dumps({"vocab": FAKE_VOCAB}))
    (model / "kokoro-v1_0.pth").write_bytes(b"weights")
    (model / "voices/af_heart.pt").write_bytes(b"voice")
    repo = fake_repo(scratch, model)
    assert len(fake_phonemes(20)) == 20 and {" ", ",", "."} <= set(fake_phonemes(20))
    tokens = {s["text"]: s["phoneme_tokens"] for s in good["sentences"]}

    def count(corpus: dict[str, Any], **kwargs: Any) -> list[str]:
        return tts.count_corpus(
            corpus,
            model / "config.json",
            repo_root=repo,
            espeak_version=lambda: "9.9-test",
            **kwargs,
        )

    with Fakes(tokens) as fakes:
        assert count(copy.deepcopy(good)) == []
        assert fakes.calls == [("pipeline", "en-US", False, False)]
        shifted = copy.deepcopy(good)
        shifted["language"] = "en-GB"
        count(shifted)
        assert fakes.calls[-1] == ("pipeline", "en-GB", False, False)
        wrong = copy.deepcopy(good)
        sentence(wrong, "s-5")["phoneme_tokens"] = 7
        expect_error(lambda: count(wrong), "s-5: 6 tokens, recorded 7")
        written = scratch / "written.json"
        stale = copy.deepcopy(good)
        stale["tokenizer"].update(g2p_pins_sha256="0" * 64, misaki="0.0.1", espeak_ng="old")
        recount("s-6", 150)(stale)
        sentence(stale, "s-6")["critical"] = ["numeral"]
        assert count(stale, write=written) == ["s-6: 7 tokens, recorded 150"]
        rewritten = json.loads(written.read_bytes())
        assert rewritten["tokenizer"] == good["tokenizer"] | {"espeak_ng": "9.9-test"}
        assert sentence(rewritten, "s-6") == sentence(good, "s-6")
        assert tts.check_corpus(rewritten, repo_root=repo) == 330
        for i in range(11):
            tokens[sentence(good, f"s-{i}")["text"]] = 30
        unwritten = scratch / "unwritten.json"
        expect_error(lambda: count(copy.deepcopy(good), write=unwritten), "stratum t1to20 holds 99")
        assert not unwritten.exists()
        for i in range(11):
            tokens[sentence(good, f"s-{i}")["text"]] = sentence(good, f"s-{i}")["phoneme_tokens"]
        fakes.versions["misaki"] = "0.9.3"
        expect_error(
            lambda: count(copy.deepcopy(good)), "installed misaki 0.9.3 is not the lock's 0.9.4"
        )
        fakes.versions["misaki"] = "0.9.4"
        for name in G2P_PINS:
            fakes.versions[name] = "0.0.1"
            expect_error(lambda: count(copy.deepcopy(good)), f"installed {name} 0.0.1")
            fakes.versions[name] = LOCK_VERSIONS[name]
        fakes.fallback = None
        expect_error(lambda: count(copy.deepcopy(good)), "espeak fallback is unavailable")
        fakes.fallback = NO_ATTRIBUTE
        assert count(copy.deepcopy(good)) == []
        fakes.fallback = object()
        tokens[sentence(good, "s-7")["text"]] = 0
        expect_error(lambda: count(copy.deepcopy(good)), "has no phonemes")
        tokens[sentence(good, "s-7")["text"]] = 8
        fakes.g2p_phonemes = lambda text: " " * tokens[text]
        expect_error(lambda: count(copy.deepcopy(good)), "has no phonemes")
        fakes.g2p_phonemes = None
        (model / "config.json").write_text(json.dumps({"vocab": {"x": 1}}))
        expect_error(
            lambda: count(copy.deepcopy(good)),
            "config.json is " + sha((model / "config.json").read_bytes()),
        )
        repo2 = fake_repo(scratch / "two", model)
        expect_error(
            lambda: tts.count_corpus(copy.deepcopy(good), model / "config.json", repo_root=repo2),
            "the model vocabulary lacks ['h']",
        )
        bare = fake_repo(scratch / "bare", model)
        (bare / tts.ARCTIC_NOTICE).unlink()
        refused(
            good,
            lambda c: None,
            "license notice tools/validation/corpus/cmu-arctic-COPYING.txt is missing",
            repo_root=bare,
        )
        repo3 = fake_repo(scratch / "three", model, kokoro=False)
        expect_error(
            lambda: tts.count_corpus(copy.deepcopy(good), model / "config.json", repo_root=repo3),
            "pins no tts-kokoro-candidate bundle",
        )
    check_lock_scope(good, scratch / "scope", model)


def check_lock_scope(good: dict[str, Any], scratch: Path, model: Path) -> None:
    """Only the g2p pins' lines of the lock are the corpus's; any of them changing refuses it."""
    repo = fake_repo(scratch, model)
    lock = repo / tts.LOCK
    lines = lock_lines(LOCK_BYTES)
    for name, line in lines.items():
        if name in G2P_PINS:
            continue
        relock(lock, name, rehash(line))
    relock(lock, "kokoro", lines["kokoro"].replace(" --hash", "   --hash"))
    assert lock.read_bytes() != LOCK_BYTES
    assert tts.check_corpus(copy.deepcopy(good), repo_root=repo) == 330
    for name in G2P_PINS:
        relock(lock, name, rehash(lines[name]))
        refused(good, lambda c: None, "counted under g2p pins", repo_root=repo)
        relock(lock, name, lines[name])
        assert tts.check_corpus(copy.deepcopy(good), repo_root=repo) == 330
    relock(lock, "regex", None)
    refused(good, lambda c: None, f"{tts.LOCK} pins no regex", repo_root=repo)
    relock(lock, "kokoro", None)
    refused(good, lambda c: None, f"{tts.LOCK} pins no kokoro, regex", repo_root=repo)


def check_reference(good: dict[str, Any], scratch: Path) -> None:
    model = scratch / "ref-model"
    (model / "voices").mkdir(parents=True)
    (model / "config.json").write_text(json.dumps({"vocab": {"h": 1}}))
    (model / "kokoro-v1_0.pth").write_bytes(b"weights")
    (model / "voices/af_heart.pt").write_bytes(b"voice")
    (model / "voices/af_bella.pt").write_bytes(b"bella voice")
    repo = fake_repo(scratch / "ref", model, voices=("af_heart", "af_bella"))
    # A g2p pin unlike the repository's, so the record's digest is shown to be the given root's.
    relock(repo / tts.LOCK, "regex", rehash(lock_lines(LOCK_BYTES)["regex"]))
    small = copy.deepcopy(good)
    small["sentences"] = small["sentences"][:3]
    tokens = {s["text"]: s["phoneme_tokens"] for s in good["sentences"]}

    def record(out: Path, corpus: dict[str, Any] = small) -> dict[str, Any]:
        return tts.record_reference(
            corpus,
            "c" * 64,
            model,
            out,
            threads=3,
            repo_root=repo,
            espeak_version=lambda: "9.9-ref",
        )

    def phonemes_sha(count: int) -> str:
        return sha(fake_phonemes(count).encode())

    with Fakes(tokens) as fakes:
        result = record(scratch / "out")
        assert fakes.threads == 3 and result["engine"]["threads"] == 3
        assert fakes.calls[:4] == [
            ("model", {"vocab": {"h": 1}}, str(model / "kokoro-v1_0.pth")),
            ("float",),
            ("to", "cpu"),
            ("eval",),
        ]
        assert fakes.calls[4] == ("pipeline", "en-US", fakes.model, False), fakes.calls[4]
        assert result["language"] == "en-US"
        assert result["engine"] == {
            "kokoro": "0.9.4",
            "misaki": "0.9.4",
            "spacy": "3.8.16",
            "en_core_web_sm": "3.8.0",
            "g2p_pins_sha256": pins_sha((repo / tts.LOCK).read_bytes()),
            "espeak_ng": "9.9-ref",
            "torch": "0.0.0+fake",
            "device": "cpu",
            "dtype": "float32",
            "threads": 3,
        }, result["engine"]
        assert result["model"] == {
            "config_sha256": sha((model / "config.json").read_bytes()),
            "weights_sha256": sha(b"weights"),
            "voice": "af_heart",
            "voice_sha256": sha(b"voice"),
        }
        expected: list[tuple[Any, ...]] = []
        for s in small["sentences"]:
            expected += [
                ("seed", tts.reference_seed("synthetic-0", s["id"])),
                ("synthesize", s["text"], str(model / "voices/af_heart.pt"), 1),
            ]
        assert fakes.calls[5:] == expected, fakes.calls[5:]
        jsonschema.validate(result, tts.load_schema(tts.RECORD_SCHEMA_PATH))
        assert json.loads((scratch / "out" / tts.RECORD_NAME).read_bytes()) == result
        assert result["corpus"] == {
            "name": "synthetic",
            "version": "synthetic-0",
            "sha256": "c" * 64,
        }
        assert result["model"]["weights_sha256"] == sha(b"weights")
        for s, clip in zip(small["sentences"], result["clips"], strict=True):
            data = (scratch / "out" / f"{s['id']}.wav").read_bytes()
            assert clip["wav_sha256"] == sha(data) and clip["runner_phonemes"] is True
            with wave.open(io.BytesIO(data)) as reader:
                assert (reader.getframerate(), reader.getnchannels(), reader.getsampwidth()) == (
                    24000,
                    1,
                    2,
                )
                pcm = reader.readframes(reader.getnframes())
            assert clip["pcm_sha256"] == sha(pcm) and clip["samples"] == s["phoneme_tokens"] * 10
            assert clip["text_sha256"] == sha(s["text"].encode())
            assert clip["phonemes_sha256"] == phonemes_sha(s["phoneme_tokens"])
            assert clip["seed"] == tts.reference_seed("synthetic-0", s["id"])
            assert clip["clipped_samples"] == 0
        expect_error(lambda: record(scratch / "out"), "already exists")
        expect_error(lambda: record(repo / "inside"), "outside the repository")
        expect_error(lambda: record(repo), "outside the repository")
        assert not (repo / "inside").exists()
        fakes.pipeline_phonemes = lambda text: fake_phonemes(tokens[text] + 1)
        for s, clip in zip(small["sentences"], record(scratch / "out2")["clips"], strict=True):
            assert not clip["runner_phonemes"]
            assert clip["phonemes_sha256"] == phonemes_sha(s["phoneme_tokens"] + 1)
        fakes.pipeline_phonemes = None
        fakes.gain = 2.0
        for s, clip in zip(small["sentences"], record(scratch / "loud")["clips"], strict=True):
            loud = sum(1 for i in range(s["phoneme_tokens"] * 10) if abs(i % 7 - 3) == 3)
            assert loud > 0 and clip["clipped_samples"] == loud, (loud, clip)
        fakes.gain = 1.0
        fakes.max_threads = 2
        assert record(scratch / "capped")["engine"]["threads"] == 2
        fakes.max_threads = None
        british = copy.deepcopy(small)
        british["language"] = "en-GB"
        assert record(scratch / "british", british)["language"] == "en-GB"
        assert [c for c in fakes.calls if c[0] == "pipeline"][-1][1] == "en-GB"
        bella = copy.deepcopy(small)
        bella["voice"] = "af_bella"
        voiced = record(scratch / "bella", bella)
        assert voiced["model"]["voice"] == "af_bella"
        assert voiced["model"]["voice_sha256"] == sha(b"bella voice")
        assert fakes.calls[-1][2] == str(model / "voices/af_bella.pt")
        fakes.fallback = NO_ATTRIBUTE
        assert len(record(scratch / "bare-g2p")["clips"]) == 3
        fakes.fallback = object()
        fakes.chunks = 2
        expect_error(lambda: record(scratch / "out3"), "returned 2 chunks, not one")
        fakes.chunks = 1
        fakes.dtype = fakes.torch.float64  # type: ignore[attr-defined]
        expect_error(lambda: record(scratch / "out4"), "no one-dimensional float32 audio")
        fakes.dtype = None
        fakes.ndim = 2
        expect_error(lambda: record(scratch / "out4a"), "no one-dimensional float32 audio")
        fakes.ndim = 1
        fakes.empty = True
        expect_error(lambda: record(scratch / "out4b"), "no one-dimensional float32 audio")
        fakes.empty = False
        miscounted = copy.deepcopy(small)
        sentence(miscounted, "s-1")["phoneme_tokens"] = 9
        failed = scratch / "failed" / "out5"
        before = len(fakes.calls)
        expect_error(lambda: record(failed, miscounted), "s-1: 2 tokens here, the corpus counted 9")
        # The first clip was made before the second sentence was refused.
        assert [c[1] for c in fakes.calls[before:] if c[0] == "synthesize"] == [
            sentence(small, "s-0")["text"]
        ]
        assert list(failed.parent.iterdir()) == [], list(failed.parent.iterdir())
        # While a run synthesizes, only its temporary directory is beside the output.
        seen: list[tuple[bool, list[str]]] = []
        fakes.during = lambda: seen.append(
            (failed.exists(), [p.name for p in failed.parent.iterdir()])
        )
        assert len(record(failed)["clips"]) == 3
        fakes.during = None
        assert len(seen) == 3 and not any(exists for exists, _ in seen), seen
        assert all(
            len(names) == 1 and names[0].startswith(".out5.") and names[0].endswith(".partial")
            for _, names in seen
        ), seen
        assert sorted(p.name for p in failed.parent.iterdir()) == ["out5"]
        assert sorted(p.name for p in failed.iterdir()) == sorted(
            [tts.RECORD_NAME] + [f"{s['id']}.wav" for s in small["sentences"]]
        )
        fakes.fallback = None
        expect_error(lambda: record(scratch / "out6"), "espeak fallback is unavailable")
        fakes.fallback = object()
        fakes.versions["kokoro"] = "0.9.5"
        expect_error(lambda: record(scratch / "out7"), "installed kokoro 0.9.5")
        fakes.versions["kokoro"] = "0.9.4"
        for name in G2P_PINS:
            fakes.versions[name] = "0.0.1"
            expect_error(lambda: record(scratch / "out7a"), f"installed {name} 0.0.1")
            fakes.versions[name] = LOCK_VERSIONS[name]
        fakes.max_threads = 0
        zero = scratch / "zero-threads"
        expect_error(lambda: record(zero), "the reference record does not match its schema")
        assert not zero.exists()
        assert not [p for p in scratch.iterdir() if p.name.startswith(".zero-threads")]
        fakes.max_threads = None
        unpinned = copy.deepcopy(small)
        unpinned["voice"] = "af_sky"
        (model / "voices/af_sky.pt").write_bytes(b"voice")
        expect_error(lambda: record(scratch / "out8", unpinned), "voices/af_sky.pt is")
        (model / "config.json").write_text(json.dumps({"vocab": {"h": 2}}))
        expect_error(lambda: record(scratch / "out8a"), "config.json is ")
        (model / "config.json").write_text(json.dumps({"vocab": {"h": 1}}))
        (model / "kokoro-v1_0.pth").write_bytes(b"other weights")
        expect_error(
            lambda: record(scratch / "out9"), "kokoro-v1_0.pth is " + sha(b"other weights")
        )


def check_cli(good: dict[str, Any], scratch: Path) -> None:
    path = scratch / "corpus.json"
    path.write_text(json.dumps(good))
    run = [sys.executable, str(TOOL)]
    completed = subprocess.run(
        [*run, "check", str(path)], capture_output=True, text=True, check=False
    )
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout == "tts-corpus: valid: 330 sentences, sources not read\n"
    broken = copy.deepcopy(good)
    broken["listening"].pop()
    path.with_name("broken.json").write_text(json.dumps(broken))
    completed = subprocess.run(
        [*run, "check", str(path.with_name("broken.json"))],
        capture_output=True,
        text=True,
        check=False,
    )
    assert (
        completed.returncode == 1 and "tts-corpus: refused: the listening set" in completed.stderr
    )
    path.with_name("unreadable.json").write_text("{")
    completed = subprocess.run(
        [*run, "check", str(path.with_name("unreadable.json"))],
        capture_output=True,
        text=True,
        check=False,
    )
    assert completed.returncode == 2 and "unreadable" in completed.stderr
    out = io.StringIO()
    with redirect_stdout(out):
        assert tts.main(["summary", str(path)]) == 0
    assert json.loads(out.getvalue()) == {
        "sentences": 330,
        "per_stratum": {"t101to200": 110, "t1to20": 110, "t21to100": 110},
        "per_source": {"authored": 110, "cmu-arctic": 110, "libritts-r": 110},
        "per_critical": {"negation": 36, "numeral": 36},
        "long_or_boundary": 30,
        "listening": 30,
        "queued_sequences": 100,
    }


def tool(*args: object) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        code = tts.main([str(a) for a in args])
    return code, out.getvalue(), err.getvalue()


def check_cli_commands(
    good: dict[str, Any], scratch: Path, pinned: dict[str, Any], files: dict[str, Path]
) -> None:
    model = scratch / "cli-model"
    (model / "voices").mkdir(parents=True)
    (model / "config.json").write_text(json.dumps({"vocab": FAKE_VOCAB}))
    (model / "kokoro-v1_0.pth").write_bytes(b"weights")
    (model / "voices/af_heart.pt").write_bytes(b"voice")
    repo = fake_repo(scratch / "cli", model)
    # A g2p pin unlike the repository's, so a command that checks against REPO_ROOT's is refused.
    relock(repo / tts.LOCK, "regex", rehash(lock_lines(LOCK_BYTES)["regex"]))
    pins_digest = pins_sha((repo / tts.LOCK).read_bytes())
    assert pins_digest != pins_sha(LOCK_BYTES)
    good, pinned = copy.deepcopy(good), copy.deepcopy(pinned)
    for relocked in (good, pinned):
        relocked["tokenizer"]["g2p_pins_sha256"] = pins_digest
    config = model / "config.json"
    corpus = scratch / "cli-corpus.json"
    corpus.write_text(json.dumps(good, indent=4))
    saved = tts.REPO_ROOT, tts._dpkg_espeak_version
    with Fakes({s["text"]: s["phoneme_tokens"] for s in good["sentences"]}):
        tts.REPO_ROOT, tts._dpkg_espeak_version = repo, lambda: "9.9-cli"
        try:
            schemaless = scratch / "cli-schemaless.json"
            schemaless.write_text(json.dumps({**good, "extra": 1}))
            code, _, err = tool("count", schemaless, "--model-config", config)
            assert code == 1 and "does not match its schema" in err, (code, err)
            written = scratch / "cli-counted.json"
            code, out, _ = tool("count", corpus, "--model-config", config, "--write", written)
            assert code == 0 and out == "tts-corpus: counted: 330 sentences\n", (code, out)
            counted = json.loads(written.read_bytes())
            assert counted["tokenizer"]["espeak_ng"] == "9.9-cli"
            assert counted["tokenizer"]["g2p_pins_sha256"] == pins_digest
            assert counted["sentences"] == good["sentences"]
            sources = [f"--source={name}={path}" for name, path in files.items()]
            texts = scratch / "cli-pinned.json"
            texts.write_text(json.dumps(pinned))
            code, out, _ = tool("check", texts, *sources)
            assert code == 0 and out.endswith("texts compared with their sources\n"), out
            changed = copy.deepcopy(pinned)
            retext("s-150", "Changed text.")(changed)
            texts.write_text(json.dumps(changed))
            code, _, err = tool("check", texts, *sources)
            assert code == 1 and "s-150: text differs" in err, (code, err)
            out_dir = scratch / "cli-reference"
            code, _, err = tool("reference", corpus, "--model-dir", model, "--out", out_dir)
            assert code == 0, err
            recorded = json.loads((out_dir / tts.RECORD_NAME).read_bytes())
            assert recorded["corpus"]["sha256"] == sha(corpus.read_bytes())
            assert recorded["engine"]["g2p_pins_sha256"] == pins_digest
            assert recorded["engine"]["espeak_ng"] == "9.9-cli"
            assert len(recorded["clips"]) == 330
        finally:
            tts.REPO_ROOT, tts._dpkg_espeak_version = saved


def main() -> int:
    check_derivations()
    good = synthetic()
    check_rules(good)
    check_schemas(good)
    with tempfile.TemporaryDirectory() as directory:
        scratch = Path(directory)
        pinned, files = check_source_texts(good, scratch)
        check_count(good, scratch)
        check_reference(good, scratch)
        check_cli(good, scratch)
        check_cli_commands(good, scratch, pinned, files)
    check_filed()
    print("tts corpus checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
