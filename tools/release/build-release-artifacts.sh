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
  build-release-artifacts.sh --version 0.1.0 --tag v0.1.0 --artifacts-dir DIR [options]
  build-release-artifacts.sh --snapshot --branch develop --artifacts-dir DIR [options]

Options:
  --version VERSION      Canonical release version, for example 0.1.0. Always
                         bare MAJOR.MINOR.PATCH for a release build; this is
                         what the source tree and release manifest record.
  --python-version VER   PEP 440 SDK version, for example 0.1.0rc1. Derived
                         from --tag when omitted; names the wheel and sdist.
  --deb-version VERSION  Debian package version, for example 0.1.0~rc.1.
                         Derived from --tag when omitted. The `~` sorts below
                         the bare version, so the final release is an upgrade
                         from the candidate.
  --tag TAG              Git tag being published, for example v0.1.0.
  --artifacts-dir DIR    Output directory for .deb artifacts.
  --manifest FILE        Artifact manifest JSON path. Defaults to
                         DIR/tensorplate-TAG-artifacts.json: the name a URL
                         install fetches, in the directory
                         install.sh --local-artifacts reads. Any other is
                         refused.
  --checksums FILE       SHA256SUMS output path. Defaults to DIR/SHA256SUMS;
                         any other is refused.
  --target-os VALUE      Manifest target OS label.
  --arch ARCH            Manifest target architecture. Defaults to arm64.
                         amd64 configures the serving worker from
                         tools/release/amd64-build-profile.sh, as the release
                         workflow does, and refuses TP_ENABLE_TENSORRT,
                         TP_REQUIRE_TENSORRT_SDK, TP_ENABLE_LIBTORCH and
                         TP_ENABLE_PYTHON_PYTORCH_SIDECAR overrides.
  --skip-tag-verify      Verify manifest/checksums without requiring an annotated tag.
  --snapshot             Build unreleased local-source snapshot artifacts.
  --branch BRANCH        Provenance label recorded in the manifest's
                         release.branch field. Not necessarily a branch: the
                         publish path passes the trunk, and a build-only
                         rehearsal passes the ref it was dispatched on, which
                         the runbook makes the tag's own commit -- so a
                         rehearsal manifest records a 40-hex SHA equal to
                         release.commit. Defaults to the checked-out branch, or
                         the short commit when HEAD is detached, which it is on
                         the tag-driven release path.
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

# The name GitHub serves a release asset under. GitHub rewrites `~` to `.`
# in the name of an uploaded asset: v0.2.1-rc.1 uploaded
# tensorplate-agent_0.2.1~rc.1-1_arm64.deb, and the release serves it only
# as tensorplate-agent_0.2.1.rc.1-1_arm64.deb (the tilde URL is a 404),
# while its signed SHA256SUMS and manifest named the tilde file. `~` is
# the one rewrite relied on because it is the one observed; manifest
# generation refuses any asset name that still carries one.
#
# Only the file name changes. The package's control Version stays
# 0.2.1~rc.1-1, and that is what apt, dpkg and the lifecycle harnesses
# order on. The file name must never be: 0.2.1.rc.1 sorts ABOVE 0.2.1.
release_asset_name() {
  printf '%s\n' "${1//\~/.}"
}

# Copy each package into the artifacts directory under the name GitHub
# will serve it as. This is the only way a .deb enters that directory, so
# the file on disk, the manifest's `file`, SHA256SUMS, the signature over
# it and the published asset all carry one name.
stage_release_debs() {
  local dest="$1" deb name published
  shift
  for deb in "$@"; do
    name="$(basename -- "$deb")"
    # Manifest generation recovers the package version from the published
    # name by restoring the tilde of --deb-version, which is exact only
    # while that is the only tilde the name holds.
    if [[ "${name#*_"${DEB_VERSION}"-}" == *"~"* ]]; then
      die "$name carries a '~' outside its package version ${DEB_VERSION}; its published name would not identify its version"
    fi
    published="$(release_asset_name "$name")"
    if [[ -e "${dest%/}/${published}" ]]; then
      die "two packages would be published as ${published}"
    fi
    cp -- "$deb" "${dest%/}/${published}" ||
      die "could not stage $name as ${dest%/}/${published}"
  done
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

# Refuse to relabel one source release as another. Release-candidate and
# snapshot identities add a suffix to the source's numeric version, but all
# three version authorities in the checkout must still agree on that base.
# Keep this check independent of Cargo and CMake so it runs before either
# toolchain (and before the Debian changelog is staged).
verify_source_version_identity() {
  local expected_source_version packaging_source_version
  expected_source_version="${VERSION%%~*}"
  packaging_source_version="$(base_version)"

  [[ "$packaging_source_version" == "$expected_source_version" ]] ||
    die "source version mismatch: --version base ${expected_source_version} does not match packaging/VERSION base ${packaging_source_version}"

  python3 - "$expected_source_version" <<'PY'
import re
import sys
from pathlib import Path

expected = sys.argv[1]


def fail(message: str) -> None:
    raise SystemExit(f"error: source version mismatch: {message}")


cmake_text = Path("CMakeLists.txt").read_text()
project_match = re.search(
    r"(?ms)^\s*project\s*\((.*?)^\s*\)",
    cmake_text,
)
if project_match is None:
    fail("cannot read project() from CMakeLists.txt")
cmake_match = re.search(
    r"(?m)^\s*VERSION\s+([0-9]+\.[0-9]+\.[0-9]+)\s*$",
    project_match.group(1),
)
if cmake_match is None:
    fail("cannot read project VERSION from CMakeLists.txt")
cmake_version = cmake_match.group(1)
if cmake_version != expected:
    fail(
        f"--version base {expected} does not match "
        f"CMakeLists.txt project VERSION {cmake_version}"
    )

cargo_text = Path("Cargo.toml").read_text()
workspace_match = re.search(
    r"(?ms)^\s*\[workspace\.package\]\s*(.*?)(?=^\s*\[|\Z)",
    cargo_text,
)
if workspace_match is None:
    fail("cannot read [workspace.package] from Cargo.toml")
cargo_match = re.search(
    r'''(?m)^\s*version\s*=\s*["']([0-9]+\.[0-9]+\.[0-9]+)(?:[-+][^"']+)?["']\s*(?:#.*)?$''',
    workspace_match.group(1),
)
if cargo_match is None:
    fail("cannot read [workspace.package].version from Cargo.toml")
cargo_version = cargo_match.group(1)
if cargo_version != expected:
    fail(
        f"--version base {expected} does not match "
        f"Cargo.toml workspace package version {cargo_version}"
    )
PY
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
      "v${VERSION}-rc."*)
        candidate_prefix="v${VERSION}-rc."
        candidate_number="${TAG#"$candidate_prefix"}"
        [[ "$candidate_number" =~ ^[1-9][0-9]*$ ]] ||
          die "--tag ${TAG} must end in a positive numeric release-candidate number"
        expected_deb="${VERSION}~rc.${candidate_number}"
        ;;
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

verify_source_version_identity

host_arch="$(dpkg --print-architecture)"
CROSS_BUILD=0
if [[ "$TARGET_ARCH" != "$host_arch" ]]; then
  if ((SNAPSHOT)); then
    CROSS_BUILD=1
  else
    die "runner architecture $host_arch does not match release target $TARGET_ARCH"
  fi
fi

# The checks from here to the changelog staging refuse, before anything is
# compiled, a build that would otherwise fail or produce an uninstallable
# set only after cargo and the C++ build had run.

# install.sh --local-artifacts reads exactly one tensorplate-*-artifacts.json
# and SHA256SUMS from the artifacts directory itself, and a URL install
# fetches tensorplate-${TAG}-artifacts.json by that name. A manifest written
# anywhere else produces a set nothing can install.
MANIFEST="${MANIFEST:-${ARTIFACTS_DIR%/}/tensorplate-${TAG}-artifacts.json}"
CHECKSUMS="${CHECKSUMS:-${ARTIFACTS_DIR%/}/SHA256SUMS}"

# Directories are compared physically, so a symlink or `..` spelling of the
# artifacts directory is accepted. CDPATH is cleared because with it set
# `cd` prints the directory it changed to, and the substitution would
# return that path twice.
physical_dir() {
  CDPATH='' cd -- "$1" 2>/dev/null && pwd -P
}

artifacts_dir_physical="$(mkdir -p -- "$ARTIFACTS_DIR" && physical_dir "$ARTIFACTS_DIR")" ||
  die "cannot create --artifacts-dir $ARTIFACTS_DIR"

require_in_artifacts_dir() {
  local flag="$1" path="$2" name="$3" parent=""
  parent="$(physical_dir "$(dirname -- "$path")")" || parent=""
  if [[ "${path##*/}" != "$name" || "$parent" != "$artifacts_dir_physical" ]]; then
    die "$flag must be ${ARTIFACTS_DIR%/}/${name}, the file install.sh reads; omit $flag to use it"
  fi
}
require_in_artifacts_dir --manifest "$MANIFEST" "tensorplate-${TAG}-artifacts.json"
require_in_artifacts_dir --checksums "$CHECKSUMS" SHA256SUMS

if [[ -z "$BUILD_DIR" ]]; then
  if ((SNAPSHOT)); then
    BUILD_DIR="build/snapshot-${TARGET_ARCH}"
  else
    BUILD_DIR="build/release"
  fi
fi

# The amd64 serving worker is configured from the profile the release
# workflow's amd64 job reads, so a snapshot is built the way the release is.
if [[ "$TARGET_ARCH" == "$SECONDARY_ARCH" ]]; then
  # Checked first: under errexit, bash 3.2 exits on a failed `.` before
  # any `|| die` could name the file.
  if [[ ! -r tools/release/amd64-build-profile.sh ]]; then
    die "cannot read tools/release/amd64-build-profile.sh"
  fi
  # shellcheck source=tools/release/amd64-build-profile.sh disable=SC1091
  . tools/release/amd64-build-profile.sh
  for override in TP_ENABLE_TENSORRT TP_REQUIRE_TENSORRT_SDK TP_ENABLE_LIBTORCH TP_ENABLE_PYTHON_PYTORCH_SIDECAR; do
    if [[ -n "${!override:-}" ]]; then
      die "$override is set; an $SECONDARY_ARCH build takes it from tools/release/amd64-build-profile.sh, as the release does. Unset it"
    fi
  done
  if ! command -v "$TP_AMD64_CXX" >/dev/null 2>&1; then
    die "$TP_AMD64_CXX is required for an $SECONDARY_ARCH build (tools/release/amd64-build-profile.sh); install it first"
  fi
  # CMake reads CXX only until a build directory records a compiler, so a
  # directory configured with another compiler would silently keep it. A
  # cache with no compiler recorded, left by a configure that stopped before
  # compiler detection (no Ninja, say), still takes CXX from the environment.
  if [[ -f "${BUILD_DIR}/CMakeCache.txt" ]]; then
    cached_cxx="$(sed -n 's/^CMAKE_CXX_COMPILER:[A-Z]*=//p' "${BUILD_DIR}/CMakeCache.txt")" ||
      cached_cxx=""
    if [[ -n "$cached_cxx" && "${cached_cxx##*/}" != "${TP_AMD64_CXX##*/}" ]]; then
      die "$BUILD_DIR was configured with C++ compiler '${cached_cxx}', not $TP_AMD64_CXX; remove $BUILD_DIR or pass another --build-dir"
    fi
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
# metadata cannot express `~`, so the package version's tilde becomes a
# hyphen and everything else is already shared.
#
# Derived from the package version rather than from the tag, because a
# snapshot's tag is `snapshot-<branch>-<sha>` and using it made the Rust
# crates report a non-numeric version. That is not cosmetic: the loose
# parser in protocol takes the segment before the first hyphen, so
# `snapshot-...` parsed as 0.0.0 and a snapshot agent rejected the shipped
# backend and otherwise compatible bundles on version-floor checks.
export TP_RELEASE_VERSION="${DEB_VERSION/\~/-}"

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
)
if [[ "$TARGET_ARCH" == "$SECONDARY_ARCH" ]]; then
  cmake_args+=("${TP_AMD64_CMAKE_ARGS[@]}")
else
  cmake_args+=(
    -DTP_ENABLE_TENSORRT="${TP_ENABLE_TENSORRT:-ON}"
    -DTP_REQUIRE_TENSORRT_SDK="${TP_REQUIRE_TENSORRT_SDK:-ON}"
    -DTP_ENABLE_LIBTORCH="${TP_ENABLE_LIBTORCH:-OFF}"
    -DTP_ENABLE_PYTHON_PYTORCH_SIDECAR="${TP_ENABLE_PYTHON_PYTORCH_SIDECAR:-ON}"
  )
fi

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

if [[ "$TARGET_ARCH" == "$SECONDARY_ARCH" ]]; then
  CC="$TP_AMD64_CC" CXX="$TP_AMD64_CXX" cmake "${cmake_args[@]}" ||
    die "C++ configure failed"
else
  cmake "${cmake_args[@]}"
fi

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

# The lifecycle harness verifiers check the validation tooling, not the
# artifacts being built, so the build runs only the packaging checks.
note "running packaging verification suite"
test/packaging/run.sh core

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
stage_release_debs "$ARTIFACTS_DIR" "${debs[@]}"
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
manifest_args+=(--release-branch "$BRANCH")
if ((SNAPSHOT)); then
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
  printf 'Source provenance label recorded in the manifest: %s\n' "$BRANCH"
fi
