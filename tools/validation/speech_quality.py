#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Score a speech evaluation run's final transcripts against the corpus references.

A run has two arms over the same pool of a corpus: the streaming arm, the
product's speech session, and the batch arm, the same model and configuration
decoding each whole file. Each arm's finals are read from a transcripts file
(`config/schemas/speech_transcripts.json`); the references come from the corpus
manifest, or by digest from a private references file.

Both sides are normalized per language before scoring: English by Unicode NFC,
casefold, punctuation to space, NFC again and whitespace collapse; Arabic by
NFC, tatweel and harakat removed, punctuation to space, NFC again and
whitespace collapse, every other letter and every numeral kept as written. The
record holds, per language and codec, each arm's micro word error rate,
normalized and raw, and the streaming arm's distance from the batch arm; the
words decoded on silence; and the semantic fixtures' critical regressions
against the batch arm. A failed file is scored as an empty prediction and
counted as a failure; a completed speech file with no words is counted too.
Each gate reads pass, fail, or insufficient when it has fewer samples than its
minimum.

Exit status: 0 every gate passed, 1 refused, 2 unreadable, 3 a gate failed or
had too few samples.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import unicodedata
from collections import Counter
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
TRANSCRIPTS_SCHEMA_PATH = REPO_ROOT / "config/schemas/speech_transcripts.json"
RECORD_SCHEMA_PATH = REPO_ROOT / "config/schemas/speech_quality_record.json"
SCORER = "tools/validation/speech_quality.py"

sys.path.insert(0, str(HERE))
import speech_corpus as corpus  # noqa: E402
from candidate_record import RecordError, load_error_codes, validate_record  # noqa: E402

TOOL_NAME = "speech-quality"

ARMS = ("streaming", "batch")
LANGUAGES = ("en", "ar")
NORMALIZERS = {"en": "tp-wer-en-nfc-casefold-v1", "ar": "tp-wer-ar-nfc-dediacritized-v1"}
TATWEEL = "\u0640"
# Tanwin, the short vowels, shadda, sukun and the superscript alef. Hamza and
# madda stay: NFC composes them with their carrier into letters of their own.
HARAKAT = frozenset(chr(code) for code in range(0x064B, 0x0653)) | {"\u0670"}
# Arabic Presentation Forms-A and -B, which NFC leaves as they are.
PRESENTATION_FORM = re.compile("[\ufb50-\ufdff\ufe70-\ufeff]")
WHITESPACE_CONTROLS = "\t\n\v\f\r"
TEXT_COUNTS = {"presentation_form": "presentation_forms", "format_character": "format_characters"}

# The release quality gates: the micro WER ceiling of each stratum, the
# streaming arm's largest distance above batch, and the minimum samples below
# which a gate is insufficient.
CEILING_PERCENT = {
    ("en", "clean16k"): 15,
    ("en", "noisy16k"): 25,
    ("en", "telephony8k"): 25,
    ("ar", "clean16k"): 25,
    ("ar", "noisy16k"): 35,
    ("ar", "telephony8k"): 35,
}
BATCH_DELTA_PP = 2
MIN_STRATUM_UTTERANCES = 100
MIN_LANGUAGE_RECORDINGS = 500
MIN_LANGUAGE_WORDS = 10_000
MIN_SILENCE_RECORDINGS = 100
MIN_SEMANTIC_FIXTURES = 50
MIN_SEMANTIC_CLASS_FIXTURES = 10

SEMANTIC_CLASSES = ("negation", "numerals", "repetition", "window_seam")
REPEAT_WINDOW = 3
SEAM_RUN = 2
# The conjunctions Arabic writes joined to the next word.
ARABIC_PREFIXES = ("و", "ف")
NEGATION = {
    "en": frozenset("no not never nothing none nobody nowhere neither nor cannot".split()),
    "ar": frozenset("لا لم لن ليس ليست ما".split()),
}
NUMBER_WORDS = {
    "en": frozenset(
        (
            "zero one two three four five six seven eight nine ten eleven twelve thirteen "
            "fourteen fifteen sixteen seventeen eighteen nineteen twenty thirty forty fifty "
            "sixty seventy eighty ninety hundred thousand million billion trillion"
        ).split()
    ),
    "ar": frozenset(
        (
            "صفر واحد واحدة اثنان اثنين اثنتان اثنتين إثنان إثنين ثلاث ثلاثة أربع أربعة "
            "خمس خمسة ست ستة سبع سبعة ثمان ثماني ثمانية تسع تسعة عشر عشرة "
            "عشرون عشرين ثلاثون ثلاثين أربعون أربعين خمسون خمسين ستون ستين سبعون سبعين "
            "ثمانون ثمانين تسعون تسعين مئة مائة مئتان مئتين مائتان مائتين "
            "ثلاثمئة ثلاثمائة أربعمئة أربعمائة خمسمئة خمسمائة ستمئة ستمائة سبعمئة سبعمائة "
            "ثمانمئة ثمانمائة تسعمئة تسعمائة ألف ألفا ألفان ألفين آلاف "
            "مليون مليونا مليونان مليونين ملايين مليار ملياران مليارين مليارات"
        ).split()
    ),
}

EXIT_PASS = 0
EXIT_REFUSED = 1
EXIT_UNREADABLE = 2
EXIT_GATE = 3

SHA256_HEX = re.compile(r"[0-9a-f]{64}")

Ops = list[tuple[str, int | None, int | None]]


class QualityError(Exception):
    """The run's transcripts or references cannot be scored against this corpus."""


class _PunctuationToSpace(dict):  # type: ignore[type-arg]
    """A str.translate table mapping every Unicode punctuation character to a space."""

    def __missing__(self, code: int) -> int:
        value = 0x20 if unicodedata.category(chr(code)).startswith("P") else code
        self[code] = value
        return value


PUNCTUATION_TO_SPACE = _PunctuationToSpace()
DEDIACRITIZE = {ord(ch): None for ch in HARAKAT | {TATWEEL}}


def normalize(text: str, language: str) -> list[str]:
    """The words the frozen normalizer of `language` leaves in `text`.

    The last NFC pass composes what the case fold or a removal left decomposed, so the
    words are NFC and normalizing them again changes nothing.
    """
    text = unicodedata.normalize("NFC", text)
    if language == "en":
        text = text.casefold()
    elif language == "ar":
        text = text.translate(DEDIACRITIZE)
    else:
        raise QualityError(f"no normalizer for language {language!r}")
    return unicodedata.normalize("NFC", text.translate(PUNCTUATION_TO_SPACE)).split()


def text_findings(text: str) -> set[str]:
    """Characters NFC keeps that no transcript should need."""
    found = set()
    if PRESENTATION_FORM.search(text):
        found.add("presentation_form")
    # Every Cc and Cf character is unprintable, so printable text has none.
    if not text.isprintable() and any(
        unicodedata.category(ch) in ("Cc", "Cf") and ch not in WHITESPACE_CONTROLS for ch in text
    ):
        found.add("format_character")
    return found


def _require_text(text: str, what: str) -> None:
    if any(unicodedata.category(ch) == "Cs" for ch in text):
        raise QualityError(f"{what}: not Unicode text (an unpaired surrogate)")


def align(ref: list[str], hyp: list[str]) -> Ops:
    """A minimum edit-distance alignment as (op, reference index, hypothesis index).

    The ops are match, sub, del and ins. On a tie the backtrace from the end
    takes a match or substitution first, then a deletion, so each pair of word
    lists has one alignment.
    """
    if ref == hyp:
        return [("match", i, i) for i in range(len(ref))]
    rows, cols = len(ref), len(hyp)
    cost = [[0] * (cols + 1) for _ in range(rows + 1)]
    for i in range(rows + 1):
        cost[i][0] = i
    for j in range(cols + 1):
        cost[0][j] = j
    for i in range(1, rows + 1):
        for j in range(1, cols + 1):
            cost[i][j] = min(
                cost[i - 1][j - 1] + (ref[i - 1] != hyp[j - 1]),
                cost[i - 1][j] + 1,
                cost[i][j - 1] + 1,
            )
    ops: Ops = []
    i, j = rows, cols
    while i or j:
        if i and j and cost[i][j] == cost[i - 1][j - 1] + (ref[i - 1] != hyp[j - 1]):
            ops.append(("match" if ref[i - 1] == hyp[j - 1] else "sub", i - 1, j - 1))
            i, j = i - 1, j - 1
        elif i and cost[i][j] == cost[i - 1][j] + 1:
            ops.append(("del", i - 1, None))
            i -= 1
        else:
            ops.append(("ins", None, j - 1))
            j -= 1
    ops.reverse()
    return ops


def edit_counts(ops: Ops) -> dict[str, int]:
    kinds = Counter(op for op, _, _ in ops)
    return {"substitutions": kinds["sub"], "deletions": kinds["del"], "insertions": kinds["ins"]}


def _in_lexicon(word: str, lexicon: frozenset[str], language: str) -> bool:
    if word in lexicon:
        return True
    return language == "ar" and word[0] in ARABIC_PREFIXES and word[1:] in lexicon


def _contracted_not(words: list[str], i: int) -> bool:
    """The t of an n't contraction, split from its stem by punctuation to space."""
    return words[i] == "t" and i > 0 and words[i - 1].endswith("n")


def negation_words(words: list[str], language: str) -> list[str]:
    """The words a negation fixture is judged on: in English, the t of n't reads as not."""
    if language != "en":
        return words
    return ["not" if _contracted_not(words, i) else word for i, word in enumerate(words)]


def critical_positions(words: list[str], semantic_class: str, language: str) -> set[int]:
    """The positions in `words` that a fixture of `semantic_class` must keep."""
    if semantic_class == "repetition":
        marked: set[int] = set()
        for j, word in enumerate(words):
            for back in range(1, REPEAT_WINDOW + 1):
                if j >= back and words[j - back] == word:
                    marked.update((j - back, j))
        return marked
    if semantic_class == "negation":
        return {
            i for i, word in enumerate(words) if _in_lexicon(word, NEGATION[language], language)
        }
    if semantic_class == "numerals":
        return {
            i
            for i, word in enumerate(words)
            if any(ch.isdecimal() for ch in word)
            or _in_lexicon(word, NUMBER_WORDS[language], language)
        }
    raise QualityError(f"no critical words for semantic class {semantic_class!r}")


def _matched_ref(ops: Ops) -> set[int]:
    return {i for op, i, _ in ops if op == "match" and i is not None}


def _matched_hyp(ops: Ops) -> set[int]:
    return {j for op, _, j in ops if op == "match" and j is not None}


def lexical_regressions(
    ref: list[str],
    streaming: tuple[list[str], Ops],
    batch: tuple[list[str], Ops],
    semantic_class: str,
    language: str,
) -> list[dict[str, Any]]:
    """Critical words the streaming arm lost or added beyond what the batch arm did."""
    if semantic_class == "negation":
        ref = negation_words(ref, language)
        hyps = [negation_words(arm[0], language) for arm in (streaming, batch)]
        streaming, batch = ((hyp, align(ref, hyp)) for hyp in hyps)
    kept_streaming, kept_batch = _matched_ref(streaming[1]), _matched_ref(batch[1])
    found: list[dict[str, Any]] = [
        {"class": semantic_class, "kind": "lost", "position": i, "words": [ref[i]]}
        for i in sorted(critical_positions(ref, semantic_class, language))
        if i in kept_batch and i not in kept_streaming
    ]

    def unmatched(arm: tuple[list[str], Ops]) -> Counter[str]:
        hyp, ops = arm
        kept = _matched_hyp(ops)
        critical = critical_positions(hyp, semantic_class, language)
        return Counter(hyp[j] for j in critical if j not in kept)

    added = unmatched(streaming) - unmatched(batch)
    found.extend(
        {"class": semantic_class, "kind": "added", "position": None, "words": [word] * added[word]}
        for word in sorted(added)
    )
    return found


def _runs(positions: list[int]) -> list[list[int]]:
    runs: list[list[int]] = []
    for position in positions:
        if runs and position == runs[-1][-1] + 1:
            runs[-1].append(position)
        else:
            runs.append([position])
    return runs


def _deleted(ops: Ops) -> set[int]:
    return {i for op, i, _ in ops if op == "del" and i is not None}


def _inserted(ops: Ops) -> dict[int, list[int]]:
    """Inserted hypothesis positions by place: the reference words aligned before them."""
    places: dict[int, list[int]] = {}
    consumed = 0
    for op, _, j in ops:
        if op == "ins" and j is not None:
            places.setdefault(consumed, []).append(j)
        else:
            consumed += 1
    return places


def seam_regressions(
    ref: list[str], streaming: tuple[list[str], Ops], batch: tuple[list[str], Ops]
) -> list[dict[str, Any]]:
    """Runs of SEAM_RUN or more words the streaming arm dropped or inserted where batch did not."""
    hyp, ops = streaming
    found: list[dict[str, Any]] = [
        {"class": "window_seam", "kind": "lost", "position": run[0], "words": [ref[i] for i in run]}
        for run in _runs(sorted(_deleted(ops) - _deleted(batch[1])))
        if len(run) >= SEAM_RUN
    ]
    batch_places = _inserted(batch[1])
    found.extend(
        {"class": "window_seam", "kind": "added", "position": place, "words": [hyp[j] for j in run]}
        for place, run in sorted(_inserted(ops).items())
        if len(run) >= SEAM_RUN and place not in batch_places
    )
    return found


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _percent(errors: int, words: int) -> float | None:
    return None if words == 0 else round(100 * errors / words, 3)


def _gate(
    minimums: list[tuple[int, int]],
    ratio_ok: bool | None,
    zero_ok: bool | None,
    complete: bool = True,
) -> str:
    """A zero-tolerance violation fails at any count; a ratio needs its minimum samples first.

    A gate judged against a batch arm that failed some of its files is never a pass: a
    failed batch file scores empty and shrinks a stratum's distance, and a fixture whose
    batch file failed is not judged.
    """
    if zero_ok is False:
        return "fail"
    if any(have < need for have, need in minimums):
        return "insufficient"
    if ratio_ok is False:
        return "fail"
    return "pass" if complete else "insufficient"


def load_references(document: Any) -> dict[str, str]:
    """A references file: an object from each reference text's SHA-256 to that text."""
    if not isinstance(document, dict):
        raise QualityError("references: not an object of digests to texts")
    for digest, text in document.items():
        if SHA256_HEX.fullmatch(digest) is None or not isinstance(text, str) or not text:
            raise QualityError(f"references: {digest!r} is not a digest with a text")
        _require_text(text, f"references: {digest}")
        if _sha256(text.encode("utf-8")) != digest:
            raise QualityError(f"references: {digest} is not the digest of its text")
    return document


def _arm_clips(document: Any, arm: str, manifest_sha256: str) -> dict[str, dict[str, Any]]:
    try:
        validate_record(document, json.loads(TRANSCRIPTS_SCHEMA_PATH.read_text(encoding="utf-8")))
    except RecordError as exc:
        raise QualityError(f"the {arm} transcripts do not match their schema: {exc}") from exc
    if document["arm"] != arm:
        raise QualityError(f"the {arm} transcripts are those of the {document['arm']} arm")
    if document["manifest_sha256"] != manifest_sha256:
        raise QualityError(f"the {arm} transcripts are of another corpus manifest")
    codes = load_error_codes()
    clips: dict[str, dict[str, Any]] = {}
    for clip in document["clips"]:
        if clip["id"] in clips:
            raise QualityError(f"{arm}: file {clip['id']!r} appears twice")
        if clip["outcome"] == "failed" and clip["error"] not in codes:
            raise QualityError(f"{arm}: {clip['id']}: {clip['error']!r} is not an error code")
        for final in clip.get("finals", []):
            _require_text(final, f"{arm}: {clip['id']}")
        clips[clip["id"]] = clip
    return clips


def _hypothesis(clip: dict[str, Any]) -> str:
    return " ".join(clip["finals"]) if clip["outcome"] == "completed" else ""


def _count_failure(totals: dict[str, Any], clip: dict[str, Any]) -> None:
    if clip["outcome"] == "failed":
        totals["failed_files"] += 1
        totals["failures"][clip["error"]] = totals["failures"].get(clip["error"], 0) + 1


def _reference(entry: dict[str, Any], references: dict[str, str]) -> str:
    transcript = entry["transcript"]
    text = transcript["text"]
    if text is None:
        text = references.get(transcript["sha256"])
        if text is None:
            raise QualityError(f"{entry['id']}: reference {transcript['sha256']} is not provided")
    findings = text_findings(text)
    if findings:
        raise QualityError(f"{entry['id']}: the reference carries a {min(findings)}")
    return text


class _Tally:
    """Running totals of one run, filled file by file."""

    def __init__(self) -> None:
        self.strata: dict[tuple[str, str], dict[str, Any]] = {
            key: {
                "files": 0,
                "words": 0,
                "raw_words": 0,
                "arms": {arm: self._arm() for arm in ARMS},
            }
            for key in CEILING_PERCENT
        }
        self.breakdowns: dict[tuple[str, str, str, str], dict[str, int]] = {}
        self.languages: dict[str, dict[str, Any]] = {
            language: {"recordings": set(), "references": set(), "words": 0}
            for language in LANGUAGES
        }
        self.silence: dict[str, Any] = {
            "recordings": set(),
            "unscored": set(),
            "files": 0,
            "codecs": {},
            "arms": {
                arm: {
                    "hallucinated_words": 0,
                    "files_with_words": 0,
                    "failed_files": 0,
                    "failures": {},
                }
                for arm in ARMS
            },
        }
        self.fixtures: dict[str, dict[str, Any]] = {
            language: {
                "recordings": set(),
                "classes": Counter(),
                "batch_failed": 0,
                "class_batch_failed": Counter(),
            }
            for language in LANGUAGES
        }
        self.regressions: list[dict[str, Any]] = []
        self.text = {arm: {name: 0 for name in TEXT_COUNTS.values()} for arm in ARMS}

    @staticmethod
    def _arm() -> dict[str, Any]:
        return {
            "errors": 0,
            "substitutions": 0,
            "deletions": 0,
            "insertions": 0,
            "raw_errors": 0,
            "failed_files": 0,
            "failures": {},
            "empty_files": 0,
        }

    def hypothesis_text(self, arm: str, text: str) -> None:
        for finding in text_findings(text):
            self.text[arm][TEXT_COUNTS[finding]] += 1

    def silence_file(self, entry: dict[str, Any], clean_id: str, clips: dict[str, Any]) -> None:
        self.silence["recordings"].add(clean_id)
        if clips["streaming"]["outcome"] == "failed":
            self.silence["unscored"].add(clean_id)
        self.silence["files"] += 1
        codec = self.silence["codecs"].setdefault(
            entry["codec"], {arm: {"files": 0, "hallucinated_words": 0} for arm in ARMS}
        )
        for arm in ARMS:
            clip = clips[arm]
            # A silence clip has no language: count with whichever normalizer leaves more.
            words = max(len(normalize(_hypothesis(clip), language)) for language in LANGUAGES)
            totals = self.silence["arms"][arm]
            totals["hallucinated_words"] += words
            totals["files_with_words"] += words > 0
            _count_failure(totals, clip)
            codec[arm]["files"] += 1
            codec[arm]["hallucinated_words"] += words

    def speech_file(
        self, entry: dict[str, Any], clean: dict[str, Any], text: str, clips: dict[str, Any]
    ) -> None:
        language = entry["language"]
        ref, raw_ref = normalize(text, language), text.split()
        if not ref:
            raise QualityError(f"{entry['id']}: the reference has no words once normalized")
        stratum = self.strata[(language, entry["codec"])]
        stratum["files"] += 1
        stratum["words"] += len(ref)
        stratum["raw_words"] += len(raw_ref)
        if entry["derived_from"] is None:
            totals = self.languages[language]
            totals["recordings"].add(entry["id"])
            totals["references"].add(entry["transcript"]["sha256"])
            totals["words"] += len(ref)
        groups = [("source", clean["source"]["source"]), ("duration_bin", entry["duration_bin"])]
        if entry["codec"] == "noisy16k":
            groups.append(("snr_db", str(entry["recipe"]["snr_db"])))
        for by, value in groups:
            group = self.breakdowns.setdefault(
                (language, entry["codec"], by, value),
                {"files": 0, "words": 0, "streaming": 0, "batch": 0},
            )
            group["files"] += 1
            group["words"] += len(ref)
        aligned: dict[str, tuple[list[str], Ops]] = {}
        for arm in ARMS:
            clip = clips[arm]
            hyp_text = _hypothesis(clip)
            hyp = normalize(hyp_text, language)
            ops = align(ref, hyp)
            counts = edit_counts(ops)
            totals = stratum["arms"][arm]
            for name, value in counts.items():
                totals[name] += value
            totals["errors"] += sum(counts.values())
            totals["raw_errors"] += sum(edit_counts(align(raw_ref, hyp_text.split())).values())
            _count_failure(totals, clip)
            totals["empty_files"] += clip["outcome"] == "completed" and not hyp
            for by, value in groups:
                self.breakdowns[(language, entry["codec"], by, value)][arm] += sum(counts.values())
            aligned[arm] = (hyp, ops)
        if entry["semantic"]:
            self.semantic_file(
                entry, clean["id"], ref, aligned, clips["batch"]["outcome"] == "failed"
            )

    def semantic_file(
        self,
        entry: dict[str, Any],
        clean_id: str,
        ref: list[str],
        aligned: dict[str, tuple[list[str], Ops]],
        batch_failed: bool,
    ) -> None:
        language = entry["language"]
        fixtures = self.fixtures[language]
        fixtures["recordings"].add(clean_id)
        fixtures["batch_failed"] += batch_failed
        # A reference held by digest is not republished through its words.
        public = entry["transcript"]["text"] is not None
        if entry["derived_from"] is None:
            fixtures["classes"].update(entry["semantic"])
        # An empty batch prediction would make every critical word streaming decodes read as added.
        if batch_failed:
            fixtures["class_batch_failed"].update(entry["semantic"])
            return
        for semantic_class in entry["semantic"]:
            if semantic_class == "window_seam":
                found = seam_regressions(ref, aligned["streaming"], aligned["batch"])
            else:
                found = lexical_regressions(
                    ref, aligned["streaming"], aligned["batch"], semantic_class, language
                )
            self.regressions.extend(
                {
                    "file": entry["id"],
                    "language": language,
                    "codec": entry["codec"],
                    "class": item["class"],
                    "kind": item["kind"],
                    "position": item["position"],
                    "count": len(item["words"]),
                    "words": item["words"] if public else None,
                }
                for item in found
            )


def _arm_record(totals: dict[str, Any], words: int, raw_words: int) -> dict[str, Any]:
    return {
        **totals,
        "failures": dict(sorted(totals["failures"].items())),
        "wer_percent": _percent(totals["errors"], words),
        "raw_wer_percent": _percent(totals["raw_errors"], raw_words),
    }


def _strata_record(tally: _Tally) -> list[dict[str, Any]]:
    records = []
    for (language, codec), stratum in tally.strata.items():
        words, files = stratum["words"], stratum["files"]
        stream_errors = stratum["arms"]["streaming"]["errors"]
        batch_errors = stratum["arms"]["batch"]["errors"]
        ceiling = CEILING_PERCENT[(language, codec)]
        ratio_ok = (
            100 * stream_errors <= ceiling * words
            and 100 * (stream_errors - batch_errors) <= BATCH_DELTA_PP * words
        )
        records.append(
            {
                "language": language,
                "codec": codec,
                "utterances": files,
                "minimum_utterances": MIN_STRATUM_UTTERANCES,
                "reference_words": words,
                "raw_reference_words": stratum["raw_words"],
                "ceiling_percent": ceiling,
                "batch_delta_limit_pp": BATCH_DELTA_PP,
                "streaming": _arm_record(stratum["arms"]["streaming"], words, stratum["raw_words"]),
                "batch": _arm_record(stratum["arms"]["batch"], words, stratum["raw_words"]),
                "delta_pp": _percent(stream_errors - batch_errors, words),
                "verdict": _gate(
                    [(files, MIN_STRATUM_UTTERANCES)],
                    ratio_ok,
                    None,
                    stratum["arms"]["batch"]["failed_files"] == 0,
                ),
            }
        )
    return records


def score(
    manifest: Any,
    manifest_sha256: str,
    streaming: Any,
    batch: Any,
    references: dict[str, str] | None,
    *,
    input_digests: dict[str, str | None],
    repo_root: Path = corpus.REPO_ROOT,
) -> dict[str, Any]:
    """The quality record of one run; refuses arms that do not cover the same pool."""
    corpus.check_manifest(manifest, repo_root=repo_root)
    arms = {
        "streaming": _arm_clips(streaming, "streaming", manifest_sha256),
        "batch": _arm_clips(batch, "batch", manifest_sha256),
    }
    pool = streaming["pool"]
    if batch["pool"] != pool:
        raise QualityError(f"the streaming arm ran the {pool} pool, the batch arm {batch['pool']}")
    files = [entry for entry in manifest["files"] if entry["pool"] == pool]
    expected = {entry["id"] for entry in files}
    if not expected:
        raise QualityError(f"the manifest has no {pool} files")
    for arm, clips in arms.items():
        missing, extra = sorted(expected - clips.keys()), sorted(clips.keys() - expected)
        if missing or extra:
            raise QualityError(
                f"{arm}: the transcribed files are not the {pool} pool's "
                f"(missing {missing[:3]}, outside the pool {extra[:3]})"
            )
    by_id = {entry["id"]: entry for entry in manifest["files"]}
    tally = _Tally()
    for arm, clips in arms.items():
        for clip in clips.values():
            tally.hypothesis_text(arm, _hypothesis(clip))
    for entry in files:
        clean = entry if entry["derived_from"] is None else by_id[entry["derived_from"]]
        clips = {arm: arms[arm][entry["id"]] for arm in ARMS}
        if entry["suite"] == "silence":
            tally.silence_file(entry, clean["id"], clips)
        else:
            tally.speech_file(entry, clean, _reference(entry, references or {}), clips)

    strata = _strata_record(tally)
    languages = [
        {
            "language": language,
            "recordings": len(totals["recordings"]),
            "distinct_references": len(totals["references"]),
            "reference_words": totals["words"],
            "minimum_recordings": MIN_LANGUAGE_RECORDINGS,
            "minimum_words": MIN_LANGUAGE_WORDS,
            "verdict": _gate(
                [
                    (len(totals["recordings"]), MIN_LANGUAGE_RECORDINGS),
                    (totals["words"], MIN_LANGUAGE_WORDS),
                ],
                None,
                None,
            ),
        }
        for language, totals in tally.languages.items()
    ]
    silence_arms = {
        arm: {**totals, "failures": dict(sorted(totals["failures"].items()))}
        for arm, totals in tally.silence["arms"].items()
    }
    scored = tally.silence["recordings"] - tally.silence["unscored"]
    silence = {
        "recordings": len(tally.silence["recordings"]),
        "scored_recordings": len(scored),
        "files": tally.silence["files"],
        "minimum_recordings": MIN_SILENCE_RECORDINGS,
        **silence_arms,
        "by_codec": [
            {"codec": codec, **counts} for codec, counts in sorted(tally.silence["codecs"].items())
        ],
        "verdict": _gate(
            [(len(scored), MIN_SILENCE_RECORDINGS)],
            None,
            silence_arms["streaming"]["hallucinated_words"] == 0,
        ),
    }
    semantic = []
    for language, fixtures in tally.fixtures.items():
        found = Counter(item["class"] for item in tally.regressions if item["language"] == language)
        class_minimums = [
            (fixtures["classes"][name], MIN_SEMANTIC_CLASS_FIXTURES) for name in SEMANTIC_CLASSES
        ]
        semantic.append(
            {
                "language": language,
                "fixtures": len(fixtures["recordings"]),
                "minimum_fixtures": MIN_SEMANTIC_FIXTURES,
                "minimum_class_fixtures": MIN_SEMANTIC_CLASS_FIXTURES,
                "by_class": {
                    name: {
                        "fixtures": fixtures["classes"][name],
                        "batch_failed_files": fixtures["class_batch_failed"][name],
                        "regressions": found[name],
                        "verdict": _gate(
                            [(fixtures["classes"][name], MIN_SEMANTIC_CLASS_FIXTURES)],
                            None,
                            found[name] == 0,
                            fixtures["class_batch_failed"][name] == 0,
                        ),
                    }
                    for name in SEMANTIC_CLASSES
                },
                "batch_failed_files": fixtures["batch_failed"],
                "regressions": sum(found.values()),
                "verdict": _gate(
                    [(len(fixtures["recordings"]), MIN_SEMANTIC_FIXTURES), *class_minimums],
                    None,
                    not found,
                    fixtures["batch_failed"] == 0,
                ),
            }
        )
    verdicts = {item["verdict"] for item in strata + languages + semantic} | {silence["verdict"]}
    verdict = (
        "fail" if "fail" in verdicts else "insufficient" if "insufficient" in verdicts else "pass"
    )
    return {
        "schema_version": "0.1",
        "scorer": {
            "path": SCORER,
            "sha256": _sha256((repo_root / SCORER).read_bytes()),
            "unicode_version": unicodedata.unidata_version,
        },
        "normalizers": dict(NORMALIZERS),
        "corpus": {
            "name": manifest["corpus"]["name"],
            "version": manifest["corpus"]["version"],
            "manifest_sha256": manifest_sha256,
        },
        "pool": pool,
        "inputs": {
            "streaming_sha256": input_digests["streaming"],
            "batch_sha256": input_digests["batch"],
            "references_sha256": input_digests.get("references"),
        },
        "verdict": verdict,
        "strata": strata,
        "breakdowns": [
            {
                "language": language,
                "codec": codec,
                "by": by,
                "value": value,
                "utterances": group["files"],
                "reference_words": group["words"],
                "streaming_wer_percent": _percent(group["streaming"], group["words"]),
                "batch_wer_percent": _percent(group["batch"], group["words"]),
            }
            for (language, codec, by, value), group in sorted(tally.breakdowns.items())
        ],
        "languages": languages,
        "silence": silence,
        "semantic": {"languages": semantic, "regressions": tally.regressions},
        "hypothesis_text": tally.text,
    }


def unmet(record: dict[str, Any]) -> list[str]:
    """The gates of a record that did not pass, as `<gate>:<verdict>`."""
    found = [
        f"wer {item['language']}/{item['codec']}:{item['verdict']}"
        for item in record["strata"]
        if item["verdict"] != "pass"
    ]
    found += [
        f"corpus {item['language']}:{item['verdict']}"
        for item in record["languages"]
        if item["verdict"] != "pass"
    ]
    if record["silence"]["verdict"] != "pass":
        found.append(f"silence:{record['silence']['verdict']}")
    found += [
        f"semantic {item['language']}:{item['verdict']}"
        for item in record["semantic"]["languages"]
        if item["verdict"] != "pass"
    ]
    return found


def _read_json(path: Path) -> tuple[Any, str]:
    data = path.read_bytes()
    return json.loads(data.decode("utf-8")), _sha256(data)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("score", help="score a run's two arms and write its quality record")
    run.add_argument("manifest", type=Path)
    run.add_argument(
        "--streaming", type=Path, required=True, help="the streaming arm's transcripts"
    )
    run.add_argument("--batch", type=Path, required=True, help="the batch arm's transcripts")
    run.add_argument("--references", type=Path, help="the texts of references held by digest")
    run.add_argument("--output", type=Path, help="write the record here instead of to stdout")
    args = parser.parse_args(argv)
    try:
        manifest, manifest_sha256 = _read_json(args.manifest)
        streaming, streaming_sha256 = _read_json(args.streaming)
        batch, batch_sha256 = _read_json(args.batch)
        references, references_sha256 = (
            _read_json(args.references) if args.references else (None, None)
        )
    except (OSError, ValueError) as exc:
        print(f"{TOOL_NAME}: unreadable: {exc}", file=sys.stderr)
        return EXIT_UNREADABLE
    try:
        record = score(
            manifest,
            manifest_sha256,
            streaming,
            batch,
            None if references is None else load_references(references),
            input_digests={
                "streaming": streaming_sha256,
                "batch": batch_sha256,
                "references": references_sha256,
            },
        )
        data = (json.dumps(record, indent=2, ensure_ascii=False) + "\n").encode("utf-8")
    except (QualityError, corpus.ManifestError, UnicodeError) as exc:
        print(f"{TOOL_NAME}: refused: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    if args.output is None:
        sys.stdout.buffer.write(data)
        sys.stdout.flush()
    else:
        args.output.write_bytes(data)
    gates = unmet(record)
    print(
        f"{TOOL_NAME}: {record['verdict']}" + (f": {', '.join(gates)}" if gates else ""),
        file=sys.stderr,
    )
    return EXIT_PASS if record["verdict"] == "pass" else EXIT_GATE


if __name__ == "__main__":
    sys.exit(main())
