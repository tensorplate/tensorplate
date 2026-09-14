# Cloud-row validation runbooks

The two Ubuntu 24.04 x86_64 rows are validated on a cloud VM the
operator starts themselves. Unlike the Jetson and the MacBook, the
machine is disposable and billed by the minute, which shapes this
procedure: host prerequisites are checked before the first install,
and the harness leaves the candidate installed when it finishes — or
the baseline, when upgrade and rollback ran. It restores the
configuration changed by crash-loop testing, but does not restore the
install or state removed at the start of the run.

`tools/validation/ubuntu-l4-cloud-lifecycle.sh` **provisions nothing**.
It does not create, start, resize or delete any cloud resource, and it
carries no project, zone or account identifier. Starting and stopping
the VM is a separate, deliberate act.

## What a run produces

One `lifecycle-report.json` per row, valid against
`config/schemas/lifecycle_report.json`, written to the evidence
directory along with the stage logs it cites and an
`artifact-digest.txt` naming the candidate artifact set that was
installed.

**A run today reports `incomplete`, and the release gate refuses the
row.** That is the accurate state rather than a defect:

| Canonical stage | Cloud rows |
| --- | --- |
| install | covered |
| upgrade | covered with `--baseline-assets-dir`; **skipped** without it |
| deploy-smoke | covered |
| status-logs | covered |
| rollback | covered with `--baseline-assets-dir`; **skipped** without it |
| restart | covered |
| crash-loop | covered |
| offline | **skipped** — GCE platform detection requires live metadata |

Upgrade and rollback need a baseline: a published, signed release whose
amd64 runtime set is older than the candidate's. `v0.1.x` published only
the CLI for that architecture, so the first release that can serve is
`v0.2.1-rc.1`, once it is published. Without `--baseline-assets-dir` the
harness skips both stages and names the option in the skip reason.

A local build or a snapshot is never a baseline. Preflight refuses a
baseline whose manifest records a local source snapshot or an unreleased
build, and the harness installs the baseline with its signature verified
even when the candidate is run with `--allow-unsigned`. A manifest's
provenance label alone does not show a release was published — every
non-snapshot build carries the same label — but the installer's
signature check against the release workflow's identity does.

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

**upgrade and rollback** run after crash-loop, so the five stages above
are always about a clean candidate install. A failed upgrade ends the
run there, and the report has no rollback record.

**upgrade** purges the candidate, installs the baseline through the
baseline's own `install.sh`, and deploys the smoke bundle on it. The
baseline's doctor output is filed as `doctor-baseline.json` with its exit
status, not asserted: the installer already refuses a critical finding,
and a working deploy on the baseline is what shows it is a place to move
from. A defect fixed only in the candidate must not fail the candidate's
run. The harness then appends a newline to `/etc/tensorplate/cli.json`,
as an operator edit to a conffile, and runs the candidate's `install.sh`
over the running baseline. It passes when:

- every runtime package is installed at exactly the candidate's version,
  and no other TensorPlate package is installed apart from
  `tensorplate-apt-source`;
- both services run under new PIDs, brought back by the installer alone —
  the harness starts nothing itself, because the packages do not;
- the edited `cli.json` keeps its bytes;
- doctor is green and resolves the row, as in install;
- the deployment made on the baseline answers health and inference on
  the candidate without being deployed again, re-warmed from the
  baseline's durable state within the installer's own readiness wait.

**rollback** follows the procedure in
[`docs/install/lifecycle.md`](../install/lifecycle.md). It refuses to
start if `/var/lib/tensorplate/state.bak` already exists, stops both
services, moves durable state aside to `state.bak`, and removes — never
purges — every installed TensorPlate package except
`tensorplate-apt-source`. Every package, because any newer one left
behind makes the older installer's `apt-get -y` a refused downgrade.
Before installing the baseline fresh through its `install.sh`, it
requires that no TensorPlate package other than `tensorplate-apt-source`
is still installed and that the agent's conffiles were kept.
It passes when exactly the baseline's packages are installed, the edited
`cli.json` still has its bytes, `state.bak/state.json` is preserved, the
rolled-back agent answers with no active or previous deployment — it did
not load the newer agent's state — and a fresh deploy answers health and
inference.

A run with a baseline ends with the baseline installed.

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

6. **For upgrade and rollback, the baseline release's assets** in their
   own directory: `install.sh`, its one `tensorplate-*-artifacts.json`,
   `SHA256SUMS`, and every other file `SHA256SUMS` lists, not only the
   amd64 packages, because the harness checks the whole list. The
   installer fetches the signature bundle when it is absent, so the VM
   needs network access to the release. The harness hashes this
   `SHA256SUMS` into `baseline-digest.txt`, refuses to install either set
   if its `SHA256SUMS` has changed since then, and refuses a baseline
   whose runtime packages are not all older than the candidate's.

## Running it

```bash
tools/validation/ubuntu-l4-cloud-lifecycle.sh \
  --assets-dir <candidate artifacts> \
  --tested-version <bare X.Y.Z> \
  --evidence-dir <new or empty dir> \
  --confirm RESET-TENSORPLATE
```

To exercise upgrade and rollback, add
`--baseline-assets-dir <baseline release assets>`.

`--tested-version` is the bare release version the evidence authorizes —
`0.2.1`, never `0.2.1-rc.1` or `0.2.1~rc.1`, even when the artifacts
under test came from a candidate. The harness refuses the candidate
spellings rather than accepting them.

Add `--allow-unsigned` for a candidate build with no published
signature, and say so in the pull request: the digest then identifies an
artifact set nobody outside that machine can resolve. It never applies
to the baseline.

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

The first L4 run's logs were scanned for instance names, project ids,
instance ids, internal addresses, account names and GPU UUIDs. The
current harness captures raw JSON journal records through `sudo`, up to
100 entries per service from its current invocation. These richer
captures require a fresh sanitization pass; the earlier scan does not
cover them.

| File | Carries |
| --- | --- |
| `agent-journal.txt`, `observability-journal.txt`, `crash-loop-journal.txt` | raw journal metadata and service messages in JSON records; inspect every field, including host identifiers, before publishing |
| `install.log`, `upgrade.log`, `rollback.log` | may include the operator's account name in the assets paths the installers echo; `upgrade.log` and `rollback.log` carry both the candidate's and the baseline's |
| `checksums.txt`, `baseline-checksums.txt`, `baseline-digest.txt`, `upgrade-path.json` | the file lists and digests of both sets, and which release tags and package versions the upgrade moved between |
| `packages-baseline.txt`, `packages-after-upgrade.txt`, `packages-after-remove.txt`, `packages-after-rollback.txt` | the TensorPlate packages dpkg listed at each step |
| `doctor-baseline.json`, `doctor-after-rollback.json` and their `.exit` files, `doctor-after-upgrade.json` | doctor on the baseline, filed; doctor after the upgrade, asserted |
| `upgrade-baseline-deploy.json`, `status-after-upgrade.json`, `upgrade-result.json`, `status-after-rollback.json`, `rollback-result.json` | the live results on the baseline, after the upgrade and after the rollback |
| `lifecycle-report.json` | a **failing** stage's `detail` is the tail of its log and may copy identifiers from the commands or journal records it quotes |
| `doctor.json`, `status.json`, `deploy-result.json` and the rest | no identifiers were found in the earlier run; scan every current output as well |

So the report itself is not automatically clean: check every `detail`
before filing a run that did not pass. Put both assets directories
somewhere without an account name in their paths to keep it out of the
stage logs.

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
