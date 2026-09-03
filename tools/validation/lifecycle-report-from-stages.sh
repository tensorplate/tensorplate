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
#   lifecycle-report-from-stages.sh <stages.tsv> <row_id> <harness> <out.json> [map...]
# where each map is `harness_stage=canonical_stage`.

set -Eeuo pipefail

die() { printf 'lifecycle-report: %s\n' "$1" >&2; exit 2; }

[[ $# -ge 4 ]] || die "usage: $0 <stages.tsv> <row_id> <harness> <out.json> [harness_stage=canonical]..."

stages_tsv="$1"; row_id="$2"; harness="$3"; out="$4"; shift 4
[[ -f "$stages_tsv" ]] || die "no stage log at ${stages_tsv}"

python3 - "$stages_tsv" "$row_id" "$harness" "$out" "$@" <<'PY'
import json, sys

stages_tsv, row_id, harness, out = sys.argv[1:5]
mapping = {}
for pair in sys.argv[5:]:
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
seen, stages = {}, []
for row in rows:
    canonical = mapping.get(row.get("stage", ""))
    if canonical is None:
        continue
    # A canonical stage covered by several harness stages fails if ANY
    # of them failed: the weakest result is the honest one.
    prior = seen.get(canonical)
    status = row.get("status", "fail")
    if prior is None or (prior["status"] == "pass" and status != "pass"):
        entry = {
            "stage": canonical,
            "status": status if status in ("pass", "fail", "skipped") else "fail",
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
report = {
    "schema_version": "0.1",
    "row_id": row_id,
    "harness": harness,
    "started_at": started,
    "finished_at": finished,
    "outcome": "fail" if any(s["status"] == "fail" for s in stages) else "pass",
    "stages": stages,
}
with open(out, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2)
    handle.write("\n")
print(f"lifecycle-report: wrote {out}", file=sys.stderr)
PY
