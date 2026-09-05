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
  tensorplate-apt-source
  tensorplate
)
# The complete runtime set for the secondary architecture, built on its own
# native runner and staged into the repository parent before this script
# collects. Kept in lockstep with SECONDARY_ARCH_PACKAGES in
# tools/release/tensorplate-release.sh, which is what enforces it.
readonly SECONDARY_ARCH="amd64"
readonly SECONDARY_ARCH_PACKAGES=(
  tensorplate-agent
  tensorplate-serving
  tensorplate-observability
  tensorplate-cli
  tensorplate
)
readonly INSTALLER_SOURCE="packaging/scripts/install.sh"

usage() {
  cat <<'EOF'
Usage:
  build-release-artifacts.sh --version 0.1.0 --tag v0.1.0 --artifacts-dir DIR --manifest FILE --checksums FILE [options]
  build-release-artifacts.sh --snapshot --branch develop --artifacts-dir DIR --manifest FILE --checksums FILE [options]

Options:
  --version VERSION      Canonical release version, for example 0.1.0. Always
                         bare MAJOR.MINOR.PATCH for a release build; this is
                         what the manifest and installer record.
  --python-version VER   PEP 440 SDK version, for example 0.1.0rc1. Defaults
                         to --version. The wheel and sdist are named with it.
  --deb-version VERSION  Debian package version, for example 0.1.0~rc.1.
                         Defaults to --version. A candidate differs here and
                         only here: `~` sorts below the bare version, so the
                         final release is an upgrade from the candidate.
  --tag TAG              Git tag being published, for example v0.1.0.
  --artifacts-dir DIR    Output directory for .deb artifacts.
  --manifest FILE        Artifact manifest JSON path.
  --checksums FILE       SHA256SUMS output path.
  --target-os VALUE      Manifest target OS label.
  --arch ARCH            Manifest target architecture. Defaults to arm64.
  --skip-tag-verify      Verify manifest/checksums without requiring an annotated tag.
  --snapshot             Build unreleased local-source snapshot artifacts.
  --branch BRANCH        Branch/provenance label for snapshot manifests.
  --build-dir DIR        CMake build directory. Defaults to build/release, or build/snapshot-ARCH for snapshots.
  --sdk-dist-dir DIR     Directory holding the tensorplate-python wheel + sdist to include in the release.
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
DEB_VERSION=""
PYTHON_VERSION=""
TAG=""
ARTIFACTS_DIR=""
MANIFEST=""
CHECKSUMS=""
BUILD_DIR=""
TARGET_OS="Ubuntu 22.04 / JetPack 6.x (L4T 36.x)"
TARGET_ARCH="arm64"
SKIP_TAG_VERIFY=0
SNAPSHOT=0
BRANCH=""
CHANGELOG_BACKUP=""
SDK_DIST_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --deb-version) DEB_VERSION="${2:-}"; shift 2 ;;
    --python-version) PYTHON_VERSION="${2:-}"; shift 2 ;;
    --tag) TAG="${2:-}"; shift 2 ;;
    --artifacts-dir) ARTIFACTS_DIR="${2:-}"; shift 2 ;;
    --manifest) MANIFEST="${2:-}"; shift 2 ;;
    --checksums) CHECKSUMS="${2:-}"; shift 2 ;;
    --target-os) TARGET_OS="${2:-}"; shift 2 ;;
    --arch) TARGET_ARCH="${2:-}"; shift 2 ;;
    --skip-tag-verify) SKIP_TAG_VERIFY=1; shift ;;
    --snapshot) SNAPSHOT=1; shift ;;
    --branch) BRANCH="${2:-}"; shift 2 ;;
    --build-dir) BUILD_DIR="${2:-}"; shift 2 ;;
    --sdk-dist-dir) SDK_DIST_DIR="${2:-}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown option '$1'" ;;
  esac
done

[[ -n "$ARTIFACTS_DIR" ]] || die "--artifacts-dir is required"
[[ -n "$MANIFEST" ]] || die "--manifest is required"
[[ -n "$CHECKSUMS" ]] || die "--checksums is required"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" ||
  die "not inside a git repository"
cd "$repo_root"

base_version() {
  local raw
  raw="$(packaging/version.sh)"
  printf '%s\n' "${raw%%~*}"
}

safe_branch_name() {
  printf '%s\n' "$1" | tr '/[:space:]' '---' | tr -cd 'A-Za-z0-9._-'
}

derive_snapshot_version() {
  local date short_sha
  date="$(date -u +%Y%m%d)"
  short_sha="$(git rev-parse --short=12 HEAD)"
  printf '%s~dev.%s.%s\n' "$(base_version)" "$date" "$short_sha"
}

restore_staged_changelog() {
  if [[ -n "$CHANGELOG_BACKUP" && -f "$CHANGELOG_BACKUP" ]]; then
    cp -- "$CHANGELOG_BACKUP" packaging/debian/changelog
    rm -f -- "$CHANGELOG_BACKUP"
  fi
}

# Stage the version being built into the Debian changelog.
#
# dpkg takes the package version from this file, not from `--version`, so
# a build whose version differs from the tree's must rewrite it or ship a
# package labelled with the wrong version. The tree always carries the
# final release version; snapshots and release candidates do not.
write_staged_changelog() {
  local distribution="$1"
  CHANGELOG_BACKUP="$(mktemp)"
  cp -- packaging/debian/changelog "$CHANGELOG_BACKUP"
  trap restore_staged_changelog EXIT
  python3 - "$DEB_VERSION" "$distribution" <<'PY'
import sys
from pathlib import Path

version, distribution = sys.argv[1], sys.argv[2]
path = Path("packaging/debian/changelog")
lines = path.read_text().splitlines()
if not lines:
    raise SystemExit("packaging/debian/changelog is empty")
lines[0] = f"tensorplate ({version}-1) {distribution}; urgency=medium"
path.write_text("\n".join(lines) + "\n")
PY
}

BRANCH="${BRANCH:-$(git symbolic-ref --quiet --short HEAD 2>/dev/null || git rev-parse --short=12 HEAD)}"
if ((SNAPSHOT)); then
  VERSION="${VERSION:-$(derive_snapshot_version)}"
  TAG="${TAG:-snapshot-$(safe_branch_name "$BRANCH")-$(git rev-parse --short=12 HEAD)}"
  SKIP_TAG_VERIFY=1
  [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+~dev\.[0-9]{8}\.[0-9a-f]+$ ]] ||
    die "snapshot --version must look like X.Y.Z~dev.YYYYMMDD.gitsha"
  DEB_VERSION="$VERSION"
  PYTHON_VERSION="$VERSION"
else
  [[ -n "$VERSION" ]] || die "--version is required"
  [[ -n "$TAG" ]] || die "--tag is required"
  # The canonical version stays bare. Downstream consumers disagree about
  # syntax -- the manifest generator and the installer both reject a
  # Debian-form version -- so the candidate identity travels in
  # --deb-version and nowhere else.
  [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "--version must be MAJOR.MINOR.PATCH for release builds"
  expected_deb="$VERSION"
  if [[ -n "$TAG" ]]; then
    case "$TAG" in
      "v${VERSION}") ;;
      "v${VERSION}-rc."*) expected_deb="${VERSION}~rc.${TAG##*-rc.}" ;;
      *) die "--tag ${TAG} is not a tag for version ${VERSION}" ;;
    esac
  fi
  DEB_VERSION="${DEB_VERSION:-$expected_deb}"
  expected_python="$VERSION"
  [[ "$expected_deb" == *"~rc."* ]] && expected_python="${VERSION}rc${expected_deb##*~rc.}"
  PYTHON_VERSION="${PYTHON_VERSION:-$expected_python}"
  [[ "$PYTHON_VERSION" == "$expected_python" ]] ||
    die "--python-version ${PYTHON_VERSION} contradicts --tag ${TAG:-<none>}; expected ${expected_python}"
  # `~rc.N` is the Debian prerelease form, and the tilde is load-bearing:
  # it sorts BELOW the bare version, so 0.2.1~rc.1-1 < 0.2.1-1 and apt
  # offers the final release as an upgrade. Building a candidate as plain
  # 0.2.1 produced a package indistinguishable from the real release --
  # same version to dpkg, so no upgrade path off it at all.
  [[ "$DEB_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(~rc\.[1-9][0-9]*)?$ ]] ||
    die "--deb-version must be X.Y.Z or X.Y.Z~rc.N"
  [[ "$DEB_VERSION" == "$expected_deb" ]] ||
    die "--deb-version ${DEB_VERSION} contradicts --tag ${TAG:-<none>}; expected ${expected_deb}"
fi

host_arch="$(dpkg --print-architecture)"
CROSS_BUILD=0
if [[ "$TARGET_ARCH" != "$host_arch" ]]; then
  if ((SNAPSHOT)); then
    CROSS_BUILD=1
  else
    die "runner architecture $host_arch does not match release target $TARGET_ARCH"
  fi
fi

if ((SNAPSHOT)); then
  write_staged_changelog UNRELEASED
elif [[ "$DEB_VERSION" == *"~"* ]]; then
  # A release candidate is published and installable, so it takes a real
  # distribution rather than UNRELEASED. Without this the candidate would
  # be built from the tree's changelog and labelled with the final
  # release's version -- the same version, so no upgrade path off it.
  write_staged_changelog unstable
fi

note "validating release installer"
[[ -f "$INSTALLER_SOURCE" ]] || die "missing installer script at $INSTALLER_SOURCE"
bash -n "$INSTALLER_SOURCE"
command -v shellcheck >/dev/null 2>&1 || die "shellcheck is required to validate $INSTALLER_SOURCE"
shellcheck "$INSTALLER_SOURCE"

# The apt bootstrap keyring is the repository trust root. Refuse to build
# publish-grade artifacts while it still holds the reviewed staging
# placeholder (replaced when the production archive signing key is
# provisioned; see packaging/apt/README.md).
note "validating apt bootstrap keyring"
keyring_asc="packaging/apt/tensorplate-archive-keyring.asc"
[[ -f "$keyring_asc" ]] || die "missing apt bootstrap keyring at $keyring_asc"
if grep -q 'STAGING PLACEHOLDER' "$keyring_asc"; then
  if ((SNAPSHOT)) || ((SKIP_TAG_VERIFY)); then
    note "WARNING: $keyring_asc holds the staging placeholder key; these artifacts must not be published"
  else
    die "$keyring_asc still holds the staging placeholder key; provision the production archive signing key (see packaging/apt/README.md) before building publishable release artifacts"
  fi
fi

note "building Rust release binaries"
# Runtime identity, resolved before anything is compiled. Both toolchains
# have to agree: the Rust crates read TP_RELEASE_VERSION and the C++ build
# takes TP_RUNTIME_VERSION_SUFFIX, and a build that set one after the
# other had run shipped a candidate whose CLI reported the final version.
runtime_version_suffix=""
if [[ "$DEB_VERSION" == *"~"* ]]; then
  runtime_version_suffix="${DEB_VERSION#*~}"
fi
# The semver spelling of the same identity, for the Rust crates: Cargo
# metadata cannot express `~`, and the tag already carries `-rc.N`.
if [[ -n "$TAG" ]]; then
  export TP_RELEASE_VERSION="${TAG#v}"
elif [[ -n "$runtime_version_suffix" ]]; then
  export TP_RELEASE_VERSION="${VERSION}-${runtime_version_suffix}"
fi

cargo_args=(
  build
  --release
  --bin tensorplate-agent
  --bin tensorplate-observability
  --bin tensorplate
)
if ((CROSS_BUILD)); then
  case "$TARGET_ARCH" in
    arm64) CARGO_TARGET="${TP_RUST_TARGET:-aarch64-unknown-linux-gnu}" ;;
    *) die "snapshot cross-build only knows a Rust target for $TARGET_ARCH" ;;
  esac
  cargo_args+=(--target "$CARGO_TARGET")
fi
cargo "${cargo_args[@]}"
if ((CROSS_BUILD)); then
  mkdir -p target/release
  for bin in tensorplate-agent tensorplate-observability tensorplate; do
    install -m 0755 "target/${CARGO_TARGET}/release/${bin}" "target/release/${bin}"
  done
fi

note "configuring C++ release build"
if [[ -z "$BUILD_DIR" ]]; then
  if ((SNAPSHOT)); then
    BUILD_DIR="build/snapshot-${TARGET_ARCH}"
  else
    BUILD_DIR="build/release"
  fi
fi
# Everything after the tilde is the prerelease identity: `dev.DATE.SHA`
# for a snapshot, `rc.N` for a candidate, absent for a final release. The
# runtime reports it, so `tensorplate --version` distinguishes a candidate
# from the release it is a candidate for.
cmake_args=(
  -S .
  -B "$BUILD_DIR"
  -G Ninja
  -DCMAKE_BUILD_TYPE=RelWithDebInfo
  "-DTP_RUNTIME_VERSION_SUFFIX=${runtime_version_suffix}"
  -DTP_BUILD_TESTS=OFF
  -DTP_BUILD_EXAMPLES=OFF
  -DTP_ENABLE_SANITIZERS=OFF
  -DTP_ENABLE_TENSORRT="${TP_ENABLE_TENSORRT:-ON}"
  -DTP_REQUIRE_TENSORRT_SDK="${TP_REQUIRE_TENSORRT_SDK:-ON}"
  -DTP_ENABLE_LIBTORCH="${TP_ENABLE_LIBTORCH:-OFF}"
  -DTP_ENABLE_PYTHON_PYTORCH_SIDECAR="${TP_ENABLE_PYTHON_PYTORCH_SIDECAR:-ON}"
)

vcpkg_toolchain=""
if [[ -n "${TP_CMAKE_TOOLCHAIN_FILE:-}" ]]; then
  vcpkg_toolchain="$TP_CMAKE_TOOLCHAIN_FILE"
elif [[ -n "${VCPKG_ROOT:-}" && -f "${VCPKG_ROOT}/scripts/buildsystems/vcpkg.cmake" ]]; then
  vcpkg_toolchain="${VCPKG_ROOT}/scripts/buildsystems/vcpkg.cmake"
elif [[ -n "${VCPKG_INSTALLATION_ROOT:-}" && -f "${VCPKG_INSTALLATION_ROOT}/scripts/buildsystems/vcpkg.cmake" ]]; then
  vcpkg_toolchain="${VCPKG_INSTALLATION_ROOT}/scripts/buildsystems/vcpkg.cmake"
fi

if ((CROSS_BUILD)); then
  [[ "$TARGET_ARCH" == "arm64" ]] || die "snapshot cross-build only supports --arch arm64"
  [[ -n "${TP_JETSON_SYSROOT:-}" ]] || die "TP_JETSON_SYSROOT is required for x86-to-Jetson snapshot cross-builds"
  [[ -n "${TP_JETSON_CC:-}" ]] || die "TP_JETSON_CC is required for x86-to-Jetson snapshot cross-builds"
  [[ -n "${TP_JETSON_CXX:-}" ]] || die "TP_JETSON_CXX is required for x86-to-Jetson snapshot cross-builds"
  [[ -n "$vcpkg_toolchain" ]] || die "vcpkg toolchain is required for x86-to-Jetson snapshot cross-builds; set VCPKG_ROOT or TP_CMAKE_TOOLCHAIN_FILE"
  cmake_args+=(
    "-DCMAKE_TOOLCHAIN_FILE=${vcpkg_toolchain}"
    "-DVCPKG_CHAINLOAD_TOOLCHAIN_FILE=${repo_root}/cmake/toolchains/aarch64-jetson.cmake"
    "-DVCPKG_TARGET_TRIPLET=arm64-linux"
  )
elif [[ -n "${TP_CMAKE_TOOLCHAIN_FILE:-}" ]]; then
  cmake_args+=("-DCMAKE_TOOLCHAIN_FILE=${TP_CMAKE_TOOLCHAIN_FILE}")
elif [[ -n "$vcpkg_toolchain" ]]; then
  cmake_args+=("-DCMAKE_TOOLCHAIN_FILE=${vcpkg_toolchain}")
fi

cmake "${cmake_args[@]}"

note "building serving worker"
cmake --build "$BUILD_DIR" --target tp_serving_worker --parallel
mkdir -p build/release
if [[ -x "${BUILD_DIR}/serving_worker/tensorplate-serving" ]]; then
  install -m 0755 "${BUILD_DIR}/serving_worker/tensorplate-serving" build/release/tensorplate-serving
elif [[ -x "${BUILD_DIR}/tensorplate-serving" ]]; then
  install -m 0755 "${BUILD_DIR}/tensorplate-serving" build/release/tensorplate-serving
fi
[[ -x build/release/tensorplate-serving ]] ||
  die "serving worker binary was not staged at build/release/tensorplate-serving"

note "running packaging verification suite"
test/packaging/run.sh

note "building Debian packages"
build_deb_args=()
if ((CROSS_BUILD)); then
  build_deb_args+=("-a" "$TARGET_ARCH")
fi
packaging/scripts/build-deb.sh "${build_deb_args[@]}"

mkdir -p "$ARTIFACTS_DIR"
find "$ARTIFACTS_DIR" -maxdepth 1 -type f -name 'tensorplate*.deb' -delete

repo_parent="$(dirname "$repo_root")"
debs=()
for pkg in "${REQUIRED_PACKAGES[@]}"; do
  matches=()
  mapfile -t candidates < <(find "$repo_parent" -maxdepth 1 -type f -name "${pkg}_${DEB_VERSION}-*_*.deb" | sort)
  for candidate in "${candidates[@]}"; do
    candidate_name="$(basename -- "$candidate")"
    case "$candidate_name" in
      ${pkg}_${DEB_VERSION}-*_${TARGET_ARCH}.deb|${pkg}_${DEB_VERSION}-*_all.deb)
        matches+=("$candidate")
        ;;
    esac
  done
  ((${#matches[@]} == 1)) ||
    die "expected exactly one ${pkg}_${DEB_VERSION}-*_${TARGET_ARCH}.deb or ${pkg}_${DEB_VERSION}-*_all.deb in $repo_parent; found ${#matches[@]}"
  debs+=("${matches[0]}")
done
# Collect the secondary-architecture runtime set staged alongside the
# primary build. Matching per package name (rather than a `tensorplate-*`
# wildcard) keeps auto-generated -dbgsym packages out of the release, and
# keeps the exactly-one assertion above scoped to the primary target.
if [[ "$TARGET_ARCH" != "$SECONDARY_ARCH" ]]; then
  for pkg in "${SECONDARY_ARCH_PACKAGES[@]}"; do
    mapfile -t matches < <(find "$repo_parent" -maxdepth 1 -type f \
      -name "${pkg}_${DEB_VERSION}-*_${SECONDARY_ARCH}.deb" | sort)
    if ((${#matches[@]} == 1)); then
      debs+=("${matches[0]}")
      continue
    fi
    if ((${#matches[@]} > 1)); then
      die "expected at most one ${pkg}_${DEB_VERSION}-*_${SECONDARY_ARCH}.deb in $repo_parent; found ${#matches[@]}"
    fi
    # Releases ship the complete x86_64 runtime set from the same asset set
    # (the release workflow's hosted amd64 job stages the packages), and
    # manifest generation rejects release artifact sets without it. Only
    # single-architecture local-source snapshots may omit it.
    if ((SNAPSHOT)); then
      note "WARNING: no ${pkg} ${SECONDARY_ARCH} package staged; snapshot artifacts omit it"
    else
      die "missing ${pkg}_${DEB_VERSION}-*_${SECONDARY_ARCH}.deb in $repo_parent; the release workflow's ${SECONDARY_ARCH} packaging job must stage it before release artifact builds"
    fi
  done
fi
cp "${debs[@]}" "$ARTIFACTS_DIR/"
install -m 0755 "$INSTALLER_SOURCE" "$ARTIFACTS_DIR/install.sh"

# The installer is published as a release asset, and its documented flow is
# to run it with no arguments. Copied verbatim it carries whatever default
# the branch happened to hold, so a v0.2.1 asset installed some older
# release -- the one thing a user downloading it from THIS release cannot
# be expected to check. The env override is preserved.
install_default="${TAG:-$VERSION}"
sed -i.bak -E \
  "s|(TP_INSTALL_DEFAULT_VERSION:-)[^}]*|\1${install_default}|" \
  "$ARTIFACTS_DIR/install.sh"
rm -f "$ARTIFACTS_DIR/install.sh.bak"
grep -Fq "TP_INSTALL_DEFAULT_VERSION:-${install_default}}" "$ARTIFACTS_DIR/install.sh" ||
  die "install.sh was not stamped with ${install_default}; the published installer would default to another release"

# The tensorplate-python SDK wheel + sdist are built by a separate hosted
# job (pure Python; no Jetson toolchain) and staged here so they are covered
# by the same signed manifest and SHA256SUMS as the runtime/CLI assets.
if [[ -n "$SDK_DIST_DIR" ]]; then
  note "staging tensorplate-python SDK wheel and sdist"
  shopt -s nullglob
  sdk_dists=("$SDK_DIST_DIR"/tensorplate_python-*.whl "$SDK_DIST_DIR"/tensorplate_python-*.tar.gz)
  shopt -u nullglob
  ((${#sdk_dists[@]} == 2)) ||
    die "expected one tensorplate-python wheel and one sdist in $SDK_DIST_DIR; found ${#sdk_dists[@]}"
  cp "${sdk_dists[@]}" "$ARTIFACTS_DIR/"
fi

note "generating manifest and checksums"
manifest_args=(
  manifest
  --version "$VERSION" \
  --deb-version "$DEB_VERSION" \
  --python-version "$PYTHON_VERSION" \
  --tag "$TAG" \
  --artifacts-dir "$ARTIFACTS_DIR" \
  --manifest "$MANIFEST" \
  --checksums "$CHECKSUMS" \
  --target-os "$TARGET_OS" \
  --arch "$TARGET_ARCH"
)
if ((SNAPSHOT)); then
  manifest_args+=(--release-branch "$BRANCH")
  manifest_args+=(--allow-snapshot-version)
fi
tools/release/tensorplate-release.sh "${manifest_args[@]}"

verify_args=(
  verify
  --version "$VERSION"
  --deb-version "$DEB_VERSION"
  --python-version "$PYTHON_VERSION"
  --tag "$TAG"
  --artifacts-dir "$ARTIFACTS_DIR"
  --manifest "$MANIFEST"
  --checksums "$CHECKSUMS"
)
if [[ "$SKIP_TAG_VERIFY" -eq 1 ]]; then
  verify_args+=(--skip-tag-verify)
fi
if ((SNAPSHOT)); then
  verify_args+=(--allow-snapshot-version)
fi
tools/release/tensorplate-release.sh "${verify_args[@]}"

if ((SNAPSHOT)); then
  note "unreleased snapshot artifacts are ready in $ARTIFACTS_DIR"
  printf 'Snapshot version: %s\n' "$VERSION"
  printf 'Snapshot tag: %s\n' "$TAG"
  printf 'Source branch/provenance label: %s\n' "$BRANCH"
else
  note "release artifacts are ready in $ARTIFACTS_DIR"
fi
