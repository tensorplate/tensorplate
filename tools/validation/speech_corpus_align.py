#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Speech onset and end labels for corpus clips, by CTC forced alignment.

The labels are the start of the first aligned word and the end of the last, in
samples at 16 kHz. They come from a forced aligner that is not the product's
voice activity detector, so a latency measured from them includes the
detector's own delay. English uses torchaudio's WAV2VEC2_ASR_BASE_960H
(wav2vec 2.0 weights, MIT); Arabic uses jonatasgrosman/wav2vec2-large-xlsr-53-arabic
(Apache-2.0) from a local directory. Neither runs in CI: this is an operator
step whose output, one JSON object per line, the corpus build reads.

    speech_corpus_align.py JOBS.json OUT.jsonl --arabic-model DIR

JOBS.json is a list of {"id", "path", "language", "text"}. With --screen, each
job instead names an excerpt ("start", "frames") whose greedy transcript is
written, so noise can be screened for intelligible speech. A word with no
character the aligner knows is not aligned; when the first or last word is
such a word, or carries such a character, the result says so (`edge_oov`),
because the label would then miss part of the speech.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

import soundfile
import torch
import torchaudio
from torchaudio.functional import forced_align, merge_tokens

RATE = 16000
# wav2vec 2.0's feature encoder emits one frame per 320 samples at 16 kHz.
STRIDE = 320
HARAKAT_AND_TATWEEL = "".join(chr(c) for c in range(0x064B, 0x0653)) + chr(0x0670) + chr(0x0640)


class Aligner:
    def __init__(self, labels: list[str], blank: int, model: Any, word_separator: str) -> None:
        # The blank is never a target; English's blank is "-", which transcripts also use.
        self.index = {label: i for i, label in enumerate(labels) if label and i != blank}
        self.blank = blank
        self.model = model
        self.separator = word_separator

    def emissions(self, samples: torch.Tensor) -> torch.Tensor:
        raise NotImplementedError

    def words(self, text: str) -> list[str]:
        raise NotImplementedError

    def decode(self, samples: torch.Tensor) -> str:
        """Greedy CTC transcript, for screening noise for intelligible speech."""
        with torch.inference_mode():
            best = self.emissions(samples)[0].argmax(dim=-1).tolist()
        labels = {i: label for label, i in self.index.items()}
        out = [
            labels[i]
            for n, i in enumerate(best)
            if i != self.blank and (n == 0 or i != best[n - 1])
        ]
        return "".join(out).replace(self.separator, " ").strip()

    def align(self, samples: torch.Tensor, text: str) -> dict[str, Any]:
        words = self.words(text)
        known = ["".join(ch for ch in w if ch in self.index) for w in words]
        edge_oov = not words or known[0] != words[0] or known[-1] != words[-1]
        kept = [w for w in known if w]
        if not kept:
            return {"aligned": False, "edge_oov": True}
        tokens = []
        for n, word in enumerate(kept):
            if n:
                tokens.append(self.index[self.separator])
            tokens.extend(self.index[ch] for ch in word)
        with torch.inference_mode():
            log_probs = self.emissions(samples)
        frames = log_probs.shape[1]
        if frames < len(tokens):
            return {"aligned": False, "edge_oov": edge_oov}
        path, scores = forced_align(
            log_probs, torch.tensor([tokens], dtype=torch.int32), blank=self.blank
        )
        spans = [
            s
            for s in merge_tokens(path[0], scores[0].exp(), blank=self.blank)
            if s.token != self.index[self.separator]
        ]
        onset = spans[0].start * STRIDE
        end = min(samples.shape[-1], spans[-1].end * STRIDE)
        return {
            "aligned": True,
            "edge_oov": edge_oov,
            "speech_onset_sample": onset,
            "speech_end_sample": end,
            "frames": frames,
            "stride": STRIDE,
            "score": round(sum(s.score for s in spans) / len(spans), 4),
            "words_aligned": len(kept),
            "words": len(words),
        }


class EnglishAligner(Aligner):
    def __init__(self) -> None:
        bundle = torchaudio.pipelines.WAV2VEC2_ASR_BASE_960H
        super().__init__(list(bundle.get_labels()), 0, bundle.get_model().eval(), "|")

    def emissions(self, samples: torch.Tensor) -> torch.Tensor:
        emission, _ = self.model(samples.unsqueeze(0))
        return torch.log_softmax(emission, dim=-1)

    def words(self, text: str) -> list[str]:
        return text.upper().replace("-", " ").split()


class ArabicAligner(Aligner):
    def __init__(self, model_dir: Path) -> None:
        from transformers import Wav2Vec2ForCTC, Wav2Vec2Processor

        self.processor = Wav2Vec2Processor.from_pretrained(model_dir)
        vocab = self.processor.tokenizer.get_vocab()
        labels = [""] * len(vocab)
        for label, i in vocab.items():
            labels[i] = label
        pad = self.processor.tokenizer.pad_token_id
        model = Wav2Vec2ForCTC.from_pretrained(model_dir).eval()
        super().__init__(labels, pad, model, "|")

    def emissions(self, samples: torch.Tensor) -> torch.Tensor:
        values = self.processor(
            samples.numpy(), sampling_rate=RATE, return_tensors="pt"
        ).input_values
        return torch.log_softmax(self.model(values).logits, dim=-1)

    def words(self, text: str) -> list[str]:
        text = re.sub(f"[{HARAKAT_AND_TATWEEL}]", "", text)
        return text.split()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("jobs", type=Path)
    parser.add_argument("out", type=Path)
    parser.add_argument("--arabic-model", type=Path)
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--screen", action="store_true", help="decode excerpts instead of aligning")
    args = parser.parse_args(argv)
    torch.set_num_threads(args.threads)
    jobs = json.loads(args.jobs.read_text(encoding="utf-8"))
    done = set()
    if args.out.exists():
        done = {
            json.loads(line)["id"] for line in args.out.read_text(encoding="utf-8").splitlines()
        }
    aligners: dict[str, Aligner] = {}
    with args.out.open("a", encoding="utf-8") as out:
        for job in jobs:
            if job["id"] in done:
                continue
            language = job["language"]
            if language not in aligners:
                aligners[language] = (
                    EnglishAligner() if language == "en" else ArabicAligner(args.arabic_model)
                )
            if args.screen:
                data, rate = soundfile.read(
                    job["path"], dtype="float32", start=job["start"], frames=job["frames"]
                )
            else:
                data, rate = soundfile.read(job["path"], dtype="float32")
            if rate != RATE or data.ndim != 1:
                raise SystemExit(f"{job['id']}: not 16 kHz mono")
            samples = torch.from_numpy(data)
            if args.screen:
                result = {"text": aligners[language].decode(samples)}
            else:
                result = aligners[language].align(samples, job["text"])
            out.write(json.dumps({"id": job["id"], "samples": len(data), **result}) + "\n")
            out.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
