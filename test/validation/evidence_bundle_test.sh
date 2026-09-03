#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The evidence completeness check.
#
# The guard this replaces passed for a directory nobody had created, so
# the property that matters is not "it runs" but "it fails when the
# evidence is not there, and passes when it is". Both directions are
# exercised against staged registries, because the real one currently
# has no complete row to prove the passing direction with.

set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
checker="${repo_root}/tools/release/check-evidence-bundles.sh"
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

# A registry with one Production row, plus whatever evidence the caller
# stages beneath it.
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

write_report() {
  local dir="$1" row="$2" outcome="$3"
  shift 3
  mkdir -p "${dir}/evidence/synthetic-row"
  local stages="" stage
  for stage in install upgrade deploy-smoke status-logs rollback restart crash-loop offline; do
    local status="pass"
    for skip in "$@"; do [[ "$stage" == "$skip" ]] && status="skipped"; done
    [[ -n "$stages" ]] && stages="${stages},"
    stages="${stages}{\"stage\":\"${stage}\",\"status\":\"${status}\",\"log\":\"${stage}.log\"}"
  done
  cat >"${dir}/evidence/synthetic-row/lifecycle-report.json" <<JSON
{"schema_version":"0.1","row_id":"${row}","outcome":"${outcome}","stages":[${stages}]}
JSON
}

run_checker() {
  local dir="$1"
  set +e
  "$checker" --registry "${dir}/registry" --root "$dir" >"${dir}/out.txt" 2>&1
  local status=$?
  set -e
  printf '%s' "$status"
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
printf 'evidence completeness check\n'

# A row nobody has observed.
d="${work}/unrecorded"; stage_registry "$d" spec_authored
check "spec_authored row fails" "1" "$(run_checker "$d")"
check "  and says why" "yes" "$(grep -q 'not .recorded.' "${d}/out.txt" && echo yes || echo no)"

# Recorded, but no evidence on disk.
d="${work}/nodir"; stage_registry "$d" recorded
check "missing evidence directory fails" "1" "$(run_checker "$d")"

# Directory exists, no report.
d="${work}/noreport"; stage_registry "$d" recorded; mkdir -p "${d}/evidence/synthetic-row"
check "missing lifecycle report fails" "1" "$(run_checker "$d")"

# A complete bundle. The passing direction the real registry cannot show.
d="${work}/complete"; stage_registry "$d" recorded; write_report "$d" synthetic-row pass
check "a complete bundle passes" "0" "$(run_checker "$d")"

# A report whose stages did not all pass.
d="${work}/skipped"; stage_registry "$d" recorded; write_report "$d" synthetic-row pass offline
check "a skipped stage fails completeness" "1" "$(run_checker "$d")"
check "  and names the stage" "yes" "$(grep -q 'offline=skipped' "${d}/out.txt" && echo yes || echo no)"

# Evidence filed under the wrong row proves nothing about this one.
d="${work}/wrongrow"; stage_registry "$d" recorded; write_report "$d" some-other-row pass
check "a report for another row fails" "1" "$(run_checker "$d")"
check "  and says whose it is" "yes" "$(grep -q 'some-other-row' "${d}/out.txt" && echo yes || echo no)"

# A registry with no Production rows means the check did nothing, which
# must not read as success.
d="${work}/empty"; mkdir -p "${d}/registry/rows"
check "an empty registry is an error, not a pass" "1" "$(run_checker "$d")"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "all checks passed" || echo "${failures} check(s) failed")"
exit "$failures"
