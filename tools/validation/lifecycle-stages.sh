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
# a stage that fails must APPEAR in the report as a failure. lifecycle_stage
# therefore captures its command's status and classifies the stage itself,
# rather than letting `set -e` unwind past its own bookkeeping. Relying on
# ambient errexit was unsound: bash disables -e inside a function invoked
# from a tested context, so `lifecycle_stage install false || :` recorded a
# pass for a command that failed. The EXIT trap remains as a backstop for
# signals and for exits raised outside a stage.
#
# Usage:
#   source tools/validation/lifecycle-stages.sh
#   lifecycle_begin <row_id> <evidence_dir> <tested_version> [harness_name]
#   trap 'lifecycle_abort $?' EXIT
#   lifecycle_stage install install_fn args...
#   lifecycle_artifact_digest <sha256> <what was hashed>
#   lifecycle_skip upgrade "no prior release on this row"
#   lifecycle_finish            # writes the report
#
# The reboot stage, which crosses a host reboot, sits beside the eight and
# only the rows the release gate names run it:
#   lifecycle_suspend <marker>  # before the reboot; the marker is not
#                               # evidence, keep it outside the evidence dir
#   ... reboot; in the new boot, a new shell:
#   source tools/validation/lifecycle-stages.sh
#   lifecycle_resume <marker>
#   trap 'lifecycle_abort $?' EXIT
#   lifecycle_reboot_begin true|false      # did the boot ID change?
#   lifecycle_reboot_subcase blocked fn args...
#   lifecycle_reboot_retry_window <seconds>
#   lifecycle_reboot_subcase transient fn args...
#   lifecycle_reboot_subcase denied_egress_resumes fn args...
#   lifecycle_reboot_finish
#   lifecycle_finish
# Sub-cases run in that order, because each consumes the state the one
# before it leaves; lifecycle_reboot_skip <sub-case> <reason> records one
# that did not run. A sub-case that re-runs a canonical stage's checks
# calls that stage's function directly: a second `lifecycle_stage` of the
# same name is a repeated stage, which the release gate refuses.
#
# TP_LIFECYCLE_SOURCE_REVISION, if set, is the full 40-hex git SHA the
# tested artifacts were built from. Anything else is refused rather than
# written through: a report carrying a tag or a short SHA violates the
# schema, and the release gate is a worse place to learn that than the
# machine the run is happening on.

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

LIFECYCLE_REBOOT_SUB_CASES=(blocked transient denied_egress_resumes)

_lc_row_id=""
_lc_harness=""
_lc_version=""
_lc_source_revision=""
_lc_artifact_digest=""
_lc_finished=0
_lc_dir=""
_lc_report=""
_lc_started=""
_lc_active=""
_lc_active_started=""
_lc_active_log=""
_lc_records=()
# The reboot stage: begun at, the boot-ID verdict, the index of the next
# sub-case, the one running, the observed retry window, each sub-case's
# JSON, and the stage's JSON once finished.
_lc_rb_started=""
_lc_rb_boot_changed=""
_lc_rb_next=0
_lc_rb_active=""
_lc_rb_window=""
_lc_rb_subs=()
_lc_rb_json=""

_lc_reboot_reset() {
  _lc_rb_started=""
  _lc_rb_boot_changed=""
  _lc_rb_next=0
  _lc_rb_active=""
  _lc_rb_window=""
  _lc_rb_subs=()
  _lc_rb_json=""
}

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
  # Required: a report that does not say which version it exercised cannot
  # authorize a release, because nothing stops it being reused for a later one.
  _lc_version="${3:?tested version required}"
  _lc_harness="${4:-$(basename "${BASH_SOURCE[1]:-unknown}")}"
  [[ "$_lc_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
    _lc_die "tested version \`${_lc_version}\` is not a release version"
  _lc_source_revision="${TP_LIFECYCLE_SOURCE_REVISION:-}"
  [[ -z "$_lc_source_revision" || "$_lc_source_revision" =~ ^[0-9a-f]{40}$ ]] ||
    _lc_die "source revision \`${_lc_source_revision}\` is not a full git SHA"
  _lc_artifact_digest=""
  mkdir -p "$_lc_dir"
  # The converter also reads this sidecar; resetting only the in-memory
  # value would let it recover a previous run's digest after a retry.
  rm -f "${_lc_dir}/artifact-digest.txt" ||
    _lc_die "could not clear the previous artifact digest"
  _lc_report="${_lc_dir}/lifecycle-report.json"
  _lc_started="$(_lc_now)"
  _lc_records=()
  _lc_finished=0
  _lc_reboot_reset
}

# Record the artifact this run installed, as the sha256 of the one
# immutable file the install verified, plus what that file was.
#
# Called by the harness after its install stage, not read from the
# environment: the value cannot be recovered once the run is over, and
# the harness is the only party that knows which bytes it trusted. The
# hex goes into the report; the pair is also written to
# `artifact-digest.txt` beside the stage logs, because `subject` is a
# closed object with nowhere to say what was hashed, and a digest nobody
# can attribute later is not evidence.
#
# The value is refused rather than normalized. The repository's other
# digests are `sha256:`-prefixed, and quietly accepting that spelling
# here would file a report the schema rejects at the release gate.
lifecycle_artifact_digest() {
  local digest="${1:?artifact digest required}" what="${2:?what was hashed is required}"
  [[ -n "$_lc_report" ]] || _lc_die "lifecycle_artifact_digest before lifecycle_begin"
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] ||
    _lc_die "artifact digest \`${digest}\` is not bare lowercase sha256 hex"
  # One run installs one artifact set. A second value means the harness
  # does not know which one the report is about.
  [[ -z "$_lc_artifact_digest" ]] ||
    _lc_die "artifact digest already recorded as ${_lc_artifact_digest}"
  _lc_artifact_digest="$digest"
  printf '%s  %s\n' "$digest" "$what" >"${_lc_dir}/artifact-digest.txt"
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

# Run one stage, capturing its output and its status.
#
# The status is captured explicitly rather than inferred from control flow.
# Under `set -e` a failing command would unwind before the pass record was
# written -- but bash suspends errexit inside a function called from a
# tested context (`lifecycle_stage install false || :`), and there the
# unconditional record ran and certified a failure as a pass. Branching on
# the captured status is correct in both contexts, and returning the
# original status keeps the caller's errexit behaviour unchanged.
lifecycle_stage() {
  local stage="$1"
  shift
  _lc_known_stage "$stage" || _lc_die "unknown stage \`${stage}\`"
  [[ -z "$_lc_rb_started" || -n "$_lc_rb_json" ]] ||
    _lc_die "stage \`${stage}\` inside the reboot stage; run its checks as a sub-case"
  _lc_active="$stage"
  _lc_active_started="$(_lc_now)"
  _lc_active_log="${stage}.log"
  printf '== stage %s\n' "$stage" >&2

  local status=0
  "$@" >"${_lc_dir}/${_lc_active_log}" 2>&1 || status=$?

  if (( status != 0 )); then
    local detail="stage exited ${status}"
    if [[ -s "${_lc_dir}/${_lc_active_log}" ]]; then
      detail="${detail}: $(tail -n 3 "${_lc_dir}/${_lc_active_log}" | tr '\n' ' ')"
    fi
    _lc_record "$stage" fail "$_lc_active_started" "$(_lc_now)" "$_lc_active_log" "$detail"
    _lc_active=""
    return "$status"
  fi

  _lc_record "$stage" pass "$_lc_active_started" "$(_lc_now)" "$_lc_active_log"
  _lc_active=""
}

# A stage that does not apply to this row. Requires a reason: an
# unexplained skip is indistinguishable from a stage nobody ran.
lifecycle_skip() {
  local stage="$1" reason="${2:?a skipped stage needs a reason}"
  _lc_known_stage "$stage" || _lc_die "unknown stage \`${stage}\`"
  [[ -z "$_lc_rb_started" || -n "$_lc_rb_json" ]] ||
    _lc_die "stage \`${stage}\` skipped inside the reboot stage"
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
  # lifecycle_finish already wrote the report. Returning here rather than
  # clearing the EXIT trap leaves the caller's own cleanup handler intact:
  # this file is sourced, and the harnesses chain real cleanup (restoring
  # packages, services and Homebrew formulae) off the same trap.
  (( _lc_finished )) && return 0
  if [[ "$status" -ne 0 && -n "$_lc_active" ]]; then
    local detail="stage exited ${status}"
    if [[ -s "${_lc_dir}/${_lc_active_log}" ]]; then
      detail="${detail}: $(tail -n 3 "${_lc_dir}/${_lc_active_log}" | tr '\n' ' ')"
    fi
    _lc_record "$_lc_active" fail "$_lc_active_started" "$(_lc_now)" "$_lc_active_log" "$detail"
    _lc_active=""
  fi
  # A run that stops inside the reboot stage records where: the sub-case
  # that was running failed, and the ones after it were not reached.
  if [[ -n "$_lc_rb_started" && -z "$_lc_rb_json" ]]; then
    if [[ -n "$_lc_rb_active" ]]; then
      _lc_reboot_set "$_lc_rb_active" fail "reboot-${_lc_rb_active}.log" "the run stopped in this sub-case (exit ${status})"
      _lc_rb_active=""
      _lc_rb_next=$((_lc_rb_next + 1))
    fi
    _lc_reboot_close "the run stopped inside the reboot stage (exit ${status})" ||
      _lc_reboot_unrecorded
  fi
  [[ -n "$_lc_report" ]] && _lc_write_report
  return 0
}

# pass requires all eight canonical stages present and passing. Treating
# "nothing failed" as a pass let a run of two passes and six skips certify
# itself, which is the exact claim the release gate exists to refuse.
_lc_write_report() {
  local outcome record stage passed=0 failed=0
  for record in ${_lc_records[@]+"${_lc_records[@]}"}; do
    case "$record" in *'"status":"fail"'*) failed=1 ;; esac
  done
  case "$_lc_rb_json" in '{"status":"fail"'*) failed=1 ;; esac
  for stage in "${LIFECYCLE_STAGES[@]}"; do
    for record in ${_lc_records[@]+"${_lc_records[@]}"}; do
      case "$record" in
        *"\"stage\":\"${stage}\",\"status\":\"pass\""*) (( ++passed )); break ;;
      esac
    done
  done
  if (( failed )); then
    outcome="fail"
  elif (( passed == ${#LIFECYCLE_STAGES[@]} )); then
    outcome="pass"
  else
    outcome="incomplete"
  fi
  {
    printf '{\n  "schema_version": "0.1",\n'
    printf '  "row_id": %s,\n' "$(_lc_json_escape "$_lc_row_id")"
    printf '  "subject": {\n    "tested_version": %s' "$(_lc_json_escape "$_lc_version")"
    if [[ -n "$_lc_source_revision" ]]; then
      printf ',\n    "source_revision": %s' "$(_lc_json_escape "$_lc_source_revision")"
    fi
    if [[ -n "$_lc_artifact_digest" ]]; then
      printf ',\n    "artifact_digest": %s' "$(_lc_json_escape "$_lc_artifact_digest")"
    fi
    printf '\n  },\n'
    printf '  "harness": %s,\n' "$(_lc_json_escape "$_lc_harness")"
    printf '  "started_at": "%s",\n  "finished_at": "%s",\n' "$_lc_started" "$(_lc_now)"
    printf '  "outcome": "%s",\n' "$outcome"
    if [[ -n "$_lc_rb_json" ]]; then
      printf '  "reboot": %s,\n' "$_lc_rb_json"
    fi
    printf '  "stages": [\n'
    local i
    for i in "${!_lc_records[@]}"; do
      printf '    %s' "${_lc_records[$i]}"
      [[ "$i" -lt $(( ${#_lc_records[@]} - 1 )) ]] && printf ','
      printf '\n'
    done
    printf '  ]\n}\n'
  } >"$_lc_report"
}

# Write the report for a run that completed. Marks the run finished so a
# later lifecycle_abort is a no-op, without touching the caller's EXIT trap.
lifecycle_finish() {
  [[ -z "$_lc_rb_started" || -n "$_lc_rb_json" ]] ||
    _lc_die "lifecycle_finish with the reboot stage still open; call lifecycle_reboot_finish"
  _lc_write_report
  _lc_finished=1
  printf '== lifecycle report: %s\n' "$_lc_report" >&2
}

# --- The reboot stage.

# Persist this run so a new shell in the next boot can resume it. The
# report is written only when the run finishes, and a reboot ends the
# process that holds the records, so they go to a marker file first:
# written to a temporary name, flushed to disk and renamed into place,
# because the reboot can follow immediately. The run is then closed in
# this process, so its exit does not write a partial report over nothing.
lifecycle_suspend() {
  local marker="${1:-}"
  [[ -n "$marker" ]] || _lc_die "lifecycle_suspend needs a marker path"
  [[ -n "$_lc_report" ]] || _lc_die "lifecycle_suspend before lifecycle_begin"
  [[ -z "$_lc_active" ]] || _lc_die "lifecycle_suspend during stage \`${_lc_active}\`"
  [[ -z "$_lc_rb_started" ]] || _lc_die "lifecycle_suspend inside the reboot stage"
  (( _lc_finished == 0 )) || _lc_die "lifecycle_suspend after the run finished"
  python3 - "$marker" "$_lc_row_id" "$_lc_dir" "$_lc_version" "$_lc_harness" \
    "$_lc_started" "$_lc_source_revision" "$_lc_artifact_digest" \
    ${_lc_records[@]+"${_lc_records[@]}"} <<'PY' || _lc_die "could not write the suspend marker"
import json, os, sys
marker, row, evidence, version, harness, started, revision, digest = sys.argv[1:9]
state = {
    "marker": "lifecycle-suspend",
    "version": 1,
    "row_id": row,
    "evidence_dir": os.path.abspath(evidence),
    "tested_version": version,
    "harness": harness,
    "started_at": started,
    "source_revision": revision,
    "artifact_digest": digest,
    "records": sys.argv[9:],
}
temporary = marker + ".tmp"
with open(temporary, "w", encoding="utf-8") as handle:
    json.dump(state, handle)
    handle.flush()
    os.fsync(handle.fileno())
os.replace(temporary, marker)
directory = os.open(os.path.dirname(os.path.abspath(marker)), os.O_RDONLY)
try:
    os.fsync(directory)
finally:
    os.close(directory)
PY
  _lc_finished=1
}

# Restore a suspended run in this shell. Every value is checked as
# lifecycle_begin checks it: the marker sat on disk across a reboot. A
# marker resumes once: it is renamed to <marker>.resumed before the run
# continues, so a second resume, by an operator retrying or by leftover
# automation, cannot replay the run. And a marker older than a report in
# its evidence directory is refused: another run finished there since, and
# resuming would write this run's records over that run's result.
lifecycle_resume() {
  local marker="${1:-}" value
  [[ -n "$marker" ]] || _lc_die "lifecycle_resume needs a marker path"
  [[ -f "$marker" ]] || _lc_die "no suspend marker at ${marker}"
  local fields=()
  while IFS= read -r -d '' value; do
    fields+=("$value")
  done < <(python3 - "$marker" "${LIFECYCLE_STAGES[@]}" <<'PY' || printf 'INVALID\0'
import json, sys
path, known = sys.argv[1], set(sys.argv[2:])
try:
    with open(path, encoding="utf-8") as handle:
        state = json.load(handle)
    keys = ["row_id", "evidence_dir", "tested_version", "harness", "started_at",
            "source_revision", "artifact_digest"]
    if state.get("marker") != "lifecycle-suspend" or state.get("version") != 1:
        raise ValueError("not a suspend marker")
    values = [state[key] for key in keys]
    records = state["records"]
    if not all(isinstance(v, str) for v in values + records):
        raise ValueError("a value is not a string")
    for record in records:
        entry = json.loads(record)
        if not isinstance(entry, dict) or entry.get("stage") not in known:
            raise ValueError("a record is not a stage record")
    out = values + [str(len(records))] + records
    if any("\0" in v for v in out):
        raise ValueError("a value holds NUL")
except (OSError, ValueError, KeyError, TypeError):
    sys.exit(1)
sys.stdout.write("".join(v + "\0" for v in out))
PY
  )
  [[ ${#fields[@]} -ge 8 && "${fields[0]}" != INVALID ]] ||
    _lc_die "the suspend marker at ${marker} is not a lifecycle run"
  [[ "${fields[${#fields[@]}-1]}" != INVALID ]] ||
    _lc_die "the suspend marker at ${marker} is not a lifecycle run"
  _lc_row_id="${fields[0]}"
  _lc_dir="${fields[1]}"
  _lc_version="${fields[2]}"
  _lc_harness="${fields[3]}"
  _lc_started="${fields[4]}"
  _lc_source_revision="${fields[5]}"
  _lc_artifact_digest="${fields[6]}"
  [[ "${fields[7]}" =~ ^[0-9]+$ && $(( ${#fields[@]} - 8 )) -eq "${fields[7]}" ]] ||
    _lc_die "the suspend marker at ${marker} is not a lifecycle run"
  _lc_records=("${fields[@]:8}")
  [[ -n "$_lc_row_id" && -d "$_lc_dir" ]] ||
    _lc_die "the suspended run's evidence directory is gone"
  # Nanoseconds, not `-nt`: bash 3.2 compares whole seconds.
  python3 -c 'import os, sys
report = os.path.join(sys.argv[1], "lifecycle-report.json")
newer = os.path.exists(report) and os.stat(report).st_mtime_ns > os.stat(sys.argv[2]).st_mtime_ns
sys.exit(1 if newer else 0)' "$_lc_dir" "$marker" ||
    _lc_die "a run finished in ${_lc_dir} after this marker was written; it cannot be resumed"
  [[ "$_lc_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
    _lc_die "tested version \`${_lc_version}\` is not a release version"
  [[ -z "$_lc_source_revision" || "$_lc_source_revision" =~ ^[0-9a-f]{40}$ ]] ||
    _lc_die "source revision \`${_lc_source_revision}\` is not a full git SHA"
  [[ -z "$_lc_artifact_digest" || "$_lc_artifact_digest" =~ ^[0-9a-f]{64}$ ]] ||
    _lc_die "artifact digest \`${_lc_artifact_digest}\` is not bare lowercase sha256 hex"
  python3 - "$marker" <<'PY' || _lc_die "could not consume the suspend marker"
import os, sys
marker = sys.argv[1]
os.replace(marker, marker + ".resumed")
directory = os.open(os.path.dirname(os.path.abspath(marker)), os.O_RDONLY)
try:
    os.fsync(directory)
finally:
    os.close(directory)
PY
  _lc_report="${_lc_dir}/lifecycle-report.json"
  _lc_active=""
  _lc_finished=0
  _lc_reboot_reset
}

# Open the reboot stage with the harness's verdict on the boot ID: true
# when the ID recorded before the reboot differs from this boot's.
lifecycle_reboot_begin() {
  local changed="${1:-}"
  [[ -n "$_lc_report" ]] || _lc_die "lifecycle_reboot_begin before lifecycle_begin or lifecycle_resume"
  (( _lc_finished == 0 )) || _lc_die "lifecycle_reboot_begin after the run finished"
  [[ -z "$_lc_rb_started" ]] || _lc_die "the reboot stage has already begun"
  [[ -z "$_lc_active" ]] || _lc_die "lifecycle_reboot_begin during stage \`${_lc_active}\`"
  [[ "$changed" == true || "$changed" == false ]] ||
    _lc_die "the boot ID verdict is \`${changed}\`, not true or false"
  # The verdict first: a stop between the two leaves the stage unopened
  # rather than open with no verdict.
  _lc_rb_boot_changed="$changed"
  _lc_rb_started="$(_lc_now)"
  printf '== stage reboot\n' >&2
}

_lc_reboot_expect() {
  local name="$1"
  [[ -n "$_lc_rb_started" ]] || _lc_die "reboot sub-case \`${name}\` before lifecycle_reboot_begin"
  [[ -z "$_lc_rb_json" ]] || _lc_die "reboot sub-case \`${name}\` after lifecycle_reboot_finish"
  (( _lc_rb_next < ${#LIFECYCLE_REBOOT_SUB_CASES[@]} )) ||
    _lc_die "reboot sub-case \`${name}\` after all three"
  [[ "$name" == "${LIFECYCLE_REBOOT_SUB_CASES[$_lc_rb_next]}" ]] ||
    _lc_die "reboot sub-case \`${name}\` out of order; next is \`${LIFECYCLE_REBOOT_SUB_CASES[$_lc_rb_next]}\`"
}

_lc_reboot_set() {
  local name="$1" status="$2" log="$3" detail="${4:-}" entry
  entry="$(printf '"%s":{"status":"%s","log":"%s"' "$name" "$status" "$log")"
  if [[ -n "$detail" ]]; then
    entry="${entry},\"detail\":$(_lc_json_escape "$detail")"
  fi
  _lc_rb_subs+=("${entry}}")
}

# Run one sub-case, capturing its output and status as lifecycle_stage
# does, and return that status.
lifecycle_reboot_subcase() {
  local name="${1:-}"
  _lc_reboot_expect "$name"
  shift
  (( $# > 0 )) || _lc_die "reboot sub-case \`${name}\` has no command to run"
  local log="reboot-${name}.log" status=0
  printf '== reboot sub-case %s\n' "$name" >&2
  _lc_rb_active="$name"
  "$@" >"${_lc_dir}/${log}" 2>&1 || status=$?
  # Recorded before the sub-case stops being the active one, as
  # lifecycle_stage does: a signal in between then records it twice, and
  # the later record (a failure) is the one kept.
  if (( status != 0 )); then
    local detail="sub-case exited ${status}" tail_text
    if [[ -s "${_lc_dir}/${log}" ]]; then
      # Bounded: the stage is composed from command-line arguments, and a
      # log line of any length would otherwise exceed the argument limit
      # and leave the stage unrecordable. The whole log is filed anyway.
      tail_text="$(tail -n 3 "${_lc_dir}/${log}" | tr '\n' ' ')"
      (( ${#tail_text} <= 2000 )) || tail_text="...${tail_text: -2000}"
      detail="${detail}: ${tail_text}"
    fi
    _lc_reboot_set "$name" fail "$log" "$detail"
  else
    _lc_reboot_set "$name" pass "$log"
  fi
  _lc_rb_active=""
  _lc_rb_next=$((_lc_rb_next + 1))
  return "$status"
}

# A sub-case that did not run, with the reason.
lifecycle_reboot_skip() {
  local name="${1:-}" reason="${2:-}"
  _lc_reboot_expect "$name"
  [[ -n "$reason" ]] || _lc_die "a skipped sub-case needs a reason"
  : >"${_lc_dir}/reboot-${name}.log"
  _lc_reboot_set "$name" skipped "reboot-${name}.log" "$reason"
  _lc_rb_next=$((_lc_rb_next + 1))
}

# The agent's metadata retry window as the run observed it, in seconds.
lifecycle_reboot_retry_window() {
  local seconds="${1:-}"
  [[ -n "$_lc_rb_started" && -z "$_lc_rb_json" ]] ||
    _lc_die "lifecycle_reboot_retry_window outside the reboot stage"
  [[ "$seconds" =~ ^[0-9]{1,9}(\.[0-9]{1,9})?$ && ! "$seconds" =~ ^0+(\.0+)?$ ]] ||
    _lc_die "retry window \`${seconds}\` is not a positive number of seconds"
  _lc_rb_window="$seconds"
}

# Sub-cases not recorded become skipped with the reason given, then the
# stage's JSON is composed: pass only when the boot ID changed, all three
# sub-cases passed and the retry window was recorded. Which sub-cases are
# recorded is read from the records, not the counter: a stop just after a
# sub-case is recorded and before the counter moves must not record it
# again as not reached and empty its log. Returns non-zero, rather than
# exiting, when the stage cannot be composed: lifecycle_abort calls this
# from the EXIT trap and must return.
_lc_reboot_close() {
  local reason="$1" name line entry recorded
  for name in "${LIFECYCLE_REBOOT_SUB_CASES[@]}"; do
    recorded=0
    for entry in ${_lc_rb_subs[@]+"${_lc_rb_subs[@]}"}; do
      [[ "$entry" == "\"${name}\":{"* ]] && recorded=1
    done
    (( recorded )) && continue
    : >"${_lc_dir}/reboot-${name}.log"
    _lc_reboot_set "$name" skipped "reboot-${name}.log" "not reached: ${reason}"
  done
  _lc_rb_next=${#LIFECYCLE_REBOOT_SUB_CASES[@]}
  {
    printf 'boot ID changed: %s\n' "$_lc_rb_boot_changed"
    printf 'retry window observed: %s\n' "${_lc_rb_window:-not observed}"
    for line in "${_lc_rb_subs[@]}"; do
      printf '%s\n' "$line"
    done
  } >"${_lc_dir}/reboot.log"
  local json
  json="$(python3 - "$_lc_rb_started" "$(_lc_now)" "$_lc_rb_boot_changed" \
    "$_lc_rb_window" "${_lc_rb_subs[@]}" <<'PY'
import json, sys
started, finished, changed, window = sys.argv[1:5]
subs = json.loads("{" + ",".join(sys.argv[5:]) + "}")
failing = [name for name, sub in subs.items() if sub["status"] != "pass"]
reasons = ([] if changed == "true" else ["the boot ID did not change"]) + (
    ["sub-cases did not pass: " + ", ".join(failing)] if failing else []) + (
    [] if window else ["the retry window was not recorded"])
# Status first: the report writer reads it from the front of this JSON.
stage = {"status": "fail" if reasons else "pass", "started_at": started,
         "finished_at": finished, "log": "reboot.log"}
if reasons:
    stage["detail"] = "; ".join(reasons)
stage["boot_id_changed"] = changed == "true"
if window:
    stage["retry_window_seconds"] = float(window) if "." in window else int(window)
stage["sub_cases"] = subs
sys.stdout.write(json.dumps(stage, separators=(",", ":"), allow_nan=False))
PY
  )" || return 1
  _lc_rb_json="$json"
}

# The stage when it cannot be composed: failed, every sub-case marked as not
# recorded, and no log emptied. Built without python3, whose failure is the
# likeliest reason to be here.
_lc_reboot_unrecorded() {
  local name subs=""
  for name in "${LIFECYCLE_REBOOT_SUB_CASES[@]}"; do
    [[ -e "${_lc_dir}/reboot-${name}.log" ]] || : >"${_lc_dir}/reboot-${name}.log"
    subs="${subs:+${subs},}\"${name}\":{\"status\":\"skipped\",\"log\":\"reboot-${name}.log\",\"detail\":\"the reboot stage could not be recorded\"}"
  done
  [[ -e "${_lc_dir}/reboot.log" ]] || : >"${_lc_dir}/reboot.log"
  _lc_rb_json="$(printf '{"status":"fail","started_at":"%s","finished_at":"%s","log":"reboot.log","detail":"%s","boot_id_changed":%s,"sub_cases":{%s}}' \
    "$_lc_rb_started" "$(_lc_now)" "the reboot stage could not be recorded" \
    "${_lc_rb_boot_changed:-false}" "$subs")"
}

# Close the reboot stage.
lifecycle_reboot_finish() {
  [[ -n "$_lc_rb_started" ]] || _lc_die "lifecycle_reboot_finish before lifecycle_reboot_begin"
  [[ -z "$_lc_rb_json" ]] || _lc_die "the reboot stage has already finished"
  _lc_reboot_close "the harness finished the reboot stage without it" ||
    _lc_die "could not compose the reboot stage"
}
