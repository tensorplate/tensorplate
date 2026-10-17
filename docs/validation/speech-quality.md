# Speech evaluation quality scoring

`tools/validation/speech_quality.py` scores the final transcripts of a speech
evaluation run against the references of a
[speech evaluation corpus](speech-corpus.md) and writes a quality record
([`config/schemas/speech_quality_record.json`](../../config/schemas/speech_quality_record.json)).
It reads text only: no audio and no model.

A run has two arms over the same pool of the corpus:

- the **streaming** arm, which is the product's speech session;
- the **batch** arm, which is the same model and configuration decoding each
  whole file.

Each arm is a transcripts file
([`config/schemas/speech_transcripts.json`](../../config/schemas/speech_transcripts.json))
naming the manifest by the SHA-256 of its bytes. It holds one entry per file of
the pool, speech and silence alike: either the file's finals in order, or the
protocol error code the file failed with.

```sh
tools/validation/speech_quality.py score manifest.json \
  --streaming streaming.json --batch batch.json \
  --references references.json --output quality.json
```

A reference whose source does not allow republishing it is held in the
manifest by digest only. Its text comes from `--references`, a JSON object
from each text's SHA-256 to the text, which is checked against the digest and
kept privately.

Exit status: 0 every gate passed, 1 refused, 2 unreadable, 3 the record was
written and a gate failed or had too few samples.

## Refusals

The scorer refuses rather than scores when:

- the manifest fails the corpus check;
- an arm is the other arm's file, names another manifest, or ran another pool;
- an arm misses a file of the pool, lists one twice, or lists a file outside the
  pool;
- a failed file's error is not a protocol error code;
- a reference text is missing, or does not match its digest;
- a reference carries an Arabic presentation form, a control or a format
  character, or has no words once normalized;
- any text holds an unpaired surrogate.

## Normalizers

Both arms and the references go through the same normalizer, chosen by the
file's language. The punctuation step maps every character of Unicode general
category P to a space. Symbols such as `$`, `+` and `°` are not punctuation and
stay in their word.

| Language | Steps, in order | Identity |
| --- | --- | --- |
| `en` | NFC; full Unicode casefold; punctuation to a space; split on whitespace | `tp-wer-en-nfc-casefold-v1` |
| `ar` | NFC; remove tatweel (U+0640) and harakat (U+064B to U+0652 and U+0670); punctuation to a space; split on whitespace | `tp-wer-ar-nfc-dediacritized-v1` |

What the normalizers keep, on purpose:

- **Hamza and madda.** NFC composes them with their carrier into the letters
  أ إ آ ؤ ئ, whatever order a decomposed input had them in. NFC runs before
  tatweel is removed, so a tatweel written between a carrier and its hamza
  keeps them apart.
- **Letter variants.** Alef forms, ta marbuta and alef maksura are not folded:
  أ and ا, ة and ه, ى and ي stay different words.
- **Digits as written.** Western, Arabic-Indic and Extended Arabic-Indic digits
  are distinct, so `3`, `٣` and `۳` are three different words. The Arabic
  decimal and thousands separators (٫ ٬) are punctuation, so `٣٫٥` is two
  words.
- **The n't contraction.** The apostrophe of `don't` becomes a space, leaving
  the words `don` and `t`.

The normalizers expand no numbers. A reviewed list of spoken-number
expansions per corpus is not part of this tool yet.

NFC does not fold Arabic presentation forms (U+FB50 to U+FDFF, U+FE70 to
U+FEFF) into the letters they stand for, and no step removes control or
format characters such as U+200F. A reference carrying either is refused as an
authoring error. Tab, line feed, vertical tab, form feed and carriage return
are whitespace and allowed; every other control character is refused, even
where Python would split words on it. In a hypothesis they are scored as written, so ﻻ does not
match لا, and the record counts the files whose finals carry them
(`hypothesis_text`). Character properties and NFC come from the Python
Unicode database, whose version the record states.

## Word error rate

Each file's normalized reference and hypothesis are aligned at minimum edit
distance. On a tie, the backtrace from the end of both lists takes a match or
substitution first, then a deletion, so each pair of word lists has exactly one
alignment. For example, the reference `a a` against the hypothesis `a` deletes
the first `a`.

A stratum is one language and codec. Its micro word error rate is the sum of
its files' substitutions, deletions and insertions, divided by the sum of
their normalized reference words. Only finals count: an arm's finals for a file
are joined with a space, and a failed file is an empty prediction. Failures are
also counted per arm and error code.

The raw rate scores the same texts split on whitespace with no normalization.
It is reported and not gated.

Each stratum is also reported, ungated, in three breakdowns: by source, by
duration bin and, for `noisy16k`, by signal-to-noise ratio.

## Gates

| Gate | Pass | Minimum samples |
| --- | --- | --- |
| Streaming word error rate, `en` | at most 15% clean, 25% noisy, 25% telephony | 100 files per stratum |
| Streaming word error rate, `ar` | at most 25% clean, 35% noisy, 35% telephony | 100 files per stratum |
| Streaming minus batch, per stratum | at most 2 percentage points | as above |
| Corpus size, per language | met by the minimums | 500 clean recordings and 10,000 normalized reference words |
| Silence | no words in any streaming final of a silence file | 100 silence recordings |
| Semantic fixtures, per language | no critical regression | 50 clean recordings with a semantic class |

A gate below its minimum reads `insufficient`, never `pass`. A ratio gate
below its minimum is `insufficient` even when its rate is over the ceiling,
because more samples could change the rate. A zero-tolerance gate (silence,
semantic) reads `fail` on any violation at any sample count. The record's
verdict is `fail` if any gate failed, `insufficient` if any other gate was
insufficient, and `pass` otherwise.

The batch arm is the reference the streaming arm is judged against, so a
batch file that failed scores empty and can only make streaming look better.
A stratum in which the batch arm failed any file is therefore `insufficient`
unless its ceiling or its distance already fails. Likewise, a language's
semantic gate is `insufficient` while the batch arm failed any of its fixture
files, unless a regression already fails it. A silence recording counts
toward the silence minimum only when none of its files failed in the streaming
arm.

A record reads the pool its arms ran. Release evidence is a `held_out` run;
the same gates on a `tuning` run are only a check of the tuning set.

The rates are compared in integers: errors × 100 against the ceiling × words,
so a stratum exactly at its ceiling passes.

A silence file has no language. Its words are counted with whichever
normalizer leaves more of them, so a final made only of tatweel still counts.
The batch arm's silence words are reported and not gated.

## Semantic fixtures

A manifest file may carry the classes `negation`, `numerals`, `repetition` and
`window_seam`. Derived renderings carry their clean file's classes, and each
rendering is checked. A regression is judged against the batch arm on the same
file, never against the reference alone. A word both arms get wrong is not a
regression.

**Negation, numerals and repetition.** The class marks critical words in the
reference and in each hypothesis:

- **negation.** `en`: no, not, never, nothing, none, nobody, nowhere,
  neither, nor, cannot, and the `t` an n't contraction leaves: a `t` right
  after a word ending in `n`, so `don't` counts and `T-shirt` does not. `ar`:
  لا لم لن ليس ليست ما, also with و or ف joined in front.
- **numerals.** Any word containing a decimal digit, in any script, or a word
  of the language's number-word list. The Arabic list also matches with و or
  ف joined in front.
- **repetition.** A word equal to one of the three words before it, and that
  earlier word.

The semantic minimum counts fixtures of all four classes together; the record
reports each class's count beside it. The manifest check refuses a class
listed twice on one file.

A regression is either of:

- a critical reference word the batch arm matched and the streaming arm did not
  (`lost`, with its position);
- a critical word the streaming arm left unmatched, beyond those the batch arm
  left unmatched, counted as a multiset (`added`).

An unmatched critical word is one the alignment inserted or substituted in. So
a substituted `never` for `not` is both a lost and an added word.

**Window seams.** The class marks utterances long enough to span more than one
streaming decode window. A regression is a run of two or more
consecutive words that the streaming arm:

- deleted where the batch arm deleted none of them (`lost`, at the first
  word's position); or
- inserted at a place where the batch arm inserted nothing (`added`, at the
  number of reference words before the run).

Substitutions do not count. The streaming final is currently one decode of the
whole utterance, not a sequence of windows. It differs from batch decoding
word by word for reasons that have nothing to do with seams, so comparing every
word would flag ordinary decode differences. Dropped and duplicated runs are
the signature of a seam fault.

Every regression is listed in the record with its file, class, kind, position,
number of words and the words themselves. Where the file's reference is held by
digest only, the words are left out (`null`), so the record can be published
without republishing a reference its source keeps private.

## Not scored here

- **Segment timestamps.** The boundary error of segment timestamps against
  hand-aligned segments waits for the rule that matches hypothesis segments to
  reference segments.
- **Model and configuration identity.** The scorer binds each arm to the
  corpus manifest by digest. It does not check that both arms ran the same
  model and configuration.
