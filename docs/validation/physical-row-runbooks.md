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

### Neither harness covers all eight stages yet

Both runbooks below produce an `incomplete` report today, and the
release gate refuses both rows. That is the accurate state, not a
defect in the runbook:

| Canonical stage | Jetson | macOS |
| --- | --- | --- |
| install | covered | covered |
| upgrade | **not implemented** | covered |
| deploy-smoke | covered | covered |
| status-logs | covered | **not implemented** |
| rollback | **not implemented** | covered |
| restart | covered | covered |
| crash-loop | covered | covered |
| offline | **not implemented** | covered |

Closing these gaps means adding the missing operations to the harnesses
themselves, which is tracked as hardware work. Until then each gap is
recorded as `skipped` with its reason — by the Jetson harness itself, and
by the converter for macOS — so the gate reports what is missing rather
than accepting a partial run.

## Jetson Orin Nano 8GB Super

Prerequisites:

- The device is in **Super power mode**, reachable over SSH, and has an
  operator account with sudo. The harness refuses to run as root and
  calls `sudo` for privileged steps.
- The operator is in the `tensorplate` group, in a fresh session. The
  agent's control socket is group-only, so every CLI call the harness
  makes as the operator needs it; the group survives a purge. Without it
  the install stage fails on doctor's `agent_reachable` finding:

  ```bash
  sudo usermod -aG tensorplate "$USER"
  ```

- The device can build the TensorRT identity bundle: a C++ compiler, the
  CUDA headers and `libnvinfer`. `tools/validation/create_trt_identity_bundle.sh`
  compiles a small TensorRT builder and runs it on the device, because an
  engine is built for the TensorRT version that will load it. The device
  is not a clean rootfs in any case — the fleet's single Jetson is also
  where the arm64 release packages are built — so say in the pull request
  that the run was made on a device carrying a build toolchain. Where
  the device under test has no toolchain, build the bundle on a Jetson
  with the same JetPack and TensorRT and pass it with `--bundle-dir`; the
  harness builds nothing when one is given.
- Network access for the installer, which runs `apt-get update` and
  verifies the release signature with cosign.
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

4. Check eligibility first. This builds the bundle in a temporary
   directory it removes, installs nothing and writes no evidence:

   ```bash
   tools/validation/jetson-lifecycle.sh \
     --candidate-tag <asset_tag> \
     --candidate-assets-dir /var/tmp/tensorplate-<asset_tag>/assets \
     --tested-version <tested_version> \
     --evidence-dir <evidence> \
     --preflight-only --confirm RESET-TENSORPLATE
   ```

   It refuses a host that is not aarch64, not Ubuntu 22.04 or not L4T
   R36; a `--tested-version` that is not a bare `X.Y.Z`; a tag whose
   `X.Y.Z` is not the tested version; an assets directory without exactly
   one artifact manifest, whose manifest's `release.tag` is not the tag
   given, or whose files fail `SHA256SUMS`; and a device that cannot build
   the bundle. The manifest binding matters because the installer accepts
   a signature from any release tag, so a signed set is not thereby the
   set for the tag you named.

5. Run the lifecycle harness. It refuses to start without the
   confirmation token, because it purges TensorPlate packages and state,
   and it leaves the candidate installed:

   ```bash
   tools/validation/jetson-lifecycle.sh \
     --candidate-tag <asset_tag> \
     --candidate-assets-dir /var/tmp/tensorplate-<asset_tag>/assets \
     --tested-version <tested_version> \
     --evidence-dir <evidence> \
     --confirm RESET-TENSORPLATE
   ```

   It writes `<evidence>/lifecycle-report.json` itself; there is no stage
   log to convert, and neither `tools/validation/jetson-stages-from-evidence.sh`
   nor the converter is part of this procedure.

   **install** purges every installed `tensorplate*` package except
   `tensorplate-apt-source` — the channel bootstrap, which depends on
   nothing in TensorPlate — and requires that none remain before it clears
   `/etc`, `/var/lib`, `/var/log` and `/run/tensorplate`. It installs
   through the candidate's `install.sh` with signature verification and
   without the Python backend, requires each of the five runtime packages
   to be installed at the version its candidate `.deb` declares, and
   requires doctor to report nothing failing, `platform_row` to resolve
   this row, and the agent, socket, serving binary, path layout and
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

   **upgrade**, **rollback** and **offline** are skipped with their
   reasons in the report, so a run today is `incomplete` and the gate
   refuses the row. Upgrade and rollback against the v0.1.5 arm64
   baseline, and offline under per-unit network denial, are follow-up
   harness work.

   The digest this run files is the sha256 of the candidate's
   `SHA256SUMS`, taken in preflight once the asset set has verified and
   written into the report only after the install stage passes. That file
   is what the install itself trusts: it is signature-verified, and the
   packages are checked against its lines. It identifies the release build
   under test rather than the local package selection, and covers the
   whole published asset set, including files this row never installs.

6. File the report, its stage logs and the recorded row facts under
   `docs/validation/evidence/<version>/jetson-orin-nano-8gb-jp62/`.
   **Sanitize first** — see that directory's README. What the harness
   writes:

   | File | Carries |
   | --- | --- |
   | `agent-journal.txt`, `observability-journal.txt`, `crash-loop-journal.txt` | raw journal records in JSON, including host metadata fields such as the host name and the machine and boot ids; inspect every field before publishing |
   | `install.log` | the assets path the installer echoes, which names an account if the assets were under a home directory |
   | `host-facts.txt` | kernel release, OS name, the first line of `/etc/nv_tegra_release`, the systemd version and the power mode; no host name or serial is read, but check the release line |
   | `doctor.json` | host OS and accelerator facts |
   | `packages.txt`, `checksums.txt`, `status*.json`, `deploy-result.json`, `restart-result.json`, `crash-loop-*.json`, `agent-cli.log` | package versions, file names, the deployment id and loopback serving URLs; scan them as well |
   | `lifecycle-report.json` | a **failing** stage's `detail` is the tail of its log and may copy identifiers from the commands or records it quotes |

## MacBook Pro M1 Pro

Prerequisites: Homebrew present, the tap reachable, and **no TensorPlate
services running** from a previous run — `macos-homebrew-lifecycle.sh`
restores a baseline on exit and a half-cleaned host makes its rollback
stage meaningless.

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
     rollback=rollback launchd-restart=restart \
     launchd-crash-loop=crash-loop offline-runtime=offline
   ```

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

   `status-logs` is deliberately unmapped. The harness's `host-facts`
   stage collects inventory before anything is installed; it is not an
   observation of status or log behaviour, and mapping it would claim a
   stage that never ran.

4. File under `docs/validation/evidence/<version>/macos26-m1pro-16gb/`.
   **Sanitize first** — see that directory's README.

## What a failed run is worth

A failing stage is evidence, not a wasted run: the report names the stage
and attaches its log, and that is what a fix is written against. Re-run
after fixing rather than editing the report — a report is a record of
what happened, and one that says otherwise is worse than none.
