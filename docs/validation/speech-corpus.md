# Speech evaluation corpus

`tools/validation/speech_corpus.py` renders the derived strata of the speech
evaluation corpus and checks the corpus manifest. The manifest's shape is
[`config/schemas/speech_corpus_manifest.json`](../../config/schemas/speech_corpus_manifest.json).
No corpus manifest is filed yet; this page describes the tool and the format
a filed one must pass.

The audio is never in this repository. The manifest holds what identifies
it: each file's path under the corpus root, its SHA-256 and the SHA-256 of its
samples alone, its source dataset, revision and license, its pool, stratum and
speech labels, and for a derived file the recipe parameters that reproduce it
from its clean source. A transcript is carried as text only where its
source's terms allow republishing it, and otherwise by digest.

## Strata

Every clean clip is 16 kHz mono 16-bit PCM WAV (`clean16k`). Each clean
speech clip may be rendered twice:

| Rendering | Recipe |
| --- | --- |
| `noisy16k` | A noise excerpt mixed in at 5, 10, 15 or 20 dB signal-to-noise ratio |
| `telephony8k` | Levelled to -26 dBov, then decimated to 8 kHz and G.711 mu-law encoded |

Silence clips (the `silence` suite) carry no language, transcript or labels,
and have a telephony rendering but no noisy one. Speech clips fall in one of
three duration bins by their labelled speech, end minus onset: under 1 s
(`lt1s`), 1 to 5 s (`1to5s`) and over 5 to 30 s (`gt5to30s`). Speech longer
than 30 s is refused.

## Recipes

Both recipes are integer arithmetic on the Python standard library, so a
rendering has the same bytes on every machine and the check can rebuild it.
A gain is computed exactly as `floor(g * 2^30)` by raising it to the 20th
power, which clears the square root of the power ratio and the decibel
fraction together.

**noisy16k.**
- The seed is the first eight bytes of
  SHA-256(`<corpus version>/noisy16k/<clean file id>`), as a big-endian
  integer, so seeds stay stable when clips are added. The noise excerpt starts
  at the seed modulo the number of starting positions that fit,
  `noise samples - clip samples + 1`.
- The ratio is measured over the labelled speech only. Leading and trailing
  silence does not change it.
- A mixture that would exceed 16 bits is scaled as a whole to fit, preserving
  the ratio. The manifest records the peak before scaling (`mixture_peak`).
- A noise file shorter than the clip is refused rather than looped, so noise
  files are chosen at least as long as the clips they are mixed into. A silent
  span of speech or of noise is refused too.

**telephony8k.**
- The labelled speech is levelled to an RMS of -26 dBov, where 0 dBov is a
  full-scale 16-bit square wave.
- If the level gain would push any sample past 16 bits, it is lowered to the
  largest gain that does not. `achieved_dbov` then reads below the target.
- The levelled clip goes through the qualification tool's telephony recipe,
  `tp-telephony8k-fir63-g711stl-v1` in `tools/validation/candidate_audio.py`:
  - the 63-tap Q15 low-pass decimator `tp-fir63-blackman-q15-v1`;
  - then the ITU-T STL G.711 mu-law encoder.
- The file is the headerless mu-law bytes, as a client sends them.
- The decimator is zero-phase: 16 kHz sample `2k` becomes 8 kHz sample `k`. A
  rendering's labels are therefore the clean clip's `onset // 2` and
  `ceil(end / 2)`, and its length is `ceil(samples / 2)`.

The encoder is checked in `test/validation/candidate_qualify_test.py` against
the G.711 reference files of the ITU-T Software Tool Library (STL2024,
`src/g711/test_data`): the digests of every 16-bit input encoded, and of every
code decoded, equal those of `sweep-r.u` and `sweep-r.u-u`. Because the
encoder maps each sample on its own, this covers every input a rendering can
contain.

The level is cross-checked by recording, per telephony speech rendering, the
active speech level that the STL's `sv56demo` measures on the levelled 16 kHz
signal (`sv56_dbov`). `check --frozen` refuses a manifest in which any is
missing. The tool records the value; it sets no threshold on it.

## Pools

A file is in the `tuning` pool or the `held_out` pool.

- Derived files are in their clean file's pool, and a noisy file mixes noise
  from the same pool.
- No source file, noise file included, may appear in both pools. Nor may a
  speaker key or a reference transcript.
- Speaker keys are per corpus, across all sources, never a dataset's own
  speaker identifiers.
- Each clean file is a distinct recording: no two clean files have the same
  samples, and a source utterance is used by one clean speech file, except
  inside a concatenation. A clean file whose source lists several files is a
  concatenation of real utterances, and a silence clip may be one of several
  excerpts of a long file.

## Checking a manifest

```sh
tools/validation/speech_corpus.py check manifest.json
tools/validation/speech_corpus.py check manifest.json --root /path/to/corpus --rebuild
tools/validation/speech_corpus.py summary manifest.json
```

Without `--root`, the check runs on the manifest alone:
- the schema;
- the recipe sources' digests against the files beside the tool, so a change
  to either recipe invalidates the manifest's renderings;
- duration bins against labels, and inherited labels, lengths and fields
  against each derived file's clean file;
- each seed against the corpus version, and each noise offset against the
  seed and the noise file's length;
- transcript digests against their text, pool separation and distinct
  recordings.

With `--root`, it also reads every file and noise file, under that directory
only, and compares the sizes, sample counts, rates and both digests.

`--rebuild` re-renders every derived file from its clean file and noise, then
compares the bytes and the recorded parameters.

`summary` prints the counts per pool, suite, language, stratum and bin, the
distinct references per language and pool, the files per source, and the
noisy files per SNR.

Exit status: 0 valid, 1 refused, 2 unreadable.
