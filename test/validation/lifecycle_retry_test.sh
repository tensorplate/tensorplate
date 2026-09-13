#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Retrying in an evidence directory must not attest the previous attempt's
# artifact. Exercise the real Jetson control flow without loading its
# package-management or hardware-validation function bodies.

set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
runner="${repo_root}/tools/validation/lifecycle-stages.sh"
adapter="${repo_root}/tools/validation/jetson-stages-from-evidence.sh"
converter="${repo_root}/tools/validation/lifecycle-report-from-stages.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
unset TP_LIFECYCLE_SOURCE_REVISION
failures=0

check() {
  if [[ "$2" == "$3" ]]; then
    printf '  ok   %s\n' "$1"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$1" "$2" "$3"
    failures=$((failures + 1))
  fi
}

digest_in_report() {
  python3 -c 'import json,sys
print(json.load(open(sys.argv[1]))["subject"].get("artifact_digest", "absent"))' "$1"
}

# The production Jetson path uses sha256sum. On macOS, use the equivalent
# real SHA-256 tool if that command is not installed.
if ! command -v sha256sum >/dev/null 2>&1; then
  sha256sum() { shasum -a 256 "$@"; }
  export -f sha256sum
fi

python3 - "${repo_root}/tools/validation/jetson-clean-room.sh" "${work}/jetson-functions.sh" <<'PY'
import re, sys
from pathlib import Path

names = {
    "die", "note", "normalize_version", "parse_args", "require_confirm",
    "prepare_paths", "clear_artifact_digest", "capture", "required",
    "verify_assets", "cmd_run", "cmd_reset", "cmd_download",
}
source = Path(sys.argv[1]).read_text()
functions = re.findall(r"^([a-z_]+)\(\) \{\n.*?^\}", source, re.M | re.S)
if not names.issubset(functions):
    sys.exit(f"missing testable Jetson functions: {sorted(names - set(functions))}")
selected = [
    match.group(0)
    for match in re.finditer(r"^([a-z_]+)\(\) \{\n.*?^\}", source, re.M | re.S)
    if match.group(1) in names
]
Path(sys.argv[2]).write_text("\n\n".join(selected) + "\n")
PY

assets="${work}/assets"
evidence="${work}/evidence"
mkdir -p "$assets" "$evidence"
printf 'keep doctor recording\n' >"${evidence}/doctor-fixture.json"

# These globals and stubs are consumed by the extracted production functions.
# shellcheck disable=SC2034,SC2329
jetson_attempt() {
  local host_status="${1:-0}" confirmation="${2:-RESET-TENSORPLATE}" command="${3:-cmd_run}"
  set +e
  (
    set -Eeuo pipefail
    # Only allowlisted functions are sourced; hardware and package functions
    # are absent, so an unstubbed new call fails rather than mutating the host.
    # shellcheck disable=SC1091
    source "${work}/jetson-functions.sh"
    check_host() { return "$host_status"; }
    record_environment() { :; }
    reset_tensorplate() { :; }
    install_tensorplate() { :; }
    validate_runtime() { :; }
    write_summary() { :; }
    archive_evidence() { :; }
    download_release_assets() { die "unexpected download in fixture run"; }
    VERSION=0.2.1 TAG="" CONFIRM_TOKEN=RESET-TENSORPLATE CONFIRM_VALUE=""
    WORK_DIR="${work}/jetson-work" RUN_TMP_DIR=""
    ASSETS_DIR="$assets" EVIDENCE_DIR="$evidence"
    "$command" --confirm "$confirmation"
  ) >"${work}/attempt.log" 2>&1
  jetson_status=$?
  set -e
}

convert_jetson() {
  "$adapter" "$evidence" checksums >"${evidence}/stages.tsv"
  "$converter" "${evidence}/stages.tsv" jetson-orin-nano-8gb-jp62 0.2.1 \
    retry-test "${evidence}/lifecycle-report.json" checksums=install >/dev/null 2>&1
}

printf 'lifecycle evidence retries\n'
printf 'artifact A\n' >"${assets}/install.sh"
(cd "$assets" && sha256sum install.sh >SHA256SUMS)
digest_a="$(sha256sum "${assets}/SHA256SUMS" | awk '{print $1}')"
jetson_attempt
check "the first Jetson attempt succeeds" 0 "$jetson_status"
convert_jetson
check "  and its report identifies artifact A" "$digest_a" \
  "$(digest_in_report "${evidence}/lifecycle-report.json")"

printf 'artifact B\n' >"${assets}/install.sh"
jetson_attempt
check "a retry with failed checksums exits non-zero" 1 "$jetson_status"
check "  and removes the previous artifact sidecar" no \
  "$([[ -e "${evidence}/artifact-digest.txt" ]] && echo yes || echo no)"
convert_jetson
check "  and conversion cannot resurrect artifact A" absent \
  "$(digest_in_report "${evidence}/lifecycle-report.json")"
check "  and the checksum failure survives conversion" fail \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' \
    "${evidence}/lifecycle-report.json")"

(cd "$assets" && sha256sum install.sh >SHA256SUMS)
digest_b="$(sha256sum "${assets}/SHA256SUMS" | awk '{print $1}')"
check "the success control uses a different artifact set" yes \
  "$([[ "$digest_a" != "$digest_b" ]] && echo yes || echo no)"
jetson_attempt
check "a subsequent verified retry succeeds" 0 "$jetson_status"
convert_jetson
check "  and replaces the digest with artifact B" "$digest_b" \
  "$(digest_in_report "${evidence}/lifecycle-report.json")"

for command in cmd_run cmd_reset; do
  jetson_attempt 0 not-confirmed "$command"
  check "an unconfirmed ${command} refuses before beginning an attempt" 1 "$jetson_status"
  check "  and preserves the prior artifact sidecar" "$digest_b" \
    "$(awk '{print $1}' "${evidence}/artifact-digest.txt")"
done

# The download command re-fetches into the same directory, so it carries
# the same hazard: a fetch that dies partway must not leave the previous
# attempt's digest behind for the converter to pick up.
jetson_attempt 0 RESET-TENSORPLATE cmd_download
check "a failed download exits non-zero" 1 "$jetson_status"
check "  and clears the previous artifact sidecar" no \
  "$([[ -e "${evidence}/artifact-digest.txt" ]] && echo yes || echo no)"

(cd "$assets" && sha256sum install.sh >SHA256SUMS)
jetson_attempt
check "a verified run after the failed download succeeds" 0 "$jetson_status"

jetson_attempt 42
check "an early host-check failure keeps its exit status" 42 "$jetson_status"
check "  and cannot leave artifact B attached to the new attempt" no \
  "$([[ -e "${evidence}/artifact-digest.txt" ]] && echo yes || echo no)"
check "retry cleanup preserves the doctor recording" "keep doctor recording" \
  "$(cat "${evidence}/doctor-fixture.json")"

# The shared runner must reset the disk sidecar as well as its in-memory
# subject. Otherwise a later conversion can bring an old digest back.
shared="${work}/shared"
(
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$runner"
  lifecycle_begin r "$shared" 0.2.1 retry-test
  lifecycle_artifact_digest "$digest_a" SHA256SUMS
  lifecycle_stage install true
  lifecycle_finish
) >/dev/null 2>&1
check "the shared runner's first attempt carries its digest" "$digest_a" \
  "$(digest_in_report "${shared}/lifecycle-report.json")"
printf 'keep shared recording\n' >"${shared}/doctor-fixture.json"
(
  # shellcheck source=tools/validation/lifecycle-stages.sh
  source "$runner"
  lifecycle_begin r "$shared" 0.2.1 retry-test
  lifecycle_stage install true
  lifecycle_finish
) >/dev/null 2>&1
check "a shared-runner retry removes the old sidecar" no \
  "$([[ -e "${shared}/artifact-digest.txt" ]] && echo yes || echo no)"
check "  and its report omits the previous digest" absent \
  "$(digest_in_report "${shared}/lifecycle-report.json")"
python3 - "$shared" <<'PY'
import json, sys
from pathlib import Path
directory = Path(sys.argv[1])
report = json.loads((directory / "lifecycle-report.json").read_text())
stage = next(s for s in report["stages"] if s["stage"] == "install")
columns = ["stage", "status", "started_at", "finished_at", "log"]
(directory / "stages.tsv").write_text(
    "\t".join(columns) + "\n" + "\t".join(stage[c] for c in columns) + "\n"
)
PY
"$converter" "${shared}/stages.tsv" r 0.2.1 retry-test \
  "${shared}/converted.json" install=install >/dev/null 2>&1
check "  and conversion also omits the previous digest" absent \
  "$(digest_in_report "${shared}/converted.json")"
check "shared-runner cleanup preserves the doctor recording" "keep shared recording" \
  "$(cat "${shared}/doctor-fixture.json")"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "all retry checks passed" || echo "${failures} retry check(s) failed")"
exit "$failures"
