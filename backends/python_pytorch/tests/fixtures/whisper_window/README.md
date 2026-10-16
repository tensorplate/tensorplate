# Whisper window decode record

`jfk_turbo_int8_cpu.json` is what `record.py` saw at each seam of one
`stt_decode` job's single pass over a real clip, and the job's result: the
decode's prompt, options and tokens, the alignment's call and items, the items
after punctuation is attached, and the transcript. It was recorded, not
written. `tests/test_whisper_window.py` replays its seams through the faked
engine and expects its result.

- Model: the Whisper large-v3-turbo files of the speech provisioning
  (`packaging/provisioning/manifest.json`, bundle `stt-whisper-candidate`),
  loaded by CTranslate2 on an x86 CPU with compute type `int8`, which it ran
  as `int8_float32`, with 4 CPU threads and `OMP_NUM_THREADS` unset. The
  record names each model file by its SHA-256, and a test checks them against
  the manifest.
- Clip: `tests/data/jfk.flac` of the faster-whisper repository at its
  `v1.2.1` tag (commit `65882eee9f5cdbeeb2d877f1131d48cf241b327d`), 11 s of
  speech. The record holds the SHA-256 of the file and of the 16 kHz 16-bit
  PCM decoded from it, not the audio.
- Job: language `en` from `start_sample` 160000, so word samples read as
  offsets on a session's timeline.
- Library versions: the record's `recorded_with`.

The recorder names files by hash and writes no path, so nothing in the record
identifies the host it ran on, and it is committed as recorded.

On that CPU the aligned word times did not move with CTranslate2's thread
count (2, 4 and 8 gave this record) but did with `OMP_NUM_THREADS`: set to 2
or 8, two to four word boundaries moved, one by 0.54 s, with the same text
and tokens. The record names both, and the real-pass case refuses another
`OMP_NUM_THREADS`. That is a property of this CPU record: the service runs
the model on a GPU.

To record again, with faster-whisper and the sidecar package importable
(faster-whisper 1.2.1's `decode_audio` worked with PyAV 15.1.0 and failed
with PyAV 19.0.1, which no longer takes an argument it passes):

```bash
PYTHONPATH=backends/python_pytorch/src python \
  backends/python_pytorch/tests/fixtures/whisper_window/record.py \
  MODEL_DIR jfk.flac RECORD_JSON CLIP_PCM [--device cuda --compute-type float16]
```

Another device or compute type makes another record. With the model directory
and the PCM the recorder wrote named in `TENSORPLATE_TEST_WHISPER_MODEL_DIR`
and `TENSORPLATE_TEST_WHISPER_CLIP_PCM`, `tests/test_whisper_window.py` also
runs the real pass with the record's device, compute type and threads and
holds it to this record.
