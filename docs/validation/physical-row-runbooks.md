# Physical-row validation runbooks

Two rows are validated on hardware someone owns rather than provisions:
the in-lab **Jetson Orin Nano 8GB Super** and a **MacBook Pro M1 Pro**.
Both produce the same lifecycle report the cloud rows will, so the
release gate reads one format regardless of who ran it.

These are written to be executable by someone who did not write them.
Where a step needs a judgement call, it says what the call is.

## What a run must produce

One `lifecycle-report.json` per row, valid against
`config/schemas/lifecycle_report.json`, filed under the row's evidence
directory. It carries all eight lifecycle stages — a stage that did not
run appears as `skipped` **with a reason**, because an absent stage and
an unrun one must not look alike to the gate.

The report also names the version it exercised. A run that does not say
what it tested would otherwise authorize every later tag, so the gate
requires `subject.tested_version` to equal the version being released.

`<tested_version>` is the bare release version — `0.2.1`, never
`0.2.1~rc.1`. That holds even when the packages under test came from a
release candidate: a candidate's `.deb` reports `0.2.1~rc.1` so that
`apt` offers the real release as an upgrade, but the evidence is filed
against the release it stands for, not the build it was collected on.
The schema rejects the `~` form rather than accepting both spellings.

Which *build* was installed is the other half of that fact, and it is
recorded as `subject.artifact_digest`. You never type it: each harness
writes `<evidence>/artifact-digest.txt` during the run, holding the
sha256 and the name of what was hashed. The Jetson harness writes the
report itself and records the digest into it once its install stage has
passed; for the macOS harness the converter reads the file from beside
`stages.tsv`. Nothing on an installed system reports which artifact it
came from, so a digest not captured while the machine is in hand cannot
be recovered afterwards — the run has to be repeated.

For a converted report, a missing file is not fatal: the report omits
the field and the conversion says so on stderr. A file that is there but
malformed stops the conversion, because the two ways of continuing are
filing a report the release gate rejects, or filing one that looks like
a run nobody recorded a digest for.

`TP_LIFECYCLE_SOURCE_REVISION` is optional and, if set, must be the full
40-character git SHA the artifacts were built from. A tag or a short SHA
is now refused by both producers rather than written into a report that
fails at the gate.

The Jetson harness, `tools/validation/jetson-lifecycle.sh`, writes that
report natively through `tools/validation/lifecycle-stages.sh`, as the
cloud harness does: each canonical stage is one function, and a failure
is recorded against that stage. The macOS harness predates the report
and records its own stage names, so it emits the report through the
converter rather than by rewriting stage calls on code that only runs on
hardware:

```bash
tools/validation/lifecycle-report-from-stages.sh \
  <evidence>/stages.tsv <row_id> <tested_version> <harness> \
  <evidence>/lifecycle-report.json \
  <harness_stage>=<canonical_stage> ...
```

The mapping is an assertion: naming `clean-install=install` claims that
the harness's clean-install stage *is* the canonical install stage. Get
it wrong and the report lies in the gate's favour, which is the one
direction that matters. Several harness stages may name the same
canonical stage; the weakest of the results that are *present* is the
one reported, so a mapped step that never ran counts for nothing. That
is why the Jetson row no longer goes through the converter: its old
mapping could report deploy-smoke as a pass without the inference step,
and mapped status-logs to a `tensorplate logs` call that cannot succeed
on a packaged install.

A harness stage the mapping does not name is evidence for no canonical
stage. Its `fail` or invalid status makes the outcome `fail`; an unmapped
`pass` or `skipped` status leaves the canonical stages' outcome unchanged.

### The Jetson harness does not cover all eight stages yet

The Jetson runbook below cannot produce better than an `incomplete`
report today, and the release gate refuses that row. That is the
accurate state, not a defect in the runbook. The macOS mapping names
all eight stages, so a macOS report can be `pass` when all eight passed
and no unmapped stage failed or recorded an invalid status:

| Canonical stage | Jetson | macOS |
| --- | --- | --- |
| install | covered | covered |
| upgrade | covered with a baseline set | covered |
| deploy-smoke | covered | covered |
| status-logs | covered | covered |
| rollback | covered with a baseline set | covered |
| restart | covered | covered |
| crash-loop | covered | covered |
| offline | **not implemented** | covered |

Upgrade and rollback run only when the baseline options are given; a run
without them skips both with that reason. Closing the remaining gap
means adding offline to the Jetson harness itself. Until then it records
it as `skipped` with its reason, so the gate reports what is missing
rather than accepting a partial run.

## Jetson Orin Nano 8GB Super

Prerequisites:

- The device is in **Super power mode**, reachable over SSH, and has an
  operator account with sudo. The harness refuses to run as root and
  calls `sudo` for privileged steps.
- The operator is in the `tensorplate` group, in a fresh session. The
  agent's control socket is group-only, so every CLI call the harness
  makes as the operator needs it. Only the TensorPlate packages create
  that group, and a purge keeps it. On a device that has never had
  TensorPlate installed, download the assets (step 3) and install once
  first, then add the group and start a new session; the harness purges
  that install when it runs:

  ```bash
  sudo bash /var/tmp/tensorplate-<asset_tag>/assets/install.sh \
    --local-artifacts /var/tmp/tensorplate-<asset_tag>/assets --yes
  sudo usermod -aG tensorplate "$USER"
  ```

  Preflight refuses a session outside the group once the group exists.
  Without the first install there is no group to check, and the install
  stage fails on doctor's `agent_reachable` finding instead.

- The device can build the TensorRT identity bundle: a C++ compiler, the
  CUDA headers and `libnvinfer`. `tools/validation/create_trt_identity_bundle.sh`
  compiles a small TensorRT builder and runs it on the device, because an
  engine is built for the TensorRT version that will load it. The device
  is not a clean rootfs in any case — the fleet's single Jetson is also
  where the arm64 release packages are built — so say in the pull request
  that the run was made on a device carrying a build toolchain. Where
  the device under test has no toolchain, build the bundle on a Jetson
  with the same JetPack and TensorRT and pass it with `--bundle-dir`; the
  harness builds nothing when one is given. Keep that bundle, the assets
  and the evidence directory outside `/etc/tensorplate`,
  `/var/lib/tensorplate`, `/var/log/tensorplate`, `/run/tensorplate` and
  `/opt/tensorplate-validation/trt-identity`: the run deletes those after
  purging the device, so preflight refuses an input under any of them,
  including the bundle the clean-room smoke leaves under
  `/var/lib/tensorplate/validation`.
- Network access for the installer, which runs `apt-get update` and
  verifies the release signature with cosign. A run with a baseline set
  installs four times — candidate, baseline, candidate, baseline — so
  the device needs the apt mirror and the cosign bootstrap to stay
  reachable throughout, and the run takes correspondingly longer.
- A checkout of this repository on the device at the revision under
  test: the harness, the bundle generator and the response verifier run
  from it.

1. Confirm identity **before** running anything. The row names the BSP
   generation, not a revision:

   ```bash
   tensorplate doctor --output json | \
     python3 -c 'import json,sys; d=json.load(sys.stdin)
   f={x["id"]: x["message"] for x in d["payload"]["findings"]}
   print(f.get("host_os")); print(f.get("platform_row"))'
   ```

   PASS: `host_os` reads JetPack 6.2 with image identity `L4T r36.x`, and
   `platform_row` resolves `jetson-orin-nano-8gb-jp62`. A device on
   r36.4.3 and one on r36.5.0 both satisfy this — the row covers the
   JetPack 6.2 line, and a run that insisted on one revision would refuse
   the other on identical hardware.

2. Record the exact stack as row facts — these are evidence, not match
   keys, and they differ legitimately between devices on the same row.
   Record them into a separate `<row-facts>` directory, not the evidence
   directory: the harness refuses an evidence directory that is not new
   or empty.

   ```bash
   tensorplate doctor --record <row-facts>/
   nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null || true
   dpkg-query -W -f='${Package} ${Version}\n' 'nvidia-l4t-core' 'cuda-toolkit*' 'tensorrt*' 2>/dev/null
   uname -r
   ```

3. Download the candidate's published assets by tag, outside your home
   directory — the installer echoes the assets path into `install.log`:

   ```bash
   tools/validation/jetson-clean-room.sh download \
     --version <asset_tag> \
     --work-dir /var/tmp/tensorplate-<asset_tag>
   ```

   The assets land in `/var/tmp/tensorplate-<asset_tag>/assets`, checked
   against their `SHA256SUMS`; the download's own checksum record under
   the work directory's `evidence/` is not the run's evidence. The
   signature over `SHA256SUMS` is verified later, by the release's own
   `install.sh` during the install stage, with the cosign bundle
   downloaded alongside.

   `<asset_tag>` is the release **tag** whose published assets this run
   installs — `v0.2.1-rc.1` — and it is deliberately not the
   `<tested_version>` the report is filed under. A candidate's assets are
   what can be downloaded while the evidence is being collected: the
   release workflow's evidence gate blocks only the final release, so a
   candidate publishes before its evidence exists, while the final release
   waits on that evidence and is created as a draft whose assets are not
   reachable at the public tag URL.

   Download the **baseline** the same way. It is `v0.1.5`, the last
   published arm64 runtime set, and the upgrade and rollback stages are
   skipped without it:

   ```bash
   tools/validation/jetson-clean-room.sh download \
     --version v0.1.5 \
     --work-dir /var/tmp/tensorplate-v0.1.5
   ```

   `v0.1.5` is the only baseline the harness accepts for this row: the
   report's subject names the candidate alone, so nothing a gate reads
   could tell a run against an earlier candidate of the same release, or
   against another predecessor, from the path this row validates. The
   tag is still required to sort strictly below the candidate's, with a
   release candidate sorting below the release it leads to. Preflight
   refuses any other baseline tag, a candidate that is not newer than
   `v0.1.5`, a baseline whose manifest names another tag, and one whose
   manifest records it as an unreleased local snapshot.
   It also refuses one whose runtime packages are not each strictly older
   than the candidate's: the tag is release metadata, and what `apt`
   orders is the Debian version each `.deb` carries, so an older tag over
   newer packages would make the upgrade stage's candidate install a
   downgrade that `apt-get -y` refuses — after the device had already
   been rebuilt twice. The versions compared are read from each `.deb`'s
   own control field with `dpkg-deb`, not from the manifest: the release
   driver parses the manifest's `version` out of the file name and never
   reads the package, so a set whose two disagree would be ordered on a
   string `apt` does not use. The comparison is recorded in
   `upgrade-path.json`.

   The snapshot check reads fields the set's own manifest declares, so it
   keeps a locally built set out of the run but is not by itself a proof
   of publication. What establishes that here is downloading the set with
   `jetson-clean-room.sh download`, which fetches it unauthenticated from
   the public release URL that a draft's assets are not reachable at.
   (`tools/validation/check-baseline-publication.py`, which the Ubuntu
   cloud harness calls, binds the two directly; adopting it on this row
   would make preflight depend on reaching GitHub from the device and is
   follow-up work.)

   The baseline is always installed with its signature verified; there is
   no option to skip that for either set. Both releases' `install.sh` also
   read `TP_INSTALL_*` variables, and some of them turn verification off
   (`TP_INSTALL_ALLOW_UNSIGNED`, `TP_INSTALL_SKIP_SELF_CHECK`) or point it
   elsewhere (`TP_INSTALL_COSIGN`, `TP_INSTALL_REPO`), so preflight
   refuses a run whose environment sets any `TP_INSTALL_*` variable, by
   name. The upgrade's and the rollback's three installs additionally run
   the installer behind a prefix that drops every `TP_INSTALL_*` variable
   the sudo policy or PAM still hands over, and each must print the
   installer's `SHA256SUMS signature verified: signed by
   tensorplate/tensorplate release workflow` line, which is filed in
   `install-baseline.txt`, `install-upgrade.txt` and
   `install-rollback.txt`. The install stage keeps its own `install.sh`
   call unchanged, so its candidate install is covered by the preflight
   refusal alone.

4. Check eligibility first. This builds the bundle in a temporary
   directory it removes, installs nothing and writes no evidence:

   ```bash
   tools/validation/jetson-lifecycle.sh \
     --candidate-tag <asset_tag> \
     --candidate-assets-dir /var/tmp/tensorplate-<asset_tag>/assets \
     --baseline-tag v0.1.5 \
     --baseline-assets-dir /var/tmp/tensorplate-v0.1.5/assets \
     --tested-version <tested_version> \
     --evidence-dir <evidence> \
     --preflight-only --confirm RESET-TENSORPLATE
   ```

   It refuses a host that is not aarch64, not Ubuntu 22.04 or not L4T
   R36; a `--tested-version` that is not a bare `X.Y.Z`; a tag whose
   `X.Y.Z` is not the tested version; an assets directory without exactly
   one artifact manifest, whose manifest's `release.tag` is not the tag
   given, or whose files fail `SHA256SUMS`; an assets, evidence, baseline
   or bundle directory under a directory the run deletes; one baseline
   option without the other; a baseline tag other than `v0.1.5`; a
   baseline whose tag or whose runtime package versions are not strictly
   older than the candidate's, whose `.deb` files `dpkg-deb` cannot read,
   or whose manifest records it as an unreleased snapshot; a
   `TP_INSTALL_*` variable in the environment; a session outside the
   `tensorplate` group once that group exists; and a device that cannot
   build the bundle. The manifest binding matters because the installer accepts
   a signature from any release tag, so a signed set is not thereby the
   set for the tag you named.

5. Run the lifecycle harness. It refuses to start without the
   confirmation token, because it purges TensorPlate packages and state.
   **With the baseline options it leaves the baseline installed, not the
   candidate**, because rollback is the last stage; reinstall the
   candidate afterwards if the device should carry it:

   ```bash
   tools/validation/jetson-lifecycle.sh \
     --candidate-tag <asset_tag> \
     --candidate-assets-dir /var/tmp/tensorplate-<asset_tag>/assets \
     --baseline-tag v0.1.5 \
     --baseline-assets-dir /var/tmp/tensorplate-v0.1.5/assets \
     --tested-version <tested_version> \
     --evidence-dir <evidence> \
     --confirm RESET-TENSORPLATE
   ```

   It writes `<evidence>/lifecycle-report.json` itself; there is no stage
   log to convert, and neither `tools/validation/jetson-stages-from-evidence.sh`
   nor the converter is part of this procedure.

   The harness's CLI calls use a private temporary configuration targeting
   the installed agent's local socket. An operator's saved profile or
   `TENSORPLATE_CLI_CONFIG` cannot redirect them. Inference must use the
   endpoint discovered through that agent, and its reported endpoint must
   match the active deployment URL checked for health. The temporary
   configuration is removed on exit.

   **install** purges every installed `tensorplate*` package except
   `tensorplate-apt-source` — the channel bootstrap, which depends on
   nothing in TensorPlate — and requires that none remain before it clears
   `/etc/tensorplate`, `/var/lib/tensorplate`, `/var/log/tensorplate` and
   `/run/tensorplate`. A package-inventory query error stops the stage
   before state is cleared; only dpkg's explicit no-match result counts
   as an empty inventory. It installs
   through the candidate's `install.sh` with signature verification and
   without the Python backend, requires each of the five runtime packages
   to be installed at the version its candidate `.deb` declares, and
   requires doctor to report nothing failing, `platform_row` to resolve
   this row, `platform_profile` to list it among the host's candidate
   rows, and the registry, agent, socket, serving binary, path layout and
   config files to be ok. `host_os`, `accelerator_facts`,
   `tensorrt_runtime` and `cuda_runtime` are recorded, not asserted.

   **deploy-smoke** deploys the TensorRT identity bundle, staged under
   `/opt/tensorplate-validation/trt-identity` where the agent's sandbox
   can read it, and sends the bundle's own sample request. The engine
   must return its input unchanged, checked by
   `tools/validation/verify_trt_identity_response.py`; status must report
   the deployment active on the `tensorrt` backend at a loopback serving
   URL, and that endpoint's health must name the deployment. **The Python
   backend is not installed and not exercised on this row.** A pass shows
   the TensorRT serving path loads an engine and round-trips a tensor. It
   is no claim about model accuracy, throughput or accelerator
   performance, and `deploy-result.json` records that as `compute_claim`
   `none`.

   **status-logs**, **restart** and **crash-loop** are the cloud
   harness's stages with the TensorRT round trip in place of the fixture
   echo; see [cloud-row-runbooks.md](cloud-row-runbooks.md) for what each
   requires, including the config restoration crash-loop performs on
   interruption. `tensorplate logs` is recorded, not required, for the
   reason given there.

   **upgrade** clears the candidate, installs the baseline through the
   baseline's own `install.sh`, deploys `<deployment-id>-baseline` on it
   and round-trips the identity engine, appends a newline to
   `/etc/tensorplate/cli.json` as an operator edit, then installs the
   candidate over the running baseline. It requires every runtime package
   to be at the candidate `.deb`'s version, both services to come back
   with new main pids, the operator's edited conffile to survive, doctor
   to be green and to resolve this row, and the baseline's deployment to
   be serving — with no deploy of the harness's own, so what answers is
   state the candidate re-warmed. The harness issues no `systemctl start`
   or `enable` of its own around either install: the installer enables
   and starts both units, and doing it here would hide an installer that
   no longer does. It does stop them — before clearing the candidate, and
   again in the rollback — and always both together: the `v0.1.5`
   observability unit shares the agent's `RuntimeDirectory`, so stopping
   the agent alone strands it. The candidate's unit no longer shares it,
   and stopping the pair together is right whichever set is installed.
   Each of the three installs in this pair must be followed by both
   units active and the agent socket present.

   **rollback** follows [the documented procedure](../install/lifecycle.md):
   it requires the candidate to be serving `<deployment-id>-baseline`,
   refuses to start if `/var/lib/tensorplate/state.bak` already exists,
   stops both services, moves `/var/lib/tensorplate/state` aside to
   `state.bak`, files dpkg's unfiltered listing in
   `packages-before-remove.txt`, and `apt remove`s every installed
   `tensorplate*` package except `tensorplate-apt-source` — including
   `tensorplate-common`, without which the older set would be a
   downgrade that `apt-get -y` refuses. It then reads dpkg's own listing
   back, unfiltered, into `packages-after-remove.txt` and requires each
   of the four packages that ship a file under `/etc` — agent, serving,
   observability, cli — to be in dpkg's `config-files` state: a package
   the listing does not name, or names as `not-installed`, was purged and
   its conffiles are gone. `tensorplate-common` is exempt because it
   installs nothing under `/etc`, so dpkg legitimately drops it to
   `not-installed`. Every other `tensorplate*` package must be
   `not-installed` or `config-files`; `installed`, `half-configured`,
   `unpacked` and the like are packages the removal left behind.
   `tensorplate-apt-source` must be exactly as the first listing found
   it: `install.sh` never installs it, so a device set up by the steps
   above has none and must still have none, and a lab image that reaches
   the channel through it must still have it installed. The two listings
   show that, rather than the words the harness passed to `apt-get`.
   It then installs the baseline
   fresh through its own `install.sh` and requires the baseline versions,
   the operator's edit, and the set-aside `state.bak/state.json` to be
   intact. The older agent must report **no** active or previous
   deployment: the state was set aside on purpose, and what to restore
   from it is the operator's decision. A fresh `<deployment-id>-rollback`
   deploy and identity round trip is what shows the baseline serves.

   Doctor on the baseline is recorded, not asserted, in
   `doctor-baseline.json` and `doctor-after-rollback.json` with their exit
   statuses beside them. The baseline's own `install.sh` already refuses a
   critical finding, and the baseline predates `platform_row`; asserting
   more would let a finding the candidate fixed fail the candidate's run.

   Three windows in this pair can leave the device serving nothing: the
   upgrade's, from clearing the candidate until the baseline install
   succeeds; the rollback's, from stopping the candidate's services until
   the removal starts, with the candidate still installed; and the
   rollback's again, from the removal until the baseline install
   succeeds. A run that ends in any of them — a failure, or an interrupt —
   files `stranded-device.txt` beside the stage logs, prints the same text
   on stderr, and names the command to recover with; it reinstalls
   nothing by itself. It is written first to the evidence directory, so it
   survives a terminal that has gone away, and a terminal that has gone
   away does not stop the lifecycle report from being written.

   What it says is what dpkg reports when it is written, not what an
   earlier listing said: `install.sh` can fail after installing every
   package, when the services do not come up or doctor reports a critical
   finding. It names the packages dpkg lists as present, says **no
   TensorPlate installed** only when that listing names none, and says
   the listing could not be read when it could not, with the query to run.
   It says whether the baseline installer was started, and where its
   output is; when it was not and packages are still present, installing
   the baseline over them is the downgrade `apt-get -y` refuses, and the
   report says to remove them first. It says `/etc/tensorplate`
   conffiles are kept only when the removal's listing was read and showed
   them kept, and names the packages whose conffiles were lost otherwise.
   It says where durable state is: the upgrade's clearing step deletes
   `/etc/tensorplate` and `/var/lib/tensorplate` along with the packages —
   the report says whether it got that far — while the rollback sets
   durable state aside at `state.bak` first. When the rollback stopped
   before removing anything, it gives the commands that return to the
   candidate instead: moving `state.bak` back, when it was moved, and
   starting both services.

   Re-running the harness also recovers the device, because its install
   stage purges and installs the candidate from scratch — but that same
   stage deletes `/etc/tensorplate` and `/var/lib/tensorplate`, including
   a `state.bak` the message above just pointed at. Copy anything worth
   keeping elsewhere before re-running.

   **offline** is skipped with its reason in the report, so a run today
   is `incomplete` and the gate refuses the row. Offline under per-unit
   network denial is follow-up harness work. A hardware run of this
   native harness on the Jetson is deferred to release validation; the
   fixture checks do not replace it.

   The digest this run files is the sha256 of the candidate's
   `SHA256SUMS`, taken in preflight once the asset set has verified and
   written into the report only after the install stage passes. That file
   is what the install itself trusts: it is signature-verified, and the
   packages are checked against its lines. It identifies the release build
   under test rather than the local package selection, and covers the
   whole published asset set, including files this row never installs.
   The baseline is verified the same way, but its digest is filed on its
   own in `baseline-digest.txt` and in `upgrade-path.json`: the report
   attests one artifact set, and that set is the candidate.
   `upgrade-path.json` also carries the runtime package versions each
   set's `.deb` files carry, which is what preflight compared to admit
   the path, so the evidence says why the two sets form an upgrade rather
   than only which tags were named.

6. File the report, its stage logs and the recorded row facts under
   `docs/validation/evidence/<version>/jetson-orin-nano-8gb-jp62/`.
   **Sanitize before the first commit** — see that directory's README —
   then scan the sanitized copy with the device's host name, the account
   name and any other name of this machine listed in a literal file kept
   outside the repository, and file only on exit 0:

   ```bash
   tools/validation/check-evidence-publication.sh \
     --literals <literal file outside the repository> \
     docs/validation/evidence/<version>/jetson-orin-nano-8gb-jp62
   ```

   Keep the raw report and logs privately. The native harness records its
   own stage times, so there is no adapter to re-run after sanitization.
   What the harness writes:

   | File | Carries |
   | --- | --- |
   | `agent-journal.txt`, `observability-journal.txt`, `crash-loop-journal.txt` | raw journal records in JSON, including host metadata fields such as the host name and the machine and boot ids; inspect every field before publishing |
   | `install.log`, `deploy-smoke.log`, `status-logs.log`, `restart.log`, `crash-loop.log`, `upgrade.log`, `rollback.log`, `install-baseline.txt`, `install-upgrade.txt`, `install-rollback.txt`, `stranded-device.txt` | everything the stage's commands printed, and each upgrade and rollback install's own output. These carry the assets path the installer echoes — for both sets — which names an account if the assets were under a home directory, and `stranded-device.txt` names the baseline assets path in its recovery command. When services do not become ready, the harness and `install.sh` print `systemctl status` output and journal lines in the short format, and both carry the host name; inspect every line of any stage log that records a failure |
   | `host-facts.txt` | kernel release, OS name, the first line of `/etc/nv_tegra_release`, the systemd version and the power mode; no host name or serial is read, but check the release line |
   | `doctor.json`, `doctor-baseline.json`, `doctor-after-upgrade.json`, `doctor-after-rollback.json` | host OS and accelerator facts, for both the candidate and the baseline |
   | `packages.txt`, `packages-baseline.txt`, `packages-after-upgrade.txt`, `packages-before-remove.txt`, `packages-after-remove.txt`, `packages-after-rollback.txt`, `checksums.txt`, `baseline-checksums.txt`, `baseline-digest.txt`, `upgrade-path.json`, `status*.json`, `deploy-result.json`, `restart-result.json`, `crash-loop-*.json`, `upgrade-*.json`, `rollback-*.json`, `agent-cli.log` | package versions and states, file names, release tags and digests, the deployment ids and loopback serving URLs; scan them as well |
   | `lifecycle-report.json` | a **failing** stage's `detail` is the tail of its log and may copy identifiers from the commands or records it quotes |

## MacBook Pro M1 Pro

Prerequisites: Homebrew present, the tap reachable, and **no TensorPlate
services running** from a previous run — `macos-homebrew-lifecycle.sh`
restores a baseline on exit and a half-cleaned host makes its rollback
stage meaningless. `TENSORPLATE_CLI_CONFIG` must be unset in the shell
that runs the harness: the Homebrew launcher honours a value that is
already set, and the status-logs stage fails unless the CLI reads the
packaged configuration. On a failure or an INT, TERM or HUP, cleanup
puts both services back under their normal launchd jobs before
restoring the baseline, even if the terminal has closed. It writes its
output to `cleanup.log` in the evidence directory and copies its
messages to the terminal. If it reports a sandboxed job still loaded,
run the `launchctl bootout` command it prints;
`macos-homebrew-lifecycle.md` has the full recovery.

1. Confirm identity, as above. PASS: `platform_row` resolves
   `macos26-m1pro-16gb`, and `model_class_rows` reports `chunked_policy
   (Preview)`.

2. Run the lifecycle harness. It mutates Homebrew state, so it refuses to
   start unless that is acknowledged explicitly, and it needs all three
   inputs:

   ```bash
   TP_HOMEBREW_LIFECYCLE_ALLOW=1 \
     tools/validation/macos-homebrew-lifecycle.sh \
       --candidate-formula-dir <six rendered formulae, pinned to one build> \
       --baseline-formula <historical CLI-only tensorplate.rb> \
       --bundle-dir <MPS deploy-smoke fixture containing manifest.json> \
       --evidence-dir <evidence>
   ```

3. Convert its stage log. The harness writes `stages.tsv` itself:

   ```bash
   tools/validation/lifecycle-report-from-stages.sh \
     <evidence>/stages.tsv macos26-m1pro-16gb <tested_version> \
     macos-homebrew-lifecycle <evidence>/lifecycle-report.json \
     clean-install=install upgrade=upgrade deploy-smoke=deploy-smoke \
     status-logs=status-logs rollback=rollback launchd-restart=restart \
     launchd-crash-loop=crash-loop offline-runtime=offline
   ```

   The converter reads only what `stages.tsv` recorded. A harness that
   exits non-zero without recording a failed stage, because it failed
   while writing `summary.json` or the sanitized transcript or was
   interrupted between two stages, can leave a log that converts to
   `pass`. Never file a `pass` report from a run whose harness exited
   non-zero: re-run it.

   The digest this run files is the source archive every candidate
   formula is pinned to. That channel publishes no binary — all six
   formulae build from one archive — so the archive is the only
   immutable artifact a digest can name here, and it identifies the
   build input rather than the bytes that landed: two Macs build
   different binaries from it. Homebrew, not this harness, is what
   verifies the downloaded archive against that checksum. It is recorded
   after the candidate install, so a `--preflight-only` run files none:
   a preflight downloads nothing, and a digest filed for it would attest
   an install that never happened.

   The harness's `status-logs` stage runs after deploy-smoke. It asserts
   that `tensorplate status` still reports the deploy-smoke deployment as
   ready, that both launchd stderr logs gained output after launchd-start
   recorded their sizes, and that `tensorplate logs` reads the packaged
   structured event log and returns an observability event written
   during this run. Before starting services, the harness snapshots file
   identities and byte offsets for `events.ndjson` and its retained
   generation, `events.1`. It reads only new bytes from these generations,
   so rotation before the status check or between the CLI read and
   verification can still prove the returned event belongs to this run.
   The harness does not delete or truncate these logs. It does not query
   the agent component, because the agent writes no structured events.
   `host-facts` stays unmapped: it collects inventory before anything is
   installed and observes neither status nor logs. The status-logs stage,
   including its rotation checks, still requires a hardware run; the
   historical evidence predates it.

   `offline-runtime` backs `offline`. Both launchd services, startup
   recovery, a fresh deploy, inference, doctor and the MPS probe run
   under a `sandbox-exec` profile. The harness reads each bootstrapped
   job at most 30 times for its initial PID, waiting one second between
   pending reads; both services must still finish on that PID after
   exactly one launchd run. The
   profile allows only loopback on
   the two serving ports, plus unix sockets other than mDNSResponder, so
   names do not resolve. A probe inside the sandbox must be refused every
   other destination, and a bind on any other port. This is not an IP
   firewall. `fe80::1`, reached
   through another interface on those two ports, is an accepted gap, and
   so are unix-socket and XPC brokers; `macos-homebrew-lifecycle.md`
   lists them. A wildcard listener on the serving ports is also allowed
   by the profile but fails the stage's loopback-only socket check. The
   full offline-runtime stage still requires an M1 Pro hardware run;
   the historical record does not cover it. `offline-profile` stays
   unmapped. It checks the profile
   against `sandbox-exec` during preflight, before anything is
   installed. The converter keeps each canonical stage's worst status,
   so mapping it to `offline` would let a `--preflight-only` run report
   offline as passed.

4. File under `docs/validation/evidence/<version>/macos26-m1pro-16gb/`.
   Convert first, so the report reflects the harness's own `stages.tsv`.
   The converter's `detail` names harness stages and never quotes a log,
   so the report needs no sanitizing; the logs do. **Sanitize before the
   first commit** — see that directory's README — then scan the sanitized
   copy with the Mac's computer name, local host name, account name and
   any other name of this machine listed in a literal file kept outside
   the repository, and file only on exit 0:

   ```bash
   tools/validation/check-evidence-publication.sh \
     --literals <literal file outside the repository> \
     docs/validation/evidence/<version>/macos26-m1pro-16gb
   ```

## What a failed run is worth

A failing stage is evidence, not a wasted run: the report names the stage
and attaches its log, and that is what a fix is written against. Re-run
after fixing rather than editing the report — a report is a record of
what happened, and one that says otherwise is worse than none.
