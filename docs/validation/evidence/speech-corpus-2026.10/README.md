# Speech evaluation corpus 2026.10

[`manifest.json`](manifest.json) is the speech evaluation corpus `2026.10`. It names every audio file by digest, with its source, license, pool, stratum and speech labels, and the recipe parameters of every rendering. [`tools/validation/speech_corpus_build.py`](../../../../tools/validation/speech_corpus_build.py) built it from the plan [`plan.json`](plan.json). [`labels.jsonl`](labels.jsonl) holds the forced-alignment labels its selection read: one line per examined file, with the aligner's score, its frame stride and the file's band-energy onset, and no text. With the sources, `build --labels labels.jsonl` selects the same corpus without re-running the aligner. [docs/validation/speech-corpus.md](../../speech-corpus.md) describes the tools.

The audio is not in this repository; the project holds it privately.

This filing holds English. Arabic is added to the same corpus and manifest next, so this manifest's SHA-256 is not yet the corpus's identity. The identity is the digest of the manifest that holds both languages. Adding Arabic leaves every English entry as it is: seeds, noise choices, SNR assignment and fixtures are all per clip, per language or per pool.

## Contents (English)

| Pool | Bin | Clean clips | Sources |
| --- | --- | --- | --- |
| held out | under 1 s | 150 | Speech Commands v0.02 test list |
| held out | 1 to 5 s | 300 | LibriSpeech test-clean 150, test-other 150 |
| held out | over 5 to 30 s | 600 | LibriSpeech test-clean 160, test-other 160, FLEURS en_us test 280 |
| tuning | under 1 s | 50 | Speech Commands v0.02 validation list |
| tuning | 1 to 5 s | 100 | LibriSpeech dev-clean |
| tuning | over 5 to 30 s | 100 | LibriSpeech dev-clean 50, FLEURS en_us dev 50 |

- **Renderings.** Every clean clip is also rendered `noisy16k` and `telephony8k`, for 3,900 files in all.
- **Held out:** 1,050 clips, 19,041 reference words as `tools/validation/speech_quality.py` normalizes them, and 922 distinct references.
  - 223 speakers are identified. FLEURS identifies no speakers.
  - 100 files are semantic fixtures, 25 taken for each class, read as the quality scorer reads them. A fixture lists every class it meets, so by class there are:
    - 38 negation;
    - 42 numerals;
    - 40 repetition;
    - 56 window seam.
- **Noisy renderings** are balanced across 5, 10, 15 and 20 dB within every pool and bin. 45 mixtures were scaled as a whole to stay within 16 bits.

## Sources and their licenses

| Source | Authors | License | Where it is stated |
| --- | --- | --- | --- |
| LibriSpeech test-clean, test-other, dev-clean (OpenSLR 12) | V. Panayotov, G. Chen, D. Povey, S. Khudanpur | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) | `LICENSE.TXT` in each archive |
| FLEURS en_us test and dev (Hugging Face `google/fleurs` at `70bb2e84`) | A. Conneau, M. Ma, S. Khanuja, Y. Zhang, V. Axelrod, S. Dalmia, J. Riesa, C. Rivera, A. Bapna (Google) | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) | the dataset card at that revision |
| Speech Commands v0.02 | P. Warden (Google) | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) | `LICENSE` in the archive |
| MUSAN, the free-sound noise | D. Snyder, G. Chen, D. Povey | public domain ([OpenSLR 17](https://www.openslr.org/17/)) | `noise/free-sound/LICENSE` in the archive |

- **Archive digests.** Every archive was downloaded once and recorded by SHA-256. The digest is the source's `revision` in the manifest.
- **Publishers' digests.**
  - The LibriSpeech archives match the md5 sums OpenSLR publishes.
  - The FLEURS archives match the SHA-256 that Hugging Face lists for the files at the pinned revision.
- **Withheld files.** Each source's `selection` counts the files each rule withheld. For example, among Speech Commands clips:
  - 214 with 10 or more full-scale samples;
  - 169 whose aligner score is below 0.2;
  - 131 whose labelled speech is shorter than 120 ms;
  - 16 whose labelled speech comes within 20 ms of the file's edge;
  - 568 whose labelled onset is more than 50 ms from the band-energy onset.

  Among the other sources:
  - 26 FLEURS en_us test files whose first or last word did not align;
  - 25 LibriSpeech files longer than 30 s;
  - 40 LibriSpeech files whose text is in both pools;
  - 2 LibriSpeech test files whose labelled speech is under 1 s and whose onset is off the band-energy onset.

  The label rules count only the files the selection examined. The noise source counts:
  - 653 recordings shorter than 32 s;
  - 100 recordings after the first 90.

## Labels

- The speech onset and end come from CTC forced alignment of each reference with torchaudio 2.5.1 `WAV2VEC2_ASR_BASE_960H`. That model is wav2vec 2.0 under MIT; the weights file's SHA-256 is `488fd4f16de84438ffc945334278c1b9fb9b7159a806c1080b16111a958c945d`.
- The aligner is not the product's voice activity detector.
- **Frames.** A label sits on the model's 20 ms frames: frame `k` is sample `320 * k`. The aligner first placed frame `k` at `k * samples / frames`, up to 20 ms late on a 1 s clip. Its labels were converted exactly with `speech_corpus_build.py restride-labels`: each recorded sample maps back to one frame index. Labels for the further Speech Commands clips the refill needed come from the corrected aligner.
- **Label rules.** The plan's `label_rules` withhold a label:
  - whose aligner score, the mean probability of its aligned tokens, is below 0.2;
  - whose labelled speech is under 120 ms;
  - under 1 s only, whose onset is more than 50 ms from the band-energy onset. That onset is the first of three 10 ms frames whose 300-3400 Hz energy is within 25 dB of the clip's loudest frame.
- **Why the energy check covers only the under-1 s bin.** Over 1 s, breaths, clicks and handling noise before speech fall inside the band, and the crude onset cannot tell them from speech. Among the chosen clips over 1 s, the energy onset is more than 50 ms from the label on:
  - 111 of 620 LibriSpeech test clips;
  - 111 of 330 FLEURS clips;
  - 25 of 150 LibriSpeech dev clips.

  Ends are not cross-checked: a CTC aligner ends a word early, and no gate times from the end.
- **What the energy check removes.** The aligner places the onset of a vowel- or glide-initial keyword late, and the check withholds those labels. The held-out under-1 s set therefore has:
  - no "one", "on" or "sheila";
  - one "off" and one "stop";
  - three "eight".

  Every other keyword has 4 to 8 clips; the cap is 8. The under-1 s latency stratum under-represents vowel-initial onsets. Tuning "up" is absent from the tuning set: of its 112 examined clips, 111 fail the score or span floor and the energy check withholds the last.
- **The listen-check.** Every label reads `human_verified: false`. By the project owner's decision of 2026-10-17, a listener checks 50 held-out clips per language by ear, with a 50 ms flag:
  - at least 15 are under 1 s;
  - of the 35 longer clips, half come from those whose filed band-energy onset is more than 100 ms from the label, in a seeded order, and half at random;
  - if more than 2 of a source's doubtful half are off by more than 50 ms, the withholding for that source is ruled again, probably at 100 ms.

  The plan states this as `listen_check`. Its result goes back through the build as a verification file. Until it is recorded, no label is human-verified.

## Noise screen

- **The pool.** The noise pool is the first 32 s of 90 MUSAN free-sound recordings: 60 held out and 30 tuning, chosen in a seeded order.
- **How it was screened.** Nobody listened to it. Each file was decoded greedily by the English aligner's model (`speech_corpus_align.py --screen`).
- **The rule.** A file whose decode has 10 or more letters is excluded.
- **The result.**
  - 82 files decoded to fewer than 10 letters and no English word of three or more letters.
  - 8 files gave 16 to 154 letters and are excluded, with the count as the reason. Some of those decodes are repetitive syllables, such as "MA MA MA" and "WIT WIT"; the rule excludes them all the same.
  - All 82 remaining files are used.

## Disclosures

- **Contamination.**
  - Every source is a public benchmark.
  - Whether any of them was in the speech model's training data is unknown: its model cards name no training sources.
  - Absolute word error rates on this corpus may therefore read better than on unseen speech.
  - A difference between batch and streaming decoding of the same clips is unaffected.
- **Speakers.** FLEURS has no speaker identifiers, so its speakers are not counted, and its dev-to-test speaker overlap is unknown.
- **Pools.**
  - LibriSpeech's and Speech Commands' splits keep speakers apart.
  - Speech Commands' keywords are divided between the pools, so no reference text is in both.

## Checks run on the audio

- **Determinism.** The filed tools built the English corpus twice from the same sources and labels: once alone, for this filing, and once with the Arabic part added. The 3,900 English files and the noise files both use are byte-identical by SHA-256, and so are their manifest entries and filed labels. A third build, after the filed labels gained each label's frame stride, wrote the same audio and manifest as the checked one below, and its `labels.jsonl` is the filed one.
- **Full check.** `tools/validation/speech_corpus.py check manifest.json --root CORPUS --rebuild --frozen` ran on the audio on 2026-10-17 with the filed tools. It reported "valid: 3900 files, audio rebuilt" and exited 0:
  - it read and hashed every file, and found no file under the root that the manifest does not list;
  - it rebuilt every noisy and telephony rendering from its clean file and compared the bytes and the recorded parameters;
  - it ran as one Python process under `ulimit -v 4000000` (KiB), took 19 min 25 s, and peaked at 102 MB resident.
- **Telephony level cross-check.** `sv56demo` is from ITU-T STL v2024 (commit `5a9febb4`), built locally; it is not distributed. On the levelled 16 kHz signal of the 1,300 telephony speech renderings it measured:
  - a median active speech level of -26.1 dBov;
  - 5th to 95th percentiles of -28.6 to -25.4 dBov;
  - a range of -31.3 to -23.5 dBov.
  
  The recipe levels the labelled span's RMS to -26 dBov, which a P.56 active level only approximates. The tool records the measurement and sets no threshold on it.
- **G.711 encoding.** The encoder is the one `test/validation/candidate_qualify_test.py` checks against the STL reference files for every 16-bit input. It maps each sample on its own, so that check covers every rendering, and no per-file run of `g711demo` was needed.
