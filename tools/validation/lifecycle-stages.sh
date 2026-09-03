#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Shared lifecycle-stage runner for the validation harnesses.
#
# Sourced, not executed. The macOS harness already had this shape and the
# Jetson one did not, so the two produced different evidence for the same
# eight stages and only one of them was machine-readable. Both now emit
# the same report, because the release gate has to decide completeness by
# reading it rather than by reading prose.
#
# The failure discipline is the part worth preserving from the original:
# a stage that fails must APPEAR in the report as a failure. Under
# `set -e` the runner never reaches its own bookkeeping, so the caller
# installs an EXIT trap that records the stage left in flight. A harness
# that merely stopped writing would produce a short report that reads
# exactly like a short run.
#
# Usage:
#   source tools/validation/lifecycle-stages.sh
#   lifecycle_begin <row_id> <evidence_dir> [harness_name]
#   trap 'lifecycle_abort $?' EXIT
#   lifecycle_stage install install_fn args...
#   lifecycle_skip upgrade "no prior release on this row"
#   lifecycle_finish            # writes the report, clears the trap

set -Eeuo pipefail

LIFECYCLE_STAGES=(
  install
  upgrade
  deploy-smoke
  status-logs
  rollback
  restart
  crash-loop
  offline
)

_lc_row_id=""
_lc_harness=""
_lc_dir=""
_lc_report=""
_lc_started=""
_lc_active=""
_lc_active_started=""
_lc_active_log=""
_lc_records=()

_lc_now() { date -u +%Y-%m-%dT%H:%M:%SZ; }

_lc_die() {
  printf 'lifecycle: %s\n' "$1" >&2
  exit 2
}

# JSON string escaping. The detail field carries error text from the
# machine under test, which is not ours to trust as well-formed.
_lc_json_escape() {
  python3 -c 'import json,sys; sys.stdout.write(json.dumps(sys.argv[1]))' "$1"
}

_lc_known_stage() {
  local candidate="$1" known
  for known in "${LIFECYCLE_STAGES[@]}"; do
    [[ "$candidate" == "$known" ]] && return 0
  done
  return 1
}

lifecycle_begin() {
  _lc_row_id="${1:?row id required}"
  _lc_dir="${2:?evidence dir required}"
  _lc_harness="${3:-$(basename "${BASH_SOURCE[1]:-unknown}")}"
  mkdir -p "$_lc_dir"
  _lc_report="${_lc_dir}/lifecycle-report.json"
  _lc_started="$(_lc_now)"
  _lc_records=()
}

_lc_record() {
  local stage="$1" status="$2" started="$3" finished="$4" log="$5" detail="${6:-}"
  local entry
  entry="$(printf '{"stage":"%s","status":"%s","started_at":"%s","finished_at":"%s","log":"%s"' \
    "$stage" "$status" "$started" "$finished" "$log")"
  if [[ -n "$detail" ]]; then
    entry="${entry},\"detail\":$(_lc_json_escape "$detail")"
  fi
  _lc_records+=("${entry}}")
}

# Run one stage, capturing its output. On success the record is written
# here; on failure `set -e` unwinds to the caller's trap, which calls
# lifecycle_abort.
lifecycle_stage() {
  local stage="$1"
  shift
  _lc_known_stage "$stage" || _lc_die "unknown stage \`${stage}\`"
  _lc_active="$stage"
  _lc_active_started="$(_lc_now)"
  _lc_active_log="${stage}.log"
  printf '== stage %s\n' "$stage" >&2
  "$@" >"${_lc_dir}/${_lc_active_log}" 2>&1
  _lc_record "$stage" pass "$_lc_active_started" "$(_lc_now)" "$_lc_active_log"
  _lc_active=""
}

# A stage that does not apply to this row. Requires a reason: an
# unexplained skip is indistinguishable from a stage nobody ran.
lifecycle_skip() {
  local stage="$1" reason="${2:?a skipped stage needs a reason}"
  _lc_known_stage "$stage" || _lc_die "unknown stage \`${stage}\`"
  local now
  now="$(_lc_now)"
  : >"${_lc_dir}/${stage}.log"
  _lc_record "$stage" skipped "$now" "$now" "${stage}.log" "$reason"
}

# Called from the caller's EXIT trap. Records the in-flight stage as a
# failure and writes the report, so an aborted run leaves evidence of
# where it stopped rather than no evidence at all.
lifecycle_abort() {
  local status="${1:-1}"
  set +e
  if [[ "$status" -ne 0 && -n "$_lc_active" ]]; then
    local detail="stage exited ${status}"
    if [[ -s "${_lc_dir}/${_lc_active_log}" ]]; then
      detail="${detail}: $(tail -n 3 "${_lc_dir}/${_lc_active_log}" | tr '\n' ' ')"
    fi
    _lc_record "$_lc_active" fail "$_lc_active_started" "$(_lc_now)" "$_lc_active_log" "$detail"
    _lc_active=""
  fi
  [[ -n "$_lc_report" ]] && _lc_write_report
  return 0
}

_lc_write_report() {
  local outcome="pass" record
  for record in "${_lc_records[@]}"; do
    case "$record" in *'"status":"fail"'*) outcome="fail" ;; esac
  done
  {
    printf '{\n  "schema_version": "0.1",\n'
    printf '  "row_id": %s,\n' "$(_lc_json_escape "$_lc_row_id")"
    printf '  "harness": %s,\n' "$(_lc_json_escape "$_lc_harness")"
    printf '  "started_at": "%s",\n  "finished_at": "%s",\n' "$_lc_started" "$(_lc_now)"
    printf '  "outcome": "%s",\n  "stages": [\n' "$outcome"
    local i
    for i in "${!_lc_records[@]}"; do
      printf '    %s' "${_lc_records[$i]}"
      [[ "$i" -lt $(( ${#_lc_records[@]} - 1 )) ]] && printf ','
      printf '\n'
    done
    printf '  ]\n}\n'
  } >"$_lc_report"
}

# Write the report for a run that completed. Clears the trap so the
# caller's own cleanup does not write it twice.
lifecycle_finish() {
  _lc_write_report
  trap - EXIT
  printf '== lifecycle report: %s\n' "$_lc_report" >&2
}
