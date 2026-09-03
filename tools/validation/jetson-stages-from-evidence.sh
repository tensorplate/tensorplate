#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Derive a stage log from what the Jetson clean-room harness already wrote.
#
# `jetson-clean-room.sh` records each step through `capture`, which leaves
# `<step>.stdout`, `<step>.stderr` and `<step>.exit` in the evidence
# directory. What it does not write is the `stages.tsv` the lifecycle
# converter reads, so the documented Jetson runbook had a converter step
# with no possible input.
#
# Instrumenting the harness itself would mean editing, untested, code that
# only runs on a device nobody can reach from CI. This reads its evidence
# instead: an adapter over files, which is testable here.
#
# Timestamps come from the evidence files' own modification times --
# `.stdout` for the start, `.exit` for the finish -- because the harness
# does not record its own. They are observations of when the files were
# written, not a claim the harness timed itself.
#
# Usage:
#   jetson-stages-from-evidence.sh <evidence_dir> [step...]
#
# With no steps named, every captured step is emitted. Naming steps
# restricts the log to those, in the order given.

set -Eeuo pipefail

die() { printf 'jetson-stages: %s\n' "$1" >&2; exit 2; }

[[ $# -ge 1 ]] || die "usage: $0 <evidence_dir> [step...]"
evidence_dir="$1"; shift
[[ -d "$evidence_dir" ]] || die "no evidence directory at ${evidence_dir}"

python3 - "$evidence_dir" "$@" <<'PY'
import os, sys, time

evidence_dir, wanted = sys.argv[1], sys.argv[2:]


def stamp(path):
    """File mtime as an RFC 3339 UTC instant, or empty if absent."""
    try:
        return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(os.path.getmtime(path)))
    except OSError:
        return ""


captured = sorted(
    name[: -len(".exit")]
    for name in os.listdir(evidence_dir)
    if name.endswith(".exit")
)
if not captured:
    print(
        f"jetson-stages: no captured steps under {evidence_dir}; the harness "
        "writes <step>.exit for each step it runs",
        file=sys.stderr,
    )
    raise SystemExit(2)

steps = wanted or captured
missing = [s for s in steps if s not in captured]
if missing:
    print(
        "jetson-stages: these steps were named but the harness did not run "
        f"them: {', '.join(missing)}",
        file=sys.stderr,
    )
    raise SystemExit(2)

print("stage\tstatus\tstarted_at\tfinished_at\tlog")
for step in steps:
    exit_path = os.path.join(evidence_dir, f"{step}.exit")
    try:
        with open(exit_path, encoding="utf-8") as handle:
            code = int(handle.read().strip())
    except (OSError, ValueError):
        # An unreadable exit code is a failure, not a stage to drop: the
        # step ran, and what it did is no longer knowable.
        code = 1
    status = "pass" if code == 0 else "fail"
    started = stamp(os.path.join(evidence_dir, f"{step}.stdout")) or stamp(exit_path)
    finished = stamp(exit_path)
    print(f"{step}\t{status}\t{started}\t{finished}\t{step}.stdout")
PY
