# Whisper candidate qualification on an NVIDIA L4, 2026-10-01

Two runs of `tools/validation/candidate-qualify.py` over the
`stt-whisper-candidate` bundle (a CTranslate2 conversion of Whisper
large-v3-turbo through faster-whisper, `float16` on CUDA, languages `en`
and `ar`), recorded on a disposable `g2-standard-8` with one NVIDIA L4
running the published `v0.3.1-rc.1` packages. These are candidate
records: each says `presented_as: candidate` and `production_evidence:
false`, and neither is release evidence for any support row.

**Run 1 is `fail` and run 2 is `incomplete`, and they are filed as
recorded.** Every fixture request in both runs returned `ok`, and every
negative case that fails at request time or at admission returned its
typed code. What failed in run 1 is the one case built to fail while the
runner loads: `tensorplate deploy` reported `timeout` where a typed code
was expected, because the CLI gave up before the agent answered. Nothing
recorded attributes that to the model or its runner; it is described
under [What the runs found](#what-the-runs-found). Run 2 left that one case
out, so it has no failing step and one that did not run. The candidate
was run again on 2026-10-08, with the deploy path fixed:
[summary](../speech-candidate-whisper-l4-2026-10-08/SUMMARY.md).

The filed files carry no host, account or cloud identifier, so nothing
needed the [evidence README](../v0.2.1/README.md)'s synthetic forms: they
are as recorded. The raw run is retained privately. Transcripts under each run's
`outputs/` are not filed: the records carry each output's length, segment
and word counts and path, never the text.

## The two records

| | `run-1/record.json` | `run-2/record.json` |
| --- | --- | --- |
| Result | `fail` | `incomplete` |
| Fixtures | 5 clips, 54 requests each, all `ok` | 5 clips, 54 requests each, all `ok` |
| Candidate deploy, wall | 16.6 s | 16.8 s |
| Runner load | 6.41 s | 6.42 s |
| `malformed_audio_dtype` | matched `shape_mismatch` | matched `shape_mismatch` |
| `malformed_text_utf8` | matched `config_invalid` | matched `config_invalid` |
| `unsupported_language` | matched `unsupported` | matched `unsupported` |
| `corrupt_artifact_digest` | matched `load_failed` | matched `load_failed` |
| `oom_at_load` (expects `oom_error`) | **mismatched: `timeout`** | not run (no ballast given) |
| `cancel_during_request` (recorded, never judged) | HTTP 501 | HTTP 501 |
| Status after the negatives | **agent's last error is not the code the CLI reported** | as expected |
| Teardown by rollback | **failed: agent `busy`** | ok, predecessor restored |
| Memory windows | three measured; `after_teardown` not run | all four measured |

Run 1's ballast held 19,466,813,440 bytes of device memory during the
`oom_at_load` deploy. Run 2 was started without a ballast so that the
teardown and the after-teardown window would complete; its `oom_at_load`
case is `not_run` for that reason and the record is `incomplete`, not
`pass`.

Run 1's status snapshot after the negatives failed one check: the tool
expects the agent's `last_error` to be the code of the last failed
deploy, and the agent held `load_failed` (from the corrupt-artifact
case) while the last deploy the CLI reported was the `timeout`. It is the
same defect seen from the status side.

In run 1 the `after_teardown` window carries the reason `pending
hardware`. That string is the tool's default for a window that has not
run; the window did not run because the teardown before it failed.

### Fixtures

Two clean clips, each with the variants the tool derives from it, pinned
by digest in `test/models/speech/candidate_fixture_inventory.json`:

| Clip | Duration | Source |
| --- | --- | --- |
| `en-clean-16k-01` | 5.72 s | FLEURS, `en_us`, test split |
| `ar-clean-16k-01` | 9.62 s | FLEURS, `ar_eg`, test split |
| `en-noisy-16k-01`, `ar-noisy-16k-01` | as their source | seeded noise mixed into the clean clip |
| `en-telephony-8k-01` | 5.72 s | the clean clip through 8 kHz G.711 mu-law, resampled back to 16 kHz for the request |

The clips are not in this repository.
[The recipes doc](../../speech-candidate-recipes.md#the-clips-of-the-2026-10-01-run)
says where they come from and how they were converted.

### Timing

Median decode is 193 ms for the 5.72 s English clip and 280 ms for the
9.62 s Arabic clip, a real-time factor (decode time over clip duration)
of 0.034 and 0.029; the noisy and telephony variants are within a few
milliseconds of their clean source. The first request after a load is
slower: 678 ms in run 1 and 559 ms in run 2, against the worker's 8 s
whole-exchange timeout. Of the 6.4 s runner load, 4.6 s is the runner
verifying the 1.6 GB model's digest and 1.6 s is building the model
(`runner_timings_us` in each sample). Each record holds every sample.

### Memory

Sampled maxima over 60 s windows at 1 s, from the platform sampler, in
MiB (bytes over 2^20); both runs gave the same sidecar figures except
where two are shown:

| Window | Sidecar VRAM | Sidecar RSS |
| --- | --- | --- |
| Predecessor idle | 248 | 718 / 719 |
| Candidate warm idle | 2,138 | 200 |
| Candidate under load | 2,348 | 643 |
| After teardown (run 2 only) | 248 | 719 |

The predecessor is a small CUDA smoke bundle, so its row is that bundle's
sidecar, not an empty device. The observations behind each figure are the
JSON Lines files under each run's `memory/`.

## What the runs found

**The deploy built to fail while the runner loads reached the operator
as `timeout`, not as a typed code.** In run 1, the candidate was deployed again while a
ballast held most of the device's memory. The expected outcome is
`oom_error`. What the runner itself raised is not in this record: the
candidate's agent does not capture the worker's output, so the only
observations are the CLI's. `run-1/run.log` shows what the operator got: the
deploy ended after 30,200 ms with exit 4, which is the CLI's default 30 s
agent timeout, and the `rollback` issued immediately afterwards was
refused as `busy` (exit 5) because the agent was still inside that
deploy's transaction. The teardown therefore failed and the last memory
window did not run. The other candidate's runs in the same session show
the same behaviour for its load-time negative.

Neither run shows a failed fixture request, a crash that lost the active
deployment, or a negative case that was accepted.

The qualification tool now passes every deploy and rollback a CLI timeout
above the agent's warm timeout and waits out `busy` before it gives up on
the teardown, so the next run records the agent's own answer and completes
its teardown. That does not make the agent return the runner's code; it
is a separate fix.

## The host

All of `host/` was recorded in the same session: the build, install,
settings, provisioning and cold deploy before the runs, and
`default-startup-deadline/` after the last run.

### The speech runtime packages (`host/family-build/`, `host/family-install/`)

No release publishes the speech runtime package family yet, so the eight
packages were built on the instance from the lock at the candidate's
commit (`builder-commit.txt`) and never uploaded anywhere.
`builder-tree-status.txt` and `changelog-rewrite.txt` record the one
change to that checkout: the Debian changelog's head version was set to
the candidate's, so the packages' exact-version dependency on
`tensorplate-serving` could bind to the installed candidate.

- `times.txt`: fetch 94 s, build 14 min 53 s; `wheelhouse-files.txt` and
  `wheelhouse-bytes.txt`: 114 files, 4,410,397,733 bytes.
- `family-debs.sha256`, `family-debs.sizes`, `family-debs.control`: each
  package's digest, size and dependency fields.
- `packages-before.txt`, `packages-after.txt`: one `dpkg -i` of all eight.
- `agent-before.txt`, `agent-after.txt`,
  `agent-restarts-during-install.json`: the agent was restarted exactly
  once by that install.
- `dpkg-verify.txt`, `installed-bytes.txt`: verify clean; 8,145,387,966
  bytes installed.
- `environment-self-report.json`: the installed environment's interpreter
  and engine versions, with CUDA available to both engines.

### Interim settings (`host/settings/`)

The candidate's launcher and backend probe do not yet select the speech
runtime environment by themselves, so the runs set three variables in the
agent's environment file (`default-after.txt`, and `agent-environ.txt` as
the running agent had them):

- `TP_PYTHON_PYTORCH_EXECUTABLE`: the environment's interpreter;
- `LD_LIBRARY_PATH`: the environment's cuBLAS directory;
- `TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS=120000`: a raised sidecar startup
  deadline.

The backend probe runs the system interpreter, so PyTorch was also
installed there (`system-torch.txt`); the runs themselves used the
environment's own engines, as each record's `device_facts` shows.
`gpu-memory-before-runs.txt` is the device's total, used and
free memory in MiB before the first run.

The raised deadline turned out not to be needed on this host.
`host/default-startup-deadline/` removes it (`agent-environ.txt`) and
deploys each candidate from a dropped page cache and again warm
(`summary.txt`): Whisper became active in 18.0 s cold and 17.4 s warm.

The library search path matters more than this host shows. A probe in
the same session, kept in the raw run and not filed (the publication
scanner reads a four-part library version as a network address), loaded
and decoded the model outside the appliance three ways. With no search
path, CTranslate2 mapped the cloud image's own cuBLAS, which the image
carries in its loader cache; with the environment's cuBLAS directory on
the path it mapped the environment's. No cuDNN library was mapped for
load or decode either way. This host therefore cannot show what happens
with no search path on an image that has no system CUDA, and a host check
has to look at which file is mapped, not at whether the load succeeds.

### Provisioning (`host/provision/`)

`tensorplate bundle provision stt-whisper-candidate` from the packaged
manifest (`provisioning-manifest.sha256`): seven files, 1,621,669,131
bytes, in 58 s (`times.txt`, which also carries the other candidate's
lines). A second invocation verified the set and wrote nothing
(`provision-stt-whisper-candidate-repeat.json`).
`files-stt-whisper-candidate.sha256` and
`files-stt-whisper-candidate.sizes` list every provisioned file; the digests equal the record's `subject.artifacts`.

### Cold deploy (`host/cold-deploy/`)

One deploy of the candidate from a dropped page cache, with the raised
deadline still set: active in 17.9 s (`deploy-wall.txt`, `deploy.json`),
the sidecar holding 2,138 MiB of device memory afterwards
(`gpu-compute-apps.txt`, `gpu-memory-after-deploy.txt`).
