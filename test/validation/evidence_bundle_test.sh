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

# The producer refuses a malformed one, so an exported value would fail
# these checks for a reason unrelated to what they test.
unset TP_LIFECYCLE_SOURCE_REVISION

# A recorded digest, from the tool the harnesses use rather than a
# literal typed here.
if command -v sha256sum >/dev/null 2>&1; then
  DIGEST="$(sha256sum "$checker" | awk '{print $1}')"
else
  DIGEST="$(shasum -a 256 "$checker" | awk '{print $1}')"
fi

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
  # A real bundle names the artifact it installed, so the schema-valid
  # direction below covers a report that carries one.
  lifecycle_artifact_digest "$DIGEST" SHA256SUMS
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
  local dir="$1" version="${2:-$VERSION}" schema_path="${3:-$schema}"
  set +e
  "$checker" --registry "${dir}/registry" --root "$dir" \
    --schema "$schema_path" --version "$version" >"${dir}/out.txt" 2>&1
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
elif how == "bad-digest":
    report["subject"]["artifact_digest"] = "sha256:" + "a" * 64
elif how == "uppercase-digest":
    report["subject"]["artifact_digest"] = "A" * 64
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

# --- A malformed artifact digest is refused by the schema the gate
# already validates against, which is why the gate itself needs no
# check of its own. The passing direction above proves it is not a
# blanket refusal of any report carrying a digest.
d="${work}/baddigest"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
mutate_report "$d" bad-digest
check "a prefixed artifact digest fails" "1" "$(run_checker "$d")"
check "  and says it violated the schema" "yes" \
  "$(grep -q 'violates the schema at subject/artifact_digest' "${d}/out.txt" && echo yes || echo no)"

d="${work}/updigest"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
mutate_report "$d" uppercase-digest
check "an uppercase artifact digest fails" "1" "$(run_checker "$d")"

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

# --- The reboot stage: required on the L4 cloud row from 0.3.0, and
# carried by no other row or release. Reports are produced by the real
# runner, suspending before the reboot and resuming after it, or taken
# from the recorded 0.2.1 evidence the rule must leave exactly as it is.
L4=ubuntu2404-x86-l4-g2s8
JETSON=jetson-orin-nano-8gb-jp62
recorded="${repo_root}/docs/validation/evidence/v0.2.1"

# stage_row <dir> <row>: a registry holding that one Production row.
stage_row() {
  local dir="$1" row="$2"
  mkdir -p "${dir}/registry/rows"
  cat >"${dir}/registry/rows/${row}.json" <<JSON
{
  "schema_version": "0.1",
  "row_id": "${row}",
  "support_level": "Production",
  "provenance": "recorded",
  "evidence": { "location": "evidence/${row}/" }
}
JSON
}

# produce_row_bundle <dir> <row> <version> <reboot>: all eight stages
# passing, then the reboot stage as <reboot> says: none, pass,
# subcase-fails, same-boot or no-window.
produce_row_bundle() (
  local dir="$1" row="$2" version="$3" reboot="$4" stage
  # shellcheck disable=SC1090
  source "$runner"
  lifecycle_begin "$row" "${dir}/evidence/${row}" "$version" test-harness
  trap 'lifecycle_abort $?' EXIT
  lifecycle_artifact_digest "$DIGEST" SHA256SUMS
  for stage in install upgrade deploy-smoke status-logs rollback restart crash-loop offline; do
    lifecycle_stage "$stage" true
  done
  if [[ "$reboot" != none ]]; then
    lifecycle_suspend "${dir}/suspend.json"
    lifecycle_resume "${dir}/suspend.json"
    if [[ "$reboot" == same-boot ]]; then
      lifecycle_reboot_begin false
    else
      lifecycle_reboot_begin true
    fi
    lifecycle_reboot_subcase blocked true
    [[ "$reboot" == no-window ]] || lifecycle_reboot_retry_window 120
    if [[ "$reboot" == subcase-fails ]]; then
      lifecycle_reboot_subcase transient false || :
    else
      lifecycle_reboot_subcase transient true
    fi
    lifecycle_reboot_subcase denied_egress_resumes true
    lifecycle_reboot_finish
  fi
  lifecycle_finish
)

# recorded_bundle <dir> <row>: the row's recorded 0.2.1 evidence, copied.
recorded_bundle() {
  local dir="$1" row="$2"
  stage_row "$dir" "$row"
  mkdir -p "${dir}/evidence"
  cp -R "${recorded}/${row}" "${dir}/evidence/${row}"
}

# mutate_row <dir> <row> <how>: edit a staged report in place.
mutate_row() {
  python3 - "${1}/evidence/${2}" "$3" <<'PY'
import json, os, sys
directory, how = sys.argv[1], sys.argv[2]
path = os.path.join(directory, "lifecycle-report.json")
with open(path) as handle:
    report = json.load(handle)
if how.startswith("version="):
    report["subject"]["tested_version"] = how.split("=", 1)[1]
elif how == "add-reboot":
    # A complete, passing stage with its logs, so the only thing wrong is
    # that this row and release do not run one.
    subs = {}
    for name in ("blocked", "transient", "denied_egress_resumes"):
        log = "reboot-" + name + ".log"
        open(os.path.join(directory, log), "w").close()
        subs[name] = {"status": "pass", "log": log}
    open(os.path.join(directory, "reboot.log"), "w").close()
    report["reboot"] = {
        "status": "pass", "started_at": "2026-09-23T00:00:00Z",
        "finished_at": "2026-09-23T00:10:00Z", "log": "reboot.log",
        "boot_id_changed": True, "retry_window_seconds": 120, "sub_cases": subs,
    }
elif how == "drop-sub-case":
    del report["reboot"]["sub_cases"]["transient"]
elif how == "drop-reboot-log":
    os.remove(os.path.join(directory, report["reboot"]["log"]))
elif how.startswith("edit:"):
    # Python statements on `report`, `reboot` and `directory`, for the one
    # field a case needs wrong.
    exec(how[len("edit:"):], {"report": report, "reboot": report.get("reboot"),
                              "directory": directory, "os": os})
elif how == "non-canonical-stage":
    report["stages"].append({
        "stage": "reboot", "status": "pass", "log": "install.log",
        "started_at": "2026-01-01T00:00:00Z", "finished_at": "2026-01-01T00:00:01Z",
    })
else:
    sys.exit(f"unknown mutation {how}")
with open(path, "w") as handle:
    json.dump(report, handle, indent=2)
PY
}

has_out() {
  if grep -qF -- "$2" "${1}/out.txt"; then echo yes; else echo no; fi
}

d="${work}/l4-reboot"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.1 pass >/dev/null 2>&1
check "an L4 report at 0.3.1 with a passing reboot stage is complete" "0" "$(run_checker "$d" 0.3.1)"
check "  the producer's report is schema-valid" "yes" "$(
  python3 - "$schema" "${d}/evidence/${L4}/lifecycle-report.json" <<'PY'
import json, sys, jsonschema
schema = json.load(open(sys.argv[1]))
report = json.load(open(sys.argv[2]))
print("yes" if not list(jsonschema.Draft7Validator(schema).iter_errors(report)) else "no")
PY
)"

d="${work}/l4-no-reboot"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.1 none >/dev/null 2>&1
check "an L4 report at 0.3.1 without the reboot stage fails" "1" "$(run_checker "$d" 0.3.1)"
check "  and says the stage is required" "yes" "$(has_out "$d" "omits the reboot stage")"

d="${work}/l4-missing-sub-case"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.1 pass >/dev/null 2>&1
mutate_row "$d" "$L4" drop-sub-case
check "a reboot stage missing a sub-case fails" "1" "$(run_checker "$d" 0.3.1)"
check "  and says it violated the schema" "yes" "$(has_out "$d" "violates the schema")"

d="${work}/l4-sub-case-fails"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.1 subcase-fails >/dev/null 2>&1
check "a reboot stage whose sub-case failed fails" "1" "$(run_checker "$d" 0.3.1)"
check "  and names the sub-case" "yes" "$(has_out "$d" "transient=fail")"

d="${work}/l4-same-boot"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.1 same-boot >/dev/null 2>&1
check "a reboot stage whose boot ID did not change fails" "1" "$(run_checker "$d" 0.3.1)"
check "  and says no reboot was crossed" "yes" "$(has_out "$d" "no reboot was crossed")"

d="${work}/l4-no-window"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.1 no-window >/dev/null 2>&1
check "a reboot stage without the observed retry window fails" "1" "$(run_checker "$d" 0.3.1)"
check "  and says so" "yes" "$(has_out "$d" "retry window")"

d="${work}/l4-no-reboot-log"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.1 pass >/dev/null 2>&1
mutate_row "$d" "$L4" drop-reboot-log
check "a reboot stage citing a log that does not exist fails" "1" "$(run_checker "$d" 0.3.1)"
check "  and names the log" "yes" "$(has_out "$d" "cites a log that does not exist: reboot.log")"

# Each case below is a passing stage with one thing wrong, and asserts the
# message only that check writes.
reboot_case() {
  local what="$1" message="$2" edit="$3"
  case_count=$((${case_count:-0} + 1))
  d="${work}/l4-reboot-case-${case_count}"; stage_row "$d" "$L4"
  produce_row_bundle "$d" "$L4" 0.3.1 pass >/dev/null 2>&1
  mutate_row "$d" "$L4" "edit:${edit}"
  check "${what} fails" "1 yes" "$(run_checker "$d" 0.3.1) $(has_out "$d" "$message")"
}
reboot_case "a reboot stage recorded as failed" "reboot stage did not pass: recorded as failed" \
  'reboot["status"] = "fail"; reboot["detail"] = "recorded as failed"'
# The runner fails a stage with no window, so this is the gate's own rule
# on a report that says the stage passed.
reboot_case "a passing reboot stage with no retry window" "does not record the observed retry window" \
  'del reboot["retry_window_seconds"]'
# Python's parser accepts NaN and Infinity, which are not JSON, and NaN
# satisfies no numeric bound.
reboot_case "a retry window of NaN" "is unreadable: NaN is not JSON" \
  'reboot["retry_window_seconds"] = float("nan")'
reboot_case "a retry window of Infinity" "is unreadable: Infinity is not JSON" \
  'reboot["retry_window_seconds"] = float("inf")'
reboot_case "a skipped sub-case" "transient=skipped" \
  'reboot["sub_cases"]["transient"].update(status="skipped", detail="not applied")'
reboot_case "a sub-case log that does not exist" "cites a log that does not exist: reboot-transient.log" \
  'os.remove(os.path.join(directory, "reboot-transient.log"))'
reboot_case "a sub-case log outside the bundle" "cites an unsafe log path" \
  'reboot["sub_cases"]["transient"]["log"] = "../reboot-transient.log"'
reboot_case "a stage log outside the bundle" "cites an unsafe log path" \
  'reboot["log"] = "/etc/hostname"'
# What the schema refuses before the gate's own rules run.
reboot_case "a retry window of zero" "violates the schema" 'reboot["retry_window_seconds"] = 0'
reboot_case "a retry window given as text" "violates the schema" 'reboot["retry_window_seconds"] = "120"'
reboot_case "a reboot stage without a start time" "violates the schema" 'del reboot["started_at"]'
reboot_case "a reboot stage with an unknown key" "violates the schema" 'reboot["attempts"] = 2'
reboot_case "a reboot stage recorded as skipped" "violates the schema" \
  'reboot["status"] = "skipped"; reboot["detail"] = "not run"'
reboot_case "a failed reboot stage without a reason" "violates the schema" 'reboot["status"] = "fail"'
reboot_case "a skipped sub-case without a reason" "violates the schema" \
  'reboot["sub_cases"]["blocked"]["status"] = "skipped"'
reboot_case "a sub-case outside the three" "violates the schema" \
  'reboot["sub_cases"]["rollback"] = dict(reboot["sub_cases"]["blocked"])'
reboot_case "a boot ID verdict given as text" "violates the schema" 'reboot["boot_id_changed"] = "true"'

# The threshold is numeric: a string compare puts 0.10.0 before 0.3.0.
d="${work}/l4-0.10.0"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.10.0 none >/dev/null 2>&1
check "an L4 report at 0.10.0 without the reboot stage fails" "1" "$(run_checker "$d" 0.10.0)"
check "  and says the stage is required" "yes" "$(has_out "$d" "omits the reboot stage")"

# A pre-release counts as its release.
d="${work}/l4-0.3.0-rc"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.0-rc.1 none >/dev/null 2>&1
check "an L4 report at 0.3.0-rc.1 without the reboot stage fails" "1" "$(run_checker "$d" 0.3.0-rc.1)"
check "  and says the stage is required" "yes" "$(has_out "$d" "omits the reboot stage")"

d="${work}/l4-0.2.9"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.2.9 none >/dev/null 2>&1
check "an L4 report before 0.3.0 without the reboot stage is complete" "0" "$(run_checker "$d" 0.2.9)"

d="${work}/l4-0.2.9-reboot"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.2.9 pass >/dev/null 2>&1
check "an L4 report before 0.3.0 carrying a reboot stage fails" "1" "$(run_checker "$d" 0.2.9)"
check "  and says the row and version do not run one" "yes" "$(has_out "$d" "does not run")"

# The recorded evidence, which the rule must leave as it was.
d="${work}/l4-recorded"; recorded_bundle "$d" "$L4"
check "the recorded L4 report at 0.2.1 is still complete" "0" "$(run_checker "$d" 0.2.1)"

d="${work}/l4-recorded-reboot"; recorded_bundle "$d" "$L4"
mutate_row "$d" "$L4" add-reboot
check "  and with a reboot stage added it fails" "1" "$(run_checker "$d" 0.2.1)"

d="${work}/jetson-recorded"; recorded_bundle "$d" "$JETSON"
mutate_row "$d" "$JETSON" version=0.3.1
check "the recorded Jetson report at 0.3.1 needs no reboot stage" "0" "$(run_checker "$d" 0.3.1)"

d="${work}/jetson-recorded-reboot"; recorded_bundle "$d" "$JETSON"
mutate_row "$d" "$JETSON" version=0.3.1
mutate_row "$d" "$JETSON" add-reboot
check "a Jetson report carrying a reboot stage fails" "1" "$(run_checker "$d" 0.3.1)"
check "  and says the row does not run one" "yes" "$(has_out "$d" "does not run")"

# The reboot stage is not a ninth entry in the stage list: the list is the
# closed eight, and a stage outside it is refused by the schema.
d="${work}/l4-non-canonical"; stage_row "$d" "$L4"
produce_row_bundle "$d" "$L4" 0.3.1 pass >/dev/null 2>&1
mutate_row "$d" "$L4" non-canonical-stage
check "a stage outside the canonical eight fails" "1" "$(run_checker "$d" 0.3.1)"
check "  and says it violated the schema" "yes" "$(has_out "$d" "violates the schema")"

# --- A check that did nothing must not read as success, and must be
# distinguishable from evidence that is merely incomplete.
d="${work}/empty"; mkdir -p "${d}/registry/rows"
check "an empty registry is an internal fault, not a pass" "2" "$(run_checker "$d")"

# Malformed checker inputs are internal faults, not incomplete evidence.
# Python's default exception exit of 1 would make either fault waivable by
# the candidate release gate, so exercise the real parser failures.
d="${work}/malformed-registry"; stage_registry "$d" recorded
printf '{\n' >"${d}/registry/rows/synthetic-row.json"
check "malformed registry JSON is an internal fault" "2" "$(run_checker "$d")"

d="${work}/malformed-schema"; stage_registry "$d" recorded
produce_bundle "$d" synthetic-row "$VERSION" >/dev/null 2>&1
printf '{\n' >"${d}/malformed-schema.json"
check "malformed schema JSON is an internal fault" "2" \
  "$(run_checker "$d" "$VERSION" "${d}/malformed-schema.json")"

for option in --version --registry --schema --root; do
  status=0
  "$checker" "$option" >"${work}/invalid-args.txt" 2>&1 || status=$?
  check "${option} without a value is an invocation fault" "2" "$status"
  status=0
  "$checker" "$option" "" >"${work}/invalid-args.txt" 2>&1 || status=$?
  check "${option} with an empty value is an invocation fault" "2" "$status"
done

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "all checks passed" || echo "${failures} check(s) failed")"
exit "$failures"
