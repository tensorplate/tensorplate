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
  --bundle-dir             MPS deploy-smoke fixture with manifest.json.
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

for tool in brew git python3 stat sw_vers system_profiler launchctl sandbox-exec lsof pgrep ps; do
  command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done
# The offline stages keep their profile, probe and classification in a
# module beside this script, so they can be tested without a Mac.
offline_helper_path="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/macos_offline_runtime.py" ||
  die "cannot locate the harness directory"
[[ -f "$offline_helper_path" ]] || die "offline-runtime helper not found: ${offline_helper_path}"
# The interpreter itself rather than a version-manager shim, because the
# network probe runs it under the offline profile.
python_bin="$(python3 -c 'import sys; print(sys.executable)')" ||
  die "cannot resolve the python3 interpreter"

for formula_name in "${FORMULAE[@]}"; do
  [[ -f "${formula_dir}/${formula_name}.rb" ]] ||
    die "candidate formula missing: ${formula_name}.rb"
done

mkdir -p "$evidence_dir"
evidence_dir="$(cd "$evidence_dir" && pwd)"
work_dir="$(mktemp -d)"
stage_results="${evidence_dir}/stages.tsv"
smoke_deployment_id="wave-2b-macos-deploy-smoke-$(date -u +%Y%m%dT%H%M%SZ)"
offline_deployment_id="macos-offline-deploy-$(date -u +%Y%m%dT%H%M%SZ)"
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
lifecycle_marker=""
agent_error_log_start=0
observability_error_log_start=0
events_log_start=0
# Set just before the offline stage stops the normal launchd jobs and
# cleared once they run again, so cleanup restarts them only when this
# run is what stopped them.
offline_services_stopped=0
denial_dir=""
offline_profile=""

offline_helper() {
  python3 "$offline_helper_path" "$@"
}

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

# Boot out one service's launchd job if, and only if, it runs under
# sandbox-exec: a normal job is left alone. Called from cleanup, so it
# returns a status instead of calling die.
purge_offline_job() {
  purge_target="gui/$(id -u)/homebrew.mxcl.$1"
  purge_status=0
  purge_print="$(launchctl print "$purge_target" 2>/dev/null)" || purge_status=$?
  # launchctl print exits 113 for a label that is not loaded.
  [[ "$purge_status" -ne 113 ]] || return 0
  if [[ "$purge_status" -ne 0 ]]; then
    printf 'error: launchctl print %s failed with status %s\n' "$purge_target" "$purge_status" >&2
    return 1
  fi
  purge_program="$(printf '%s\n' "$purge_print" | offline_helper launchd-job --print - --field program)" ||
    return 1
  [[ "$purge_program" == "/usr/bin/sandbox-exec" ]] || return 0
  # Whether the bootout took effect is read back below, not trusted.
  launchctl bootout "$purge_target" >/dev/null 2>&1 || true
  for ((purge_attempt = 1; purge_attempt <= 30; purge_attempt += 1)); do
    purge_status=0
    launchctl print "$purge_target" >/dev/null 2>&1 || purge_status=$?
    [[ "$purge_status" -ne 113 ]] || return 0
    sleep 1
  done
  printf 'error: the sandboxed launchd job %s is still loaded; remove it with: launchctl bootout %s\n' \
    "$purge_target" "$purge_target" >&2
  return 1
}

# Put both services back under their normal launchd jobs. The offline
# stage calls this on success and cleanup calls it on every other exit,
# including INT, TERM and HUP. Safe to repeat: a job is booted out only
# while it runs sandbox-exec, and the normal jobs are started only while
# offline_services_stopped says this run stopped them. Never calls die.
restore_offline_supervision() {
  restore_status=0
  purge_offline_job tensorplate-agent || restore_status=1
  purge_offline_job tensorplate-observability || restore_status=1
  [[ "$restore_status" -eq 0 ]] || return 1
  [[ "$offline_services_stopped" == "1" ]] || return 0
  brew services start tensorplate-observability || restore_status=1
  brew services start tensorplate-agent || restore_status=1
  wait_for_service tensorplate-observability || restore_status=1
  wait_for_service tensorplate-agent || restore_status=1
  if [[ "$restore_status" -ne 0 ]]; then
    printf '%s\n' 'error: normal launchd supervision is not restored; run: brew services start tensorplate-observability && brew services start tensorplate-agent' >&2
    return 1
  fi
  offline_services_stopped=0
}

cleanup() {
  status=$?
  # A second INT, TERM or HUP must not interrupt the fail row or the
  # restore of launchd supervision and the agent config; the handlers
  # come back before the Homebrew restore, which an operator may need to
  # interrupt.
  trap '' INT TERM HUP
  # A stage that fails inside run_stage still has its output redirected
  # to the stage log; the operator reads cleanup's messages on the
  # terminal saved at startup.
  exec 1>&3 2>&4
  set +e
  if [[ "$status" -ne 0 && -n "$active_stage" ]]; then
    finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '%s\tfail\t%s\t%s\t%s\n' \
      "$active_stage" "$active_stage_started" "$finished_at" \
      "$(basename "$active_stage_log")" >>"$stage_results"
    tail -n 40 "$active_stage_log" >&2 || true
    printf 'error: stage %s failed with exit %s\n' "$active_stage" "$status" >&2
  fi
  restore_agent_config
  restore_offline_supervision || { [[ "$status" -ne 0 ]] || status=1; }
  trap 'exit 130' INT
  trap 'exit 143' TERM
  trap 'exit 129' HUP
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
  [[ -z "$lifecycle_marker" ]] || rm -f "$lifecycle_marker"
  restore_formula_trust
  rm -rf "$work_dir"
  exit "$status"
}
exec 3>&1 4>&2
# Without these, bash runs the EXIT trap with status 0 after TERM or HUP,
# and the interrupted stage would record no fail row.
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
trap cleanup EXIT

# A stage's pass row is written as soon as its body returns, so what
# stops a failing body is errexit, and errexit is only active inside the
# body because every call below is a bare top-level statement. Calling
# run_stage as an `if`, `while` or `until` condition, after `!`, or on the
# left of `||` or `&&` suspends errexit for the whole body, and a failed
# command would then record pass; so does `set +e`, which only cleanup
# uses. For the same reason a body must not end an assertion with a
# `[[ ]]`, `(( ))` or `!` command: macOS /bin/bash is 3.2, which does not
# apply errexit to `[[ ]]` or `(( ))`, and no bash applies it to `!`, so
# each one needs an explicit `|| die`.
# test/packaging/verify_macos_homebrew_lifecycle.sh lints the harness for
# these forms; it accepts a run_stage call only as a bare statement.
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

# The attempt count is this function's own optional argument, not the
# script's. A bash function does not inherit positional parameters, so
# calling it bare is what selects the default -- which is every call
# site here. SC2120 (and SC2119 at each call) warns about the shape
# rather than a defect.
# shellcheck disable=SC2120
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

# The artifact this run installed, taken from the pin the formula graph
# was verified against.
#
# This channel publishes no binary: all six formulae build from one
# source archive, so that archive is the only immutable artifact a
# digest can name here. It identifies the build input, not the bytes
# that landed -- two Macs build different binaries from it. Homebrew,
# not this harness, is what checks the downloaded archive against the
# pinned sha256.
record_artifact_digest() {
  python3 - "${evidence_dir}/formula-pin.json" >"${evidence_dir}/artifact-digest.txt" <<'PY'
import json
import pathlib
import sys

pin = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(f"{pin['source_sha256']}  {pin['source_url']}")
PY
}

capture_deploy_input() {
  python3 - "$bundle_dir" >"${evidence_dir}/deploy-input.json" <<'PY'
import hashlib
import json
import pathlib
import sys

bundle_dir = pathlib.Path(sys.argv[1]).resolve()
manifest = json.loads((bundle_dir / "manifest.json").read_text(encoding="utf-8"))
if manifest.get("backend_hint") != "python_pytorch":
    raise SystemExit("deploy-smoke bundle must declare backend_hint=python_pytorch")
model_artifacts = [
    item for item in manifest.get("artifacts", [])
    if isinstance(item, dict) and item.get("role") == "model"
]
if len(model_artifacts) != 1:
    raise SystemExit("deploy-smoke bundle must declare exactly one model artifact")
artifact = model_artifacts[0]
artifact_path = (bundle_dir / artifact["path"]).resolve()
if bundle_dir not in artifact_path.parents:
    raise SystemExit("deploy-smoke model artifact escapes the bundle root")
config = json.loads(artifact_path.read_text(encoding="utf-8"))
if config.get("backend_profile") != "mps_fixture" or config.get("device") != "mps":
    raise SystemExit("deploy-smoke config must select the MPS fixture on device=mps")
actual_digest = "sha256:" + hashlib.sha256(artifact_path.read_bytes()).hexdigest()
if actual_digest != artifact.get("digest"):
    raise SystemExit("deploy-smoke model artifact digest does not match its manifest")
print(json.dumps({
    "bundle_name": manifest.get("name"),
    "bundle_version": manifest.get("version"),
    "backend_hint": manifest.get("backend_hint"),
    "backend_profile": config["backend_profile"],
    "device": config["device"],
    "model_artifact": artifact["path"],
    "model_artifact_digest": actual_digest,
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
  # Homebrew reuses installed dependencies at the same version even when
  # their source archive changed. Remove the whole graph so the pin we
  # record describes every component this run exercises, not just the
  # newly installed umbrella formula.
  remove_candidate_graph
  for formula_name in "${FORMULAE[@]}"; do
    if formula_is_installed "$formula_name"; then
      die "formula remains installed before clean install: ${formula_name}"
    fi
  done
  HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 \
    brew install --formula "${tap_name}/tensorplate"
  candidate_active=1
  for formula_name in "${FORMULAE[@]}"; do
    installed="$(linked_formula_version "$formula_name")"
    [[ "$installed" == "$candidate_version" ]] ||
      die "candidate ${formula_name} install produced ${installed:-missing}; expected ${candidate_version}"
  done
}

verify_packaged_closure() {
  prefix="$(brew --prefix)"
  for formula_name in "${FORMULAE[@]}"; do
    formula_is_installed "$formula_name" ||
      die "formula is not installed: ${formula_name}"
  done
  [[ "$(command -v tensorplate)" == "${prefix}/bin/tensorplate" ]] ||
    die "tensorplate on PATH is not the Homebrew launcher at ${prefix}/bin/tensorplate"
  [[ -x "$(brew --prefix tensorplate-agent)/bin/tensorplate-agent" ]] ||
    die "tensorplate-agent binary is missing or not executable"
  [[ -x "$(brew --prefix tensorplate-serving)/libexec/tensorplate-serving" ]] ||
    die "tensorplate-serving binary is missing or not executable"
  [[ -x "$(brew --prefix tensorplate-observability)/bin/tensorplate-observability" ]] ||
    die "tensorplate-observability binary is missing or not executable"
  [[ -x "$(brew --prefix tensorplate-backend-python-pytorch)/bin/tensorplate-backend-python-pytorch" ]] ||
    die "tensorplate-backend-python-pytorch binary is missing or not executable"
  [[ -f "${prefix}/share/tensorplate/platform/rows/macos26-m1pro-16gb.json" ]] ||
    die "installed platform registry is missing the macos26-m1pro-16gb row"
  [[ -f "${prefix}/share/tensorplate/backends/python_pytorch/backend.json" ]] ||
    die "installed python_pytorch backend descriptor is missing"

  for directory in \
    "${prefix}/etc/tensorplate" \
    "${prefix}/var/tensorplate" \
    "${prefix}/var/tensorplate/state" \
    "${prefix}/var/log/tensorplate"; do
    [[ "$(stat -f '%Lp' "$directory")" == "750" ]] ||
      die "unexpected mode for ${directory}"
  done
  [[ "$(stat -f '%Lp' "${prefix}/var/run/tensorplate")" == "700" ]] ||
    die "unexpected mode for ${prefix}/var/run/tensorplate"
  [[ "$(stat -f '%Lp' "${prefix}/etc/tensorplate/agent.json")" == "640" ]] ||
    die "unexpected mode for ${prefix}/etc/tensorplate/agent.json"
  [[ "$(stat -f '%Lp' "${prefix}/etc/tensorplate/observability.json")" == "640" ]] ||
    die "unexpected mode for ${prefix}/etc/tensorplate/observability.json"
  [[ "$(stat -f '%Lp' "${prefix}/etc/tensorplate/cli.json")" == "644" ]] ||
    die "unexpected mode for ${prefix}/etc/tensorplate/cli.json"
}

verify_m1_exact_row() {
  prefix="$(brew --prefix)"
  doctor_output="${work_dir}/doctor-m1-exact-row.json"
  current_run_agent_log="${work_dir}/agent-error-current-run.log"
  tensorplate doctor --output json >"$doctor_output"
  tail -c "+$((agent_error_log_start + 1))" \
    "${prefix}/var/log/tensorplate/agent.error.log" >"$current_run_agent_log"
  python3 - \
    "$doctor_output" \
    "$current_run_agent_log" \
    "${prefix}/share/tensorplate/platform/rows/macos26-m1pro-16gb.json" \
    "${prefix}/share/tensorplate/platform/rows/macos26-apple-m-series-preview.json" \
    >"${evidence_dir}/m1-exact-row.json" <<'PY'
import json
import re
import sys

doctor_path, agent_log_path, exact_path, family_path = sys.argv[1:]
payload = json.load(open(doctor_path, encoding="utf-8"))["payload"]
findings = {item["id"]: item for item in payload["findings"]}
profile = findings["platform_profile"]
exact = json.load(open(exact_path, encoding="utf-8"))
family = json.load(open(family_path, encoding="utf-8"))
agent_log = open(agent_log_path, encoding="utf-8").read()

exact_row = "macos26-m1pro-16gb"
family_row = "macos26-apple-m-series-preview"
matches = re.findall(
    r"platform admission: row=(\S+) reason=(\S+) "
    r".*?max_resident_model_memory=(\d+)",
    agent_log,
)
if not matches:
    raise SystemExit("current-run agent log contains no platform admission decision")
selected_row, reason, memory_ceiling = matches[-1]
checks = {
    "doctor_has_no_failures": payload["failing"] == 0,
    "platform_profile_ok": profile["status"] == "ok",
    "exact_row_selected": selected_row == exact_row,
    "family_row_not_selected": selected_row != family_row,
    "admission_reason_clear": reason == "none",
    "admission_memory_ceiling": int(memory_ceiling) == exact["accelerator"]["memory_bytes"],
    "exact_row_production": exact["support_level"] == "Production",
    "family_row_preview": family["support_level"] == "Preview",
    "family_row_16_gib_ceiling": family["accelerator"]["memory_bytes"] == 17179869184,
}
failed = [name for name, passed in checks.items() if not passed]
if failed:
    raise SystemExit("M1 exact-row checks failed: " + ", ".join(failed))
print(json.dumps({
    "detected_target": "Apple M1 Pro, 16 GB",
    "selected_row": selected_row,
    "selected_support_level": exact["support_level"],
    "selected_memory_ceiling_bytes": int(memory_ceiling),
    "family_fallback_row": family_row,
    "family_fallback_selected": False,
    "family_fallback_support_level": family["support_level"],
    "family_fallback_memory_ceiling_bytes": family["accelerator"]["memory_bytes"],
}, indent=2, sort_keys=True))
PY
}

start_services() {
  agent_error_log="$(brew --prefix)/var/log/tensorplate/agent.error.log"
  if [[ -f "$agent_error_log" ]]; then
    agent_error_log_start="$(stat -f '%z' "$agent_error_log")"
  else
    agent_error_log_start=0
  fi
  observability_error_log="$(brew --prefix)/var/log/tensorplate/observability.error.log"
  if [[ -f "$observability_error_log" ]]; then
    observability_error_log_start="$(stat -f '%z' "$observability_error_log")"
  else
    observability_error_log_start=0
  fi
  events_log="$(brew --prefix)/var/log/tensorplate/events.ndjson"
  if [[ -f "$events_log" ]]; then
    events_log_start="$(stat -f '%z' "$events_log")"
  else
    events_log_start=0
  fi
  brew services start tensorplate-agent
  brew services start tensorplate-observability
  wait_for_service tensorplate-agent
  wait_for_service tensorplate-observability
  wait_for_agent_ready
  [[ "$(stat -f '%Lp' "$(brew --prefix)/var/run/tensorplate/agent.sock")" == "600" ]] ||
    die "agent socket is not mode 0600"
  if brew services list | awk '$1 == "tensorplate-serving" {found = 1} END {exit !found}'; then
    die "tensorplate-serving unexpectedly exposes a Homebrew service"
  fi
  brew services list
  launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-agent"
  launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-observability"
}

# Any arguments are a command prefix the probe runs under; the offline
# stage passes run_denied.
probe_mps() {
  pytorch_python="$(brew --prefix pytorch)/libexec/bin/python"
  backend_libexec="$(brew --prefix tensorplate-backend-python-pytorch)/libexec"
  [[ -x "$pytorch_python" ]] || die "PyTorch formula interpreter is missing"
  "$@" /usr/bin/env PYTHONPATH="$backend_libexec" "$pytorch_python" - <<'PY'
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
  deploy_output="${work_dir}/deploy-output.json"
  status_output="${work_dir}/status-output.json"
  cp -R "$bundle_dir" "$smoke_bundle"
  cd /private/tmp
  tensorplate doctor --output json
  tensorplate deploy "$smoke_bundle" \
    --deployment-id "$smoke_deployment_id" \
    --output json >"$deploy_output"
  tensorplate status --output json >"$status_output"
  cd - >/dev/null
  python3 - "$deploy_output" "$status_output" "${evidence_dir}/deploy-input.json" \
    "$smoke_deployment_id" \
    >"${evidence_dir}/deploy-result.json" <<'PY'
import json
import sys
import urllib.parse
import urllib.request

deploy_path, status_path, input_path, expected_deployment = sys.argv[1:]
deploy = json.load(open(deploy_path, encoding="utf-8"))["payload"]
status = json.load(open(status_path, encoding="utf-8"))["payload"]
deploy_input = json.load(open(input_path, encoding="utf-8"))
agent = status.get("agent") or {}
active = agent.get("active") or {}
supervision = agent.get("supervision")
serving_url = active.get("serving_url")
serving_parts = urllib.parse.urlsplit(serving_url) if isinstance(serving_url, str) else None
serving_url_valid = (
    serving_parts is not None
    and serving_parts.scheme == "http"
    and serving_parts.hostname == "127.0.0.1"
    and serving_parts.path == "/infer"
)
serving_health = {}
if serving_url_valid:
    health_url = urllib.parse.urlunsplit(
        (serving_parts.scheme, serving_parts.netloc, "/health", "", "")
    )
    try:
        with urllib.request.urlopen(health_url, timeout=5) as response:
            serving_health = json.load(response)
    except (OSError, ValueError, json.JSONDecodeError):
        serving_health = {}
supervision_healthy = (
    supervision is None
    or (
        isinstance(supervision, dict)
        and supervision.get("serving_state") == "ready"
        and supervision.get("crash_loop") is False
    )
)
checks = {
    "deployment_phase": deploy.get("phase") == "active",
    "deployment_id": deploy.get("deployment_id") == expected_deployment,
    "status_severity": status.get("severity") == "ready",
    "agent_state": agent.get("agent_state") == "ready",
    "active_deployment": active.get("deployment_id") == expected_deployment,
    "active_backend": active.get("backend") == "python_pytorch",
    "active_serving_url": serving_url_valid,
    "serving_health_state": serving_health.get("state") == "ready",
    "serving_health_deployment": (
        serving_health.get("active_model_id") == expected_deployment
    ),
    "supervision_healthy_when_configured": supervision_healthy,
}
failed = [name for name, passed in checks.items() if not passed]
if failed:
    raise SystemExit("deploy-smoke readiness checks failed: " + ", ".join(failed))
print(json.dumps({
    "deployment_id": expected_deployment,
    "deployment_phase": "active",
    "status_severity": "ready",
    "agent_state": "ready",
    "serving_endpoint": "ready",
    "supervision_state": (
        "not_configured" if supervision is None else supervision["serving_state"]
    ),
    "backend": "python_pytorch",
    "backend_profile": deploy_input["backend_profile"],
    "device": deploy_input["device"],
    "mps_tensor_operation_required_for_load": True,
}, indent=2, sort_keys=True))
PY
}

# Status still reports the deploy-smoke deployment; both launchd stderr
# logs gained output after launchd-start recorded their sizes; and
# `tensorplate logs` reads the packaged structured event log and returns
# an event this run's observability service wrote. The agent component
# is not queried: the agent writes no structured events, so that filter
# returns nothing on every install. Every command here checks its own
# status rather than relying on errexit.
verify_status_logs() {
  log_dir="$(brew --prefix)/var/log/tensorplate" || die "brew --prefix failed"
  status_logs_status="${work_dir}/status-logs-status.json"
  status_logs_cli="${work_dir}/status-logs-cli.json"
  tensorplate status --output json >"$status_logs_status" ||
    die "tensorplate status did not answer"
  tensorplate logs --component observability --tail 100 --output json \
    >"$status_logs_cli" ||
    die "tensorplate logs failed on the Homebrew install"
  # The raw documents go to this stage's local log only; status-logs.json
  # carries path-free results.
  cat "$status_logs_status" "$status_logs_cli" ||
    die "could not record the status and logs output"
  # The log files are read after the CLI ran, so the current-run event
  # slice holds every event the CLI could have returned.
  python3 - "$status_logs_status" "$status_logs_cli" "$log_dir" \
    "$smoke_deployment_id" "$agent_error_log_start" \
    "$observability_error_log_start" "$events_log_start" \
    >"${evidence_dir}/status-logs.json" <<'PY' || die "status-logs checks failed"
import json
import pathlib
import sys

(status_path, cli_path, log_dir, expected_deployment,
 agent_start, observability_start, events_start) = sys.argv[1:]
log_dir = pathlib.Path(log_dir)

def since(name, offset):
    # Bytes appended after launchd-start recorded the file's size. A file
    # that shrank or rotated since then yields nothing, which fails closed.
    try:
        with open(log_dir / name, "rb") as handle:
            handle.seek(int(offset))
            return handle.read()
    except OSError as error:
        raise SystemExit(f"cannot read {name}: {error.strerror}")

status_document = json.load(open(status_path, encoding="utf-8"))
status = status_document.get("payload") or {}
active = (status.get("agent") or {}).get("active") or {}
logs_document = json.load(open(cli_path, encoding="utf-8"))
logs = logs_document.get("payload") or {}
entries = logs.get("entries") or []
agent_output = since("agent.error.log", agent_start).strip()
observability_output = since("observability.error.log", observability_start).strip()
current_events = []
for line in since("events.ndjson", events_start).decode("utf-8", "replace").splitlines():
    # Skipped like the CLI skips them: a torn or non-JSON line is not an event.
    try:
        current_events.append(json.loads(line))
    except ValueError:
        pass

checks = {
    "status_command": status_document.get("command") == "status",
    "status_severity": status.get("severity") == "ready",
    "status_active_deployment": active.get("deployment_id") == expected_deployment,
    "agent_log_current_run_output": bool(agent_output),
    "observability_log_current_run_output": bool(observability_output),
    "logs_command": logs_document.get("command") == "logs",
    "logs_source_is_packaged_file": (
        logs.get("kind") == "file"
        and logs.get("source") == str(log_dir / "events.ndjson")
    ),
    "logs_include_current_run": bool(entries) and entries[-1] in current_events,
}
failed = [name for name, passed in checks.items() if not passed]
if "logs_source_is_packaged_file" in failed:
    print(
        "tensorplate logs did not read the packaged events.ndjson; a "
        "TENSORPLATE_CLI_CONFIG set in this shell overrides the packaged cli.json",
        file=sys.stderr,
    )
if failed:
    raise SystemExit("status-logs checks failed: " + ", ".join(failed))
print(json.dumps({
    "deployment_id": expected_deployment,
    "status_severity": "ready",
    "status_reports_deployment": True,
    "agent_launchd_log_current_run_bytes": len(agent_output),
    "observability_launchd_log_current_run_bytes": len(observability_output),
    "logs_command": "pass",
    "logs_source": "packaged events.ndjson",
    "logs_entries_returned": len(entries),
    "current_run_structured_events": len(current_events),
}, indent=2, sort_keys=True))
PY
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
  [[ -n "$before_agent" && -n "$after_agent" && "$before_agent" != "$after_agent" ]] ||
    die "agent PID did not change across restart: ${before_agent:-none} -> ${after_agent:-none}"
  [[ -n "$before_observability" && -n "$after_observability" &&
    "$before_observability" != "$after_observability" ]] ||
    die "observability PID did not change across restart: ${before_observability:-none} -> ${after_observability:-none}"
  printf 'agent %s -> %s\nobservability %s -> %s\n' \
    "$before_agent" "$after_agent" "$before_observability" "$after_observability"
}

exercise_crash_loop() {
  agent_config="$(brew --prefix)/etc/tensorplate/agent.json"
  # launchd appends to this log and nothing truncates it, so config errors
  # from earlier runs are still in it. Only output written after the
  # config is broken shows the agent failing on this run's config.
  crash_loop_agent_log="$(brew --prefix)/var/log/tensorplate/agent.error.log" ||
    die "brew --prefix failed"
  crash_loop_agent_log_start="$(stat -f '%z' "$crash_loop_agent_log")" ||
    die "cannot size the agent launchd error log before breaking the config"
  agent_config_backup="${work_dir}/agent.json"
  cp "$agent_config" "$agent_config_backup"
  printf '{ invalid json\n' >"$agent_config"
  chmod 0640 "$agent_config"
  brew services restart tensorplate-agent >/dev/null 2>&1 || true
  sleep 12
  launchctl print "gui/$(id -u)/homebrew.mxcl.tensorplate-agent"
  tail -c "+$((crash_loop_agent_log_start + 1))" "$crash_loop_agent_log" \
    >"${work_dir}/agent-error-crash-loop.log" ||
    die "cannot read the agent launchd error log after breaking the config"
  grep -q "config" "${work_dir}/agent-error-crash-loop.log" ||
    die "agent logged no config error after its config was broken"
  cp "$agent_config_backup" "$agent_config"
  chmod 0640 "$agent_config"
  agent_config_backup=""
  brew services restart tensorplate-agent
  wait_for_service tensorplate-agent
  wait_for_agent_ready
  tensorplate status --output json
}

# Every command the offline stage runs against the denied services goes
# through here, so the CLI, the probe and the MPS check run under the
# same profile file as the launchd jobs.
run_denied() {
  sandbox-exec -f "$offline_profile" "$@"
}

# Stop the normal launchd jobs and run each service from a plist that
# differs from the formula's only in running ProgramArguments under
# sandbox-exec. `brew services run --file` bootstraps that file without
# copying it into ~/Library/LaunchAgents, so launchd keeps KeepAlive, the
# throttle and the formula environment, and a logout or reboot loads the
# normal job again.
enter_offline_denial() {
  offline_services_stopped=1
  for service_name in tensorplate-agent tensorplate-observability; do
    brew services stop --keep "$service_name" ||
      die "brew services stop --keep ${service_name} failed"
    if launchctl print "gui/${uid}/homebrew.mxcl.${service_name}" >/dev/null 2>&1; then
      die "${service_name} is still loaded after brew services stop --keep"
    fi
  done
  # `brew services run` does nothing while a job holds a pid, and a
  # leftover worker would hold a serving port.
  offline_helper quiesced --profile "$offline_profile" ||
    die "a TensorPlate process or serving-port listener outlived the stopped services"
  for service_name in tensorplate-observability tensorplate-agent; do
    formula_prefix="$(brew --prefix "$service_name")" || die "brew --prefix ${service_name} failed"
    offline_helper derive-plist \
      --formula-plist "${formula_prefix}/homebrew.mxcl.${service_name}.plist" \
      --label "homebrew.mxcl.${service_name}" \
      --program "${formula_prefix}/bin/${service_name}" \
      --profile "$offline_profile" \
      --plist-out "${denial_dir}/homebrew.mxcl.${service_name}.plist" ||
      die "cannot derive the sandboxed ${service_name} launchd plist"
    brew services run "$service_name" --file="${denial_dir}/homebrew.mxcl.${service_name}.plist" ||
      die "brew services run --file failed for ${service_name}"
  done
}

# Startup recovery of the deploy-smoke deployment, answered over the
# agent socket from inside the sandbox.
wait_for_denied_status() {
  for ((attempt = 1; attempt <= 120; attempt += 1)); do
    if run_denied tensorplate status --output json \
      >"${denial_dir}/status-recovered.json" 2>"${denial_dir}/status-recovered.err" &&
      offline_helper status-check --status "${denial_dir}/status-recovered.json" \
        --deployment "$smoke_deployment_id" --profile "$offline_profile" >/dev/null 2>&1; then
      cat "${denial_dir}/status-recovered.json" || die "cannot record the recovered status"
      return 0
    fi
    sleep 1
  done
  # Record the last answer and the checks it failed before giving up.
  cat "${denial_dir}/status-recovered.json" "${denial_dir}/status-recovered.err" || true
  offline_helper status-check --status "${denial_dir}/status-recovered.json" \
    --deployment "$smoke_deployment_id" --profile "$offline_profile" || true
  die "the agent did not recover the deploy-smoke deployment under the offline profile"
}

# After restore_offline_supervision: both services run their normal
# jobs from the formula plists, and no sandboxed TensorPlate process is
# left behind.
verify_normal_supervision() {
  for service_name in tensorplate-agent tensorplate-observability; do
    formula_prefix="$(brew --prefix "$service_name")" || die "brew --prefix ${service_name} failed"
    launch_agent="${HOME}/Library/LaunchAgents/homebrew.mxcl.${service_name}.plist"
    launchctl print "gui/${uid}/homebrew.mxcl.${service_name}" \
      >"${denial_dir}/${service_name#tensorplate-}-restored.txt" ||
      die "${service_name} is not loaded after the offline stage"
    brew services info "$service_name" --json \
      >"${denial_dir}/${service_name#tensorplate-}-restored-info.json" ||
      die "brew services info ${service_name} failed after the offline stage"
    cat "${denial_dir}/${service_name#tensorplate-}-restored-info.json" ||
      die "cannot record brew services info for ${service_name}"
    offline_helper launchd-job --print "${denial_dir}/${service_name#tensorplate-}-restored.txt" \
      --program "${formula_prefix}/bin/${service_name}" --path "$launch_agent" \
      --brew-info "${denial_dir}/${service_name#tensorplate-}-restored-info.json" \
      --out "${denial_dir}/${service_name#tensorplate-}-restored.json" ||
      die "${service_name} is not back under its normal launchd job"
    cmp "$launch_agent" "${formula_prefix}/homebrew.mxcl.${service_name}.plist" ||
      die "the ${service_name} LaunchAgents plist differs from the formula plist"
  done
  wait_for_agent_ready || die "the agent did not answer outside the sandbox after the offline stage"
  offline_helper sandbox-state --profile "$offline_profile" --expect unsandboxed \
    --job "${denial_dir}/agent-restored.json" --job "${denial_dir}/observability-restored.json" \
    --all-tensorplate-processes --out "${denial_dir}/restored-sandbox.json" ||
    die "a sandboxed TensorPlate process remains after the offline stage"
}

# offline-runtime: both launchd services, startup recovery, a fresh
# deploy, inference, doctor and the MPS probe all run under one
# sandbox-exec profile (macos_offline_runtime.py PROFILE_TEMPLATE) that
# denies every network operation except loopback on the two serving
# ports and unix sockets other than mDNSResponder. A probe inside the
# sandbox must be refused every other destination with EPERM, where the
# same sends outside it are not; sandbox_check must read every process in
# the service trees as sandboxed with the network denied; and every
# internet socket the tree holds must be bound to loopback.
#
# Accepted residual gaps: SBPL `localhost` matches every address
# configured on the host whatever the interface, so fe80::1 (on lo0)
# reached through another interface is allowed on the two serving ports;
# a wildcard listener on those ports would accept LAN peers, which the
# listener assertion refuses; and unix-socket or XPC brokers stay
# reachable.
#
# Every command checks its own status: errexit alone does not stop a
# failed assignment inside a helper, and a failing check must leave the
# stage through die so cleanup restores normal supervision.
verify_offline_runtime() {
  prefix="$(brew --prefix)" || die "brew --prefix failed"
  uid="$(id -u)" || die "id -u failed"
  denial_dir="${work_dir}/offline-denial"
  offline_profile="${denial_dir}/network-denied.sb"
  log_dir="${prefix}/var/log/tensorplate"
  [[ -z "$agent_config_backup" ]] ||
    die "the agent config is still replaced by the crash-loop stage"
  mkdir -p "$denial_dir" || die "cannot create the offline-runtime work directory"

  for service_name in tensorplate-agent tensorplate-observability; do
    formula_prefix="$(brew --prefix "$service_name")" || die "brew --prefix ${service_name} failed"
    launchctl print "gui/${uid}/homebrew.mxcl.${service_name}" \
      >"${denial_dir}/${service_name#tensorplate-}-before.txt" ||
      die "${service_name} is not loaded before the offline stage"
    offline_helper launchd-job --print "${denial_dir}/${service_name#tensorplate-}-before.txt" \
      --program "${formula_prefix}/bin/${service_name}" \
      --path "${HOME}/Library/LaunchAgents/homebrew.mxcl.${service_name}.plist" \
      --out "${denial_dir}/${service_name#tensorplate-}-before.json" ||
      die "${service_name} is not running under its normal launchd job before the offline stage"
  done

  offline_helper render-profile --agent-config "${prefix}/etc/tensorplate/agent.json" \
    --profile "$offline_profile" --out "${denial_dir}/profile.json" ||
    die "cannot render the offline profile from the installed agent config"
  offline_helper control --out "${denial_dir}/control.json" ||
    die "the unsandboxed network control failed"
  agent_log_offset="$(offline_helper log-size --log "${log_dir}/agent.error.log")" ||
    die "cannot size the agent launchd error log"
  observability_log_offset="$(offline_helper log-size --log "${log_dir}/observability.error.log")" ||
    die "cannot size the observability launchd error log"

  enter_offline_denial

  for service_name in tensorplate-agent tensorplate-observability; do
    short_name="${service_name#tensorplate-}"
    formula_prefix="$(brew --prefix "$service_name")" || die "brew --prefix ${service_name} failed"
    launchctl print "gui/${uid}/homebrew.mxcl.${service_name}" \
      >"${denial_dir}/${short_name}-denied.txt" ||
      die "${service_name} is not loaded after brew services run --file"
    brew services info "$service_name" --json >"${denial_dir}/${short_name}-denied-info.json" ||
      die "brew services info ${service_name} failed under the offline profile"
    cat "${denial_dir}/${short_name}-denied.txt" "${denial_dir}/${short_name}-denied-info.json" ||
      die "cannot record the sandboxed ${service_name} job"
    # The expected arguments come from the formula plist and the literal
    # sandbox-exec prefix, not from the derived plist, so a derivation
    # that dropped the prefix cannot match itself.
    offline_helper launchd-job --print "${denial_dir}/${short_name}-denied.txt" \
      --program /usr/bin/sandbox-exec \
      --path "${denial_dir}/homebrew.mxcl.${service_name}.plist" \
      --sandboxed-arguments-from "${formula_prefix}/homebrew.mxcl.${service_name}.plist" \
      --profile "$offline_profile" \
      --pid-differs-from "${denial_dir}/${short_name}-before.json" \
      --brew-info "${denial_dir}/${short_name}-denied-info.json" \
      --out "${denial_dir}/${short_name}-denied.json" ||
      die "${service_name} is not running as the sandboxed launchd job"
  done
  offline_helper sandbox-state --profile "$offline_profile" --expect sandboxed \
    --job "${denial_dir}/agent-denied.json" --job "${denial_dir}/observability-denied.json" \
    --out "${denial_dir}/services-sandbox.json" ||
    die "a launchd service does not read back as sandboxed with the network denied"

  wait_for_denied_status
  offline_helper admission-check \
    --agent-log "${log_dir}/agent.error.log" --agent-offset "$agent_log_offset" \
    --observability-log "${log_dir}/observability.error.log" \
    --observability-offset "$observability_log_offset" \
    --exact-row "${prefix}/share/tensorplate/platform/rows/macos26-m1pro-16gb.json" \
    --out "${denial_dir}/admission.json" ||
    die "the sandboxed services did not start once on the exact row with validated evidence"

  cp -R "$bundle_dir" "${denial_dir}/deploy-bundle" || die "cannot copy the deploy bundle"
  deploy_status=0
  # Run from the work directory so nothing resolves from a source checkout.
  (cd "$denial_dir" && run_denied tensorplate deploy "${denial_dir}/deploy-bundle" \
    --deployment-id "$offline_deployment_id" --output json) \
    >"${denial_dir}/deploy.json" 2>"${denial_dir}/deploy.err" || deploy_status=$?
  cat "${denial_dir}/deploy.json" "${denial_dir}/deploy.err" ||
    die "cannot record the deploy output"
  [[ "$deploy_status" -eq 0 ]] ||
    die "tensorplate deploy failed under the offline profile with status ${deploy_status}"
  offline_helper deploy-check --deploy "${denial_dir}/deploy.json" \
    --deployment "$offline_deployment_id" --out "${denial_dir}/deploy-check.json" ||
    die "the deploy under the offline profile did not activate ${offline_deployment_id}"
  run_denied tensorplate status --output json >"${denial_dir}/status-deployed.json" ||
    die "tensorplate status failed under the offline profile after the deploy"
  cat "${denial_dir}/status-deployed.json" || die "cannot record the status output"
  offline_helper status-check --status "${denial_dir}/status-deployed.json" \
    --deployment "$offline_deployment_id" --profile "$offline_profile" \
    --out "${denial_dir}/status-deployed-check.json" ||
    die "status under the offline profile does not report the new deployment"

  offline_helper infer-request --out "${denial_dir}/infer-request.json" >/dev/null ||
    die "cannot write the inference request"
  run_denied tensorplate infer --input "${denial_dir}/infer-request.json" \
    --output-file "${denial_dir}/infer-response.json" ||
    die "tensorplate infer failed under the offline profile"
  cat "${denial_dir}/infer-response.json" || die "cannot record the inference response"
  offline_helper infer-check --request "${denial_dir}/infer-request.json" \
    --response "${denial_dir}/infer-response.json" --out "${denial_dir}/infer-check.json" ||
    die "inference under the offline profile did not echo the request"

  run_denied "$python_bin" "$offline_helper_path" probe \
    --agent-socket "${prefix}/var/run/tensorplate/agent.sock" \
    --status "${denial_dir}/status-deployed.json" --out "${denial_dir}/probe.json" ||
    die "the network probe did not run under the offline profile"
  offline_helper classify --probe "${denial_dir}/probe.json" \
    --control "${denial_dir}/control.json" --deployment "$offline_deployment_id" \
    --out "${denial_dir}/classify.json" ||
    die "the offline profile did not refuse the network as required"

  offline_helper process-tree --job "${denial_dir}/agent-denied.json" \
    --pids-out "${denial_dir}/tree-pids.txt" --out "${denial_dir}/tree.json" ||
    die "the agent's process tree lacks a serving worker or backend sidecar"
  offline_helper sandbox-state --profile "$offline_profile" --expect sandboxed \
    --pids-file "${denial_dir}/tree-pids.txt" --job "${denial_dir}/observability-denied.json" \
    --out "${denial_dir}/tree-sandbox.json" ||
    die "a process in the service trees does not read back as sandboxed with the network denied"
  offline_helper listeners --pids-file "${denial_dir}/tree-pids.txt" \
    --status "${denial_dir}/status-deployed.json" --out "${denial_dir}/listeners.json" ||
    die "the agent's process tree holds a non-loopback socket or no serving listener"

  doctor_status=0
  run_denied tensorplate doctor --output json >"${denial_dir}/doctor.json" \
    2>"${denial_dir}/doctor.err" || doctor_status=$?
  cat "${denial_dir}/doctor.json" "${denial_dir}/doctor.err" ||
    die "cannot record the doctor output"
  offline_helper doctor-check --doctor "${denial_dir}/doctor.json" --status "$doctor_status" \
    --exact-row macos26-m1pro-16gb --out "${denial_dir}/doctor-check.json" ||
    die "doctor under the offline profile is not green on the exact row"

  probe_mps run_denied || die "the MPS probe failed under the offline profile"

  # KeepAlive would hide a crash: the same pid and one run prove both
  # services stayed up through the stage.
  for service_name in tensorplate-agent tensorplate-observability; do
    short_name="${service_name#tensorplate-}"
    launchctl print "gui/${uid}/homebrew.mxcl.${service_name}" \
      >"${denial_dir}/${short_name}-final.txt" ||
      die "${service_name} is not loaded at the end of the offline stage"
    offline_helper launchd-job --print "${denial_dir}/${short_name}-final.txt" \
      --program /usr/bin/sandbox-exec --same-pid-as "${denial_dir}/${short_name}-denied.json" \
      --runs 1 --out "${denial_dir}/${short_name}-final.json" ||
      die "${service_name} restarted during the offline stage"
  done

  restore_offline_supervision ||
    die "normal launchd supervision was not restored after the offline stage"
  verify_normal_supervision

  offline_helper evidence --dir "$denial_dir" --deployment "$offline_deployment_id" \
    --out "${evidence_dir}/offline-runtime.json" ||
    die "cannot write the offline-runtime evidence"
}

# offline-profile: before any Homebrew change, prove that sandbox-exec
# still enforces what verify_offline_runtime relies on, using the same
# profile template on two ephemeral ports.
verify_offline_profile() {
  offline_helper preflight --work-dir "${work_dir}/offline-profile" \
    --out "${evidence_dir}/offline-profile.json" ||
    die "sandbox-exec does not enforce the offline profile semantics"
}

uninstall_candidate() {
  remove_candidate_graph
  for formula_name in "${FORMULAE[@]}"; do
    if formula_is_installed "$formula_name"; then
      die "formula remains installed after uninstall: ${formula_name}"
    fi
  done
  [[ ! -e "$HOME/Library/LaunchAgents/homebrew.mxcl.tensorplate-agent.plist" ]] ||
    die "tensorplate-agent LaunchAgent plist remains after uninstall"
  [[ ! -e "$HOME/Library/LaunchAgents/homebrew.mxcl.tensorplate-observability.plist" ]] ||
    die "tensorplate-observability LaunchAgent plist remains after uninstall"
  [[ ! -e "$(brew --prefix)/bin/tensorplate" ]] ||
    die "tensorplate launcher remains after uninstall"
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
  lifecycle_marker="$(
    mktemp "$(brew --prefix)/var/tensorplate/state/lifecycle-marker.XXXXXX"
  )"
  printf 'preserve-across-formula-rollback\n' >"$lifecycle_marker"
  remove_candidate_graph
  install_baseline
  [[ "$(cat "$lifecycle_marker")" == "preserve-across-formula-rollback" ]] ||
    die "lifecycle state marker did not survive the rollback"
  [[ "$(linked_formula_version tensorplate)" == "$baseline_version" ]] ||
    die "rollback did not link the baseline tensorplate ${baseline_version}"
  rm -f "$lifecycle_marker"
  lifecycle_marker=""
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

write_sanitized_transcript() {
  python3 - "$stage_results" "$evidence_dir" "$baseline_version" "$candidate_version" \
    >"${evidence_dir}/sanitized-transcript.json" <<'PY'
import csv
import json
import pathlib
import sys

stages_path, evidence_path, baseline_version, candidate_version = sys.argv[1:]
evidence_dir = pathlib.Path(evidence_path)
with open(stages_path, encoding="utf-8", newline="") as handle:
    stages = list(csv.DictReader(handle, delimiter="\t"))

def load_json(name):
    path = evidence_dir / name
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else None

details = {
    "host-facts": load_json("host-facts.json"),
    "formula-pin": load_json("formula-pin.json"),
    "deploy-input": load_json("deploy-input.json"),
    "baseline": {"linked_version": baseline_version},
    "tap-trust": {"candidate_formula_graph_trusted": True},
    "clean-install": {"candidate_version": candidate_version},
    "packaged-closure": {"all_six_formulae_installed": True},
    "m1-exact-row": load_json("m1-exact-row.json"),
    "launchd-start": {
        "agent": "started",
        "observability": "started",
        "serving_has_independent_job": False,
    },
    "mps-capability": load_json("mps-capability.log"),
    "deploy-smoke": load_json("deploy-result.json"),
    "status-logs": load_json("status-logs.json"),
    "launchd-restart": {"agent": "restarted", "observability": "restarted"},
    "launchd-crash-loop": {"agent_recovered": True},
    "offline-profile": load_json("offline-profile.json"),
    "offline-runtime": load_json("offline-runtime.json"),
    "uninstall": {"all_six_formulae_absent": True, "launchd_jobs_absent": True},
    "baseline-restore": {"linked_version": baseline_version},
    "upgrade": {"from": baseline_version, "to": candidate_version},
    "rollback": {
        "from": candidate_version,
        "to": baseline_version,
        "state_preserved": True,
    },
    "tap-restored": {"worktree_clean": True},
}
transcript = []
for stage in stages:
    transcript.append({
        "stage": stage["stage"],
        "status": stage["status"],
        "started_at": stage["started_at"],
        "finished_at": stage["finished_at"],
        "summary": details.get(stage["stage"], {"completed": stage["status"] == "pass"}),
    })
print(json.dumps({
    "schema_version": "1",
    "result": "pass" if transcript and all(item["status"] == "pass" for item in transcript) else "fail",
    "redaction": {
        "raw_command_output_included": False,
        "operator_paths_included": False,
        "environment_values_included": False,
        "policy": "allowlisted structured results only",
    },
    "stages": transcript,
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
run_stage deploy-input capture_deploy_input
run_stage baseline tensorplate version
run_stage tap-trust verify_tap_trust
run_stage offline-profile verify_offline_profile
if [[ "$preflight_only" == "1" ]]; then
  write_summary
  write_sanitized_transcript
  trap - EXIT
  rm -rf "$work_dir"
  pass "macOS Homebrew lifecycle preflight complete; evidence: ${evidence_dir}"
  exit 0
fi
run_stage clean-install install_candidate_clean
# Recorded here rather than beside the formula-pin stage it reads: a
# preflight run returns above without installing anything, and a digest
# filed for an archive nobody fetched would attest an install that never
# happened. Not a run_stage -- it is not one of the lifecycle stages, and
# a row in the stage log would claim it was.
record_artifact_digest || die "failed to record the installed artifact digest"
run_stage packaged-closure verify_packaged_closure
run_stage launchd-start start_services
run_stage m1-exact-row verify_m1_exact_row
run_stage mps-capability probe_mps
run_stage deploy-smoke deploy_smoke
run_stage status-logs verify_status_logs
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
write_sanitized_transcript

trap - EXIT
rm -rf "$work_dir"
pass "macOS Homebrew lifecycle rehearsal complete; evidence: ${evidence_dir}"
