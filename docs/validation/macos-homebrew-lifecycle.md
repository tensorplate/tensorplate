# macOS Homebrew Lifecycle Validation

This runbook validates the complete Homebrew appliance on the current
in-lab Apple M-series target: a MacBook Pro with Apple M1 Pro, 16 GB unified
memory, and macOS 26. The M1 Pro run backs that exact row and is the currently
available hardware target for the broader M-series Preview compatibility
envelope; it does not claim per-SKU validation for other M-series chips. It
covers formula-graph closure, launchd behavior, filesystem and UDS contracts,
packaged-only discovery, the PyTorch MPS capability, deploy smoke, offline
checks, upgrade continuity from the CLI-only formula, rollback, and uninstall.
The harness also has a status-logs stage covering status and log output. It
was added after the 2026-08-17 record, which therefore does not include it.
That record's offline checks ran doctor, without its agent probe, and an MPS
check under a sandbox that denied all networking. The offline-runtime stage
now runs the services themselves with the network denied, and the
offline-profile preflight stage is new. Neither has a hardware record yet.
The installed-registry stage also proves that live M1 Pro detection selects
the exact Production row instead of the lower-priority M-series Preview
fallback, while retaining the fallback's 16 GiB admission ceiling.

The harness mutates Homebrew state. Close unrelated Homebrew work first and
run it only on the validation Mac. It refuses a dirty TensorPlate tap
checkout, preserves the original tap files, and restores the supplied
CLI-only baseline formula before exit. The clean-install stage removes the
entire TensorPlate formula graph, including any previously installed
components, and checks that no TensorPlate kegs remain before installing
the candidate. This prevents Homebrew from reusing a component built from
an older source archive with the same version.

## Prepare immutable inputs

Use an RC or release tag when one exists. Before the tag exists, the feature
gate may use a GitHub archive URL containing the exact feature-head commit.
Record that commit in the pull request. All six candidate formulae must share
one URL, checksum, and declared validation version.

Download the archive, compute its SHA-256 digest, copy the checked-in
templates to a temporary formula directory, and replace the placeholder URL
and checksum. For a commit-pinned rehearsal, add the same explicit
pre-release `version` line to every formula so Homebrew can order it after
the installed CLI-only baseline. Do not commit these rendered validation
formulae; the checked-in files remain release templates.

Export the exact historical CLI-only formula that matches the installed
baseline:

```bash
tap_repo="$(brew --repository tensorplate/tap)"
git -C "$tap_repo" log -S 'v0.1.2.tar.gz' --format='%H' -- Formula/tensorplate.rb
git -C "$tap_repo" show <matching-commit>:Formula/tensorplate.rb \
  > /private/tmp/tensorplate-baseline.rb
```

Confirm that the baseline command and tap checkout are clean:

```bash
tensorplate version
git -C "$tap_repo" status --short
brew trust --formula \
  tensorplate/tap/tensorplate-agent \
  tensorplate/tap/tensorplate-backend-python-pytorch \
  tensorplate/tap/tensorplate-cli \
  tensorplate/tap/tensorplate-observability \
  tensorplate/tap/tensorplate-serving
```

The tap-trust stage requires all six formulae to be trusted, each by name
or through the whole tap. The run changes those per-formula entries:
`brew uninstall` removes a tap formula's entry unless the whole tap is
trusted, and `brew install` and `brew upgrade` add one for the tap formula
they name. The harness records the six formulae's per-formula entries
before the tap-trust stage and puts back exactly that set before its clean
install, before its upgrade and on exit, re-trusting entries an uninstall
removed and untrusting any the run added. `cleanup.log` lists each change
made on exit. Trust granted here therefore survives the run.

The stage also refuses a tap on a custom remote, such as one tapped from
a local clone with `brew tap tensorplate/tap <path>`. Homebrew keys that
tap's formula trust by the remote rather than by `tensorplate/tap`, so the
harness could not put the trust back. Run against the tap on its default
GitHub remote.

On each device, take the unsandboxed network control once before the
stage needs it:

```bash
python3 tools/validation/macos_offline_runtime.py control --ports 18080,18081
```

It runs every denied operation unsandboxed and exits non-zero naming
`control_completed:<operation>` if any of them did not complete on this
host. The offline stage takes the same control and fails the same way, so
running it first turns a device whose baseline falls outside the recorded
completion sets into an answer that costs seconds rather than a stage
setup. It touches no launchd job, no Homebrew state and no package: every
socket is pinned to `lo0`. If a device does report an operation the sets
do not cover, record the outcome and widen the set for that operation
deliberately -- do not loosen the stage to get past it.

Before the mutating run, add `--preflight-only` to the command below. The
preflight writes the host, formula-pin, baseline, tap-trust, and
offline-profile artifacts but does not alter packages, tap files, or
services. The harness disables
Homebrew's automatic dependency removal for the entire run.

## Run

Use the checked-in MPS smoke fixture as the deploy input. Its verified model
artifact selects a package-private backend that performs and synchronizes a
PyTorch tensor operation on `mps` during load. The deployment cannot become
active if MPS is unavailable or the tensor operation fails. The bundle is
test data; the CLI, agent, serving worker, backend module, interpreter,
descriptor, and platform registry must all resolve from the installed
formulae, never from the source checkout.

```bash
TP_HOMEBREW_LIFECYCLE_ALLOW=1 \
  tools/validation/macos-homebrew-lifecycle.sh \
    --candidate-formula-dir /private/tmp/tensorplate-candidate/Formula \
    --baseline-formula /private/tmp/tensorplate-baseline.rb \
    --bundle-dir test/models/bundles/v0_1/mps_python_pytorch_smoke \
    --evidence-dir /private/tmp/tensorplate-macos-evidence
```

After every candidate component is linked at the expected version, the
harness writes `artifact-digest.txt`,
holding the pinned source archive's checksum and its URL — the same
`source_sha256` the formula pin and the sanitized transcript already
carry. The digest identifies the source input from which the candidate
components were built, not their compiled binary bytes. A failed graph
removal or candidate install writes no digest. A `--preflight-only` run
also writes none, because it downloads no archive.

The run is successful only when every stage in `summary.json` and
`sanitized-transcript.json` is `pass`. `host-facts.json` deliberately
excludes serial numbers, hardware UUIDs, and provisioning identifiers.
Attach the summary, sanitized transcript, host facts, formula pin, deploy
input, deploy result, status-logs result, offline-profile result and
offline-runtime result to the pull request. Keep the
raw `*.log` files local; the transcript contains only allowlisted structured
results and excludes operator paths and environment values.

The current post-hardening Apple M1 Pro evidence is committed as the
[`curated record`](./evidence/macos-homebrew-lifecycle-m1pro-2026-08-17.json)
and its
[`sanitized transcript`](./evidence/macos-homebrew-lifecycle-m1pro-2026-08-17-transcript.json).
Both were produced from the immutable implementation head recorded in the
formula pin and include the installed-agent exact-row decision plus the
owner-only runtime-directory and agent-socket assertions. The evidence-only
descendant does not alter the validated runtime. The
[`previous post-reconciliation run`](./evidence/macos-homebrew-lifecycle-m1pro-2026-08-16.json),
[`previous corrected rehearsal`](./evidence/macos-homebrew-lifecycle-m1pro-2026-08-02.json)
and
[`initial rehearsal`](./evidence/macos-homebrew-lifecycle-m1pro-2026-07-29.json)
are retained as historical evidence. Raw launchctl logs remain excluded
because they contain operator-local paths and environment values.

The MPS capability stage uses the Python interpreter inside the Homebrew
PyTorch formula and calls the packaged backend probe. The separate
deploy-smoke stage proves the package-installed sidecar itself loads through
MPS and reaches an active, ready deployment. The fixture is not a real model
and makes no SmolVLA support claim.

The status-logs stage runs after deploy smoke. It requires `tensorplate
status` to still report the smoke deployment as ready, and both launchd
stderr logs, `agent.error.log` and `observability.error.log`, to have
gained output after the launchd-start stage recorded their sizes. It then
runs `tensorplate logs --component observability`, which must read the
packaged `events.ndjson` and return an event the observability service
wrote during this run. The structured event log rotates under its retention
policy. Before starting services, the harness snapshots file identities and
byte offsets for both `events.ndjson` and its retained generation,
`events.1`. Verification reads only bytes new to this run from the active
and retained generations. An event can therefore still be verified when
rotation happens before the status check or between the CLI read and
verification, provided it remains in one of these generations. The harness
does not delete or truncate logs to establish the boundary.

The agent component is not queried because the agent writes no structured
events. Run the harness with `TENSORPLATE_CLI_CONFIG` unset, since the
launcher honours an existing value. The raw status and logs output stays
in the local `status-logs.log`; `status-logs.json` carries only the
deployment id, counts and pass results. The current status-logs stage,
including rotation handling, has not yet been validated on hardware; the
2026-08-17 record predates this stage.

The offline-runtime stage runs the installed services and CLI with the
network denied, without touching the Mac's interfaces or the operator
session. It renders a `sandbox-exec` profile from the installed
`agent.json` that denies every network operation except loopback on the
worker's serving and candidate ports (18080 and 18081) and unix sockets
other than mDNSResponder, so host names do not resolve. It refuses to
render unless the worker binds `127.0.0.1` on two distinct ports. Both
launchd jobs are stopped with `brew services stop --keep` and run again
with `brew services run --file` from plists that differ from the
formula's only by starting the program under `sandbox-exec`. launchd
keeps supervising them, and the plists in `~/Library/LaunchAgents` stay
the normal ones. Since bootstrapping a job does not wait for its process
to start, the harness reads each job at most 30 times for its initial PID,
waiting one second between pending reads, before checking sandbox state.
A job that never starts fails the stage.

Under that profile the agent must recover the deploy-smoke deployment and
log exactly one admission decision since the services restarted, for
`macos26-m1pro-16gb` with reason `none` and validated evidence. A fresh
deploy of the MPS fixture under a new deployment id, status, inference,
`tensorplate doctor` with its agent probe, and the MPS probe must all
pass under the same profile. A probe inside the sandbox must be refused
with `EPERM` for:

- a public address, the link-local metadata address, IPv4 and IPv6
  documentation addresses and `fe80::1`;
- TCP and UDP to a loopback port other than the serving ports;
- TCP and UDP from the IPv4 and IPv6 documentation addresses to each
  serving port, so only loopback is allowed there;
- a TCP listen and a UDP bind on a port other than the serving ports;
- a child process's send, and mDNSResponder.

An unsandboxed control runs every one of those operations before the
profile is applied to anything. None may be refused, so an `EPERM` in the
sandbox comes from the sandbox, and each must have completed: a send,
bind or listen that returned, the loopback connect the far end refused,
or, for the destinations pinned to `lo0`, the route lookup's answer --
which the sandbox decides ahead of, so a control that got that far got
past where the sandbox would have refused it. A control that never
completed its operation says the operation could not be made on this
host, not that the sandbox stopped it, and the probe's `EPERM` is then
unattributable. The control is judged as it is taken, so a host that
cannot provide a baseline fails the stage before it is changed, and again
when the probe is classified against it. The control must also reach
mDNSResponder. The probe's non-loopback sockets are pinned to `lo0`, so
no probe packet leaves the Mac even if the sandbox failed to enforce.

`sandbox_check` must read the agent, its serving worker and backend
sidecar, and the observability service as sandboxed with the network
denied. Each read is bracketed by a check that the process is still the
same one and has not exited, because `sandbox_check` reports an exited
pid, reaped or not, as sandboxed. Every run first proves the readback
tells a sandboxed process from an unsandboxed one, an exited one and an
unreaped one. Each of those four results is recorded from the check that
produced it and the run stops if any is unproved, so the flags
`offline-runtime.json` carries report checks that ran on the device
rather than properties the helper asserts of itself. Every internet socket a TensorPlate process holds must be
bound to loopback: the agent's process tree, the observability service
and any other process running a TensorPlate binary. The agent's tree
must hold the serving listener. A socket that was never bound has no
port and is not counted. Both
jobs must end the stage on the pid they started with, after one launchd
run. The stage then boots out both sandboxed jobs,
starts the normal ones, and requires their loaded plists to match the
formula plists and no sandboxed TensorPlate process to remain.
`offline-runtime.json` records the profile hash and ports, probe and
control results as errno names, the readback and socket results, process
counts and the admission decision, with no pids, paths or addresses.

The offline-profile stage runs in the preflight path, before anything is
installed. It renders the same profile on two ephemeral loopback ports and
runs the probe, the control and the readback controls against a local
listener. A macOS update that changes how `sandbox-exec` or its profile
language behaves therefore fails the run before Homebrew is touched. It
proves nothing about the installed services, so it is not mapped to a
canonical lifecycle stage.

The profile is weaker than an IP firewall, and these gaps are accepted:

- The profile language's `localhost` matches every address configured on
  the Mac, whatever the interface. macOS configures `fe80::1` on `lo0`, so
  `fe80::1` reached through another interface — often the LAN router's
  address — is allowed on the two serving ports.
- A service listening on a wildcard address on those ports would accept
  LAN connections. The stage's loopback-only socket assertion refuses it.
- Brokers reachable over unix sockets or XPC, such as the Docker Desktop
  socket or `nsurlsessiond`, are outside what the profile covers.
  TensorPlate uses none.

`sandbox-exec` is deprecated. The offline-profile and in-stage probes fail
the run if it stops enforcing the profile or disappears.

The full offline-runtime stage, including launchd startup, restoration and
the final unchanged-PID and one-run checks, still requires a new M1 Pro
hardware run. The recorded profile probes and fixture tests do not supply
that lifecycle evidence.

## Rollback and recovery

The normal run stops both services, removes the candidate graph, restores the
historical CLI-only formula, and verifies the original version. Homebrew
preserves `etc` and `var` content across formula removal, and the run asserts
that a state marker survives the rollback. The formula trust the run
started with is restored as described under the trust preflight above.

On a failure, or on INT, TERM or HUP, cleanup records the interrupted
stage as failed. It writes its own output and that of the commands it
runs to `cleanup.log` in the evidence directory, and copies its messages
to the terminal. A terminal that closes, or a `| tee` that a Ctrl-C also
stops, therefore does not stop or fail the restore.

Cleanup then restores the agent config, boots out any TensorPlate
launchd job still running under `sandbox-exec`, and starts the normal
jobs again if the offline stage stopped them. It ignores INT, TERM and
HUP while it does those, so a second Ctrl-C cannot leave a sandboxed
service behind. The Homebrew restore that follows can still be
interrupted. If the agent config cannot be copied back, cleanup keeps
the harness's work directory, prints the path of the good copy and fails
the run. The sandboxed jobs are never copied into
`~/Library/LaunchAgents`, so a logout or reboot also drops them.

A job's launchd label depends on the Homebrew that loaded it: current
Homebrew uses `sh.brew.<formula>`, and a job loaded by an older Homebrew
keeps `homebrew.mxcl.<formula>`. The harness reads the label from the
plist Homebrew generated in the formula's keg, and cleanup checks both
forms. To see which TensorPlate jobs are loaded, and under which label:

```bash
launchctl list | grep -E '(sh\.brew|homebrew\.mxcl)\.tensorplate-'
```

If cleanup reports that a sandboxed job is still loaded, its message
names the job. Boot it out, using the label from that message or from the
listing, and start the normal jobs:

```bash
launchctl bootout "gui/$(id -u)/<label>"
brew services start tensorplate-observability
brew services start tensorplate-agent
```

If the harness is killed with SIGKILL, or is otherwise interrupted
outside its cleanup path, boot out every TensorPlate job the listing
shows, under whichever label it has, then:

```bash
brew services stop tensorplate-agent
brew services stop tensorplate-observability
brew uninstall tensorplate tensorplate-agent \
  tensorplate-backend-python-pytorch tensorplate-cli \
  tensorplate-observability tensorplate-serving
cp /private/tmp/tensorplate-baseline.rb \
  "$(brew --repository tensorplate/tap)/Formula/tensorplate.rb"
HOMEBREW_NO_AUTO_UPDATE=1 brew install tensorplate/tap/tensorplate
git -C "$(brew --repository tensorplate/tap)" restore Formula/tensorplate.rb
```

Inspect `brew services list`, `tensorplate version`, and the tap worktree
before continuing. PyTorch and build dependencies may remain installed
because they can be shared with other formulae; do not remove them
automatically.

Cleanup restores formula trust last. A run killed with SIGKILL, or a
cleanup interrupted during its Homebrew restore, therefore leaves formula
trust as the run's last install or uninstall left it, and the
`brew uninstall` above removes the component entries again. Afterwards,
re-run the `brew trust --formula` command from the preflight above,
compare `brew trust --json=v1` with the trust you had before the run, and
`brew untrust --formula` only a TensorPlate entry you did not have then.
Do not untrust the entries the preflight granted: the next run's
tap-trust stage requires them.
