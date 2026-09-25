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

# A failing step must survive the whole adapter -> converter chain. The
# Jetson runbook now uses the native harness instead, but the adapter is
# still shipped, and a failure laundered here would reach the gate as a
# pass for anyone who converts clean-room evidence with it.
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
# This is the producer the macOS runbook invokes, and the digest is
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

# --- The reboot stage and the run it crosses.
#
# A reboot ends the process that holds the stage records, so the runner
# suspends the run to a marker and a new shell resumes it. Each half runs
# in its own `bash` here, as it does on the machine.
report_field() {
  python3 -c 'import json,sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2].split("."):
    value = value.get(key, "absent") if isinstance(value, dict) else "absent"
print(json.dumps(value) if isinstance(value, (bool, int, float)) else value)' "$1" "$2"
}

# before_reboot <dir> [stages]: the named canonical stages pass (install
# and upgrade by default), then the run suspends to <dir>/suspend.json.
before_reboot() {
  bash -c 'set -Eeuo pipefail
source "$1"
lifecycle_begin ubuntu2404-x86-l4-g2s8 "$2/evidence" 0.3.1 test-harness
trap "lifecycle_abort \$?" EXIT
for stage in $3; do lifecycle_stage "$stage" true; done
lifecycle_suspend "$2/suspend.json"' _ "$harness" "$1" "${2:-install upgrade}" 2>/dev/null
}

# resumed <dir> <script>: resume the run, then run the script. Its stderr
# goes to <dir>/stderr; the exit status is the script's.
resumed() {
  bash -c 'set -Eeuo pipefail
source "$1"
lifecycle_resume "$2/suspend.json"
trap "lifecycle_abort \$?" EXIT
eval "$3"' _ "$harness" "$1" "$2" 2>"${1}/stderr"
}

passing_reboot='lifecycle_reboot_begin true
lifecycle_reboot_subcase blocked true
lifecycle_reboot_retry_window 120
lifecycle_reboot_subcase transient true
lifecycle_reboot_subcase denied_egress_resumes true
lifecycle_reboot_finish'

rb="${work}/rb-pass"
before_reboot "$rb"
check "a suspended run writes the marker" "yes" "$([[ -f "${rb}/suspend.json" ]] && echo yes || echo no)"
check "  and no report before the reboot" "no" \
  "$([[ -f "${rb}/evidence/lifecycle-report.json" ]] && echo yes || echo no)"
resumed "$rb" "${passing_reboot}
lifecycle_finish"
report="${rb}/evidence/lifecycle-report.json"
check "the resumed run writes the report" "yes" "$([[ -f "$report" ]] && echo yes || echo no)"
check "  keeping the stages from before the reboot" "install upgrade" \
  "$(python3 -c 'import json,sys;print(" ".join(s["stage"] for s in json.load(open(sys.argv[1]))["stages"]))' "$report")"
check "  and the tested version" "0.3.1" "$(subject_field "$report" tested_version)"
check "a passing reboot stage is a pass" "pass" "$(report_field "$report" reboot.status)"
check "  records that the boot ID changed" "true" "$(report_field "$report" reboot.boot_id_changed)"
check "  and the observed retry window" "120" "$(report_field "$report" reboot.retry_window_seconds)"
for name in blocked transient denied_egress_resumes; do
  check "  and sub-case ${name} passed with its log" "pass yes" \
    "$(report_field "$report" "reboot.sub_cases.${name}.status") $([[ -f "${rb}/evidence/reboot-${name}.log" ]] && echo yes || echo no)"
done
check "  and writes the stage log" "yes" "$([[ -f "${rb}/evidence/reboot.log" ]] && echo yes || echo no)"
check "the marker resumes once: it is consumed" "no yes" \
  "$([[ -f "${rb}/suspend.json" ]] && echo yes || echo no) $([[ -f "${rb}/suspend.json.resumed" ]] && echo yes || echo no)"
set +e
resumed "$rb" "$passing_reboot"
status=$?
set -e
check "  and a second resume is refused" "2 yes" "${status} $(grep -q 'no suspend marker' "${rb}/stderr" && echo yes || echo no)"

# Seven canonical stages and a skip, then a passing reboot stage: the
# reboot stage is not one of the eight, so the run is still incomplete.
rb="${work}/rb-seven"
before_reboot "$rb" "install upgrade deploy-smoke status-logs rollback restart crash-loop"
resumed "$rb" "lifecycle_skip offline 'no denial on this host'
${passing_reboot}
lifecycle_finish"
check "a passing reboot stage does not stand in for a canonical stage" "incomplete" \
  "$(report_field "${rb}/evidence/lifecycle-report.json" outcome)"

rb="${work}/rb-fail"
before_reboot "$rb"
set +e
resumed "$rb" 'lifecycle_reboot_begin true
lifecycle_reboot_subcase blocked true
lifecycle_reboot_subcase transient false || :
lifecycle_reboot_subcase denied_egress_resumes true
lifecycle_reboot_finish
lifecycle_finish'
status=$?
set -e
report="${rb}/evidence/lifecycle-report.json"
check "a failing sub-case in a tested context is still a failure" "fail" \
  "$(report_field "$report" reboot.sub_cases.transient.status)"
check "  and fails the reboot stage" "fail" "$(report_field "$report" reboot.status)"
check "  and the run" "fail" "$(report_field "$report" outcome)"
check "  and the run still finishes" "0" "$status"

rb="${work}/rb-abort"
before_reboot "$rb"
set +e
resumed "$rb" 'lifecycle_reboot_begin true
lifecycle_reboot_subcase blocked true
lifecycle_reboot_subcase transient false'
status=$?
set -e
report="${rb}/evidence/lifecycle-report.json"
check "a run that stops after a failing sub-case exits non-zero" "1" "$status"
check "  and still writes the report" "yes" "$([[ -f "$report" ]] && echo yes || echo no)"
check "  naming the sub-case that failed" "fail" "$(report_field "$report" reboot.sub_cases.transient.status)"
check "  and the one never reached" "skipped" \
  "$(report_field "$report" reboot.sub_cases.denied_egress_resumes.status)"
check "  and the stage and run as failed" "fail fail" \
  "$(report_field "$report" reboot.status) $(report_field "$report" outcome)"

# A sub-case that ends the shell itself: the EXIT trap finds it running.
rb="${work}/rb-exit"
before_reboot "$rb"
set +e
resumed "$rb" 'quit() { exit 3; }
lifecycle_reboot_begin true
lifecycle_reboot_subcase blocked quit'
status=$?
set -e
report="${rb}/evidence/lifecycle-report.json"
check "a sub-case that exits the shell stops the run" "3" "$status"
check "  and is recorded as failed where it stopped" "fail yes" \
  "$(report_field "$report" reboot.sub_cases.blocked.status) $(grep -q 'stopped in this sub-case' "$report" && echo yes || echo no)"
check "  with the sub-cases after it not reached" "skipped skipped" \
  "$(report_field "$report" reboot.sub_cases.transient.status) $(report_field "$report" reboot.sub_cases.denied_egress_resumes.status)"

rb="${work}/rb-skip"
before_reboot "$rb"
resumed "$rb" 'lifecycle_reboot_begin true
lifecycle_reboot_subcase blocked true
lifecycle_reboot_retry_window 1.5
lifecycle_reboot_subcase transient true
lifecycle_reboot_skip denied_egress_resumes "the denial could not be applied"
lifecycle_reboot_finish
lifecycle_finish'
report="${rb}/evidence/lifecycle-report.json"
check "a skipped sub-case is recorded with its reason and log" "skipped the denial could not be applied yes" \
  "$(report_field "$report" reboot.sub_cases.denied_egress_resumes.status) $(report_field "$report" reboot.sub_cases.denied_egress_resumes.detail) $([[ -f "${rb}/evidence/reboot-denied_egress_resumes.log" ]] && echo yes || echo no)"
check "  and fails the stage" "fail" "$(report_field "$report" reboot.status)"
check "  and a fractional retry window is kept" "1.5" "$(report_field "$report" reboot.retry_window_seconds)"

# Everything else passes, so the boot ID is the only reason to fail.
rb="${work}/rb-same-boot"
before_reboot "$rb"
resumed "$rb" "${passing_reboot/lifecycle_reboot_begin true/lifecycle_reboot_begin false}
lifecycle_finish"
report="${rb}/evidence/lifecycle-report.json"
check "a reboot stage whose boot ID did not change fails" "fail fail" \
  "$(report_field "$report" reboot.status) $(report_field "$report" outcome)"
check "  and says why" "the boot ID did not change" "$(report_field "$report" reboot.detail)"

# Likewise the retry window: a stage that could have observed it and did
# not record it fails here rather than at the release gate.
rb="${work}/rb-no-window"
before_reboot "$rb"
resumed "$rb" "${passing_reboot/lifecycle_reboot_retry_window 120/}
lifecycle_finish"
report="${rb}/evidence/lifecycle-report.json"
check "a reboot stage with no retry window recorded fails" "fail fail" \
  "$(report_field "$report" reboot.status) $(report_field "$report" outcome)"
check "  and says why" "the retry window was not recorded" "$(report_field "$report" reboot.detail)"

rb="${work}/rb-unrun"
before_reboot "$rb"
resumed "$rb" 'lifecycle_reboot_begin true
lifecycle_reboot_retry_window 5
lifecycle_reboot_finish
lifecycle_finish'
report="${rb}/evidence/lifecycle-report.json"
check "sub-cases the harness never ran are recorded as skipped" "skipped skipped skipped" \
  "$(report_field "$report" reboot.sub_cases.blocked.status) $(report_field "$report" reboot.sub_cases.transient.status) $(report_field "$report" reboot.sub_cases.denied_egress_resumes.status)"
check "  and fail the stage" "fail" "$(report_field "$report" reboot.status)"

# A stop just after a sub-case is recorded, before the runner moves to the
# next: the sub-case keeps its record and its log. The counter is set back
# by hand because the real window is one statement wide.
rb="${work}/rb-recorded"
before_reboot "$rb"
set +e
resumed "$rb" 'say() { echo "the evidence"; return 1; }
lifecycle_reboot_begin true
lifecycle_reboot_subcase blocked say || :
_lc_rb_next=0
exit 143'
status=$?
set -e
report="${rb}/evidence/lifecycle-report.json"
check "a sub-case recorded just before a stop keeps its record" "143 fail yes" \
  "${status} $(report_field "$report" reboot.sub_cases.blocked.status) $(grep -q 'the evidence' "${rb}/evidence/reboot-blocked.log" && echo yes || echo no)"

# A failing sub-case whose last lines are longer than the argument limit
# (1 MiB here; 128 KiB per argument on Linux): its detail is bounded so the
# stage can still be composed, and the log keeps everything.
rb="${work}/rb-long-tail"
before_reboot "$rb"
set +e
resumed "$rb" 'long() { python3 -c "print(\"x\" * 1100000)"; return 1; }
lifecycle_reboot_begin true
lifecycle_reboot_subcase blocked long || :
lifecycle_reboot_retry_window 5
lifecycle_reboot_subcase transient true
lifecycle_reboot_subcase denied_egress_resumes true
lifecycle_reboot_finish
lifecycle_finish'
status=$?
set -e
report="${rb}/evidence/lifecycle-report.json"
check "a sub-case with a very long log tail does not stop the run" "0" "$status"
check "  and is recorded with a bounded detail" "fail yes" \
  "$(report_field "$report" reboot.sub_cases.blocked.status 2>/dev/null) $(python3 -c 'import json,sys;print("yes" if len(json.load(open(sys.argv[1]))["reboot"]["sub_cases"]["blocked"]["detail"]) < 2100 else "no")' "$report" 2>/dev/null)"
check "  and its log keeps the whole output" "yes" \
  "$([[ $(wc -c <"${rb}/evidence/reboot-blocked.log") -gt 1100000 ]] && echo yes || echo no)"

# When the stage cannot be composed at all, a run stopping inside it still
# writes a failed report, keeps its exit status and returns to the
# caller's own cleanup: the EXIT trap must never exit on the harness's
# behalf. python3 is made to fail for the compose call alone.
rb="${work}/rb-unrecorded"
before_reboot "$rb"
set +e
resumed "$rb" 'python3() {
  if [[ "${1:-}" == - && "${2:-}" == "${_lc_rb_started:-}" ]]; then return 1; fi
  command python3 "$@"
}
trap "lifecycle_abort \$?; echo ran >\"\$2/cleanup\"" EXIT
lifecycle_reboot_begin true
lifecycle_reboot_subcase blocked true
exit 3'
status=$?
set -e
report="${rb}/evidence/lifecycle-report.json"
check "a reboot stage that cannot be composed is recorded as failed" "fail fail" \
  "$(report_field "$report" reboot.status) $(report_field "$report" outcome)"
check "  and says so" "the reboot stage could not be recorded" "$(report_field "$report" reboot.detail)"
check "  keeping the run's exit status and the caller's cleanup" "3 yes" \
  "${status} $([[ -f "${rb}/cleanup" ]] && echo yes || echo no)"
check "  and the sub-case log it had" "yes" "$([[ -f "${rb}/evidence/reboot-blocked.log" ]] && echo yes || echo no)"

# Harness bugs stop the run with exit 2 and their own message: the script
# ends at the offending call, so the message is the only thing that can say
# which guard fired. A call made from inside a stage writes its message to
# that stage's log. Arguments are checked explicitly, not with `${2:?}`:
# under `set -e`, bash 3.2 ends the shell with status 0 on that error.
refused() {
  local what="$1" message="$2" script="$3" rb status=0
  case_count=$((${case_count:-0} + 1))
  rb="${work}/rb-refused-${case_count}"
  before_reboot "$rb"
  resumed "$rb" "$script" || status=$?
  check "${what} is refused" "2 yes" \
    "${status} $(grep -qsF -- "$message" "${rb}/stderr" "${rb}"/evidence/*.log && echo yes || echo no)"
}
refused "a sub-case before lifecycle_reboot_begin" "before lifecycle_reboot_begin" \
  'lifecycle_reboot_subcase blocked true'
refused "a boot ID verdict that is not true or false" "not true or false" \
  'lifecycle_reboot_begin maybe'
refused "a sub-case out of order" "out of order" \
  'lifecycle_reboot_begin true; lifecycle_reboot_subcase transient true'
refused "a skipped sub-case out of order" "out of order" \
  'lifecycle_reboot_begin true; lifecycle_reboot_skip transient "x"'
refused "a skipped sub-case without a reason" "a skipped sub-case needs a reason" \
  'lifecycle_reboot_begin true; lifecycle_reboot_skip blocked'
refused "a sub-case with no command" "has no command to run" \
  'lifecycle_reboot_begin true; lifecycle_reboot_subcase blocked'
refused "a sub-case with no name" "out of order" \
  'lifecycle_reboot_begin true; lifecycle_reboot_subcase'
refused "a missing boot ID verdict" "not true or false" \
  'lifecycle_reboot_begin'
refused "a missing retry window" "not a positive number" \
  'lifecycle_reboot_begin true; lifecycle_reboot_retry_window'
refused "beginning the reboot stage twice" "already begun" \
  'lifecycle_reboot_begin true; lifecycle_reboot_begin true'
refused "a canonical stage inside the reboot stage" "inside the reboot stage; run its checks" \
  'lifecycle_reboot_begin true; lifecycle_stage offline true'
refused "a canonical stage skipped inside the reboot stage" "skipped inside the reboot stage" \
  'lifecycle_reboot_begin true; lifecycle_skip offline "x"'
refused "a retry window of zero" "not a positive number" \
  'lifecycle_reboot_begin true; lifecycle_reboot_retry_window 0'
refused "a retry window that is not a number" "not a positive number" \
  'lifecycle_reboot_begin true; lifecycle_reboot_retry_window soon'
refused "a retry window of ten digits" "not a positive number" \
  'lifecycle_reboot_begin true; lifecycle_reboot_retry_window 1234567890'
refused "a retry window outside the reboot stage" "outside the reboot stage" \
  'lifecycle_reboot_retry_window 5'
refused "finishing the run with the reboot stage open" "still open" \
  'lifecycle_reboot_begin true; lifecycle_finish'
refused "finishing the reboot stage twice" "already finished" \
  'lifecycle_reboot_begin true; lifecycle_reboot_finish; lifecycle_reboot_finish'
refused "finishing the reboot stage before it begins" "before lifecycle_reboot_begin" \
  'lifecycle_reboot_finish'
refused "a sub-case after the reboot stage finished" "after lifecycle_reboot_finish" \
  'lifecycle_reboot_begin true; lifecycle_reboot_finish; lifecycle_reboot_subcase blocked true'
refused "a fourth sub-case" "after all three" \
  'lifecycle_reboot_begin true; lifecycle_reboot_subcase blocked true; lifecycle_reboot_subcase transient true; lifecycle_reboot_subcase denied_egress_resumes true; lifecycle_reboot_subcase blocked true'
refused "beginning the reboot stage during a stage" "during stage" \
  'lifecycle_stage install lifecycle_reboot_begin true'
refused "beginning the reboot stage after the run finished" "after the run finished" \
  'lifecycle_finish; lifecycle_reboot_begin true'
refused "suspending during a stage" "lifecycle_suspend during stage" \
  'lifecycle_stage install lifecycle_suspend "${2}/again.json"'
refused "suspending inside the reboot stage" "lifecycle_suspend inside the reboot stage" \
  'lifecycle_reboot_begin true; lifecycle_suspend "${2}/again.json"'
refused "suspending after the run finished" "lifecycle_suspend after the run finished" \
  'lifecycle_finish; lifecycle_suspend "${2}/again.json"'
refused "suspending with no marker path" "lifecycle_suspend needs a marker path" \
  'lifecycle_suspend'

rb="${work}/rb-unstarted"
mkdir -p "$rb"
set +e
bash -c 'set -Eeuo pipefail; source "$1"; lifecycle_reboot_begin true' _ "$harness" 2>"${rb}/stderr"
check "the reboot stage before any run is refused" "yes" \
  "$([[ $? -ne 0 ]] && grep -q 'before lifecycle_begin or lifecycle_resume' "${rb}/stderr" && echo yes || echo no)"
bash -c 'set -Eeuo pipefail; source "$1"; lifecycle_suspend "$2/m.json"' _ "$harness" "$rb" 2>"${rb}/stderr"
check "suspending before any run is refused" "yes" \
  "$([[ $? -ne 0 ]] && grep -q 'lifecycle_suspend before lifecycle_begin' "${rb}/stderr" && echo yes || echo no)"
bash -c 'set -Eeuo pipefail; source "$1"; lifecycle_resume' _ "$harness" 2>"${rb}/stderr"
check "resuming with no marker path is refused" "yes" \
  "$([[ $? -eq 2 ]] && grep -q 'lifecycle_resume needs a marker path' "${rb}/stderr" && echo yes || echo no)"
set -e

# Markers: each negative case is the valid marker of a suspended run with
# one thing wrong, so the named check is the only one that can refuse it.
# marker_case <what> <message> <python edit of `state`, or a shell action>
marker_case() {
  local what="$1" message="$2" edit="$3" rb status=0
  case_count=$((${case_count:-0} + 1))
  rb="${work}/rb-marker-${case_count}"
  before_reboot "$rb"
  if [[ "$edit" == shell:* ]]; then
    eval "${edit#shell:}"
  else
    python3 - "${rb}/suspend.json" "$edit" <<'PY'
import json, os, sys
path, edit = sys.argv[1], sys.argv[2]
stamp = os.stat(path)
state = json.load(open(path))
exec(edit)
json.dump(state, open(path, "w"))
os.utime(path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
PY
  fi
  resumed "$rb" "$passing_reboot" || status=$?
  check "${what} is refused" "2 yes" \
    "${status} $(grep -qF -- "$message" "${rb}/stderr" && echo yes || echo no)"
}
marker_case "a marker of another kind" "not a lifecycle run" 'state["marker"] = "something-else"'
marker_case "a marker of another version" "not a lifecycle run" 'state["version"] = 2'
marker_case "a marker carrying a record for no known stage" "not a lifecycle run" \
  'state["records"].append("{\"stage\":\"not-a-stage\"}")'
marker_case "a marker whose tested version is not a release" "is not a release version" \
  'state["tested_version"] = "v0.3"'
marker_case "a marker whose revision is not a full SHA" "is not a full git SHA" \
  'state["source_revision"] = "abc1234"'
marker_case "a marker whose digest is not lowercase hex" "is not bare lowercase sha256 hex" \
  'state["artifact_digest"] = "A" * 64'
marker_case "a marker whose evidence directory is gone" "evidence directory is gone" \
  'shell:rm -rf "${rb}/evidence"'
marker_case "a missing marker" "no suspend marker" 'shell:rm -f "${rb}/suspend.json"'
# Another run finished in the same evidence directory after the marker was
# written: resuming would write the older run's records over its result.
marker_case "a marker older than a report in its evidence directory" "after this marker was written" \
  'shell:bash -c '"'"'source "$1"; lifecycle_begin ubuntu2404-x86-l4-g2s8 "$2/evidence" 0.3.1 t; lifecycle_stage install false || :; lifecycle_finish'"'"' _ "$harness" "$rb" 2>/dev/null'

if ! bash "${repo_root}/test/validation/lifecycle_retry_test.sh"; then
  failures=$((failures + 1))
fi

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "all checks passed" || echo "${failures} check(s) failed")"
exit "$failures"
