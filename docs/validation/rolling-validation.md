# Rolling validation

A rolling validation pass installs the latest build on a real machine, runs
the lifecycle harness and the candidate qualification recipe, and compares
what it measured with the pass before it. It is not release evidence: a pass
files nothing in this repository. Its records and its index stay with whoever
ran it, and a difference it finds becomes a fix or a recorded run of its own.

This page covers the run index and the comparison. Both read files a run has
already recorded, so they work the same on a pass, on a qualification run and
on a release gate bundle.

## The index

One JSON object per line, one line per subject per session, newest last.
[`config/schemas/validation_run_index_line.json`](../../config/schemas/validation_run_index_line.json)
is the shape. A subject is one thing a session ran:

| Subject | Derived from | Metrics | Checks |
| --- | --- | --- | --- |
| `lifecycle` | the harness's `lifecycle-report.json` | stages passed, stages total, harness wall time | each stage's status |
| `doctor` | `tensorplate doctor --output json` | findings, failing findings | each finding's status, and whether the agent's environment file names an interpreter for the backend |
| a candidate, for example `stt-whisper` | the recipe's `record.json` | runner load time, deploy wall time, each fixture's median real-time factor and exchange time, and per memory window and domain the consumed maximum and each process's peak | the bundle's name, each negative case's outcome with the code observed, teardown, deploy, the two status snapshots, each memory window's status |

A line is derived, never written by hand:

```sh
tools/validation/validation-pass.py index \
  --date 2026-10-01 --session "L4 session, first of October" \
  --row ubuntu2404-x86-l4-g2s8 --machine "g2-standard-8, one NVIDIA L4" \
  --build v0.3.1-rc.1 \
  --lifecycle-report lifecycle/lifecycle-report.json \
  --qualification-record stt-whisper=whisper/record.json \
  --qualification-record tts-kokoro=kokoro/record.json \
  --doctor doctor.json --interpreter-override absent \
  --out lines.jsonl
```

`--setting KEY=VALUE` records anything that makes the run differ from a
default install, such as a raised deadline; a value that reads as a number
is stored as one. `--run-suffix` keeps run ids apart when a session runs a
subject twice: index each run with its own suffix.

Two things in a line are the operator's word and not read from a file: the
subject a record is indexed as, and `--interpreter-override`. The bundle's
name is recorded as a check so that a record indexed under the wrong subject
shows up in the comparison.

Derivation fails closed:

- A value that was not measured yields no metric. A memory window that is
  not `measured` contributes none, a null maximum contributes none, and a
  deploy that did not succeed has no deploy time.
- A field the source's own schema requires and that is absent is an error.
  So are a record whose provenance is not `recorded`, a lifecycle report for
  another row, a record holding two runner load timings, anything a source
  names twice (a stage, a fixture, a negative case, a finding, a process
  role), and doctor output whose failing total is not the count of its
  failing findings.
- A metric is a finite number: NaN and Infinity are refused when a line is
  written and when it is read.
- `result` is the source's own status. For `doctor` it is `pass` only when
  nothing fails, the environment file names no interpreter, and each of
  `platform_row`, `python_pytorch_runtime`, `runner_profiles`,
  `runner_profile_dependencies` and `runner_launch_environment` is `ok`. A
  warning on one of those does not count as failing in doctor's own total,
  so the list is what refuses it. A build whose doctor does not report one
  of them records it as `absent`.

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

The three `runner_*` findings come with the doctor change that reports
installed runner profiles; a build without it records them as `absent` and
its `doctor` line cannot be `pass`.

Streaming latency has no measurement yet. When it has one it is a subject of
its own, and the first pass that carries it names it with `--new-subject`.
