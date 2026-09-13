# Cloud-row validation runbooks

The two Ubuntu 24.04 x86_64 rows are validated on a cloud VM the
operator starts themselves. Unlike the Jetson and the MacBook, the
machine is disposable and billed by the minute, which shapes this
procedure: everything that can refuse a run refuses it *before* the
first install, and the harness restores nothing when it finishes.

`tools/validation/ubuntu-l4-cloud-lifecycle.sh` **provisions nothing**.
It does not create, start, resize or delete any cloud resource, and it
carries no project, zone or account identifier. Starting and stopping
the VM is a separate, deliberate act.

## What a run produces

One `lifecycle-report.json` per row, valid against
`config/schemas/lifecycle_report.json`, written to the evidence
directory along with the stage logs it cites and an
`artifact-digest.txt` naming the artifact set that was installed.

**A run today reports `incomplete`, and the release gate refuses the
row.** That is the accurate state rather than a defect:

| Canonical stage | Cloud rows |
| --- | --- |
| install | covered |
| upgrade | **skipped** — no published amd64 predecessor |
| deploy-smoke | covered |
| status-logs | covered |
| rollback | **skipped** — no published amd64 predecessor |
| restart | covered |
| crash-loop | **skipped** — separate change |
| offline | **skipped** — separate change |

No released tag carries an amd64 runtime package set: `v0.1.x` published
only the CLI for that architecture. So on these rows there is nothing to
upgrade *from* and nothing to roll back *to* until a release ships one,
at which point the next release can be validated against it. The harness
records that as the skip reason rather than building a local baseline
and calling the result an upgrade.

## What a passing run does and does not prove

It proves the candidate installs through the shipped installer on this
OS, that the services come up, that doctor's `platform_row` resolves
this row **by live detection** with nothing failing, that the control
plane admits a bundle and runs a worker for it, that an inference
request round-trips through that worker, that `tensorplate status`
answers and still reports the deployment, that the journal carries the
services' output, and that both services restart with the deployment
re-warmed from durable state.

Three things it records rather than asserts, because asserting them
would claim more than the run establishes:

- **Worker supervision.** The shipped amd64 agent config declares no
  supervision block, so the run records `not_configured` rather than a
  healthy supervisor. On these rows that check is a statement about the
  configuration, not about a supervisor being exercised.
- **`tensorplate logs`.** On a real package install it exits 2 with
  `no log_source.path configured`. The CLI does not read the packaged
  `/etc/tensorplate/cli.json` unless `--config` or
  `TENSORPLATE_CLI_CONFIG` points at it; by default it uses built-in
  settings, which name no log source. (Every other command still works
  because those defaults happen to match the packaged socket.) Behind
  that is a second gap: the file the packaged config names is one
  nothing in the product writes, since both services log to the
  journal. The exit status is filed as `logs-command.exit` and the stage
  requires the journal capture instead — a product gap the run surfaces,
  not a validation failure.
- **The accelerator.** See below.

It does **not** prove the accelerator computed anything. The
deploy-smoke bundle selects the device-neutral `fixture` backend
profile, because there is no CUDA fixture backend to select yet. The
run exercises admission, the worker launch and the inference path
against the real installed appliance; it executes no CUDA kernel. Do not
describe a run of this harness as GPU validation.

## Prerequisites

1. **A running VM** with an NVIDIA accelerator matching the row, running
   Ubuntu 24.04 on x86_64, reachable over SSH, with an operator account
   that has sudo. The harness refuses every other host.

2. **The NVIDIA driver installed**, such that
   `/proc/driver/nvidia/version` is readable. The installer treats a
   missing driver as advisory; this harness treats it as fatal, because
   a run that cannot see the accelerator cannot produce evidence for a
   row whose whole subject is that accelerator.

3. **An operator account that can reach the agent's control socket.**
   The harness refuses to run as root, so every CLI call is made as the
   operator. The agent creates `/run/tensorplate/agent.sock` owned by
   the `tensorplate` group with group-only access, so add the operator
   to that group after installing and start a new session:

   ```bash
   sudo usermod -aG tensorplate "$USER"
   ```

   Without it the install stage fails on doctor's `agent_reachable`
   finding, which is the correct outcome but an expensive way to learn
   about a group membership.

4. **PyTorch importable by the descriptor's interpreter** — by default
   `/usr/bin/python3`, pinned in the backend descriptor's
   `python.interpreter` field. This is a prerequisite rather than a step
   the harness performs, because the installer runs `tensorplate doctor`
   at the end of a runtime install and refuses a critical finding;
   `python_pytorch_runtime` is probed unconditionally on Linux.

   Ubuntu 24.04 marks its system interpreter externally-managed
   (PEP 668), so the command from
   [`python-pytorch-backend.md`](../install/python-pytorch-backend.md)
   needs one of:

   ```bash
   # On a disposable validation VM, installing into the system
   # interpreter is acceptable and keeps the descriptor's default path.
   sudo /usr/bin/python3 -m pip install --break-system-packages torch
   ```

   or a virtualenv with `TP_PYTHON_PYTORCH_EXECUTABLE` pointed at it.
   Unlike the CPU-only CI smoke, do **not** use the CPU wheel index
   here: this row has an accelerator, and the default index resolves to
   the CUDA build.

   Verified on an L4 host with the 580 driver: a plain `pip install
   torch` into the system interpreter is refused with
   `externally-managed-environment`, and the command above installs a
   CUDA 13.0 build for which `torch.cuda.is_available()` is true and the
   device reports as `NVIDIA L4`. NumPy is not required;
   `python_pytorch_runtime` is ok without it. Verify
   `python3 -c 'import torch'` before starting.

5. **A verified candidate artifact set** copied onto the VM: `install.sh`,
   the artifact manifest, `SHA256SUMS`, and the amd64 `.deb` packages.
   The harness re-verifies the set against `SHA256SUMS` before it
   installs anything, and hashes that file as the run's artifact digest.

## Running it

```bash
tools/validation/ubuntu-l4-cloud-lifecycle.sh \
  --assets-dir <candidate artifacts> \
  --tested-version <bare X.Y.Z> \
  --evidence-dir <new or empty dir> \
  --confirm RESET-TENSORPLATE
```

`--tested-version` is the bare release version the evidence authorizes —
`0.2.1`, never `0.2.1-rc.1` or `0.2.1~rc.1`, even when the artifacts
under test came from a candidate. The harness refuses the candidate
spellings rather than accepting them.

Add `--allow-unsigned` for a candidate build with no published
signature, and say so in the pull request: the digest then identifies an
artifact set nobody outside that machine can resolve.

Check eligibility without touching the host first — this installs
nothing, writes nothing, and is worth doing before the VM has been
running long:

```bash
tools/validation/ubuntu-l4-cloud-lifecycle.sh \
  --assets-dir <candidate artifacts> \
  --tested-version <bare X.Y.Z> \
  --evidence-dir <new or empty dir> \
  --preflight-only --confirm RESET-TENSORPLATE
```

The harness purges TensorPlate packages and state before installing and
**does not restore them**, which is why it demands the confirmation
token. Run it on a VM you are willing to delete.

## Filing the evidence

File the report and its stage logs under
`docs/validation/evidence/<version>/<row_id>/`. **Sanitize first** — see
that directory's README.

What a real run on an L4 host actually carried, scanned for instance
names, project ids, instance ids, internal addresses, account names and
GPU UUIDs:

| File | Carries |
| --- | --- |
| `agent-journal.txt`, `observability-journal.txt` | the instance host name, on every line |
| `install.log` | the operator's account name, in the assets path the installer echoes |
| `lifecycle-report.json` | nothing on a passing run — but a **failing** stage's `detail` is the tail of its log, so a failed install carries the host name from the journal lines it quotes |
| `doctor.json`, `status.json`, `deploy-result.json` and the rest | nothing |

So the report itself is not automatically clean: check every `detail`
before filing a run that did not pass. Put the assets directory
somewhere without an account name in its path to keep it out of
`install.log`.

Delete the VM when the run is done.

## Producing the candidate artifact set

No published release carries an amd64 runtime set yet, so a run today
validates a snapshot built from source. `build-release-artifacts.sh
--snapshot` does that, but its defaults are the arm64 Jetson release's,
and three of them differ from the amd64 release build. Building on the
host itself, as the first real run did:

- Install `shellcheck` first. The script validates the installer with it
  and refuses to start without it.
- Match the amd64 release's CMake configuration:
  `TP_ENABLE_TENSORRT=OFF TP_REQUIRE_TENSORRT_SDK=OFF`. The default
  requires the TensorRT SDK, which an x86_64 host does not have.
- Pass `CFLAGS=-gdwarf-4 CXXFLAGS=-gdwarf-4`. The release sets this; a
  current clang emits DWARF 5 by default, which `dh_dwz` rejects when it
  reaches the serving worker.
- The installer requires the manifest to be named
  `tensorplate-*-artifacts.json`. Pass that name to `--manifest`, or
  rename it and correct its line in `SHA256SUMS` — the digest of the
  bytes does not change, only the file name column.

A candidate built on the validation host means that host carries a
build toolchain, which a release install would not. Say so when filing
the evidence.

A Google Deep Learning VM image on Ubuntu 24.04 with the 580 driver
exists (`pytorch-2-9-cu129-ubuntu-2404-nvidia-580`) and would skip the
driver install and reboot. It has not been used for this harness yet:
check that its PyTorch is importable by `/usr/bin/python3`, which the
backend descriptor uses, before relying on it.

## What a failed run is worth

The same as everywhere else: the report names the stage, attaches its
log, and that is what a fix is written against. Re-run after fixing
rather than editing the report. Re-running into the same evidence
directory is refused; the harness requires a new or empty one, so a
retry cannot inherit a previous attempt's stage logs or digest.
