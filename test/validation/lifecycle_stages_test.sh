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

# Both producers read this, and both now refuse a malformed value. A
# developer with one exported would otherwise fail these checks for a
# reason that has nothing to do with what they test.
unset TP_LIFECYCLE_SOURCE_REVISION

# Real tool output, so the digest fixtures are recorded rather than
# transcribed. sha256sum on the CI runners, shasum on a developer's Mac.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1"
  else
    shasum -a 256 "$1"
  fi
}

subject_field() {
  python3 -c 'import json,sys
print(json.load(open(sys.argv[1]))["subject"].get(sys.argv[2], "absent"))' "$1" "$2"
}

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

# --- The artifact digest.
#
# The one fact about a run that cannot be recovered once the hardware is
# gone: nothing on an installed system reports which artifact it came
# from. So the value is refused rather than repaired, and a run that
# records none says so by omission rather than by a placeholder.
digest_line="$(sha256_of "$harness")"
digest_hex="${digest_line%% *}"

( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_begin ubuntu2404-x86-l4-g2s8 "${work}/digest" 0.2.1 test-harness
  trap 'lifecycle_abort $?' EXIT
  lifecycle_stage install true
  lifecycle_artifact_digest "$digest_hex" SHA256SUMS
  lifecycle_finish
) >/dev/null 2>&1
check "the artifact digest is carried" "$digest_hex" \
  "$(subject_field "${work}/digest/lifecycle-report.json" artifact_digest)"
check "  and the evidence records what was hashed" "${digest_hex}  SHA256SUMS" \
  "$(cat "${work}/digest/artifact-digest.txt")"
# The control the refusals below need: absence is a real outcome, not a
# side effect of the field never being emitted.
check "a run that records no digest omits the field" "absent" \
  "$(subject_field "${work}/pass/lifecycle-report.json" artifact_digest)"

# A prefixed or uppercase digest violates the schema. Accepting either
# spelling would file evidence the release gate rejects, on a run that
# cannot be repeated cheaply.
set +e
( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_begin ubuntu2404-x86-l4-g2s8 "${work}/prefixed" 0.2.1 test-harness
  trap 'lifecycle_abort $?' EXIT
  lifecycle_stage install true
  lifecycle_artifact_digest "sha256:${digest_hex}" SHA256SUMS
  lifecycle_finish ) >/dev/null 2>&1
check "a sha256:-prefixed digest is a harness bug" "2" "$?"
set -e
# The guard is in the setter, not in the report writer: the run's own
# evidence survives the bug, and the bad value never reaches the report.
check "  and the run's evidence survives it" "yes" \
  "$([[ -f "${work}/prefixed/lifecycle-report.json" ]] && echo yes || echo no)"
check "  and the rejected value is not in the report" "absent" \
  "$(subject_field "${work}/prefixed/lifecycle-report.json" artifact_digest)"

set +e
( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_begin r "${work}/upper" 0.2.1 t
  lifecycle_artifact_digest "$(printf '%s' "$digest_hex" | tr 'a-f' 'A-F')" SHA256SUMS ) 2>/dev/null
check "  and an uppercase digest is refused too" "2" "$?"

( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_artifact_digest "$digest_hex" SHA256SUMS ) 2>/dev/null
check "a digest recorded before the run begins is a harness bug" "2" "$?"

( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_begin r "${work}/twice" 0.2.1 t
  lifecycle_artifact_digest "$digest_hex" SHA256SUMS
  lifecycle_artifact_digest "$digest_hex" SHA256SUMS ) 2>/dev/null
check "a digest recorded twice is a harness bug" "2" "$?"

( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  lifecycle_begin r "${work}/unlabelled" 0.2.1 t
  lifecycle_artifact_digest "$digest_hex" ) 2>/dev/null
unlabelled_status=$?
check "a digest that does not say what was hashed is refused" "yes" \
  "$([[ "$unlabelled_status" -ne 0 ]] && echo yes || echo no)"

# The sibling subject field had the same silent-invalid defect: a tag or
# a short SHA was written through and only failed at the release gate.
( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  export TP_LIFECYCLE_SOURCE_REVISION=v0.2.1
  lifecycle_begin r "${work}/badrev" 0.2.1 t ) 2>/dev/null
check "a malformed source revision is refused by the runner" "2" "$?"
set -e

revision="$(printf '%040d' 0 | tr 0 a)"
( set -Eeuo pipefail
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$harness"
  export TP_LIFECYCLE_SOURCE_REVISION="$revision"
  lifecycle_begin r "${work}/goodrev" 0.2.1 t
  trap 'lifecycle_abort $?' EXIT
  lifecycle_stage install true
  lifecycle_finish ) >/dev/null 2>&1
check "  and a full SHA is still carried" "$revision" \
  "$(subject_field "${work}/goodrev/lifecycle-report.json" source_revision)"

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

# --- The converter reads the digest from the evidence, not the operator.
#
# This is the producer both physical runbooks invoke, and the digest is
# the one value an operator could not re-derive after the run. It comes
# from a file the harness wrote beside its stage log, in sha256sum's own
# format.
stage_log() {
  local dir="$1"
  mkdir -p "$dir"
  printf 'stage\tstatus\tstarted_at\tfinished_at\tlog\n' >"${dir}/stages.tsv"
  printf 'clean-install\tpass\t2026-09-03T03:00:00Z\t2026-09-03T03:01:00Z\tclean-install.log\n' \
    >>"${dir}/stages.tsv"
}

convert_with_sidecar() {
  local dir="$1"
  set +e
  "$converter" "${dir}/stages.tsv" macos26-m1pro-16gb 0.2.1 macos-homebrew-lifecycle \
    "${dir}/lifecycle-report.json" clean-install=install >/dev/null 2>&1
  local status=$?
  set -e
  printf '%s' "$status"
}

cv="${work}/cv-good"; stage_log "$cv"
sha256_of "${cv}/stages.tsv" >"${cv}/artifact-digest.txt"
expected_digest="$(awk '{print $1}' "${cv}/artifact-digest.txt")"
check "the converter takes the digest from beside the stage log" "0" "$(convert_with_sidecar "$cv")"
check "  and carries the hex alone, with no trailing newline" "$expected_digest" \
  "$(subject_field "${cv}/lifecycle-report.json" artifact_digest)"

# The sidecar belongs to the run's evidence, which is where stages.tsv
# is. A converter reading beside its own output would work in the macOS
# flow and silently omit the field whenever the report is written
# elsewhere.
cv="${work}/cv-wrong-dir"; stage_log "$cv"; mkdir -p "${cv}/out"
sha256_of "${cv}/stages.tsv" >"${cv}/out/artifact-digest.txt"
set +e
"$converter" "${cv}/stages.tsv" macos26-m1pro-16gb 0.2.1 macos-homebrew-lifecycle \
  "${cv}/out/lifecycle-report.json" clean-install=install >/dev/null 2>&1
set -e
check "a digest beside the report rather than the evidence is not picked up" "absent" \
  "$(subject_field "${cv}/out/lifecycle-report.json" artifact_digest)"

cv="${work}/cv-none"; stage_log "$cv"
check "no digest at all is not an error" "0" "$(convert_with_sidecar "$cv")"
check "  and the field is simply absent" "absent" \
  "$(subject_field "${cv}/lifecycle-report.json" artifact_digest)"

# A sidecar that is there but wrong stops the conversion. Writing it
# through would file a report the gate rejects; dropping it silently
# would look exactly like a harness that never recorded one.
for case in prefixed upper empty unlabelled two-lines; do
  cv="${work}/cv-${case}"; stage_log "$cv"
  case "$case" in
    prefixed) printf 'sha256:%s  SHA256SUMS\n' "$expected_digest" >"${cv}/artifact-digest.txt" ;;
    upper) printf '%s  SHA256SUMS\n' "$(printf '%s' "$expected_digest" | tr 'a-f' 'A-F')" \
      >"${cv}/artifact-digest.txt" ;;
    empty) : >"${cv}/artifact-digest.txt" ;;
    unlabelled) printf '%s\n' "$expected_digest" >"${cv}/artifact-digest.txt" ;;
    two-lines) printf '%s  a\n%s  b\n' "$expected_digest" "$expected_digest" \
      >"${cv}/artifact-digest.txt" ;;
  esac
  check "a ${case} digest sidecar stops the conversion" "1" "$(convert_with_sidecar "$cv")"
  check "  and no report is written" "no" \
    "$([[ -f "${cv}/lifecycle-report.json" ]] && echo yes || echo no)"
done

cv="${work}/cv-rev"; stage_log "$cv"
set +e
( export TP_LIFECYCLE_SOURCE_REVISION=v0.2.1
  "$converter" "${cv}/stages.tsv" macos26-m1pro-16gb 0.2.1 macos-homebrew-lifecycle \
    "${cv}/lifecycle-report.json" clean-install=install ) >/dev/null 2>&1
check "the converter refuses a malformed source revision" "1" "$?"
set -e
( export TP_LIFECYCLE_SOURCE_REVISION="$revision"
  "$converter" "${cv}/stages.tsv" macos26-m1pro-16gb 0.2.1 macos-homebrew-lifecycle \
    "${cv}/lifecycle-report.json" clean-install=install ) >/dev/null 2>&1
check "  and carries a full SHA" "$revision" \
  "$(subject_field "${cv}/lifecycle-report.json" source_revision)"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "all checks passed" || echo "${failures} check(s) failed")"
exit "$failures"
