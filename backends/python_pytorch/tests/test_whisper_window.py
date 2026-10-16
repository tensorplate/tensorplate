"""An stt_decode job's single pass against a recording of it on a real Whisper model.

``fixtures/whisper_window/record.py`` recorded each seam of the pass over a real
clip: the decode's prompt, options and tokens, the alignment's items before and
after punctuation is attached, and the job's result. The faked engine replays
it on every run. With faster-whisper installed, the wheel's own helpers are held
to it; with a model directory and the clip's PCM named, so is the real pass.
"""

from __future__ import annotations

import copy
import hashlib
import importlib
import inspect
import json
import os
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from tensorplate_pytorch_backend.backends.whisper_window import (
    APPEND_PUNCTUATIONS,
    PREPEND_PUNCTUATIONS,
    open_whisper_window,
    token_budget,
)
from tensorplate_pytorch_backend.job_objects import TranscriptResult
from test_faster_whisper_backend import _Engine, _job, _loaded
from test_faster_whisper_backend import engine as engine  # the fixture

_RECORD: dict[str, Any] = json.loads(
    (Path(__file__).parent / "fixtures" / "whisper_window" / "jfk_turbo_int8_cpu.json").read_text(
        encoding="utf-8"
    )
)
_MANIFEST = Path(__file__).resolve().parents[3] / "packaging" / "provisioning" / "manifest.json"


def _optional(name: str) -> Any:
    try:
        return importlib.import_module(name)
    except Exception:
        return None


# Imported before any case fakes them; None where they are not installed.
_NUMPY = _optional("numpy")
_WHEEL = _optional("faster_whisper")
_WHEEL_AUDIO = _optional("faster_whisper.audio")
_WHEEL_TOKENIZER = _optional("faster_whisper.tokenizer")
_WHEEL_TRANSCRIBE = _optional("faster_whisper.transcribe")
_MODEL_DIR = os.environ.get("TENSORPLATE_TEST_WHISPER_MODEL_DIR")
_CLIP_PCM = os.environ.get("TENSORPLATE_TEST_WHISPER_CLIP_PCM")

_WHEEL_MISSING = any(
    module is None for module in (_WHEEL, _WHEEL_AUDIO, _WHEEL_TOKENIZER, _WHEEL_TRANSCRIBE)
)
needs_wheel = pytest.mark.skipif(_WHEEL_MISSING, reason="needs faster-whisper")
needs_model = pytest.mark.skipif(
    _WHEEL_MISSING or _NUMPY is None or not (_MODEL_DIR and _CLIP_PCM),
    reason="needs faster-whisper, NumPy, TENSORPLATE_TEST_WHISPER_MODEL_DIR and "
    "TENSORPLATE_TEST_WHISPER_CLIP_PCM",
)


def _as_record(result: TranscriptResult) -> dict[str, Any]:
    words = [
        [word.text, word.token_begin, word.token_end, word.start_sample, word.end_sample]
        for word in result.words
    ]
    return {"text": result.text, "tokens": list(result.tokens), "words": words}


def test_the_record_was_made_with_the_provisioned_candidate_model() -> None:
    manifest = json.loads(_MANIFEST.read_text(encoding="utf-8"))
    (bundle,) = (item for item in manifest["bundles"] if item["name"] == "stt-whisper-candidate")
    provisioned = {
        item["path"].removeprefix("model/"): item["sha256"]
        for item in bundle["files"]
        if item["path"].startswith("model/")
    }
    assert _RECORD["model"]["files_sha256"] == provisioned


def test_the_record_replays_through_the_faked_engine_to_the_recorded_result(
    engine: _Engine, tmp_path: Path
) -> None:
    generate, job = _RECORD["generate"], _RECORD["job"]
    engine.hypothesis = list(generate["tokens"])
    engine.alignment = copy.deepcopy(_RECORD["alignment"])
    engine.merged = copy.deepcopy(_RECORD["merged"])
    engine.decoded = {tuple(generate["tokens"]): _RECORD["result"]["text"]}
    backend = _loaded(tmp_path)
    request = _job(job["language"], [0] * job["samples"], start_sample=job["start_sample"])
    result = backend.run_job(request)
    _, (prompt, options) = engine.passes[1]
    assert prompt == generate["prompt"]
    # The fake's suppression list is its own.
    assert {**options, "suppress_tokens": None} == {**generate["options"], "suppress_tokens": None}
    align = _RECORD["align"]
    assert engine.passes[2] == ("align", (align["text_tokens"], align["num_frames"]))
    assert _as_record(result) == _RECORD["result"]


@needs_wheel
def test_the_runners_punctuation_is_what_transcribe_attaches() -> None:
    parameters = inspect.signature(_WHEEL.WhisperModel.transcribe).parameters
    wheel = (parameters["prepend_punctuations"].default, parameters["append_punctuations"].default)
    assert wheel == (PREPEND_PUNCTUATIONS, APPEND_PUNCTUATIONS)


@needs_wheel
def test_the_wheel_attaches_the_recorded_alignments_punctuation_as_recorded() -> None:
    aligned = copy.deepcopy(_RECORD["alignment"])
    _WHEEL_TRANSCRIBE.merge_punctuations(aligned, PREPEND_PUNCTUATIONS, APPEND_PUNCTUATIONS)
    assert aligned == _RECORD["merged"]


@needs_wheel
def test_the_runners_calls_bind_to_the_wheels_signatures() -> None:
    model = _WHEEL.WhisperModel
    calls: list[tuple[Any, tuple[Any, ...], dict[str, Any]]] = [
        (_WHEEL_TOKENIZER.Tokenizer, (None, True), {"task": "transcribe", "language": "en"}),
        (model.get_prompt, (None, None, []), {"without_timestamps": True}),
        (model.encode, (None, None), {}),
        (model.find_alignment, (None, None, [[1]], None, 1), {}),
        (_WHEEL_AUDIO.pad_or_trim, (None,), {}),
        (_WHEEL_TRANSCRIBE.get_suppressed_tokens, (None, [-1]), {}),
        (_WHEEL_TRANSCRIBE.merge_punctuations, ([], "", ""), {}),
    ]
    for function, args, kwargs in calls:
        inspect.signature(function).bind(*args, **kwargs)


@pytest.fixture(scope="module")
def real() -> SimpleNamespace:
    """The model, the clip's PCM, the runner's pass over it, and the pass's encoding."""
    assert _MODEL_DIR is not None
    assert _CLIP_PCM is not None
    pcm = Path(_CLIP_PCM).read_bytes()
    assert hashlib.sha256(pcm).hexdigest() == _RECORD["clip"]["pcm_sha256"], "not the clip"
    recorded = _RECORD["model"]
    model = _WHEEL.WhisperModel(
        _MODEL_DIR,
        device=recorded["device"],
        compute_type=recorded["compute_type"],
        cpu_threads=recorded["cpu_threads"],
    )
    samples = _NUMPY.frombuffer(pcm, dtype="<i2").astype(_NUMPY.float32) / 32768.0
    features = model.feature_extractor(samples)
    frames = features.shape[-1] - 1
    tokenizer = _WHEEL_TOKENIZER.Tokenizer(
        model.hf_tokenizer, model.model.is_multilingual, task="transcribe", language="en"
    )
    return SimpleNamespace(
        model=model,
        pcm=pcm,
        window=open_whisper_window(model, _NUMPY),
        frames=frames,
        encoded=model.encode(_WHEEL_AUDIO.pad_or_trim(features[:, :frames])),
        tokenizer=tokenizer,
        prompt=model.get_prompt(tokenizer, [], without_timestamps=True),
    )


def _generate(real: SimpleNamespace, max_length: int) -> list[int]:
    options = {**_RECORD["generate"]["options"], "max_length": max_length}
    generated = real.model.model.generate(real.encoded, [real.prompt], **options)
    return list(generated[0].sequences_ids[0])


@needs_model
def test_the_real_pass_over_the_clip_returns_the_recorded_result(real: SimpleNamespace) -> None:
    omp = os.environ.get("OMP_NUM_THREADS")
    assert omp == _RECORD["model"]["omp_num_threads"], "word times move with OMP_NUM_THREADS"
    job = _RECORD["job"]
    result = real.window.decode(real.pcm, job["language"], job["start_sample"])
    assert _as_record(result) == _RECORD["result"]


@needs_model
@pytest.mark.parametrize("max_length", [4, 5, 6, 10, 21, 40])
def test_ctranslate2_returns_at_most_the_runners_token_budget(
    real: SimpleNamespace, max_length: int
) -> None:
    # The clip's transcript is longer than each budget, so the decode fills it; at 4
    # only the second term binds.
    assert len(_generate(real, max_length)) == token_budget(len(real.prompt), max_length)


@needs_model
def test_the_clip_decodes_to_the_same_tokens_at_448_and_888(real: SimpleNamespace) -> None:
    assert _generate(real, 448) == _generate(real, 888) == _RECORD["generate"]["tokens"]


@needs_model
@pytest.mark.parametrize("samples", [100, 300, 320, 480, 640])
def test_a_window_under_two_encoder_positions_is_not_decoded_on_the_real_model(
    real: SimpleNamespace, samples: int
) -> None:
    # A second into the clip, in a word: decoded, these windows gave no alignment
    # (100, 300) or times that are not finite (320, 480).
    window = real.pcm[32_000 : 32_000 + 2 * samples]
    result = real.window.decode(window, "en", 0)
    if samples < 640:
        assert result == TranscriptResult("", [], [])
    else:
        assert result.tokens


@needs_model
@pytest.mark.parametrize("count", [443, 444])
def test_ctranslate2_aligns_at_most_443_text_tokens_after_the_prompt(
    real: SimpleNamespace, count: int
) -> None:
    tokens = (_RECORD["generate"]["tokens"] * 20)[:count]

    def align() -> Any:
        sot = real.tokenizer.sot_sequence
        return real.model.model.align(real.encoded, sot, [tokens], [real.frames])

    if count == 443:
        assert len(align()[0].text_token_probs) == count
    else:
        with pytest.raises(RuntimeError, match="position"):
            align()
