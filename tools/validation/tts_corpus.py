#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The text-to-speech evaluation corpus: its check, its phoneme counts and its reference arm.

The corpus's shape is `config/schemas/tts_text_corpus.json`. `check` needs only
the standard library: it derives each sentence's stratum and critical classes
from its count and text, holds the tokenizer record to the pins of the speech
runtime's lock that the phonemizer runs on, and checks the listening set and
the queued sequences. With the source files it also compares every sourced
sentence with its source.

`count` and `reference` import kokoro and run inside the speech runtime's
locked environment. `count` re-phonemizes every sentence the way the Kokoro
runner does and compares the counts. `reference` synthesizes every sentence
with kokoro's own pipeline on the CPU in float32, the upstream reference the
listening review compares the candidate with, writes the clips outside the
repository and records their digests in `config/schemas/tts_reference_record.json`.
`g2p-pins`, also run there, derives the pins the phonemizer runs on from the installed
packages' metadata and compares them with G2P_PINS.

Exit status: 0 valid, 1 refused, 2 unreadable.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import io
import json
import math
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unicodedata
import wave
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
SCHEMA_PATH = REPO_ROOT / "config/schemas/tts_text_corpus.json"
RECORD_SCHEMA_PATH = REPO_ROOT / "config/schemas/tts_reference_record.json"
LOCK = "packaging/speech-runtime/lock/kokoro.txt"
# The lock files the phonemizer's pins live in.
G2P_LOCKS = (LOCK, "packaging/speech-runtime/lock/base.txt")
PROVISIONING = "packaging/provisioning/manifest.json"
KOKORO_BUNDLE = "tts-kokoro-candidate"
# The CMU ARCTIC license asks that its notice, conditions and disclaimer be retained.
ARCTIC_NOTICE = "tools/validation/corpus/cmu-arctic-COPYING.txt"

sys.path.insert(0, str(HERE))
from candidate_record import RecordError, validate_record  # noqa: E402

TOOL_NAME = "tts-corpus"

# Each source's license and identifier shape. A source outside this table is
# refused, so a sentence set without a license found cannot be added by data alone.
SOURCES: dict[str, tuple[str, str | None]] = {
    "cmu-arctic": ("CMU-ARCTIC", r"arctic_[ab][0-9]{4}"),
    "libritts-r": ("CC-BY-4.0", r"[0-9]+_[0-9]+_[0-9]+_[0-9]+"),
    "authored": ("CC0-1.0", None),
}
# kokoro, misaki, and the packages misaki.en, misaki.espeak and misaki.token import.
# torch, which kokoro's pipeline module imports, computes no part of the phonemes.
G2P_IMPORTS = (
    "addict",
    "espeakng-loader",
    "kokoro",
    "misaki",
    "num2words",
    "phonemizer-fork",
    "regex",
)
# spaCy tags the words and the model carries its weights; their runtime requirements, run
# to closure, decide the tags too.
G2P_MODEL = ("en-core-web-sm", "spacy")
# G2P_IMPORTS and the closure of G2P_MODEL's runtime requirements in the locked
# environment, as `tts_corpus.py g2p-pins` derives them there; the lock pins each in
# one of G2P_LOCKS.
G2P_PINS = (
    "addict",
    "annotated-doc",
    "annotated-types",
    "anyio",
    "blis",
    "catalogue",
    "certifi",
    "charset-normalizer",
    "click",
    "cloudpathlib",
    "confection",
    "cymem",
    "en-core-web-sm",
    "espeakng-loader",
    "h11",
    "httpcore",
    "httpx",
    "idna",
    "jinja2",
    "kokoro",
    "markdown-it-py",
    "markupsafe",
    "mdurl",
    "misaki",
    "murmurhash",
    "num2words",
    "numpy",
    "packaging",
    "phonemizer-fork",
    "preshed",
    "pydantic",
    "pydantic-core",
    "pygments",
    "regex",
    "requests",
    "rich",
    "setuptools",
    "shellingham",
    "smart-open",
    "spacy",
    "spacy-legacy",
    "spacy-loggers",
    "srsly",
    "thinc",
    "tqdm",
    "typer",
    "typing-extensions",
    "typing-inspection",
    "urllib3",
    "wasabi",
    "weasel",
    "wrapt",
)
# The tokenizer fields that must equal the lock's pins.
LOCKED = {
    "kokoro": "kokoro",
    "misaki": "misaki",
    "spacy": "spacy",
    "en_core_web_sm": "en-core-web-sm",
}

STRATA = (("t1to20", 1, 20), ("t21to100", 21, 100), ("t101to200", 101, 200))
MAX_TOKENS = 200
MAX_TEXT_BYTES = 4096
MIN_PER_STRATUM = 100
# The stratum edges and the segment bound; a long sentence has 181 tokens or more.
BOUNDARY_TOKENS = (20, 21, 100, 101, 200)
LONG_TOKENS = 181
MIN_LONG_OR_BOUNDARY = 20
MIN_CRITICAL = 10
MIN_LISTENING = 30
MIN_LISTENING_CRITICAL = 3
MIN_QUEUED = 100
SHORT, LONG = "t1to20", "t101to200"

NEGATIONS = frozenset(
    {"no", "not", "never", "nothing", "none", "nobody", "nowhere", "neither", "nor", "cannot"}
)
WORD = re.compile(r"[a-z]+(?:'[a-z]+)*")
SENTENCE_END = re.compile(r"[.!?][\"'\u2019\u201d]?\Z")

SAMPLE_RATE_HZ = 24000
RECORD_NAME = "reference-record.json"

EXIT_VALID = 0
EXIT_REFUSED = 1
EXIT_UNREADABLE = 2


class CorpusError(Exception):
    """The corpus, or what it is checked against, is not what it claims."""


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def load_schema(path: Path = SCHEMA_PATH) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def stratum(tokens: int) -> str:
    for name, low, high in STRATA:
        if low <= tokens <= high:
            return name
    raise CorpusError(f"{tokens} phoneme tokens is outside every stratum (1 to {MAX_TOKENS})")


def critical_classes(text: str) -> list[str]:
    """Negation by word, numeral by a decimal digit written in the text."""
    words = WORD.findall(text.casefold().replace("\u2019", "'"))
    classes = []
    if any(word in NEGATIONS or word.endswith("n't") for word in words):
        classes.append("negation")
    if any(ch.isdecimal() for ch in text):
        classes.append("numeral")
    return classes


def distinct_key(text: str) -> str:
    return " ".join(text.casefold().split())


def _lock_lines(repo_root: Path) -> dict[str, tuple[str, str]]:
    """Each pin of G2P_LOCKS: its version and its lock line with the whitespace collapsed."""
    pins: dict[str, tuple[str, str]] = {}
    for lock in G2P_LOCKS:
        for line in (repo_root / lock).read_text(encoding="utf-8").splitlines():
            match = re.match(r"([A-Za-z0-9_.-]+)==(\S+)", line)
            if match:
                if match.group(1) in pins:
                    raise CorpusError(f"{' and '.join(G2P_LOCKS)} pin {match.group(1)} twice")
                pins[match.group(1)] = (match.group(2), " ".join(line.split()))
    return pins


def lock_pins(repo_root: Path = REPO_ROOT) -> dict[str, str]:
    return {name: version for name, (version, _) in _lock_lines(repo_root).items()}


def g2p_pins_sha256(repo_root: Path = REPO_ROOT) -> str:
    """SHA-256 of the G2P_PINS lock lines, in that order, each ended by a newline."""
    lines = _lock_lines(repo_root)
    missing = [name for name in G2P_PINS if name not in lines]
    if missing:
        raise CorpusError(f"{' and '.join(G2P_LOCKS)} pin no {', '.join(missing)}")
    return _sha256("".join(lines[name][1] + "\n" for name in G2P_PINS).encode("utf-8"))


def _canonical(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def _requirement(line: str) -> tuple[str, bool]:
    """A requirement's distribution, and whether it applies here with no extra requested."""
    from packaging.requirements import Requirement

    requirement = Requirement(line)
    return requirement.name, requirement.marker is None or requirement.marker.evaluate(
        {"extra": ""}
    )


def derive_g2p_pins(locked: Iterable[str]) -> tuple[str, ...]:
    """G2P_IMPORTS and the installed runtime-requirement closure of G2P_MODEL, as locked."""
    closure: set[str] = set()
    todo = list(G2P_MODEL)
    while todo:
        name = _canonical(todo.pop())
        if name in closure:
            continue
        closure.add(name)
        for line in importlib.metadata.requires(name) or []:
            dependency, applies = _requirement(line)
            if applies:
                todo.append(dependency)
    pinned = {_canonical(name) for name in locked}
    return tuple(sorted((closure & pinned) | set(G2P_IMPORTS)))


def _unique(items: Iterable[dict[str, Any]], key: str, what: str) -> dict[str, dict[str, Any]]:
    seen: dict[str, dict[str, Any]] = {}
    for item in items:
        if item[key] in seen:
            raise CorpusError(f"{what} {key} {item[key]!r} appears twice")
        seen[item[key]] = item
    return seen


def _check_tokenizer(tokenizer: dict[str, Any], repo_root: Path) -> None:
    actual = g2p_pins_sha256(repo_root)
    if tokenizer["g2p_pins_sha256"] != actual:
        raise CorpusError(
            f"counted under g2p pins {tokenizer['g2p_pins_sha256']}, the G2P_PINS lines of"
            f" {' and '.join(G2P_LOCKS)} are {actual}; re-record the counts with"
            " `tts_corpus.py count CORPUS --model-config MODEL/config.json --write CORPUS`"
            " in the environment built from the lock (docs/validation/tts-text-corpus.md)"
        )
    pins = lock_pins(repo_root)
    for field, name in LOCKED.items():
        if tokenizer[field] != pins.get(name):
            raise CorpusError(f"tokenizer {field} {tokenizer[field]!r} is not the lock's {name}")


def _check_sources(sources: list[dict[str, Any]], repo_root: Path) -> dict[str, dict[str, Any]]:
    listed = _unique(sources, "id", "source")
    for name, source in listed.items():
        if source["license"] != SOURCES[name][0]:
            raise CorpusError(f"source {name}: license is {SOURCES[name][0]}")
        fetched = source["url"] is not None
        if fetched != (name != "authored") or fetched != (source["sha256"] is not None):
            raise CorpusError(f"source {name}: a fetched source has a url and digest, and only one")
    if "cmu-arctic" in listed:
        if not (repo_root / ARCTIC_NOTICE).is_file():
            raise CorpusError(f"source cmu-arctic: its license notice {ARCTIC_NOTICE} is missing")
        if ARCTIC_NOTICE not in listed["cmu-arctic"]["attribution"]:
            raise CorpusError(f"source cmu-arctic: the attribution does not name {ARCTIC_NOTICE}")
    if "libritts-r" in listed and listed["libritts-r"].get("text_form") != "original":
        raise CorpusError("source libritts-r: text_form is original, the transcript used")
    return listed


def _check_text(name: str, text: str) -> None:
    if text != text.strip() or unicodedata.normalize("NFC", text) != text:
        raise CorpusError(f"{name}: text is not stripped NFC")
    if any(unicodedata.category(ch) in ("Cc", "Cf") for ch in text):
        raise CorpusError(f"{name}: text holds a control or format character")
    # U+02BC is a letter, so "can't" written with it stays one word and hides its negation.
    if "\u02bc" in text:
        raise CorpusError(f"{name}: text holds U+02BC; write the apostrophe as U+0027 or U+2019")
    if len(text.encode("utf-8")) > MAX_TEXT_BYTES:
        raise CorpusError(f"{name}: text is over {MAX_TEXT_BYTES} UTF-8 bytes")
    # A heading, a bare number or a clause cut off mid-sentence is not a sentence.
    if SENTENCE_END.search(text) is None:
        raise CorpusError(f"{name}: text does not end in . ! or ?, with at most a closing quote")
    if not any(ch.islower() for ch in text):
        raise CorpusError(f"{name}: text has no lowercase letter")


def _check_sentences(
    sentences: list[dict[str, Any]], sources: dict[str, dict[str, Any]]
) -> dict[str, dict[str, Any]]:
    by_id = _unique(sentences, "id", "sentence")
    used: set[tuple[str, str]] = set()
    texts: dict[str, str] = {}
    for name, sentence in by_id.items():
        source = sentence["source"]
        if source not in sources:
            raise CorpusError(f"{name}: source {source!r} is not listed")
        shape = SOURCES[source][1]
        source_id = sentence["source_id"]
        if (source_id is None) != (shape is None) or (
            shape is not None and re.fullmatch(shape, source_id) is None
        ):
            raise CorpusError(f"{name}: {source_id!r} is not a {source} identifier")
        if source_id is not None:
            if (source, source_id) in used:
                raise CorpusError(f"{name}: {source} {source_id} is used twice")
            used.add((source, source_id))
        _check_text(name, sentence["text"])
        key = distinct_key(sentence["text"])
        if key in texts:
            raise CorpusError(f"{name}: the same text as {texts[key]}")
        texts[key] = name
        derived = stratum(sentence["phoneme_tokens"])
        if sentence["stratum"] != derived:
            raise CorpusError(f"{name}: {sentence['phoneme_tokens']} tokens is stratum {derived}")
        if sentence["critical"] != critical_classes(sentence["text"]):
            raise CorpusError(f"{name}: critical classes are {critical_classes(sentence['text'])}")
        # An edge count from espeak-ng would move with the distribution's espeak-ng.
        if sentence["phoneme_tokens"] in BOUNDARY_TOKENS and sentence["espeak_fallback"]:
            raise CorpusError(
                f"{name}: holds the edge count {sentence['phoneme_tokens']} with phonemes from"
                " the espeak fallback; an edge count is held by lexicon-only text"
            )
    return by_id


def _check_coverage(sentences: list[dict[str, Any]]) -> None:
    for name, _, _ in STRATA:
        count = sum(1 for s in sentences if s["stratum"] == name)
        if count < MIN_PER_STRATUM:
            raise CorpusError(f"stratum {name} holds {count} sentences, under {MIN_PER_STRATUM}")
    tokens = [s["phoneme_tokens"] for s in sentences]
    for edge in BOUNDARY_TOKENS:
        if edge not in tokens:
            raise CorpusError(f"no sentence has exactly {edge} phoneme tokens")
    long_or_boundary = sum(1 for t in tokens if t >= LONG_TOKENS or t in BOUNDARY_TOKENS)
    if long_or_boundary < MIN_LONG_OR_BOUNDARY:
        raise CorpusError(
            f"{long_or_boundary} long or boundary sentences, under {MIN_LONG_OR_BOUNDARY}"
        )
    for kind in ("negation", "numeral"):
        count = sum(1 for s in sentences if kind in s["critical"])
        if count < MIN_CRITICAL:
            raise CorpusError(f"{count} {kind} sentences, under {MIN_CRITICAL}")


def _check_listening(listening: list[str], by_id: dict[str, dict[str, Any]]) -> None:
    if len(set(listening)) != len(listening):
        raise CorpusError("the listening set names a sentence twice")
    missing = [name for name in listening if name not in by_id]
    if missing:
        raise CorpusError(f"the listening set names unlisted sentences {missing}")
    if len(listening) < MIN_LISTENING:
        raise CorpusError(f"the listening set holds {len(listening)}, under {MIN_LISTENING}")
    counts = {name: 0 for name, _, _ in STRATA}
    for name in listening:
        counts[by_id[name]["stratum"]] += 1
    if len(set(counts.values())) != 1:
        raise CorpusError(f"the listening set is not balanced across strata: {counts}")
    for kind in ("negation", "numeral"):
        count = sum(1 for name in listening if kind in by_id[name]["critical"])
        if count < MIN_LISTENING_CRITICAL:
            raise CorpusError(f"the listening set holds {count} {kind} sentences")


def _check_queued(sequences: list[dict[str, Any]], by_id: dict[str, dict[str, Any]]) -> None:
    _unique(sequences, "id", "queued sequence")
    seen: set[tuple[str, ...]] = set()
    firsts: dict[str, str] = {}
    lengths = set()
    for sequence in sequences:
        segments = tuple(sequence["segments"])
        name = sequence["id"]
        if len(segments) > 3:
            raise CorpusError(f"{name}: a queued sequence has two or three segments")
        missing = [s for s in segments if s not in by_id]
        if missing:
            raise CorpusError(f"{name}: names unlisted sentences {missing}")
        strata = [by_id[s]["stratum"] for s in segments]
        if strata[0] != LONG or any(s != SHORT for s in strata[1:]):
            raise CorpusError(f"{name}: a long first segment and short followers, not {strata}")
        if segments in seen:
            raise CorpusError(f"{name}: the same segments as an earlier sequence")
        seen.add(segments)
        if segments[0] in firsts:
            raise CorpusError(f"{name}: the same first segment as {firsts[segments[0]]}")
        firsts[segments[0]] = name
        lengths.add(len(segments))
    if len(sequences) < MIN_QUEUED:
        raise CorpusError(f"{len(sequences)} queued sequences, under {MIN_QUEUED}")
    if lengths != {2, 3}:
        raise CorpusError("queued sequences need both a single follower and two followers")


def arctic_prompts(data: bytes) -> dict[str, str]:
    prompts = {}
    for line in data.decode("utf-8").splitlines():
        match = re.fullmatch(r'\( (arctic_[ab][0-9]{4}) "(.*)" \)\s*', line)
        if match is None:
            raise CorpusError(f"cmu-arctic: an unreadable prompt line {line!r}")
        prompts[match.group(1)] = match.group(2)
    return prompts


def libritts_texts(archive: Path, wanted: set[str], form: str) -> dict[str, str]:
    """The `<id>.<form>.txt` transcripts of the wanted utterances in a LibriTTS-R archive."""
    texts = {}
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar:
            stem = member.name.rsplit("/", 1)[-1]
            utterance = stem.removesuffix(f".{form}.txt")
            if utterance in wanted:
                handle = tar.extractfile(member)
                assert handle is not None
                texts[utterance] = handle.read().decode("utf-8").strip()
    return texts


def _check_source_texts(
    sentences: list[dict[str, Any]], sources: dict[str, dict[str, Any]], files: dict[str, Path]
) -> None:
    for name in files:
        source = sources.get(name)
        if source is None or source["sha256"] is None:
            raise CorpusError(f"{name}: the corpus lists no fetched source by that name")
    missing = sorted(
        name for name, s in sources.items() if s["url"] is not None and name not in files
    )
    if missing:
        raise CorpusError(f"every fetched source is compared once one is; not given: {missing}")
    for name, path in files.items():
        source = sources[name]
        actual = _file_sha256(path)
        if actual != source["sha256"]:
            raise CorpusError(f"{name}: {path} is {actual}, the corpus pins {source['sha256']}")
        wanted = {s["source_id"] for s in sentences if s["source"] == name}
        if name == "cmu-arctic":
            texts = arctic_prompts(path.read_bytes())
        else:
            texts = libritts_texts(path, wanted, source["text_form"])
        for sentence in sentences:
            if sentence["source"] == name and texts.get(sentence["source_id"]) != sentence["text"]:
                raise CorpusError(
                    f"{sentence['id']}: text differs from {name} {sentence['source_id']}"
                )


def check_schema(corpus: Any) -> None:
    try:
        validate_record(corpus, load_schema())
    except RecordError as exc:
        raise CorpusError(f"the corpus does not match its schema: {exc}") from exc


def check_corpus(
    corpus: Any, *, source_files: dict[str, Path] | None = None, repo_root: Path = REPO_ROOT
) -> int:
    """Refuse a corpus that disagrees with what it derives; return its sentence count."""
    check_schema(corpus)
    _check_tokenizer(corpus["tokenizer"], repo_root)
    sources = _check_sources(corpus["sources"], repo_root)
    sentences = corpus["sentences"]
    by_id = _check_sentences(sentences, sources)
    _check_coverage(sentences)
    _check_listening(corpus["listening"], by_id)
    _check_queued(corpus["queued_sequences"], by_id)
    if source_files:
        _check_source_texts(sentences, sources, source_files)
    return len(sentences)


def summarize(corpus: dict[str, Any]) -> dict[str, Any]:
    sentences = corpus["sentences"]

    def per(key: Callable[[dict[str, Any]], Iterable[str]]) -> dict[str, int]:
        counts: dict[str, int] = {}
        for sentence in sentences:
            for value in key(sentence):
                counts[value] = counts.get(value, 0) + 1
        return dict(sorted(counts.items()))

    return {
        "sentences": len(sentences),
        "per_stratum": per(lambda s: [s["stratum"]]),
        "per_source": per(lambda s: [s["source"]]),
        "per_critical": per(lambda s: s["critical"]),
        "long_or_boundary": sum(
            1
            for s in sentences
            if s["phoneme_tokens"] >= LONG_TOKENS or s["phoneme_tokens"] in BOUNDARY_TOKENS
        ),
        "listening": len(corpus["listening"]),
        "queued_sequences": len(corpus["queued_sequences"]),
    }


def _require_locked(repo_root: Path, names: Iterable[str]) -> dict[str, str]:
    pins = lock_pins(repo_root)
    installed = {}
    for name in names:
        version = importlib.metadata.version(name)
        if version != pins.get(name):
            raise CorpusError(f"installed {name} {version} is not the lock's {pins.get(name)}")
        installed[name] = version
    return installed


def phonemizer(language: str) -> Callable[[str], tuple[str, bool]]:
    """The Kokoro runner's phonemizer: KPipeline's g2p, with its espeak fallback present.

    Each text is phonemized again with the fallback turned off, and the flag returned with
    its phonemes says whether the two differ: whether espeak-ng made any of them.
    """
    from kokoro import KPipeline

    g2p = KPipeline(lang_code=language, model=False, trf=False).g2p
    if getattr(g2p, "fallback", True) is None:
        raise CorpusError("the phonemizer's espeak fallback is unavailable")
    if not hasattr(g2p, "fallback"):
        raise CorpusError("the phonemizer has no espeak fallback to turn off")
    fallback = g2p.fallback

    def phonemes(text: str) -> tuple[str, bool]:
        result, _ = g2p(text)
        if not result.strip():
            raise CorpusError(f"{text!r} has no phonemes")
        g2p.fallback = None
        try:
            lexicon_only, _ = g2p(text)
        finally:
            g2p.fallback = fallback
        return result, lexicon_only != result

    return phonemes


def _require_espeak(corpus: dict[str, Any], installed: str) -> None:
    recorded = corpus["tokenizer"]["espeak_ng"]
    if installed != recorded:
        raise CorpusError(
            f"installed libespeak-ng1 {installed} is not the corpus's {recorded}; the counts"
            " hold only for the libespeak-ng1 they were made with"
        )


def _pinned_files(repo_root: Path) -> dict[str, str]:
    manifest = json.loads((repo_root / PROVISIONING).read_text(encoding="utf-8"))
    for bundle in manifest["bundles"]:
        if bundle["name"] == KOKORO_BUNDLE:
            return {f["path"]: f["sha256"] for f in bundle["files"]}
    raise CorpusError(f"{PROVISIONING} pins no {KOKORO_BUNDLE} bundle")


def _pinned(model_dir: Path, relative: str, pins: dict[str, str]) -> tuple[Path, str]:
    path = model_dir / relative
    actual = _file_sha256(path)
    pinned = pins.get("model/" + relative)
    if pinned != actual:
        raise CorpusError(f"{relative} is {actual}, {PROVISIONING} pins {pinned}")
    return path, actual


def count_corpus(
    corpus: dict[str, Any],
    model_config: Path,
    *,
    write: Path | None = None,
    repo_root: Path = REPO_ROOT,
    espeak_version: Callable[[], str] | None = None,
) -> list[str]:
    """Re-count every sentence and refuse a changed count, or write the counted corpus."""
    _require_locked(repo_root, G2P_PINS)
    espeak = (espeak_version or _dpkg_espeak_version)()
    if write is None:
        _require_espeak(corpus, espeak)
    path, _ = _pinned(model_config.parent, model_config.name, _pinned_files(repo_root))
    vocab = json.loads(path.read_text(encoding="utf-8"))["vocab"]
    phonemes = phonemizer(corpus["language"])
    mismatches = []
    for sentence in corpus["sentences"]:
        result, fallback = phonemes(sentence["text"])
        unknown = sorted({symbol for symbol in result if symbol not in vocab})
        if unknown:
            raise CorpusError(f"{sentence['id']}: the model vocabulary lacks {unknown}")
        tokens = len(result)
        if tokens != sentence["phoneme_tokens"]:
            mismatches.append(
                f"{sentence['id']}: {tokens} tokens, recorded {sentence['phoneme_tokens']}"
            )
        if fallback != sentence["espeak_fallback"]:
            mismatches.append(
                f"{sentence['id']}: espeak fallback {json.dumps(fallback)},"
                f" recorded {json.dumps(sentence['espeak_fallback'])}"
            )
        sentence["phoneme_tokens"] = tokens
        sentence["stratum"] = stratum(tokens)
        sentence["critical"] = critical_classes(sentence["text"])
        sentence["espeak_fallback"] = fallback
    if mismatches and write is None:
        raise CorpusError("; ".join(mismatches))
    if write is not None:
        pins = lock_pins(repo_root)
        corpus["tokenizer"].update(
            {field: pins[name] for field, name in LOCKED.items()},
            g2p_pins_sha256=g2p_pins_sha256(repo_root),
            espeak_ng=espeak,
        )
        check_corpus(corpus, repo_root=repo_root)
        write.write_text(json.dumps(corpus, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return mismatches


def _dpkg_espeak_version() -> str:
    return subprocess.run(
        ["dpkg-query", "-W", "-f=${Version}", "libespeak-ng1"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout


def reference_seed(version: str, sentence_id: str) -> int:
    digest = hashlib.sha256(f"{version}/reference/{sentence_id}".encode()).digest()
    return int.from_bytes(digest[:8], "big")


def pcm16(samples: list[float]) -> tuple[bytes, int]:
    """The Kokoro runner's conversion: clip to [-1, 1], scale by 2^15, round half to even."""
    out = bytearray()
    clipped = 0
    for sample in samples:
        if not math.isfinite(sample):
            raise CorpusError("the model returned nonfinite audio")
        if not -1.0 <= sample <= 1.0:
            clipped += 1
        value = min(32767, round(max(-1.0, min(1.0, sample)) * 32768.0))
        out += value.to_bytes(2, "little", signed=True)
    return bytes(out), clipped


def wav_bytes(pcm: bytes, rate: int) -> bytes:
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as writer:
        writer.setnchannels(1)
        writer.setsampwidth(2)
        writer.setframerate(rate)
        writer.writeframes(pcm)
    return buffer.getvalue()


def record_reference(
    corpus: dict[str, Any],
    corpus_sha256: str,
    model_dir: Path,
    out: Path,
    *,
    threads: int,
    repo_root: Path = REPO_ROOT,
    espeak_version: Callable[[], str] | None = None,
) -> dict[str, Any]:
    """Synthesize every sentence with kokoro's own pipeline on the CPU in float32."""
    resolved = out.resolve()
    if resolved == repo_root or repo_root in resolved.parents:
        raise CorpusError(f"{out}: reference audio is written outside the repository")
    if out.exists():
        raise CorpusError(f"{out}: the output directory already exists")
    installed = _require_locked(repo_root, G2P_PINS)
    espeak = (espeak_version or _dpkg_espeak_version)()
    _require_espeak(corpus, espeak)
    tokenizer = {
        **{field: installed[name] for field, name in LOCKED.items()},
        "g2p_pins_sha256": g2p_pins_sha256(repo_root),
        "espeak_ng": espeak,
    }
    pins = _pinned_files(repo_root)
    config_path, config_sha = _pinned(model_dir, "config.json", pins)
    weights_path, weights_sha = _pinned(model_dir, "kokoro-v1_0.pth", pins)
    voice_path, voice_sha = _pinned(model_dir, f"voices/{corpus['voice']}.pt", pins)
    import torch
    from kokoro import KModel, KPipeline

    torch.set_num_threads(threads)
    config = json.loads(config_path.read_text(encoding="utf-8"))
    model = KModel(config=config, model=str(weights_path)).float().to("cpu").eval()
    pipeline = KPipeline(lang_code=corpus["language"], model=model, trf=False)
    if getattr(pipeline.g2p, "fallback", True) is None:
        raise CorpusError("the phonemizer's espeak fallback is unavailable")
    out.parent.mkdir(parents=True, exist_ok=True)
    # Built beside the output and renamed into place, so a failed run leaves no directory behind.
    partial = Path(tempfile.mkdtemp(prefix=f".{out.name}.", suffix=".partial", dir=out.parent))
    try:
        clips = []
        for sentence in corpus["sentences"]:
            name = sentence["id"]
            runner_phonemes, _ = pipeline.g2p(sentence["text"])
            tokens = len(runner_phonemes)
            if tokens != sentence["phoneme_tokens"]:
                raise CorpusError(
                    f"{name}: {tokens} tokens here, the corpus counted {sentence['phoneme_tokens']}"
                )
            seed = reference_seed(corpus["corpus"]["version"], name)
            torch.manual_seed(seed)
            results = list(pipeline(sentence["text"], voice=str(voice_path), speed=1))
            if len(results) != 1:
                raise CorpusError(f"{name}: the pipeline returned {len(results)} chunks, not one")
            audio = results[0].audio
            if audio.dtype != torch.float32 or audio.ndim != 1 or audio.shape[0] == 0:
                raise CorpusError(f"{name}: the pipeline returned no one-dimensional float32 audio")
            pcm, clipped = pcm16(audio.detach().cpu().tolist())
            data = wav_bytes(pcm, SAMPLE_RATE_HZ)
            (partial / f"{name}.wav").write_bytes(data)
            clips.append(
                {
                    "id": name,
                    "text_sha256": _sha256(sentence["text"].encode("utf-8")),
                    "phonemes_sha256": _sha256(results[0].phonemes.encode("utf-8")),
                    "runner_phonemes": results[0].phonemes == runner_phonemes,
                    "seed": seed,
                    "samples": len(pcm) // 2,
                    "clipped_samples": clipped,
                    "wav_sha256": _sha256(data),
                    "pcm_sha256": _sha256(pcm),
                }
            )
        record = {
            "schema_version": "0.1",
            "corpus": {**corpus["corpus"], "sha256": corpus_sha256},
            "arm": "upstream-reference",
            "engine": {
                **tokenizer,
                "torch": str(torch.__version__),
                "device": "cpu",
                "dtype": "float32",
                "threads": int(torch.get_num_threads()),
            },
            "model": {
                "config_sha256": config_sha,
                "weights_sha256": weights_sha,
                "voice": corpus["voice"],
                "voice_sha256": voice_sha,
            },
            "language": corpus["language"],
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "clips": clips,
        }
        try:
            validate_record(record, load_schema(RECORD_SCHEMA_PATH))
        except RecordError as exc:
            raise CorpusError(f"the reference record does not match its schema: {exc}") from exc
        (partial / RECORD_NAME).write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    except BaseException:
        shutil.rmtree(partial, ignore_errors=True)
        raise
    partial.rename(out)
    return record


def _source_file(value: str) -> tuple[str, Path]:
    name, _, path = value.partition("=")
    if not path:
        raise argparse.ArgumentTypeError("expected <source>=<path>")
    return name, Path(path)


def _g2p_pins_command() -> int:
    try:
        derived = derive_g2p_pins(lock_pins(REPO_ROOT))
    except (CorpusError, OSError, ImportError) as exc:
        print(f"{TOOL_NAME}: refused: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    print("\n".join(derived))
    if derived != G2P_PINS:
        added = sorted(set(derived) - set(G2P_PINS))
        dropped = sorted(set(G2P_PINS) - set(derived))
        print(
            f"{TOOL_NAME}: refused: G2P_PINS is not the derived list; add {added}, drop {dropped}",
            file=sys.stderr,
        )
        return EXIT_REFUSED
    return EXIT_VALID


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check", help="verify a corpus and, with --source, its texts")
    check.add_argument("corpus", type=Path)
    check.add_argument(
        "--source",
        type=_source_file,
        action="append",
        default=[],
        help="<source>=<file>: cmuarctic.data, or the LibriTTS-R archive",
    )
    summary = commands.add_parser("summary", help="print a valid corpus's counts as JSON")
    summary.add_argument("corpus", type=Path)
    count = commands.add_parser("count", help="re-count every sentence with the locked phonemizer")
    count.add_argument("corpus", type=Path)
    count.add_argument("--model-config", type=Path, required=True)
    count.add_argument("--write", type=Path, help="write the corpus with the counts made here")
    reference = commands.add_parser("reference", help="record the CPU float32 reference arm")
    reference.add_argument("corpus", type=Path)
    reference.add_argument("--model-dir", type=Path, required=True)
    reference.add_argument("--out", type=Path, required=True)
    reference.add_argument("--threads", type=int, default=4)
    commands.add_parser(
        "g2p-pins", help="derive the phonemizer's pins in the locked environment, and compare"
    )
    args = parser.parse_args(argv)
    if args.command == "g2p-pins":
        return _g2p_pins_command()
    try:
        raw = args.corpus.read_bytes()
        corpus = json.loads(raw)
    except (OSError, ValueError) as exc:
        print(f"{TOOL_NAME}: unreadable: {args.corpus}: {exc}", file=sys.stderr)
        return EXIT_UNREADABLE
    try:
        if args.command == "count":
            check_schema(corpus)
            count_corpus(corpus, args.model_config, write=args.write, repo_root=REPO_ROOT)
            print(f"{TOOL_NAME}: counted: {len(corpus['sentences'])} sentences")
            return EXIT_VALID
        count_total = check_corpus(
            corpus, source_files=dict(getattr(args, "source", [])), repo_root=REPO_ROOT
        )
        if args.command == "reference":
            record = record_reference(
                corpus,
                _sha256(raw),
                args.model_dir,
                args.out,
                threads=args.threads,
                repo_root=REPO_ROOT,
            )
            print(f"{TOOL_NAME}: recorded: {len(record['clips'])} clips in {args.out}")
        elif args.command == "summary":
            print(json.dumps(summarize(corpus), indent=2))
        else:
            checked = "texts compared with their sources" if args.source else "sources not read"
            print(f"{TOOL_NAME}: valid: {count_total} sentences, {checked}")
    except (CorpusError, RecordError, OSError) as exc:
        print(f"{TOOL_NAME}: refused: {exc}", file=sys.stderr)
        return EXIT_REFUSED
    return EXIT_VALID


if __name__ == "__main__":
    sys.exit(main())
