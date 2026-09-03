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

The harnesses predate that report and record their own stage names, so
they emit it through the converter rather than by rewriting stage calls
on code that only runs on hardware:

```bash
tools/validation/lifecycle-report-from-stages.sh \
  <evidence>/stages.tsv <row_id> <harness> <evidence>/lifecycle-report.json \
  <harness_stage>=<canonical_stage> ...
```

The mapping is an assertion: naming `clean-install=install` claims that
the harness's clean-install stage *is* the canonical install stage. Get
it wrong and the report lies in the gate's favour, which is the one
direction that matters.

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

3. Run the clean-room harness, then convert its stage log:

   ```bash
   tools/validation/jetson-clean-room.sh --evidence-dir <evidence>
   tools/validation/lifecycle-report-from-stages.sh \
     <evidence>/stages.tsv jetson-orin-nano-8gb-jp62 jetson-clean-room \
     <evidence>/lifecycle-report.json <mappings...>
   ```

4. File the report and the recorded fixtures under
   `dist/release/<tag>/jetson-orin-nano-8gb-jp62/`.

## MacBook Pro M1 Pro

Prerequisites: Homebrew present, the tap reachable, and **no TensorPlate
services running** from a previous run — `macos-homebrew-lifecycle.sh`
restores a baseline on exit and a half-cleaned host makes its rollback
stage meaningless.

1. Confirm identity, as above. PASS: `platform_row` resolves
   `macos26-m1pro-16gb`, and `model_class_rows` reports `chunked_policy
   (Preview)`.

2. Run the lifecycle harness and convert:

   ```bash
   tools/validation/macos-homebrew-lifecycle.sh --evidence-dir <evidence>
   tools/validation/lifecycle-report-from-stages.sh \
     <evidence>/stages.tsv macos26-m1pro-16gb macos-homebrew-lifecycle \
     <evidence>/lifecycle-report.json \
     clean-install=install upgrade=upgrade deploy-smoke=deploy-smoke \
     host-facts=status-logs rollback=rollback launchd-restart=restart \
     launchd-crash-loop=crash-loop offline-runtime=offline
   ```

3. File under `dist/release/<tag>/macos26-m1pro-16gb/`.

## What a failed run is worth

A failing stage is evidence, not a wasted run: the report names the stage
and attaches its log, and that is what a fix is written against. Re-run
after fixing rather than editing the report — a report is a record of
what happened, and one that says otherwise is worse than none.
