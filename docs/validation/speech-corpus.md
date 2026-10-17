# Speech evaluation corpus

`tools/validation/speech_corpus.py` renders the derived strata of the speech
evaluation corpus and checks the corpus manifest. The manifest's shape is
[`config/schemas/speech_corpus_manifest.json`](../../config/schemas/speech_corpus_manifest.json).
The filed corpus, its manifest and the plan it was built from, is in
[`evidence/speech-corpus-2026.10/`](evidence/speech-corpus-2026.10/README.md).

The audio is never in this repository. The manifest holds what identifies
it: each file's path under the corpus root, its SHA-256 and the SHA-256 of its
samples alone, its source dataset, revision and license, its pool, stratum and
speech labels, and for a derived file the recipe parameters that reproduce it
from its clean source. A transcript is carried as text only where its
source's terms allow republishing it, and otherwise by digest. Each source
names where its license is stated (`license_statement`) and how many of its
files each selection rule withheld (`selection`).

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
- The SNR is assigned, not chosen: within each pool, language and duration
  bin, the clean speech clips in sorted id order take 5, 10, 15 and 20 dB in
  turn, so every group is balanced across the four.
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
  largest gain that does not, and the level then reads below the target.
- The manifest records the level as the levelled span's exact integer power
  (`levelled_span_power`), so a rebuild compares it without floating point;
  `speech_corpus.achieved_dbov` turns it into dBov.
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
- each SNR against the corpus's assignment, each seed against the corpus
  version, and each noise offset against the seed and the noise file's length;
- a mu-law file's size against its sample count;
- transcript digests against their text, pool separation and distinct
  recordings;
- no semantic class listed twice on one file.

With `--root`, it also reads every file and noise file, under that directory
only, and compares the sizes, sample counts, rates and both digests.

`--rebuild` re-renders every derived file from its clean file and noise, then
compares the bytes and the recorded parameters.

`summary` prints the counts per pool, suite, language, stratum and bin, the
distinct references per language and pool, the files per source, and the
noisy files per SNR.

Exit status: 0 valid, 1 refused, 2 unreadable.

A run over the corpus is scored with `tools/validation/speech_quality.py`; see
[Speech evaluation quality scoring](speech-quality.md).

## Building the corpus

`tools/validation/speech_corpus_build.py` builds a corpus from downloaded
sources and a plan: the sources with their pools, the quotas, the noise pool
and its exclusions. The aligner runs between the steps:

```sh
speech_corpus_build.py noise PLAN --sources SRC --work WORK
speech_corpus_build.py screen-jobs PLAN --sources SRC --work WORK
speech_corpus_align.py WORK/screen-jobs.json WORK/screen.jsonl --screen
speech_corpus_build.py screen-exclusions PLAN --sources SRC --work WORK
speech_corpus_build.py candidates PLAN --sources SRC --work WORK
speech_corpus_align.py WORK/align-jobs.json WORK/labels.jsonl --arabic-model DIR
speech_corpus_build.py build PLAN --sources SRC --work WORK --root CORPUS --sv56 sv56demo
speech_corpus.py check CORPUS/manifest.json --root CORPUS --rebuild --frozen
```

- `screen-exclusions` prints the exclusions the screen finds. They go into
  the plan's `noise.excluded`, and `noise` runs again so that `build` mixes
  none of them.
- `build` writes into an empty `CORPUS`: the audio, `manifest.json`, and
  `labels.jsonl`, the labels its selection read.
- `build --labels CORPUS/labels.jsonl` selects the same corpus again without
  re-running the aligner, which on another machine could move a label by a
  frame.
- Without `--root`, `build` only selects and prints the quotas still unfilled.

**Candidates.**
- Every file of a source split is a candidate for that split's pool. A split
  may narrow its source to listed keywords, speakers or prompts.
- The rules withhold a file under the first rule it meets, and the manifest
  records each rule's count per source:

| Rule | Why |
| --- | --- |
| not 16 kHz mono | the clean rendering is 16 kHz mono; also a WAV that is not 16-bit, or an empty file |
| file longer than 30 s | the longest utterance a session admits |
| 10 or more samples at 16-bit full scale | a clipped recording |
| reference text used in the other pool | the check refuses a transcript in both pools; only files the rules above left are compared |
| first or last word not aligned | the labels would miss part of the speech |
| aligner score below 0.2 | the aligner's mean token probability says it did not find the text in the audio |
| labelled speech shorter than 120 ms | no reference word is that short; the label is not the speech |
| labelled speech within 20 ms of the file's start or end | the file may cut the speech off, so a label at its edge is not the speech's |
| under 1 s, labelled onset more than 50 ms from the band-energy onset | a single word's onset sits at its first rise in energy |

- The last five rules need labels. They are counted only up to the last file
  a quota examined, so the counts do not depend on how far the aligner ran
  past it.
- The plan states the label rules (`label_rules`), and the build refuses a
  plan that states other values than the tool applies.
- **The band-energy onset** (`tp-band-energy-onset-300-3400hz-25db-3x10ms-v1`):
  - the samples pass two Butterworth high-pass biquads at 300 Hz, then two
    low-pass biquads at 3400 Hz, with coefficients fixed in the tool;
  - the onset is the start of the first of 3 consecutive 10 ms frames whose
    energy is within 25 dB of the clip's loudest frame.
- **Why the energy check covers only the under-1 s bin.** It is a cross-check,
  not the label. On longer clips, breaths, clicks and handling noise before
  speech fall inside the band, and the crude onset cannot tell them from
  speech; the filed corpus's README gives the counts. So the longer bins are
  checked by ear instead. The build files every
  examined label's energy onset (`energy_onset_sample` in `labels.jsonl`),
  and the onset listen-check draws half of its longer clips from those whose
  energy onset is more than 100 ms from the label. Ends are not
  cross-checked: a CTC aligner ends a word early, and no gate times from the
  end.
- A float source is converted to 16 bits by `round-half-even(x * 32767)`,
  clipped; a 16-bit source keeps its samples.

**Labels.**
- `tools/validation/speech_corpus_align.py` labels each candidate by CTC
  forced alignment of its reference text. The onset is the start of the
  first aligned word and the end the end of the last, at the model's 20 ms
  frames: frame `k` is sample `320 * k`.
- Labels recorded before the aligner used that stride were placed at
  `k * samples / frames`. `speech_corpus_build.py restride-labels` converts
  them exactly: each label maps back to one frame index.
- The aligners are not the product's voice activity detector, so a latency
  measured from these labels includes the detector's own delay:

| Language | Aligner | License |
| --- | --- | --- |
| en | torchaudio 2.5.1 `WAV2VEC2_ASR_BASE_960H` | MIT (wav2vec 2.0) |
| ar | `jonatasgrosman/wav2vec2-large-xlsr-53-arabic`, transformers 4.46.3 | Apache-2.0 |

- A word is aligned on the characters the aligner's alphabet has, and a word
  with none of them is dropped. When the first or last word has a character
  the alphabet lacks, such as a digit, the file is withheld.
- Arabic diacritics and tatweel are removed before alignment, and English
  hyphens split words.
- Neither aligner runs in CI.

**Selection.**
- Each quota names a source, a pool, a duration bin and a count. It takes the
  first files of that bin in a seeded order, SHA-256 over
  `<version>/select/<source>/<file id>`, so a file's place does not move when
  others come or go.
- A quota skips a file whose speaker already has `speaker_cap` files in it,
  or whose text group already has `text_cap`. The text group is:
  - the reference text, which for Speech Commands is the keyword;
  - the sentence id for FLEURS;
  - the speaker and keyword for Arabic Speech Commands, so a speaker's ten
    takes of one keyword count once.
- The aligner labels each source and pool's files in this same order, and
  selection stops at the first unlabelled file before a quota fills. Labelling
  a prefix therefore selects exactly what labelling every file would.

**Speakers and fixtures.**
- A speaker key is `s` and the first 12 hex digits of SHA-256 over
  `<version>/speaker/<namespace>/<speaker>`, where the plan names one
  namespace per dataset. A speaker has one key across a dataset's archives,
  so the check sees a speaker in both pools.
- The semantic fixtures are held-out files. Per language, each class takes
  its first `per_class` qualifying files, in a seeded order, that no class
  took before it, so a language has four times `per_class` fixtures. The build
  refuses a class with too few. A fixture lists every class it meets.
  The reference is read exactly as `tools/validation/speech_quality.py`
  scores it, with its normalizer and its critical words, so a fixture is one
  the scorer judges:
  - `negation`: a negation word in the scorer's negation view, where
    English "cannot" reads as "can not" and the t of an n't contraction,
    with either apostrophe, as "not";
  - `numerals`: a digit or a number word;
  - `repetition`: a word equal to one of the three before it;
  - `window_seam`: more than 8 s of labelled speech.

**Noise.**
- The noise pool is the first 32 s of the first `count` recordings at least
  that long, in a seeded order, so every clip fits every noise file. The
  manifest counts the recordings too short and those after the first `count`.
- A noise file is tuning noise when the first 16 hex digits of SHA-256 over
  `<version>/noise-pool/<file id>`, read as a number, are divisible by
  `tuning_one_in`. A noisy rendering's noise file is the eligible file at
  the same kind of number, over `<version>/noise/<clean file id>`, modulo
  their count.
- The eligible files are the rendering's pool's files that are not excluded,
  in id order.
- Before the build, the aligner decodes every noise file with the English
  model, greedily. A file whose decode has 10 or more letters may hold
  speech, and the plan excludes it with that count as the reason. The
  screen does not listen; it decides only what the decode shows.

**Cross-checks.** With `--sv56`, the build records for every telephony speech
rendering the active speech level that ITU-T STL `sv56demo` measures on the
levelled 16 kHz signal (`sv56_dbov`). `check --frozen` requires these levels.
