#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# CI entrypoint for building TensorPlate release artifacts from a checked-out
# release tag. The runner must match the target package architecture.

set -Eeuo pipefail

readonly REQUIRED_PACKAGES=(
  tensorplate-common
  tensorplate-agent
  tensorplate-serving
  tensorplate-observability
  tensorplate-cli
  tensorplate-backend-python-pytorch
)
readonly INSTALLER_SOURCE="packaging/scripts/install.sh"

usage() {
  cat <<'EOF'
Usage:
  build-release-artifacts.sh --version 0.1.0 --tag v0.1.0 --artifacts-dir DIR --manifest FILE --checksums FILE [options]

Options:
  --version VERSION      Core release version, for example 0.1.0.
  --tag TAG              Git tag being published, for example v0.1.0.
  --artifacts-dir DIR    Output directory for .deb artifacts.
  --manifest FILE        Artifact manifest JSON path.
  --checksums FILE       SHA256SUMS output path.
  --target-os VALUE      Manifest target OS label.
  --arch ARCH            Manifest target architecture. Defaults to arm64.
  --skip-tag-verify      Verify manifest/checksums without requiring an annotated tag.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '==> %s\n' "$*"
}

VERSION=""
TAG=""
ARTIFACTS_DIR=""
MANIFEST=""
CHECKSUMS=""
TARGET_OS="Ubuntu 22.04 / JetPack 6.x (L4T 36.x)"
TARGET_ARCH="arm64"
SKIP_TAG_VERIFY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --tag) TAG="${2:-}"; shift 2 ;;
    --artifacts-dir) ARTIFACTS_DIR="${2:-}"; shift 2 ;;
    --manifest) MANIFEST="${2:-}"; shift 2 ;;
    --checksums) CHECKSUMS="${2:-}"; shift 2 ;;
    --target-os) TARGET_OS="${2:-}"; shift 2 ;;
    --arch) TARGET_ARCH="${2:-}"; shift 2 ;;
    --skip-tag-verify) SKIP_TAG_VERIFY=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown option '$1'" ;;
  esac
done

[[ -n "$VERSION" ]] || die "--version is required"
[[ -n "$TAG" ]] || die "--tag is required"
[[ -n "$ARTIFACTS_DIR" ]] || die "--artifacts-dir is required"
[[ -n "$MANIFEST" ]] || die "--manifest is required"
[[ -n "$CHECKSUMS" ]] || die "--checksums is required"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" ||
  die "not inside a git repository"
cd "$repo_root"

[[ "$TARGET_ARCH" == "$(dpkg --print-architecture)" ]] ||
  die "runner architecture $(dpkg --print-architecture) does not match release target $TARGET_ARCH"

note "validating release installer"
[[ -f "$INSTALLER_SOURCE" ]] || die "missing installer script at $INSTALLER_SOURCE"
bash -n "$INSTALLER_SOURCE"
command -v shellcheck >/dev/null 2>&1 || die "shellcheck is required to validate $INSTALLER_SOURCE"
shellcheck "$INSTALLER_SOURCE"

note "building Rust release binaries"
cargo build --release \
  --bin tensorplate-agent \
  --bin tensorplate-observability \
  --bin tensorplate

note "configuring C++ release build"
cmake_args=(
  -S .
  -B build/release
  -G Ninja
  -DCMAKE_BUILD_TYPE=RelWithDebInfo
  -DTP_RUNTIME_VERSION_SUFFIX=""
  -DTP_BUILD_TESTS=OFF
  -DTP_BUILD_EXAMPLES=OFF
  -DTP_ENABLE_SANITIZERS=OFF
  -DTP_ENABLE_TENSORRT="${TP_ENABLE_TENSORRT:-ON}"
  -DTP_ENABLE_LIBTORCH="${TP_ENABLE_LIBTORCH:-OFF}"
  -DTP_ENABLE_PYTHON_PYTORCH_SIDECAR="${TP_ENABLE_PYTHON_PYTORCH_SIDECAR:-ON}"
)

if [[ -n "${TP_CMAKE_TOOLCHAIN_FILE:-}" ]]; then
  cmake_args+=("-DCMAKE_TOOLCHAIN_FILE=${TP_CMAKE_TOOLCHAIN_FILE}")
elif [[ -n "${VCPKG_ROOT:-}" && -f "${VCPKG_ROOT}/scripts/buildsystems/vcpkg.cmake" ]]; then
  cmake_args+=("-DCMAKE_TOOLCHAIN_FILE=${VCPKG_ROOT}/scripts/buildsystems/vcpkg.cmake")
elif [[ -n "${VCPKG_INSTALLATION_ROOT:-}" && -f "${VCPKG_INSTALLATION_ROOT}/scripts/buildsystems/vcpkg.cmake" ]]; then
  cmake_args+=("-DCMAKE_TOOLCHAIN_FILE=${VCPKG_INSTALLATION_ROOT}/scripts/buildsystems/vcpkg.cmake")
fi

cmake "${cmake_args[@]}"

note "building serving worker"
cmake --build build/release --target tp_serving_worker --parallel
if [[ -x build/release/serving_worker/tensorplate-serving && ! -e build/release/tensorplate-serving ]]; then
  cp build/release/serving_worker/tensorplate-serving build/release/tensorplate-serving
fi
[[ -x build/release/tensorplate-serving ]] ||
  die "serving worker binary was not staged at build/release/tensorplate-serving"

note "running packaging verification suite"
test/packaging/run.sh

note "building Debian packages"
packaging/scripts/build-deb.sh

mkdir -p "$ARTIFACTS_DIR"
find "$ARTIFACTS_DIR" -maxdepth 1 -type f -name 'tensorplate*.deb' -delete

repo_parent="$(dirname "$repo_root")"
debs=()
for pkg in "${REQUIRED_PACKAGES[@]}"; do
  matches=()
  mapfile -t candidates < <(find "$repo_parent" -maxdepth 1 -type f -name "${pkg}_${VERSION}-*_*.deb" | sort)
  for candidate in "${candidates[@]}"; do
    candidate_name="$(basename -- "$candidate")"
    case "$candidate_name" in
      ${pkg}_${VERSION}-*_${TARGET_ARCH}.deb|${pkg}_${VERSION}-*_all.deb)
        matches+=("$candidate")
        ;;
    esac
  done
  ((${#matches[@]} == 1)) ||
    die "expected exactly one ${pkg}_${VERSION}-*_${TARGET_ARCH}.deb or ${pkg}_${VERSION}-*_all.deb in $repo_parent; found ${#matches[@]}"
  debs+=("${matches[0]}")
done
mapfile -t candidates < <(find "$repo_parent" -maxdepth 1 -type f -name "tensorplate-cli_${VERSION}-*_*.deb" | sort)
for candidate in "${candidates[@]}"; do
  candidate_name="$(basename -- "$candidate")"
  case "$candidate_name" in
    tensorplate-cli_${VERSION}-*_${TARGET_ARCH}.deb|tensorplate-cli_${VERSION}-*_all.deb)
      ;;
    tensorplate-cli_${VERSION}-*_*.deb)
      debs+=("$candidate")
      ;;
  esac
done
cp "${debs[@]}" "$ARTIFACTS_DIR/"
install -m 0755 "$INSTALLER_SOURCE" "$ARTIFACTS_DIR/install.sh"

note "generating manifest and checksums"
tools/release/tensorplate-release.sh manifest \
  --version "$VERSION" \
  --tag "$TAG" \
  --artifacts-dir "$ARTIFACTS_DIR" \
  --manifest "$MANIFEST" \
  --checksums "$CHECKSUMS" \
  --target-os "$TARGET_OS" \
  --arch "$TARGET_ARCH"

verify_args=(
  verify
  --version "$VERSION"
  --tag "$TAG"
  --artifacts-dir "$ARTIFACTS_DIR"
  --manifest "$MANIFEST"
  --checksums "$CHECKSUMS"
)
if [[ "$SKIP_TAG_VERIFY" -eq 1 ]]; then
  verify_args+=(--skip-tag-verify)
fi
tools/release/tensorplate-release.sh "${verify_args[@]}"

note "release artifacts are ready in $ARTIFACTS_DIR"
