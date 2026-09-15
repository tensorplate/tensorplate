# Cloud-row validation runbooks

The two Ubuntu 24.04 x86_64 rows are validated on a cloud VM the
operator starts themselves. Unlike the Jetson and the MacBook, the
machine is disposable and billed by the minute, which shapes this
procedure: host prerequisites are checked before the first install,
and the harness leaves the candidate installed when it finishes. It
restores the configuration changed by crash-loop testing, but does not
restore the install or state removed at the start of the run.

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
| crash-loop | covered |
| offline | **skipped** — GCE platform detection requires live metadata |

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
request returns the expected fixture echo through that worker, and that
`tensorplate status` answers and still reports the deployment. Both
services must have actual journal entries from their current invocation;
an empty capture or entries from an earlier invocation do not pass.

After restarting both services, the harness requires new service PIDs,
the same active deployment in status, a healthy serving endpoint, and
another successful inference with the expected echo. This checks that
the deployment can serve again after being re-warmed from durable state.
The live health and inference results are filed in `restart-result.json`
alongside `status-after-restart.json`.

**crash-loop** replaces `/etc/tensorplate/agent.json` with invalid JSON
and restarts the agent. It passes when systemd retries the agent at
least once and then gives up: the unit ends `failed` and its restart
count stops changing across a sample longer than `RestartSec`. The
journal must also show the agent refusing the config on at least two of
those starts, so the loop is known to be about the config. The original
config is restored whether or not those checks pass. The agent is then
started again and must answer health and inference for the same
deployment. Filed as `crash-loop-result.json`, `crash-loop-journal.txt`
and `crash-loop-recovery.json`.

Cleanup also attempts restoration on `SIGINT`, `SIGTERM`, `SIGHUP`, and
shell exit. If restoration fails, the run fails and retains the backup,
with its path reported for manual recovery. Uncatchable termination such
as `SIGKILL` cannot run cleanup.

**offline is deferred.** On GCE, both the agent's platform detection and
`tensorplate doctor` query `169.254.169.254` for the machine type. Removing
network access makes that source unreadable, so doctor cannot resolve
the row. Offline validation requires product support for trustworthy
identity detection without network access; exempting metadata or treating
an undetected row as a pass would not establish that behavior.

The harness records this dependency as the offline skip reason. It does
not install network drop-ins or produce offline pass artifacts. The
report remains `incomplete`, and cannot satisfy the release lifecycle
evidence gate, until offline and the other skipped stages are implemented
and validated.

The first reported L4 hardware run passed install, deploy-smoke,
status-logs and restart using the earlier assertions. The stronger
journal and post-restart health and inference checks, and the
crash-loop stage, have not yet been run on hardware; T4
validation of the current harness remains pending.

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
   needs the externally-managed override on this disposable host:

   ```bash
   # On a disposable validation VM, installing into the system
   # interpreter is acceptable and keeps the descriptor's default path.
   sudo /usr/bin/python3 -m pip install --break-system-packages torch
   ```

   A virtualenv selected only by `TP_PYTHON_PYTORCH_EXECUTABLE` does not
   satisfy this prerequisite. That variable selects the serving
   sidecar's interpreter; both this harness's preflight and doctor's
   packaged backend probe still require PyTorch in `/usr/bin/python3`.
   Unlike the CPU-only CI smoke, do **not** use the CPU wheel index
   here: this row has an accelerator, and the default index resolves to
   the CUDA build.

   Verified on an L4 host with the 580 driver: a plain `pip install
   torch` into the system interpreter is refused with
   `externally-managed-environment`, and the command above installs a
   CUDA 13.0 build for which `torch.cuda.is_available()` is true and the
   device reports as `NVIDIA L4`. NumPy is not required;
   `python_pytorch_runtime` is ok without it. Verify
   `/usr/bin/python3 -c 'import torch'` before starting.

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
the [release evidence rules](evidence/v0.2.1/README.md).

Sanitize a copy on your workstation, then scan it with the VM's name and
FQDN, the account name, the project id and number, the instance id and
the zone listed in a literal file kept outside the repository:

```bash
tools/validation/check-evidence-publication.sh \
  --literals <literal file outside the repository> \
  docs/validation/evidence/<version>/<row_id>
```

File only on exit 0. Findings name a file, a line and a class, never the
value; the README lists the synthetic value each class accepts. What a
run's files are known to carry:

| File | Carries |
| --- | --- |
| `agent-journal.txt`, `observability-journal.txt`, `crash-loop-journal.txt` | JSON journal records with host metadata (`_HOSTNAME`, `_MACHINE_ID`, `_BOOT_ID`, `__CURSOR` and more) beside the service's messages; keep only `MESSAGE`, `PRIORITY`, `SYSLOG_IDENTIFIER`, `UNIT`, `_PID`, `_SYSTEMD_UNIT`, `_SYSTEMD_INVOCATION_ID` and `__REALTIME_TIMESTAMP` |
| `install.log` | short-format journal lines prefixed with the host name, and the operator's account name in the assets path the installer echoes |
| `packages.txt` | package descriptions carrying planning identifiers, which do not belong in evidence |
| `lifecycle-report.json` | a **failing** stage's `detail` is the tail of its log and copies whatever that tail quotes |

Put the assets directory somewhere without an account name in its path
to keep it out of `install.log`.

Delete the VM when the run is done.

## Producing the candidate artifact set

No published release carries an amd64 runtime set yet, so a run today
validates a snapshot built from source. Build it on the host itself:

```bash
tools/release/build-release-artifacts.sh --snapshot --arch amd64 \
  --artifacts-dir <assets-dir>
```

- Install `shellcheck` and `clang` first. The script validates the
  installer with shellcheck and configures the serving worker with
  clang++, and refuses before compiling anything if either is missing.
- The serving worker's CMake configuration comes from
  `tools/release/amd64-build-profile.sh`, the file the release job's
  amd64 build reads: clang, `-gdwarf-4`, TensorRT off with no SDK
  requirement, and the python_pytorch sidecar on. Nothing needs to be
  set in the environment, and `VCPKG_ROOT`, `VCPKG_INSTALLATION_ROOT`
  and `TP_CMAKE_TOOLCHAIN_FILE` must be unset: with any of them the
  builder adds a CMake toolchain file the release build does not use.
  `TP_ENABLE_TENSORRT`, `TP_REQUIRE_TENSORRT_SDK`, `TP_ENABLE_LIBTORCH`
  and `TP_ENABLE_PYTHON_PYTORCH_SIDECAR` are refused on amd64, and so is
  a build directory already configured with another compiler: remove it
  or pass another `--build-dir`.
- The manifest and `SHA256SUMS` are written into the assets directory
  under the names `install.sh --local-artifacts` reads. Omit
  `--manifest` and `--checksums`; any other path is refused.
- The release job builds on Ubuntu 22.04 so its packages install on
  both 22.04 and 24.04. A snapshot built on 24.04 takes 24.04's glibc as
  its floor and installs on 24.04 only, which is enough for these rows.

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
