#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check the speech quality scorer: its normalizers, alignment, gates and refusals.

The normalizers and the semantic rules are checked on authored text in both
languages. A corpus manifest at the gates' minimum sizes is then built without
audio, so the corpus tool's own check accepts it, and each gate is moved across
its boundary by changing the hypotheses one error at a time.
"""

from __future__ import annotations

import copy
import hashlib
import json
import subprocess
import sys
import tempfile
import unicodedata
from collections.abc import Callable
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools/validation/speech_quality.py"
sys.path.insert(0, str(ROOT / "tools/validation"))

import jsonschema  # noqa: E402
import speech_corpus as corpus  # noqa: E402
import speech_quality as quality  # noqa: E402

VERSION = "synthetic-0"
MANIFEST_SHA = "0" * 64
DIGESTS = {"streaming": "1" * 64, "batch": "2" * 64, "references": None}
EN_LETTERS = "bcdfghjklmpqrsvwxz"
CEILING_PERCENT = {
    ("en", "clean16k"): 15,
    ("en", "noisy16k"): 25,
    ("en", "telephony8k"): 25,
    ("ar", "clean16k"): 25,
    ("ar", "noisy16k"): 35,
    ("ar", "telephony8k"): 35,
}
# No letter here starts or spells a word of the Arabic lexicons.
AR_LETTERS = "بثجحخدذرزسشصضطظعغقك"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def norm(text: str, language: str) -> list[str]:
    return quality.normalize(text, language)


def check_english_normalizer() -> None:
    assert norm("Café", "en") == norm("Cafe\u0301", "en") == ["café"]
    assert norm("STRASSE Straße", "en") == ["strasse", "strasse"]
    assert norm("İstanbul", "en") == [unicodedata.normalize("NFC", "İstanbul").casefold()]
    assert norm("Hello, world!", "en") == ["hello", "world"]
    assert norm("don't", "en") == norm("don’t", "en") == ["don", "t"]
    assert norm("twenty-one U.S.A. «quoted» — dash", "en") == [
        "twenty",
        "one",
        "u",
        "s",
        "a",
        "quoted",
        "dash",
    ]
    assert norm("3.5 100% (x)", "en") == ["3", "5", "100", "x"]
    # Symbols are not punctuation and stay in their word.
    assert norm("$5 a+b", "en") == ["$5", "a+b"]
    assert norm("\ta\u00a0b\u2009c\u3000d\n", "en") == ["a", "b", "c", "d"]
    assert norm(" ,.;: ", "en") == []
    assert norm("a\u200bb", "en") == ["a\u200bb"]
    # Full casefold decomposes j with caron; the last NFC pass composes it again.
    assert norm("\u01f0", "en") == norm("J\u030c", "en") == ["\u01f0"]
    try:
        norm("a", "fr")
    except quality.QualityError:
        pass
    else:
        raise AssertionError("a language with no normalizer was normalized")


def check_arabic_normalizer() -> None:
    # NFC composes a carrier and its hamza or madda, whatever the input order.
    assert norm("أحمد", "ar") == norm("ا\u0654حمد", "ar")
    assert norm("آ", "ar") == norm("ا\u0653", "ar") == ["آ"]
    assert norm("إ", "ar") == norm("ا\u0655", "ar") == ["إ"]
    assert norm("ا\u064e\u0654", "ar") == ["أ"]
    assert norm("ؤ", "ar") == norm("و\u0654", "ar") == ["ؤ"]
    assert norm("ئ", "ar") == norm("ي\u0654", "ar") == ["ئ"]
    assert norm("ك\u0640ت\u0640اب", "ar") == ["كتاب"]
    # A tatweel between a carrier and its hamza is removed, and the two compose.
    assert norm("ا\u0640\u0654", "ar") == ["أ"] and norm("و\u0640\u0654", "ar") == ["ؤ"]
    assert norm(
        "ك\u0650ت\u064eاب\u064c م\u064fد\u064e\u0651ة\u064c ق\u064fل\u0652 ك\u0650ت\u064eاب\u064bا ك\u0650ت\u064eاب\u064d",
        "ar",
    ) == ["كتاب", "مدة", "قل", "كتابا", "كتاب"]
    assert norm("ه\u0670ذا", "ar") == ["هذا"]
    assert set(quality.HARAKAT) == {chr(c) for c in range(0x064B, 0x0653)} | {"\u0670"}
    # Distinct letters stay distinct: alef forms, ta marbuta, alef maksura.
    for one, other in (("أحمد", "احمد"), ("إلى", "الى"), ("مدرسة", "مدرسه"), ("على", "علي")):
        assert norm(one, "ar") != norm(other, "ar"), (one, other)
    assert norm("قال، ثم؛ لماذا؟ انتهى۔ ٥٠٪ «نعم»", "ar") == [
        "قال",
        "ثم",
        "لماذا",
        "انتهى",
        "٥٠",
        "نعم",
    ]
    assert norm("٣٫٥ ١٬٠٠٠", "ar") == ["٣", "٥", "١", "٠٠٠"]
    assert norm("عام ٢٠٢٤ و2024 ۲۰۲۴", "ar") == ["عام", "٢٠٢٤", "و2024", "۲۰۲۴"]
    assert norm("3", "ar") != norm("٣", "ar") != norm("۳", "ar")
    assert norm("GPS", "ar") == ["GPS"]
    # NFC leaves presentation forms and format characters; the scorer counts them.
    assert norm("\ufefb", "ar") == ["\ufefb"] != norm("لا", "ar")
    assert quality.text_findings("\ufefb") == {"presentation_form"}
    assert quality.text_findings("\ufdfa") == {"presentation_form"}
    assert quality.text_findings("كلمة\u200f") == {"format_character"}
    assert quality.text_findings("\ufeffa") == {"presentation_form", "format_character"}
    assert quality.text_findings("a\tb\nc\u00a0d\u000be\u000cf\r") == set()
    assert quality.text_findings("a\u001fb") == {"format_character"}
    assert quality.text_findings("لا 3 ٣") == set()


def check_idempotence() -> None:
    """Every assigned code point, alone and between two letters, normalizes to NFC words
    that normalize to themselves. Surrogates are refused before any normalizer runs."""
    for code in range(0x110000):
        char = chr(code)
        if unicodedata.category(char) in ("Cn", "Cs"):
            continue
        for language, (left, right) in (("en", ("a", "b")), ("ar", ("\u0628", "\u062a"))):
            for text in (char, left + char + right):
                words = norm(text, language)
                assert all(unicodedata.is_normalized("NFC", word) for word in words), (
                    language,
                    hex(code),
                )
                assert norm(" ".join(words), language) == words, (language, hex(code))


def check_lexicons() -> None:
    for language, lexicons in (
        ("en", (quality.NEGATION["en"], quality.NUMBER_WORDS["en"])),
        ("ar", (quality.NEGATION["ar"], quality.NUMBER_WORDS["ar"])),
    ):
        for lexicon in lexicons:
            for word in lexicon:
                assert norm(word, language) == [word], (language, word)


def check_alignment() -> None:
    assert quality.align(["a", "b", "c"], ["a", "x", "c"]) == [
        ("match", 0, 0),
        ("sub", 1, 1),
        ("match", 2, 2),
    ]
    assert quality.align(["a", "b"], []) == [("del", 0, None), ("del", 1, None)]
    assert quality.align([], ["a"]) == [("ins", None, 0)]
    assert quality.align(["a", "b"], ["b", "a"]) == [("sub", 0, 0), ("sub", 1, 1)]
    # A tie keeps the later word matched and deletes the earlier one.
    assert quality.align(["a", "a"], ["a"]) == [("del", 0, None), ("match", 1, 0)]
    assert quality.align(["x"], ["a", "a"]) == [("ins", None, 0), ("sub", 0, 1)]
    ops = quality.align("the cat sat on the mat".split(), "the bat sat the mat too".split())
    assert quality.edit_counts(ops) == {"substitutions": 1, "deletions": 1, "insertions": 1}


def regress(
    ref: str, streaming: str, batch: str, semantic_class: str, language: str
) -> list[dict[str, Any]]:
    words = norm(ref, language)
    arms = {}
    for arm, text in (("streaming", streaming), ("batch", batch)):
        hyp = norm(text, language)
        arms[arm] = (hyp, quality.align(words, hyp))
    if semantic_class == "window_seam":
        return quality.seam_regressions(words, arms["streaming"], arms["batch"])
    return quality.lexical_regressions(
        words, arms["streaming"], arms["batch"], semantic_class, language
    )


def check_critical_words() -> None:
    def critical(text: str, semantic_class: str, language: str) -> list[str]:
        words = norm(text, language)
        if semantic_class == "negation":
            words = quality.negation_words(words, language)
        return [
            words[i] for i in sorted(quality.critical_positions(words, semantic_class, language))
        ]

    assert critical("buy a T-shirt, T", "negation", "en") == []
    assert critical("T-shirt then", "negation", "en") == []
    assert quality.negation_words(["in", "t"], "ar") == ["in", "t"]
    assert (
        regress("buy a t shirt now", "buy a shirt now", "buy a t shirt now", "negation", "en") == []
    )
    assert quality.negation_words(norm("I can't, won't; isn\u2019t", "en"), "en") == (
        "i can not won not isn not".split()
    )
    assert critical("I don't know, nobody said no; never", "negation", "en") == [
        "not",
        "nobody",
        "no",
        "never",
    ]
    # The joined conjunctions are Arabic's only.
    assert critical("\u0648not \u0641no", "negation", "en") == []
    assert critical("cannot nor neither nothing none nowhere not", "negation", "en") == (
        "cannot nor neither nothing none nowhere not".split()
    )
    # Only the listed conjunctions join: a beh in front is another word.
    assert critical("بما بلا", "negation", "ar") == []
    assert critical("ولا فلم لن ليست وما ماذا لاعب وليس", "negation", "ar") == [
        "ولا",
        "فلم",
        "لن",
        "ليست",
        "وما",
        "وليس",
    ]
    assert critical("seven 3rd ninety a hundred dollars", "numerals", "en") == [
        "seven",
        "3rd",
        "ninety",
        "hundred",
    ]
    assert critical("خمسة وعشرون ٣ ۳ 3 ألف آلاف مليون كتاب", "numerals", "ar") == [
        "خمسة",
        "وعشرون",
        "٣",
        "۳",
        "3",
        "ألف",
        "آلاف",
        "مليون",
    ]
    assert critical("no no", "repetition", "en") == ["no", "no"]
    assert critical("go to go", "repetition", "en") == ["go", "go"]
    assert critical("a b c a", "repetition", "en") == ["a", "a"]
    assert critical("a b c d a", "repetition", "en") == []
    try:
        quality.critical_positions(["a"], "window_seam", "en")
    except quality.QualityError:
        pass
    else:
        raise AssertionError("window_seam has no lexical critical words")


def check_lexical_regressions() -> None:
    ref = "i did not go"
    assert regress(ref, "i did go", ref, "negation", "en") == [
        {"class": "negation", "kind": "lost", "position": 2, "words": ["not"]}
    ]
    assert regress(ref, "i did go", "i did go", "negation", "en") == []
    assert regress(ref, "i did never go", ref, "negation", "en") == [
        {"class": "negation", "kind": "lost", "position": 2, "words": ["not"]},
        {"class": "negation", "kind": "added", "position": None, "words": ["never"]},
    ]
    assert regress("i did go", "i did not go", "i did go", "negation", "en") == [
        {"class": "negation", "kind": "added", "position": None, "words": ["not"]}
    ]
    assert regress("i did go", "i did not go", "i did not go", "negation", "en") == []
    # An unmatched word outside the class is no regression of it.
    assert regress("i did go", "i did run", "i did go", "negation", "en") == []
    assert regress("i did go", "i not go", "i did go", "negation", "en") == [
        {"class": "negation", "kind": "added", "position": None, "words": ["not"]}
    ]
    assert regress("i did go", "not i not did go", "i not did go", "negation", "en") == [
        {"class": "negation", "kind": "added", "position": None, "words": ["not"]}
    ]
    assert regress("i did go", "not i not did go", "i did go", "negation", "en") == [
        {"class": "negation", "kind": "added", "position": None, "words": ["not", "not"]}
    ]
    # In a negation fixture alone, the t of n't reads as not: didn't and did not are one negation.
    assert regress(ref, "i didn't go", ref, "negation", "en") == []
    assert regress("i didn't go", ref, "i didn't go", "negation", "en") == []
    assert regress("i didn't go", "i did go", "i didn't go", "negation", "en") == [
        {"class": "negation", "kind": "lost", "position": 2, "words": ["not"]}
    ]
    assert regress("i did go", "i didn't go", "i did go", "negation", "en") == [
        {"class": "negation", "kind": "added", "position": None, "words": ["not"]}
    ]
    # Numeral words and digits stay distinct.
    assert regress("room seven", "room 7", "room seven", "numerals", "en") == [
        {"class": "numerals", "kind": "lost", "position": 1, "words": ["seven"]},
        {"class": "numerals", "kind": "added", "position": None, "words": ["7"]},
    ]
    assert regress("لم يذهب", "يذهب", "لم يذهب", "negation", "ar") == [
        {"class": "negation", "kind": "lost", "position": 0, "words": ["لم"]}
    ]
    # Bare ma stays critical where it is a relative pronoun, not a negation.
    assert regress("أعرف ما قال", "أعرف قال", "أعرف ما قال", "negation", "ar") == [
        {"class": "negation", "kind": "lost", "position": 1, "words": ["ما"]}
    ]
    assert regress("دفع خمسة وعشرين", "دفع خمسة وثلاثين", "دفع خمسة وعشرين", "numerals", "ar") == [
        {"class": "numerals", "kind": "lost", "position": 2, "words": ["وعشرين"]},
        {"class": "numerals", "kind": "added", "position": None, "words": ["وثلاثين"]},
    ]
    assert regress("room 12", "room 13", "room 13", "numerals", "en") == []
    assert regress("no no stop", "no stop", "no no stop", "repetition", "en") == [
        {"class": "repetition", "kind": "lost", "position": 0, "words": ["no"]}
    ]
    assert regress("we go now", "we go go now", "we go now", "repetition", "en") == [
        {"class": "repetition", "kind": "added", "position": None, "words": ["go"]}
    ]


def check_seam_regressions() -> None:
    ref = "a b c d e f g h"
    assert regress(ref, "a b e f g h", ref, "window_seam", "en") == [
        {"class": "window_seam", "kind": "lost", "position": 2, "words": ["c", "d"]}
    ]
    assert regress(ref, "a b d e f g h", ref, "window_seam", "en") == []
    assert regress(ref, "a c e f g h", ref, "window_seam", "en") == []
    assert regress(ref, "a b e f g h", "a b d e f g h", "window_seam", "en") == []
    assert regress(ref, "a b x y e f g h", ref, "window_seam", "en") == []
    # A deleted word still counts toward the place of a run inserted after it.
    assert regress(ref, "a c d x y e f g h", "a b c d x y e f g h", "window_seam", "en") == []
    # On a tie the duplicate is the earlier copy, inserted after the two words before it.
    assert regress(ref, "a b c d c d e f g h", ref, "window_seam", "en") == [
        {"class": "window_seam", "kind": "added", "position": 2, "words": ["c", "d"]}
    ]
    assert regress(ref, "a b c d x e f g h", ref, "window_seam", "en") == []
    assert regress(ref, "a b c d c d e f g h", "a b z c d e f g h", "window_seam", "en") == []
    assert regress(ref, "a b c d c d e f g h", "a z b c d e f g h", "window_seam", "en") == [
        {"class": "window_seam", "kind": "added", "position": 2, "words": ["c", "d"]}
    ]


def en_word(clip: int, index: int) -> str:
    value, letters = clip * 32 + index, ""
    for _ in range(4):
        value, digit = divmod(value, len(EN_LETTERS))
        letters += EN_LETTERS[digit]
    return "w" + letters


def ar_word(clip: int, index: int) -> str:
    value, letters = clip * 32 + index, ""
    for _ in range(4):
        value, digit = divmod(value, len(AR_LETTERS))
        letters += AR_LETTERS[digit]
    return letters


FIXTURE_TEXT = {
    "en": {"negation": "not", "numerals": "seven", "repetition": "again again", "window_seam": ""},
    "ar": {"negation": "لا", "numerals": "سبعة", "repetition": "قد قد", "window_seam": ""},
}


class Built:
    """A manifest at the gates' minimum sizes, its references, and both arms' transcripts."""

    def __init__(
        self,
        recordings: int = 500,
        derived: int = 100,
        fixtures: int = 50,
        classes: Callable[[int], str] = lambda clip: quality.SEMANTIC_CLASSES[clip % 4],
    ) -> None:
        self.files: list[dict[str, Any]] = []
        self.references: dict[str, str] = {}
        self.texts: dict[str, tuple[str, str]] = {}
        for language in ("en", "ar"):
            word = en_word if language == "en" else ar_word
            for clip in range(recordings):
                words = [word(clip, index) for index in range(21)]
                semantic = []
                if clip < fixtures:
                    semantic_class = classes(clip)
                    semantic = [semantic_class]
                    extra = FIXTURE_TEXT[language][semantic_class].split()
                    words = words[: 21 - len(extra)] + extra if extra else words
                clean = self.clean(f"{language}-{clip:03d}", language, " ".join(words), semantic)
                if clip < derived:
                    self.noisy(clean, (5, 10, 15, 20)[clip % 4])
                    self.telephony(clean)
        for clip in range(100):
            self.telephony(self.clean(f"sil-{clip:03d}", None, None, []))

    def clean(
        self, clip_id: str, language: str | None, text: str | None, semantic: list[str]
    ) -> dict[str, Any]:
        speech = language is not None
        entry: dict[str, Any] = {
            "id": f"{clip_id}.clean16k",
            "path": f"held_out/{clip_id}.clean16k.wav",
            "sha256": sha(f"{clip_id} file".encode()),
            "pcm_sha256": sha(f"{clip_id} samples".encode()),
            "size": 96044,
            "samples": 48000,
            "rate": 16000,
            "encoding": "pcm_s16le_wav",
            "suite": "speech" if speech else "silence",
            "pool": "held_out",
            "codec": "clean16k",
            "language": language,
            "duration_bin": "1to5s" if speech else None,
            "source": {
                "source": "read" if language != "ar" else "crowd",
                "file_ids": [f"{clip_id}.src"],
                "speaker": None,
            },
            "derived_from": None,
            "transcript": None,
            "labels": None,
            "recipe": None,
            "semantic": semantic,
        }
        if language is not None and text is not None:
            public = language == "en"
            entry["transcript"] = {"sha256": sha(text.encode()), "text": text if public else None}
            if not public:
                self.references[sha(text.encode())] = text
            entry["labels"] = {
                "speech_onset_sample": 8000,
                "speech_end_sample": 40000,
                "method": "synthetic",
                "tool": "speech_quality_test.py",
                "human_verified": False,
            }
            self.texts[entry["id"]] = (language, text)
        self.files.append(entry)
        return entry

    def derived(self, clean: dict[str, Any], codec: str) -> dict[str, Any]:
        entry = copy.deepcopy(clean)
        name = clean["id"].replace(".clean16k", f".{codec}")
        entry.update(
            id=name,
            path=f"held_out/{name}",
            sha256=sha(f"{name} file".encode()),
            pcm_sha256=sha(f"{name} samples".encode()),
            codec=codec,
            source=None,
            derived_from=clean["id"],
        )
        if clean["labels"] is not None:
            entry["labels"] = dict(clean["labels"], method="inherited", tool=None)
        if clean["id"] in self.texts:
            self.texts[name] = self.texts[clean["id"]]
        self.files.append(entry)
        return entry

    def noisy(self, clean: dict[str, Any], snr: int) -> None:
        entry = self.derived(clean, "noisy16k")
        seed = corpus.noisy_seed(VERSION, clean["id"])
        entry["recipe"] = {
            "kind": "noisy16k",
            "noise": "noise-held",
            "noise_offset_sample": corpus.noise_offset(seed, 1_000_000, 48000),
            "snr_db": snr,
            "seed": seed,
            "mixture_peak": 1000,
        }

    def telephony(self, clean: dict[str, Any]) -> None:
        entry = self.derived(clean, "telephony8k")
        speech = clean["suite"] == "speech"
        entry.update(rate=8000, encoding="mulaw", samples=24000, size=24000)
        if speech:
            entry["labels"].update(speech_onset_sample=4000, speech_end_sample=20000)
        entry["recipe"] = {
            "kind": "telephony8k",
            "level_gain_q30": 1 << 30 if speech else None,
            "levelled_span_power": 1 if speech else None,
            "sv56_dbov": None,
        }

    def manifest(self) -> dict[str, Any]:
        return {
            "schema_version": "0.1",
            "corpus": {"name": "synthetic", "version": VERSION},
            "recipes": {
                "telephony8k": {
                    "recipe": corpus.audio.TELEPHONY_RECIPE_ID,
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
            "sources": [self.source("read", True), self.source("crowd", False)],
            "noise": [
                {
                    "id": "noise-held",
                    "source": "read",
                    "file_id": "noise.src",
                    "path": "noise/noise-held.wav",
                    "sha256": sha(b"noise"),
                    "size": 2_000_044,
                    "samples": 1_000_000,
                    "pool": "held_out",
                    "excluded": False,
                    "exclusion_reason": None,
                }
            ],
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
            "condition": "synthetic text",
            "transcripts_public": public,
        }

    def arm(self, arm: str, hypotheses: dict[str, Any] | None = None) -> dict[str, Any]:
        """Each file's reference as its final, unless `hypotheses` gives another clip entry."""
        clips = []
        for entry in self.files:
            clip = (hypotheses or {}).get(entry["id"])
            if clip is None:
                finals = [self.texts[entry["id"]][1]] if entry["id"] in self.texts else []
                clip = {"id": entry["id"], "outcome": "completed", "finals": finals}
            clips.append(clip)
        return {
            "schema_version": "0.1",
            "manifest_sha256": MANIFEST_SHA,
            "arm": arm,
            "pool": "held_out",
            "clips": clips,
        }


def substituted(text: str, count: int) -> str:
    words = text.split()
    return " ".join(["zzz"] * count + words[count:])


def with_errors(built: Built, ids: list[str], errors: int) -> dict[str, Any]:
    """Hypotheses carrying exactly `errors` substitutions over the files `ids`."""
    hypotheses = {}
    for file_id in ids:
        if errors == 0:
            break
        take = min(errors, 21)
        hypotheses[file_id] = {
            "id": file_id,
            "outcome": "completed",
            "finals": [substituted(built.texts[file_id][1], take)],
        }
        errors -= take
    assert errors == 0
    return hypotheses


def dashed(manifest: dict[str, Any]) -> dict[str, Any]:
    """The manifest with each English reference ending in a standalone dash, a raw word only."""
    manifest = copy.deepcopy(manifest)
    for entry in manifest["files"]:
        if entry["language"] == "en":
            text = entry["transcript"]["text"] + " \u2014"
            entry["transcript"] = {"sha256": sha(text.encode()), "text": text}
    return manifest


def stratum(record: dict[str, Any], language: str, codec: str) -> dict[str, Any]:
    return next(s for s in record["strata"] if (s["language"], s["codec"]) == (language, codec))


def by_language(items: list[dict[str, Any]], language: str) -> dict[str, Any]:
    return next(item for item in items if item["language"] == language)


class Scorer:
    def __init__(self, built: Built) -> None:
        self.built = built
        self.manifest = built.manifest()
        self.record_schema = json.loads(quality.RECORD_SCHEMA_PATH.read_text(encoding="utf-8"))
        self.transcripts_schema = json.loads(
            quality.TRANSCRIPTS_SCHEMA_PATH.read_text(encoding="utf-8")
        )

    def score(
        self,
        streaming: dict[str, Any] | None = None,
        batch: dict[str, Any] | None = None,
        manifest: dict[str, Any] | None = None,
        references: dict[str, str] | None = None,
        cross_check: bool = False,
    ) -> dict[str, Any]:
        streaming = streaming if streaming is not None else self.built.arm("streaming")
        batch = batch if batch is not None else self.built.arm("batch")
        if cross_check:
            for document in (streaming, batch):
                jsonschema.validate(document, self.transcripts_schema)
        record = quality.score(
            manifest if manifest is not None else self.manifest,
            MANIFEST_SHA,
            streaming,
            batch,
            references if references is not None else self.built.references,
            input_digests=DIGESTS | {"references": "3" * 64},
        )
        if cross_check:
            jsonschema.validate(record, self.record_schema)
        quality.validate_record(record, self.record_schema)
        return record

    def refused(self, fragment: str, **inputs: Any) -> None:
        try:
            quality.score(
                inputs.get("manifest", self.manifest),
                MANIFEST_SHA,
                inputs.get("streaming", self.built.arm("streaming")),
                inputs.get("batch", self.built.arm("batch")),
                inputs.get("references", self.built.references),
                input_digests=DIGESTS,
            )
        except (quality.QualityError, corpus.ManifestError) as exc:
            assert fragment in str(exc), f"refused for {exc!r}, not {fragment!r}"
        else:
            raise AssertionError(f"accepted; expected a refusal naming {fragment!r}")


def file_ids(built: Built, language: str, codec: str) -> list[str]:
    return [
        e["id"]
        for e in built.files
        if e["language"] == language and e["codec"] == codec and e["suite"] == "speech"
    ]


def check_passing_run(scorer: Scorer) -> None:
    record = scorer.score(cross_check=True)
    assert record["verdict"] == "pass", quality.unmet(record)
    assert quality.unmet(record) == []
    assert [(s["language"], s["codec"]) for s in record["strata"]] == list(quality.CEILING_PERCENT)
    en_clean = stratum(record, "en", "clean16k")
    assert en_clean["utterances"] == 500 and en_clean["reference_words"] == 500 * 21
    assert en_clean["streaming"]["wer_percent"] == 0.0 and en_clean["delta_pp"] == 0.0
    assert stratum(record, "ar", "telephony8k")["utterances"] == 100
    assert by_language(record["languages"], "ar") == {
        "language": "ar",
        "recordings": 500,
        "distinct_references": 500,
        "reference_words": 500 * 21,
        "minimum_recordings": 500,
        "minimum_words": 10_000,
        "verdict": "pass",
    }
    assert record["silence"]["recordings"] == 100 and record["silence"]["files"] == 200
    assert [c["codec"] for c in record["silence"]["by_codec"]] == ["clean16k", "telephony8k"]
    semantic = by_language(record["semantic"]["languages"], "en")
    assert semantic["fixtures"] == 50
    assert semantic["minimum_class_fixtures"] == 10
    assert semantic["by_class"] == {
        name: {"fixtures": count, "batch_failed_files": 0, "regressions": 0, "verdict": "pass"}
        for name, count in (
            ("negation", 13),
            ("numerals", 13),
            ("repetition", 12),
            ("window_seam", 12),
        )
    }
    assert record["inputs"] == {
        "streaming_sha256": "1" * 64,
        "batch_sha256": "2" * 64,
        "references_sha256": "3" * 64,
    }
    assert record["scorer"]["sha256"] == sha(TOOL.read_bytes())
    assert record["scorer"]["unicode_version"] == unicodedata.unidata_version
    snr = {
        b["value"]: b["utterances"]
        for b in record["breakdowns"]
        if (b["language"], b["codec"], b["by"]) == ("en", "noisy16k", "snr_db")
    }
    assert snr == {"5": 25, "10": 25, "15": 25, "20": 25}
    assert {b["by"] for b in record["breakdowns"] if b["codec"] == "clean16k"} == {
        "source",
        "duration_bin",
    }


def check_wer_gates(scorer: Scorer) -> None:
    built = scorer.built
    noisy = file_ids(built, "en", "noisy16k")
    words = 100 * 21
    # 25% of 2,100 words is 525 errors; the batch arm keeps the delta inside 2 pp.
    at_ceiling = scorer.score(
        built.arm("streaming", with_errors(built, noisy, 525)),
        built.arm("batch", with_errors(built, noisy, 525 - 42)),
    )
    row = stratum(at_ceiling, "en", "noisy16k")
    assert row["verdict"] == "pass" and row["streaming"]["errors"] == 525
    assert row["streaming"]["wer_percent"] == 25.0 and row["delta_pp"] == 2.0
    assert row["streaming"]["substitutions"] == 525 and row["batch"]["errors"] == 483
    over = scorer.score(
        built.arm("streaming", with_errors(built, noisy, 526)),
        built.arm("batch", with_errors(built, noisy, 526)),
    )
    assert stratum(over, "en", "noisy16k")["verdict"] == "fail"
    assert over["verdict"] == "fail" and "wer en/noisy16k:fail" in quality.unmet(over)
    delta = scorer.score(
        built.arm("streaming", with_errors(built, noisy, 43)),
        built.arm("batch", with_errors(built, noisy, 0)),
    )
    assert stratum(delta, "en", "noisy16k")["verdict"] == "fail"
    assert stratum(delta, "en", "noisy16k")["delta_pp"] == round(4300 / words, 3)
    inside = scorer.score(built.arm("streaming", with_errors(built, noisy, 42)))
    assert stratum(inside, "en", "noisy16k")["verdict"] == "pass"
    # The rate, the ceiling and the distance divide by the normalized words, not the raw ones.
    manifest = dashed(scorer.manifest)
    at_ceiling = scorer.score(
        built.arm("streaming", with_errors(built, noisy, 525)),
        built.arm("batch", with_errors(built, noisy, 525)),
        manifest,
    )
    row = stratum(at_ceiling, "en", "noisy16k")
    assert (row["reference_words"], row["raw_reference_words"]) == (words, words + 100)
    assert row["streaming"]["wer_percent"] == 25.0 and row["verdict"] == "pass"
    assert row["streaming"]["raw_errors"] == 525 + 100
    assert row["streaming"]["raw_wer_percent"] == round(62500 / (words + 100), 3)
    over = scorer.score(
        built.arm("streaming", with_errors(built, noisy, 526)),
        built.arm("batch", with_errors(built, noisy, 526)),
        manifest,
    )
    assert stratum(over, "en", "noisy16k")["verdict"] == "fail"
    delta = scorer.score(built.arm("streaming", with_errors(built, noisy, 43)), manifest=manifest)
    row = stratum(delta, "en", "noisy16k")
    assert (row["delta_pp"], row["verdict"]) == (round(4300 / words, 3), "fail")
    # A batch arm worse than streaming leaves a negative distance, not a failure.
    better = scorer.score(batch=built.arm("batch", with_errors(built, noisy, 100)))
    assert stratum(better, "en", "noisy16k")["delta_pp"] < 0
    assert stratum(better, "en", "noisy16k")["verdict"] == "pass"
    assert better["semantic"]["regressions"] == []
    # Each stratum at its own ceiling passes, and one error over it fails.
    assert quality.CEILING_PERCENT == CEILING_PERCENT
    for extra, verdict in ((0, "pass"), (1, "fail")):
        both: dict[str, Any] = {}
        for (language, codec), ceiling in CEILING_PERCENT.items():
            ids = file_ids(built, language, codec)
            both |= with_errors(built, ids, ceiling * len(ids) * 21 // 100 + extra)
        record = scorer.score(built.arm("streaming", both), built.arm("batch", both))
        assert [s["verdict"] for s in record["strata"]] == [verdict] * 6, record["strata"]


def check_failures_and_raw(scorer: Scorer) -> None:
    built = scorer.built
    words = len(file_ids(built, "ar", "clean16k")) * 21
    first, second = file_ids(built, "ar", "clean16k")[60:62]
    failed = {
        first: {"id": first, "outcome": "failed", "error": "inference_failed"},
        second: {"id": second, "outcome": "completed", "finals": []},
    }
    record = scorer.score(
        built.arm("streaming", failed), built.arm("batch", {first: failed[first]})
    )
    row = stratum(record, "ar", "clean16k")
    assert row["streaming"]["deletions"] == 42 and row["streaming"]["errors"] == 42
    assert row["streaming"]["failed_files"] == 1
    assert row["streaming"]["failures"] == {"inference_failed": 1}
    assert row["batch"]["errors"] == 21 and row["batch"]["failures"] == {"inference_failed": 1}
    assert row["delta_pp"] == round(2100 / words, 3)
    assert row["verdict"] == "insufficient"
    # The file completed with no finals is counted, the failed one is not.
    assert (row["streaming"]["empty_files"], row["batch"]["empty_files"]) == (1, 0)
    blank = {second: {"id": second, "outcome": "completed", "finals": ["", " ... "]}}
    row = stratum(scorer.score(batch=built.arm("batch", blank)), "ar", "clean16k")
    assert (row["streaming"]["empty_files"], row["batch"]["empty_files"]) == (0, 1)
    assert row["batch"]["deletions"] == 21 and row["batch"]["failed_files"] == 0
    clip = file_ids(built, "en", "clean16k")[70]
    text = built.texts[clip][1]
    split = text.split()
    styled = " ".join([split[0].upper() + ","] + split[1:]) + "."
    finals = [" ".join(split[:10]), " ".join(split[10:])]
    record = scorer.score(
        built.arm(
            "streaming",
            {clip: {"id": clip, "outcome": "completed", "finals": [styled]}},
        ),
        built.arm("batch", {clip: {"id": clip, "outcome": "completed", "finals": finals}}),
    )
    row = stratum(record, "en", "clean16k")
    assert row["streaming"]["errors"] == 0 and row["streaming"]["raw_errors"] == 2
    assert row["batch"]["errors"] == 0 and row["batch"]["raw_errors"] == 0
    assert row["raw_reference_words"] == row["reference_words"] == words
    assert row["streaming"]["raw_wer_percent"] == round(200 / words, 3)
    # A reference written with case and punctuation: the raw rate scores it as written.
    written = " ".join([split[0].capitalize() + ","] + split[1:20] + ["Again."])
    manifest = copy.deepcopy(scorer.manifest)
    for entry in manifest["files"]:
        if entry["id"].startswith(clip.split(".")[0] + "."):
            entry["transcript"] = {"sha256": sha(written.encode()), "text": written}
    plain = " ".join(split[:20] + ["again"])
    record = scorer.score(
        built.arm("streaming", {clip: {"id": clip, "outcome": "completed", "finals": [written]}}),
        built.arm("batch", {clip: {"id": clip, "outcome": "completed", "finals": [plain]}}),
        manifest,
    )
    row = stratum(record, "en", "clean16k")
    assert (row["streaming"]["errors"], row["streaming"]["raw_errors"]) == (0, 0)
    assert (row["batch"]["errors"], row["batch"]["raw_errors"]) == (0, 2)


def check_sample_minimums(scorer: Scorer) -> None:
    built = scorer.built
    gone = "en-000.noisy16k"
    fewer = copy.deepcopy(scorer.manifest)
    fewer["files"] = [e for e in fewer["files"] if e["id"] != gone]

    def without(arm: str, hypotheses: dict[str, Any] | None = None) -> dict[str, Any]:
        document = built.arm(arm, hypotheses)
        document["clips"] = [c for c in document["clips"] if c["id"] != gone]
        return document

    record = scorer.score(without("streaming"), without("batch"), fewer)
    assert stratum(record, "en", "noisy16k")["verdict"] == "insufficient"
    assert record["verdict"] == "insufficient"
    assert quality.unmet(record) == ["wer en/noisy16k:insufficient"]
    # Below its minimum a stratum over its ceiling is insufficient, not failed.
    ids = [i for i in file_ids(built, "en", "noisy16k") if i != gone]
    record = scorer.score(
        without("streaming", with_errors(built, ids, 99 * 21)), without("batch"), fewer
    )
    assert stratum(record, "en", "noisy16k")["verdict"] == "insufficient"

    record = Scorer(Built(recordings=499)).score()
    assert quality.unmet(record) == ["corpus en:insufficient", "corpus ar:insufficient"]
    # 500 recordings, 502 words short of 10,500: under the word minimum.
    trimmed = Built()
    for entry in trimmed.files:
        if entry["language"] == "en" and 100 <= int(entry["id"][3:6]) < 351:
            text = " ".join(entry["transcript"]["text"].split()[:19])
            entry["transcript"] = {"sha256": sha(text.encode()), "text": text}
            trimmed.texts[entry["id"]] = ("en", text)
    record = Scorer(trimmed).score()
    english = by_language(record["languages"], "en")
    assert (english["recordings"], english["reference_words"]) == (500, 9_998)
    assert quality.unmet(record) == ["corpus en:insufficient"]

    record = Scorer(Built(fixtures=49)).score()
    assert quality.unmet(record) == ["semantic en:insufficient", "semantic ar:insufficient"]
    # Fifty fixtures with a class short of ten, or missing, are insufficient.
    for seams, verdict in ((9, "insufficient"), (10, "pass"), (0, "insufficient")):
        built = Built(
            recordings=100,
            classes=lambda clip, seams=seams: (
                "window_seam" if clip < seams else quality.SEMANTIC_CLASSES[clip % 3]
            ),
        )
        for item in Scorer(built).score()["semantic"]["languages"]:
            assert item["fixtures"] == 50 and item["by_class"]["window_seam"]["fixtures"] == seams
            assert item["verdict"] == verdict, (seams, item)
            assert item["by_class"]["window_seam"]["verdict"] == verdict
            assert {item["by_class"][name]["verdict"] for name in quality.SEMANTIC_CLASSES[:3]} == {
                "pass"
            }


def check_silence(scorer: Scorer) -> None:
    built = scorer.built
    loud = {
        "sil-007.telephony8k": {
            "id": "sil-007.telephony8k",
            "outcome": "completed",
            "finals": ["Thank you."],
        }
    }
    record = scorer.score(built.arm("streaming", loud))
    assert record["silence"]["streaming"]["hallucinated_words"] == 2
    assert record["silence"]["streaming"]["files_with_words"] == 1
    assert record["silence"]["verdict"] == "fail" and record["verdict"] == "fail"
    assert record["silence"]["by_codec"][1]["streaming"] == {"files": 100, "hallucinated_words": 2}
    batch_only = scorer.score(batch=built.arm("batch", loud))
    assert batch_only["silence"]["verdict"] == "pass"
    assert batch_only["silence"]["batch"]["hallucinated_words"] == 2
    quiet = {
        "sil-007.clean16k": {
            "id": "sil-007.clean16k",
            "outcome": "completed",
            "finals": ["...", " "],
        }
    }
    assert scorer.score(built.arm("streaming", quiet))["silence"]["verdict"] == "pass"
    # Tatweel alone is a word only under the English normalizer, so it counts.
    mark = {
        "sil-007.clean16k": {"id": "sil-007.clean16k", "outcome": "completed", "finals": ["\u0640"]}
    }
    assert scorer.score(built.arm("streaming", mark))["silence"]["verdict"] == "fail"
    failed = {
        "sil-001.clean16k": {"id": "sil-001.clean16k", "outcome": "failed", "error": "timeout"}
    }
    record = scorer.score(built.arm("streaming", failed))
    assert record["silence"]["streaming"]["failures"] == {"timeout": 1}
    assert (record["silence"]["recordings"], record["silence"]["scored_recordings"]) == (100, 99)
    assert record["silence"]["verdict"] == "insufficient"
    assert scorer.score(batch=built.arm("batch", failed))["silence"]["verdict"] == "pass"
    # A recording is scored only when each of its renderings completed.
    phone = {
        "sil-002.telephony8k": {
            "id": "sil-002.telephony8k",
            "outcome": "failed",
            "error": "timeout",
        }
    }
    silence = scorer.score(built.arm("streaming", phone))["silence"]
    assert (silence["scored_recordings"], silence["verdict"]) == (99, "insufficient")
    record = scorer.score(built.arm("streaming", failed | phone))
    assert record["silence"]["scored_recordings"] == 98
    record = scorer.score(built.arm("streaming", loud | failed), cross_check=True)
    assert record["silence"]["verdict"] == "fail"

    fewer = copy.deepcopy(scorer.manifest)
    gone = {"sil-000.clean16k", "sil-000.telephony8k"}
    fewer["files"] = [e for e in fewer["files"] if e["id"] not in gone]

    def without(arm: str, hypotheses: dict[str, Any] | None = None) -> dict[str, Any]:
        document = built.arm(arm, hypotheses)
        document["clips"] = [c for c in document["clips"] if c["id"] not in gone]
        return document

    record = scorer.score(without("streaming"), without("batch"), fewer)
    assert record["silence"]["recordings"] == 99 and record["silence"]["verdict"] == "insufficient"
    # A hallucination fails the gate even below the minimum.
    record = scorer.score(without("streaming", loud), without("batch"), fewer)
    assert record["silence"]["verdict"] == "fail"


def check_semantic_run(scorer: Scorer) -> None:
    built = scorer.built
    negation = "en-000.clean16k"
    text = built.texts[negation][1]
    assert text.endswith(" not")
    dropped = {negation: {"id": negation, "outcome": "completed", "finals": [text[: -len(" not")]]}}
    record = scorer.score(built.arm("streaming", dropped))
    assert record["semantic"]["regressions"] == [
        {
            "file": negation,
            "language": "en",
            "codec": "clean16k",
            "class": "negation",
            "kind": "lost",
            "position": 20,
            "count": 1,
            "words": ["not"],
        }
    ]
    english = by_language(record["semantic"]["languages"], "en")
    assert english["verdict"] == "fail" and english["regressions"] == 1
    assert english["by_class"]["negation"]["regressions"] == 1
    assert [english["by_class"][name]["verdict"] for name in quality.SEMANTIC_CLASSES] == [
        "fail",
        "pass",
        "pass",
        "pass",
    ]
    assert by_language(record["semantic"]["languages"], "ar")["verdict"] == "pass"
    assert record["verdict"] == "fail"
    both = scorer.score(built.arm("streaming", dropped), built.arm("batch", dropped))
    assert by_language(both["semantic"]["languages"], "en")["verdict"] == "pass"
    seam = "ar-003.telephony8k"
    words = built.texts[seam][1].split()
    cut = {seam: {"id": seam, "outcome": "completed", "finals": [" ".join(words[:5] + words[7:])]}}
    record = scorer.score(built.arm("streaming", cut), cross_check=True)
    assert [
        (r["file"], r["class"], r["kind"], r["position"]) for r in record["semantic"]["regressions"]
    ] == [(seam, "window_seam", "lost", 5)]
    # The Arabic references are held by digest, so the record names no words of them.
    assert [(r["count"], r["words"]) for r in record["semantic"]["regressions"]] == [(2, None)]
    # A fixture the batch arm failed cannot show a regression, so the gate cannot pass.
    failed = {negation: {"id": negation, "outcome": "failed", "error": "inference_failed"}}
    record = scorer.score(built.arm("streaming", dropped), built.arm("batch", failed))
    english = by_language(record["semantic"]["languages"], "en")
    assert record["semantic"]["regressions"] == []
    assert (english["batch_failed_files"], english["verdict"]) == (1, "insufficient")
    negation_class = english["by_class"]["negation"]
    assert (negation_class["batch_failed_files"], negation_class["verdict"]) == (1, "insufficient")
    assert english["by_class"]["numerals"]["verdict"] == "pass"
    record = scorer.score(built.arm("streaming", cut), built.arm("batch", failed))
    english = by_language(record["semantic"]["languages"], "en")
    assert english["verdict"] == "insufficient"
    assert by_language(record["semantic"]["languages"], "ar")["verdict"] == "fail"
    # Nor is a word the streaming arm added on it, no regression when batch adds it too.
    never = text[: -len(" not")] + " never not"
    added = {negation: {"id": negation, "outcome": "completed", "finals": [never]}}
    record = scorer.score(built.arm("streaming", added), built.arm("batch", failed))
    english = by_language(record["semantic"]["languages"], "en")
    assert record["semantic"]["regressions"] == []
    assert (english["batch_failed_files"], english["verdict"]) == (1, "insufficient")
    record = scorer.score(built.arm("streaming", added), built.arm("batch", added))
    assert by_language(record["semantic"]["languages"], "en")["verdict"] == "pass"
    seam_en = "en-003.clean16k"
    seam_words = built.texts[seam_en][1].split()
    doubled = seam_words[:4] + seam_words[2:4] + seam_words[4:]
    inserted = {seam_en: {"id": seam_en, "outcome": "completed", "finals": [" ".join(doubled)]}}
    failed_seam = {seam_en: {"id": seam_en, "outcome": "failed", "error": "timeout"}}
    assert [
        r["kind"] for r in scorer.score(built.arm("streaming", inserted))["semantic"]["regressions"]
    ] == ["added"]
    record = scorer.score(built.arm("streaming", inserted), built.arm("batch", failed_seam))
    assert record["semantic"]["regressions"] == []
    assert by_language(record["semantic"]["languages"], "en")["verdict"] == "insufficient"
    # A batch arm that failed every noisy file judges none of their fixtures.
    noisy_ids = file_ids(built, "en", "noisy16k")
    repeated = "en-002.noisy16k"
    assert built.texts[repeated][1].endswith(" again again")
    record = scorer.score(
        built.arm(
            "streaming",
            {
                repeated: {
                    "id": repeated,
                    "outcome": "completed",
                    "finals": [built.texts[repeated][1] + " again"],
                }
            },
        ),
        built.arm(
            "batch", {i: {"id": i, "outcome": "failed", "error": "timeout"} for i in noisy_ids}
        ),
    )
    english = by_language(record["semantic"]["languages"], "en")
    assert record["semantic"]["regressions"] == [] and record["verdict"] == "insufficient"
    assert (english["batch_failed_files"], english["verdict"]) == (50, "insufficient")
    # A rendering of a fixture is a fixture: its words are listed, and its batch failure counts.
    noisy = "en-000.noisy16k"
    dropped_noisy = {noisy: {"id": noisy, "outcome": "completed", "finals": [text[: -len(" not")]]}}
    record = scorer.score(built.arm("streaming", dropped_noisy))
    assert [(r["file"], r["count"], r["words"]) for r in record["semantic"]["regressions"]] == [
        (noisy, 1, ["not"])
    ]
    failed_noisy = {noisy: {"id": noisy, "outcome": "failed", "error": "timeout"}}
    record = scorer.score(batch=built.arm("batch", failed_noisy))
    english = by_language(record["semantic"]["languages"], "en")
    assert (english["batch_failed_files"], english["verdict"]) == (1, "insufficient")
    # Fewer fixtures than the minimum and a regression: the regression fails the gate.
    few = Scorer(Built(recordings=100, fixtures=49))
    record = few.score(few.built.arm("streaming", dropped))
    assert by_language(record["semantic"]["languages"], "en")["verdict"] == "fail"


def check_batch_failures(scorer: Scorer) -> None:
    built = scorer.built
    noisy = file_ids(built, "en", "noisy16k")
    # 60 errors is 2.857 pp above a clean batch arm: over the distance.
    over = with_errors(built, noisy[10:], 60)
    record = scorer.score(built.arm("streaming", over))
    assert stratum(record, "en", "noisy16k")["verdict"] == "fail"
    # Two failed batch files bring the distance to 0.857 pp, which proves nothing.
    failed = {i: {"id": i, "outcome": "failed", "error": "timeout"} for i in noisy[:2]}
    record = scorer.score(built.arm("streaming", over), built.arm("batch", failed))
    row = stratum(record, "en", "noisy16k")
    assert (row["delta_pp"], row["verdict"]) == (0.857, "insufficient")
    # A distance still over the limit fails, failed batch files or not.
    record = scorer.score(
        built.arm("streaming", with_errors(built, noisy[10:], 120)), built.arm("batch", failed)
    )
    assert stratum(record, "en", "noisy16k")["verdict"] == "fail"


def check_breakdowns(scorer: Scorer) -> None:
    built = scorer.built
    noisy = file_ids(built, "en", "noisy16k")
    five = [
        i for i in noisy if next(e for e in built.files if e["id"] == i)["recipe"]["snr_db"] == 5
    ]
    assert len(five) == 25
    record = scorer.score(built.arm("streaming", with_errors(built, five, 25 * 3)))
    rates = {
        (b["by"], b["value"]): (
            b["utterances"],
            b["reference_words"],
            b["streaming_wer_percent"],
            b["batch_wer_percent"],
        )
        for b in record["breakdowns"]
        if (b["language"], b["codec"]) == ("en", "noisy16k")
    }
    assert rates == {
        ("snr_db", "5"): (25, 525, round(7500 / 525, 3), 0.0),
        ("snr_db", "10"): (25, 525, 0.0, 0.0),
        ("snr_db", "15"): (25, 525, 0.0, 0.0),
        ("snr_db", "20"): (25, 525, 0.0, 0.0),
        ("source", "read"): (100, 2100, round(7500 / 2100, 3), 0.0),
        ("duration_bin", "1to5s"): (100, 2100, round(7500 / 2100, 3), 0.0),
    }, rates
    assert {(b["by"], b["value"]) for b in record["breakdowns"] if b["language"] == "ar"} == {
        ("source", "crowd"),
        ("duration_bin", "1to5s"),
        ("snr_db", "5"),
        ("snr_db", "10"),
        ("snr_db", "15"),
        ("snr_db", "20"),
    }
    # Two recordings of one sentence are two recordings and one distinct reference.
    manifest = copy.deepcopy(scorer.manifest)
    shared = built.texts["en-061.clean16k"][1]
    for entry in manifest["files"]:
        if entry["id"].startswith("en-060."):
            entry["transcript"] = {"sha256": sha(shared.encode()), "text": shared}
    record = scorer.score(manifest=manifest)
    english = by_language(record["languages"], "en")
    assert (english["recordings"], english["distinct_references"]) == (100, 99)


def check_hypothesis_text(scorer: Scorer) -> None:
    built = scorer.built
    clip = file_ids(built, "ar", "clean16k")[80]
    words = built.texts[clip][1].split()
    forms = {
        clip: {"id": clip, "outcome": "completed", "finals": [" ".join(words) + " \ufefb\u200f"]}
    }
    record = scorer.score(built.arm("streaming", forms))
    assert record["hypothesis_text"]["streaming"] == {
        "presentation_forms": 1,
        "format_characters": 1,
    }
    assert record["hypothesis_text"]["batch"] == {"presentation_forms": 0, "format_characters": 0}
    assert stratum(record, "ar", "clean16k")["streaming"]["insertions"] == 1


def check_refusals(scorer: Scorer) -> None:
    built = scorer.built
    streaming, batch = built.arm("streaming"), built.arm("batch")

    def edit(document: dict[str, Any], change: Callable[[dict[str, Any]], None]) -> dict[str, Any]:
        changed = copy.deepcopy(document)
        change(changed)
        return changed

    scorer.refused("do not match their schema", streaming=edit(streaming, lambda d: d.pop("pool")))
    scorer.refused(
        "do not match their schema",
        streaming=edit(streaming, lambda d: d["clips"][0].update(outcome="failed")),
    )
    scorer.refused("those of the batch arm", streaming=batch)
    scorer.refused("those of the streaming arm", batch=streaming)
    scorer.refused(
        "another corpus manifest", batch=edit(batch, lambda d: d.update(manifest_sha256="f" * 64))
    )
    scorer.refused("the batch arm tuning", batch=edit(batch, lambda d: d.update(pool="tuning")))
    scorer.refused(
        "the manifest has no tuning files",
        streaming=edit(streaming, lambda d: d.update(pool="tuning")),
        batch=edit(batch, lambda d: d.update(pool="tuning")),
    )
    scorer.refused(
        "missing ['ar-000.clean16k']",
        streaming=edit(
            streaming,
            lambda d: d.update(clips=[c for c in d["clips"] if c["id"] != "ar-000.clean16k"]),
        ),
    )
    scorer.refused(
        "outside the pool ['zz.clean16k']",
        batch=edit(
            batch,
            lambda d: d["clips"].append(
                {"id": "zz.clean16k", "outcome": "completed", "finals": []}
            ),
        ),
    )
    scorer.refused(
        "appears twice", streaming=edit(streaming, lambda d: d["clips"].append(d["clips"][3]))
    )
    scorer.refused(
        "'no_such_error' is not an error code",
        batch=edit(
            batch,
            lambda d: d["clips"].__setitem__(
                0, {"id": d["clips"][0]["id"], "outcome": "failed", "error": "no_such_error"}
            ),
        ),
    )
    scorer.refused(
        "unpaired surrogate",
        streaming=edit(streaming, lambda d: d["clips"][0]["finals"].append("a\ud800")),
    )
    missing = dict(built.references)
    missing.pop(next(iter(missing)))
    scorer.refused("is not provided", references=missing)

    def reference(text: str) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
        manifest = copy.deepcopy(scorer.manifest)
        for entry in manifest["files"]:
            if entry["id"].startswith("en-090."):
                entry["transcript"] = {"sha256": sha(text.encode()), "text": text}
        return manifest, streaming, batch

    for text, fragment in (
        ("ok \ufefb", "carries a presentation_form"),
        ("ok\u200fgo", "carries a format_character"),
        ("ok \u0007", "carries a format_character"),
        ("ok\u001cgo", "carries a format_character"),
        ("ok\u0085go", "carries a format_character"),
        (" ... ", "has no words once normalized"),
    ):
        manifest, s, b = reference(text)
        scorer.refused(fragment, manifest=manifest, streaming=s, batch=b)
    twice = copy.deepcopy(scorer.manifest)
    for entry in twice["files"]:
        if entry["id"].startswith("en-000."):
            entry["semantic"] = ["negation", "negation"]
    scorer.refused("a semantic class is listed twice", manifest=twice)
    broken = copy.deepcopy(scorer.manifest)
    broken["files"][0]["duration_bin"] = "lt1s"
    scorer.refused("is bin '1to5s'", manifest=broken)

    good = next(iter(built.references.items()))
    for document, fragment in (
        ([], "not an object"),
        ({good[0].upper(): good[1]}, "is not a digest with a text"),
        ({good[0] + "\n": good[1]}, "is not a digest with a text"),
        ({good[0]: ""}, "is not a digest with a text"),
        ({good[0]: good[1] + " "}, "is not the digest of its text"),
        ({sha("a\ud800".encode("utf-8", "surrogatepass")): "a\ud800"}, "unpaired surrogate"),
    ):
        try:
            quality.load_references(document)
        except quality.QualityError as exc:
            assert fragment in str(exc), (document, exc)
        else:
            raise AssertionError(f"references accepted: {fragment}")
    assert quality.load_references(dict([good])) == dict([good])


def check_schemas() -> None:
    for path in (quality.TRANSCRIPTS_SCHEMA_PATH, quality.RECORD_SCHEMA_PATH):
        jsonschema.Draft7Validator.check_schema(json.loads(path.read_text(encoding="utf-8")))


def run_cli(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(TOOL), "score", *args], capture_output=True, text=True, check=False
    )


def check_cli(scorer: Scorer) -> None:
    built = scorer.built
    with tempfile.TemporaryDirectory(prefix="tp-speech-quality-") as tmp:
        root = Path(tmp)
        manifest_path = root / "manifest.json"
        manifest_path.write_text(json.dumps(scorer.manifest, ensure_ascii=False), encoding="utf-8")
        digest = sha(manifest_path.read_bytes())

        def write(name: str, document: Any) -> Path:
            path = root / name
            path.write_text(json.dumps(document), encoding="utf-8")
            return path

        def arm(name: str, hypotheses: dict[str, Any] | None = None) -> Path:
            return write(f"{name}.json", built.arm(name, hypotheses) | {"manifest_sha256": digest})

        references = write("references.json", built.references)
        streaming, batch = arm("streaming"), arm("batch")
        args = [str(manifest_path), "--streaming", str(streaming), "--batch", str(batch)]
        done = run_cli(*args, "--references", str(references))
        assert done.returncode == 0, done.stderr
        record = json.loads(done.stdout)
        assert record["verdict"] == "pass" and done.stderr == "speech-quality: pass\n"
        assert record["inputs"] == {
            "streaming_sha256": sha(streaming.read_bytes()),
            "batch_sha256": sha(batch.read_bytes()),
            "references_sha256": sha(references.read_bytes()),
        }
        assert record["corpus"]["manifest_sha256"] == digest
        output = root / "record.json"
        loud = {
            "sil-001.clean16k": {
                "id": "sil-001.clean16k",
                "outcome": "completed",
                "finals": ["you"],
            }
        }
        done = run_cli(
            str(manifest_path),
            "--streaming",
            str(arm("streaming", loud)),
            "--batch",
            str(batch),
            "--references",
            str(references),
            "--output",
            str(output),
        )
        assert done.returncode == 3 and done.stdout == "", done
        assert done.stderr == "speech-quality: fail: silence:fail\n", done.stderr
        assert json.loads(output.read_text(encoding="utf-8"))["silence"]["verdict"] == "fail"
        unnamed = copy.deepcopy(scorer.manifest)
        unnamed["corpus"]["name"] = "synthetic \ud800"
        unnamed_path = write("unnamed.json", unnamed)
        digest_unnamed = sha(unnamed_path.read_bytes())
        done = run_cli(
            str(unnamed_path),
            "--streaming",
            str(write("s2.json", built.arm("streaming") | {"manifest_sha256": digest_unnamed})),
            "--batch",
            str(write("b2.json", built.arm("batch") | {"manifest_sha256": digest_unnamed})),
            "--references",
            str(references),
            "--output",
            str(root / "unnamed-record.json"),
        )
        assert done.returncode == 1 and "speech-quality: refused: " in done.stderr, done
        assert not (root / "unnamed-record.json").exists()
        done = run_cli(*args)
        assert done.returncode == 1 and "speech-quality: refused: " in done.stderr
        assert "is not provided" in done.stderr
        (root / "bad.json").write_text("{", encoding="utf-8")
        done = run_cli(str(root / "bad.json"), "--streaming", str(streaming), "--batch", str(batch))
        assert done.returncode == 2 and "speech-quality: unreadable: " in done.stderr
        done = run_cli(*args[:3], "--batch", str(root / "absent.json"))
        assert done.returncode == 2 and "absent.json" in done.stderr
        surrogate = copy.deepcopy(scorer.manifest)
        for entry in surrogate["files"]:
            if entry["id"] == "en-400.clean16k":
                entry["transcript"]["text"] = "ok \ud800"
        done = run_cli(str(write("surrogate.json", surrogate)), *args[1:])
        assert done.returncode == 1 and "speech-quality: refused: " in done.stderr, done
        (root / "latin1.json").write_bytes(b'{"a": "\xe9"}')
        done = run_cli(*args, "--references", str(root / "latin1.json"))
        assert done.returncode == 2, done


def main() -> int:
    check_english_normalizer()
    check_arabic_normalizer()
    check_idempotence()
    check_lexicons()
    check_alignment()
    check_critical_words()
    check_lexical_regressions()
    check_seam_regressions()
    check_schemas()
    # Every stratum at its minimum; only the corpus size checks need the full corpus.
    small = Scorer(Built(recordings=100))
    check_wer_gates(small)
    check_failures_and_raw(small)
    check_silence(small)
    check_semantic_run(small)
    check_batch_failures(small)
    check_breakdowns(small)
    check_hypothesis_text(small)
    check_refusals(small)
    full = Scorer(Built())
    check_passing_run(full)
    check_sample_minimums(full)
    check_cli(full)
    print("speech quality: normalizers, alignment, gates, refusals and the command line passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
