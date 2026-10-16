"""Record what one stt_decode job's single pass sees and returns, from a real Whisper model.

Usage, with the sidecar package and faster-whisper importable:
    python record.py MODEL_DIR CLIP RECORD_JSON PCM_OUT
        [--language en] [--start-sample N] [--device cpu] [--compute-type int8]
        [--cpu-threads 4]

The clip is decoded to 16 kHz mono 16-bit PCM, the job's payload, which is
written to PCM_OUT for the real-model test cases. The record names files by
their SHA-256 only, never by path. On a CPU the aligned times can move with
OpenMP's thread setting, so the record names it and the CPU threads.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import importlib.metadata
import json
import os
import platform
import sys
from pathlib import Path
from typing import Any

from tensorplate_pytorch_backend.backends import whisper_window


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _plain(value: Any) -> Any:
    if isinstance(value, dict):
        return {key: _plain(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_plain(item) for item in value]
    if hasattr(value, "item"):
        return value.item()
    return value


class _Recording:
    """The model, with its generate and alignment calls written into ``record``."""

    def __init__(self, model: Any, record: dict[str, Any]) -> None:
        self._model, self._record = model, record
        self.model = self._Engine(model.model, record)

    class _Engine:
        def __init__(self, engine: Any, record: dict[str, Any]) -> None:
            self._engine, self._record = engine, record

        def __getattr__(self, name: str) -> Any:
            return getattr(self._engine, name)

        def generate(self, features: Any, prompts: list[list[int]], **options: Any) -> Any:
            results = self._engine.generate(features, prompts, **options)
            self._record["generate"] = {
                "prompt": list(prompts[0]),
                "options": _plain(options),
                "tokens": list(results[0].sequences_ids[0]),
            }
            return results

    def __getattr__(self, name: str) -> Any:
        return getattr(self._model, name)

    def find_alignment(
        self,
        tokenizer: Any,
        text_tokens: list[list[int]],
        encoder_output: Any,
        num_frames: int,
    ) -> list[list[dict[str, Any]]]:
        aligned = self._model.find_alignment(tokenizer, text_tokens, encoder_output, num_frames)
        self._record["align"] = {"text_tokens": _plain(text_tokens), "num_frames": num_frames}
        self._record["alignment"] = _plain(copy.deepcopy(aligned[0]))
        return list(aligned)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("model_dir", type=Path)
    parser.add_argument("clip", type=Path)
    parser.add_argument("record", type=Path)
    parser.add_argument("pcm_out", type=Path)
    parser.add_argument("--language", default="en")
    parser.add_argument("--start-sample", type=int, default=160_000)
    parser.add_argument("--device", default="cpu")
    parser.add_argument("--compute-type", default="int8")
    parser.add_argument("--cpu-threads", type=int, default=4)
    args = parser.parse_args()

    import faster_whisper
    import numpy

    audio = faster_whisper.decode_audio(str(args.clip), sampling_rate=16_000)
    pcm = numpy.clip(numpy.round(audio * 32768.0), -32768, 32767).astype("<i2").tobytes()
    args.pcm_out.write_bytes(pcm)
    model = faster_whisper.WhisperModel(
        str(args.model_dir),
        device=args.device,
        compute_type=args.compute_type,
        cpu_threads=args.cpu_threads,
    )
    versions = {
        name: importlib.metadata.version(name)
        for name in ("faster-whisper", "ctranslate2", "numpy", "tokenizers", "av")
    }
    record: dict[str, Any] = {
        "recorded_with": {**versions, "python": platform.python_version()},
        "model": {
            "device": args.device,
            "compute_type": args.compute_type,
            "loaded_compute_type": model.model.compute_type,
            "cpu_threads": args.cpu_threads,
            "omp_num_threads": os.environ.get("OMP_NUM_THREADS"),
            "files_sha256": {
                path.name: _sha256(path)
                for path in sorted(args.model_dir.iterdir())
                if path.is_file()
            },
        },
        "clip": {"sha256": _sha256(args.clip), "pcm_sha256": hashlib.sha256(pcm).hexdigest()},
        "job": {
            "language": args.language,
            "start_sample": args.start_sample,
            "samples": len(pcm) // 2,
        },
    }
    window = whisper_window.open_whisper_window(_Recording(model, record), numpy)
    merge = window._merge_punctuations

    def merging(alignment: list[dict[str, Any]], prepended: str, appended: str) -> None:
        merge(alignment, prepended, appended)
        record["merged"] = _plain(copy.deepcopy(alignment))

    window._merge_punctuations = merging
    status = 0
    try:
        result = window.decode(pcm, args.language, args.start_sample)
        record["result"] = {
            "text": result.text,
            "tokens": list(result.tokens),
            "words": [
                [word.text, word.token_begin, word.token_end, word.start_sample, word.end_sample]
                for word in result.words
            ],
        }
    except Exception as exc:  # the seams recorded so far are kept
        record["error"] = type(exc).__name__
        status = 1
    args.record.write_text(json.dumps(record, ensure_ascii=False, indent=1) + "\n")
    return status


if __name__ == "__main__":
    sys.exit(main())
