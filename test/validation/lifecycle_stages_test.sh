#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The lifecycle stage runner, exercised as a harness would use it.
#
# The property under test is the one a harness cannot verify about
# itself: a stage that FAILS must appear in the report as a failure.
# Under `set -e` the runner never reaches its own bookkeeping, so the
# recording happens in the caller's trap -- and a mistake there produces
# a short report that reads exactly like a short run.

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
    lifecycle_begin ubuntu2404-x86-l4-g2s8 "$dir" test-harness
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
    lifecycle_begin jetson-orin-nano-8gb-jp62 "$dir" test-harness
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
check "passing run outcome" "pass" "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' "$report")"
check "skip is recorded with its reason" "no prior release on this row" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
print(next(s["detail"] for s in d["stages"] if s["stage"]=="upgrade"))' "$report")"
check "row id is carried" "ubuntu2404-x86-l4-g2s8" \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["row_id"])' "$report")"

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
  lifecycle_begin r "${work}/bad" t; lifecycle_stage not-a-stage true ) 2>/dev/null
check "an unknown stage name is rejected" "2" "$?"
set -e

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "all checks passed" || echo "${failures} check(s) failed")"
exit "$failures"
