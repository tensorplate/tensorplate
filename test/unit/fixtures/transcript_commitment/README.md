# Transcript commitment records

Each record holds what a real Whisper model and the sidecar's voice activity
model returned for everything a replay of `TranscriptCommitment` asked for
over one clip, and the whole clip in one decode (`reference`). They were
recorded, not written: `record.py` adds a decode or a run of voice activity
only when `tp_test_unit`'s replay of the record names it as missing, so a
record is exactly what the replay asks for, closed-loop.
`transcript_commitment_record_test.cpp` replays each record;
`transcript_commitment_test.cpp` covers with built decodes what a record
cannot choose: drift at the tolerance, unstable alignment, refusals, resets.

How a replay asks (`"utterances"` in each record):

- `voice activity`, as the speech dispatch decodes a session: the voice
  activity policy over the clip's frame probabilities, one 512-sample
  frame at a time; in each utterance a decode of the ring's window once
  320 ms past its speech start are scored and again after each further
  160 ms (the profile's initial accumulation and decode stride), and the
  tail at its endpoint. Unlike the dispatch, the replay resets the voice
  activity model's state after each endpoint: a run of voice activity
  starts at the clip's start or at such a reset and goes to the clip's
  end, in jobs of 32 frames.
- `whole clip`: the clip as one utterance from its first sample, decoded
  320 ms in and every 160 ms after, without voice activity.

The replay scores a frame and decodes a due window at once; the dispatch
takes as long as its jobs do, so a busy decode lane decodes later windows.
The dispatch keeps one voice activity state over a session's audio and
never resets it at an endpoint, so after the first endpoint a session's
voice activity, and with it its utterances, can differ from the replay's.

Voice detection splits the English clip's repeated words ("ask", "what",
"can do for") into separate utterances, and no Arabic repetition falls
inside one commitment, so repeated words within one utterance are covered
only by the built decodes of `transcript_commitment_test.cpp`.

- Model: the Whisper large-v3-turbo files of the speech provisioning
  (`packaging/provisioning/manifest.json`, bundle `stt-whisper-candidate`),
  loaded by CTranslate2 on an x86 CPU with compute type `int8`, which it ran
  as `int8_float32`, with 4 CPU threads and `OMP_NUM_THREADS` unset; each
  record names the model files by their SHA-256 and the library versions it
  ran with.
- Decode: one `stt_decode` job's single pass
  (`backends/python_pytorch/src/tensorplate_pytorch_backend/backends/whisper_window.py`),
  so a word's text carries the punctuation the runner attaches to it.
- Voice activity: the sidecar's `SileroVad`
  (`backends/python_pytorch/src/tensorplate_pytorch_backend/backends/silero_vad.py`)
  over faster-whisper's Silero model, on one CPU thread.
- `en_jfk_turbo_int8_cpu.json`: `tests/data/jfk.flac` of the faster-whisper
  repository at its `v1.2.1` tag (commit
  `65882eee9f5cdbeeb2d877f1131d48cf241b327d`), 11 s of English speech whose
  pauses end utterances.
- `ar_fleurs_turbo_int8_cpu.json` and `ar_fleurs_whole_clip_turbo_int8_cpu.json`:
  `dev/13551758606629841190.wav` of the `ar_eg` configuration of FLEURS
  (Conneau et al., 2022, "FLEURS: Few-shot Learning Evaluation of Universal
  Representations of Speech"), <https://huggingface.co/datasets/google/fleurs>
  at revision `70bb2e84b976b7e960aa89f1c648e09c59f894dd`, archive
  `data/ar_eg/audio/dev.tar.gz`: 11 s of read Modern Standard Arabic, one
  utterance of about 10 s, licensed under the Creative Commons Attribution
  4.0 International licence (<https://creativecommons.org/licenses/by/4.0/>).
  The records hold the models' output for it, not the audio.

A record holds the SHA-256 of its clip and of the 16 kHz 16-bit PCM decoded
from it, never the audio, and names no path, so nothing in it identifies the
host it ran on; it is committed as recorded.

To record again, with faster-whisper 1.2.1, CTranslate2 4.8.2, PyAV 15.1.0 and
the sidecar package importable, and `tp_test_unit` built from the tree the
record is written in (the replay reads it from there):

```bash
PYTHONPATH=backends/python_pytorch/src python \
  test/unit/fixtures/transcript_commitment/record.py \
  MODEL_DIR jfk.flac test/unit/fixtures/transcript_commitment/en_jfk_turbo_int8_cpu.json \
  CLIP_PCM --language en -- \
  build/test/unit/tp_test_unit --gtest_filter=TranscriptCommitmentRecord.EnglishUtterances
```

`--whole-clip` records a `whole clip` replay. `--reuse EARLIER_RECORD` takes
an entry an earlier record of the same clip, model, settings and library
versions already holds instead of running it again; these records reuse the
entries of earlier recordings under earlier rules. A change to the
commitment's rules or the voice activity policy can ask for other windows;
record again then, and the replay's expectations follow the new record.
