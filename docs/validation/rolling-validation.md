# Rolling validation

A rolling validation pass installs the latest build on a real machine, runs
the lifecycle harness and the candidate qualification recipe, and compares
what it measured with the pass before it. It is not release evidence: a pass
files nothing in this repository. Its records and its index stay with whoever
ran it, and a difference it finds becomes a fix or a recorded run of its own.

This page covers running a pass, the run index and the comparison. The index
and the comparison read files a run has already recorded, so they work the
same on a pass, on a qualification run and on a release gate bundle.

## Running a pass

`validation-pass.py run` runs one pass on the machine it is started on. It
creates, starts and deletes no machine and names none. The machine exists
already and holds a checkout of this repository at the commit the candidate
artifact set was built from, that set, and, for the upgrade and rollback
stages, a published predecessor set. The pass purges TensorPlate from the
machine, because the lifecycle harness does, so it asks for the harness's
confirmation token and belongs on a disposable machine.

```sh
tools/validation/validation-pass.py run \
  --assets-dir ~/candidate --allow-unsigned \
  --baseline-assets-dir ~/baseline \
  --source-commit <the commit the candidate set was built from> \
  --speech-family host-built \
  --candidate stt-whisper:stt-whisper-candidate:$HOME/clips \
  --candidate tts-kokoro:tts-kokoro-candidate \
  --staging-dir /opt/tensorplate-validation/pass-staging \
  --predecessor-bundle /opt/tensorplate-validation/x86_cuda_smoke \
  --sampler target/release/examples/memory_sample \
  --oom-ballast-python <an interpreter that imports the runtime's torch> \
  --oom-ballast stt-whisper=headroom:<bytes> \
  --oom-ballast tts-kokoro=headroom:<bytes> \
  --session "<the session's name>" --row ubuntu2404-x86-l4-g2s8 \
  --machine "g2-standard-8, one NVIDIA L4" --build "<branch and commit>" \
  --out-dir ~/pass --confirm RESET-TENSORPLATE
```

A candidate's subject is its index subject and names its directories, so it
is lower-case letters, digits and hyphens, and not `lifecycle`, `doctor`,
`logs` or `streaming-latency`. `--predecessor-bundle`, `--sampler`,
`--oom-ballast-python` and each `--qualify-arg` go to the recipe unchanged,
and `--oom-ballast` becomes the recipe's `--oom-ballast-bytes` for the one
candidate it names. A `--qualify-arg` that sets `--oom-ballast-bytes`,
`--predecessor-bundle` or `--sampler`, or abbreviates one of them, is refused:
the report would no longer say what ran, and a sampler given that way would go
around the predecessor check below.

### The predecessor

The recipe deploys the predecessor first so that the teardown can roll back
to it, and samples memory while it is the active deployment (`predecessor_idle`)
and again after the rollback (`after_teardown`). A window is `incomplete` when
a process it samples is never observed, and the device window samples the
sidecar. A predecessor has to put something on the device for that: the
fixture the lifecycle harness stages (`x86_fixture_smoke`) selects the
device-neutral `fixture` profile and does not, and `x86_cuda_smoke` selects
`cuda_fixture`, whose load runs a matrix product on the device.

Where `nvidia-smi` reports device 0, a pass that has candidates and
`--sampler` therefore needs `--predecessor-bundle` to name a bundle that does:
its manifest lists one `python_pytorch_entry`, inside the bundle, and that
entry names the `cuda` device and one of the runner profiles `cuda_fixture`,
`faster_whisper` or `kokoro`. `test/models/bundles/v0_1/x86_cuda_smoke` is the
CUDA smoke fixture; copy it where the agent can read it, as the
[cloud-row runbook](cloud-row-runbooks.md) does. Preflight refuses any other
predecessor, and none, before anything runs.

Without `--sampler` no window is taken and the predecessor is the operator's
choice. With it, what `nvidia-smi` does decides:

| `nvidia-smi` | A pass that samples memory |
| --- | --- |
| reports device 0 | the predecessor is checked as above |
| cannot be run at all | the machine is taken to have no GPU: the predecessor is the operator's choice |
| runs, and exits non-zero or prints no single figure | refused in preflight: the machine has a GPU that does not report, and its device windows could not be trusted |

`predecessor_check` in `pass-report.json` says whether the check was applied
and, when it was not, why: no memory window is taken, or `nvidia-smi` cannot
be run.

Whatever else the pass has, preflight refuses a `--predecessor-bundle` whose
path, with its links resolved, lies under a directory the agent's unit cannot
read (the staging directory's rule, below): the agent could not deploy it,
and every qualification step would fail on it after the lifecycle and the
family build. That test is of the path alone, so the bundle need not exist
yet.

### The out-of-memory ballast

The recipe's out-of-memory case deploys the candidate again while a process
holds device memory, and the case holds only when the load no longer fits in
what is left. Two candidates of different size do not fit one amount, so each
candidate is given its own with `--oom-ballast SUBJECT=RULE`, repeatable:

| Rule | Ballast |
| --- | --- |
| `bytes:N` | N bytes |
| `headroom:N` | the memory `nvidia-smi --id=0 --query-gpu=memory.total` reports for device 0 on this machine, less N bytes |
| `none` | none: the candidate's out-of-memory case is skipped on purpose |

The pass does not know how much memory a candidate needs or how much the
ballast process takes beside its allocation: choose N from a run of the
recipe by hand on the same machine type, and expect to adjust it. A headroom
is subtracted from the device's total memory, so N has to cover the
candidate's own resident memory and the ballast process's overhead, and a
figure sized on one build can stop fitting on the next. The pass does not
size a ballast from the memory that is free at the moment the ballast is
taken. A `headroom` rule is read with `nvidia-smi` (`--nvidia-smi` names
another program) and evaluated in preflight, so a machine that reports no
single figure for device 0 is refused there; so is a headroom that leaves no
ballast and, where the machine reports a figure, a ballast that does not fit
the device. A `bytes` or `headroom` rule needs `--oom-ballast-python`, the
interpreter that holds the ballast, and the pass refuses to start without
the option. The interpreter itself need not exist at preflight: it is usually
the speech runtime family's, which the lifecycle harness purges and the pass
installs again. Each `qualify` step whose candidate takes a ballast checks
that it can be run before it runs the recipe, and is `failed` with that
reason when it cannot.

Without a ballast the recipe records the out-of-memory case as `not_run`, and
a recipe result with a case that did not run is not `pass`. Where
`nvidia-smi` reports device 0, preflight therefore refuses a candidate that no
`--oom-ballast` names. `--oom-ballast SUBJECT=none` is how a pass skips the
case on purpose. Where no device answers, a candidate without a rule is not
refused, and the report says its case will not run.

`pass-report.json` records what was used. `device_memory` holds the figure in
bytes, or null with the reason: what `nvidia-smi` answered, that the pass has
no candidate and so did not ask, or that preflight ended before it.
`oom_ballast` holds, for each candidate, the rule (`bytes`, `headroom`, `none`
or null), the resulting `bytes` (null when none), the `headroom_bytes` of a
headroom rule, and for a candidate without a ballast the reason, which says
that its out-of-memory case will not run.

`--staging-dir` is where the recipe writes the bundles of its negative cases,
which the agent has to read. The agent's unit runs with `ProtectHome` and
`PrivateTmp`, so a directory under `/home`, `/root`, `/run/user`, `/tmp` or
`/var/tmp` is refused in preflight. The pass creates one directory per
subject there through `sudo`, owned by the operator.

The steps run in this order. Each one's commands, exit code, wall time and
log path go into `pass-report.json` in the output directory, which is
replaced when a step starts and when it ends, so the report always shows the
step that is `running`. A step is `ok` when every command in it exited 0,
`failed` or `timeout` otherwise, and `not_run` when a step it needs did not
end `ok`. A pass that receives `SIGINT`, `SIGTERM` or `SIGHUP`, for example
when its connection drops, stops the running command the way a time limit
does, marks the step `interrupted` and runs nothing more. The report also
lists the variable names the agent's environment file set at preflight and
when doctor ran, so what an earlier pass left there is visible.
The `lifecycle` and `qualify` steps also carry `result`: the outcome the
harness's report or the recipe's record states itself, or null when the
command left none.

| Step | What it runs | When it does not end `ok` |
| --- | --- | --- |
| `preflight` | Verifies every file the two sets' `SHA256SUMS` list; that the candidate set's manifest and the checkout are both at `--source-commit`, with no uncommitted change; that the agent's environment file names no interpreter yet; that neither the staging directory nor a named predecessor is hidden from the agent; where a device answers, that every candidate has a ballast rule and that each ballast fits the device; where the pass samples memory, that `nvidia-smi` either reports device 0 or cannot be run, and where it reports one that the predecessor can leave the sidecar on the device ([above](#the-predecessor)); that the pass is not run as root; and, last, that `sudo` does not ask for a password | Nothing else runs |
| `lifecycle` | [`ubuntu-l4-cloud-lifecycle.sh`](cloud-row-runbooks.md) on the candidate set, with the upgrade and rollback stages when `--baseline-assets-dir` is given | The pass goes on, and a report the harness wrote is still indexed |
| `install` | The candidate set's own `install.sh`, then enables both services and waits for the agent to answer. A run with a baseline ends with the baseline installed, so this is an upgrade over it | No later step runs |
| `speech-family` | With `--speech-family host-built` only: builds the speech runtime packages from a scratch clone of the checkout, as [the family's README](../../packaging/speech-runtime/README.md) describes, at the version of the candidate set's serving package, and installs them with `apt-get` | Doctor still runs; no candidate does |
| `agent-environment` | With `--agent-environment` only: appends the variables to the agent's environment file and restarts the agent | Doctor still runs; no candidate does |
| `doctor` | `tensorplate doctor --output json`, and reads whether the agent's environment file names an interpreter for the backend | Candidates still run |
| `provision:<subject>` | `tensorplate bundle provision <bundle>` | That candidate's two other steps do not run |
| `cold-deploy:<subject>` | `sync`, drops the page cache, then times one `tensorplate deploy` of the bundle | The candidate is still qualified |
| `qualify:<subject>` | Checks that the ballast interpreter can be run when the candidate takes a ballast, creates the subject's staging directory, then runs [`candidate-qualify.py`](speech-candidate-recipes.md) on the provisioned bundle | The other candidates still run |
| `streaming-latency` | The [slice measurement client](slice-measurement.md) with the `--streaming-arg` arguments, when one names a stream endpoint; `not_run` otherwise | The pass goes on, and a record the client wrote is still indexed |
| `index` | Derives a line for each subject that left a record, into `lines.jsonl` | A subject whose record is malformed has no line; the others keep theirs |

The harness and the recipe report a failing verdict through their exit code,
so a `lifecycle` or `qualify` step is `failed` when the subject failed, and
the line indexed from its record says how. The pass exits 0 when every step
ended `ok`, not counting `streaming-latency` when no argument named a stream
endpoint, 1 when one did not, and 2 when it could not start: an argument is
refused, or the output or scratch directory is not new or empty. Exit 0 says
the steps ran, not that the indexed lines passed: a `doctor` line can read
`fail` after a `doctor` step that ended `ok`, and the comparison is what judges
the lines.

A pass leaves behind the subjects' directories under `--staging-dir`, the
scratch directory of the family build (`<out-dir>-work` unless `--work-dir`
names another), the installed packages including the speech runtime family,
and any lines it appended to the agent's environment file.

Every command a step runs has that step's time limit (`--step-timeout
STEP=SECONDS` changes one). The `streaming-latency` step's limit is
[derived](#the-streaming-measurement) from the client's arguments. A command that reaches it is sent `SIGTERM`, so
that the harness removes what it put in place. Once it has ended, or after
`--stop-grace-s` seconds (240 by default), its process group is sent
`SIGKILL`, which ends whatever ignored the first signal. Both go through
`sudo kill`, because the group holds root-owned children. The step is then
`timeout`, its reason names the signal the command ended on, and the pass
goes on as for any other failed step. A command that outlives both signals
leaves its step `failed`, and no later step but `index` runs, because it may
still be changing the machine. What the harness started as transient units
is outside the group and is the harness's own cleanup to remove. Whether the
group's `SIGKILL` reaches a command that `sudo` runs on a terminal of its
own (`use_pty`) has not been observed on hardware; check it on the first
pass. Commands
that need root run through `sudo`, which must not ask for a password.

### The streaming measurement

`--streaming-arg` appends to the command line of `slice_measure.py run`,
repeatably, and is written `--streaming-arg=--name=value`, because an argument
that begins with `--` is not accepted after a space. The pass sets `--out`
(`streaming-latency/record.json` in the output directory) and
`--source-commit` itself and refuses them, `--provenance`, or an abbreviation
of one of them, in `--streaming-arg`.

```sh
  --streaming-python <an interpreter with the client's packages> \
  --streaming-arg=--stt-endpoint=127.0.0.1:<port> \
  --streaming-arg=--stt-target=<deployment> \
  --streaming-arg=--stt-language=<language> \
  --streaming-arg=--audio=<utterance.wav> \
  --streaming-arg=--tts-endpoint=127.0.0.1:<port> \
  --streaming-arg=--tts-target=<deployment> \
  --streaming-arg=--tts-language=<language> \
  --streaming-arg=--voice=<voice> \
  --streaming-arg=--text-file=<segment.txt> \
  --streaming-arg=--worker-version=<release> \
  --streaming-arg=--iterations=20 \
```

The example names both modes because the client's record is `complete` only
when both ran: a run of one mode is `incomplete`, its step `failed`, however
well that mode did. The client refuses a mode whose target, language or input
is missing, and the step then ends `failed` with no record.

The step runs only when an argument names `--stt-endpoint` or
`--tts-endpoint`. With no `--streaming-arg` at all it is `not_run`, and
arguments that name neither are refused. An endpoint also needs
`--worker-version` or `--worker-build`, with a value: without one the client
writes a `synthetic` record, which measured nothing. The pass refuses to
start that way, the step fails on a record that is not `recorded`, and the
index refuses one. `--streaming-python` is the
interpreter that has the client's packages, the one running the pass when it
is not given. The pass starts and stops no worker; the endpoint is one the
operator already serves. The step runs after the steps a candidate's
`provision` step needs and, like a candidate's, is `not_run` when one of them
did not end `ok`.

The client exits 0 only for a `complete` record, so any other result leaves
the step `failed` with that result beside it, and a record the client's own
check accepts is indexed all the same; the index runs that check again before
it derives a line. That holds for a record with no session in it. Where
nothing answers at an endpoint the client writes a `failed` record whose mode
has no session, and the line's check for that mode reads `none: the channel
never became ready`; a mode that did answer keeps its figures beside it. A
record that is `synthetic`, or that the check refuses, has no line, and the
`index` step says so.

The step's time limit is the sum of three parts. The first is every wait of
the client at its event timeout: for each endpoint named, one wait for the
channel and four per session (`Ready`, the segment's completion, the answer
to `Finalize` and the call's end), each `--event-timeout-ms` long, times
`--iterations`. The second is the audio, which is sent in real time: the
length of the `--audio` clip, read from its WAV header, times `--iterations`,
when a speech-to-text endpoint is named. The third is 300 seconds for the
interpreter's start, the generated bindings and the GPU sampler. Forty
iterations of a 20-second clip against one endpoint with a 1,000 ms event
timeout give 161 + 800 + 300 = 1,261 seconds. The pass reads the options
from `--streaming-arg` and assumes the client's defaults (1 and 30,000) for
one that is absent; a clip it cannot read adds nothing, since the client
refuses it before any session. The client's timeout starts again at each
event a session needs, so a run can still be longer than this without being
stuck: `--step-timeout streaming-latency=SECONDS` replaces the derived limit.
A client stopped at the limit leaves the record it saved last, which is
`failed` and is indexed; one stopped before its first session was saved
shows both modes as `not run`.

### The speech runtime family

`--speech-family` says how the family reaches the machine, and with that what
doctor must report:

| Mode | What the pass does | Doctor must report |
| --- | --- | --- |
| `in-assets` | Passes `--with-speech-runtime` to the installer: the candidate set holds the family's packages | `platform_row` and the three `runner_*` findings `ok`; `python_pytorch_runtime` `ok` or `missing` |
| `host-built` | Builds the family on the machine in the `speech-family` step. The machine needs the family's build dependencies, and the fetch needs the package indexes the lock names. A build-only set carries stub wheels or no family at all, so a pass on one uses this mode | The same |
| `none` | Installs no family. No candidate can be named | `platform_row` and `python_pytorch_runtime` `ok`; the three `runner_*` findings `skipped` |

`python_pytorch_runtime` is `missing` where a runner profile is installed and
the backend descriptor's own interpreter has no PyTorch. That is a supported
install for a machine that serves only bundles naming a profile, so it does
not fail the `doctor` line. The lifecycle harness deploys a bundle that names
no profile and refuses such a machine in its own preflight, so on a pass the
`lifecycle` line is what shows it.

### Bundles that name no runner profile

A bundle that names no runner profile is served in the backend descriptor's
own interpreter, which has none of the family's engines. Until the candidate
bundles name their profile, they deploy only when the agent's environment
names the family's interpreter, and that interpreter exists only once the
family is installed, after the harness has run. `--agent-environment
NAME=VALUE` sets it at that point:

```sh
  --agent-environment TP_PYTHON_PYTORCH_EXECUTABLE=<the family's interpreter>
```

A pass run this way shows the `doctor` line as `fail` with
`interpreter_override` reading `present`, and its lines record the variable
names under `settings.agent_environment`. Without the option the deploy of
such a bundle is expected to fail, and the `cold-deploy` and `qualify` steps
and their lines are where that shows. Once the bundles name a profile the
option is dropped,
`interpreter_override` reads `absent` and the `doctor` line can pass; the
comparison reports both changes.

The pass does not remove what it appended, and the harness does not either,
while it does purge the family the variable points into. A second pass on
the same machine is therefore refused in preflight until the lines are
removed from the agent's environment file.

### What the pass measures itself

The recipe reads and hashes the whole bundle before it deploys it, so the
deploy time in its record is always taken with the bundle in the page cache.
The `cold-deploy` step times one more deploy, of the same bundle, right after
`sync` and `sysctl --write vm.drop_caches=3`, with the same two deploy
timeouts the recipe is given, and writes `<subject>/cold-deploy.json`. The
candidate stays deployed; the recipe's own deploys replace it.

### Getting the artifact sets

The candidate set for a commit that has no release is the unsigned asset
artifact of a build-only run of the Release workflow
([release runbook](../release/runbook.md)), fetched with
`gh run download <run id> --name <artifact> --dir <directory>` and installed
with `--allow-unsigned`. Starting such a run is for whoever owns the
repository's workflows; this tool does not do it. The baseline is a published
release's assets (`gh release download`), always installed with its
signature verified. Copying the sets on, copying the output directory off and
the machine's own lifecycle are the operator's.

## The index

One JSON object per line, one line per subject per session, newest last.
[`config/schemas/validation_run_index_line.json`](../../config/schemas/validation_run_index_line.json)
is the shape. A subject is one thing a session ran:

| Subject | Derived from | Metrics | Checks |
| --- | --- | --- | --- |
| `lifecycle` | the harness's `lifecycle-report.json` | stages passed, stages total, harness wall time | each stage's status |
| `doctor` | `tensorplate doctor --output json` | findings, failing findings | each finding's status, and whether the agent's environment file names an interpreter for the backend |
| `streaming-latency` | the slice measurement client's `record.json` | for each mode and measurement the number of samples and of misses, and the 50th, 95th and 99th percentile in milliseconds, none of them when no session gave a sample | each mode's session outcomes, `none` with the reason for a mode that has no session, or `not run` for a mode the record did not run |
| a candidate, for example `stt-whisper` | the recipe's `record.json`, and the pass's `cold-deploy.json` when there is one | runner load time, deploy wall time, the first request's exchange time, each fixture's median real-time factor and exchange time, per memory window and domain the consumed maximum and each process's peak, and the cold-cache deploy time | the bundle's name, each negative case's outcome with the code observed, teardown, deploy, the cold deploy, the two status snapshots, each memory window's status |

A line is derived, never written by hand:

```sh
tools/validation/validation-pass.py index \
  --date 2026-10-01 --session "L4 session, first of October" \
  --row ubuntu2404-x86-l4-g2s8 --machine "g2-standard-8, one NVIDIA L4" \
  --build v0.3.1-rc.1 \
  --lifecycle-report lifecycle/lifecycle-report.json \
  --qualification-record stt-whisper=whisper/record.json \
  --qualification-record tts-kokoro=kokoro/record.json \
  --doctor doctor.json --interpreter-override absent --speech-family in-assets \
  --out lines.jsonl
```

`--setting KEY=VALUE` records anything that makes the run differ from a
default install, such as a raised deadline; a value that reads as a number
is stored as one. `--run-suffix` keeps run ids apart when a session runs a
subject twice: index each run with its own suffix.

Three things in a line are the operator's word and not read from a file: the
subject a record is indexed as, `--interpreter-override` and
`--speech-family`. The bundle's name is recorded as a check so that a record
indexed under the wrong subject shows up in the comparison. `run` reads the
override from the agent's environment file itself. `--cold-deploy
SUBJECT=PATH` adds a pass's cold-deploy measurement to the subject's line.

Derivation fails closed:

- A value that was not measured yields no metric. A memory window that is
  not `measured` contributes none, a null maximum contributes none, and a
  deploy that did not succeed has no deploy time.
- `first_request_ms` is the recipe's first request after the deploy: the
  first fixture's first timing sample. There is none when that sample failed.
- `deploy_wall_cold_cache_ms` is there only when the cold deploy succeeded
  with the page cache dropped and the agent reported the same bundle digest
  for it as for the record's own deploy. Two different digests are an error.
- A field the source's own schema requires and that is absent is an error.
  So are a record whose provenance is not `recorded`, a lifecycle report for
  another row, a record holding two runner load timings, anything a source
  names twice (a stage, a fixture, a negative case, a finding, a process
  role), and doctor output whose failing total is not the count of its
  failing findings.
- A metric is a finite number: NaN and Infinity are refused when a line is
  written and when it is read.
- `result` is the source's own status. For `doctor` it is `pass` only when
  nothing fails, the environment file names no interpreter, and
  `platform_row`, `python_pytorch_runtime`, `runner_profiles`,
  `runner_profile_dependencies` and `runner_launch_environment` each have a
  status the speech family mode accepts (the table under "The speech runtime
  family"). A warning or a skipped check does not count as failing in
  doctor's own total, so the table is what refuses it. A build whose doctor
  does not report one of them records it as `absent`.

## The comparison

```sh
tools/validation/validation-pass.py compare \
  --previous runs.jsonl --current lines.jsonl
```

Each current line is compared with the newest earlier line for the same row
and subject. Every metric and check gets one status:

| Status | Meaning |
| --- | --- |
| `unchanged` | equal, or a time, memory or real-time-factor value within the tolerance |
| `improved`, `regressed` | such a value moved down or up by more than the tolerance (`--tolerance-pct`, 10 by default) |
| `changed` | a check's outcome, the result or a count differs |
| `missing` | the earlier line has it and the current one does not |
| `new` | only the current line has it |
| `note` | the build or a setting differs; the runs are compared anyway |

| Exit | Meaning |
| --- | --- |
| 0 | nothing moved |
| 1 | a value moved by more than the tolerance, up or down, or an outcome changed |
| 2 | no verdict: a value or a subject is missing, a line or an input is malformed, a subject has no earlier line, or the tool failed |

A missing value is never reported as unchanged, and a subject the previous
session ran and this one did not is reported as missing. A subject run for
the first time is refused unless it is named with `--new-subject`. The
lines of the pass itself are never the earlier line, so an index the pass
has already been appended to gives the same answer.

Anything that moved exits 1, whether it got better or worse. A value that
drops sharply is as much a finding as one that rises: memory that halves
can mean the model loaded somewhere else. The reader decides, and the next
pass compares against the new line.

## What the checks cover

| Capability | Where a pass shows it |
| --- | --- |
| A deploy that fails at load reports the runner's typed code | `negative.oom_at_load`, `negative.unsupported_voice` and `negative.corrupt_artifact_digest` read `matched (<code>)` |
| A rollback is accepted right after a failed deploy | `teardown` reads `ok (rollback)` and `status_after_negatives` reads `ok` |
| Speech profiles are served without an interpreter override | `interpreter_override` reads `absent`, and the three `runner_*` findings read `ok` |
| A deploy from a cold page cache succeeds | `cold_deploy` reads `ok`, and `deploy_wall_cold_cache_ms` is compared with the pass before |
| The first request after a deploy | `first_request_ms` is compared with the pass before |

The three `runner_*` findings come with the doctor change that reports
installed runner profiles; a build without it records them as `absent` and
its `doctor` line cannot be `pass`.

Streaming latency is a subject of its own, `streaming-latency`, and the first
pass that carries it names it with `--new-subject`. The line's settings carry
`streaming_iterations` and what each mode that ran was sent: the clip's
`streaming_stt_samples`, `streaming_stt_audio_bytes` and
`streaming_stt_frame_ms`, and the segment's `streaming_tts_text_bytes`.
Percentiles over different numbers of sessions, or over another clip or
text, are not the same measurement, and the comparison notes the setting that
differs. The measurement is described in
[`slice-measurement.md`](slice-measurement.md); no serving worker answers it
yet, so a pass without a stream endpoint names the gap with a
`streaming-latency` step that is `not_run`.
