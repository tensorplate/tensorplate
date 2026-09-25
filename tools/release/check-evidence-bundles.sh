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
#   4. its subject names the version released -- for THIS release
#   5. every stage log it cites exists        -- and can be read
#   6. the reboot stage, where the row and      -- and the boot boundary
#      version require it, and nowhere else        was crossed
#
# (4) and (5) are what stop a report being reusable. Without the subject
# check a run recorded once blesses every later tag; without the log check
# a report can cite evidence that was never written.
#
# Validation is against config/schemas/lifecycle_report.json itself rather
# than a hand-written restatement of it, so the checker cannot drift from
# the contract it enforces.
#
# Exit 0 when every Production row is complete, 1 when any is not, and 2
# on an internal fault (bad arguments, unreadable registry, missing
# dependency) -- a release gate must be able to tell "the evidence is
# incomplete" from "the checker did not run".
#
# Usage: check-evidence-bundles.sh --version X.Y.Z [--registry DIR] [--root DIR]

set -Eeuo pipefail

registry_dir="config/platform"
root_dir="."
schema="config/schemas/lifecycle_report.json"
expect_version=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --registry|--root|--schema|--version)
      if [[ $# -lt 2 || -z "$2" ]]; then
        printf 'check-evidence-bundles: %s requires a nonempty value\n' "$1" >&2
        exit 2
      fi
      ;;
  esac
  case "$1" in
    --registry) registry_dir="$2"; shift 2 ;;
    --root) root_dir="$2"; shift 2 ;;
    --schema) schema="$2"; shift 2 ;;
    --version) expect_version="$2"; shift 2 ;;
    -h|--help) sed -n '3,37p' "$0"; exit 0 ;;
    *) printf 'check-evidence-bundles: unknown argument `%s`\n' "$1" >&2; exit 2 ;;
  esac
done

# Required, and validated here: a gate that silently skipped the identity
# check when the caller forgot the flag would be the weakest link nobody
# notices.
[[ -n "$expect_version" ]] || {
  printf 'check-evidence-bundles: --version X.Y.Z is required; without it the\n' >&2
  printf '  checker cannot tell evidence for this release from evidence for another\n' >&2
  exit 2
}
[[ "$expect_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || {
  printf 'check-evidence-bundles: `%s` is not a release version\n' "$expect_version" >&2
  exit 2
}
[[ -f "$schema" ]] || {
  printf 'check-evidence-bundles: no lifecycle report schema at %s\n' "$schema" >&2
  exit 2
}

# Python uses exit 1 for uncaught exceptions and startup/syntax errors.
# Reserve exit 3 exclusively for its normal incomplete-evidence verdict,
# then map it to the public exit 1 below. Every other Python failure is an
# internal fault, so a release candidate cannot waive a broken checker.
checker_status=0
python3 - "$registry_dir" "$root_dir" "$schema" "$expect_version" <<'PY' || checker_status=$?
import json, os, sys

try:
    import jsonschema
except ImportError:
    print(
        "check-evidence-bundles: python3 jsonschema is required to validate "
        "lifecycle reports.\n  Install it with: "
        "python3 -m pip install -r tools/release/requirements.txt",
        file=sys.stderr,
    )
    raise SystemExit(2)

registry_dir, root_dir, schema_path, expect_version = sys.argv[1:5]
rows_dir = os.path.join(registry_dir, "rows")
if not os.path.isdir(rows_dir):
    print(f"check-evidence-bundles: no registry rows at {rows_dir}", file=sys.stderr)
    raise SystemExit(2)

with open(schema_path, encoding="utf-8") as handle:
    SCHEMA = json.load(handle)
VALIDATOR = jsonschema.Draft7Validator(SCHEMA)

CANONICAL = [
    "install", "upgrade", "deploy-smoke", "status-logs",
    "rollback", "restart", "crash-loop", "offline",
]

# The reboot stage sits beside the canonical eight and is required only
# where a row's release boundary depends on it: the L4 cloud row, from the
# 0.3 line on, whose denied-egress operation holds within one boot and
# must recover through the metadata service after a reboot. Every other
# row, and every earlier release, must not carry one, so every report
# recorded before this rule (all of them before 0.3.0) is still accepted.
REBOOT_REQUIRED_FROM = {"ubuntu2404-x86-l4-g2s8": (0, 3, 0)}
REBOOT_SUB_CASES = ["blocked", "transient", "denied_egress_resumes"]


def refuse_constant(name):
    """NaN and Infinity are not JSON, but Python's parser accepts them, and
    NaN satisfies no numeric bound: a retry window of NaN is neither above
    nor below zero, so the schema would pass it."""
    raise ValueError(f"{name} is not JSON")


def release_triple(version):
    """(major, minor, patch) of a release version, compared as numbers.

    A string compare puts 0.10.0 before 0.3.0. A pre-release counts as its
    release: requiring the stage from 0.3.0 includes every 0.3.0 candidate,
    which is the fail-closed reading.
    """
    core = version.split("-", 1)[0]
    return tuple(int(part) for part in core.split("."))


def reboot_problems(row_id, tested, report):
    """What is wrong with the report's reboot stage, or an empty list."""
    reboot = report.get("reboot")
    threshold = REBOOT_REQUIRED_FROM.get(row_id)
    required = threshold is not None and release_triple(tested) >= threshold
    if not required:
        if reboot is not None:
            return [
                f"lifecycle report carries a reboot stage that `{row_id}` at "
                f"`{tested}` does not run -- a stage no release requires here "
                "is not evidence for anything"
            ]
        return []
    if reboot is None:
        return [
            "lifecycle report omits the reboot stage, which this row requires "
            f"from {'.'.join(str(part) for part in threshold)}"
        ]
    found = []
    if reboot.get("status") != "pass":
        found.append(f"reboot stage did not pass: {reboot.get('detail', reboot.get('status'))}")
    if reboot.get("boot_id_changed") is not True:
        found.append("reboot stage does not show the boot ID changed -- no reboot was crossed")
    if "retry_window_seconds" not in reboot:
        found.append("reboot stage does not record the observed retry window")
    sub_cases = reboot.get("sub_cases") or {}
    not_passed = [
        f"{name}={(sub_cases.get(name) or {}).get('status')}"
        for name in REBOOT_SUB_CASES
        if (sub_cases.get(name) or {}).get("status") != "pass"
    ]
    if not_passed:
        found.append(f"reboot sub-cases did not pass: {', '.join(not_passed)}")
    return found


def safe_relative(target):
    """A log path that stays inside the bundle.

    Reports arrive from machines the release process does not control, so
    an absolute path or one climbing out of the bundle is rejected rather
    than resolved.
    """
    if not target or os.path.isabs(target):
        return False
    parts = os.path.normpath(target).split(os.sep)
    return ".." not in parts

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
            report = json.load(handle, parse_constant=refuse_constant)
    except (OSError, ValueError) as err:
        found.append(f"lifecycle-report.json is unreadable: {err}")
        return found

    # Validate against the schema before reading anything out of the
    # report: parsing proves only that it is JSON, and the fixture that
    # motivated this omitted every required timestamp while still exiting 0.
    schema_errors = sorted(VALIDATOR.iter_errors(report), key=lambda e: list(e.path))
    if schema_errors:
        for err in schema_errors[:5]:
            location = "/".join(str(part) for part in err.path) or "(root)"
            found.append(f"lifecycle report violates the schema at {location}: {err.message}")
        if len(schema_errors) > 5:
            found.append(f"...and {len(schema_errors) - 5} further schema violations")
        return found

    if report.get("row_id") != row["row_id"]:
        found.append(
            f"lifecycle report is for `{report.get('row_id')}`, not this row "
            "-- evidence filed under the wrong row proves nothing about it"
        )

    # The report must name the release it authorizes. Without this a run
    # recorded once is valid evidence for every tag that follows it.
    tested = (report.get("subject") or {}).get("tested_version")
    if tested != expect_version:
        found.append(
            f"evidence tested version `{tested}`, but this release is "
            f"`{expect_version}` -- a report authorizes only the version it exercised"
        )

    # Exactly one record per canonical stage. A dict keyed by stage name
    # silently kept the last duplicate, so a second `install=pass` appended
    # to a report hid the first one's failure.
    counts = {}
    for entry in report.get("stages", []):
        counts.setdefault(entry.get("stage"), []).append(entry)
    missing = [name for name in CANONICAL if name not in counts]
    if missing:
        found.append(f"lifecycle report omits stages: {', '.join(missing)}")
    duplicated = sorted(name for name, entries in counts.items() if len(entries) > 1)
    if duplicated:
        found.append(
            f"lifecycle report repeats stages: {', '.join(duplicated)} "
            "-- one record per stage, or a later pass can mask an earlier failure"
        )
    extra = sorted(name for name in counts if name not in CANONICAL)
    if extra:
        found.append(f"lifecycle report carries non-canonical stages: {', '.join(extra)}")

    not_passed = sorted(
        f"{name}={entries[0].get('status')}"
        for name, entries in counts.items()
        if name in CANONICAL and entries[0].get("status") != "pass"
    )
    if not_passed:
        found.append(f"stages did not pass: {', '.join(not_passed)}")

    found.extend(reboot_problems(row["row_id"], tested, report))

    # Every cited log must be readable. A report may otherwise reference
    # evidence that was never written, which is the failure this whole
    # gate exists to make impossible.
    cited = [(name, entry.get("log")) for name, entries in sorted(counts.items())
             for entry in entries]
    reboot = report.get("reboot")
    if reboot is not None:
        cited.append(("reboot", reboot.get("log")))
        for name in REBOOT_SUB_CASES:
            cited.append((f"reboot/{name}", ((reboot.get("sub_cases") or {}).get(name) or {}).get("log")))
    for name, target in cited:
        if not safe_relative(target):
            found.append(f"stage `{name}` cites an unsafe log path: {target!r}")
            continue
        if not os.path.isfile(os.path.join(directory, target)):
            found.append(f"stage `{name}` cites a log that does not exist: {target}")

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
    print(
        "check-evidence-bundles: no Production rows found; the check is not "
        "doing anything",
        file=sys.stderr,
    )
    raise SystemExit(2)

print(f"\n{checked - incomplete}/{checked} Production rows carry complete evidence")
if incomplete:
    print(
        "\nA Production row without evidence is a support claim nobody has "
        "verified. Record the evidence or lower the row's support level.",
        file=sys.stderr,
    )
    sys.exit(3)
PY

case "$checker_status" in
    0) exit 0 ;;
    3) exit 1 ;;
    *)
        printf 'check-evidence-bundles: checker failed before reaching a verdict (Python exit %s)\n' "$checker_status" >&2
        exit 2
        ;;
esac
