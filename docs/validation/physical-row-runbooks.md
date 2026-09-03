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

The harnesses predate that report and record their own stage names, so
they emit it through the converter rather than by rewriting stage calls
on code that only runs on hardware:

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
canonical stage; the weakest of their results is the one reported.

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
| crash-loop | **not implemented** | covered |
| offline | **not implemented** | covered |

Closing these gaps means adding the missing operations to the harnesses
themselves, which is tracked as hardware work. Until then the converter
emits each gap as `skipped` with the reason, so the gate reports what is
missing rather than accepting a partial run.

## Jetson Orin Nano 8GB Super

Prerequisites: the device is in **Super power mode**, reachable over SSH,
and carries no build toolchain — `tools/validation/jetson-clean-room.sh`
validates a released artifact set on a clean rootfs, and a toolchain on
the device invalidates the glibc-floor comparison the run exists for.

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
   keys, and they differ legitimately between devices on the same row:

   ```bash
   tensorplate doctor --record <evidence>/
   nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null || true
   dpkg-query -W -f='${Package} ${Version}\n' 'nvidia-l4t-core' 'cuda-toolkit*' 'tensorrt*' 2>/dev/null
   uname -r
   ```

3. Run the clean-room harness. It takes the `run` subcommand and refuses
   to start without the confirmation token, because it purges TensorPlate
   packages and state:

   ```bash
   tools/validation/jetson-clean-room.sh run \
     --evidence-dir <evidence> \
     --confirm RESET-TENSORPLATE
   ```

4. Derive the stage log, then the report. The harness records each step
   as `<step>.exit` rather than writing a `stages.tsv`, so the adapter
   reads what it left behind:

   ```bash
   tools/validation/jetson-stages-from-evidence.sh <evidence> \
     > <evidence>/stages.tsv

   tools/validation/lifecycle-report-from-stages.sh \
     <evidence>/stages.tsv jetson-orin-nano-8gb-jp62 <tested_version> \
     jetson-clean-room <evidence>/lifecycle-report.json \
     install=install \
     deploy-trt-identity=deploy-smoke infer-trt-identity=deploy-smoke \
     status-after-infer=status-logs logs-agent=status-logs \
     journal-agent=status-logs \
     stop-services=restart start-services=restart services-ready=restart
   ```

   `upgrade`, `rollback`, `crash-loop` and `offline` are deliberately
   unmapped: the harness has no such steps, and naming one anyway would
   assert a stage that never ran.

5. File the report and the recorded fixtures under
   `docs/validation/evidence/<version>/jetson-orin-nano-8gb-jp62/`.
   **Sanitize first** — see that directory's README.

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
