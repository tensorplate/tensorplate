"""One ``stt_decode`` job as a single pass over one Whisper input window.

faster-whisper's ``transcribe`` loops: a window whose text does not end on a
timestamp is decoded again from there, so one clip can cost several encoder
passes. A job instead runs the wheel's pieces once each: the encoder over the
window's mel frames padded to the model's input length, one greedy decode
without timestamps, and the word alignment over the same encoder output.
"""

from __future__ import annotations

import math
import numbers
from collections.abc import Callable
from typing import Any, Final

from tensorplate_pytorch_backend.backends.base import BackendError
from tensorplate_pytorch_backend.job_objects import TranscriptResult, WordUnit
from tensorplate_pytorch_backend.protocol import (
    ERR_INFERENCE_FAILED,
    ERR_LOAD_FAILED,
    LIMIT_JOB_INPUT_SAMPLE_RATE_HZ,
)

#: CTranslate2 4.8.2 decodes at most min(max_length // 2, max_length - len(prompt) + 1)
#: tokens (``whisper.cc``); 888 gives 444, all the decoder's 448 positions hold after
#: a 4-token prompt.
MAX_LENGTH: Final[int] = 888

#: The punctuation ``transcribe`` attaches to the following and the preceding word.
PREPEND_PUNCTUATIONS: Final[str] = "\"'“¿([{-"
APPEND_PUNCTUATIONS: Final[str] = "\"'.。,，!！?？:：”)]}、"  # noqa: RUF001

#: CTranslate2 aligns over num_frames / 2 encoder positions (``whisper.cc``); with fewer
#: than two it returns no alignment or times that are not finite.
_ALIGNED_POSITIONS: Final[int] = 2

_PCM_FULL_SCALE: Final[float] = 32768.0


def token_budget(prompt_length: int, max_length: int = MAX_LENGTH) -> int:
    """How many tokens one decode can return; a hypothesis that fills it never ended."""
    return min(max_length // 2, max_length - prompt_length + 1)


class WhisperWindow:
    """Decodes one window of 16 kHz PCM into text, tokens and aligned word units."""

    def __init__(
        self,
        model: Any,  # noqa: ANN401 -- faster-whisper's WhisperModel
        numpy: Any,  # noqa: ANN401
        *,
        pad_or_trim: Callable[[Any], Any],
        tokenizer: Callable[..., Any],
        suppressed_tokens: Callable[[Any, list[int]], list[int]],
        merge_punctuations: Callable[[list[dict[str, Any]], str, str], None],
    ) -> None:
        self._model = model
        self._numpy = numpy
        self._pad_or_trim = pad_or_trim
        self._tokenizer = tokenizer
        self._suppressed_tokens = suppressed_tokens
        self._merge_punctuations = merge_punctuations

    def decode(self, pcm: bytes, language: str, start_sample: int) -> TranscriptResult:
        """Transcribe ``pcm``, whose first sample is ``start_sample`` on the session's timeline.

        The engine's own exceptions propagate; the caller types them.
        """
        model, numpy = self._model, self._numpy
        # A mel frame per hop, and an encoder position per input_stride frames.
        position = model.feature_extractor.hop_length * model.input_stride
        if len(pcm) // 2 < _ALIGNED_POSITIONS * position:
            return TranscriptResult("", [], [])
        samples = numpy.frombuffer(pcm, dtype="<i2").astype(numpy.float32) / _PCM_FULL_SCALE
        features = model.feature_extractor(samples)
        # The extractor pads the audio, so its last frame is not content.
        frames = features.shape[-1] - 1
        encoded = model.encode(self._pad_or_trim(features[:, :frames]))
        tokenizer = self._tokenizer(
            model.hf_tokenizer,
            model.model.is_multilingual,
            task="transcribe",
            language=language,
        )
        prompt = model.get_prompt(tokenizer, [], without_timestamps=True)
        generated = model.model.generate(
            encoded,
            [prompt],
            beam_size=1,
            patience=1,
            length_penalty=1,
            repetition_penalty=1,
            no_repeat_ngram_size=0,
            max_length=MAX_LENGTH,
            suppress_blank=True,
            suppress_tokens=self._suppressed_tokens(tokenizer, [-1]),
        )[0]
        decoded = [int(token) for token in generated.sequences_ids[0]]
        if len(decoded) >= token_budget(len(prompt)):
            raise BackendError(ERR_INFERENCE_FAILED, "the transcript reached the job's token limit")
        # Without timestamp rules the decoder may still emit a special token; transcribe
        # aligns only the text tokens too.
        tokens = [token for token in decoded if token < tokenizer.eot]
        text = str(tokenizer.decode(tokens))
        aligned = []
        if tokens:
            aligned = model.find_alignment(tokenizer, [tokens], encoded, frames)[0]
        self._merge_punctuations(aligned, PREPEND_PUNCTUATIONS, APPEND_PUNCTUATIONS)
        end_sample = start_sample + len(pcm) // 2
        return TranscriptResult(
            text, tokens, _words(aligned, len(tokens), start_sample, end_sample)
        )


def _words(
    aligned: list[dict[str, Any]], token_count: int, start_sample: int, end_sample: int
) -> list[WordUnit]:
    words = []
    begin = 0
    for item in aligned:
        end = begin + len(item["tokens"])
        # Punctuation attached to a neighbour leaves an empty item in its place.
        if item["word"]:
            words.append(
                WordUnit(
                    str(item["word"]),
                    begin,
                    end,
                    _sample(item["start"], start_sample, end_sample),
                    _sample(item["end"], start_sample, end_sample),
                )
            )
        begin = end
    if begin != token_count:
        raise BackendError(
            ERR_INFERENCE_FAILED, "the word alignment does not cover the transcript's tokens"
        )
    return words


def _sample(seconds: object, start_sample: int, end_sample: int) -> int:
    if (
        not isinstance(seconds, numbers.Real)
        or isinstance(seconds, bool)
        or not math.isfinite(seconds)
        or seconds < 0
    ):
        raise BackendError(
            ERR_INFERENCE_FAILED, "the model returned a time that is not a non-negative number"
        )
    # The window is zero-padded, so a time may run past its audio.
    return min(start_sample + round(float(seconds) * LIMIT_JOB_INPUT_SAMPLE_RATE_HZ), end_sample)


def open_whisper_window(model: Any, numpy: Any) -> WhisperWindow:  # noqa: ANN401
    """The single-pass decoder over ``model``, with the wheel's padding, tokenizer and alignment."""
    try:
        from faster_whisper import audio, tokenizer, transcribe

        return WhisperWindow(
            model,
            numpy,
            pad_or_trim=audio.pad_or_trim,
            tokenizer=tokenizer.Tokenizer,
            suppressed_tokens=transcribe.get_suppressed_tokens,
            merge_punctuations=transcribe.merge_punctuations,
        )
    except Exception as exc:
        raise BackendError(
            ERR_LOAD_FAILED, "faster-whisper's decoding helpers are not importable"
        ) from exc


__all__ = [
    "APPEND_PUNCTUATIONS",
    "MAX_LENGTH",
    "PREPEND_PUNCTUATIONS",
    "WhisperWindow",
    "open_whisper_window",
    "token_budget",
]
