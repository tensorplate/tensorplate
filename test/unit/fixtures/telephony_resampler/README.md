# Telephony resampler fixtures

`tp_fir63_blackman_q15_v1.json` holds what the pinned reference in
`tools/validation/candidate_audio.py` gives for the cases
`test/unit/telephony_resampler_test.cpp` replays against
`TelephonyResampler`:

- `mulaw_to_linear`: the reference's G.711 decoder over all 256 bytes,
  which the recorder first checks equal to CPython's `audioop.ulaw2lin`.
- `resampler_id` and `taps_q15`: the reference's filter, which the recorder
  first checks equal to the formula it was quantized from.
- `cases`: mu-law input (`mulaw_hex`) and the reference's `interpolate_2x`
  of its decoded samples over the whole input (`pcm_s16le_hex`, 16-bit
  little-endian at 16 kHz). `noise` is 2,000 seeded random bytes, which
  drive the filter past the 16-bit range; `tones` is 2,000 samples of
  300 Hz, 1,000 Hz and 3,400 Hz tones through the reference's encoder,
  which only builds this input: a later change to the encoder changes
  this case's input alone, and the record its expected output with it;
  `short` is the first seven bytes of `noise`.
- `flushed_prefix`: the reference's resample of the first 1,000 bytes of
  `noise` alone, as a flush after them gives it.

The inputs are synthetic and the file names no host path. The recorder
exits unless the noise case holds a rounding tie of each sign and an
output at each 16-bit limit (`rounding_ties`, `clamped_outputs`). To
record again, with a CPython that still has `audioop` (3.12 or earlier;
only `recorded_with.python` changes between versions):

```bash
python3 test/unit/fixtures/telephony_resampler/record.py \
  test/unit/fixtures/telephony_resampler/tp_fir63_blackman_q15_v1.json
```
