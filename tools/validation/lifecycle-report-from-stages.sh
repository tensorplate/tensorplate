#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Derive the canonical lifecycle report from a harness's own stage log.
#
# The macOS harness records nineteen stages under its own names and the
# Jetson harness records none; both predate the shared report and both
# run only on hardware. Rewriting their stage calls would mean editing,
# untested, the code whose whole purpose is to be trustworthy on a
# machine nobody can reach from CI. This converts instead: a mapping from
# harness stage names to the canonical eight, applied to the TSV the
# harness already writes.
#
# What the mapping asserts is real: a harness stage named here CLAIMS to
# be the canonical stage it maps to. A canonical stage with no mapped
# source is emitted as skipped with that stated, rather than omitted --
# an absent stage and an unrun one must not look alike.
#
# Usage:
#   lifecycle-report-from-stages.sh <stages.tsv> <row_id> <tested_version> \
#       <harness> <out.json> [map...]
# where each map is `harness_stage=canonical_stage`. A canonical stage may
# be named by several harness stages; the weakest of their results is the
# one reported.

set -Eeuo pipefail

die() { printf 'lifecycle-report: %s\n' "$1" >&2; exit 2; }

[[ $# -ge 5 ]] || die "usage: $0 <stages.tsv> <row_id> <tested_version> <harness> <out.json> [harness_stage=canonical]..."

stages_tsv="$1"; row_id="$2"; tested_version="$3"; harness="$4"; out="$5"; shift 5
[[ -f "$stages_tsv" ]] || die "no stage log at ${stages_tsv}"
[[ "$tested_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
  die "tested version \`${tested_version}\` is not a release version"

python3 - "$stages_tsv" "$row_id" "$tested_version" "$harness" "$out" "$@" <<'PY'
import json, os, sys

stages_tsv, row_id, tested_version, harness, out = sys.argv[1:6]
mapping = {}
for pair in sys.argv[6:]:
    if "=" not in pair:
        sys.exit(f"lifecycle-report: bad mapping `{pair}`, expected harness_stage=canonical")
    src, dst = pair.split("=", 1)
    mapping[src] = dst

CANONICAL = [
    "install", "upgrade", "deploy-smoke", "status-logs",
    "rollback", "restart", "crash-loop", "offline",
]
unknown = sorted(set(mapping.values()) - set(CANONICAL))
if unknown:
    sys.exit(f"lifecycle-report: not canonical stages: {unknown}")

rows = []
with open(stages_tsv, encoding="utf-8") as handle:
    header = handle.readline().rstrip("\n").split("\t")
    for line in handle:
        if not line.strip():
            continue
        rows.append(dict(zip(header, line.rstrip("\n").split("\t"))))

# A harness stage may map to a canonical one; the rest are its own
# business and are not evidence for this contract.
# Weakest-result ordering. fail dominates skipped dominates pass, so a
# canonical stage covered by several harness stages reports the worst of
# them regardless of the order they appear in the log. The previous rule
# replaced only a `pass`, which meant a `skipped` seen first silently
# absorbed a later `fail` -- the same two stages produced `pass` or `fail`
# depending on their order in the TSV.
SEVERITY = {"pass": 0, "skipped": 1, "fail": 2}

seen, stages = {}, []
for row in rows:
    canonical = mapping.get(row.get("stage", ""))
    if canonical is None:
        continue
    prior = seen.get(canonical)
    status = row.get("status", "fail")
    if status not in SEVERITY:
        status = "fail"
    if prior is None or SEVERITY[status] > SEVERITY[prior["status"]]:
        entry = {
            "stage": canonical,
            "status": status,
            "log": row.get("log", f"{canonical}.log"),
        }
        for key in ("started_at", "finished_at"):
            if row.get(key):
                entry[key] = row[key]
        seen[canonical] = entry
        if seen[canonical]["status"] != "pass":
            seen[canonical]["detail"] = f"from harness stage `{row.get('stage','')}`"

for canonical in CANONICAL:
    if canonical in seen:
        stages.append(seen[canonical])
    else:
        sources = sorted(s for s, d in mapping.items() if d == canonical)
        detail = (
            f"no harness stage mapped to `{canonical}`"
            if not sources
            else f"harness stages {sources} did not run"
        )
        # No timestamps: the stage did not run, and inventing one would
        # make an omission look like a very fast pass.
        stages.append({
            "stage": canonical, "status": "skipped",
            "log": f"{canonical}.log", "detail": detail,
        })

started = min((s["started_at"] for s in stages if s.get("started_at")), default="")
finished = max((s["finished_at"] for s in stages if s.get("finished_at")), default="")
# pass requires all eight present and passing; "nothing failed" is not the
# same claim, and reporting it as pass let two passes and six skips look
# like a validated row.
if any(s["status"] == "fail" for s in stages):
    outcome = "fail"
elif all(s["status"] == "pass" for s in stages) and len(stages) == len(CANONICAL):
    outcome = "pass"
else:
    outcome = "incomplete"

subject = {"tested_version": tested_version}
revision = os.environ.get("TP_LIFECYCLE_SOURCE_REVISION", "")
if revision:
    subject["source_revision"] = revision

report = {
    "schema_version": "0.1",
    "row_id": row_id,
    "subject": subject,
    "harness": harness,
    "started_at": started,
    "finished_at": finished,
    "outcome": outcome,
    "stages": stages,
}
with open(out, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2)
    handle.write("\n")
print(f"lifecycle-report: wrote {out}", file=sys.stderr)
PY
