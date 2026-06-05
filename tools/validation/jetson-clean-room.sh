#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Native Jetson clean-room validation helper.
#
# This intentionally validates on the Jetson host instead of in a VM or
# container. TensorRT/CUDA behavior is release-critical for TensorPlate, and a
# sandboxed package smoke is not equivalent to host GPU/runtime validation.

set -Eeuo pipefail

readonly DEFAULT_REPO="tensorplate/tensorplate"
readonly DEFAULT_VERSION="0.1.0"
readonly CONFIRM_TOKEN="RESET-TENSORPLATE"

COMMAND="${1:-}"
if [[ -n "$COMMAND" ]]; then
  shift || true
fi

VERSION="${TP_VERSION:-$DEFAULT_VERSION}"
TAG=""
REPO="${TP_REPO:-$DEFAULT_REPO}"
WORK_DIR="${TP_CLEAN_ROOM_WORK_DIR:-}"
ASSETS_DIR="${TP_CLEAN_ROOM_ASSETS_DIR:-}"
EVIDENCE_DIR="${TP_CLEAN_ROOM_EVIDENCE_DIR:-}"
RUN_TMP_DIR="${TP_CLEAN_ROOM_TMP_DIR:-}"
BUNDLE_DIR="${TP_CLEAN_ROOM_BUNDLE_DIR:-/var/lib/tensorplate/validation/tensorplate-trt-identity-bundle}"
DEPLOYMENT_ID="${TP_CLEAN_ROOM_DEPLOYMENT_ID:-release-clean-room}"
AGENT_SOCKET_PATH="${TP_CLEAN_ROOM_AGENT_SOCKET_PATH:-/run/tensorplate/agent.sock}"
SERVICE_READY_TIMEOUT_SECONDS="${TP_CLEAN_ROOM_SERVICE_READY_TIMEOUT_SECONDS:-30}"
WITH_PYTHON_BACKEND=0
ALLOW_UNSIGNED=0
CONFIRM_VALUE=""

usage() {
  cat <<EOF
Usage:
  jetson-clean-room.sh run [options] --confirm ${CONFIRM_TOKEN}
  jetson-clean-room.sh reset [options] --confirm ${CONFIRM_TOKEN}
  jetson-clean-room.sh download [options]

Runs native clean-room validation on a Jetson target. The run command purges
TensorPlate packages/state, installs from signed release assets or a supplied
artifact directory, starts services, deploys a target-generated TensorRT
identity bundle, runs inference, and writes bounded evidence.

Commands:
  run       Download/use assets, reset TensorPlate state, install, validate.
  reset     Stop services, purge TensorPlate packages, and clear state only.
  download  Download release assets into the work directory only.

Options:
  --version VERSION        Release version or tag. Default: ${DEFAULT_VERSION}
  --repo OWNER/REPO        GitHub release repository. Default: ${DEFAULT_REPO}
  --assets-dir DIR         Use existing local artifacts instead of downloading.
  --work-dir DIR           Working directory. Default: mktemp under /tmp.
  --evidence-dir DIR       Evidence directory. Default: WORK_DIR/evidence.
  --tmp-dir DIR            Temporary directory for validation subprocesses.
                            Default: WORK_DIR/tmp.
  --bundle-dir DIR         TensorRT validation bundle path.
                            Default: /var/lib/tensorplate/validation/tensorplate-trt-identity-bundle
  --deployment-id ID       Deployment id for validation. Default: release-clean-room.
  --with-python-backend    Install optional tensorplate-backend-python-pytorch.
  --allow-unsigned         Pass --allow-unsigned to install.sh for build-only
                            artifacts. Do not use for public release signoff.
  --confirm TOKEN          Required for run/reset. Must equal ${CONFIRM_TOKEN}.
  --help                   Show this help text.

Examples:
  tools/validation/jetson-clean-room.sh run \\
    --version 0.1.0 \\
    --with-python-backend \\
    --confirm ${CONFIRM_TOKEN}

  tools/validation/jetson-clean-room.sh run \\
    --assets-dir /tmp/tensorplate-v0.1.0-assets \\
    --with-python-backend \\
    --confirm ${CONFIRM_TOKEN}
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '==> %s\n' "$*"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

repo_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd
}

normalize_version() {
  if [[ "$VERSION" == v* ]]; then
    TAG="$VERSION"
    VERSION="${VERSION#v}"
  else
    TAG="v${VERSION}"
  fi
  [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[1-9][0-9]*)?$ ]] ||
    die "--version must be vX.Y.Z, X.Y.Z, or vX.Y.Z-rc.N"
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --version) VERSION="${2:-}"; shift 2 ;;
      --repo) REPO="${2:-}"; shift 2 ;;
      --assets-dir) ASSETS_DIR="${2:-}"; shift 2 ;;
      --work-dir) WORK_DIR="${2:-}"; shift 2 ;;
      --evidence-dir) EVIDENCE_DIR="${2:-}"; shift 2 ;;
      --tmp-dir) RUN_TMP_DIR="${2:-}"; shift 2 ;;
      --bundle-dir) BUNDLE_DIR="${2:-}"; shift 2 ;;
      --deployment-id) DEPLOYMENT_ID="${2:-}"; shift 2 ;;
      --with-python-backend) WITH_PYTHON_BACKEND=1; shift ;;
      --allow-unsigned) ALLOW_UNSIGNED=1; shift ;;
      --confirm) CONFIRM_VALUE="${2:-}"; shift 2 ;;
      --help|-h) usage; exit 0 ;;
      *) usage >&2; die "unknown option: $1" ;;
    esac
  done
}

require_confirm() {
  [[ "$CONFIRM_VALUE" == "$CONFIRM_TOKEN" ]] ||
    die "run/reset requires --confirm ${CONFIRM_TOKEN}; this purges TensorPlate packages and state"
}

prepare_paths() {
  normalize_version
  if [[ -z "$WORK_DIR" ]]; then
    WORK_DIR="$(mktemp -d -t "tensorplate-${TAG}-clean-room.XXXXXX")"
  fi
  mkdir -p "$WORK_DIR"
  WORK_DIR="$(cd "$WORK_DIR" && pwd)"

  if [[ -z "$RUN_TMP_DIR" ]]; then
    RUN_TMP_DIR="${WORK_DIR}/tmp"
  fi
  mkdir -p "$RUN_TMP_DIR"
  RUN_TMP_DIR="$(cd "$RUN_TMP_DIR" && pwd)"
  export TMPDIR="$RUN_TMP_DIR"

  if [[ -z "$ASSETS_DIR" ]]; then
    ASSETS_DIR="${WORK_DIR}/assets"
  fi
  mkdir -p "$ASSETS_DIR"
  ASSETS_DIR="$(cd "$ASSETS_DIR" && pwd)"

  if [[ -z "$EVIDENCE_DIR" ]]; then
    EVIDENCE_DIR="${WORK_DIR}/evidence"
  fi
  mkdir -p "$EVIDENCE_DIR"
  EVIDENCE_DIR="$(cd "$EVIDENCE_DIR" && pwd)"
}

check_host() {
  [[ "${EUID}" -ne 0 ]] ||
    die "run as a normal user; this script calls sudo for privileged steps"
  require_command sudo
  require_command systemctl
  require_command python3
  require_command sha256sum
  require_command tar
  require_command dpkg

  local arch
  arch="$(dpkg --print-architecture 2>/dev/null || true)"
  [[ "$arch" == "arm64" ]] ||
    die "native clean-room validation must run on arm64 Jetson host; got architecture ${arch:-unknown}"

  sudo -v
}

capture() {
  local name="$1"
  shift
  set +e
  "$@" >"${EVIDENCE_DIR}/${name}.stdout" 2>"${EVIDENCE_DIR}/${name}.stderr"
  local rc=$?
  set -e
  printf '%s\n' "$rc" >"${EVIDENCE_DIR}/${name}.exit"
  return "$rc"
}

required() {
  local name="$1"
  shift
  capture "$name" "$@" || {
    local rc
    rc="$(<"${EVIDENCE_DIR}/${name}.exit")"
    printf 'required step failed: %s\n' "$name" >&2
    cat "${EVIDENCE_DIR}/${name}.stderr" >&2 || true
    exit "$rc"
  }
}

optional() {
  local name="$1"
  shift
  capture "$name" "$@" || true
}

wait_for_services_ready() {
  local deadline=$((SECONDS + SERVICE_READY_TIMEOUT_SECONDS))

  while ((SECONDS <= deadline)); do
    if systemctl is-active --quiet tensorplate-agent &&
      systemctl is-active --quiet tensorplate-observability &&
      [[ -S "$AGENT_SOCKET_PATH" ]]; then
      return 0
    fi
    sleep 1
  done

  systemctl --no-pager --full status tensorplate-agent tensorplate-observability >&2 || true
  printf 'TensorPlate services did not become ready within %ss\n' "$SERVICE_READY_TIMEOUT_SECONDS" >&2
  return 1
}

download_one() {
  local release_url="$1"
  local name="$2"
  if [[ -f "${ASSETS_DIR}/${name}" ]]; then
    return
  fi
  note "downloading ${name}"
  curl -fL -o "${ASSETS_DIR}/${name}" "${release_url}/${name}"
}

download_release_assets() {
  require_command curl
  local release_url="https://github.com/${REPO}/releases/download/${TAG}"
  local manifest="tensorplate-${TAG}-artifacts.json"

  note "downloading release assets from ${release_url}"
  download_one "$release_url" install.sh
  download_one "$release_url" SHA256SUMS
  download_one "$release_url" SHA256SUMS.cosign.bundle
  download_one "$release_url" "$manifest"

  python3 - "$ASSETS_DIR/$manifest" <<'PY' |
import json
import pathlib
import sys

manifest = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
for artifact in manifest.get("artifacts", []):
    file_name = artifact.get("file")
    if file_name:
        print(file_name)
PY
  while IFS= read -r file_name; do
    download_one "$release_url" "$file_name"
  done
}

verify_assets() {
  required artifacts-list bash -c 'cd "$1" && find . -maxdepth 1 -type f -print | sort' _ "$ASSETS_DIR"
  required checksums bash -c 'cd "$1" && sha256sum -c SHA256SUMS' _ "$ASSETS_DIR"
}

reset_tensorplate() {
  note "stopping TensorPlate services and clearing installed TensorPlate state"
  optional stop-services sudo systemctl stop tensorplate-agent tensorplate-observability
  optional purge-packages sudo apt purge -y \
    tensorplate-agent \
    tensorplate-serving \
    tensorplate-observability \
    tensorplate-cli \
    tensorplate-common \
    tensorplate-backend-python-pytorch
  required clear-state sudo rm -rf \
    /etc/tensorplate \
    /var/lib/tensorplate \
    /var/log/tensorplate \
    /run/tensorplate
  optional packages-after-purge dpkg-query -W -f='${binary:Package} ${Version}\n' 'tensorplate-*'
}

install_tensorplate() {
  [[ -f "${ASSETS_DIR}/install.sh" ]] || die "missing ${ASSETS_DIR}/install.sh"
  local install_flags=(--local-artifacts "$ASSETS_DIR" --yes)
  if [[ "$WITH_PYTHON_BACKEND" -eq 1 ]]; then
    install_flags+=(--with-python-backend)
  fi
  if [[ "$ALLOW_UNSIGNED" -eq 1 ]]; then
    install_flags+=(--allow-unsigned)
  fi

  required install-dry-run sudo bash "${ASSETS_DIR}/install.sh" "${install_flags[@]}" --dry-run
  required install sudo bash "${ASSETS_DIR}/install.sh" "${install_flags[@]}"
  required packages-after-install dpkg-query -W -f='${binary:Package} ${Version}\n' 'tensorplate-*'
}

record_environment() {
  required environment bash -c '
    set -e
    hostnamectl || true
    uname -a
    . /etc/os-release && printf "Ubuntu: %s\n" "$PRETTY_NAME"
    cat /etc/nv_tegra_release 2>/dev/null || true
    dpkg --print-architecture
    python3 --version
    python3 - <<'"'"'PY'"'"' || true
try:
    import torch
    print(f"PyTorch: {torch.__version__}")
except Exception as exc:
    print(f"PyTorch: not importable ({exc})")
PY
  '
  optional tensorrt-packages dpkg-query -W -f='${binary:Package} ${Version}\n' 'libnvinfer*' 'tensorrt*'
}

validate_runtime() {
  local root
  root="$(repo_root)"

  required version tensorplate --version
  required start-services sudo systemctl enable --now tensorplate-agent tensorplate-observability
  required services-ready wait_for_services_ready
  required doctor-after tensorplate doctor --output json
  required status-before-deploy tensorplate status --output json

  required prepare-validation-dir sudo mkdir -p "$(dirname "$BUNDLE_DIR")"
  required own-validation-dir sudo chown "${USER}:${USER}" "$(dirname "$BUNDLE_DIR")"
  required create-trt-identity-bundle bash "${root}/tools/validation/create_trt_identity_bundle.sh" "$BUNDLE_DIR"
  required bundle-list bash -c 'find "$1" -maxdepth 1 -type f -print | sort' _ "$BUNDLE_DIR"
  required deploy-trt-identity tensorplate deploy "$BUNDLE_DIR" --deployment-id "$DEPLOYMENT_ID" --output json
  required infer-trt-identity tensorplate infer \
    --input "${BUNDLE_DIR}/sample_infer.json" \
    --output-file "${EVIDENCE_DIR}/infer-response.json" \
    --output json
  required verify-trt-identity python3 "${root}/tools/validation/verify_trt_identity_response.py" "${EVIDENCE_DIR}/infer-response.json"
  required status-after-infer tensorplate status --output json
  required status-after-infer-ready python3 -c '
import json
import sys
doc = json.load(open(sys.argv[1], encoding="utf-8"))
agent = doc.get("payload", {}).get("agent", {})
active = agent.get("active") or {}
checks = {
    "status": doc.get("status") == "ok",
    "severity": doc.get("payload", {}).get("severity") == "ready",
    "agent_state": agent.get("agent_state") == "ready",
    "deployment_id": active.get("deployment_id") == sys.argv[2],
    "backend": active.get("backend") == "tensorrt",
}
for name, passed in checks.items():
    print(f"{name}={passed}")
if not all(checks.values()):
    sys.exit(1)
' "${EVIDENCE_DIR}/status-after-infer.stdout" "$DEPLOYMENT_ID"
  required doctor-final tensorplate doctor --output json
  required doctor-final-failing python3 -c '
import json
import sys
doc = json.load(open(sys.argv[1], encoding="utf-8"))
failing = doc.get("payload", {}).get("failing")
print(f"failing={failing}")
sys.exit(0 if failing == 0 else 1)
' "${EVIDENCE_DIR}/doctor-final.stdout"

  optional logs-agent tensorplate logs --component agent --tail 100
  optional logs-observability tensorplate logs --component observability --tail 100
  optional journal-agent sudo journalctl -u tensorplate-agent -n 100 --no-pager
  optional journal-observability sudo journalctl -u tensorplate-observability -n 100 --no-pager
  optional systemctl-agent systemctl status tensorplate-agent --no-pager
  optional systemctl-observability systemctl status tensorplate-observability --no-pager
}

write_summary() {
  local decision="pass"
  local known_gaps=""
  local logs_agent_rc="missing"
  local logs_observability_rc="missing"
  local signature_status="verified"

  if [[ "$ALLOW_UNSIGNED" -eq 1 ]]; then
    decision="conditional-pass"
    signature_status="skipped with --allow-unsigned"
    known_gaps+=$'- install.sh ran with --allow-unsigned; this is package validation only, not public release signoff.\n'
  fi

  if [[ -f "${EVIDENCE_DIR}/logs-agent.exit" ]]; then
    logs_agent_rc="$(<"${EVIDENCE_DIR}/logs-agent.exit")"
  fi
  if [[ -f "${EVIDENCE_DIR}/logs-observability.exit" ]]; then
    logs_observability_rc="$(<"${EVIDENCE_DIR}/logs-observability.exit")"
  fi
  if [[ "$logs_agent_rc" != "0" || "$logs_observability_rc" != "0" ]]; then
    decision="conditional-pass"
    known_gaps+=$'- tensorplate logs exits nonzero without log_source.path; bounded journalctl evidence captured instead.\n'
  fi
  if grep -q '0.1.0-dev0' "${EVIDENCE_DIR}/doctor-final.stdout" 2>/dev/null; then
    decision="conditional-pass"
    known_gaps+=$'- Python/PyTorch backend descriptor still reports 0.1.0-dev0 while the Debian package is 0.1.0-1.\n'
  fi
  if [[ -z "$known_gaps" ]]; then
    known_gaps="- None."
  fi

  cat >"${EVIDENCE_DIR}/clean-room.md" <<EOF
Release: ${TAG}
Release decision: ${decision}
Validation date: $(date -u +%Y-%m-%dT%H:%M:%SZ)

Artifact source:
- Repository: ${REPO}
- Assets directory: ${ASSETS_DIR}
- Work directory: ${WORK_DIR}
- Temporary directory: ${RUN_TMP_DIR}
- Evidence directory: ${EVIDENCE_DIR}

Artifact verification:
- sha256sum -c SHA256SUMS: pass
- install.sh signature verification: ${signature_status}

Clean install:
- Existing TensorPlate packages were purged.
- /etc/tensorplate, /var/lib/tensorplate, /var/log/tensorplate, and /run/tensorplate were cleared.
- install.sh --local-artifacts dry run: pass
- install.sh --local-artifacts install: pass

Installed package versions:
$(sed 's/^/- /' "${EVIDENCE_DIR}/packages-after-install.stdout")

Service and doctor:
- tensorplate --version: $(tr -d '\n' < "${EVIDENCE_DIR}/version.stdout")
- tensorplate-agent: active
- tensorplate-observability: active
- /run/tensorplate/agent.sock: present
- tensorplate doctor after install: pass
- tensorplate doctor after deploy/infer: pass, $(tr -d '\n' < "${EVIDENCE_DIR}/doctor-final-failing.stdout")

TensorRT functional validation:
- TensorRT identity bundle generated on target at ${BUNDLE_DIR}.
- tensorplate deploy ${BUNDLE_DIR} --deployment-id ${DEPLOYMENT_ID}: pass
- tensorplate infer with sample_infer.json: pass
- verify_trt_identity_response.py: pass
- tensorplate status after inference: pass

Known gaps:
${known_gaps}

Redactions:
- No credentials, tokens, raw tensor payload archives, or unbounded logs are included in this summary.
EOF
}

archive_evidence() {
  local archive="${WORK_DIR}/tensorplate-${TAG}-clean-room-evidence.tgz"
  tar czf "$archive" -C "$EVIDENCE_DIR" .
  printf 'clean_room_summary: %s\n' "${EVIDENCE_DIR}/clean-room.md"
  printf 'clean_room_archive: %s\n' "$archive"
}

cmd_download() {
  prepare_paths
  download_release_assets
  verify_assets
  printf 'assets_dir: %s\n' "$ASSETS_DIR"
}

cmd_reset() {
  parse_args "$@"
  prepare_paths
  require_confirm
  check_host
  record_environment
  reset_tensorplate
}

cmd_run() {
  parse_args "$@"
  prepare_paths
  require_confirm
  check_host
  record_environment
  if [[ ! -f "${ASSETS_DIR}/install.sh" ]]; then
    download_release_assets
  else
    note "using existing assets from ${ASSETS_DIR}"
  fi
  verify_assets
  reset_tensorplate
  install_tensorplate
  validate_runtime
  write_summary
  archive_evidence
}

case "$COMMAND" in
  run) cmd_run "$@" ;;
  reset) cmd_reset "$@" ;;
  download) parse_args "$@"; cmd_download ;;
  --help|-h|help|"") usage ;;
  *) usage >&2; die "unknown command: $COMMAND" ;;
esac
