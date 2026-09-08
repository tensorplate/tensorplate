#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The lifecycle stage runner, exercised as a harness would use it.
#
# The property under test is the one a harness cannot verify about
# itself: a stage that FAILS must appear in the report as a failure --
# in every calling context, not only the ones where errexit happens to
# be live. The runner classifies each stage from its command's captured
# status; the caller's EXIT trap remains a backstop for signals and for
# exits raised outside a stage.

set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
harness="${repo_root}/tools/validation/lifecycle-stages.sh"
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

# A run where every stage passes.
passing_run() {
  local dir="$1"
  ( set -Eeuo pipefail
    # shellcheck source=tools/validation/lifecycle-stages.sh
    source "$harness"
    lifecycle_begin ubuntu2404-x86-l4-g2s8 "$dir" 0.2.1 test-harness
    trap 'lifecycle_abort $?' EXIT
    lifecycle_stage install true
    lifecycle_skip upgrade "no prior release on this row"
    lifecycle_finish
  )
}

# A run whose second stage fails. The subshell exits non-zero; the report
# must still exist and must name the stage that failed.
#
# The subshell is NOT wrapped in `|| true`. Bash disables errexit inside
# any command that is part of a tested list, so `( ... ) || true` would
# run the whole harness with `set -e` suppressed -- the failing stage
# would return non-zero, execution would continue, and the runner would
# record it as a pass. The status is captured with `set +e` in the parent
# instead, which leaves the subshell's own errexit intact.
failing_run() {
  local dir="$1"
  set +e
  ( set -Eeuo pipefail
    # shellcheck source=tools/validation/lifecycle-stages.sh
    source "$harness"
    lifecycle_begin jetson-orin-nano-8gb-jp62 "$dir" 0.2.1 test-harness
    trap 'lifecycle_abort $?' EXIT
    lifecycle_stage install true
    lifecycle_stage deploy-smoke bash -c 'echo "engine load failed" >&2; exit 3'
    lifecycle_finish
  )
  failing_run_status=$?
  set -e
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

printf 'lifecycle stage runner\n'

passing_run "${work}/pass"
report="${work}/pass/lifecycle-report.json"
check "passing run writes a report" "yes" "$([[ -f "$report" ]] && echo yes || echo no)"
check "a run with a skip is incomplete, not pass" "incomplete" "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' "$report")"
check "skip is recorded with its reason" "no prior release on this row" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
print(next(s["detail"] for s in d["stages"] if s["stage"]=="upgrade"))' "$report")"
check "row id is carried" "ubuntu2404-x86-l4-g2s8" \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["row_id"])' "$report")"
check "the tested version is carried" "0.2.1" \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["subject"]["tested_version"])' "$report")"

# Only all eight passing is a pass. Without this the gate would accept a
# run of two passes and six skips as a validated row.
( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_begin ubuntu2404-x86-l4-g2s8 "${work}/all8" 0.2.1 test-harness
  trap 'lifecycle_abort $?' EXIT
  for stage in install upgrade deploy-smoke status-logs rollback restart crash-loop offline; do
    lifecycle_stage "$stage" true
  done
  lifecycle_finish
) >/dev/null 2>&1
check "all eight passing is a pass" "pass" \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' "${work}/all8/lifecycle-report.json")"

# Regression: bash suspends errexit inside a function invoked from a
# tested context, so a runner that relied on ambient `set -e` recorded
# `outcome=pass` and `install=pass` for a command that failed. The
# classification must not depend on the caller's control flow.
( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_begin ubuntu2404-x86-l4-g2s8 "${work}/tested" 0.2.1 test-harness
  trap 'lifecycle_abort $?' EXIT
  lifecycle_stage install false || true
  lifecycle_finish
) >/dev/null 2>&1
check "a failure in a tested context is still a failure" "fail" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
print(next(s["status"] for s in d["stages"] if s["stage"]=="install"))' "${work}/tested/lifecycle-report.json")"
check "  and the run does not certify itself" "fail" \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' "${work}/tested/lifecycle-report.json")"

# lifecycle_finish must not clear the caller's EXIT trap: this file is
# sourced, and the harnesses chain real cleanup (restoring packages,
# services and Homebrew formulae) off the same trap.
( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  trap 'printf caller-cleanup-ran >"${work}/cleanup-marker"; lifecycle_abort $?' EXIT
  lifecycle_begin ubuntu2404-x86-l4-g2s8 "${work}/trap" 0.2.1 test-harness
  lifecycle_stage install true
  lifecycle_finish
) >/dev/null 2>&1
check "the caller's EXIT cleanup survives lifecycle_finish" "caller-cleanup-ran" \
  "$(cat "${work}/cleanup-marker" 2>/dev/null || echo MISSING)"
check "  and the report is not written twice" "1" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
print(sum(1 for s in d["stages"] if s["stage"]=="install"))' "${work}/trap/lifecycle-report.json")"

failing_run "${work}/fail"
check "a failing run exits non-zero" "3" "$failing_run_status"
report="${work}/fail/lifecycle-report.json"
check "failing run still writes a report" "yes" "$([[ -f "$report" ]] && echo yes || echo no)"
check "failing run outcome" "fail" "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' "$report")"
check "the failed stage is named" "deploy-smoke" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
print(next(s["stage"] for s in d["stages"] if s["status"]=="fail"))' "$report")"
check "the passing stage before it is kept" "install" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
print(next(s["stage"] for s in d["stages"] if s["status"]=="pass"))' "$report")"
check "the failure carries detail" "yes" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
s=next(s for s in d["stages"] if s["status"]=="fail")
print("yes" if s.get("detail") else "no")' "$report")"
check "the stage log is attached" "yes" \
  "$([[ -s "${work}/fail/deploy-smoke.log" ]] && echo yes || echo no)"

# An unknown stage name is a harness bug, not a run failure.
set +e
( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_begin r "${work}/bad" 0.2.1 t; lifecycle_stage not-a-stage true ) 2>/dev/null
check "an unknown stage name is rejected" "2" "$?"
set -e

# --- The Jetson evidence adapter.
#
# The clean-room harness writes <step>.exit per step and no stages.tsv, so
# the documented converter step had no possible input. The adapter reads
# what the harness actually leaves behind; these check that it reports the
# step's real status rather than its own presence.
adapter="${repo_root}/tools/validation/jetson-stages-from-evidence.sh"
converter="${repo_root}/tools/validation/lifecycle-report-from-stages.sh"
ev="${work}/jetson"; mkdir -p "$ev"
for step in install deploy-trt-identity status-after-infer stop-services; do
  printf 'output\n' >"${ev}/${step}.stdout"; printf '0\n' >"${ev}/${step}.exit"
done
printf 'engine load failed\n' >"${ev}/infer-trt-identity.stdout"
printf '3\n' >"${ev}/infer-trt-identity.exit"
"$adapter" "$ev" >"${ev}/stages.tsv" 2>/dev/null

check "the adapter emits a row per captured step" "5" \
  "$(($(wc -l <"${ev}/stages.tsv") - 1))"
check "a step that exited non-zero is a fail" "fail" \
  "$(awk -F'\t' '$1=="infer-trt-identity"{print $2}' "${ev}/stages.tsv")"
check "a step that exited zero is a pass" "pass" \
  "$(awk -F'\t' '$1=="install"{print $2}' "${ev}/stages.tsv")"

set +e
"$adapter" "${work}/not-a-dir" >/dev/null 2>&1
check "a missing evidence directory is an internal fault" "2" "$?"
mkdir -p "${work}/empty-ev"
"$adapter" "${work}/empty-ev" >/dev/null 2>&1
check "an evidence directory with no captured steps is a fault" "2" "$?"
"$adapter" "$ev" install never-ran >/dev/null 2>&1
check "naming a step the harness never ran is a fault" "2" "$?"
set -e

# A failing step must survive the whole adapter -> converter chain. This
# is the path the Jetson runbook documents, so a failure laundered here
# would reach the gate as a pass.
"$converter" "${ev}/stages.tsv" jetson-orin-nano-8gb-jp62 0.2.1 jetson-clean-room \
  "${ev}/lifecycle-report.json" \
  install=install deploy-trt-identity=deploy-smoke \
  infer-trt-identity=deploy-smoke status-after-infer=status-logs \
  stop-services=restart >/dev/null 2>&1
check "a failed step survives conversion" "fail" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
print(next(s["status"] for s in d["stages"] if s["stage"]=="deploy-smoke"))' "${ev}/lifecycle-report.json")"
check "  and the run does not read as complete" "fail" \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' "${ev}/lifecycle-report.json")"
check "  and stages the harness cannot run are named as gaps" "yes" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
gaps=[s for s in d["stages"] if s["stage"] in ("upgrade","rollback","crash-loop","offline")]
print("yes" if all(g["status"]=="skipped" and g.get("detail") for g in gaps) else "no")' "${ev}/lifecycle-report.json")"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "all checks passed" || echo "${failures} check(s) failed")"
exit "$failures"
