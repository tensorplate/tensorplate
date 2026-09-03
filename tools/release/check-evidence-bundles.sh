#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Does every Production row have the evidence its support claim rests on?
#
# The guard this replaces asserted that a row DECLARED an evidence
# location whose string contained the row id. It passed for a directory
# nobody had created, which is how four Production rows reached a release
# branch carrying `provenance: spec_authored` and empty evidence
# directories. A check that cannot fail is not a check.
#
# Completeness is deliberately defined in terms of artifacts that already
# exist rather than a new manifest format:
#
#   1. `provenance: recorded` on the row      -- somebody observed it
#   2. a schema-valid lifecycle-report.json   -- the eight stages ran
#   3. that report's outcome is `pass`        -- and they passed
#
# Exit 0 when every Production row is complete, 1 when any is not. The
# report is per row and lists what is missing, because "evidence
# incomplete" without a subject is not actionable.
#
# Usage: check-evidence-bundles.sh [--registry DIR] [--root DIR]

set -Eeuo pipefail

registry_dir="config/platform"
root_dir="."

while [[ $# -gt 0 ]]; do
  case "$1" in
    --registry) registry_dir="${2:?}"; shift 2 ;;
    --root) root_dir="${2:?}"; shift 2 ;;
    -h|--help) sed -n '3,25p' "$0"; exit 0 ;;
    *) printf 'check-evidence-bundles: unknown argument `%s`\n' "$1" >&2; exit 2 ;;
  esac
done

python3 - "$registry_dir" "$root_dir" <<'PY'
import json, os, sys

registry_dir, root_dir = sys.argv[1], sys.argv[2]
rows_dir = os.path.join(registry_dir, "rows")
if not os.path.isdir(rows_dir):
    sys.exit(f"check-evidence-bundles: no registry rows at {rows_dir}")

CANONICAL = [
    "install", "upgrade", "deploy-smoke", "status-logs",
    "rollback", "restart", "crash-loop", "offline",
]

def problems_for(row):
    """Everything wrong with this row's evidence, or an empty list."""
    found = []
    if row.get("provenance") != "recorded":
        found.append(
            f"provenance is `{row.get('provenance')}`, not `recorded` "
            "-- nobody has observed this row"
        )
    evidence = (row.get("evidence") or {}).get("location")
    if not evidence:
        found.append("declares no evidence location")
        return found

    directory = os.path.join(root_dir, evidence)
    if not os.path.isdir(directory):
        found.append(f"evidence directory does not exist: {evidence}")
        return found

    report_path = os.path.join(directory, "lifecycle-report.json")
    if not os.path.isfile(report_path):
        found.append(f"no lifecycle-report.json under {evidence}")
        return found

    try:
        with open(report_path, encoding="utf-8") as handle:
            report = json.load(handle)
    except (OSError, json.JSONDecodeError) as err:
        found.append(f"lifecycle-report.json is unreadable: {err}")
        return found

    if report.get("row_id") != row["row_id"]:
        found.append(
            f"lifecycle report is for `{report.get('row_id')}`, not this row "
            "-- evidence filed under the wrong row proves nothing about it"
        )
    stages = {s.get("stage"): s.get("status") for s in report.get("stages", [])}
    missing = [name for name in CANONICAL if name not in stages]
    if missing:
        found.append(f"lifecycle report omits stages: {', '.join(missing)}")
    not_passed = sorted(
        f"{name}={stages[name]}" for name in CANONICAL
        if name in stages and stages[name] != "pass"
    )
    if not_passed:
        found.append(f"stages did not pass: {', '.join(not_passed)}")
    if report.get("outcome") != "pass":
        found.append(f"lifecycle outcome is `{report.get('outcome')}`")
    return found

incomplete = 0
checked = 0
for name in sorted(os.listdir(rows_dir)):
    if not name.endswith(".json"):
        continue
    with open(os.path.join(rows_dir, name), encoding="utf-8") as handle:
        row = json.load(handle)
    if row.get("support_level") != "Production":
        continue
    checked += 1
    found = problems_for(row)
    if found:
        incomplete += 1
        print(f"INCOMPLETE  {row['row_id']}")
        for problem in found:
            print(f"            - {problem}")
    else:
        print(f"complete    {row['row_id']}")

if checked == 0:
    sys.exit("check-evidence-bundles: no Production rows found; the check is not doing anything")

print(f"\n{checked - incomplete}/{checked} Production rows carry complete evidence")
if incomplete:
    print(
        "\nA Production row without evidence is a support claim nobody has "
        "verified. Record the evidence or lower the row's support level.",
        file=sys.stderr,
    )
    sys.exit(1)
PY
