#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Hardware-in-loop lifecycle rehearsal for the supported macOS row.
#
# THIS SCRIPT MUTATES HOMEBREW STATE. It temporarily replaces the managed
# TensorPlate formula files in an existing, clean tap checkout; installs and
# removes formulae; and starts/stops user launchd services. It restores the
# original tap files and the supplied CLI-only baseline formula before exit.

set -Eeuo pipefail

readonly FORMULAE=(
  tensorplate-agent
  tensorplate-serving
  tensorplate-cli
  tensorplate-observability
  tensorplate-backend-python-pytorch
  tensorplate
)
readonly COMPONENT_FORMULAE=(
  tensorplate-agent
  tensorplate-backend-python-pytorch
  tensorplate-cli
  tensorplate-observability
  tensorplate-serving
)

usage() {
  cat <<'EOF'
Usage:
  TP_HOMEBREW_LIFECYCLE_ALLOW=1 \
    tools/validation/macos-homebrew-lifecycle.sh \
      --candidate-formula-dir DIR \
      --baseline-formula FILE \
      --bundle-dir DIR \
      --evidence-dir DIR \
      [--preflight-only] \
      [--tap tensorplate/tap]

Required inputs:
  --candidate-formula-dir  Six rendered formulae pinned to one immutable
                           source archive and checksum.
  --baseline-formula       Historical CLI-only tensorplate.rb used to restore
                           the starting version after the rehearsal.
  --bundle-dir             Deploy-smoke fixture with manifest.json.
  --evidence-dir           New or empty directory for redacted run artifacts.
  --preflight-only         Validate the host and immutable formula pin without
                           changing Homebrew packages, tap files, or services.
  --tap                    Existing Homebrew tap to stage temporarily.

The script refuses to run unless the host is arm64, reports Apple M1 Pro with
16 GB memory, runs macOS 26 or newer, has the baseline tensorplate formula
installed, and the tap checkout is clean.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '==> %s\n' "$*"
}

pass() {
  printf 'PASS: %s\n' "$*"
}

formula_dir=""
baseline_formula=""
bundle_dir=""
evidence_dir=""
tap_name="tensorplate/tap"
preflight_only=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --candidate-formula-dir)
      formula_dir="${2:-}"
      shift 2
      ;;
    --baseline-formula)
      baseline_formula="${2:-}"
      shift 2
      ;;
    --bundle-dir)
      bundle_dir="${2:-}"
      shift 2
      ;;
    --evidence-dir)
      evidence_dir="${2:-}"
      shift 2
      ;;
    --tap)
      tap_name="${2:-}"
      shift 2
      ;;
    --preflight-only)
      preflight_only=1
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      die "unknown option '$1'"
      ;;
  esac
done

[[ "${TP_HOMEBREW_LIFECYCLE_ALLOW:-0}" == "1" ]] ||
  die "this rehearsal mutates Homebrew state; set TP_HOMEBREW_LIFECYCLE_ALLOW=1"
[[ -n "$formula_dir" && -d "$formula_dir" ]] ||
  die "--candidate-formula-dir must name a directory"
[[ -n "$baseline_formula" && -f "$baseline_formula" ]] ||
  die "--baseline-formula must name a file"
[[ -n "$bundle_dir" && -f "${bundle_dir}/manifest.json" ]] ||
  die "--bundle-dir must contain manifest.json"
[[ -n "$evidence_dir" ]] || die "--evidence-dir is required"
[[ ! -e "$evidence_dir" || -z "$(find "$evidence_dir" -mindepth 1 -print -quit)" ]] ||
  die "--evidence-dir must be new or empty"

for tool in brew git python3 stat sw_vers system_profiler launchctl sandbox-exec; do
  command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done

for formula_name in "${FORMULAE[@]}"; do
  [[ -f "${formula_dir}/${formula_name}.rb" ]] ||
    die "candidate formula missing: ${formula_name}.rb"
done

mkdir -p "$evidence_dir"
evidence_dir="$(cd "$evidence_dir" && pwd)"
work_dir="$(mktemp -d)"
stage_results="${evidence_dir}/stages.tsv"
printf 'stage\tstatus\tstarted_at\tfinished_at\tlog\n' >"$stage_results"

tap_repo=""
tap_backup="${work_dir}/tap-backup"
tap_staged=0
baseline_version=""
candidate_version=""
candidate_active=0
agent_config_backup=""
trust_added=()
active_stage=""
active_stage_log=""
active_stage_started=""

restore_tap() {
  [[ "$tap_staged" == "1" ]] || return 0
  for formula_name in "${FORMULAE[@]}"; do
    rm -f "${tap_repo}/Formula/${formula_name}.rb"
    if [[ -f "${tap_backup}/${formula_name}.rb" ]]; then
      cp "${tap_backup}/${formula_name}.rb" "${tap_repo}/Formula/${formula_name}.rb"
    fi
  done
  tap_staged=0
}

stop_candidate_services() {
  brew services stop tensorplate-agent >/dev/null 2>&1 || true
  brew services stop tensorplate-observability >/dev/null 2>&1 || true
}

restore_agent_config() {
  [[ -n "$agent_config_backup" && -f "$agent_config_backup" ]] || return 0
  cp "$agent_config_backup" "$(brew --prefix)/etc/tensorplate/agent.json"
  chmod 0640 "$(brew --prefix)/etc/tensorplate/agent.json"
  agent_config_backup=""
}

restore_formula_trust() {
  [[ "${#trust_added[@]}" -gt 0 ]] || return 0
  brew untrust --formula "${trust_added[@]}" >/dev/null 2>&1 || true
  trust_added=()
}

formula_is_installed() {
  brew list --formula --versions "$1" >/dev/null 2>&1
}

linked_formula_version() {
  brew info --json=v2 "$1" 2>/dev/null |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["formulae"][0]["linked_keg"] or "")'
}

remove_candidate_graph() {
  stop_candidate_services
  if formula_is_installed tensorplate; then
    brew uninstall --formula tensorplate >/dev/null
  fi
  for formula_name in "${COMPONENT_FORMULAE[@]}"; do
    if formula_is_installed "$formula_name"; then
      brew uninstall --formula "$formula_name" >/dev/null
    fi
  done
  candidate_active=0
}

restore_baseline() {
  restore_agent_config
  remove_candidate_graph
  restore_tap
  cp "$baseline_formula" "${tap_repo}/Formula/tensorplate.rb"
  HOMEBREW_NO_AUTO_UPDATE=1 brew install --formula "${tap_name}/tensorplate" >/dev/null
  brew link --overwrite "${tap_name}/tensorplate" >/dev/null
  rm -f "${tap_repo}/Formula/tensorplate.rb"
  if [[ -f "${tap_backup}/tensorplate.rb" ]]; then
    cp "${tap_backup}/tensorplate.rb" "${tap_repo}/Formula/tensorplate.rb"
  fi
}

cleanup() {
  status=$?
  set +e
  if [[ "$status" -ne 0 && -n "$active_stage" ]]; then
    finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '%s\tfail\t%s\t%s\t%s\n' \
      "$active_stage" "$active_stage_started" "$finished_at" \
      "$(basename "$active_stage_log")" >>"$stage_results"
    tail -n 40 "$active_stage_log" >&2 || true
    printf 'error: stage %s failed with exit %s\n' "$active_stage" "$status" >&2
  fi
  if [[ -n "$tap_repo" && -d "$tap_repo" && -n "$baseline_version" ]]; then
    current_version="$(linked_formula_version tensorplate)"
    if [[ "$candidate_active" == "1" || "$current_version" != "$baseline_version" ]]; then
      note "restoring baseline tensorplate ${baseline_version}"
      restore_baseline
    else
      restore_agent_config
      restore_tap
    fi
  else
    restore_agent_config
    restore_tap
  fi
  restore_formula_trust
  rm -rf "$work_dir"
  exit "$status"
}
trap cleanup EXIT

run_stage() {
  active_stage="$1"
  shift
  active_stage_log="${evidence_dir}/${active_stage}.log"
  active_stage_started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  note "stage ${active_stage}"
  "$@" >"$active_stage_log" 2>&1
  finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '%s\tpass\t%s\t%s\t%s\n' \
    "$active_stage" "$active_stage_started" "$finished_at" \
    "$(basename "$active_stage_log")" >>"$stage_results"
  pass "$active_stage"
  active_stage=""
  active_stage_log=""
  active_stage_started=""
}

stage_candidate_tap() {
  mkdir -p "$tap_backup"
  for formula_name in "${FORMULAE[@]}"; do
    if [[ -f "${tap_repo}/Formula/${formula_name}.rb" &&
      ! -f "${tap_backup}/${formula_name}.rb" ]]; then
      cp "${tap_repo}/Formula/${formula_name}.rb" "${tap_backup}/${formula_name}.rb"
    fi
    cp "${formula_dir}/${formula_name}.rb" "${tap_repo}/Formula/${formula_name}.rb"
  done
  tap_staged=1
}

wait_for_service() {
  service_name="$1"
  attempts="${2:-30}"
  for ((attempt = 1; attempt <= attempts; attempt += 1)); do
    if brew services list | awk -v name="$service_name" \
      '$1 == name && $2 == "started" {found = 1} END {exit !found}'; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_for_agent_ready() {
  attempts="${1:-30}"
  for ((attempt = 1; attempt <= attempts; attempt += 1)); do
    if tensorplate status --output json >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

collect_host_facts() {
  chip="$(system_profiler SPHardwareDataType | awk -F': ' '/^[[:space:]]*Chip:/{print $2; exit}')"
  memory="$(system_profiler SPHardwareDataType | awk -F': ' '/^[[:space:]]*Memory:/{print $2; exit}')"
  model="$(system_profiler SPHardwareDataType | awk -F': ' '/^[[:space:]]*Model Name:/{print $2; exit}')"
  os_version="$(sw_vers -productVersion)"
  os_build="$(sw_vers -buildVersion)"
  architecture="$(uname -m)"
  brew_version="$(brew --version | head -n 1 | awk '{print $2}')"

  [[ "$architecture" == "arm64" ]] || die "expected arm64, found ${architecture}"
  [[ "$chip" == "Apple M1 Pro" ]] || die "expected Apple M1 Pro, found ${chip:-unknown}"
  [[ "$memory" == "16 GB" ]] || die "expected 16 GB memory, found ${memory:-unknown}"
  os_major="${os_version%%.*}"
  [[ "$os_major" =~ ^[0-9]+$ && "$os_major" -ge 26 ]] ||
    die "expected macOS 26 or newer, found ${os_version}"

  python3 - \
    "$architecture" "$model" "$chip" "$memory" "$os_version" "$os_build" "$brew_version" \
    >"${evidence_dir}/host-facts.json" <<'PY'
import json
import sys

architecture, model, chip, memory, os_version, os_build, brew_version = sys.argv[1:]
print(json.dumps({
    "architecture": architecture,
    "model": model,
    "chip": chip,
    "memory": memory,
    "macos_version": os_version,
    "macos_build": os_build,
    "homebrew_version": brew_version,
}, indent=2, sort_keys=True))
PY
}

capture_formula_pin() {
  python3 - "$formula_dir" >"${evidence_dir}/formula-pin.json" <<'PY'
import json
import pathlib
import re
import sys

formula_dir = pathlib.Path(sys.argv[1])
records = {}
for formula in sorted(formula_dir.glob("*.rb")):
    text = formula.read_text(encoding="utf-8")
    url = re.search(r'^\s*url "([^"]+)"$', text, re.MULTILINE)
    sha = re.search(r'^\s*sha256 "([0-9a-f]{64})"$', text, re.MULTILINE)
    version = re.search(r'^\s*version "([^"]+)"$', text, re.MULTILINE)
    if not url or not sha:
        raise SystemExit(f"{formula.name}: missing source URL or checksum")
    records[formula.stem] = {
        "url": url.group(1),
        "sha256": sha.group(1),
        "declared_version": version.group(1) if version else None,
    }

pins = {(item["url"], item["sha256"], item["declared_version"]) for item in records.values()}
if len(pins) != 1:
    raise SystemExit("candidate formula graph does not share one source pin")
url, sha256, declared_version = next(iter(pins))
if sha256 == "0" * 64 or "v0.0.0.tar.gz" in url:
    raise SystemExit("candidate formula graph still contains placeholder release data")
print(json.dumps({
    "source_url": url,
    "source_sha256": sha256,
    "declared_version": declared_version,
    "formulae": records,
}, indent=2, sort_keys=True))
PY
}

verify_tap_trust() {
  trust_json="$(brew trust --json=v1)"
  python3 - "$tap_name" "$trust_json" "${FORMULAE[@]}" <<'PY'
import json
import sys

tap_name = sys.argv[1].lower()
trusted = json.loads(sys.argv[2])
formula_names = sys.argv[3:]
trusted_taps = {item.lower() for item in trusted.get("taps", [])}
trusted_formulae = {item.lower() for item in trusted.get("formulae", [])}
missing = [
    f"{tap_name}/{name}"
    for name in formula_names
    if tap_name not in trusted_taps and f"{tap_name}/{name}" not in trusted_formulae
]
if missing:
    joined = " ".join(missing)
    raise SystemExit(f"formula trust is missing; run `brew trust --formula {joined}`")
PY
}

ensure_candidate_formula_trust() {
  trust_json="$(brew trust --json=v1)"
  missing_trust_file="${work_dir}/missing-formula-trust"
  python3 - "$tap_name" "$trust_json" "${FORMULAE[@]}" \
    >"$missing_trust_file" <<'PY'
import json
import sys

tap_name = sys.argv[1].lower()
trusted = json.loads(sys.argv[2])
formula_names = sys.argv[3:]
trusted_taps = {item.lower() for item in trusted.get("taps", [])}
trusted_formulae = {item.lower() for item in trusted.get("formulae", [])}
for name in formula_names:
    full_name = f"{tap_name}/{name}"
    if tap_name not in trusted_taps and full_name not in trusted_formulae:
        print(full_name)
PY
  missing_formulae=()
  while IFS= read -r formula_name; do
    [[ -n "$formula_name" ]] && missing_formulae+=("$formula_name")
  done <"$missing_trust_file"
  [[ "${#missing_formulae[@]}" -gt 0 ]] || return 0
  brew trust --formula "${missing_formulae[@]}"
  trust_added+=("${missing_formulae[@]}")
}

record_formula_graph() {
  brew deps --tree "${tap_name}/tensorplate"
  for formula_name in "${FORMULAE[@]}"; do
    brew info --json=v2 "${tap_name}/${formula_name}" |
      python3 -c 'import json,sys; f=json.load(sys.stdin)["formulae"][0]; print(f["name"], f["versions"]["stable"])'
  done
}

install_candidate_clean() {
  stage_candidate_tap
  candidate_version="$(
    brew info --json=v2 "${tap_name}/tensorplate" |
      python3 -c 'import json,sys; print(json.load(sys.stdin)["formulae"][0]["versions"]["stable"])'
  )"
  record_formula_graph
  if formula_is_installed tensorplate; then
    brew uninstall --formula tensorplate
  fi
  HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 \
    brew install --formula "${tap_name}/tensorplate"
  candidate_active=1
  installed="$(linked_formula_version tensorplate)"
  [[ "$installed" == "$candidate_version" ]] ||
    die "candidate install produced ${installed:-missing}; expected ${candidate_version}"
}

verify_packaged_closure() {
  prefix="$(brew --prefix)"
  for formula_name in "${FORMULAE[@]}"; do
    formula_is_installed "$formula_name" ||
      die "formula is not installed: ${formula_name}"
  done
  [[ "$(command -v tensorplate)" == "${prefix}/bin/tensorplate" ]]
  [[ -x "$(brew --prefix tensorplate-agent)/bin/tensorplate-agent" ]]
  [[ -x "$(brew --prefix tensorplate-serving)/libexec/tensorplate-serving" ]]
  [[ -x "$(brew --prefix tensorplate-observability)/bin/tensorplate-observability" ]]
  [[ -x "$(brew --prefix tensorplate-backend-python-pytorch)/bin/tensorplate-backend-python-pytorch" ]]
  [[ -f "${prefix}/share/tensorplate/platform/rows/macos26-m1pro-16gb.json" ]]
  [[ -f "${prefix}/share/tensorplate/backends/python_pytorch/backend.json" ]]

  for directory in \
    "${prefix}/etc/tensorplate" \
    "${prefix}/var/tensorplate" \
    "${prefix}/var/tensorplate/state" \
    "${prefix}/var/run/tensorplate" \
    "${prefix}/var/log/tensorplate"; do
    [[ "$(stat -f '%Lp' "$directory")" == "750" ]] ||
      die "unexpected mode for ${directory}"
  done
  [[ "$(stat -f '%Lp' "${prefix}/etc/tensorplate/agent.json")" == "640" ]]
  [[ "$(stat -f '%Lp' "${prefix}/etc/tensorplate/observability.json")" == "640" ]]
  [[ "$(stat -f '%Lp' "${prefix}/etc/tensorplate/cli.json")" == "644" ]]
}

start_services() {
  brew services start tensorplate-agent
  brew services start tensorplate-observability
  wait_for_service tensorplate-agent
  wait_for_service tensorplate-observability
  wait_for_agent_ready
  [[ "$(stat -f '%Lp' "$(brew --prefix)/var/run/tensorplate/agent.sock")" == "660" ]]
  if brew services list | awk '$1 == "tensorplate-serving" {found = 1} END {exit !found}'; then
    die "tensorplate-serving unexpectedly exposes a Homebrew service"
  fi
  brew services list
  launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-agent"
  launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-observability"
}

probe_mps() {
  pytorch_python="$(brew --prefix pytorch)/libexec/bin/python"
  backend_libexec="$(brew --prefix tensorplate-backend-python-pytorch)/libexec"
  [[ -x "$pytorch_python" ]] || die "PyTorch formula interpreter is missing"
  PYTHONPATH="$backend_libexec" "$pytorch_python" - <<'PY'
import json
import platform
import torch
from tensorplate_pytorch_backend.accelerator import probe_mps_runtime

capability = probe_mps_runtime(torch, accelerator_runtime_version=platform.mac_ver()[0])
payload = capability.to_wire()
payload["python_version"] = platform.python_version()
if not payload["accelerator_runtime_built"] or not payload["accelerator_runtime_available"]:
    raise SystemExit(json.dumps(payload, sort_keys=True))
print(json.dumps(payload, indent=2, sort_keys=True))
PY
}

deploy_smoke() {
  smoke_bundle="${work_dir}/deploy-smoke"
  cp -R "$bundle_dir" "$smoke_bundle"
  cd /private/tmp
  tensorplate doctor --output json
  tensorplate deploy "$smoke_bundle" \
    --deployment-id wave-2b-macos-deploy-smoke \
    --output json
  tensorplate status --output json
  tensorplate logs --component agent --tail 100
  cd - >/dev/null
}

restart_services() {
  before_agent="$(
    launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-agent" |
      awk '/^[[:space:]]*pid = / {print $3; exit}'
  )"
  before_observability="$(
    launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-observability" |
      awk '/^[[:space:]]*pid = / {print $3; exit}'
  )"
  brew services restart tensorplate-agent
  brew services restart tensorplate-observability
  wait_for_service tensorplate-agent
  wait_for_service tensorplate-observability
  wait_for_agent_ready
  after_agent="$(
    launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-agent" |
      awk '/^[[:space:]]*pid = / {print $3; exit}'
  )"
  after_observability="$(
    launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-observability" |
      awk '/^[[:space:]]*pid = / {print $3; exit}'
  )"
  [[ -n "$before_agent" && -n "$after_agent" && "$before_agent" != "$after_agent" ]]
  [[ -n "$before_observability" && -n "$after_observability" &&
    "$before_observability" != "$after_observability" ]]
  printf 'agent %s -> %s\nobservability %s -> %s\n' \
    "$before_agent" "$after_agent" "$before_observability" "$after_observability"
}

exercise_crash_loop() {
  agent_config="$(brew --prefix)/etc/tensorplate/agent.json"
  agent_config_backup="${work_dir}/agent.json"
  cp "$agent_config" "$agent_config_backup"
  printf '{ invalid json\n' >"$agent_config"
  chmod 0640 "$agent_config"
  brew services restart tensorplate-agent >/dev/null 2>&1 || true
  sleep 12
  launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-agent"
  grep -q "config" "$(brew --prefix)/var/log/tensorplate/agent.error.log"
  cp "$agent_config_backup" "$agent_config"
  chmod 0640 "$agent_config"
  agent_config_backup=""
  brew services restart tensorplate-agent
  wait_for_service tensorplate-agent
  wait_for_agent_ready
  tensorplate status --output json
}

verify_offline_runtime() {
  profile='(version 1)(allow default)(deny network*)'
  sandbox-exec -p "$profile" tensorplate doctor --output json
  sandbox-exec -p "$profile" \
    "$(brew --prefix pytorch)/libexec/bin/python" - <<'PY'
import json
import torch

result = {
    "mps_built": torch.backends.mps.is_built(),
    "mps_available": torch.backends.mps.is_available(),
}
if not all(result.values()):
    raise SystemExit(json.dumps(result, sort_keys=True))
print(json.dumps(result, sort_keys=True))
PY
}

uninstall_candidate() {
  remove_candidate_graph
  for formula_name in "${FORMULAE[@]}"; do
    if formula_is_installed "$formula_name"; then
      die "formula remains installed after uninstall: ${formula_name}"
    fi
  done
  [[ ! -e "$HOME/Library/LaunchAgents/homebrew.mxcl.tensorplate-agent.plist" ]]
  [[ ! -e "$HOME/Library/LaunchAgents/homebrew.mxcl.tensorplate-observability.plist" ]]
  [[ ! -e "$(brew --prefix)/bin/tensorplate" ]]
}

install_baseline() {
  restore_tap
  cp "$baseline_formula" "${tap_repo}/Formula/tensorplate.rb"
  HOMEBREW_NO_AUTO_UPDATE=1 brew install --formula "${tap_name}/tensorplate"
  brew link --overwrite "${tap_name}/tensorplate"
  rm -f "${tap_repo}/Formula/tensorplate.rb"
  if [[ -f "${tap_backup}/tensorplate.rb" ]]; then
    cp "${tap_backup}/tensorplate.rb" "${tap_repo}/Formula/tensorplate.rb"
  fi
  installed="$(linked_formula_version tensorplate)"
  [[ "$installed" == "$baseline_version" ]] ||
    die "baseline restore produced ${installed:-missing}; expected ${baseline_version}"
  tensorplate version
}

upgrade_from_baseline() {
  ensure_candidate_formula_trust
  stage_candidate_tap
  HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 \
    brew upgrade --formula "${tap_name}/tensorplate"
  candidate_active=1
  installed="$(linked_formula_version tensorplate)"
  [[ "$installed" == "$candidate_version" ]] ||
    die "upgrade produced ${installed:-missing}; expected ${candidate_version}"
  verify_packaged_closure
}

rollback_to_baseline() {
  marker="$(brew --prefix)/var/tensorplate/state/lifecycle-marker"
  printf 'preserve-across-formula-rollback\n' >"$marker"
  remove_candidate_graph
  install_baseline
  [[ "$(cat "$marker")" == "preserve-across-formula-rollback" ]]
  [[ "$(linked_formula_version tensorplate)" == "$baseline_version" ]]
}

write_summary() {
  python3 - "$stage_results" "$baseline_version" "$candidate_version" \
    >"${evidence_dir}/summary.json" <<'PY'
import csv
import json
import sys

stages_path, baseline_version, candidate_version = sys.argv[1:]
with open(stages_path, encoding="utf-8", newline="") as handle:
    stages = list(csv.DictReader(handle, delimiter="\t"))
print(json.dumps({
    "result": "pass" if stages and all(item["status"] == "pass" for item in stages) else "fail",
    "baseline_version": baseline_version,
    "candidate_version": candidate_version,
    "stages": stages,
}, indent=2, sort_keys=True))
PY
}

export HOMEBREW_NO_AUTO_UPDATE=1
export HOMEBREW_NO_INSTALL_CLEANUP=1
export HOMEBREW_NO_AUTOREMOVE=1

tap_repo="$(brew --repository "$tap_name")"
[[ -d "${tap_repo}/.git" ]] || die "tap repository not found: ${tap_repo}"
[[ -z "$(git -C "$tap_repo" status --porcelain)" ]] ||
  die "tap checkout must be clean: ${tap_repo}"
mkdir -p "$tap_backup"
for formula_name in "${FORMULAE[@]}"; do
  if [[ -f "${tap_repo}/Formula/${formula_name}.rb" ]]; then
    cp "${tap_repo}/Formula/${formula_name}.rb" "${tap_backup}/${formula_name}.rb"
  fi
done

baseline_version="$(linked_formula_version tensorplate)"
[[ -n "$baseline_version" ]] || die "the CLI-only tensorplate baseline must be installed"

run_stage host-facts collect_host_facts
run_stage formula-pin capture_formula_pin
run_stage baseline tensorplate version
run_stage tap-trust verify_tap_trust
if [[ "$preflight_only" == "1" ]]; then
  write_summary
  trap - EXIT
  rm -rf "$work_dir"
  pass "macOS Homebrew lifecycle preflight complete; evidence: ${evidence_dir}"
  exit 0
fi
run_stage clean-install install_candidate_clean
run_stage packaged-closure verify_packaged_closure
run_stage launchd-start start_services
run_stage mps-capability probe_mps
run_stage deploy-smoke deploy_smoke
run_stage launchd-restart restart_services
run_stage launchd-crash-loop exercise_crash_loop
run_stage offline-runtime verify_offline_runtime
run_stage uninstall uninstall_candidate
run_stage baseline-restore install_baseline
run_stage upgrade upgrade_from_baseline
run_stage rollback rollback_to_baseline
# shellcheck disable=SC2016 # The expression is evaluated by bash -c.
run_stage tap-restored bash -c '[[ -z "$(git -C "$1" status --porcelain)" ]]' _ "$tap_repo"
restore_formula_trust
write_summary

trap - EXIT
rm -rf "$work_dir"
pass "macOS Homebrew lifecycle rehearsal complete; evidence: ${evidence_dir}"
