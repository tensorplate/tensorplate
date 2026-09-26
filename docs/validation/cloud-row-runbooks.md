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

**A run without `--baseline-assets-dir` reports `incomplete`, and the
release gate refuses the row.** That is the accurate state rather than a
defect:

| Canonical stage | Cloud rows |
| --- | --- |
| install | covered |
| upgrade | covered with `--baseline-assets-dir`; **skipped** without it |
| deploy-smoke | covered |
| status-logs | covered |
| rollback | covered with `--baseline-assets-dir`; **skipped** without it |
| restart | covered |
| crash-loop | covered |
| offline | covered |

Upgrade and rollback need a baseline: a published, signed release whose
amd64 runtime set is older than the candidate's. `v0.1.x` published only
the CLI for that architecture, so the first release that can serve is the
first candidate published with an amd64 runtime set under the names its
manifest lists. `v0.2.1-rc.1` is not one: its manifest and signed
`SHA256SUMS` named each package with a `~` that GitHub serves as `.`, and
its release and tag are no longer on GitHub. Without
`--baseline-assets-dir` the harness skips both stages and names the
option in the skip reason.

A local build or a snapshot is never a baseline. Preflight refuses a
baseline whose manifest records a local source snapshot or an unreleased
build. It also checks the tag's public GitHub Release without credentials,
refuses a draft or unpublished release, and matches the local `SHA256SUMS`
to the checksum asset downloaded from that release. An unavailable release
or a failed request refuses the run before any installation or removal;
retry preflight once access is restored. This check also runs with
`--preflight-only` and needs access to GitHub's API and release assets.

The harness installs the baseline with its signature verified even when
the candidate is run with `--allow-unsigned`. Signing and publication are
separate checks: the release workflow can produce a valid signature before
it publishes a release, so a local signature bundle alone is insufficient.
The verified public checksum digest stays bound to the baseline assets
when the harness records evidence and later installs them.

### The reboot stage (L4 row, from 0.3.0)

From 0.3.0 the release gate also requires the L4 row's report to carry a
`reboot` stage. It is a stage beside the canonical eight, not one of them:
no other row, and no earlier release of this one, may carry it, so every
report recorded before the rule stays valid. `check-evidence-bundles.sh`
compares versions as numbers, so 0.10.0 and a 0.3.0 candidate both count
as 0.3.0 or later.

The stage crosses a host reboot. Before it, the runner suspends the run to
a marker file (`lifecycle_suspend`). After it, a new shell resumes the run
from the marker (`lifecycle_resume`). A marker resumes once: the runner
renames it to `<marker>.resumed`, and it refuses a marker older than a
report already in the evidence directory, since another run has finished
there since. The resumed run records:

- whether the boot ID changed;
- the three sub-cases, in this order, because each consumes the state the
  one before it leaves:
  - `blocked`: metadata denied, the agent fails closed after its retry
    window;
  - `transient`: the denial lifted inside the window, the committed set
    recovers with no reprovisioning;
  - `denied_egress_resumes`: denied-egress operation resumes within the
    new boot;
- the retry window the run observed.

The gate accepts the stage only when the boot ID changed, all three
sub-cases passed, the window was recorded and every cited log exists.
**The Ubuntu cloud harness does not run this stage yet**, so an L4 run
filed for 0.3.0 or later is refused until it does. The marker, before and
after it is resumed, records the evidence directory's absolute path: keep
it outside the evidence directory and never file it.

## What a passing run does and does not prove

On these rows doctor reports `cuda_runtime = ok` with the NVIDIA driver
installed and no system CUDA toolkit: the amd64 serving worker is built
without the TensorRT adapter, and the python_pytorch sidecar's CUDA
build of PyTorch carries its own CUDA runtime in the wheel, so no system
toolkit is required. The driver is what that `ok` rests on — with no
driver the finding is `missing` even if a system CUDA toolkit is
installed, because `libcuda` comes from the driver package and nothing
installed reaches the accelerator without it. On these x86_64 rows the
driver means `/proc/driver/nvidia/version`, the same file the installer
reads for its hardware check; a host carrying only the user-mode
`libcuda.so.1` gets the driverless verdict with a message saying the
kernel module may not be loaded, so a row whose recorded value is that
is worth reading before the deploy stage. The message names the
sidecar as installed or not, so on a run without the Python backend it
says so rather than citing a wheel that is not on the host. The finding
is paths-only and does not establish that the sidecar's PyTorch can
reach the accelerator; the prerequisite below is what does.

It proves the candidate installs through the shipped installer on this
OS, that the services come up, that doctor's `platform_row` resolves
this row **by live detection** with nothing failing, that the control
plane admits a bundle and runs a worker for it, that an inference
request returns the expected fixture echo through that worker, and that
`tensorplate status` answers and still reports the deployment. Both
services must have actual journal entries from their current invocation;
an empty capture or entries from an earlier invocation do not pass.

Every journal capture is projected as it is taken. The harness reads
`journalctl --output=json` into a scratch directory, writes only
`MESSAGE`, `PRIORITY`, `SYSLOG_IDENTIFIER`, `UNIT`, `_PID`,
`_SYSTEMD_UNIT`, `_SYSTEMD_INVOCATION_ID` and `__REALTIME_TIMESTAMP`
into the evidence directory, and deletes the scratch directory whatever
the verdict on the capture was. A signal that interrupts a capture
deletes it on the way out; only a `SIGKILL` or a machine failure can
leave it behind, in a private directory under `$TMPDIR`. The host
metadata systemd attaches to every entry is therefore never recorded,
and the projected file is the run's only copy — the retention rule in
[the evidence rules](fixture-and-evidence-rules.md) applies to it as the
raw record. A line that cannot be parsed as a JSON record, including
journalctl's own `-- No entries --`, is refused rather than copied
through, and fails the stage that captured it.

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

**offline** runs after crash-loop and before upgrade. It denies both
services, and every TensorPlate CLI call it makes, all IP traffic except
the two loopback host addresses, and then requires the appliance to keep
working.

The denial is `IPAddressDeny=any` with `IPAddressAllow=127.0.0.1/32` and
`IPAddressAllow=::1/128`. It is deliberately **not** systemd's
`localhost` shorthand: that expands to `127.0.0.0/8`, which admits the
systemd-resolved stub at `127.0.0.53` and the whole DNS namespace behind
it. The probe sends a datagram to `127.0.0.53` and requires it to be
refused, and connects to it over TCP and requires the connect to go
unanswered, so a stage that went back to the shorthand fails rather than
passing with DNS still reachable.

Both services get the denial as a **runtime** drop-in under
`/run/systemd/system/<unit>.service.d/`. Nothing is written under
`/etc/systemd/system`: a persistent drop-in would outlive the run and the
host's next reboot. The drop-ins are removed on every exit path,
including `SIGINT`, `SIGTERM`, `SIGHUP` and a failed stage, and the
removal is read back from systemd rather than assumed. The cleanup never
stops at its first failure: a removal that fails for one unit does not
stop the other unit's, nor the `daemon-reload`, the restart and the
readback that follow, and the exit handler retries all of it. If the
removal still fails, the run fails and prints the drop-in paths and the
command that removes them:

```bash
sudo rm -f <drop-in paths> && sudo systemctl daemon-reload \
  && sudo systemctl restart tensorplate-agent tensorplate-observability
```

Uncatchable termination such as `SIGKILL` cannot run cleanup; a reboot
clears `/run` in that case. A drop-in left behind that way is refused by
the next run's preflight, before anything is installed, with its path
and the same removal command. Install and every other stage before
offline would otherwise run with both services denied.

Each of `status`, `doctor`, a fresh `deploy` of the smoke bundle under a
new deployment id, and `infer` runs inside its own denied transient unit
(`systemd-run --pipe --wait --collect`), as the operator with their own
groups. The stage never makes an undenied CLI call.

Configuring a denial is not enforcing one. `IPAddressDeny=` is silently
inert wherever systemd cannot install its BPF filter, and `systemctl
show` answers for a dead or nonexistent unit with **empty** property
values — which a readback that only looked for unexpected allow entries
would read as a denied unit. So the readback requires each unit to be
loaded, active and carrying an invocation id first, and the verdict on
enforcement comes from probes, starting with one in a transient unit:

- the **control** runs first, in a transient unit with no address policy,
  and every operation in it must have **completed**: each datagram sent,
  the child process run to a clean exit with its own send done, and each
  TCP connect answered, accepted or reset. A control that was refused,
  timed out, or whose child exited non-zero or never ran sent nothing, so
  it cannot show that a refusal of the same operation under the denial
  was the denial's doing. The GCE metadata service at `169.254.169.254`
  must answer it outright, over TCP and as a datagram, since the stage's
  whole claim is that the denial is what made that service unreachable.
  The helper files a control only if it meets both, so a control that
  does not is named and fails the stage as it is taken, before anything
  is denied;
- the **probe** then runs denied, against the same operations: the
  metadata service, the resolver stub on TCP and UDP, another loopback
  address, the `192.0.2.0/24` TEST-NET-1 and `2001:db8::/32`
  documentation addresses, and the same send from a child process. Every
  datagram must be refused outright with `EPERM` or `EACCES`. The two TCP
  connects must go unanswered (`timeout`), and that counts only where
  their control was answered;
- the agent socket, the serving port on `127.0.0.1` and both loopback
  host addresses must still work under the denial.

The two protocols answer differently because the kernel does. systemd's
filter is a cgroup egress program, and a packet it drops comes back from
the IP output path as `EPERM`. A UDP `sendto()` returns that to the
caller. A TCP connect does not: `tcp_connect()` in
`net/ipv4/tcp_output.c` passes on only `ECONNREFUSED` from a transmit,
and otherwise leaves the SYN queued for retransmission, so the connect
waits out its timeout. A timed-out connect is attributable to the denial
only if the same connect was answered moments earlier, so a control that
timed out as well fails the stage. The certificate files the timed-out
connects under `classification.operations_silenced_under_the_denial`,
apart from the refusals.

systemd also installs the filter on each unit separately, and on a
best-effort basis: `cgroup_apply_firewall()` in `src/core/cgroup.c`
ignores whether it worked. A filtered transient unit therefore says
nothing about a service whose own attach failed. So the datagrams are
also sent from **inside each service's own control group**, with that
service's own control taken there before the denial and its probe after
it, under the same rules. Joining a service's control group takes root.
The helper refuses any control group that is not exactly that unit's,
reads the move back from `/proc/self/cgroup`, and drops to the operator's
uid and gid before it sends anything. The move is the only change it
makes to the service, and the probe process exits before the services
are restarted.

The same holds for the transient units the CLI calls run in. Each is a
unit of its own, and giving every one of them the same properties
establishes their configuration, not any one unit's filter: a deploy
unit whose attach failed would deploy with the network reachable while
the probe unit after it was filtered. So each CLI call runs behind the
helper's `run-denied`, which sends the same datagrams from **inside that
call's own unit** first, files them as `offline-cli-probe-<call>.json`,
and classifies them against the transient control. Only if they classify
as enforced does it exec the call, in the same process and so in the same
control group, and the unit's exit status is then the call's. Otherwise
it exits 71 and the call is never made, which fails the stage. It checks
the transient control again before it sends anything: a control that
could not be a baseline says nothing about this unit, so it is refused
with exit status 1, naming the control rather than the unit, and the call
is not made either.

The control also decides what each operation can prove. On Linux the
cgroup egress filter runs *after* the route lookup, so an operation the
host has no route for answers the same way with and without the drop-in —
and the default Compute Engine VPC is IPv4-only, so the IPv6
documentation address answers `ENETUNREACH` either way there. Such an
operation is listed in the certificate under
`classification.operations_this_host_cannot_send`, and the only thing
required of the probe is that the denial did not make it start working.
Loopback destinations are never excused this way: every host routes
them, and they are what shows the shorthand was not used. Everything else
has to be refused.

Configuration is not the running service, either. `systemctl show`
answers with the unit's *loaded* configuration, which counts a drop-in
from `daemon-reload` onwards whether or not anything restarted under it —
and stops counting a removed one the same way. So each readback also
compares the unit's invocation id against the one it carried before the
policy changed, on the way in and on the way out, and a restart that
never replaced the running instance fails the stage. systemd prints both
prefix lists from a hash set, in an order that changes with each PID 1
start. The readback compares them as sets and files them in one canonical
order.

Filed as `offline-control.json`, `offline-probe.json`,
`offline-classification.json`, the per-service
`offline-unit-{control,probe,classification}-<unit>.service.json`, the
per-call `offline-cli-probe-<call>.json` for `status`, `doctor`,
`deploy`, `status-after-deploy` and `infer`, `offline-denial.json`,
`offline-restored.json` and `offline-runtime.json`.
`offline-runtime.json` states nothing it did not read back:

- its enforcement verdict comes from classifying every probe against its
  control: the transient unit's, each service's, and each CLI call's
  unit's, which is classified again rather than taken from `run-denied`;
- its four CLI verdicts come from the result files those checks filed
  only after passing;
- its allow list is what systemd reported, and must be exactly the two
  host addresses;
- the restore must have read back one removal per denied unit;
- a persistent drop-in found on either side refuses the certificate.

**Identity, and what it costs.** `tensorplate-agent` writes a
machine-type record to `/var/lib/tensorplate/state/machine-type.json` on
every start where the GCE metadata service answered. The record is bound
to the kernel boot id, the logical CPU count, `MemTotal` and the NVIDIA
display PCI ids, and offline detection uses it only while the metadata
query gives no answer (nothing in time, a refused connection, or a
transient `429` or `503`) **and** every one of those facts still
matches. The stage requires the record to exist before it denies
anything, and then requires the identity to have come from it: the
agent's `platform identity: ... source=recorded_gce_metadata
record=not_applicable` line in its own journal, and doctor's `host_os`
finding saying the machine type was recorded rather than read live.
Doctor must still resolve the row with nothing failing.

**The record's instance.** Beside the record, the agent writes
`/var/lib/tensorplate/identity/instance-binding.json` on the same starts:
the instance id the metadata service answered, the machine type, the boot
id, and the SHA-256 of the record bytes it wrote. It never logs the
instance id; its journal line is `platform instance binding: written`,
`unchanged` or `not_applicable`, among others. With the service answering, a binding
that names another instance fails detection, because the disk was moved
to or cloned into another instance, and so does one that names this
instance on another machine type than the live answer, because the
instance was given a different machine type; neither start writes either
file. Reprovision by stopping the agent, deleting both files and starting
it with the service reachable. With no answer from the service, a binding
written in the same boot must agree with the record, and one from an
earlier boot must name the record's machine type. Upgrade requires the record the baseline wrote to be
byte-identical after the candidate starts and the binding to exist;
rollback restores the record from `state.bak` before the baseline
installs, as the documented procedure does, compares the restored copy
with the one taken while the services were stopped, and requires the
record and the binding to be byte-identical once the baseline is up.
Because these stages run online and every start in the same boot rewrites
exactly the record's bytes, the checks made after an agent starts show
that each release writes the same layout, not that the file survived; the
comparison right after the restore, before any agent starts, is the one
that shows the rollback put the record back.

**Because the record is bound to the boot, offline cold boot is not
supported.** After a reboot the agent must start once with the metadata
service reachable before offline detection works at all; a host that
comes up with the network already denied has no record for that boot and
fails detection rather than silently reporting no machine type. This
stage does not claim otherwise, and the runbook's procedure is to run it
on a host that has been online since its last boot.

**A network that is merely late no longer costs the whole boot.** On a
Compute Engine host, a start that cannot establish an identity retries
the observation a bounded number of times over a bounded window before
settling a verdict, so an agent that starts a second or two ahead of
DHCP now reaches the metadata service on a later attempt and records the
machine type as usual. The window is six attempts over a twenty-second
budget on the sleep schedule, about 15.5 seconds of sleeping in the worst
case. The journal says what happened: one `platform detection retry:`
line per failed attempt, then `platform detection recovered:` reporting
which attempt answered and how long it took, `platform detection
exhausted:` reporting the attempt count and the budget when none did, or
`platform detection stopped:` when an attempt failed for a reason another
attempt cannot settle and the retry ended early. None of these lines
appears on a host that answers first time, and none appears off Compute
Engine at all.

Retried alike: nothing answering in time, a refused connection, and the
two statuses Google documents as transient, `503` (the metadata server
booting or migrating, or host maintenance) and `429` (an endpoint's rate
limiting). With a record for the current boot the start uses it and does
not retry; without one it retries. Any other answer is not retried. When
the window ends without an answer, the error names the cause class of the
last attempt and the remedy: transient unavailability, blocked access or
not reached. This is the reboot boundary the release states: after a
reboot the agent must reach the metadata service once, within this
window or on a later restart, before denied-egress operation resumes.

This narrows the window rather than removing it. A host whose network is
denied for longer than the budget, or denied outright, still has no
record for that boot and still fails detection, so the procedure above is
unchanged: run the offline stage on a host that has been online since its
last boot.

**install and upgrade stay online.** Both run the shipped installer the
way an operator does, and denying them would validate a procedure nobody
follows. Their doctor runs must show live detection: a `host_os` without
`(from GCE metadata)` fails the stage, since a recorded shape there would
mean the metadata service did not answer a host that was meant to be
online. The stubbed-appliance tests also check the other direction: no
installer runs with a denial in place, and no CLI call outside the
offline stage runs denied or in a transient unit.

**offline runs before upgrade, and has to.** Upgrade's clean baseline
install deletes `/var/lib/tensorplate`, taking the candidate's
machine-type record with it. An offline stage after upgrade would rest on
the record the baseline wrote, which is evidence about the baseline rather
than the candidate it certifies.

The report still reports `incomplete` without `--baseline-assets-dir`,
because upgrade and rollback are skipped. With a baseline, all eight
canonical stages run.

**upgrade and rollback** run after offline, so the six stages above are
always about a clean candidate install. A failed upgrade ends the run
there, and the report has no rollback record.

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
  baseline's durable state within the installer's own readiness wait;
- the machine-type record the baseline wrote is byte-identical once the
  candidate is up, and the candidate has written the instance binding.

**rollback** follows the procedure in
[`docs/install/lifecycle.md`](../install/lifecycle.md). It stops both
services, moves durable state aside to `state.bak`, copies the
machine-type record back into a fresh `state/`, and removes — never
purges — every installed TensorPlate package except
`tensorplate-apt-source`. Every package, because any newer one left
behind makes the older installer's `apt-get -y` a refused downgrade.
Before installing the baseline fresh through its `install.sh`, it
requires that no TensorPlate package other than `tensorplate-apt-source`
is still installed and that the agent's conffiles were kept.
It passes when exactly the baseline's packages are installed, the edited
`cli.json` still has its bytes, the whole set-aside `state.bak` directory
is intact, the restored machine-type record and the instance binding are
byte-identical to their copies taken with the services stopped, the
rolled-back agent answers with no active or previous deployment — it did
not load the newer agent's state — and a fresh deploy answers health and
inference.

Intact means byte for byte, file by file: with the services stopped and
before the move, the harness lists `state/` and digests every file in it,
and after the baseline install it does the same to `state.bak/` and
requires the two listings to match name for name and digest for digest. A
file that changed, that went missing, or that was added is named. It is
the directory rather than one pathname in it because a host keeps more
than the agent's `state.json` there: the agent also refreshes
`state.json.bak`, the copy it falls back to when the primary fails to
decode, and the observability unit writes `observability-snapshot.json`
beside them. A removal or an install that emptied, truncated or rewrote
any of them in place would leave the pathname a regular file, and nothing
else in the stage reads those files back — the agent check above exists
to show the older agent did **not** load them — so the digests are what
make the preservation claim mean anything. A `state/` with no `state.json` in
it is refused where the digests are taken, before anything is moved or
removed: there is no deployment state for the rollback to preserve, and
two directories holding only the rest would compare equal all the way to
a pass.

No `state.bak` from before the run survives it: install and upgrade each
delete `/var/lib/tensorplate` while clearing the host, so copy any earlier
`state.bak` off the VM before running the harness. Rollback still refuses
to start if `state.bak` exists when it begins, so the move can never
nest state inside, or replace, a directory it did not create.

A run with a baseline ends with the baseline installed.

The current harness has now been run end to end on hardware for both
rows, from the `v0.2.1-rc.3` tag commit: an L4 on `g2-standard-8` and a
single non-partitioned H100 80GB on `a3-highgpu-1g`. All eight canonical
stages passed on each, including the crash-loop stage and the stronger
journal and post-restart health and inference checks that the first
reported L4 run predates. The bundles are filed under
[`docs/validation/evidence/v0.2.1/ubuntu2404-x86-l4-g2s8/`](evidence/v0.2.1/ubuntu2404-x86-l4-g2s8/)
and
[`docs/validation/evidence/v0.2.1/ubuntu2404-x86-h100-80g-a3hg1/`](evidence/v0.2.1/ubuntu2404-x86-h100-80g-a3hg1/),
so T4 validation of the current harness is no longer pending for these
rows. Both runs carried a baseline, so both exercised upgrade and
rollback rather than reporting them skipped.

Three things it records rather than asserts, because asserting them
would claim more than the run establishes:

- **Worker supervision.** The shipped amd64 agent config declares no
  supervision block, so the run records `not_configured` rather than a
  healthy supervisor. On these rows that check is a statement about the
  configuration, not about a supervisor being exercised.
- **`tensorplate logs`.** On a real package install it exits 6
  (`unavailable`) with a hint naming `journalctl -u tensorplate-agent`.
  Both services log to the journal, so the packaged config declares no
  `log_source.path` and the NDJSON reader has nothing to read; the
  command says where the logs are instead of returning an empty
  successful read. The exit status is filed as `logs-command.exit` and
  the stage requires the journal capture instead. The harness still
  records this status rather than requiring it; requiring 6 is a
  follow-up.
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

5. **`systemd-run` available, and the host online since its last boot.**
   The offline stage runs every CLI call inside a transient unit, so
   preflight refuses a host without `systemd-run`. It also needs the
   agent to have started at least once this boot with the GCE metadata
   service reachable, which the install stage provides: the machine-type
   record it writes is bound to the boot id, so a VM rebooted into a
   denied network has nothing to resolve its row from. The agent's
   bounded start-up detection retry does not change this: it helps a
   network that is late, not one that is denied. Do not reboot the VM
   between the install stage and the offline stage.

6. **A verified candidate artifact set** copied onto the VM: `install.sh`,
   the artifact manifest, `SHA256SUMS`, and the amd64 `.deb` packages.
   The harness re-verifies the set against `SHA256SUMS` before it
   installs anything, and hashes that file as the run's artifact digest.

7. **For upgrade and rollback, the baseline release's assets** in their
   own directory: `install.sh`, its one `tensorplate-*-artifacts.json`,
   `SHA256SUMS`, and every other file `SHA256SUMS` lists, not only the
   amd64 packages, because the harness checks the whole list. The
   preflight must reach GitHub's public API and checksum asset even when
   the directory contains a signature bundle. The installer also fetches
   the signature bundle when it is absent. The harness hashes this
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
| `agent-journal.txt`, `observability-journal.txt`, `crash-loop-journal.txt` | nothing to edit: the harness projects each capture to `MESSAGE`, `PRIORITY`, `SYSLOG_IDENTIFIER`, `UNIT`, `_PID`, `_SYSTEMD_UNIT`, `_SYSTEMD_INVOCATION_ID` and `__REALTIME_TIMESTAMP` as it records it, so the host metadata systemd attaches (`_HOSTNAME`, `_MACHINE_ID`, `_BOOT_ID`, `__CURSOR` and more) is never written down. A service's own message can still quote a host name or an address |
| `install.log`, `upgrade.log`, `rollback.log` | short-format journal lines prefixed with the host name, and the operator's account name in the assets paths the installers echo; inspect both sets' paths in upgrade and rollback logs |
| `packages.txt` | the TensorPlate packages dpkg listed after the install, without descriptions |
| `checksums.txt`, `baseline-checksums.txt`, `baseline-digest.txt`, `upgrade-path.json` | the file lists and digests of both sets, and which release tags and package versions the upgrade moved between |
| `packages-baseline.txt`, `packages-after-upgrade.txt`, `packages-after-remove.txt`, `packages-after-rollback.txt` | the TensorPlate packages dpkg listed at each step |
| `doctor-baseline.json`, `doctor-after-rollback.json` and their `.exit` files, `doctor-after-upgrade.json` | doctor on the baseline, filed; doctor after the upgrade, asserted |
| `upgrade-baseline-deploy.json`, `status-after-upgrade.json`, `upgrade-result.json`, `status-after-rollback.json`, `rollback-result.json` | the live results on the baseline, after the upgrade and after the rollback |
| `lifecycle-report.json` | a **failing** stage's `detail` is the tail of its log and copies whatever that tail quotes |
| `reboot.log`, `reboot-blocked.log`, `reboot-transient.log`, `reboot-denied_egress_resumes.log` | the reboot stage's summary and each sub-case's output, when the run records the stage; a sub-case's output can quote whatever the checks it runs print |

Check every file and report `detail` before filing. Put both assets
directories somewhere without an account name in their paths to keep it
out of the stage logs. The checkout's own path is kept out by the
harness: the offline helper reports a failure it did not anticipate as
`error: <subcommand> failed unexpectedly: <exception type>`, with exit
status 70 and never a traceback or the exception's message; the
deploy-smoke bundle check names a file it cannot read by its place in
the bundle; and the bundle is copied from inside itself, so `cp` names a
file it cannot copy by a relative path. The scan above still decides: on
a checkout that root cannot read, such as a home directory on NFS with
root squashing, `python3` itself names the offline helper's path when it
cannot open it.

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
skips the driver install and reboot. The H100 run recorded for 0.2.1
used one — `common-cu129-ubuntu-2404-nvidia-580` — and preflight passed
on it, so on that image PyTorch is importable by `/usr/bin/python3`,
which the backend descriptor uses. Its apt source list carries the
image's own cuDNN, package-registry and container repositories, which
the stock Ubuntu image the L4 run used does not; that is a difference in
what the log records, not in what the harness asserts. Other images in
the family, such as `pytorch-2-9-cu129-ubuntu-2404-nvidia-580`, have not
been used here: preflight checks the interpreter either way and refuses
a host where the import fails, so let it, rather than assuming the
family behaves alike.

## What a failed run is worth

The same as everywhere else: the report names the stage, attaches its
log, and that is what a fix is written against. Re-run after fixing
rather than editing the report. Re-running into the same evidence
directory is refused; the harness requires a new or empty one, so a
retry cannot inherit a previous attempt's stage logs or digest.
