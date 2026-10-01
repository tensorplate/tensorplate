# Candidate memory sampling

Qualification recipes use the platform memory sampler for each warm-idle and
load window. The sampler performs no deploy, workload generation or admission
change. The operator starts it on the machine being measured after warming the
candidate, or while a separately controlled workload runs.

Build the operator binary once from the matching source checkout with the pinned
Rust toolchain; copy the binary to the measured machine if building elsewhere:

```sh
cargo build --locked --release -p tensorplate-platform --example memory_sample
```

Invoke that binary directly, or use the thin wrapper from the checkout:

```sh
tools/validation/memory-sample.sh target/release/examples/memory_sample \
  --interval 1s --duration 60s --phase warm-idle --domain guest_ram \
  --out /var/tmp/candidate-warm-idle.jsonl \
  --process 1001:agent --process 1002:serving_worker --process 1003:python_sidecar \
  > /var/tmp/candidate-warm-idle-summary.json
```

For VRAM, run a second domain-selected capture using `--domain device_vram`
and only the PIDs that own GPU allocations (for example, the Python sidecar).

The PIDs above are examples: supply the live PIDs and their actual roles. Repeat
with `--phase load` and different output files while the recipe drives its
workload. Phase is an operator assertion, never inferred from usage. Establish
workload completion separately. PID ownership must remain stable for the window;
stop and start a new window after a process restart. This tool does not resolve
deployment generations or detect PID reuse.

Both `guest_ram` and `device_vram` are selected by default. Repeat `--domain`
to select either or both explicitly. An absent NVIDIA process entry is unavailable
for that PID, not zero; agent/CPU-only PIDs may therefore make VRAM attribution
incomplete. To measure all host PIDs but only GPU-owning PIDs, use separate,
explicitly domain-selected invocations with the appropriate process sets. Never
sum RSS/process usage into physical domain consumption.

Each JSON Lines record conforms to `memory_observation.json`. Standard output is
one summary object with `schema_version: "0.1"`, `phase`, `interval_ms`,
`duration_ms` and `report`. The report contains `expected_ticks`,
`completed_ticks`, `late_ticks`, `complete` and `domains`. Each domain includes
`consumed` and `processes`; each process identifies its PID, role and `peak`.
Every peak has `max_bytes` (null if never observed) and `available_samples`.
The warm-idle value is the maximum in the explicitly warm-idle window; the load
peak is the maximum in the load window, using the same platform reducer. These
are sampled maxima, not continuous-time peaks, allocator values or reserves.

Recipes must require `report.complete == true` and every required PID/domain
measurement before importing those values. Exit status is 0 for complete
coverage, 3 for incomplete coverage, and 1 for an invocation/write failure.
Incomplete windows keep the observations and any observed maxima for diagnosis;
they do not establish qualification. A write failure leaves a partial output
file. Existing output files are refused rather than overwritten.

Intervals and durations accept positive integer `ms` or `s` values. Each is
at most 24 hours, with at most 100,000 scheduled ticks and 64 attributed PIDs.
Sampling starts at zero, on a fixed monotonic schedule, with one observation per
selected domain per batch. Overdue ticks are skipped, not replayed. Blocking
source calls can overrun the interval or duration; a late batch or skipped tick
makes coverage incomplete. A batch must finish before its next scheduled tick
or the window end, whichever comes first. A hung command needs operator interruption. This
operator tool supplies no hard invocation deadline or pressure directives.

Device sampling first runs one full `nvidia-smi -q -x`; missing/unusable required
readings trigger a fresh header-bearing CSV query pair. Records identify the
actual source and never blend XML and CSV. Host sampling uses `MemAvailable`
and per-PID `VmRSS`. Raw command output and device identities are not written.
Follow [fixture and evidence rules](fixture-and-evidence-rules.md) before
publishing results. The repository's model-free idle recordings test the parser
and loop; they supply no candidate memory or reserve qualification.

## Provisioning the reference artifacts

On the qualification host, run `tensorplate bundle provision
stt-whisper-candidate` and `tensorplate bundle provision tts-kokoro-candidate`
from the operator shell before denying egress. Use the resulting directories
under `/var/lib/tensorplate/bundles/import/` unchanged. The
[packaged example manifest](../../packaging/provisioning/manifest.json) pins
the converted Whisper directory and Kokoro config, weights and af_heart voice;
its companion [provenance record](../../packaging/provisioning/README.md) names
the candidate revisions and the publisher's conversion-provenance limits.
Record the provisioning manifest's SHA-256 and every file digest with the run.
An operator's alternate manifest uses the same verification path. The VAD and
G2P assets are runtime wheel data, not additional bundle downloads. Provisioning
proves retrieval and integrity; the recipe still must measure real load,
inference, unload and negative paths through the installed runner environment.

## Candidate qualification recipe

`tools/validation/candidate-qualify.py` qualifies one candidate bundle on the
machine it runs on and writes one record conforming to
`config/schemas/candidate_qualification_record.json`. It is a tool for any
candidate runner family: the bundle's entry names the `backend_profile`, and
the inventory maps that profile to a suite. The two reference candidates,
`faster_whisper` (the `stt` suite) and `kokoro` (the `tts` suite), are its
first two subjects. A record describes a candidate run and is never presented
as Production evidence; the record says so in its `qualification` block.

### What one run does

1. Reads the candidate bundle, verifies every manifest digest and byte size
   and records the artifact identities and the entry as deployed.
2. Deploys the predecessor bundle named with `--predecessor-bundle`, so the
   teardown has something to roll back to (the agent refuses `rollback` with
   no previous active deployment and does not yet serve `undeploy`), then
   samples its idle memory.
3. Deploys the candidate with `tensorplate deploy`, takes a status snapshot
   (agent state, active deployment, backend, serving URL, worker health) and
   samples the candidate's warm-idle memory.
4. Runs every fixture of the suite `--timing-iterations` times over the
   serving worker's `/infer` route through the Python SDK, binary transport
   by default, and records each exchange's client wall time, the worker's
   timing, the runner's `timings_us` and the real-time factor: `decode` over
   the clip's duration for STT, `synthesize` over the generated duration for
   TTS. Each sample is judged against `--request-timeout-ms`, the worker's
   whole-exchange timeout (8,000 ms in the packaged configuration).
5. Starts the load window and keeps running the fixtures until the sampler
   finishes, so the load peak is sampled under this tool's own workload.
6. Drives the negative paths and records each typed outcome: malformed input
   tensors, an undeclared language (STT) or an entry selecting an undeclared
   voice (TTS), a bundle whose largest artifact differs by one byte from its
   manifest digest, a request submitted and cancelled through the
   asynchronous policy route (recorded, never judged: the Python-backed worker
   refuses that route today, and a sidecar that serves one request at a time
   could not honour the cancel if it did), and, with
   `--oom-ballast-bytes`, a second deploy while `tools/validation/vram_ballast.py`
   holds device memory.
7. Takes a second status snapshot, which expects the agent `degraded` with the
   last failed deploy as its `last_error`, tears the candidate down with
   `tensorplate rollback`, confirms the predecessor is active and ready and
   samples memory after the teardown.
8. Writes `record.json`, the sampler's JSON Lines files under `memory/`, the
   generated outputs under `outputs/` and the tool's log under `run.log`.

The exit status is 0 when the record's `result.status` is `pass`, 2 for
`fail` (a step ran and did not hold) and 3 for `incomplete` (a step did not
run; `result.reasons` names each one). A record whose memory windows were
not sampled is incomplete, never a pass.

### Memory

The record's memory fields come only from the platform sampler described
above, invoked through `tools/validation/memory-sample.sh`; this tool never
runs `nvidia-smi` itself. Pass the built `memory_sample` binary with
`--sampler`. Each window runs two sampler invocations over the same
`--window`: `device_vram` with the `python_sidecar` PID alone, and
`guest_ram` with the agent, serving worker and sidecar PIDs. PIDs are
discovered from the agent unit's main PID and its process tree, or given
with `--pid <pid>:<role>`. A window is `measured` only when both invocations
report complete coverage and every listed PID was observed; otherwise it is
`incomplete` with the sampler's reason, and its maxima are kept for
diagnosis only. The four windows are the predecessor's idle, the candidate's
warm idle, the candidate under load and the state after the teardown; the
last two together show what the teardown released. All of them are sampled
maxima, not continuous-time peaks.

Until the qualification runs happen, a record produced without hardware
carries every memory window as `not_run` with the reason `pending hardware`.

### Fixtures

`test/models/speech/candidate_fixture_inventory.json` lists the fixtures. STT
clips are mono 16-bit PCM WAV files provisioned outside the repository into
the `--clips` directory as `<clip id>.wav`, pinned in the inventory by
digest. Two clips are derived by the tool from their clean source with the
derivation the inventory records, so their digests are stable and pinned the
same way: a noisy variant mixes seeded, integer-generated noise at the given
signal-to-noise ratio, and the telephony variant decimates to 8 kHz and
transcodes through G.711 mu-law (the Sun reference encoder). A clip with a
`null` digest, provisioned or derived, is unpinned and the tool refuses to run
on it; for a derived clip the refusal names the digest to pin. The 8 kHz clip is resampled back to the
entry's rate before it is sent, because the runner takes exactly the model's
input rate; the record names the resampler (`tp-fir63-blackman-q15-v1`, a
63-tap windowed sinc with literal Q15 taps in `tools/validation/candidate_audio.py`)
so the derivation is reproducible. TTS sentences are the fixture bytes
themselves, pinned by the digest of their UTF-8 encoding.

### Running it

Stage the candidate and predecessor bundles where the agent can read them,
build the sampler once as described above, then:

```sh
tools/validation/candidate-qualify.py \
  --candidate-bundle /var/lib/tensorplate/bundles/import/<candidate> \
  --predecessor-bundle /var/lib/tensorplate/bundles/import/<predecessor> \
  --clips /var/tmp/candidate-clips \
  --sampler target/release/examples/memory_sample \
  --window 60s --sample-interval 1s \
  --evidence-dir /var/tmp/candidate-evidence-<date>
```

The evidence directory must be new. Variant bundles for the negative cases
are written under `<evidence-dir>/staging` unless `--staging-dir` says
otherwise; the agent reads them, so that path must be readable by its
account. Generated transcripts and PCM under `outputs/` may carry licensed
fixture content: keep the evidence directory access-controlled and publish
only the sanitized record and sampler files, following the
[fixture and evidence rules](fixture-and-evidence-rules.md). The record
itself carries output metadata and digests, never request text or audio.

`test/validation/candidate_qualify_test.py` runs the tool against a fake
appliance (a CLI, worker, sampler and ballast that answer in the real shapes)
for both suites, compares a fresh run with the committed synthetic record in
`test/validation/fixtures/`, and breaks each guard once. The synthetic record
measures nothing; its `provenance` says so.

## Recorded runs

### Kokoro on an NVIDIA L4, 2026-10-01

[`evidence/speech-candidate-kokoro-l4-2026-10-01/`](evidence/speech-candidate-kokoro-l4-2026-10-01/README.md)
holds two runs of the `tts` suite against the first 0.3.1 release
candidate on a `g2-standard-8`, with the host facts each run depends on:
the speech runtime packages as built and installed, the interim agent
settings, the provisioned files and a cold deploy.

Both records are `fail` and are filed as recorded. Every fixture request
returned `ok`, with a median real-time factor of 0.012 to 0.049, and the
sidecar's sampled device memory was 552 MiB warm idle and 1,152 MiB under
load. Every negative case that fails at request time or at admission
returned its typed code. The two cases built to fail while the
runner loads did not: `tensorplate deploy` reported `timeout` for an entry
selecting an undeclared voice (expected `unsupported`) and for a load with
the device's memory held by a ballast (expected `oom_error`), and a
rollback issued right after was refused as `busy`. The worker's output is
not captured, so what the runner raised is not in the records. The
record's README shows the reproduction. The candidate is run again once
the deploy path returns the runner's typed failure.
