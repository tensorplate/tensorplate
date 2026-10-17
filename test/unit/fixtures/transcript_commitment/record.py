"""Record what real Whisper and voice activity models return for what a commitment replay asks.

Usage, with the sidecar package and faster-whisper importable:
    python record.py MODEL_DIR CLIP RECORD_JSON PCM_OUT --language en [--whole-clip]
        [--reuse EARLIER_RECORD ...] [--device cpu] [--compute-type int8] [--cpu-threads 4]
        -- REPLAY...

REPLAY is the command that replays RECORD_JSON: a build's tp_test_unit with the
record's case selected. The record is closed-loop: the recorder adds what the
replay names as missing, "no recorded decode of [start, end)" or "no recorded
voice activity from start", and runs the replay again until it names nothing.
A decode is one stt_decode job's single pass (whisper_window); voice activity is
the sidecar's own Silero session run over the clip from `start` to its end in
jobs of 32 frames, from reset state where the replay resets it. An entry an
earlier record of the same clip, model, settings and library versions already
holds (--reuse) is taken from it instead of run again. The record names files by
their SHA-256 only, never by path.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.metadata
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

from tensorplate_pytorch_backend.backends import silero_vad, whisper_window

_DECODE = re.compile(r"no recorded decode of \[(\d+), (\d+)\)")
_VOICE = re.compile(r"no recorded voice activity from (\d+)")
_FRAMES_PER_JOB = 32


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _decode(
    window: whisper_window.WhisperWindow, pcm: bytes, language: str, start: int, end: int
) -> dict[str, Any]:
    entry: dict[str, Any] = {"start": start, "end": end}
    try:
        result = window.decode(pcm[start * 2 : end * 2], language, start)
    except Exception as exc:  # a failed job is part of the record
        entry["error"] = type(exc).__name__
        return entry
    entry["text"] = result.text
    entry["tokens"] = list(result.tokens)
    entry["words"] = [
        [word.text, word.token_begin, word.token_end, word.start_sample, word.end_sample]
        for word in result.words
    ]
    return entry


def _voice(vad: silero_vad.SileroVad, pcm: bytes, start: int) -> dict[str, Any]:
    frame = vad.frame_samples
    frames = (len(pcm) // 2 - start) // frame
    probabilities: list[float] = []
    state = None
    for first in range(0, frames, _FRAMES_PER_JOB):
        count = min(_FRAMES_PER_JOB, frames - first)
        begin = (start + first * frame) * 2
        scored, state = vad.run(pcm[begin : begin + count * frame * 2], state)
        probabilities.extend(scored)
    return {"start": start, "probabilities": probabilities}


def _write(path: Path, record: dict[str, Any]) -> None:
    lists = {key: record.pop(key) for key in ("voice_activity", "decodes")}
    text = json.dumps(record, ensure_ascii=False, indent=1)[:-2]
    # One line per entry keeps the record reviewable as a diff.
    for key, entries in lists.items():
        lines = ",\n  ".join(json.dumps(entry, ensure_ascii=False) for entry in entries)
        text += f',\n "{key}": [\n  {lines}\n ]' if entries else f',\n "{key}": []'
    path.write_text(text + "\n}\n")
    record.update(lists)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("model_dir", type=Path)
    parser.add_argument("clip", type=Path)
    parser.add_argument("record", type=Path)
    parser.add_argument("pcm_out", type=Path)
    parser.add_argument("--language", required=True)
    parser.add_argument("--whole-clip", action="store_true")
    parser.add_argument("--device", default="cpu")
    parser.add_argument("--compute-type", default="int8")
    parser.add_argument("--cpu-threads", type=int, default=4)
    parser.add_argument("--reuse", type=Path, action="append", default=[])
    parser.add_argument("replay", nargs="+")
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
    window = whisper_window.open_whisper_window(model, numpy)
    vad = silero_vad.open_silero_vad()
    samples = len(pcm) // 2
    versions = {
        name: importlib.metadata.version(name)
        for name in ("faster-whisper", "ctranslate2", "onnxruntime", "numpy", "tokenizers", "av")
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
                path.name: _sha256(path.read_bytes())
                for path in sorted(args.model_dir.iterdir())
                if path.is_file()
            },
            "voice_activity_model": silero_vad.MODEL_FILE,
        },
        "clip": {
            "sha256": _sha256(args.clip.read_bytes()),
            "pcm_sha256": _sha256(pcm),
            "samples": samples,
        },
        "language": args.language,
        "utterances": "whole clip" if args.whole_clip else "voice activity",
        # The whole clip in one decode, the transcript the commitment is held to.
        "reference": _decode(window, pcm, args.language, 0, samples),
        "voice_activity": [],
        "decodes": [],
    }
    reused: dict[str, dict[str, Any]] = {}
    for earlier_path in args.reuse:
        earlier = json.loads(earlier_path.read_text())
        same = (
            all(earlier[key] == record[key] for key in ("recorded_with", "model", "language"))
            and earlier["clip"]["pcm_sha256"] == record["clip"]["pcm_sha256"]
        )
        if not same:
            print(f"not reused, recorded otherwise: {earlier_path.name}", file=sys.stderr)
            return 1
        for entry in earlier["decodes"]:
            reused[f"no recorded decode of [{entry['start']}, {entry['end']})"] = entry
        for entry in earlier["voice_activity"]:
            reused[f"no recorded voice activity from {entry['start']}"] = entry
    asked: set[str] = set()
    while True:
        _write(args.record, record)
        replay = subprocess.run(args.replay, capture_output=True, text=True, check=False)
        decode, voice = _DECODE.search(replay.stdout), _VOICE.search(replay.stdout)
        missing = decode or voice
        if missing is None:
            print(replay.stdout[-3000:])
            return replay.returncode
        if missing.group(0) in asked:
            print(f"the replay asks again: {missing.group(0)}", file=sys.stderr)
            return 1
        asked.add(missing.group(0))
        if missing.group(0) in reused:
            key = "decodes" if decode is not None else "voice_activity"
            record[key].append(reused[missing.group(0)])
        elif decode is not None:
            start, end = int(decode.group(1)), int(decode.group(2))
            record["decodes"].append(_decode(window, pcm, args.language, start, end))
        else:
            record["voice_activity"].append(_voice(vad, pcm, int(voice.group(1))))
        print(f"recorded {missing.group(0)}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
