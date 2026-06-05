#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# TensorPlate release installer.
#
# Download this script from a GitHub Release, then run it from disk. The
# script self-checks against SHA256SUMS before downloading or installing
# release package assets. It intentionally does not support curl | sh.

set -euo pipefail

readonly DEFAULT_VERSION="${TP_INSTALL_DEFAULT_VERSION:-0.1.1}"
readonly DEFAULT_REPO="${TP_INSTALL_REPO:-tensorplate/tensorplate}"
readonly OPTIONAL_PYTHON_PACKAGE="tensorplate-backend-python-pytorch"
readonly CLI_PACKAGE="tensorplate-cli"
readonly COMMON_PACKAGE="tensorplate-common"
readonly AGENT_UNIT="tensorplate-agent"
readonly OBSERVABILITY_UNIT="tensorplate-observability"
readonly AGENT_SOCKET_PATH="${TP_INSTALL_AGENT_SOCKET_PATH:-/run/tensorplate/agent.sock}"
readonly SERVICE_READY_TIMEOUT_SECONDS="${TP_INSTALL_SERVICE_READY_TIMEOUT_SECONDS:-30}"
readonly COSIGN_BOOTSTRAP_VERSION="${TP_INSTALL_COSIGN_VERSION:-v3.0.2}"
readonly COSIGN_BOOTSTRAP_BASE_URL="${TP_INSTALL_COSIGN_BASE_URL:-https://github.com/sigstore/cosign/releases/download/${COSIGN_BOOTSTRAP_VERSION}}"
readonly COSIGN_LINUX_AMD64_SHA256="46dbdcb5467a3dfec2526923d0b3365e40c8d9dc00ec23d5aca3437449e8cbfd"
readonly COSIGN_LINUX_ARM64_SHA256="17fd784737ca54d7d8a343c82da6c5d6dbdee971e66644d923d1b057fb97d7ed"

VERSION_INPUT="$DEFAULT_VERSION"
INSTALL_MODE="runtime"
WITH_PYTHON_BACKEND=0
YES=0
FORCE_OS=0
STRICT_HARDWARE=0
DRY_RUN=0
ALLOW_UNSIGNED=0
LOCAL_ARTIFACTS_DIR=""
INSTALL_WORKDIR=""
COSIGN_BOOTSTRAP_BIN=""

readonly SIGSTORE_OIDC_ISSUER="https://token.actions.githubusercontent.com"

usage() {
  cat <<'EOF'
Usage:
  sudo bash install.sh [options]

Options:
  --version VERSION          Release to install. Accepts 0.1.1, v0.1.1, or v0.1.1-rc.N.
                             Defaults to the pinned current release.
  --cli-only                 Install only the operator CLI for this host architecture.
                             Skips Jetson OS/hardware validation, service enablement, and doctor.
  --with-python-backend      Install tensorplate-backend-python-pytorch in addition to core packages.
                             Runtime install mode only.
  --yes, -y                  Continue without interactive prompts; intended for unattended provisioning.
  --force-os                 Continue on an unsupported OS. This is unsupported and at your own risk.
  --strict-hardware          Treat hardware or architecture warnings as fatal.
  --dry-run                  Validate host gates and print planned actions without downloading or installing.
  --local-artifacts DIR      Install from a local artifact directory containing install.sh,
                             tensorplate-*-artifacts.json, SHA256SUMS, and package .debs.
                             Intended for unreleased snapshot/build-only artifacts.
  --allow-unsigned           Skip the cosign signature check on SHA256SUMS and accept checksum-only
                             integrity. Unsupported; for air-gapped or bootstrap use at your own risk.
  --help, -h                 Show this help text.

The supported OS baseline is NVIDIA Jetson Linux / JetPack 6.x with L4T 36.x.
Hardware validation is advisory by default: unrecognized Jetson models or
non-arm64 architectures warn and require confirmation in interactive mode.
CLI-only mode is for Debian/Ubuntu desktops and requires a matching
tensorplate-cli package asset for the host Debian architecture.

By default the installer verifies a keyless cosign signature over SHA256SUMS
that binds it to the TensorPlate release workflow before trusting any
checksum. If cosign is not already on PATH, the installer downloads a pinned
Linux cosign binary to its temporary work directory, verifies its pinned
SHA256, and runs it from there. Pass --allow-unsigned only to accept
checksum-only integrity.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

note() {
  printf '==> %s\n' "$*"
}

command_exists() {
  command -v "$1" >/dev/null 2>&1
}

is_interactive() {
  [[ -t 0 && -t 1 ]]
}

confirm_continue() {
  local prompt="$1"
  local reply
  if ((YES)); then
    note "continuing because --yes was provided"
    return 0
  fi
  if ! is_interactive; then
    die "$prompt; rerun with --yes for unattended provisioning or fix the host mismatch"
  fi
  printf '%s [y/N] ' "$prompt" >&2
  read -r reply
  case "$reply" in
    y|Y|yes|YES) return 0 ;;
    *) die "aborted by operator" ;;
  esac
}

while (($# > 0)); do
  case "$1" in
    --version)
      (($# >= 2)) || die "--version requires a value"
      VERSION_INPUT="$2"
      shift 2
      ;;
    --cli-only)
      INSTALL_MODE="cli"
      shift
      ;;
    --with-python-backend)
      WITH_PYTHON_BACKEND=1
      shift
      ;;
    --yes|-y)
      YES=1
      shift
      ;;
    --force-os)
      FORCE_OS=1
      shift
      ;;
    --strict-hardware)
      STRICT_HARDWARE=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --local-artifacts)
      (($# >= 2)) || die "--local-artifacts requires a directory"
      LOCAL_ARTIFACTS_DIR="$2"
      shift 2
      ;;
    --allow-unsigned)
      ALLOW_UNSIGNED=1
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

if [[ -n "$LOCAL_ARTIFACTS_DIR" ]]; then
  LOCAL_ARTIFACTS_DIR="$(cd -- "$LOCAL_ARTIFACTS_DIR" && pwd)" ||
    die "local artifact directory does not exist: $LOCAL_ARTIFACTS_DIR"
  shopt -s nullglob
  local_manifests=("$LOCAL_ARTIFACTS_DIR"/tensorplate-*-artifacts.json)
  shopt -u nullglob
  ((${#local_manifests[@]} == 1)) ||
    die "--local-artifacts requires exactly one tensorplate-*-artifacts.json in $LOCAL_ARTIFACTS_DIR; found ${#local_manifests[@]}"
  MANIFEST_NAME="$(basename -- "${local_manifests[0]}")"
  local_manifest_meta=()
  while IFS= read -r line; do
    local_manifest_meta+=("$line")
  done < <(python3 - "${local_manifests[0]}" <<'PY'
import json
import sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text())
release = manifest.get("release", {})
print(release.get("tag", "local-artifacts"))
print(release.get("version", "0.0.0~local"))
PY
  )
  ((${#local_manifest_meta[@]} == 2)) ||
    die "could not read release metadata from ${local_manifests[0]}"
  TAG="${local_manifest_meta[0]}"
  RELEASE_VERSION="${local_manifest_meta[1]%%-*}"
else
  if [[ "$VERSION_INPUT" == v* ]]; then
    TAG="$VERSION_INPUT"
    RELEASE_VERSION="${VERSION_INPUT#v}"
    RELEASE_VERSION="${RELEASE_VERSION%%-*}"
  else
    RELEASE_VERSION="$VERSION_INPUT"
    TAG="v${VERSION_INPUT}"
  fi

  [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[1-9][0-9]*)?$ ]] ||
    die "--version must be 0.1.0, v0.1.0, or v0.1.0-rc.N"
  [[ "$RELEASE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "release version must be MAJOR.MINOR.PATCH"
  MANIFEST_NAME="tensorplate-${TAG}-artifacts.json"
fi
if [[ "$INSTALL_MODE" == "cli" && "$WITH_PYTHON_BACKEND" -eq 1 ]]; then
  die "--with-python-backend cannot be combined with --cli-only"
fi

readonly REPO="$DEFAULT_REPO"
readonly RELEASE_URL="https://github.com/${REPO}/releases/download/${TAG}"

host_deb_arch() {
  if [[ -n "${TP_INSTALL_DEB_ARCH:-}" ]]; then
    printf '%s\n' "$TP_INSTALL_DEB_ARCH"
    return 0
  fi
  if command_exists dpkg; then
    dpkg --print-architecture
    return 0
  fi
  case "$(uname -m)" in
    x86_64) printf 'amd64\n' ;;
    aarch64|arm64) printf 'arm64\n' ;;
    armv7l) printf 'armhf\n' ;;
    *) die "cannot infer Debian architecture; set TP_INSTALL_DEB_ARCH" ;;
  esac
}

script_path() {
  local source_path="${BASH_SOURCE[0]}"
  if [[ "$source_path" != /* ]]; then
    source_path="${PWD}/${source_path}"
  fi
  printf '%s\n' "$source_path"
}

verify_installer_checksum() {
  if ((DRY_RUN)); then
    return 0
  fi
  if [[ "${TP_INSTALL_SKIP_SELF_CHECK:-0}" == "1" ]]; then
    warn "TP_INSTALL_SKIP_SELF_CHECK=1 set; skipping installer self-check"
    return 0
  fi

  local path dir base checksums subset tmpdir bundle
  path="$(script_path)"
  dir="$(cd -- "$(dirname -- "$path")" && pwd)"
  base="$(basename -- "$path")"
  [[ "$base" == "install.sh" ]] ||
    die "installer self-check requires the script to be named install.sh"
  [[ -f "$dir/$base" ]] ||
    die "installer self-check requires running install.sh from a downloaded file"

  tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/tensorplate-installer-check.XXXXXX")"
  if [[ -f "$dir/SHA256SUMS" ]]; then
    checksums="$dir/SHA256SUMS"
  else
    checksums="${tmpdir}/SHA256SUMS"
    download_file "${RELEASE_URL}/SHA256SUMS" "$checksums"
  fi

  bundle="${tmpdir}/SHA256SUMS.cosign.bundle"
  verify_checksums_signature "$checksums" "$dir" "$bundle"

  subset="${tmpdir}/install.sh.SHA256SUMS"
  grep -E '  install[.]sh$' "$checksums" >"$subset" ||
    die "SHA256SUMS does not list install.sh"
  note "verifying install.sh with SHA256SUMS"
  (cd "$dir" && sha256sum -c "$subset") || {
    rm -rf "$tmpdir"
    die "installer checksum verification failed"
  }
  rm -rf "$tmpdir"
}

read_os_release_field() {
  local key="$1"
  local file="${TP_INSTALL_OS_RELEASE:-/etc/os-release}"
  [[ -r "$file" ]] || return 0
  awk -F= -v key="$key" '
    $1 == key {
      value = $2
      gsub(/^"/, "", value)
      gsub(/"$/, "", value)
      print value
      exit
    }
  ' "$file"
}

validate_os() {
  local nv_file="${TP_INSTALL_NV_TEGRA_RELEASE:-/etc/nv_tegra_release}"
  local os_file="${TP_INSTALL_OS_RELEASE:-/etc/os-release}"
  local os_id os_version
  local failures=()

  if [[ ! -r "$nv_file" ]]; then
    failures+=("missing ${nv_file}; expected NVIDIA Jetson L4T release metadata")
  elif ! grep -Eq '(^|[^0-9])R36([^0-9]|$)' "$nv_file"; then
    failures+=("${nv_file} is not L4T R36.x")
  fi

  os_id="$(read_os_release_field ID || true)"
  os_version="$(read_os_release_field VERSION_ID || true)"
  if [[ ! -r "$os_file" ]]; then
    failures+=("missing ${os_file}; expected Ubuntu 22.04 base OS metadata")
  elif [[ "$os_id" != "ubuntu" || "$os_version" != 22.04* ]]; then
    failures+=("${os_file} reports ID=${os_id:-unknown}, VERSION_ID=${os_version:-unknown}; expected ubuntu 22.04")
  fi

  if ((${#failures[@]} > 0)); then
    local failure
    for failure in "${failures[@]}"; do
      warn "$failure"
    done
    if ((FORCE_OS)); then
      warn "--force-os was provided; continuing on an unsupported OS"
      return 0
    fi
    die "unsupported OS; TensorPlate ${TAG} supports JetPack 6.x / L4T 36.x by default"
  fi

  note "OS validation passed: JetPack 6.x / L4T 36.x baseline detected"
}

read_device_model() {
  local model_file="${TP_INSTALL_DEVICE_MODEL:-/proc/device-tree/model}"
  [[ -r "$model_file" ]] || return 0
  tr -d '\0' <"$model_file" | awk '{$1=$1; print}'
}

validate_hardware() {
  local arch model
  local warnings=()

  arch="${TP_INSTALL_ARCH:-$(uname -m)}"
  case "$arch" in
    aarch64|arm64) ;;
    *) warnings+=("architecture ${arch} is not arm64/aarch64") ;;
  esac

  model="$(read_device_model || true)"
  if [[ -z "$model" ]]; then
    warnings+=("unable to read Jetson model from ${TP_INSTALL_DEVICE_MODEL:-/proc/device-tree/model}")
  elif [[ "$model" != *"Jetson Orin Nano"* && "$model" != *"Jetson Orin NX"* ]]; then
    warnings+=("unrecognized Jetson model: ${model}")
  fi

  if ((${#warnings[@]} == 0)); then
    note "hardware validation passed: ${model:-unknown model}, arch=${arch}"
    return 0
  fi

  local item
  for item in "${warnings[@]}"; do
    warn "$item"
  done

  if ((STRICT_HARDWARE)); then
    die "hardware validation failed because --strict-hardware was provided"
  fi

  confirm_continue "hardware validation is advisory but reported warnings; continue anyway?"
}

require_root() {
  if ((DRY_RUN)); then
    return 0
  fi
  [[ "${EUID}" -eq 0 ]] || die "run as root, for example: sudo bash install.sh"
}

require_base_commands() {
  local cmd
  local required=(curl sha256sum python3)
  for cmd in "${required[@]}"; do
    command_exists "$cmd" || die "required command not found: $cmd"
  done
}

require_install_commands() {
  local cmd
  local required=()
  if ! ((DRY_RUN)); then
    required+=(apt-get)
    if [[ "$INSTALL_MODE" == "runtime" ]]; then
      required+=(systemctl)
    fi
  fi
  ((${#required[@]} == 0)) && return 0
  for cmd in "${required[@]}"; do
    command_exists "$cmd" || die "required command not found: $cmd"
  done
}

download_file() {
  local url="$1"
  local dest="$2"
  note "downloading ${url}"
  # --speed-limit/--speed-time abort a stalled transfer (under 1 KiB/s for 30s)
  # without capping a slow-but-progressing large download.
  curl -fL --retry 3 --connect-timeout 20 --speed-limit 1024 --speed-time 30 \
    --output "$dest" "$url"
}

cosign_bin() {
  if [[ -n "$COSIGN_BOOTSTRAP_BIN" && "${TP_INSTALL_FORCE_COSIGN_BOOTSTRAP:-0}" == "1" ]]; then
    printf '%s\n' "$COSIGN_BOOTSTRAP_BIN"
  elif [[ -n "${TP_INSTALL_COSIGN:-}" ]]; then
    printf '%s\n' "$TP_INSTALL_COSIGN"
  elif [[ -n "$COSIGN_BOOTSTRAP_BIN" ]]; then
    printf '%s\n' "$COSIGN_BOOTSTRAP_BIN"
  else
    printf '%s\n' "cosign"
  fi
}

cosign_available() {
  command_exists "$(cosign_bin)"
}

cosign_bootstrap_asset() {
  if [[ "${TP_INSTALL_UNSAFE_TEST_HOOKS:-0}" == "1" && -n "${TP_INSTALL_COSIGN_BOOTSTRAP_ASSET:-}" ]]; then
    printf '%s\n' "$TP_INSTALL_COSIGN_BOOTSTRAP_ASSET"
    return 0
  fi
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64|Linux:amd64) printf '%s\n' "cosign-linux-amd64" ;;
    Linux:aarch64|Linux:arm64) printf '%s\n' "cosign-linux-arm64" ;;
    *) return 1 ;;
  esac
}

cosign_bootstrap_sha256() {
  case "$1" in
    cosign-linux-amd64)
      if [[ "${TP_INSTALL_UNSAFE_TEST_HOOKS:-0}" == "1" && -n "${TP_INSTALL_COSIGN_LINUX_AMD64_SHA256:-}" ]]; then
        printf '%s\n' "$TP_INSTALL_COSIGN_LINUX_AMD64_SHA256"
      else
        printf '%s\n' "$COSIGN_LINUX_AMD64_SHA256"
      fi
      ;;
    cosign-linux-arm64)
      if [[ "${TP_INSTALL_UNSAFE_TEST_HOOKS:-0}" == "1" && -n "${TP_INSTALL_COSIGN_LINUX_ARM64_SHA256:-}" ]]; then
        printf '%s\n' "$TP_INSTALL_COSIGN_LINUX_ARM64_SHA256"
      else
        printf '%s\n' "$COSIGN_LINUX_ARM64_SHA256"
      fi
      ;;
    *) return 1 ;;
  esac
}

bootstrap_cosign() {
  local dest_dir="$1"
  local asset expected dest

  asset="$(cosign_bootstrap_asset)" ||
    die "cosign is required to verify the release signature, and automatic cosign bootstrap supports only Linux amd64/arm64; install cosign or re-run with --allow-unsigned to accept checksum-only integrity at your own risk"
  expected="$(cosign_bootstrap_sha256 "$asset")" ||
    die "cannot resolve pinned checksum for ${asset}"
  dest="${dest_dir}/${asset}"

  note "cosign not found; downloading pinned ${asset} ${COSIGN_BOOTSTRAP_VERSION}"
  download_file "${COSIGN_BOOTSTRAP_BASE_URL}/${asset}" "$dest"
  (
    cd "$dest_dir"
    printf '%s  %s\n' "$expected" "$asset" | sha256sum -c -
  ) || die "downloaded cosign bootstrap checksum did not match the pinned value"
  chmod 0755 "$dest"
  COSIGN_BOOTSTRAP_BIN="$dest"
}

ensure_cosign() {
  local dest_dir="$1"

  if [[ "${TP_INSTALL_FORCE_COSIGN_BOOTSTRAP:-0}" != "1" ]] && cosign_available; then
    return 0
  fi
  if [[ -n "${TP_INSTALL_COSIGN:-}" && "${TP_INSTALL_FORCE_COSIGN_BOOTSTRAP:-0}" != "1" ]]; then
    die "configured cosign binary was not found: ${TP_INSTALL_COSIGN}"
  fi
  if [[ "${TP_INSTALL_COSIGN_BOOTSTRAP:-1}" != "1" ]]; then
    die "cosign is required to verify the release signature; install cosign or re-run with --allow-unsigned to accept checksum-only integrity at your own risk"
  fi
  bootstrap_cosign "$dest_dir"
}

checksums_identity_regexp() {
  # OIDC identity of the TensorPlate release workflow for any vX.Y.Z[-rc.N] tag.
  printf '^https://github.com/%s/[.]github/workflows/release[.]yml@refs/tags/v[0-9]+[.][0-9]+[.][0-9]+(-rc[.][0-9]+)?$\n' "$REPO"
}

fetch_signature_bundle() {
  # Resolve the cosign bundle for SHA256SUMS: reuse a local copy next to the
  # checksums when present, otherwise download it from the release.
  local local_dir="$1"
  local dest="$2"
  local local_bundle="${local_dir}/SHA256SUMS.cosign.bundle"
  if [[ -f "$local_bundle" ]]; then
    [[ "$local_bundle" -ef "$dest" ]] || cp -- "$local_bundle" "$dest"
    return 0
  fi
  download_file "${RELEASE_URL}/SHA256SUMS.cosign.bundle" "$dest"
}

verify_checksums_signature() {
  # Verify the keyless cosign signature over SHA256SUMS before trusting it.
  # This establishes authenticity (signed by the TensorPlate release workflow
  # on a release tag), not merely integrity against accidental corruption.
  # The bundle is only fetched once the guards below decide we will verify, so
  # --allow-unsigned and --dry-run never require a published signature.
  local checksums="$1"
  local local_dir="$2"
  local bundle="$3"
  local output

  if ((DRY_RUN)); then
    return 0
  fi
  if [[ "${TP_INSTALL_ALLOW_UNSIGNED:-0}" == "1" || "$ALLOW_UNSIGNED" == "1" ]]; then
    warn "signature verification disabled (--allow-unsigned); SHA256SUMS authenticity is NOT verified"
    return 0
  fi
  ensure_cosign "$(dirname -- "$bundle")"

  fetch_signature_bundle "$local_dir" "$bundle"
  note "verifying SHA256SUMS signature with cosign (keyless)"
  if ! output="$("$(cosign_bin)" verify-blob \
    --bundle "$bundle" \
    --certificate-identity-regexp "$(checksums_identity_regexp)" \
    --certificate-oidc-issuer "$SIGSTORE_OIDC_ISSUER" \
    "$checksums" 2>&1)"; then
    printf '%s\n' "$output" >&2
    die "cosign signature verification failed for SHA256SUMS"
  fi
  note "SHA256SUMS signature verified: signed by ${REPO} release workflow"
}

write_install_deb_list() {
  local manifest="$1"
  local include_python="$2"
  local mode="$3"
  local deb_arch="$4"
  python3 - "$manifest" "$include_python" "$mode" "$deb_arch" "$OPTIONAL_PYTHON_PACKAGE" "$COMMON_PACKAGE" "$CLI_PACKAGE" <<'PY'
import json
import sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text())
include_python = sys.argv[2] == "1"
mode = sys.argv[3]
deb_arch = sys.argv[4]
optional_package = sys.argv[5]
common_package = sys.argv[6]
cli_package = sys.argv[7]
if mode == "runtime":
    required = [
        common_package,
        "tensorplate-agent",
        "tensorplate-serving",
        "tensorplate-observability",
        cli_package,
    ]
    if include_python:
        required.append(optional_package)
elif mode == "cli":
    required = [common_package, cli_package]
else:
    raise SystemExit(f"unknown install mode: {mode}")

artifacts = manifest.get("artifacts", [])
selected = []
for package in required:
    matches = []
    for artifact in artifacts:
        name = artifact.get("file")
        arch = artifact.get("architecture")
        if artifact.get("package") != package:
            continue
        if not isinstance(name, str) or not name.endswith(".deb"):
            continue
        if "/" in name or name in {".", ".."} or ".." in name.split("/"):
            raise SystemExit(f"unsafe artifact filename in manifest: {name!r}")
        if arch not in ("all", deb_arch):
            continue
        matches.append(name)
    if len(matches) != 1:
        raise SystemExit(
            f"manifest expected exactly one {package} package for architecture {deb_arch} "
            f"(or architecture all); found {len(matches)}"
        )
    selected.append(matches[0])
for name in selected:
    print(name)
PY
}

write_checksum_subset() {
  local checksums="$1"
  shift
  python3 - "$checksums" "$@" <<'PY'
import sys
from pathlib import Path

checksums = Path(sys.argv[1]).read_text().splitlines()
requested = sys.argv[2:]
by_name = {}
for line in checksums:
    if not line.strip():
        continue
    parts = line.split(None, 1)
    if len(parts) != 2:
        raise SystemExit(f"malformed SHA256SUMS line: {line!r}")
    digest, name = parts
    by_name[name.strip()] = digest
for name in requested:
    digest = by_name.get(name)
    if digest is None:
        raise SystemExit(f"SHA256SUMS is missing {name}")
    print(f"{digest}  {name}")
PY
}

download_and_verify_assets() {
  local workdir="$1"
  local manifest_path="${workdir}/${MANIFEST_NAME}"
  local checksums_path="${workdir}/SHA256SUMS"
  local bundle_path="${workdir}/SHA256SUMS.cosign.bundle"
  local file
  local deb_arch
  local selected=()

  download_file "${RELEASE_URL}/${MANIFEST_NAME}" "$manifest_path"
  download_file "${RELEASE_URL}/SHA256SUMS" "$checksums_path"
  verify_checksums_signature "$checksums_path" "$workdir" "$bundle_path"

  deb_arch="$(host_deb_arch)"
  while IFS= read -r file; do
    [[ -n "$file" ]] || continue
    selected+=("$file")
    [[ -f "${workdir}/${file}" ]] || download_file "${RELEASE_URL}/${file}" "${workdir}/${file}"
  done < <(write_install_deb_list "$manifest_path" "$WITH_PYTHON_BACKEND" "$INSTALL_MODE" "$deb_arch")

  note "verifying downloaded assets with SHA256SUMS"
  (
    cd "$workdir"
    write_checksum_subset "$checksums_path" "$MANIFEST_NAME" "${selected[@]}" >SELECTED-SHA256SUMS
    sha256sum -c SELECTED-SHA256SUMS
  )
}

copy_and_verify_local_assets() {
  local workdir="$1"
  local manifest_path="${workdir}/${MANIFEST_NAME}"
  local checksums_path="${workdir}/SHA256SUMS"
  local bundle_path="${workdir}/SHA256SUMS.cosign.bundle"
  local file
  local deb_arch
  local selected=()

  [[ -f "${LOCAL_ARTIFACTS_DIR}/${MANIFEST_NAME}" ]] ||
    die "local artifact manifest is missing: ${LOCAL_ARTIFACTS_DIR}/${MANIFEST_NAME}"
  [[ -f "${LOCAL_ARTIFACTS_DIR}/SHA256SUMS" ]] ||
    die "local artifact checksums are missing: ${LOCAL_ARTIFACTS_DIR}/SHA256SUMS"

  cp -- "${LOCAL_ARTIFACTS_DIR}/${MANIFEST_NAME}" "$manifest_path"
  cp -- "${LOCAL_ARTIFACTS_DIR}/SHA256SUMS" "$checksums_path"
  verify_checksums_signature "$checksums_path" "$LOCAL_ARTIFACTS_DIR" "$bundle_path"

  deb_arch="$(host_deb_arch)"
  while IFS= read -r file; do
    [[ -n "$file" ]] || continue
    selected+=("$file")
    [[ -f "${LOCAL_ARTIFACTS_DIR}/${file}" ]] ||
      die "local artifact package is missing: ${LOCAL_ARTIFACTS_DIR}/${file}"
    cp -- "${LOCAL_ARTIFACTS_DIR}/${file}" "${workdir}/${file}"
  done < <(write_install_deb_list "$manifest_path" "$WITH_PYTHON_BACKEND" "$INSTALL_MODE" "$deb_arch")

  note "verifying local artifacts with SHA256SUMS"
  (
    cd "$workdir"
    write_checksum_subset "$checksums_path" "$MANIFEST_NAME" "${selected[@]}" >SELECTED-SHA256SUMS
    sha256sum -c SELECTED-SHA256SUMS
  )
}

install_packages() {
  local workdir="$1"
  local manifest_path="${workdir}/${MANIFEST_NAME}"
  local deb
  local deb_arch
  local deb_paths=()

  deb_arch="$(host_deb_arch)"
  while IFS= read -r deb; do
    [[ -n "$deb" ]] || continue
    deb_paths+=("./${deb}")
  done < <(write_install_deb_list "$manifest_path" "$WITH_PYTHON_BACKEND" "$INSTALL_MODE" "$deb_arch")

  ((${#deb_paths[@]} > 0)) || die "manifest did not select any Debian packages to install"

  note "updating apt package metadata"
  apt-get update

  if [[ "$INSTALL_MODE" == "cli" ]]; then
    note "installing TensorPlate CLI package with apt --reinstall"
  else
    note "installing TensorPlate runtime packages with apt --reinstall"
  fi
  (
    cd "$workdir"
    DEBIAN_FRONTEND=noninteractive apt-get \
      -y \
      -o Dpkg::Options::=--force-confdef \
      -o Dpkg::Options::=--force-confold \
      install --reinstall "${deb_paths[@]}"
  )
}

enable_services() {
  note "enabling TensorPlate services"
  systemctl enable --now "$AGENT_UNIT" "$OBSERVABILITY_UNIT"
}

wait_for_services_ready() {
  local deadline=$((SECONDS + SERVICE_READY_TIMEOUT_SECONDS))

  note "waiting for TensorPlate services to become ready"
  while ((SECONDS <= deadline)); do
    if systemctl is-active --quiet "$AGENT_UNIT" &&
      systemctl is-active --quiet "$OBSERVABILITY_UNIT" &&
      [[ -S "$AGENT_SOCKET_PATH" ]]; then
      return 0
    fi
    sleep 1
  done

  systemctl --no-pager --full status "$AGENT_UNIT" "$OBSERVABILITY_UNIT" >&2 || true
  die "TensorPlate services did not become ready within ${SERVICE_READY_TIMEOUT_SECONDS}s"
}

check_cli() {
  command_exists tensorplate || die "tensorplate CLI was not found after package installation"
  note "validating tensorplate CLI"
  tensorplate version >/dev/null
}

check_doctor() {
  local workdir="$1"
  local doctor_json="${workdir}/doctor.json"
  local doctor_stderr="${workdir}/doctor.stderr"
  local rc=0
  local critical noncritical

  command_exists tensorplate || die "tensorplate CLI was not found after package installation"
  note "running tensorplate doctor --output json"
  set +e
  tensorplate doctor --output json >"$doctor_json" 2>"$doctor_stderr"
  rc=$?
  set -e

  [[ -s "$doctor_json" ]] || {
    [[ -s "$doctor_stderr" ]] && cat "$doctor_stderr" >&2
    die "tensorplate doctor did not emit JSON output"
  }

  critical="$(python3 - "$doctor_json" <<'PY'
import json
import sys
from pathlib import Path

data = json.loads(Path(sys.argv[1]).read_text())
findings = data.get("payload", {}).get("findings", [])
for finding in findings:
    if finding.get("status") == "fail" and finding.get("severity") == "critical":
        line = f"{finding.get('id', '<unknown>')}: {finding.get('message', '<no message>')}"
        hint = finding.get("hint")
        if hint:
            line += f" (hint: {hint})"
        print(line)
PY
)"
  if [[ -n "$critical" ]]; then
    printf '%s\n' "$critical" >&2
    die "tensorplate doctor reported critical finding(s)"
  fi

  noncritical="$(python3 - "$doctor_json" <<'PY'
import json
import sys
from pathlib import Path

data = json.loads(Path(sys.argv[1]).read_text())
findings = data.get("payload", {}).get("findings", [])
for finding in findings:
    if finding.get("status") == "fail" and finding.get("severity") != "critical":
        print(f"{finding.get('id', '<unknown>')}: {finding.get('message', '<no message>')}")
PY
)"
  if [[ "$rc" -ne 0 && -n "$noncritical" ]]; then
    warn "tensorplate doctor reported non-critical failing finding(s):"
    printf '%s\n' "$noncritical" >&2
  elif [[ "$rc" -ne 0 ]]; then
    warn "tensorplate doctor exited ${rc}, but no critical findings were present in JSON"
    [[ -s "$doctor_stderr" ]] && cat "$doctor_stderr" >&2
  fi
}

dry_run_summary() {
  note "dry-run selected ${TAG}"
  printf 'Install mode: %s\n' "$INSTALL_MODE"
  printf 'Debian architecture: %s\n' "$(host_deb_arch)"
  if [[ -n "$LOCAL_ARTIFACTS_DIR" ]]; then
    printf 'Would install from local artifacts: %s\n' "$LOCAL_ARTIFACTS_DIR"
    printf 'Would read:\n'
    printf '  %s/%s\n' "$LOCAL_ARTIFACTS_DIR" "$MANIFEST_NAME"
    printf '  %s/SHA256SUMS\n' "$LOCAL_ARTIFACTS_DIR"
  else
    printf 'Would download:\n'
    printf '  %s/%s\n' "$RELEASE_URL" "$MANIFEST_NAME"
    printf '  %s/SHA256SUMS\n' "$RELEASE_URL"
    printf '  %s/SHA256SUMS.cosign.bundle\n' "$RELEASE_URL"
  fi
  if [[ "${TP_INSTALL_ALLOW_UNSIGNED:-0}" == "1" || "$ALLOW_UNSIGNED" == "1" ]]; then
    printf 'Signature check: DISABLED (--allow-unsigned); checksum-only integrity.\n'
  else
    printf 'Would verify the SHA256SUMS cosign signature against the %s release workflow identity.\n' "$REPO"
  fi
  if [[ "$INSTALL_MODE" == "cli" ]]; then
    printf '  %s and %s package assets matching this host architecture\n' "$COMMON_PACKAGE" "$CLI_PACKAGE"
    printf 'Would verify the manifest and selected package assets with SHA256SUMS.\n'
    printf 'Would install only the CLI package set via apt-get install --reinstall.\n'
  else
    printf '  TensorPlate runtime package assets matching this host architecture\n'
    printf 'Would verify the manifest and selected package assets with SHA256SUMS.\n'
    printf 'Would install core runtime packages via apt-get install --reinstall.\n'
    if ((WITH_PYTHON_BACKEND)); then
      printf 'Would also install %s.\n' "$OPTIONAL_PYTHON_PACKAGE"
    fi
    printf 'Would enable %s and %s, wait for %s, then run tensorplate doctor --output json.\n' \
      "$AGENT_UNIT" "$OBSERVABILITY_UNIT" "$AGENT_SOCKET_PATH"
  fi
}

main() {
  note "TensorPlate installer ${TAG}"
  require_base_commands
  verify_installer_checksum
  if [[ "$INSTALL_MODE" == "runtime" ]]; then
    validate_os
    validate_hardware
  else
    note "CLI-only mode selected; skipping Jetson OS and hardware validation"
  fi
  require_root
  require_install_commands

  if ((DRY_RUN)); then
    dry_run_summary
    return 0
  fi

  local workdir
  workdir="$(mktemp -d "${TMPDIR:-/tmp}/tensorplate-install-${TAG}.XXXXXX")"
  INSTALL_WORKDIR="$workdir"
  if [[ "${TP_INSTALL_KEEP_WORKDIR:-0}" != "1" ]]; then
    trap 'rm -rf "$INSTALL_WORKDIR"' EXIT
  else
    note "keeping installer workdir ${workdir}"
  fi

  if [[ -n "$LOCAL_ARTIFACTS_DIR" ]]; then
    copy_and_verify_local_assets "$workdir"
  else
    download_and_verify_assets "$workdir"
  fi
  install_packages "$workdir"
  if [[ "$INSTALL_MODE" == "cli" ]]; then
    check_cli
  else
    enable_services
    wait_for_services_ready
    check_doctor "$workdir"
  fi

  note "TensorPlate ${TAG} install complete"
  if [[ "$INSTALL_MODE" == "cli" ]]; then
    if [[ -n "$LOCAL_ARTIFACTS_DIR" ]]; then
      printf 'Installed TensorPlate CLI from local artifacts at %s.\n' "$LOCAL_ARTIFACTS_DIR"
    else
      printf 'Installed TensorPlate CLI from %s.\n' "$RELEASE_URL"
    fi
    printf 'Next: configure a profile or use --agent-url to reach a Jetson runtime.\n'
  else
    if [[ -n "$LOCAL_ARTIFACTS_DIR" ]]; then
      printf 'Installed TensorPlate runtime packages from local artifacts at %s.\n' "$LOCAL_ARTIFACTS_DIR"
    else
      printf 'Installed TensorPlate runtime packages from %s.\n' "$RELEASE_URL"
    fi
    if ((WITH_PYTHON_BACKEND)); then
      printf 'Installed optional Python/PyTorch backend package. Install the platform PyTorch stack separately if doctor reports it missing.\n'
    fi
    printf 'Next: use tensorplate status and tensorplate doctor for operational checks.\n'
  fi
}

main
