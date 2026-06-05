#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Build unreleased TensorPlate snapshot artifacts from a source branch, then
# install them through the same installer path used for release artifacts.

set -Eeuo pipefail

DEFAULT_BRANCH="${TP_SOURCE_INSTALL_BRANCH:-develop}"
DEFAULT_REPO_URL="${TP_SOURCE_INSTALL_REPO_URL:-https://github.com/tensorplate/tensorplate.git}"

BRANCH="$DEFAULT_BRANCH"
REPO_URL="$DEFAULT_REPO_URL"
SOURCE_DIR=""
ARTIFACTS_DIR=""
TARGET_ARCH="${TP_SOURCE_INSTALL_ARCH:-arm64}"
WITH_PYTHON_BACKEND=0
CLI_ONLY=0
NO_INSTALL=0
NO_FETCH=0
KEEP_WORKTREE=0
SOURCE_ROOT=""
WORK_PARENT=""
SNAPSHOT_VERSION=""
SNAPSHOT_TAG=""

usage() {
  cat <<'EOF'
Usage:
  sudo bash build-install-from-source.sh --branch develop [options]

Options:
  --branch BRANCH            Source branch, tag, or ref to build. Defaults to develop.
  --with-python-backend      Install tensorplate-backend-python-pytorch after building.
                             Runtime install mode only.
  --cli-only                 Install only tensorplate-common and tensorplate-cli from
                             the built snapshot artifacts.
  --no-install               Build and verify artifacts only; print the local install command.
  --arch ARCH                Debian target architecture. Defaults to arm64.
  --repo URL                 Git repository to clone when the script is not run from a checkout.
  --source-dir DIR           Use an existing checked-out source tree instead of clone/worktree.
  --artifacts-dir DIR        Output directory for snapshot artifacts.
  --no-fetch                 Do not fetch origin before resolving --branch from a checkout.
  --keep-worktree            Keep the temporary clone/worktree after exit.
  --help, -h                 Show this help text.

Snapshot artifacts are unreleased, unsigned local-source builds. They are
verified with local SHA256SUMS, then installed with install.sh
--local-artifacts --allow-unsigned so there is only one install implementation.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '==> %s\n' "$*"
}

command_exists() {
  command -v "$1" >/dev/null 2>&1
}

abs_dir() {
  local path="$1"
  mkdir -p "$path"
  cd -- "$path" && pwd
}

safe_name() {
  printf '%s\n' "$1" | tr '/[:space:]' '---' | tr -cd 'A-Za-z0-9._-'
}

cleanup() {
  if [[ "$KEEP_WORKTREE" -eq 1 ]]; then
    [[ -n "$SOURCE_ROOT" ]] && note "kept source checkout at $SOURCE_ROOT"
    return 0
  fi
  if [[ -n "$WORK_PARENT" && -d "$WORK_PARENT" ]]; then
    rm -rf -- "$WORK_PARENT"
  fi
}

while (($# > 0)); do
  case "$1" in
    --branch)
      (($# >= 2)) || die "--branch requires a value"
      BRANCH="$2"
      shift 2
      ;;
    --with-python-backend)
      WITH_PYTHON_BACKEND=1
      shift
      ;;
    --cli-only)
      CLI_ONLY=1
      shift
      ;;
    --no-install)
      NO_INSTALL=1
      shift
      ;;
    --arch)
      (($# >= 2)) || die "--arch requires a value"
      TARGET_ARCH="$2"
      shift 2
      ;;
    --repo)
      (($# >= 2)) || die "--repo requires a value"
      REPO_URL="$2"
      shift 2
      ;;
    --source-dir)
      (($# >= 2)) || die "--source-dir requires a directory"
      SOURCE_DIR="$2"
      shift 2
      ;;
    --artifacts-dir)
      (($# >= 2)) || die "--artifacts-dir requires a directory"
      ARTIFACTS_DIR="$2"
      shift 2
      ;;
    --no-fetch)
      NO_FETCH=1
      shift
      ;;
    --keep-worktree)
      KEEP_WORKTREE=1
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

if [[ "$CLI_ONLY" -eq 1 && "$WITH_PYTHON_BACKEND" -eq 1 ]]; then
  die "--with-python-backend cannot be combined with --cli-only"
fi
if [[ "$NO_INSTALL" -eq 0 && "${EUID}" -ne 0 ]]; then
  die "run as root for install, for example: sudo bash build-install-from-source.sh --branch ${BRANCH}; pass --no-install to build only"
fi
for cmd in git bash date; do
  command_exists "$cmd" || die "required command not found: $cmd"
done

script_path="${BASH_SOURCE[0]}"
if [[ "$script_path" != /* ]]; then
  script_path="${PWD}/${script_path}"
fi
script_dir="$(cd -- "$(dirname -- "$script_path")" && pwd)"
repo_root=""
repo_candidate_parent="$(cd -- "${script_dir}/../.." && pwd)"
if repo_root_candidate="$(git -c "safe.directory=${repo_candidate_parent}" -C "$repo_candidate_parent" rev-parse --show-toplevel 2>/dev/null)"; then
  if [[ -x "${repo_root_candidate}/packaging/version.sh" &&
    -x "${repo_root_candidate}/tools/release/build-release-artifacts.sh" ]]; then
    repo_root="$repo_root_candidate"
  fi
fi

trap cleanup EXIT

if [[ -n "$SOURCE_DIR" ]]; then
  SOURCE_ROOT="$(cd -- "$SOURCE_DIR" && pwd)" ||
    die "source directory does not exist: $SOURCE_DIR"
elif [[ -n "$repo_root" ]]; then
  WORK_PARENT="$(mktemp -d "${TMPDIR:-/tmp}/tensorplate-source-install.XXXXXX")"
  SOURCE_ROOT="${WORK_PARENT}/source"
  note "creating temporary checkout from $repo_root"
  git -c "safe.directory=${repo_root}" clone --shared "$repo_root" "$SOURCE_ROOT"
  origin_url="$(git -c "safe.directory=${repo_root}" -C "$repo_root" remote get-url origin 2>/dev/null || true)"
  if [[ -n "$origin_url" ]]; then
    git -C "$SOURCE_ROOT" remote set-url origin "$origin_url"
  fi
  if [[ "$NO_FETCH" -eq 0 && -n "$origin_url" ]]; then
    note "fetching ${BRANCH} from origin"
    git -C "$SOURCE_ROOT" fetch origin "$BRANCH"
  fi
  ref="$BRANCH"
  if git -C "$SOURCE_ROOT" rev-parse --verify --quiet "origin/${BRANCH}^{commit}" >/dev/null; then
    ref="origin/${BRANCH}"
  fi
  note "checking out ${ref}"
  git -C "$SOURCE_ROOT" checkout --detach "$ref"
else
  WORK_PARENT="$(mktemp -d "${TMPDIR:-/tmp}/tensorplate-source-install.XXXXXX")"
  SOURCE_ROOT="${WORK_PARENT}/source"
  note "cloning ${REPO_URL} branch ${BRANCH}"
  git clone --depth 1 --branch "$BRANCH" "$REPO_URL" "$SOURCE_ROOT"
fi

[[ -x "${SOURCE_ROOT}/packaging/version.sh" ]] ||
  die "source tree is missing packaging/version.sh: $SOURCE_ROOT"
[[ -x "${SOURCE_ROOT}/tools/release/build-release-artifacts.sh" ]] ||
  die "source tree is missing tools/release/build-release-artifacts.sh: $SOURCE_ROOT"

base_version="$("${SOURCE_ROOT}/packaging/version.sh")"
base_version="${base_version%%~*}"
short_sha="$(git -c "safe.directory=${SOURCE_ROOT}" -C "$SOURCE_ROOT" rev-parse --short=12 HEAD)"
build_date="$(date -u +%Y%m%d)"
safe_branch="$(safe_name "$BRANCH")"
SNAPSHOT_VERSION="${base_version}~dev.${build_date}.${short_sha}"
SNAPSHOT_TAG="snapshot-${safe_branch}-${short_sha}"

if [[ -z "$ARTIFACTS_DIR" ]]; then
  if [[ -n "$repo_root" ]]; then
    ARTIFACTS_DIR="${repo_root}/dist/source-install/${safe_branch}/${SNAPSHOT_VERSION}"
  else
    ARTIFACTS_DIR="${PWD}/tensorplate-source-install/${safe_branch}/${SNAPSHOT_VERSION}"
  fi
fi
ARTIFACTS_DIR="$(abs_dir "$ARTIFACTS_DIR")"
MANIFEST="${ARTIFACTS_DIR}/tensorplate-${SNAPSHOT_TAG}-artifacts.json"
CHECKSUMS="${ARTIFACTS_DIR}/SHA256SUMS"

note "building unreleased TensorPlate snapshot"
printf 'Source: %s\n' "$SOURCE_ROOT"
printf 'Branch/ref label: %s\n' "$BRANCH"
printf 'Commit: %s\n' "$short_sha"
printf 'Snapshot version: %s\n' "$SNAPSHOT_VERSION"
printf 'Artifact directory: %s\n' "$ARTIFACTS_DIR"

(
  cd "$SOURCE_ROOT"
  tools/release/build-release-artifacts.sh \
    --snapshot \
    --branch "$BRANCH" \
    --version "$SNAPSHOT_VERSION" \
    --tag "$SNAPSHOT_TAG" \
    --artifacts-dir "$ARTIFACTS_DIR" \
    --manifest "$MANIFEST" \
    --checksums "$CHECKSUMS" \
    --arch "$TARGET_ARCH"
)

install_args=(
  --local-artifacts "$ARTIFACTS_DIR"
  --allow-unsigned
  --yes
)
if [[ "$CLI_ONLY" -eq 1 ]]; then
  install_args+=(--cli-only)
fi
if [[ "$WITH_PYTHON_BACKEND" -eq 1 ]]; then
  install_args+=(--with-python-backend)
fi

if [[ "$NO_INSTALL" -eq 1 ]]; then
  note "snapshot artifacts built and verified; install was skipped"
  printf 'Install command:\n'
  printf '  sudo bash %q' "${ARTIFACTS_DIR}/install.sh"
  printf ' %q' "${install_args[@]}"
  printf '\n'
  printf 'Caveat: this is an unreleased, unsigned local-source snapshot. It is not GitHub Release or release-validation evidence.\n'
  exit 0
fi

note "installing unreleased snapshot through release installer path"
bash "${ARTIFACTS_DIR}/install.sh" "${install_args[@]}"

note "TensorPlate unreleased source snapshot install complete"
printf 'Installed snapshot version: %s\n' "$SNAPSHOT_VERSION"
printf 'Installed from branch/ref label: %s\n' "$BRANCH"
printf 'Artifacts: %s\n' "$ARTIFACTS_DIR"
printf 'Caveat: this is an unreleased, unsigned local-source snapshot. It is not GitHub Release or release-validation evidence.\n'
