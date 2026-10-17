# Telephony dispatch fixture

`speech_pattern.json` holds a 16 kHz speech pattern and its 8 kHz mu-law
derivation by the telephony recipe of `tools/validation/candidate_audio.py`,
`tp-telephony8k-fir63-g711stl-v1` (`telephony_8k_ulaw_bytes`: `decimate_2x`,
then the ITU-T STL encoder), as
`test/unit/speech_telephony_input_test.cpp` sends both to the speech
dispatch:

- `segments`: the pattern, in order. A segment with a `tone_period` is an
  integer triangle tone of that period in samples at `amplitude`; one with
  a period of 0 is a quiet run, the same tone at `quiet_period` and
  `quiet_amplitude`. The case builds the samples again and checks them
  against `pcm_s16le_fnv1a64`, the FNV-1a 64-bit digest of their 16-bit
  little-endian bytes.
- `telephony`: the recipe id and the derivation's bytes (`mulaw_hex`),
  8 kHz, one byte a sample, as a client sends them.

The fixture backend scores a 512-sample frame as speech when its peak is at
least half of full scale, so the tones are speech and the quiet runs
silence. The pattern holds three utterances: a pause of 3,203 samples
(200 ms) too short to end the first, two silence endpoints, and a Finalize
after the last. Its odd segment lengths put edges between the samples of
the derivation. The input is synthetic and the file names no host path.
The file is the recorder's output, never edited by hand; to record again:

```bash
python3 test/unit/fixtures/telephony_dispatch/record.py \
  test/unit/fixtures/telephony_dispatch/speech_pattern.json
```
