#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The evidence completeness check.
#
# The guard this replaces passed for a directory nobody had created, so
# the property that matters is not "it runs" but "it fails when the
# evidence is not there, and passes when it is". Both directions are
# exercised against staged registries, because the real one has no
# complete row to prove the passing direction with.
#
# The staged reports are produced by the real runner rather than written
# here as literals. A hand-written fixture proved only that the checker
# accepted that fixture: the previous one omitted every required
# timestamp and cited logs that were never created, and still passed,
# because the checker parsed JSON where it claimed to validate a schema.

set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
checker="${repo_root}/tools/release/check-evidence-bundles.sh"
schema="${repo_root}/config/schemas/lifecycle_report.json"
runner="${repo_root}/tools/validation/lifecycle-stages.sh"
VERSION="0.2.1"
failures=0

check() {
  local what="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    printf '  ok   %s\n' "$what"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$what" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

stage_registry() {
  local dir="$1" provenance="$2"
  mkdir -p "${dir}/registry/rows"
  cat >"${dir}/registry/rows/synthetic-row.json" <<JSON
{
  "schema_version": "0.1",
  "row_id": "synthetic-row",
  "support_level": "Production",
  "provenance": "${provenance}",
  "evidence": { "location": "evidence/synthetic-row/" }
}
JSON
}

# Produce a bundle with the real runner: this is the producer half of the
# producer -> schema -> checker chain, so a change to either end that
# breaks the contract between them fails here rather than at a release.
produce_bundle() (
  local dir="$1" row="$2" version="$3"
  shift 3
  local skips=("$@")
  # shellcheck disable=SC1090
  source "$runner"
  lifecycle_begin "$row" "${dir}/evidence/synthetic-row" "$version" test-harness
  trap 'lifecycle_abort $?' EXIT
  local stage skip skipped
  for stage in install upgrade deploy-smoke status-logs rollback restart crash-loop offline; do
    skipped=0
    for skip in ${skips[@]+"${skips[@]}"}; do
      [[ "$stage" == "$skip" ]] && skipped=1
    done
    if (( skipped )); then
      lifecycle_skip "$stage" "not applicable to this synthetic row"
    else
      lifecycle_stage "$stage" true
    fi
  done
  lifecycle_finish
)

run_checker() {
  local dir="$1" version="${2:-$VERSION}"
  set +e
  "$checker" --registry "${dir}/registry" --root "$dir" \
    --schema "$schema" --version "$version" >"${dir}/out.txt" 2>&1
  local status=$?
  set -e
  printf '%s' "$status"
}

# Edit the report in place, to stage a defect the producer cannot emit.
mutate_report() {
  local dir="$1"
  python3 - "${dir}/evidence/synthetic-row/lifecycle-report.json" "$2" <<'PY'
import json, sys
path, how = sys.argv[1], sys.argv[2]
with open(path) as handle:
    report = json.load(handle)
if how == "duplicate-install":
    passing = next(s for s in report["stages"] if s["stage"] == "install")
    report["stages"].insert(0, {**passing, "status": "fail", "detail": "the real failure"})
elif how == "wrong-version":
    report["subject"]["tested_version"] = "9.9.9"
elif how == "drop-timestamps":
    for stage in report["stages"]:
        stage.pop("started_at", None)
        stage.pop("finished_at", None)
elif how == "escaping-log":
    report["stages"][0]["log"] = "../../../etc/passwd"
elif how == "absent-log":
    report["stages"][0]["log"] = "never-written.log"
elif how == "extra-stage":
    report["stages"].append({
        "stage": "install", "status": "pass", "log": "install.log",
        "started_at": "2026-01-01T00:00:00Z", "finished_at": "2026-01-01T00:00:01Z",
    })
else:
    sys.exit(f"unknown mutation {how}")
with open(path, "w") as handle:
    json.dump(report, handle, indent=2)
PY
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
printf 'evidence completeness check\n'

# --- The passing direction, end to end through the real producer.
d="${work}/complete"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
check "a complete bundle passes" "0" "$(run_checker "$d")"
check "  the producer's report is schema-valid" "yes" "$(
  python3 - "$schema" "${d}/evidence/synthetic-row/lifecycle-report.json" <<'PY'
import json, sys
import jsonschema
schema = json.load(open(sys.argv[1]))
report = json.load(open(sys.argv[2]))
errors = list(jsonschema.Draft7Validator(schema).iter_errors(report))
print("yes" if not errors else f"no: {errors[0].message}")
PY
)"

# --- Refusal directions.
d="${work}/unrecorded"; stage_registry "$d" spec_authored
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
check "spec_authored row fails" "1" "$(run_checker "$d")"
check "  and says why" "yes" "$(grep -q 'not .recorded.' "${d}/out.txt" && echo yes || echo no)"

d="${work}/nodir"; stage_registry "$d" recorded
check "missing evidence directory fails" "1" "$(run_checker "$d")"

d="${work}/noreport"; stage_registry "$d" recorded; mkdir -p "${d}/evidence/synthetic-row"
check "missing lifecycle report fails" "1" "$(run_checker "$d")"

d="${work}/skipped"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" offline >/dev/null 2>&1
check "a skipped stage fails completeness" "1" "$(run_checker "$d")"
check "  and names the stage" "yes" "$(grep -q 'offline=skipped' "${d}/out.txt" && echo yes || echo no)"
check "  and the producer called it incomplete, not pass" "incomplete" "$(
  python3 -c "import json;print(json.load(open('${d}/evidence/synthetic-row/lifecycle-report.json'))['outcome'])")"

d="${work}/wrongrow"; stage_registry "$d" recorded
produce_bundle "$d" some-other-row "$VERSION" >/dev/null 2>&1
check "a report for another row fails" "1" "$(run_checker "$d")"
check "  and says whose it is" "yes" "$(grep -q 'some-other-row' "${d}/out.txt" && echo yes || echo no)"

# --- Identity: evidence authorizes the version it exercised, and no other.
d="${work}/version"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
mutate_report "$d" wrong-version
check "evidence for another version fails" "1" "$(run_checker "$d")"
check "  and names both versions" "yes" "$(grep -q '9.9.9' "${d}/out.txt" && grep -q "$VERSION" "${d}/out.txt" && echo yes || echo no)"

d="${work}/oldrelease"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
check "the same bundle cannot bless a later release" "1" "$(run_checker "$d" 0.2.2)"

# --- A duplicate record must not let a later pass mask an earlier failure.
d="${work}/dupe"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
mutate_report "$d" duplicate-install
check "a duplicated stage fails" "1" "$(run_checker "$d")"
check "  and reports the repeat" "yes" "$(grep -q 'repeats stages' "${d}/out.txt" && echo yes || echo no)"

d="${work}/extra"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
mutate_report "$d" extra-stage
check "an appended duplicate pass fails" "1" "$(run_checker "$d")"

# --- Schema validation is real, not a JSON parse.
d="${work}/notimestamps"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
mutate_report "$d" drop-timestamps
check "a report missing required timestamps fails" "1" "$(run_checker "$d")"
check "  and says it violated the schema" "yes" "$(grep -q 'violates the schema' "${d}/out.txt" && echo yes || echo no)"

# --- Cited logs must exist, and must stay inside the bundle.
d="${work}/nolog"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
mutate_report "$d" absent-log
check "a cited log that does not exist fails" "1" "$(run_checker "$d")"

d="${work}/escape"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
mutate_report "$d" escaping-log
check "a log path escaping the bundle fails" "1" "$(run_checker "$d")"
check "  and calls it unsafe" "yes" "$(grep -q 'unsafe log path' "${d}/out.txt" && echo yes || echo no)"

# --- A check that did nothing must not read as success, and must be
# distinguishable from evidence that is merely incomplete.
d="${work}/empty"; mkdir -p "${d}/registry/rows"
check "an empty registry is an internal fault, not a pass" "2" "$(run_checker "$d")"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "all checks passed" || echo "${failures} check(s) failed")"
exit "$failures"
