# Kokoro candidate qualification on an NVIDIA L4, 2026-10-01

Two runs of `tools/validation/candidate-qualify.py` over the
`tts-kokoro-candidate` bundle (Kokoro-82M, voice `af_heart`, `float32` on
CUDA), recorded on a disposable `g2-standard-8` with one NVIDIA L4 running
the published `v0.3.1-rc.1` packages. These are candidate records: each
says `presented_as: candidate` and `production_evidence: false`, and
neither is release evidence for any support row.

**Both records are `fail`, and they are filed as recorded.** Every fixture
request in both runs returned `ok`, and every negative case that fails at
request time or at admission returned its typed code. What failed is how a
load failure inside the runner reaches the operator: for the two cases
built to fail while the runner loads, `tensorplate deploy` reported
`timeout` where a typed code was expected. That is a defect in the deploy
path, not in the model or its runner, and it is described under
[What the runs found](#what-the-runs-found). The
candidate is to be run again once it is fixed.

The filed files carry no host, account or cloud identifier, so nothing
needed the [evidence README](../v0.2.1/README.md)'s synthetic forms: they
are as recorded. The raw run is retained privately. Generated audio under each
run's `outputs/` is not filed: the records carry each output's digest,
length and path, never the audio.

## The two records

| | `run-1/record.json` | `run-2/record.json` |
| --- | --- | --- |
| Result | `fail` | `fail` |
| Fixtures | 14 sentences, 47 requests each, all `ok` | 14 sentences, 47 requests each, all `ok` |
| Candidate deploy, wall | 11.1 s | 14.9 s |
| Runner load | 8.84 s | 12.65 s |
| `malformed_text_blank` | matched `config_invalid` | matched `config_invalid` |
| `malformed_text_dtype` | matched `shape_mismatch` | matched `shape_mismatch` |
| `corrupt_artifact_digest` | matched `load_failed` | matched `load_failed` |
| `unsupported_voice` (expects `unsupported`) | **mismatched: `timeout`** | **mismatched: `timeout`** |
| `oom_at_load` (expects `oom_error`) | not run | **mismatched: `timeout`** |
| `cancel_during_request` (recorded, never judged) | HTTP 501 | HTTP 501 |
| Status after the negatives | as expected | **agent's last error is not the code the CLI reported** |
| Teardown by rollback | ok, predecessor restored | **failed: agent `busy`** |
| Memory windows | all four measured | three measured; `after_teardown` not run |

In run 1 the `oom_at_load` case did not run: the ballast exited with
status 4, which is the device refusing its allocation, so it held no
memory and the tool skipped the deploy. The size it asked for was the
operator's choice and was not recorded. Run 2's ballast held
21,915,238,400 bytes.

Run 2's status snapshot after the negatives failed one check: the tool
expects the agent's `last_error` to be the code of the last failed
deploy, and the agent held `load_failed` (from the corrupt-artifact
case) while the last deploy the CLI reported was the `timeout`. It is the
same defect seen from the status side.

In run 2 the `after_teardown` window carries the reason `pending
hardware`. That string is the tool's default for a window that has not
run; the window did not run because the teardown before it failed.

### Timing

Synthesis is 66 to 119 ms per sentence at the median, and the median
real-time factor (synthesis time over generated audio duration) is 0.012
for the longest sentences to 0.049 for the shortest. The first request
after a load is slow: 2.78 s in run 1 and 1.55 s in run 2, against the
worker's 8 s whole-exchange timeout. Each record holds every sample.

### Memory

Sampled maxima over 60 s windows at 1 s, from the platform sampler, in
MiB (bytes over 2^20):

| Window | Sidecar VRAM | Sidecar RSS (run 1 / run 2) |
| --- | --- | --- |
| Predecessor idle | 248 | 713 / 719 |
| Candidate warm idle | 552 | 1,290 / 1,290 |
| Candidate under load | 1,152 | 1,938 / 1,873 |
| After teardown (run 1 only) | 248 | 718 |

The predecessor is a small CUDA smoke bundle, so its row is that bundle's
sidecar, not an empty device. The observations behind each figure are the
JSON Lines files under each run's `memory/`.

## What the runs found

**A load failure inside the runner reached the operator as `timeout`,
not as a typed code.** With the CLI's default 30 s agent timeout, the
deploys built to fail while the runner loads (an entry selecting a voice
it does not declare, expected `unsupported`; a load with the device's
memory held by a ballast, expected `oom_error`) ended after 30.1 to
30.4 s with `timeout`. What the runner itself raised is not in these
records: the candidate's agent does not capture the worker's output, so
the only observations are the CLI's. `host/unsupported-voice-reproduction/`
repeats the undeclared-voice deploy by hand, twice:

- `deploy.err`, `deploy.exit`: default timeout, `timeout` after 30,430 ms,
  exit 4;
- `deploy-long-timeout.err`, `deploy-long-timeout.exit`: `--timeout-ms
  180000`, `inference_failed` after 32,088 ms, exit 3, with the agent's
  message that it could not connect to the serving worker.

So the agent answers about two seconds after the CLI's default timeout
gives up, and its answer is not a typed runner code either. A
`rollback` issued right after the cut-off deploy is refused as `busy`
(exit 5), because the agent is still inside that deploy's transaction:
that is run 2's failed teardown. Neither run shows a wrong result, a
crash that lost the active deployment, or a negative case that was
accepted.

## The host

All of `host/` was recorded in the same session: the build, install,
settings and provisioning before the runs, the cold deploy and the
reproduction between them, and `default-startup-deadline/` after run 2.

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
environment's own PyTorch, as each record's `device_facts` shows.
`gpu-memory-before-runs.txt` is the device's total, used and free memory
in MiB before the first run.

The raised deadline turned out not to be needed on this host.
`host/default-startup-deadline/` removes it (`agent-environ.txt`) and
deploys each candidate from a dropped page cache and again warm
(`summary.txt`): Kokoro became active in 16.6 s cold and 11.1 s warm.
Kokoro's runner load of 8.84 s and 12.65 s in the two runs leaves little
room under the 15 s default.

### Provisioning (`host/provision/`)

`tensorplate bundle provision tts-kokoro-candidate` from the packaged
manifest (`provisioning-manifest.sha256`): five files, 327,740,399 bytes,
in 20 s (`times.txt`, which also carries the other candidate's lines). A
second invocation verified the set and wrote nothing
(`provision-tts-kokoro-candidate-repeat.json`).
`files-tts-kokoro-candidate.sha256` and
`files-tts-kokoro-candidate.sizes` list every provisioned file; the digests equal the record's `subject.artifacts`.

### Cold deploy (`host/cold-deploy/`)

One deploy of the candidate from a dropped page cache, with the raised
deadline still set: active in 19.3 s (`deploy-wall.txt`, `deploy.json`),
the sidecar holding 552 MiB of device memory afterwards
(`gpu-compute-apps.txt`, `gpu-memory-after-deploy.txt`).
