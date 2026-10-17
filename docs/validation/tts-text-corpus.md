# Text-to-speech evaluation corpus

`tools/validation/corpus/tts-text-en-us.json` is the frozen text input of the
text-to-speech release gates for `en-US` and the `af_heart` voice. Its shape
is [`config/schemas/tts_text_corpus.json`](../../config/schemas/tts_text_corpus.json),
and `tools/validation/tts_corpus.py` checks it, re-counts it and records the
reference arm the listening review compares the candidate with. No audio is
in this repository.

## What the corpus holds

Every sentence carries its text, its source and the source's identifier for
it, its phoneme-token count, its stratum and its critical classes.

| Source | License | Text |
| --- | --- | --- |
| `cmu-arctic` | CMU ARCTIC: use, copy, modify and license for any purpose, with the notice retained ([`cmu-arctic-COPYING.txt`](../../tools/validation/corpus/cmu-arctic-COPYING.txt), kept verbatim) | The prompt list `cmuarctic.data`, unmodified |
| `libritts-r` | CC BY 4.0 | The `.original.txt` transcript of a LibriTTS-R test-clean utterance, unmodified |
| `authored` | CC0 1.0 | Written for this corpus: short replies, and numerals as digits (times, dates, money, phone numbers) |

A source outside this table is refused by the check, so text without a
license found, such as the Harvard sentences, cannot be added by data alone.
The source entries carry the attribution each license asks for, the URL each
file was fetched from and its SHA-256, and for LibriTTS-R the MD5 that
openslr publishes for the archive, recorded as published and not checked by
the tool. The check refuses a corpus listing `cmu-arctic` unless the notice
file is present and the attribution names it.

**Counts.** A sentence's count is the length of the phoneme string that
kokoro's `KPipeline(lang_code=<language>, trf=False).g2p` returns for its
text. That is the string the Kokoro runner bounds at 200 and synthesizes, so
the count is the runner's own. It depends on the phonemizer's whole stack:
misaki's lexicon, spaCy's tagger, and espeak-ng for words outside the
lexicon. The corpus records the speech runtime's lock file by digest and the
kokoro, misaki, spaCy and `en_core_web_sm` versions, which the check holds to
that lock, and the distribution's `libespeak-ng1` version the counts were
made with. A change to the lock invalidates the counts until `count --write`
re-records them.

**Strata.** `t1to20`, `t21to100` and `t101to200` by phoneme tokens. The check
derives each sentence's stratum from its count and requires:

- at least 100 sentences in each stratum, so each latency stratum's 100
  samples per run need no repeated sentence. Every `t1to20` sentence is a
  short case, sent on its own and flushed;
- a sentence at exactly 20, 21, 100, 101 and 200 tokens, the stratum edges
  and the segment bound;
- at least 20 long or boundary sentences: at one of those counts, or at 181
  tokens or more;
- distinct texts, compared after NFC, case folding and whitespace collapse;
  each text stripped NFC with no control or format character (a newline
  would split it into two segments), no U+02BC modifier letter apostrophe
  (a letter, so a contraction written with it would stay one word and hide
  its negation; write U+0027 or U+2019) and at most 4 KiB of UTF-8.

**Critical classes.** Derived from the text, never declared: `negation` for a
word among no, not, never, nothing, none, nobody, nowhere, neither, nor and
cannot, or one ending in n't; `numeral` for a decimal digit written in the
text. Number words are not numerals here. The corpus holds at least 10 of
each.

**Listening set.** At least 30 sentences whose candidate audio listeners
transcribe, the same number from each stratum, with at least three of each
critical class.

**Queued sequences.** At least 100, each a long first segment (`t101to200`)
and one or two short followers (`t1to20`), both lengths present. A sequence of
three reaches the three-outstanding-segment bound. No two sequences have the
same segments. Whether a run paces them for playback is the benchmark's
choice.

## Checking, counting, recording

```sh
tools/validation/tts_corpus.py check tools/validation/corpus/tts-text-en-us.json
tools/validation/tts_corpus.py check tools/validation/corpus/tts-text-en-us.json \
  --source cmu-arctic=cmuarctic.data --source libritts-r=test_clean.tar.gz
tools/validation/tts_corpus.py summary tools/validation/corpus/tts-text-en-us.json
```

`check` needs only the standard library. With `--source`, it also compares
each file's SHA-256 with the corpus's pin and every sourced sentence's text
with the source's, byte for byte; once one source is given, every fetched
source must be.

`count` and `reference` import kokoro, so they run in the speech runtime's
locked environment, and refuse when the installed kokoro, misaki, spaCy or
`en_core_web_sm` is not the lock's.

```sh
tools/validation/tts_corpus.py count CORPUS --model-config MODEL/config.json [--write OUT]
tools/validation/tts_corpus.py reference CORPUS --model-dir MODEL --out DIR [--threads 4]
```

- `count` re-phonemizes every sentence and refuses on any count that differs,
  or on a phoneme the model's vocabulary lacks (the runner refuses those as
  unsupported). `--write` writes the corpus with the counts, strata, critical
  classes and tokenizer versions made here instead, once the check accepts
  it; a recount the check refuses writes nothing.
- `reference` is the listening review's reference arm: every sentence
  synthesized by kokoro's own pipeline, `KPipeline(...)(text, voice, speed=1)`,
  from the provisioned model and voice in float32 on the CPU. The model files
  must have the digests `packaging/provisioning/manifest.json` pins for the
  Kokoro bundle. The vocoder draws random phase and noise, so the tool seeds
  torch before each clip with the first eight bytes of
  SHA-256(`<corpus version>/reference/<sentence id>`). Each clip is written as
  a 24 kHz 16-bit WAV, converted from float as the runner converts, into a new
  directory outside the repository, with `reference-record.json`
  ([`config/schemas/tts_reference_record.json`](../../config/schemas/tts_reference_record.json)):
  the corpus file's digest; the engine, with the tokenizer versions under
  the corpus's field names, the lock's digest and the `libespeak-ng1`
  version; the model digests; and per clip the
  seed, sample and clipped-sample counts, both digests, and whether the
  pipeline's phonemes equal the runner's. A clip whose pipeline returns more
  than one chunk, or whose count here differs from the corpus's, is refused,
  and the record is checked against its schema before it is written. The
  directory is built under a temporary name beside `--out` and renamed into
  place at the end, so a run that fails leaves nothing behind and can be
  rerun.
  The same seed reproduces a clip only with the same `--threads` on the same
  machine and build: the thread count decides how the CPU kernels split
  their sums, and the kernels differ across machines. The record carries the
  thread count torch used; a record identifies the clips it describes and is
  not a target for other machines.

Exit status: 0 valid, 1 refused, 2 unreadable.
