# Kokoro candidate qualification on an NVIDIA L4, 2026-10-08

Three runs of `tools/validation/candidate-qualify.py` over the
`tts-kokoro-candidate` bundle (Kokoro-82M, `float32` on CUDA, language
`en-US`, voice `af_heart`), recorded on a `g2-standard-8` with one
NVIDIA L4 running the published `v0.3.1-rc.2` packages. These are
candidate records: each says `presented_as: candidate` and
`production_evidence: false`, and none is release evidence for any
support row.

**Run 1 is `fail`, runs 2 and 3 are `pass`, and all three are summarized
as recorded.** In every run each fixture request returned `ok`. In runs 2
and 3 every judged negative case returned its typed code, the teardown
by rollback restored the predecessor and all four memory windows were
measured. The two cases that did not hold on
[2026-10-01](../speech-candidate-kokoro-l4-2026-10-01/README.md), an
entry selecting an undeclared voice and a deploy while a ballast holds
the device's memory, returned `unsupported` and `oom_error`; against the
first release candidate both returned `timeout`. Run 1's failure is its
`oom_at_load` case, where the deploy succeeded under the ballast the
operator had sized; it is described under
[Run 1](#run-1).

This directory holds this summary and nothing else. The three records,
their memory observations and logs, and the session files that show the
build and the host's settings are kept privately, and their SHA-256
digests are [listed below](#digests) so that a copy can be checked
against this page. Every number and outcome here is read from one of
those files; where it is not the run's own `record.json`, the file is
named. None of them needed a synthetic form from the
[evidence README](../v0.2.1/README.md): they carry no host, account or
cloud identifier and are as recorded. Generated audio is not among them.
A record carries each output's sample count, duration and digest, never
the text or the audio.

## The three runs

|  | Run 1 | Run 2 | Run 3 |
| --- | --- | --- | --- |
| Result | `fail` | `pass` | `pass` |
| Recorded (UTC) | 15:39 | 15:49 | 15:59 |
| Fixtures × requests each | 14 × 46, all `ok` | 14 × 47, all `ok` | 14 × 47, all `ok` |
| Candidate deploy, wall | 11.4 s | 11.4 s | 11.4 s |
| Runner load | 9.05 s | 8.98 s | 8.95 s |
| First request after the deploy | 862 ms | 852 ms | 848 ms |
| Ballast held, bytes | 21,300,000,000 | 21,700,000,000 | 21,700,000,000 |
| `malformed_text_blank` | matched `config_invalid` | matched `config_invalid` | matched `config_invalid` |
| `malformed_text_dtype` | matched `shape_mismatch` | matched `shape_mismatch` | matched `shape_mismatch` |
| `unsupported_voice` | matched `unsupported` | matched `unsupported` | matched `unsupported` |
| `corrupt_artifact_digest` | matched `load_failed` | matched `load_failed` | matched `load_failed` |
| `cancel_during_request` (recorded, never judged) | `unsupported`, HTTP 501 | `unsupported`, HTTP 501 | `unsupported`, HTTP 501 |
| `oom_at_load` | **mismatched: the request succeeded** | matched `oom_error` | matched `oom_error` |
| Status after the negatives | **checks failed: agent `ready`, no last error, `qualify-kokoro-1-oom` active** | as expected: agent `degraded`, last error `oom_error` | as expected: agent `degraded`, last error `oom_error` |
| Teardown by rollback | **failed: restored `qualify-kokoro-1`** | ok in 2.1 s, predecessor restored | ok in 2.1 s, predecessor restored |
| Memory windows | 3 measured; `after_teardown` not run | all four measured | all four measured |

Each sentence was requested three times for timing and then in turn
until the load window's sampler finished: 46 times in all in run 1, 47 in
runs 2 and 3. The first request after the deploy is the first sentence's
first timing sample, timed by the client. The ballast size is in each
run's `run.log`, not in its record.

In runs 2 and 3 the three deploys built to fail did so with the code
expected. `unsupported_voice` was answered `unsupported`, "serving worker
failed to start: the selected language or voice is not declared", after
2.3 s. `corrupt_artifact_digest` was refused by the agent's integrity
check as `load_failed`. `oom_at_load` was answered `oom_error`, "serving
worker failed to start: the Kokoro model could not be loaded", after
10.7 s and 10.6 s (`run.log`). The status snapshot that follows found the
agent `degraded` with that code as its last error and the candidate
still active, and the rollback issued next was accepted. On 2026-10-01
the first and the third of those deploys returned `timeout` and the
rollback was refused as `busy`.

`cancel_during_request` is recorded and never judged: the Python-backed
worker answers the asynchronous route with HTTP 501.

### Run 1

`oom_at_load` deploys the candidate a second time while a ballast process
holds device memory, and expects `oom_error`. The ballast's size is an
argument the operator passes (`session/scripts/30-qualify.sh`). For run 1
it held 21,300,000,000 bytes (`run-1/run.log`), and under it the deploy
succeeded in 11.4 s: the record's outcome is `deployed as
qualify-kokoro-1-oom` with the reason `the request succeeded`.

The record's three other failures follow from that deploy having become
the active deployment:

- the status snapshot found the agent `ready` with no last error and
  `qualify-kokoro-1-oom` active and serving, where it expects the
  candidate's own deployment: its `active_deployment_matches` and
  `health_active_model_matches` checks failed;
- the rollback then restored `qualify-kokoro-1`, the candidate, where
  the teardown expects the predecessor, so the teardown is `failed`;
- the `after_teardown` window did not run. Its reason reads `pending
  hardware`, which is the tool's default for a window that has not run.

Runs 2 and 3 were given 21,700,000,000 bytes, and the same deploy was
refused. No window samples the device during that deploy, so the records
do not show how much memory was free under either ballast. In run 1, as
in the others, every fixture request returned `ok` and the four other
judged negatives matched.

## Fixtures

Median real-time factor (the runner's `synthesize` time over the
generated audio's duration) and median exchange time at the client, over
the requests of each sentence:

| Fixture | Run 1 RTF | Run 1 exchange | Run 2 RTF | Run 2 exchange | Run 3 RTF | Run 3 exchange |
| --- | --- | --- | --- | --- | --- | --- |
| `en-US-short-01` | 0.046 | 76 ms | 0.045 | 75 ms | 0.045 | 74 ms |
| `en-US-short-02` | 0.051 | 74 ms | 0.050 | 73 ms | 0.049 | 72 ms |
| `en-US-short-03` | 0.041 | 78 ms | 0.040 | 76 ms | 0.040 | 75 ms |
| `en-US-short-04` | 0.037 | 78 ms | 0.036 | 76 ms | 0.036 | 75 ms |
| `en-US-short-05` | 0.035 | 78 ms | 0.034 | 76 ms | 0.034 | 76 ms |
| `en-US-medium-01` | 0.014 | 92 ms | 0.014 | 89 ms | 0.014 | 89 ms |
| `en-US-medium-02` | 0.014 | 93 ms | 0.014 | 91 ms | 0.014 | 90 ms |
| `en-US-medium-03` | 0.014 | 94 ms | 0.014 | 91 ms | 0.014 | 91 ms |
| `en-US-numerals-01` | 0.015 | 92 ms | 0.015 | 89 ms | 0.015 | 89 ms |
| `en-US-numerals-02` | 0.013 | 104 ms | 0.012 | 102 ms | 0.012 | 102 ms |
| `en-US-negation-01` | 0.019 | 86 ms | 0.018 | 85 ms | 0.018 | 84 ms |
| `en-US-punctuation-01` | 0.017 | 89 ms | 0.017 | 87 ms | 0.017 | 86 ms |
| `en-US-long-01` | 0.012 | 132 ms | 0.012 | 130 ms | 0.012 | 130 ms |
| `en-US-long-02` | 0.012 | 132 ms | 0.012 | 131 ms | 0.012 | 130 ms |

No request failed and none exceeded the 8,000 ms request timeout. The
longest exchange of each run was its first request. The sentences are
the fourteen the fixture inventory pins, with the digests the records
carry.

## Memory

Sampled maxima in MiB over a 60 s window at a 1 s interval, from the
platform sampler. They are not continuous-time peaks. "Sidecar" is the
Python sidecar process; "consumed" is the whole device or guest, capacity
minus available.

| Window | Measure | Run 1 | Run 2 | Run 3 |
| --- | --- | --- | --- | --- |
| Predecessor idle | sidecar, device memory | 248 | 248 | 248 |
| Predecessor idle | device consumed | 726 | 980 | 980 |
| Predecessor idle | sidecar, resident memory | 718 | 718 | 718 |
| Predecessor idle | guest consumed | 1,950 | 2,396 | 2,428 |
| Candidate warm idle | sidecar, device memory | 552 | 552 | 552 |
| Candidate warm idle | device consumed | 1,284 | 1,284 | 1,284 |
| Candidate warm idle | sidecar, resident memory | 1,292 | 1,293 | 1,293 |
| Candidate warm idle | guest consumed | 3,174 | 3,258 | 3,280 |
| Candidate under load | sidecar, device memory | 1,152 | 1,152 | 1,152 |
| Candidate under load | device consumed | 1,630 | 1,630 | 1,630 |
| Candidate under load | sidecar, resident memory | 1,877 | 1,889 | 1,878 |
| Candidate under load | guest consumed | 2,900 | 2,974 | 2,977 |
| After the teardown | sidecar, device memory | not run | 248 | 248 |
| After the teardown | device consumed | not run | 1,884 | 1,884 |
| After the teardown | sidecar, resident memory | not run | 718 | 719 |
| After the teardown | guest consumed | not run | 3,843 | 4,038 |

The sidecar held at most 552 MiB of device memory warm and idle and
1,152 MiB under the tool's own load in all three runs, the values
recorded on 2026-10-01. After the teardown the sidecar is the
predecessor's again. The device-wide maximum after the teardown is
higher than in the predecessor window before it in runs 2 and 3; the
samples attribute device memory to the sidecar only, so the records do
not say what else held it.

## Build, host and settings

- **Packages.** The session's pass report (`session/pass/pass-report.json`)
  records the commit the candidate set was built from,
  `5e6b54c1e706df2af7a150283eedafa706a24db5`, and the SHA-256 of the set's
  checksum file, `e3c2a51e3aa057e592a6c60672cfa243cafea2aee86f7df873924712e12f9f18`.
  The published `v0.3.1-rc.2` release lists the same digest for its
  `SHA256SUMS` asset (read from the release on 2026-10-10). Each record
  reports CLI version `0.3.1-rc.2` and the same commit for the tool.
- **Speech runtime.** No release attaches the speech runtime packages, so
  the pass built the eight packages on the host from a clone of that
  checkout at version `0.3.1~rc.2-1` and installed them, in 1,142 s (the
  report's `speech-family` step). `session/raw/packages-final.txt` lists
  the fourteen TensorPlate packages installed at that version. Each record
  reports Kokoro 0.9.4 and PyTorch 2.13.0+cu129, loaded as `float32` on
  `cuda`.
- **Host** (`session/raw/host-facts.txt`). Ubuntu 24.04.5, kernel
  7.0.0-1013-gcp, NVIDIA driver 580.178.04, one NVIDIA L4 reporting
  23,034 MiB, 8 processors. The agent resolved the platform row
  `ubuntu2404-x86-l4-g2s8` and reported it validated
  (`session/pass/logs/05-agent-environment.log`).

Four things make these runs not a default install:

1. **Three interim agent settings.** The candidate bundles name no runner
   profile, so nothing selects the speech runtime environment for them
   ([rolling validation](../../rolling-validation.md#bundles-that-name-no-runner-profile)).
   The pass appended three variables to the agent's environment file and
   restarted the agent (the report's `agent-environment` step and its
   log), and the runs were made with them: `TP_PYTHON_PYTORCH_EXECUTABLE`,
   the speech runtime's interpreter;
   `TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS=120000`, a raised sidecar startup
   deadline; and `LD_LIBRARY_PATH`, the speech runtime's cuBLAS directory.
   No run was made without them, so these records do not show which of
   the three this host needs.
2. **Host-built speech runtime packages**, as above.
3. **PyTorch in the system interpreter.** The host preparation
   (`session/scripts/10-prep.sh`) installs it there when the image has
   none, and recorded 2.14.1+cu130 with CUDA available
   (`session/raw/torch-system.txt`). The runs used the speech runtime's
   own engines, as each record's `device_facts` shows.
4. **The qualification command** (`session/scripts/30-qualify.sh`): a
   CUDA fixture bundle, `test/models/bundles/v0_1/x86_cuda_smoke`, as the
   predecessor, whose sidecar holds device memory (248 MiB in the first
   and last windows); `--window 60s`;
   `--agent-timeout-ms 120000` and `--deploy-wait-timeout-ms 300000`; and
   a ballast of the size the operator passed, held by
   `tools/validation/vram_ballast.py` under the speech runtime's
   interpreter.

## Digests

SHA-256 of each file, in `shasum -a 256` form. `run-1/` to `run-3/` are
the three runs' evidence directories as the tool wrote them, without
`outputs/`; `session/` holds files of the session both candidates' runs
share, under the paths they had in the raw capture.

```text
c596aeae783cffdb750155261214559aa473c20750bdb34c57dee6ee33a2a8f4  run-1/memory/candidate_load-device_vram.jsonl
7e542a47a912ce835f0285629b50cb421121816f56a3bf04921af6c2936648f9  run-1/memory/candidate_load-guest_ram.jsonl
05e3e58bbacec175072b159c9775cf91f28bd0bf6666e0e7077b1b7be8a88144  run-1/memory/candidate_warm_idle-device_vram.jsonl
d6a9ee58feb8d6bf3e77ae068b518f46dfe0603480b64d8c8f23ca6b8db947c3  run-1/memory/candidate_warm_idle-guest_ram.jsonl
5c1f5a64ae08f91ec1ca1d8ae74f929905801a5d77a6f882618dce079df37131  run-1/memory/predecessor_idle-device_vram.jsonl
14678c333bcdec9461ca2324ce0c3c1bcc65a0b1b2d0183e836ebcdc8ae4e944  run-1/memory/predecessor_idle-guest_ram.jsonl
e11a5dee8774c813f320ebbb87c94d81f3e68de5ad9599ed04be7ced545129de  run-1/record.json
db9646d2368de620f9718c6a181e2081c0dfa61855fd1a8f2096e5394eb7280b  run-1/run.log
11163c7f71d291d81c01562cbea1df7d344edd47aa971144aa45e6d3d76ae60d  run-2/memory/after_teardown-device_vram.jsonl
b1a97cef14357f51c6bae1f6d000a2179ae6a7ab9be2e607c789719c93437413  run-2/memory/after_teardown-guest_ram.jsonl
8d1c1d3033331f122e862a2af8f6c871a4f1e648266460e0ca07fe665d27e63f  run-2/memory/candidate_load-device_vram.jsonl
a93ce58ae25264259f24714b510fc8e2fae68c8da2597eae1d943e65875f4811  run-2/memory/candidate_load-guest_ram.jsonl
57a0da03e14bd3ee990e8ca9a02926190172c9b704922f2bbd6767e96d8fbbf7  run-2/memory/candidate_warm_idle-device_vram.jsonl
bdef8b78a411a7f7cf392b354b924819436e4ad509752dc62c9c6001519e56e2  run-2/memory/candidate_warm_idle-guest_ram.jsonl
a1554d536f601b041a896a387990b5689d52ea0605d0cd4dc68b4380c3eccc96  run-2/memory/predecessor_idle-device_vram.jsonl
1059eded4be67d8a402922e0fa5307d81eb9bb7ac0c6d7b79815e63982d10178  run-2/memory/predecessor_idle-guest_ram.jsonl
1030b0105ec0221c354e3f08bf94cfcd398ca16f1c3be64019fd82b2b14b17f9  run-2/record.json
848dfef5cd69a15c851d8862be84be02ae1110945edf1b0125e4d416b6d1861e  run-2/run.log
741a7491d38cb1d6ff6a2b69d1aa0f762b8260bdba7d5b0681c89b425ea5bd18  run-3/memory/after_teardown-device_vram.jsonl
2aa0fd4f0aecf9e33439dca98f03b1087b4801947dbabc17eea62d4e4d921afc  run-3/memory/after_teardown-guest_ram.jsonl
90993611fe3f8587c5c95ebd6bc0be0d2aafb42238e676af0844c48801954e8d  run-3/memory/candidate_load-device_vram.jsonl
ed1a44d4f6f9b983fb5bc50e946534bd3455018e4608c20ea4b657fa2304dd78  run-3/memory/candidate_load-guest_ram.jsonl
0282911d52f5b8b4ba75ef496257f1e788d6a25564f65898389d191197f1b3cb  run-3/memory/candidate_warm_idle-device_vram.jsonl
5453090e06236a229fe45dae7148d550bc600b0225d88bc04aaf80c50e97d47b  run-3/memory/candidate_warm_idle-guest_ram.jsonl
6e23ebd5f47f645ddd25b044485addfe203918abe9b7bba9e09cda8e6b4193ff  run-3/memory/predecessor_idle-device_vram.jsonl
add485b5ab09223ad45a52fdff8e4767a6bdb2ec218a0d6be06b1c7affa42be4  run-3/memory/predecessor_idle-guest_ram.jsonl
de7ed6065d53d5b6b2344056731443e7668c73c53ef87a13a99ebf8fd4b0eba4  run-3/record.json
c3b491541746b4e6dcb2175a5f051c8336aec1093e38ea3bb5624651dfc8ae26  run-3/run.log
d6f36597373cde89ed682a26326e93ebdb8af420e364bb73c63f75a060fc5b44  session/pass/logs/05-agent-environment.log
c70994420441c18ba7bdf2c96eaf7910bdb309599d10f16d403cf9a692d234be  session/pass/pass-report.json
5fe811bd9162c0a0875df8f33bf87df94907daa3097ed06b87aed04d32bfe491  session/raw/host-facts.txt
bb05d2b346d77cc9a4a2343cc21b798be9745d6bee2562976e70dfd8438bf620  session/raw/packages-final.txt
1bf366b66b79d30e4f8b20a6427b0bab3d9ae7765880e56807292b314bfa174e  session/raw/torch-system.txt
71c671e1735f4a92bedada0d7182a1f4c3f49826be918ef2cf4b6b56b8dfa4c0  session/scripts/10-prep.sh
d941daca3743e935ab125cca78d78a1b4b1d4229297ecb3ae02741a9613f0c2f  session/scripts/30-qualify.sh
```

## What these runs do not show

- Audio quality or intelligibility. No output is listened to or scored.
- Behaviour without the interim settings, or on a host whose speech
  runtime came from published packages.
- Concurrent requests, streaming, or the two candidates loaded together.
- A continuous-time memory peak, or a memory reserve.
- The device memory free at the moment a deploy under the ballast is
  refused or accepted.
