#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Lifecycle validation for the Ubuntu 24.04 x86_64 cloud rows.
#
# Run by hand on a cloud VM that already exists. This script provisions
# nothing: it does not create, start, resize or delete any cloud
# resource, and it carries no project, zone or account identifier. What
# it needs is a host that is already running and a candidate artifact
# set already copied onto it.
#
# It is the first consumer of tools/validation/lifecycle-stages.sh, so
# it emits the eight canonical stage names natively rather than writing
# a harness-specific stage log and converting it afterwards. The two
# physical harnesses predate that runner and cannot be rewritten
# untested on hardware nobody can reach from CI; this one has no such
# history to preserve.
#
# WHAT A PASSING RUN PROVES
#   install       the candidate installs on this OS through the shipped
#                 installer, the services come up, and doctor resolves
#                 this row by live detection with nothing failing
#   deploy-smoke  the control plane admits a bundle, runs a worker for
#                 it, and answers an inference request end to end
#   status-logs   status answers and still reports the deployment, and
#                 the services' journal carries their output
#   restart       both services restart and the agent re-warms its
#                 deployment from durable state
#
# WHAT IT DOES NOT PROVE
#   The deploy-smoke bundle selects the device-neutral `fixture` backend
#   profile. It exercises admission, worker supervision and the sidecar
#   inference path against the real installed appliance -- it does NOT
#   execute a CUDA kernel, and a passing run is not evidence that the
#   accelerator computed anything. There is no CUDA fixture backend to
#   select yet. Do not describe a run of this harness as GPU validation.
#
# Four stages are skipped, each with its reason recorded in the report
# rather than omitted: upgrade and rollback have no published amd64
# predecessor to move between, and crash-loop and offline need mechanism
# that has no precedent in this repository and is being added
# separately.
#
# Usage:
#   tools/validation/ubuntu-l4-cloud-lifecycle.sh \
#     --assets-dir <candidate artifacts> \
#     --tested-version <X.Y.Z> \
#     --evidence-dir <new or empty dir> \
#     --confirm RESET-TENSORPLATE

# Stage steps that redirect output are handed to `bash -c` so the
# redirection belongs to the command whose status is being captured,
# rather than to `step` itself. Those bodies are single-quoted on
# purpose: the child shell expands the positional parameters.
# shellcheck disable=SC2016

set -Eeuo pipefail

readonly DEFAULT_ROW="ubuntu2404-x86-l4-g2s8"
readonly CONFIRM_TOKEN="RESET-TENSORPLATE"
readonly AGENT_UNIT="tensorplate-agent"
readonly OBSERVABILITY_UNIT="tensorplate-observability"
# Overridable so the stage bodies can be driven against a stubbed
# appliance in CI. A validation harness whose stages only ever run on a
# machine CI cannot reach is a harness whose assertions nobody has seen
# fire, which is how a stage that certifies failures as passes ships.
AGENT_SOCKET_PATH="${TP_CLOUD_AGENT_SOCKET:-/run/tensorplate/agent.sock}"
LOG_DIR="${TP_CLOUD_LOG_DIR:-/var/log/tensorplate}"
BUNDLE_STAGING_DIR="${TP_CLOUD_BUNDLE_STAGING:-/var/tmp/tensorplate-cloud-lifecycle-bundle}"
# RestartSec is 5 in the shipped unit, so every readiness wait has to sit
# well above it rather than racing a restart.
readonly READY_TIMEOUT_SECONDS=60

ROW="$DEFAULT_ROW"
ASSETS_DIR=""
BUNDLE_DIR=""
EVIDENCE_DIR=""
TESTED_VERSION=""
DEPLOYMENT_ID="cloud-lifecycle-smoke"
CONFIRM_VALUE=""
ALLOW_UNSIGNED=0
PREFLIGHT_ONLY=0

# Host facts, read through overridable paths so the eligibility rules can
# be exercised off a cloud VM. The same seams the release installer uses,
# for the same reason: these branches decide whether a run may start, and
# a rule nobody can test is a rule nobody can trust.
HOST_ARCH="${TP_CLOUD_ARCH:-$(uname -m)}"
OS_RELEASE_FILE="${TP_CLOUD_OS_RELEASE:-/etc/os-release}"
NVIDIA_VERSION_FILE="${TP_CLOUD_NVIDIA_VERSION:-/proc/driver/nvidia/version}"
BACKEND_PYTHON="${TP_CLOUD_PYTHON:-/usr/bin/python3}"

usage() {
  cat <<EOF
Usage:
  ubuntu-l4-cloud-lifecycle.sh [options] --confirm ${CONFIRM_TOKEN}

Validates a candidate artifact set on an Ubuntu 24.04 x86_64 host with an
NVIDIA accelerator, and writes a canonical lifecycle report. Purges
TensorPlate packages and state, so it refuses to start without the
confirmation token. Run it on a disposable VM: it does not restore what
it removes.

Options:
  --assets-dir DIR       Candidate artifact set: install.sh, the artifact
                         manifest, SHA256SUMS, and the .deb packages. Required.
  --tested-version X.Y.Z The release this run authorizes. Bare version, never
                         a candidate spelling. Required.
  --evidence-dir DIR     Where the report and stage logs are written. Must be
                         new or empty. Required.
  --row ROW_ID           Support row under test. Default: ${DEFAULT_ROW}
  --bundle-dir DIR       deploy-smoke bundle. Default: the repository's
                         test/models/bundles/v0_1/x86_fixture_smoke
  --deployment-id ID     Deployment id for the smoke. Default: ${DEPLOYMENT_ID}
  --allow-unsigned       Pass --allow-unsigned to the installer, for a
                         candidate build with no published signature.
  --preflight-only       Check host eligibility and the inputs, then stop
                         without installing anything or writing a report.
  --confirm TOKEN        Required. Must equal ${CONFIRM_TOKEN}.
  --help                 Show this help text.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() { printf '==> %s\n' "$*"; }
pass() { printf 'PASS: %s\n' "$*"; }

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

# Read one os-release field without sourcing the file: it is host data,
# and sourcing would execute whatever it contains.
read_os_release_field() {
  awk -F= -v key="$1" '
    $1 == key {
      value = $2
      gsub(/^"/, "", value)
      gsub(/"$/, "", value)
      print value
      exit
    }
  ' "$OS_RELEASE_FILE"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --assets-dir) ASSETS_DIR="${2:-}"; shift 2 ;;
      --tested-version) TESTED_VERSION="${2:-}"; shift 2 ;;
      --evidence-dir) EVIDENCE_DIR="${2:-}"; shift 2 ;;
      --row) ROW="${2:-}"; shift 2 ;;
      --bundle-dir) BUNDLE_DIR="${2:-}"; shift 2 ;;
      --deployment-id) DEPLOYMENT_ID="${2:-}"; shift 2 ;;
      --allow-unsigned) ALLOW_UNSIGNED=1; shift ;;
      --preflight-only) PREFLIGHT_ONLY=1; shift ;;
      --confirm) CONFIRM_VALUE="${2:-}"; shift 2 ;;
      --help|-h) usage; exit 0 ;;
      *) usage >&2; die "unknown option: $1" ;;
    esac
  done
}

# Everything that must be true before the run starts.
#
# These are checked before lifecycle_begin, so a host that was never
# eligible produces a refusal and no report at all, rather than a report
# whose install stage failed for a reason that is not about the release.
preflight() {
  [[ "$CONFIRM_VALUE" == "$CONFIRM_TOKEN" ]] ||
    die "this purges TensorPlate packages and state; re-run with --confirm ${CONFIRM_TOKEN}"
  [[ -n "$ASSETS_DIR" && -d "$ASSETS_DIR" ]] || die "--assets-dir must name a directory"
  [[ -n "$EVIDENCE_DIR" ]] || die "--evidence-dir is required"
  [[ -n "$TESTED_VERSION" ]] || die "--tested-version is required"
  [[ "$TESTED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "--tested-version must be a bare X.Y.Z release version, not a candidate spelling"
  [[ ! -e "$EVIDENCE_DIR" || -z "$(find "$EVIDENCE_DIR" -mindepth 1 -print -quit)" ]] ||
    die "--evidence-dir must be new or empty: ${EVIDENCE_DIR}"

  [[ "${EUID}" -ne 0 ]] ||
    die "run as a normal user; this script calls sudo for privileged steps"
  require_command sudo
  require_command systemctl
  require_command python3
  require_command sha256sum
  require_command dpkg

  [[ "$HOST_ARCH" == "x86_64" ]] ||
    die "this harness validates x86_64 rows; host reports ${HOST_ARCH}"

  local os_id os_version
  [[ -r "$OS_RELEASE_FILE" ]] || die "cannot read ${OS_RELEASE_FILE}"
  os_id="$(read_os_release_field ID)"
  os_version="$(read_os_release_field VERSION_ID)"
  [[ "$os_id" == "ubuntu" && "$os_version" == 24.04* ]] ||
    die "expected Ubuntu 24.04; host reports ID=${os_id:-unknown} VERSION_ID=${os_version:-unknown}"

  # The rows this harness serves are all NVIDIA hosts. A missing driver
  # is fatal here rather than advisory as it is in the installer: a run
  # that cannot see the accelerator cannot produce evidence for a row
  # whose whole subject is that accelerator.
  [[ -r "$NVIDIA_VERSION_FILE" ]] ||
    die "no NVIDIA driver at ${NVIDIA_VERSION_FILE}; install the driver before validating"

  [[ -f "${ASSETS_DIR}/install.sh" ]] || die "missing ${ASSETS_DIR}/install.sh"
  [[ -f "${ASSETS_DIR}/SHA256SUMS" ]] || die "missing ${ASSETS_DIR}/SHA256SUMS"

  if [[ -z "$BUNDLE_DIR" ]]; then
    local repo_root default_bundle
    repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    default_bundle="${repo_root}/test/models/bundles/v0_1/x86_fixture_smoke"
    if [[ -d "$default_bundle" ]]; then
      BUNDLE_DIR="$default_bundle"
    fi
  fi
  [[ -n "$BUNDLE_DIR" && -f "${BUNDLE_DIR}/manifest.json" ]] ||
    die "--bundle-dir must contain manifest.json"

  # The installer runs doctor at the end of a runtime install and dies on
  # a critical finding. python_pytorch_runtime is probed unconditionally
  # on Linux, so a host without PyTorch fails the install stage for a
  # missing prerequisite rather than for anything about the release.
  # Ubuntu 24.04 marks its system interpreter externally-managed, so the
  # remedy is spelled out rather than left to the operator to discover.
  "$BACKEND_PYTHON" -c 'import torch' >/dev/null 2>&1 || die "$(cat <<'MSG'
PyTorch is not importable by the backend descriptor's interpreter, which
the installer's own doctor run probes unconditionally on Linux. It
refuses a critical finding, so install PyTorch before validating. Ubuntu
24.04 marks its system interpreter externally-managed; see
docs/validation/cloud-row-runbooks.md for the supported way to satisfy
this on a validation host.
MSG
)"
  pass "host is eligible: Ubuntu ${os_version} on ${HOST_ARCH}, NVIDIA driver present, PyTorch importable"
}

# Run one command inside a stage, capturing its status explicitly.
#
# lifecycle_stage invokes the stage function from a tested context, which
# makes bash suspend errexit for that function's whole body -- the hazard
# the runner's own header documents about itself. A stage written to rely
# on ambient `set -e` runs on past its first failure and returns the
# status of its last command, so a failed run certifies itself as a pass.
# Every command in a stage that can fail goes through here or carries its
# own `|| return`.
step() {
  local description="$1"
  shift
  local status=0
  "$@" || status=$?
  if ((status != 0)); then
    printf 'step failed (exit %s): %s\n' "$status" "$description" >&2
    return "$status"
  fi
}

# The digest that identifies the artifacts under test, taken before
# anything is installed. SHA256SUMS is the file hashed: no run installs a
# single package, and this is the file the install itself verifies
# against. Hashed from inside the assets directory so the recorded name
# is the bare file name rather than a path off this machine.
#
# The value is held in memory rather than written beside the evidence
# here: lifecycle_begin clears that sidecar so a retry cannot inherit a
# previous attempt's digest, and it is lifecycle_artifact_digest, after
# the install stage passes, that writes the run's own.
ARTIFACT_DIGEST=""

record_assets() {
  note "verifying the candidate artifact set"
  ( cd "$ASSETS_DIR" && sha256sum -c SHA256SUMS ) >"${EVIDENCE_DIR}/checksums.txt" 2>&1 ||
    { cat "${EVIDENCE_DIR}/checksums.txt" >&2; die "the candidate artifact set failed verification"; }
  ARTIFACT_DIGEST="$( cd "$ASSETS_DIR" && sha256sum SHA256SUMS | awk '{print $1}' )"
  [[ "$ARTIFACT_DIGEST" =~ ^[0-9a-f]{64}$ ]] ||
    die "could not compute a digest for ${ASSETS_DIR}/SHA256SUMS"
  pass "artifact set verified: ${ARTIFACT_DIGEST}"
}

await_unit_active() {
  local unit="$1" deadline=$((SECONDS + READY_TIMEOUT_SECONDS))
  while ((SECONDS <= deadline)); do
    [[ "$(systemctl show -p ActiveState --value "$unit")" == "active" ]] && return 0
    sleep 1
  done
  systemctl status "$unit" --no-pager >&2 || true
  return 1
}

await_agent_socket() {
  local deadline=$((SECONDS + READY_TIMEOUT_SECONDS))
  while ((SECONDS <= deadline)); do
    [[ -S "$AGENT_SOCKET_PATH" ]] && return 0
    sleep 1
  done
  journalctl -u "$AGENT_UNIT" --no-pager -n 40 >&2 || true
  return 1
}

await_services_ready() {
  await_unit_active "$AGENT_UNIT" || return 1
  await_unit_active "$OBSERVABILITY_UNIT" || return 1
  await_agent_socket || return 1
}

# --- install -----------------------------------------------------------

stage_install() {
  note "clearing any previous TensorPlate install"
  sudo systemctl stop "$AGENT_UNIT" "$OBSERVABILITY_UNIT" >/dev/null 2>&1 || true
  sudo apt-get purge -y \
    tensorplate \
    tensorplate-agent \
    tensorplate-serving \
    tensorplate-observability \
    tensorplate-cli \
    tensorplate-common \
    tensorplate-backend-python-pytorch >/dev/null 2>&1 || true
  step "clear installed state" sudo rm -rf \
    /etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate || return

  note "installing the candidate through the shipped installer"
  local flags=(--local-artifacts "$ASSETS_DIR" --yes --with-python-backend)
  ((ALLOW_UNSIGNED)) && flags+=(--allow-unsigned)
  step "install.sh" sudo bash "${ASSETS_DIR}/install.sh" "${flags[@]}" || return

  note "enabling the services"
  step "enable ${AGENT_UNIT}" sudo systemctl enable --now "$AGENT_UNIT" || return
  step "enable ${OBSERVABILITY_UNIT}" sudo systemctl enable --now "$OBSERVABILITY_UNIT" || return
  step "services ready" await_services_ready || return

  dpkg -l 'tensorplate*' >"${EVIDENCE_DIR}/packages.txt" 2>&1 || true
  step "doctor" bash -c \
    'tensorplate doctor --output json >"$1"' _ "${EVIDENCE_DIR}/doctor.json" || return
  python3 - "${EVIDENCE_DIR}/doctor.json" "$ROW" <<'PY' || return
import json, sys

path, expected_row = sys.argv[1:]
payload = json.load(open(path, encoding="utf-8"))["payload"]
by_id = {f["id"]: f for f in payload["findings"]}

assert payload["failing"] == 0, f"doctor reports {payload['failing']} failing finding(s)"

# platform_row is the finding that resolves host identity AND accelerator
# to one row; platform_profile answers from host identity alone and is
# deliberately a candidate set, so naming the row there proves only that
# the row is among the candidates. Both are asserted, because
# platform_row's refusals are Warnings rather than Fails and so are
# invisible to `failing == 0`.
row = by_id["platform_row"]
assert row["status"] == "ok", f"platform_row is {row['status']}: {row['message']}"
assert expected_row in row["message"], \
    f"platform_row did not name {expected_row}: {row['message']}"

profile = by_id["platform_profile"]
assert expected_row in profile["message"], \
    f"{expected_row} is not among the host's candidate rows: {profile['message']}"

for required_ok in ("platform_registry", "agent_reachable", "agent_socket",
                    "serving_binary_installed", "python_pytorch_backend",
                    "python_pytorch_runtime", "path_layout", "config_files"):
    finding = by_id[required_ok]
    assert finding["status"] == "ok", f"{required_ok} is {finding['status']}: {finding['message']}"

# The accelerator is the row's whole subject, so the run records what
# doctor saw rather than assuming it.
print(f"accelerator: {by_id['accelerator_facts']['message']}")
print(f"doctor: {len(payload['findings'])} findings, 0 failing, row {expected_row}")
PY
  pass "installed, services ready, doctor green, ${ROW} resolved by platform_row"
}

# --- deploy-smoke ------------------------------------------------------

stage_deploy_smoke() {
  local work deploy_output status_output infer_input infer_output staged_bundle
  work="$(mktemp -d)"
  deploy_output="${work}/deploy.json"
  status_output="${work}/status.json"
  infer_input="${work}/sample_infer.json"
  infer_output="${work}/infer-response.json"

  note "validating the deploy-smoke bundle before deploying it"
  python3 - "$BUNDLE_DIR" <<'PY' || return
import hashlib, json, pathlib, sys

bundle = pathlib.Path(sys.argv[1]).resolve()
manifest = json.loads((bundle / "manifest.json").read_text(encoding="utf-8"))
if manifest.get("backend_hint") != "python_pytorch":
    raise SystemExit("deploy-smoke bundle must declare backend_hint=python_pytorch")
models = [a for a in manifest.get("artifacts", [])
          if isinstance(a, dict) and a.get("role") == "model"]
if len(models) != 1:
    raise SystemExit("deploy-smoke bundle must declare exactly one model artifact")
artifact = models[0]
path = (bundle / artifact["path"]).resolve()
if bundle not in path.parents:
    raise SystemExit("deploy-smoke model artifact escapes the bundle root")
digest = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
if digest != artifact.get("digest"):
    raise SystemExit("deploy-smoke model artifact digest does not match its manifest")
config = json.loads(path.read_text(encoding="utf-8"))
if config.get("backend_profile") != "fixture":
    raise SystemExit("deploy-smoke config must select the device-neutral fixture profile")
print(json.dumps({"bundle": manifest.get("name"),
                  "backend_profile": config["backend_profile"]}, sort_keys=True))
PY

  # The agent opens the bundle itself, as the tensorplate user, from the
  # path the CLI sends -- the CLI does not upload it. A checkout under
  # the operator's home is not readable by that user, so the bundle is
  # staged somewhere it can be read before the path is handed over.
  staged_bundle="$BUNDLE_STAGING_DIR"
  note "staging the bundle at ${staged_bundle} for the agent to read"
  step "stage the bundle" sudo rm -rf "$staged_bundle" || return
  step "copy the bundle" sudo cp -R "$BUNDLE_DIR" "$staged_bundle" || return
  step "make the bundle readable" sudo chmod -R a+rX "$staged_bundle" || return

  note "deploying"
  step "deploy" bash -c \
    'tensorplate deploy "$1" --deployment-id "$2" --output json >"$3"' \
    _ "$staged_bundle" "$DEPLOYMENT_ID" "$deploy_output" || return

  note "issuing an inference request"
  python3 - "$infer_input" <<'PY' || return
import base64, json, struct, sys

payload = struct.pack("<4f", 1.0, 2.0, 3.0, 4.0)
request = {
    "schema_version": "0.1",
    "request_id": "cloud-lifecycle-smoke-1",
    "endpoint": "cloud-lifecycle-smoke",
    "inputs": [
        {
            "name": "probe",
            "tensor": {"dtype": "float32", "layout": "row_major", "shape": [1, 4]},
            "payload_b64": base64.b64encode(payload).decode("ascii"),
        }
    ],
}
open(sys.argv[1], "w", encoding="utf-8").write(json.dumps(request, indent=2) + "\n")
PY
  step "infer" tensorplate infer --input "$infer_input" --output-file "$infer_output" || return
  step "status" bash -c \
    'tensorplate status --output json >"$1"' _ "$status_output" || return

  python3 - "$deploy_output" "$status_output" "$infer_input" "$infer_output" "$DEPLOYMENT_ID" \
    >"${EVIDENCE_DIR}/deploy-result.json" <<'PY' || return
import json, sys, urllib.parse, urllib.request

deploy_path, status_path, input_path, response_path, expected = sys.argv[1:]
deploy = json.load(open(deploy_path, encoding="utf-8"))["payload"]
status = json.load(open(status_path, encoding="utf-8"))["payload"]
request = json.load(open(input_path, encoding="utf-8"))
response = json.load(open(response_path, encoding="utf-8"))
if "payload" in response:
    response = response["payload"]

agent = status.get("agent") or {}
active = agent.get("active") or {}
supervision = agent.get("supervision")
serving_url = active.get("serving_url")
parts = urllib.parse.urlsplit(serving_url) if isinstance(serving_url, str) else None
serving_url_valid = (
    parts is not None
    and parts.scheme == "http"
    and parts.hostname == "127.0.0.1"
    and parts.path == "/infer"
)
health = {}
if serving_url_valid:
    health_url = urllib.parse.urlunsplit((parts.scheme, parts.netloc, "/health", "", ""))
    try:
        with urllib.request.urlopen(health_url, timeout=5) as handle:
            health = json.load(handle)
    except (OSError, ValueError, json.JSONDecodeError):
        health = {}

# The fixture backend echoes each input as echo_<name>, preserving the
# tensor metadata. That is what makes this a round trip through the
# worker rather than a control-plane answer about one.
outputs = {o.get("name"): o for o in response.get("outputs", []) if isinstance(o, dict)}
echoed = outputs.get("echo_probe") or {}
sent = request["inputs"][0]
# The worker renders a tensor with byte_offset and byte_size alongside
# the three fields a request declares, so this compares the declared
# fields rather than the whole object. Requiring equality would fail on
# every response the serving layer can produce.
echoed_tensor = echoed.get("tensor") or {}
tensor_echoed = all(echoed_tensor.get(key) == value for key, value in sent["tensor"].items())
supervision_healthy = (
    supervision is None
    or (isinstance(supervision, dict)
        and supervision.get("serving_state") == "ready"
        and supervision.get("crash_loop") is False)
)
checks = {
    "deployment_phase": deploy.get("phase") == "active",
    "deployment_id": deploy.get("deployment_id") == expected,
    "status_severity": status.get("severity") == "ready",
    "agent_state": agent.get("agent_state") == "ready",
    "active_deployment": active.get("deployment_id") == expected,
    "active_backend": active.get("backend") == "python_pytorch",
    "active_serving_url": serving_url_valid,
    "serving_health_state": health.get("state") == "ready",
    "serving_health_deployment": health.get("active_model_id") == expected,
    "supervision_healthy_when_configured": supervision_healthy,
    "inference_echoed_the_input": tensor_echoed,
    "inference_preserved_the_payload": echoed.get("payload_b64") == sent["payload_b64"],
}
failed = [name for name, ok in checks.items() if not ok]
if failed:
    raise SystemExit("deploy-smoke checks failed: " + ", ".join(failed))
print(json.dumps({
    "deployment_id": expected,
    "deployment_phase": "active",
    "status_severity": "ready",
    "agent_state": "ready",
    "serving_endpoint": "ready",
    "backend": "python_pytorch",
    "backend_profile": "fixture",
    "supervision_state": "not_configured" if supervision is None else supervision["serving_state"],
    "inference_round_trip": "echoed",
    "accelerator_kernel_executed": False,
}, indent=2, sort_keys=True))
PY
  rm -rf "$work"
  pass "bundle admitted, worker supervised, inference round-tripped"
}

# --- status-logs -------------------------------------------------------

stage_status_logs() {
  local logs_status=0
  note "querying the control plane"
  step "status" bash -c \
    'tensorplate status --output json >"$1"' _ "${EVIDENCE_DIR}/status.json" || return
  python3 - "${EVIDENCE_DIR}/status.json" "$DEPLOYMENT_ID" <<'PY' || return
import json, sys

path, expected = sys.argv[1:]
document = json.load(open(path, encoding="utf-8"))
assert document.get("command") == "status", document
payload = document["payload"]
assert payload.get("severity") == "ready", payload.get("severity")
active = (payload.get("agent") or {}).get("active") or {}
assert active.get("deployment_id") == expected, \
    f"status no longer reports {expected} as active: {active.get('deployment_id')}"
print("status: control plane answered and still reports the smoke deployment")
PY

  # The operator-facing log command is recorded, not required.
  #
  # The packaged cli.json points log_source at
  # /var/log/tensorplate/tensorplate-agent.log, and nothing in the
  # product writes that file on a Linux package install: both units log
  # to stderr, which systemd routes to the journal. So the command is
  # expected to fail here, and its status is filed as evidence of that
  # gap rather than failing a stage for something the release did not
  # cause. What this stage requires instead is the log path a packaged
  # Linux install actually has.
  note "recording the operator-facing log command"
  tensorplate logs --component agent --tail 100 \
    >"${EVIDENCE_DIR}/agent-cli.log" 2>&1 || logs_status=$?
  printf '%s\n' "$logs_status" >"${EVIDENCE_DIR}/logs-command.exit"
  if ((logs_status != 0)); then
    note "tensorplate logs exited ${logs_status}: no component writes ${LOG_DIR}/tensorplate-agent.log on a packaged Linux install"
  fi

  # Bounded journal captures. These carry the instance's host name, so
  # they go to named side files and are sanitized before anything is
  # committed.
  step "capture the agent journal" bash -c \
    'journalctl -u "$1" -n 100 --no-pager >"$2" 2>&1' \
    _ "$AGENT_UNIT" "${EVIDENCE_DIR}/agent-journal.txt" || return
  step "capture the observability journal" bash -c \
    'journalctl -u "$1" -n 100 --no-pager >"$2" 2>&1' \
    _ "$OBSERVABILITY_UNIT" "${EVIDENCE_DIR}/observability-journal.txt" || return
  [[ -s "${EVIDENCE_DIR}/agent-journal.txt" ]] ||
    { printf 'the agent journal is empty; the service logged nothing\n' >&2; return 1; }
  [[ -d "$LOG_DIR" ]] ||
    { printf 'log directory missing at %s\n' "$LOG_DIR" >&2; return 1; }
  pass "status answered and still reports the deployment; journal captured; log command status recorded"
}

# --- restart -----------------------------------------------------------

stage_restart() {
  local before_agent before_observability after_agent after_observability
  before_agent="$(systemctl show -p MainPID --value "$AGENT_UNIT")"
  before_observability="$(systemctl show -p MainPID --value "$OBSERVABILITY_UNIT")"
  note "restarting both services (agent pid ${before_agent}, observability pid ${before_observability})"

  step "restart both units" sudo systemctl restart "$AGENT_UNIT" "$OBSERVABILITY_UNIT" || return
  step "services ready again" await_services_ready || return

  after_agent="$(systemctl show -p MainPID --value "$AGENT_UNIT")"
  after_observability="$(systemctl show -p MainPID --value "$OBSERVABILITY_UNIT")"
  [[ "$after_agent" != "$before_agent" ]] ||
    { printf 'agent MainPID did not change across the restart\n' >&2; return 1; }
  [[ "$after_observability" != "$before_observability" ]] ||
    { printf 'observability MainPID did not change across the restart\n' >&2; return 1; }

  # The substance of this stage: not that a process came back, but that
  # the agent re-warmed the deployment from durable state.
  step "status after restart" bash -c \
    'tensorplate status --output json >"$1"' _ "${EVIDENCE_DIR}/status-after-restart.json" || return
  python3 - "${EVIDENCE_DIR}/status-after-restart.json" "$DEPLOYMENT_ID" <<'PY' || return
import json, sys

path, expected = sys.argv[1:]
payload = json.load(open(path, encoding="utf-8"))["payload"]
active = (payload.get("agent") or {}).get("active") or {}
assert active.get("deployment_id") == expected, \
    f"the agent did not re-warm {expected} after restart: {active.get('deployment_id')}"
assert payload.get("severity") == "ready", payload.get("severity")
print("restart: both services replaced their processes and the deployment came back")
PY
  pass "services restarted with new pids; deployment re-warmed from durable state"
}

# --- run ---------------------------------------------------------------

main() {
  parse_args "$@"
  preflight
  if ((PREFLIGHT_ONLY)); then
    # Nothing has been installed, removed or written at this point, so a
    # preflight run leaves the host exactly as it found it and files no
    # evidence for a run that did not happen.
    pass "preflight only; no install attempted and no report written"
    exit 0
  fi
  mkdir -p "$EVIDENCE_DIR"
  EVIDENCE_DIR="$(cd "$EVIDENCE_DIR" && pwd)"
  ASSETS_DIR="$(cd "$ASSETS_DIR" && pwd)"
  BUNDLE_DIR="$(cd "$BUNDLE_DIR" && pwd)"

  record_assets

  # shellcheck source=tools/validation/lifecycle-stages.sh disable=SC1091
  source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lifecycle-stages.sh"
  lifecycle_begin "$ROW" "$EVIDENCE_DIR" "$TESTED_VERSION" ubuntu-l4-cloud-lifecycle
  trap 'lifecycle_abort $?' EXIT

  lifecycle_stage install stage_install
  # Recorded after the install stage passes, so the digest attests an
  # install that happened. The value comes from memory: lifecycle_begin
  # has already cleared this directory's sidecar, and writing it is this
  # call's job.
  lifecycle_artifact_digest "$ARTIFACT_DIGEST" SHA256SUMS

  lifecycle_stage deploy-smoke stage_deploy_smoke
  lifecycle_stage status-logs stage_status_logs
  lifecycle_stage restart stage_restart

  lifecycle_skip upgrade \
    "no published amd64 predecessor exists for this row: no released tag carries an amd64 runtime package set, so there is nothing to upgrade from"
  lifecycle_skip rollback \
    "no published amd64 predecessor exists for this row, so there is no released version to roll back to"
  lifecycle_skip crash-loop \
    "the restart-settling check has only ever driven stub binaries; exercising it against the real agent is a separate change"
  lifecycle_skip offline \
    "network-denied validation has no precedent on Linux in this repository and is a separate change"

  lifecycle_finish
  printf 'evidence: %s\n' "$EVIDENCE_DIR"
  pass "lifecycle run complete; four stages exercised, four skipped with reasons"
}

main "$@"
