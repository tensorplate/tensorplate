# Whisper candidate qualification on an NVIDIA L4, 2026-10-08

Two runs of `tools/validation/candidate-qualify.py` over the
`stt-whisper-candidate` bundle (a CTranslate2 conversion of Whisper
large-v3-turbo through faster-whisper, `float16` on CUDA, languages `en`
and `ar`), recorded on a host of the platform row
`ubuntu2404-x86-l4-g2s8`, with one NVIDIA L4, running the published
`v0.3.1-rc.2` packages. These are candidate
records: each says `presented_as: candidate` and `production_evidence:
false`, and neither is release evidence for any support row.

**Both runs are `pass`.** Every fixture request returned `ok`, every
judged negative case returned its typed code, the teardown by rollback
restored the predecessor and all four memory windows were measured. The
case that did not hold on
[2026-10-01](../speech-candidate-whisper-l4-2026-10-01/README.md), a
deploy while a ballast holds the device's memory, returned `oom_error` in
both runs. Against the first release candidate it returned `timeout` in
run 1; run 2 was given no ballast and did not run the case.

This directory holds this summary and nothing else. The two records,
their memory observations and logs, and the session files that show the
build and the host's settings are kept privately, and their SHA-256
digests are [listed below](#digests) so that a copy can be checked
against this page. Every number and outcome here is read from one of
those files; where it is not the run's own `record.json`, the file is
named. None of them needed a synthetic form from the
[evidence README](../v0.2.1/README.md): they carry no host, account or
cloud identifier and are as recorded. Transcripts are not among them. A
record carries each output's length, segment and word counts, never the
text.

The same session's validation pass also ran the tool once on this
candidate. The pass report cited below records that run as `incomplete`;
it is not one of the runs summarized here.

## The two runs

|  | Run 1 | Run 2 |
| --- | --- | --- |
| Result | `pass` | `pass` |
| Recorded (UTC) | 15:43 | 15:54 |
| Fixtures × requests each | 5 × 55, all `ok` | 5 × 55, all `ok` |
| Candidate deploy, wall | 17.2 s | 16.9 s |
| Runner load | 6.81 s | 6.54 s |
| First request after the deploy | 693 ms | 565 ms |
| Ballast held, bytes | 19,466,813,440 | 19,466,813,440 |
| `malformed_audio_dtype` | matched `shape_mismatch` | matched `shape_mismatch` |
| `malformed_text_utf8` | matched `config_invalid` | matched `config_invalid` |
| `unsupported_language` | matched `unsupported` | matched `unsupported` |
| `corrupt_artifact_digest` | matched `load_failed` | matched `load_failed` |
| `cancel_during_request` (recorded, never judged) | `unsupported`, HTTP 501 | `unsupported`, HTTP 501 |
| `oom_at_load` | matched `oom_error` | matched `oom_error` |
| Status after the negatives | as expected: agent `degraded`, last error `oom_error` | as expected: agent `degraded`, last error `oom_error` |
| Teardown by rollback | ok in 2.4 s, predecessor restored | ok in 2.1 s, predecessor restored |
| Memory windows | all four measured | all four measured |

Each clip was requested three times for timing and then in turn until
the load window's sampler finished, 55 times in all. The first request
after the deploy is the first clip's first timing sample, timed by the
client. The ballast size is in each run's `run.log`, not in its record.

The deploy walls are warm-cache: the tool reads and hashes the whole
bundle before it deploys it. The pass report's `cold-deploy:stt-whisper`
step, which drops the page cache and then deploys the bundle at the same
path once, took 21.2 s in all.

The two deploys built to fail did so with the code expected.
`corrupt_artifact_digest` was refused by the agent's integrity check as
`load_failed`. `oom_at_load` was answered `oom_error`, "serving worker
failed to start: the Whisper model could not be loaded", after 16.6 s in
each run (the deploy `run.log` lists after its ballast line). The status
snapshot that follows found the agent `degraded` with that code as its
last error and the candidate still active, and the rollback issued next
was accepted. On 2026-10-01 that deploy returned `timeout` in run 1, and
the rollback issued next was refused with `not_ready`, "agent is busy
with an in-flight transaction". Run 2 of that day did not run the case,
and its teardown was ok.

`cancel_during_request` is recorded and never judged: the Python-backed
worker answers the asynchronous route with HTTP 501.

## Fixtures

Median real-time factor (the runner's `decode` time over the clip's
duration) and median exchange time at the client, over the 55 requests of
each clip. A median here is the tool's nearest-rank value, a sample that
occurred, not an interpolation:

| Fixture | Run 1 RTF | Run 1 exchange | Run 2 RTF | Run 2 exchange |
| --- | --- | --- | --- | --- |
| `en-clean-16k-01` | 0.033 | 193 ms | 0.033 | 193 ms |
| `ar-clean-16k-01` | 0.029 | 281 ms | 0.029 | 282 ms |
| `en-noisy-16k-01` | 0.033 | 191 ms | 0.033 | 192 ms |
| `ar-noisy-16k-01` | 0.029 | 285 ms | 0.029 | 285 ms |
| `en-telephony-8k-01` | 0.033 | 192 ms | 0.033 | 192 ms |

No request failed and none exceeded the 8,000 ms request timeout. The
longest exchange of each run was its first request. The clips are the
two the fixture inventory pins and the three it derives from them, with
the digests the records carry;
[the recipes doc](../../speech-candidate-recipes.md#the-clips-of-the-2026-10-01-run)
names their source and license. They are not in this repository.

## Memory

Sampled maxima in MiB over a 60 s window at a 1 s interval, from the
platform sampler. They are not continuous-time peaks. "Sidecar" is the
Python sidecar process; "consumed" is the whole device or guest, capacity
minus available.

| Window | Measure | Run 1 | Run 2 |
| --- | --- | --- | --- |
| Predecessor idle | sidecar, device memory | 248 | 248 |
| Predecessor idle | device consumed | 1,284 | 980 |
| Predecessor idle | sidecar, resident memory | 718 | 718 |
| Predecessor idle | guest consumed | 3,007 | 2,430 |
| Candidate warm idle | sidecar, device memory | 2,138 | 2,138 |
| Candidate warm idle | device consumed | 2,870 | 2,870 |
| Candidate warm idle | sidecar, resident memory | 199 | 200 |
| Candidate warm idle | guest consumed | 2,437 | 2,500 |
| Candidate under load | sidecar, device memory | 2,348 | 2,348 |
| Candidate under load | device consumed | 2,826 | 2,826 |
| Candidate under load | sidecar, resident memory | 642 | 642 |
| Candidate under load | guest consumed | 2,039 | 2,075 |
| After the teardown | sidecar, device memory | 248 | 248 |
| After the teardown | device consumed | 922 | 922 |
| After the teardown | sidecar, resident memory | 718 | 718 |
| After the teardown | guest consumed | 2,684 | 2,830 |

The sidecar held at most 2,138 MiB of device memory warm and idle and
2,348 MiB under the tool's own load, the values recorded on 2026-10-01.
After the teardown the sidecar is the predecessor's again. The
device-wide maximum is higher in the warm-idle window than under load in
both runs, and differs between the two predecessor windows; the samples
attribute device memory to the sidecar only, so the records do not say
what else held it.

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
  reports CTranslate2 4.8.2 and faster-whisper 1.2.1, loaded as `float16`
  on `cuda`.
- **Host** (`session/raw/host-facts.txt`). Ubuntu 24.04.5, kernel
  7.0.0-1013-gcp, NVIDIA driver 580.178.04, one NVIDIA L4 reporting
  23,034 MiB, 8 processors. The agent resolved the platform row
  `ubuntu2404-x86-l4-g2s8` and reported it validated
  (`session/pass/logs/05-agent-environment.log`). The machine type is
  not in a filed file; that row is defined for a `g2-standard-8`.

Four things make these runs not a default install:

1. **Three interim agent settings.** The candidate bundles name no runner
   profile, so nothing selects the speech runtime environment for them
   ([rolling validation](../../rolling-validation.md#bundles-that-name-no-runner-profile)).
   The pass report's `agent-environment` step and its log show three
   variables appended to the agent's environment file and the agent
   restarted: `TP_PYTHON_PYTORCH_EXECUTABLE`, the speech runtime's
   interpreter; `TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS=120000`, a raised
   sidecar startup deadline; and `LD_LIBRARY_PATH`, the speech runtime's
   cuBLAS directory. The pass began at 15:02 UTC and these runs were
   recorded from 15:43. A record does not carry the agent's
   environment, so no filed file shows the settings in place during
   these runs; that they were is the operator's statement. Nor do the
   records show which of the three this host needs.
2. **Host-built speech runtime packages**, as above.
3. **PyTorch in the system interpreter.** The host preparation
   (`session/scripts/10-prep.sh`) installs it there when the image has
   none, and recorded 2.14.1+cu130 with CUDA available
   (`session/raw/torch-system.txt`). Each record's `device_facts` names
   the engines that answered, CTranslate2 and faster-whisper, and no
   PyTorch version.
4. **The qualification command** (`session/scripts/30-qualify.sh`): a
   predecessor bundle from an installed path; `--window 60s`;
   `--agent-timeout-ms 120000` and `--deploy-wait-timeout-ms 300000`; and
   a ballast of the size the operator passed, held by
   `tools/validation/vram_ballast.py` under the speech runtime's
   interpreter. Each record gives the predecessor's bundle digest,
   `sha256:f52b460ea693fc2209ee787b8470bae7d13d52c0de88791052970e9a6537fa6e`,
   which is the digest the row's CUDA baseline records for the
   `x86-cuda-smoke` fixture bundle
   ([`cuda-deploy.json`](../v0.2.1/ubuntu2404-x86-l4-g2s8/cuda-baseline/cuda-deploy.json)).
   Its sidecar holds device memory: 248 MiB in the first and last windows of both runs.

## Digests

SHA-256 of each file, in `shasum -a 256` form. `run-1/` and `run-2/` are
the two runs' evidence directories as the tool wrote them, without
`outputs/`; `session/` holds files of the session both candidates' runs
share, under the paths they had in the raw capture.

```text
3304d9ec814343c4a20d3c696e845087ede774bf26151944e71dc8c6a0c79ec8  run-1/memory/after_teardown-device_vram.jsonl
c92f00bfaaf9269d8630ddba2311f2b842bcf1ad1f9eff74b3fe5a617fa70a58  run-1/memory/after_teardown-guest_ram.jsonl
b85216d65773243e162238053ce34db2940143a9dfd6039f6b8e320f3b8ce04f  run-1/memory/candidate_load-device_vram.jsonl
7182070d84627e108fe37df71884bff5d6c0e7e39bbd7183ff4fdabc6d39e23b  run-1/memory/candidate_load-guest_ram.jsonl
c96e9cbe4d13c7c94cd69bf160557d98c65df2ce27f28c69c2b69d01d45bbd85  run-1/memory/candidate_warm_idle-device_vram.jsonl
fde18c53bd8bf65e6652ef00faf3ca5333a049069cf24cc09c564ab7ba3f0cf2  run-1/memory/candidate_warm_idle-guest_ram.jsonl
ddf46cbc26db82630aea80ee2ea773d5617a4ebd46ca62540478b800c5633e9b  run-1/memory/predecessor_idle-device_vram.jsonl
16774708210404c11b4d7330e58ea7939ea8b4723f638ab1fa270092fa59384f  run-1/memory/predecessor_idle-guest_ram.jsonl
3608fc83240af422684943a997ed6d23a2da4095c5655966fbc0b5297eae3629  run-1/record.json
dc5b2bab3cb7e25c7c7137fd789a1903da12836322cd787f144e2ae2f1c5622d  run-1/run.log
5445fe01f30462f42f91201ccfab60449036dd9283b7eb7aff044fb6551f31e9  run-2/memory/after_teardown-device_vram.jsonl
7ceda5086f358a031953d6901495f1b7fe15ea5ea3358a645b8ef0a0eda9b733  run-2/memory/after_teardown-guest_ram.jsonl
01134b8abf4d0a2c7d36383d845c97dd7e7ed55770e99bde907de3ed80fa4fa0  run-2/memory/candidate_load-device_vram.jsonl
51f4433fae1b153ca89124f6e453d4f4f9bcb7335343bddac68dc14776937f0f  run-2/memory/candidate_load-guest_ram.jsonl
438855e03e01b7552c60d114e60d86ffb1269d67ef3a9b971507b9e2a75aadbb  run-2/memory/candidate_warm_idle-device_vram.jsonl
da70fabc6fe5998c14699bdd65d4cc983a32ec10a503ae3de3bc172cc8cf54db  run-2/memory/candidate_warm_idle-guest_ram.jsonl
617b135e90bca12eff83d1d0eed6167e377e9dcc4d7ebfb0d15218608ed68e6b  run-2/memory/predecessor_idle-device_vram.jsonl
d7dcdcf59bad6ed82ad119da2e1d5b132f39af5949269ba6f5a2a8a7c86ebd5a  run-2/memory/predecessor_idle-guest_ram.jsonl
83d0b443e79601e485582c011a3d2f57772552884726c78250fc2c965bf480e6  run-2/record.json
e3686d18d1414ce791e46a601e6ef6fb49cfd3fdf7aeb3f74adbfa3984f9b185  run-2/run.log
d6f36597373cde89ed682a26326e93ebdb8af420e364bb73c63f75a060fc5b44  session/pass/logs/05-agent-environment.log
c70994420441c18ba7bdf2c96eaf7910bdb309599d10f16d403cf9a692d234be  session/pass/pass-report.json
5fe811bd9162c0a0875df8f33bf87df94907daa3097ed06b87aed04d32bfe491  session/raw/host-facts.txt
bb05d2b346d77cc9a4a2343cc21b798be9745d6bee2562976e70dfd8438bf620  session/raw/packages-final.txt
1bf366b66b79d30e4f8b20a6427b0bab3d9ae7765880e56807292b314bfa174e  session/raw/torch-system.txt
71c671e1735f4a92bedada0d7182a1f4c3f49826be918ef2cf4b6b56b8dfa4c0  session/scripts/10-prep.sh
d941daca3743e935ab125cca78d78a1b4b1d4229297ecb3ae02741a9613f0c2f  session/scripts/30-qualify.sh
```

## What these runs do not show

- Accuracy. No transcript is compared with a reference.
- Behaviour without the interim settings, or on a host whose speech
  runtime came from published packages.
- Concurrent requests, streaming, or the two candidates loaded together.
- A continuous-time memory peak, or a memory reserve.
