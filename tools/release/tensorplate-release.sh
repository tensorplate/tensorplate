#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# GitOps-oriented release driver for TensorPlate.
#
# The implementation PR adds this tooling, but it must not publish the
# release. Final publication happens later from the per-minor release/X.Y
# maintenance line: this script creates the reviewed source commit/tag, and
# CI owns artifact publication.

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
# A release publishes one primary runtime architecture (the target the
# manifest describes) plus one secondary runtime architecture built on a
# separate native runner. Every package below is required at the secondary
# architecture on the publish path, and no other package may appear there:
# an unlisted foreign-arch .deb is a staging mistake, not an extra asset.
# The architecture-independent packages are shared and stay `all`.
readonly SECONDARY_ARCH="amd64"
readonly SECONDARY_ARCH_PACKAGES=(
  tensorplate-agent
  tensorplate-serving
  tensorplate-observability
  tensorplate-cli
  tensorplate
)
# The secondary set is built on the oldest LTS it must run on, so its
# shared-library floor admits both supported Ubuntu releases.
readonly SECONDARY_TARGET_OS="Ubuntu 22.04 LTS / 24.04 LTS (x86_64)"
readonly APPROVED_PREPARE_FILES=(
  CMakeLists.txt
  Cargo.toml
  Cargo.lock
  vcpkg.json
  packaging/VERSION
  packaging/debian/changelog
  CHANGELOG.md
)

usage() {
  cat <<'EOF'
Usage:
  tensorplate-release.sh preflight --version 0.1.0 [options]
  tensorplate-release.sh prepare --version 0.1.0 --dry-run
  tensorplate-release.sh prepare --version 0.1.0 --execute --confirm PREPARE-v0.1.0
  tensorplate-release.sh cut --version 0.1.0 --final --execute --push --confirm CUT-v0.1.0
  tensorplate-release.sh cut --version 0.1.0 --rc 1 --execute --push --confirm CUT-v0.1.0-rc.1
  tensorplate-release.sh manifest --version 0.1.0 --tag v0.1.0 --artifacts-dir DIR [options]
  tensorplate-release.sh verify --version 0.1.0 --tag v0.1.0 --manifest FILE --checksums FILE --artifacts-dir DIR
  tensorplate-release.sh tag --version 0.1.0 (--rc N | --final) --confirm CREATE-v0.1.0[-rc.N] [options]
  tensorplate-release.sh publish --version 0.1.0 --tag v0.1.0 --manifest FILE --checksums FILE --artifacts-dir DIR [options]

Common options:
  --version VERSION          Release version without leading v, for example 0.1.0.
  --release-branch BRANCH   Expected release branch. Defaults to the
                            maintenance line release/MAJOR.MINOR.
  --base REF                Source ref for cut. Defaults to origin/develop.
  --prep-branch BRANCH      Additional branch accepted for preflight during tooling PR dry runs.
  --artifacts-dir DIR       Directory containing release artifacts.
  --manifest FILE           Artifact manifest JSON path.
  --checksums FILE          SHA256SUMS path.
  --signoff FILE            Final release sign-off record.
  --validation-report FILE  Release validation report path.
  --clean-room-report FILE  Clean-room validation report path.
  --release-notes FILE      Release notes used for GitHub Release creation.
  --report FILE             Preflight report output path.
  --skip-ci                 Record CI as blocked instead of querying GitHub.
  --skip-tag-verify         For build-only artifact validation, verify manifest
                            and checksums without requiring a local annotated tag.
  --allow-snapshot-version  Allow X.Y.Z~dev.YYYYMMDD.gitsha for unreleased
                            local-source snapshot manifest/verify operations.
  --dry-run                 Print intended action without mutating repository or GitHub state.
  --execute                 Execute a mutating operation.
  --push                    Push the release branch and tag after cut.
  --confirm TOKEN           Required confirmation token for mutating operations.

Subcommands:
  preflight   Final release readiness check. Fails closed on missing evidence,
              dirty worktree, version drift, missing artifacts, missing checksums,
              missing sign-off, unavailable CI, or tag conflicts.
  prepare     Promote release metadata from development values to final values.
              Defaults to non-mutating dry-run unless --execute and confirmation
              are provided.
  cut         Create/switch the release branch, prepare release metadata,
              commit it, create an annotated source tag, and optionally push
              branch + tag so CI builds and publishes release assets.
  manifest    Generate artifact manifest JSON and SHA256SUMS for .deb assets and install.sh.
  verify      Verify an annotated tag plus manifest/checksum/artifact integrity.
  tag         Create an annotated RC or final tag. Never pushes tags.
  publish     Validate assets and create a draft GitHub Release when --execute is
              explicitly confirmed. Dry-run is the default. Requires the
              cosign-signed SHA256SUMS.cosign.bundle next to the checksums.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '==> %s\n' "$*"
}

repo_root() {
  git rev-parse --show-toplevel 2>/dev/null
}

require_repo() {
  local root
  root="$(repo_root)" || die "not inside a git repository"
  cd "$root"
}

require_version() {
  [[ -n "${VERSION:-}" ]] || die "--version is required"
  if [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    return 0
  fi
  if [[ "${ALLOW_SNAPSHOT_VERSION:-0}" -eq 1 &&
    "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+~dev\.[0-9]{8}\.[0-9a-f]+$ ]]; then
    return 0
  fi
  die "--version must be MAJOR.MINOR.PATCH; pass --allow-snapshot-version for X.Y.Z~dev.YYYYMMDD.gitsha"
}

version_short() {
  local base major minor
  base="${VERSION%%~*}"
  IFS=. read -r major minor _ <<<"$base"
  printf '%s.%s\n' "$major" "$minor"
}

default_paths() {
  require_version
  # Tags live on the per-minor maintenance line (release/0.1, release/0.2);
  # per-patch release branches are not created.
  RELEASE_BRANCH="${RELEASE_BRANCH:-release/${VERSION%.*}}"
  ARTIFACTS_DIR="${ARTIFACTS_DIR:-dist/release/v${VERSION}}"
  MANIFEST="${MANIFEST:-dist/release/v${VERSION}/tensorplate-v${VERSION}-artifacts.json}"
  CHECKSUMS="${CHECKSUMS:-dist/release/v${VERSION}/SHA256SUMS}"
  SIGNOFF="${SIGNOFF:-dist/release/v${VERSION}/signoff.md}"
  VALIDATION_REPORT="${VALIDATION_REPORT:-docs/validation/orin-release-validation.md}"
  CLEAN_ROOM_REPORT="${CLEAN_ROOM_REPORT:-docs/validation/clean-room-release-smoke.md}"
  RELEASE_NOTES="${RELEASE_NOTES:-docs/release/notes/v${VERSION}.md}"
  REPORT="${REPORT:-/tmp/tensorplate-v${VERSION}-preflight.md}"
}

current_branch() {
  git symbolic-ref --quiet --short HEAD 2>/dev/null ||
    die "detached HEAD is not allowed for release operations"
}

require_clean_worktree() {
  if [[ -n "$(git status --porcelain)" ]]; then
    die "dirty worktree blocks this operation; commit, stash, or discard unrelated changes first"
  fi
}

assert_expected_branch() {
  local branch
  branch="$(current_branch)"
  if [[ "$branch" == "$RELEASE_BRANCH" ]]; then
    return 0
  fi
  if [[ -n "${PREP_BRANCH:-}" && "$branch" == "$PREP_BRANCH" ]]; then
    return 0
  fi
  die "current branch '$branch' is not '$RELEASE_BRANCH'${PREP_BRANCH:+ or prep branch '$PREP_BRANCH'}"
}

command_exists() {
  command -v "$1" >/dev/null 2>&1
}

parse_common_args() {
  VERSION=""
  RELEASE_BRANCH=""
  BASE_REF="origin/develop"
  PREP_BRANCH=""
  ARTIFACTS_DIR=""
  MANIFEST=""
  CHECKSUMS=""
  SIGNOFF=""
  VALIDATION_REPORT=""
  CLEAN_ROOM_REPORT=""
  RELEASE_NOTES=""
  REPORT=""
  SKIP_CI=0
  SKIP_TAG_VERIFY=0
  ALLOW_SNAPSHOT_VERSION=0
  DRY_RUN=0
  EXECUTE=0
  PUSH=0
  CONFIRM=""
  TAG=""
  RC=""
  FINAL=0
  TARGET_OS="Ubuntu 22.04 / JetPack 6.x (L4T 36.x)"
  TARGET_ARCH="arm64"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --version) VERSION="${2:-}"; shift 2 ;;
      --release-branch) RELEASE_BRANCH="${2:-}"; shift 2 ;;
      --base) BASE_REF="${2:-}"; shift 2 ;;
      --prep-branch) PREP_BRANCH="${2:-}"; shift 2 ;;
      --artifacts-dir) ARTIFACTS_DIR="${2:-}"; shift 2 ;;
      --manifest) MANIFEST="${2:-}"; shift 2 ;;
      --checksums) CHECKSUMS="${2:-}"; shift 2 ;;
      --signoff) SIGNOFF="${2:-}"; shift 2 ;;
      --validation-report) VALIDATION_REPORT="${2:-}"; shift 2 ;;
      --clean-room-report) CLEAN_ROOM_REPORT="${2:-}"; shift 2 ;;
      --release-notes) RELEASE_NOTES="${2:-}"; shift 2 ;;
      --report) REPORT="${2:-}"; shift 2 ;;
      --skip-ci) SKIP_CI=1; shift ;;
      --skip-tag-verify) SKIP_TAG_VERIFY=1; shift ;;
      --allow-snapshot-version) ALLOW_SNAPSHOT_VERSION=1; shift ;;
      --dry-run) DRY_RUN=1; shift ;;
      --execute) EXECUTE=1; shift ;;
      --push) PUSH=1; shift ;;
      --confirm) CONFIRM="${2:-}"; shift 2 ;;
      --tag) TAG="${2:-}"; shift 2 ;;
      --rc) RC="${2:-}"; shift 2 ;;
      --final) FINAL=1; shift ;;
      --target-os) TARGET_OS="${2:-}"; shift 2 ;;
      --arch) TARGET_ARCH="${2:-}"; shift 2 ;;
      --help|-h) usage; exit 0 ;;
      *) die "unknown option '$1'" ;;
    esac
  done
  default_paths
}

declare -a PASSES=()
declare -a WARNINGS=()
declare -a FAILURES=()

reset_results() {
  PASSES=()
  WARNINGS=()
  FAILURES=()
}

pass() {
  PASSES+=("$*")
}

warn() {
  WARNINGS+=("$*")
}

fail() {
  FAILURES+=("$*")
}

write_report() {
  mkdir -p "$(dirname "$REPORT")"
  {
    printf '# TensorPlate release preflight report\n\n'
    printf -- '- Version: `%s`\n' "$VERSION"
    printf -- '- Commit: `%s`\n' "$(git rev-parse HEAD)"
    printf -- '- Branch: `%s`\n' "$(current_branch)"
    printf -- '- Generated UTC: `%s`\n\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '## Summary\n\n'
    printf -- '- Passes: %d\n' "${#PASSES[@]}"
    printf -- '- Warnings: %d\n' "${#WARNINGS[@]}"
    printf -- '- Failures: %d\n\n' "${#FAILURES[@]}"
    if ((${#FAILURES[@]})); then
      printf '## Failures\n\n'
      printf -- '- %s\n' "${FAILURES[@]}"
      printf '\n'
    fi
    if ((${#WARNINGS[@]})); then
      printf '## Warnings\n\n'
      printf -- '- %s\n' "${WARNINGS[@]}"
      printf '\n'
    fi
    if ((${#PASSES[@]})); then
      printf '## Passes\n\n'
      printf -- '- %s\n' "${PASSES[@]}"
      printf '\n'
    fi
  } >"$REPORT"
}

check_file_exists() {
  local path="$1"
  local label="$2"
  if [[ -f "$path" ]]; then
    pass "$label exists at $path"
  else
    fail "$label is missing at $path"
  fi
}

check_clean() {
  if [[ -z "$(git status --porcelain)" ]]; then
    pass "worktree is clean"
  else
    fail "worktree is dirty; release operations require a reviewed clean commit"
  fi
}

check_branch_state() {
  local branch
  if branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null)"; then
    if [[ "$branch" == "$RELEASE_BRANCH" ]]; then
      pass "current branch is expected release branch $RELEASE_BRANCH"
    elif [[ -n "$PREP_BRANCH" && "$branch" == "$PREP_BRANCH" ]]; then
      pass "current branch is configured release-prep branch $PREP_BRANCH"
    else
      fail "current branch '$branch' is not '$RELEASE_BRANCH'${PREP_BRANCH:+ or '$PREP_BRANCH'}"
    fi
  else
    fail "detached HEAD is not allowed"
  fi
}

check_remote() {
  if git remote get-url origin >/dev/null 2>&1; then
    pass "origin remote is configured"
  else
    fail "origin remote is missing"
  fi
}

check_tag_state() {
  local final_tag="v${VERSION}"
  if git rev-parse -q --verify "refs/tags/${final_tag}" >/dev/null; then
    fail "final tag $final_tag already exists locally; do not retag published releases"
  else
    pass "final tag $final_tag is not present locally"
  fi

  if git remote get-url origin >/dev/null 2>&1; then
    local rc
    set +e
    git ls-remote --exit-code --tags origin "$final_tag" >/tmp/tensorplate-release-ls-remote.log 2>&1
    rc=$?
    set -e
    if [[ "$rc" -eq 0 ]]; then
      fail "final tag $final_tag already exists on origin; enter post-release verification instead"
    elif [[ "$rc" -eq 2 ]]; then
      pass "final tag $final_tag was not found on origin"
    else
      fail "could not query origin for tag $final_tag; see /tmp/tensorplate-release-ls-remote.log"
    fi
  fi
}

assert_tag_available() {
  local tag="$1"
  if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
    die "tag $tag already exists locally; refusing to rewrite"
  fi
  if git remote get-url origin >/dev/null 2>&1; then
    local rc
    set +e
    git ls-remote --exit-code --tags origin "$tag" >/tmp/tensorplate-release-ls-remote.log 2>&1
    rc=$?
    set -e
    if [[ "$rc" -eq 0 ]]; then
      die "tag $tag already exists on origin; refusing to rewrite"
    elif [[ "$rc" -ne 2 ]]; then
      die "could not query origin for tag $tag; see /tmp/tensorplate-release-ls-remote.log"
    fi
  fi
}

check_version_files() {
  local short
  short="$(version_short)"

  grep -Eq "VERSION[[:space:]]+${VERSION}" CMakeLists.txt &&
    pass "CMake project version is $VERSION" ||
    fail "CMakeLists.txt project() version must be $VERSION"

  grep -Eq 'set\(TP_RUNTIME_VERSION_SUFFIX[[:space:]]+""' CMakeLists.txt &&
    pass "CMake runtime suffix is empty for release" ||
    fail "CMakeLists.txt TP_RUNTIME_VERSION_SUFFIX must be empty for final release"

  grep -Eq "^version = \"${VERSION}\"$" Cargo.toml &&
    pass "Cargo workspace version is $VERSION" ||
    fail "Cargo.toml workspace version must be $VERSION"

  grep -Eq "tensorplate-protocol = \\{ path = \"protocol/rust\", version = \"${VERSION}\" \\}" Cargo.toml &&
    pass "Cargo protocol dependency version is $VERSION" ||
    fail "Cargo.toml tensorplate-protocol dependency version must be $VERSION"

  # Every tensorplate-* crate in Cargo.lock must be exactly $VERSION. A plain
  # grep for one matching line is unsound: prepare_python only rewrites
  # `-dev` crate versions, so on a finalized->finalized bump a partially
  # hand-bumped lockfile (some crates still at the prior version, none with a
  # -dev suffix) would otherwise pass and ship a $VERSION-named .deb whose
  # binary reports the old version. Check each crate, and reject any -dev.
  awk -v v="$VERSION" '
      /^name = "tensorplate-/ {
        getline ver
        if (ver != ("version = \"" v "\"")) { print "  stale: " $0 " -> " ver; bad = 1 }
      }
      END { exit bad }
    ' Cargo.lock &&
    ! grep -Eq 'version = "[0-9]+\.[0-9]+\.[0-9]+-dev"' Cargo.lock &&
    pass "Cargo.lock tensorplate-* crate versions are all finalized at $VERSION" ||
    fail "every Cargo.lock tensorplate-* crate version must be $VERSION without -dev suffixes"

  grep -Eq "\"version-string\": \"${VERSION}\"" vcpkg.json &&
    pass "vcpkg version-string is $VERSION" ||
    fail "vcpkg.json version-string must be $VERSION"

  [[ "$(tr -d '[:space:]' < packaging/VERSION)" == "$VERSION" ]] &&
    pass "packaging/VERSION is $VERSION" ||
    fail "packaging/VERSION must be $VERSION"

  head -n 1 packaging/debian/changelog | grep -Eq "tensorplate \\(${VERSION}-1\\) [^;]+; urgency=medium" &&
    ! head -n 1 packaging/debian/changelog | grep -q 'UNRELEASED' &&
    pass "Debian changelog is finalized for $VERSION-1" ||
    fail "packaging/debian/changelog must start with tensorplate (${VERSION}-1) and a non-UNRELEASED distribution"

  grep -Eq "set\\(TP_PROTOCOL_VERSION_MAJOR[[:space:]]+${short%%.*}" CMakeLists.txt &&
    grep -Eq "set\\(TP_PROTOCOL_VERSION_MINOR[[:space:]]+${short##*.}" CMakeLists.txt &&
    grep -Eq "pub const PROTOCOL_VERSION: &str = \"${short}\";" protocol/rust/src/lib.rs &&
    pass "protocol version surface is $short" ||
    fail "protocol version must remain $short across CMake and Rust"

  grep -Eq "set\\(TP_BUNDLE_FORMAT_VERSION_MAJOR[[:space:]]+${short%%.*}" CMakeLists.txt &&
    grep -Eq "set\\(TP_BUNDLE_FORMAT_VERSION_MINOR[[:space:]]+${short##*.}" CMakeLists.txt &&
    grep -Eq "pub const BUNDLE_FORMAT_VERSION: &str = \"${short}\";" protocol/rust/src/lib.rs &&
    pass "bundle format version surface is $short" ||
    fail "bundle format version must remain $short across CMake and Rust"

  if python3 - "$short" <<'PY'
import json
import pathlib
import sys

short = sys.argv[1]
bad = []
for path in sorted(list(pathlib.Path("config/schemas").glob("*.json")) + list(pathlib.Path("protocol/schemas").glob("*.json"))):
    data = json.loads(path.read_text())
    schema_id = data.get("$id", "")
    if f"/v{short}/" not in schema_id:
        bad.append(f"{path}: $id does not contain /v{short}/")
    found = []

    def walk(obj):
        if isinstance(obj, dict):
            for key, value in obj.items():
                if key == "schema_version" and isinstance(value, dict):
                    found.append(value.get("const"))
                walk(value)
        elif isinstance(obj, list):
            for item in obj:
                walk(item)

    walk(data)
    for observed in found:
        if observed != short:
            bad.append(f"{path}: schema_version const {observed!r} is not {short!r}")

if bad:
    print("\n".join(bad))
    sys.exit(1)
PY
  then
    pass "JSON schema ids and schema_version constants align to $short"
  else
    fail "config/protocol schema version metadata must align to $short"
  fi

  [[ -f include/tensorplate/version.hpp.in ]] &&
    pass "generated C++ version header template exists" ||
    fail "include/tensorplate/version.hpp.in is missing"
}

check_changelog() {
  if grep -Eq "^## \\[${VERSION}\\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$" CHANGELOG.md; then
    pass "CHANGELOG.md contains a dated $VERSION section"
  else
    fail "CHANGELOG.md must promote [Unreleased] to a dated [$VERSION] section before final tagging"
  fi
}

check_release_notes() {
  # The tag-driven release.yml flow passes this path to gh release create
  # --notes-file; a missing file fails late, after the multi-hour build, so
  # gate it here in preflight/cut instead.
  if [[ -f "$RELEASE_NOTES" ]]; then
    pass "release notes present at $RELEASE_NOTES"
  else
    fail "release notes file is missing: $RELEASE_NOTES (required by the publish path)"
  fi
}

check_signoff() {
  check_file_exists "$SIGNOFF" "release sign-off"
  [[ -f "$SIGNOFF" ]] || return 0

  grep -Eiq '^Release decision:[[:space:]]*(pass|conditional-pass)$' "$SIGNOFF" &&
    pass "release decision is pass or conditional-pass" ||
    fail "$SIGNOFF must contain 'Release decision: pass' or 'Release decision: conditional-pass'"

  grep -Eiq '^Validation gate:[[:space:]]*(pass|conditional-pass)$' "$SIGNOFF" &&
    pass "validation gate sign-off is recorded" ||
    fail "$SIGNOFF must contain 'Validation gate: pass' or 'Validation gate: conditional-pass'"

  for field in \
    'Release owner' \
    'Runtime reviewer' \
    'Agent CLI reviewer' \
    'Packaging reviewer' \
    'Validation reviewer' \
    'Security reviewer' \
    'Docs reviewer' \
    'Artifact manifest' \
    'Security review' \
    'Docs review'; do
    grep -Eiq "^${field}:[[:space:]]*(approved|@|[A-Za-z0-9_./:-]+)" "$SIGNOFF" &&
      pass "$field is populated in sign-off" ||
      fail "$SIGNOFF is missing populated field: $field"
  done

  if grep -Eiq '(TODO|TBD|PLACEHOLDER|BLOCKED)' "$SIGNOFF"; then
    fail "$SIGNOFF still contains placeholder or blocked text"
  fi
}

check_evidence() {
  check_file_exists "$VALIDATION_REPORT" "release validation report"
  check_file_exists "$CLEAN_ROOM_REPORT" "clean-room validation procedure or report"
  [[ -f "$VALIDATION_REPORT" ]] && grep -q 'Observed validation evidence' "$VALIDATION_REPORT" &&
    pass "release validation report includes observed evidence" ||
    fail "$VALIDATION_REPORT must include observed validation evidence"
  [[ -f "$CLEAN_ROOM_REPORT" ]] && grep -Eiq 'Release decision:[[:space:]]*(pass|conditional-pass)' "$CLEAN_ROOM_REPORT" &&
    pass "clean-room report records release decision" ||
    warn "$CLEAN_ROOM_REPORT is a procedure or incomplete report; final public publication still requires clean-room decision evidence"
}

check_artifacts() {
  if [[ ! -d "$ARTIFACTS_DIR" ]]; then
    fail "artifact directory $ARTIFACTS_DIR is missing"
    return 0
  fi
  local pkg
  for pkg in "${REQUIRED_PACKAGES[@]}"; do
    if find "$ARTIFACTS_DIR" -maxdepth 1 -type f -name "${pkg}_*.deb" | grep -q .; then
      pass "artifact for $pkg exists"
    else
      fail "missing .deb artifact for $pkg in $ARTIFACTS_DIR"
    fi
  done
  if [[ "${ALLOW_SNAPSHOT_VERSION:-0}" -eq 0 ]]; then
    for pkg in "${SECONDARY_ARCH_PACKAGES[@]}"; do
      if find "$ARTIFACTS_DIR" -maxdepth 1 -type f \
        -name "${pkg}_*_${SECONDARY_ARCH}.deb" | grep -q .; then
        pass "${SECONDARY_ARCH} artifact for $pkg exists"
      else
        fail "missing required ${pkg} ${SECONDARY_ARCH} asset in $ARTIFACTS_DIR"
      fi
    done
  fi

  [[ -f "$MANIFEST" ]] &&
    pass "artifact manifest exists at $MANIFEST" ||
    fail "artifact manifest is missing at $MANIFEST"
  [[ -f "$CHECKSUMS" ]] &&
    pass "checksum file exists at $CHECKSUMS" ||
    fail "checksum file is missing at $CHECKSUMS"

  if [[ -f "$MANIFEST" && -f "$CHECKSUMS" ]]; then
    if verify_manifest_python "$VERSION" "${TAG:-v${VERSION}}" "$ARTIFACTS_DIR" "$MANIFEST" "$CHECKSUMS" >/tmp/tensorplate-release-verify.log 2>&1; then
      pass "artifact manifest and SHA256SUMS verify"
    else
      fail "artifact manifest verification failed; see /tmp/tensorplate-release-verify.log"
    fi
  fi
}

check_ci_status() {
  local commit repo json
  if [[ "$SKIP_CI" -eq 1 ]]; then
    fail "CI check was skipped; publication remains blocked until GitHub checks are verified"
    return 0
  fi
  if ! command_exists gh; then
    fail "gh is not installed; cannot verify required GitHub CI checks"
    return 0
  fi
  commit="$(git rev-parse HEAD)"
  if ! repo="$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null)"; then
    fail "gh cannot resolve the GitHub repository; authenticate or run from a configured checkout"
    return 0
  fi
  if ! json="$(gh api "repos/${repo}/commits/${commit}/check-runs" 2>/dev/null)"; then
    fail "gh could not fetch check-runs for commit $commit"
    return 0
  fi
  if python3 - "$json" <<'PY'
import json
import sys

payload = json.loads(sys.argv[1])
runs = payload.get("check_runs", [])
if not runs:
    print("no check runs found")
    sys.exit(1)
bad = [
    run.get("name", "<unnamed>")
    for run in runs
    if run.get("status") != "completed" or run.get("conclusion") not in ("success", "neutral", "skipped")
]
if bad:
    print("bad check runs: " + ", ".join(bad))
    sys.exit(1)
PY
  then
    pass "GitHub check-runs are completed successfully for $commit"
  else
    fail "required GitHub checks are missing or not green for $commit"
  fi
}

run_preflight() {
  reset_results
  check_clean
  check_branch_state
  check_remote
  check_tag_state
  check_version_files
  check_changelog
  check_release_notes
  check_evidence
  check_signoff
  check_artifacts
  check_ci_status
  write_report

  note "preflight report: $REPORT"
  if ((${#FAILURES[@]})); then
    printf 'preflight failed with %d failure(s)\n' "${#FAILURES[@]}" >&2
    printf 'first failure: %s\n' "${FAILURES[0]}" >&2
    return 1
  fi
  printf 'preflight passed with %d checks\n' "${#PASSES[@]}"
}

prepare_python() {
  python3 - "$VERSION" <<'PY'
import datetime
import json
import re
import sys
from pathlib import Path

version = sys.argv[1]
today = datetime.date.today().isoformat()

def replace(path, pattern, repl, flags=0):
    p = Path(path)
    text = p.read_text()
    new, count = re.subn(pattern, repl, text, count=1, flags=flags)
    if count == 0 and repl not in text:
        raise SystemExit(f"{path}: expected pattern was not found")
    p.write_text(new)

replace(
    "CMakeLists.txt",
    r'set\(TP_RUNTIME_VERSION_SUFFIX[ \t]+"[^"]*"',
    'set(TP_RUNTIME_VERSION_SUFFIX       ""',
)

cargo = Path("Cargo.toml")
text = cargo.read_text()
text = re.sub(r'^version = "[^"]+"$', f'version = "{version}"', text, count=1, flags=re.MULTILINE)
text = re.sub(
    r'tensorplate-protocol = \{ path = "protocol/rust", version = "[^"]+" \}',
    f'tensorplate-protocol = {{ path = "protocol/rust", version = "{version}" }}',
    text,
    count=1,
)
cargo.write_text(text)

lock = Path("Cargo.lock")
if lock.exists():
    text = lock.read_text()
    text = re.sub(r'version = "[0-9]+\.[0-9]+\.[0-9]+-dev"', f'version = "{version}"', text)
    lock.write_text(text)

vcpkg = Path("vcpkg.json")
data = json.loads(vcpkg.read_text())
data["version-string"] = version
vcpkg.write_text(json.dumps(data, indent=2) + "\n")

Path("packaging/VERSION").write_text(version + "\n")

debian = Path("packaging/debian/changelog")
lines = debian.read_text().splitlines()
if not lines:
    raise SystemExit("packaging/debian/changelog is empty")
lines[0] = f"tensorplate ({version}-1) unstable; urgency=medium"
debian.write_text("\n".join(lines) + "\n")

changelog = Path("CHANGELOG.md")
text = changelog.read_text()
release_heading = f"## [{version}] - "
if release_heading not in text:
    marker = "## [Unreleased]\n"
    if marker not in text:
        raise SystemExit("CHANGELOG.md is missing ## [Unreleased]")
    text = text.replace(marker, f"{marker}\n## [{version}] - {today}\n", 1)
changelog.write_text(text)
PY
}

ensure_prepare_diff_scope() {
  local changed unexpected
  changed="$(git diff --name-only)"
  while IFS= read -r file; do
    [[ -n "$file" ]] || continue
    unexpected=1
    local approved
    for approved in "${APPROVED_PREPARE_FILES[@]}"; do
      if [[ "$file" == "$approved" ]]; then
        unexpected=0
        break
      fi
    done
    [[ "$unexpected" -eq 0 ]] || die "prepare touched unexpected file: $file"
  done <<<"$changed"
}

cmd_prepare() {
  parse_common_args "$@"
  if [[ "$DRY_RUN" -eq 0 && "$EXECUTE" -eq 0 ]]; then
    DRY_RUN=1
  fi
  if [[ "$DRY_RUN" -eq 1 ]]; then
    note "dry-run: prepare release metadata for v$VERSION"
    printf 'Would update only these files:\n'
    printf '  %s\n' "${APPROVED_PREPARE_FILES[@]}"
    printf '\nRequired final values:\n'
    printf '  runtime/package version: %s\n' "$VERSION"
    printf '  protocol/bundle format version: %s\n' "$(version_short)"
    printf '  release branch: %s\n' "$RELEASE_BRANCH"
    printf '  changelog heading: ## [%s] - YYYY-MM-DD\n' "$VERSION"
    return 0
  fi

  [[ "$CONFIRM" == "PREPARE-v${VERSION}" ]] ||
    die "prepare --execute requires --confirm PREPARE-v${VERSION}"
  require_clean_worktree
  assert_expected_branch
  prepare_python
  ensure_prepare_diff_scope
  note "release metadata prepared; review git diff before committing"
}

run_source_preflight() {
  reset_results
  check_clean
  check_branch_state
  check_remote
  check_version_files
  check_changelog
  check_release_notes

  if ((${#FAILURES[@]})); then
    printf 'source preflight failed with %d failure(s)\n' "${#FAILURES[@]}" >&2
    printf 'first failure: %s\n' "${FAILURES[0]}" >&2
    return 1
  fi
  printf 'source preflight passed with %d checks\n' "${#PASSES[@]}"
}

cmd_cut() {
  parse_common_args "$@"
  if [[ "$DRY_RUN" -eq 0 && "$EXECUTE" -eq 0 ]]; then
    DRY_RUN=1
  fi

  if [[ "$FINAL" -eq 1 ]]; then
    TAG="v${VERSION}"
  elif [[ -n "$RC" ]]; then
    [[ "$RC" =~ ^[1-9][0-9]*$ ]] || die "--rc must be a positive integer"
    TAG="v${VERSION}-rc.${RC}"
  else
    die "cut requires --rc N or --final"
  fi

  if [[ "$DRY_RUN" -eq 1 ]]; then
    note "dry-run: cut release source tag $TAG"
    printf 'Would create or switch release branch: %s\n' "$RELEASE_BRANCH"
    printf 'Would branch from base ref: %s\n' "$BASE_REF"
    printf 'Would prepare release metadata for: %s\n' "$VERSION"
    printf 'Would commit: Prepare %s release\n' "$TAG"
    printf 'Would create annotated tag: %s\n' "$TAG"
    if [[ "$PUSH" -eq 1 ]]; then
      printf 'Would push branch and tag to origin, triggering release CI.\n'
    else
      printf 'Would leave branch and tag local for review.\n'
    fi
    return 0
  fi

  [[ "$CONFIRM" == "CUT-${TAG}" ]] ||
    die "cut --execute requires --confirm CUT-${TAG}"
  require_clean_worktree

  if git remote get-url origin >/dev/null 2>&1; then
    git fetch origin --prune --tags
  fi

  if git show-ref --verify --quiet "refs/heads/${RELEASE_BRANCH}"; then
    git switch "$RELEASE_BRANCH"
  else
    git switch --create "$RELEASE_BRANCH" "$BASE_REF"
  fi

  require_clean_worktree
  assert_tag_available "$TAG"
  prepare_python
  ensure_prepare_diff_scope
  git add -- "${APPROVED_PREPARE_FILES[@]}"
  if git diff --cached --quiet; then
    note "release metadata already matches $VERSION; no prepare commit needed"
  else
    git commit -m "Prepare ${TAG} release"
  fi
  run_source_preflight

  local tag_mode=(-a)
  if [[ "${TP_RELEASE_SIGN_TAG:-0}" == "1" ]]; then
    tag_mode=(-s)
  fi
  git tag "${tag_mode[@]}" "$TAG" -m "TensorPlate ${TAG}"
  note "created annotated tag $TAG locally"

  if [[ "$PUSH" -eq 1 ]]; then
    git push origin "$RELEASE_BRANCH"
    git push origin "$TAG"
    note "pushed $RELEASE_BRANCH and $TAG; release CI owns artifact publication"
  else
    note "review tag metadata with: git show $TAG"
    note "push to trigger release CI: git push origin $RELEASE_BRANCH && git push origin $TAG"
  fi
}

manifest_python() {
  local secondary_packages
  secondary_packages="$(printf '%s,' "${SECONDARY_ARCH_PACKAGES[@]}")"
  python3 - "$VERSION" "$TAG" "$ARTIFACTS_DIR" "$MANIFEST" "$CHECKSUMS" "$TARGET_OS" "$TARGET_ARCH" "$(git rev-parse HEAD)" "$RELEASE_BRANCH" "$VALIDATION_REPORT" "$CLEAN_ROOM_REPORT" "$SECONDARY_ARCH" "${secondary_packages%,}" "$SECONDARY_TARGET_OS" <<'PY'
import datetime
import hashlib
import json
import re
import sys
from pathlib import Path

(
    version, tag, artifacts_dir, manifest_path, checksums_path, target_os,
    target_arch, commit, branch, validation_report, clean_room_report,
    secondary_arch, secondary_packages_raw, secondary_target_os,
) = sys.argv[1:]
secondary_packages = set(secondary_packages_raw.split(",")) if secondary_packages_raw else set()
root = Path(artifacts_dir)
required = [
    "tensorplate-common",
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
    "tensorplate-backend-python-pytorch",
    "tensorplate-apt-source",
    "tensorplate",
]
if not root.is_dir():
    raise SystemExit(f"artifact directory does not exist: {root}")

def sha256(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()

artifacts = []
missing = []
seen = set()
secondary_matches = []
for package in required:
    matches = sorted(root.glob(f"{package}_*.deb"))
    if not matches:
        missing.append(package)
        continue
    target_matches = []
    for path in matches:
        match = re.match(r"(?P<package>.+)_(?P<version>[^_]+)_(?P<arch>[^_]+)\.deb$", path.name)
        if not match:
            raise SystemExit(f"artifact name is not Debian-like: {path.name}")
        package_version = match.group("version")
        arch = match.group("arch")
        # The glob that found this file is a PREFIX match, so the name parsed
        # from the filename is not necessarily the required-list entry that
        # matched it: `tensorplate-agent_stale_X-1_amd64.deb` is globbed under
        # `tensorplate-agent`. Every decision below must use the parsed name,
        # or a stray file rides into the manifest under a sibling's identity.
        parsed_package = match.group("package")
        if not (package_version == version or package_version.startswith(version + "-")):
            raise SystemExit(f"{path.name}: package version {package_version} does not match release {version}")
        if parsed_package != package and parsed_package not in required:
            raise SystemExit(
                f"{path.name}: file name does not match a published package "
                f"(parsed {parsed_package!r} while globbing {package!r})"
            )
        if arch not in (target_arch, "all"):
            # A foreign architecture is admissible only for a package that
            # the secondary runtime set declares. Anything else reaching the
            # artifacts directory is a staging mistake, and signing it into
            # the manifest would publish a package nobody built on purpose.
            if arch != secondary_arch or parsed_package not in secondary_packages:
                raise SystemExit(
                    f"{path.name}: {parsed_package} is not published for architecture {arch}"
                )
        key = (parsed_package, arch)
        if key in seen:
            raise SystemExit(f"duplicate artifact for package/architecture: {key[0]} {key[1]}")
        seen.add(key)
        if arch in (target_arch, "all"):
            target_matches.append(path.name)
        # Independent of the branch above, not an else: when the primary
        # target IS the secondary architecture, one artifact satisfies both
        # and an `else` would report the whole secondary set as absent.
        if arch == secondary_arch:
            secondary_matches.append(parsed_package)
        digest = sha256(path)
        artifacts.append(
            {
                "file": path.name,
                "package": parsed_package,
                "version": package_version,
                "architecture": arch,
                "target_os": target_os if arch in (target_arch, "all") else secondary_target_os,
                "size_bytes": path.stat().st_size,
                "sha256": digest,
            }
        )
    if not target_matches:
        missing.append(f"{package} ({target_arch} or all)")

if missing:
    raise SystemExit("missing package artifacts: " + ", ".join(missing))

installer = root / "install.sh"
if not installer.is_file():
    raise SystemExit("missing installer asset: install.sh")
artifacts.append(
    {
        "file": installer.name,
        "kind": "installer",
        "version": version,
        "target_os": target_os,
        "size_bytes": installer.stat().st_size,
        "sha256": sha256(installer),
    }
)

# tensorplate-python SDK wheel + sdist (client-only, pure Python). Included
# in the signed manifest and SHA256SUMS when staged into the artifacts dir;
# absent from runtime-only snapshot builds.
for sdk_kind, sdk_pattern in (
    ("python-wheel", f"tensorplate_python-{version}-py3-none-any.whl"),
    ("python-sdist", f"tensorplate_python-{version}.tar.gz"),
):
    sdk_matches = sorted(root.glob(sdk_pattern))
    if not sdk_matches:
        continue
    if len(sdk_matches) != 1:
        raise SystemExit(f"expected exactly one {sdk_kind} matching {sdk_pattern} in {root}; found {len(sdk_matches)}")
    sdk_path = sdk_matches[0]
    artifacts.append(
        {
            "file": sdk_path.name,
            "kind": sdk_kind,
            "version": version,
            "target_os": "Python 3.10+ (any platform)",
            "size_bytes": sdk_path.stat().st_size,
            "sha256": sha256(sdk_path),
        }
    )

snapshot = "~dev." in version or tag.startswith("snapshot-")
# Releases ship a complete second runtime set for Ubuntu x86_64 alongside the
# primary target; snapshot builds are single-architecture local-source flows
# and may omit it. Assert the whole set, not a representative member — a
# partial set would otherwise generate, verify, and publish clean.
if not snapshot:
    absent = sorted(secondary_packages - set(secondary_matches))
    if absent:
        raise SystemExit(
            f"release artifact set is missing required {secondary_arch} packages: "
            + ", ".join(absent)
        )
provenance = "local-source-snapshot" if snapshot else "github-release"
release = {
    "project": "tensorplate",
    "version": version,
    "tag": tag,
    "commit": commit,
    "branch": branch,
    "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "provenance": provenance,
    "unreleased": snapshot,
}
if snapshot:
    release["source_kind"] = "local-source-branch"
    release["labels"] = ["unreleased", "local-source-snapshot"]

manifest = {
    "schema": "https://tensorplate.com/schemas/release-artifact-manifest-v1.json",
    "release": release,
    "target": {
        "hardware_floor": "Jetson Orin Nano 8GB Super",
        "os": target_os,
        "architecture": target_arch,
    },
    "validation": {
        "gate_report": validation_report,
        "clean_room_report": clean_room_report,
    },
    "artifacts": artifacts,
}

manifest_out = Path(manifest_path)
checksums_out = Path(checksums_path)
manifest_out.parent.mkdir(parents=True, exist_ok=True)
checksums_out.parent.mkdir(parents=True, exist_ok=True)
manifest_out.write_text(json.dumps(manifest, indent=2) + "\n")
manifest_digest = sha256(manifest_out)
checksum_lines = [f"{manifest_digest}  {manifest_out.name}\n"]
checksum_lines.extend(f"{a['sha256']}  {a['file']}\n" for a in artifacts)
checksums_out.write_text("".join(checksum_lines))
print(f"wrote {manifest_out}")
print(f"wrote {checksums_out}")
PY
}

verify_manifest_python() {
  local secondary_packages
  secondary_packages="$(printf '%s,' "${SECONDARY_ARCH_PACKAGES[@]}")"
  python3 - "$1" "$2" "$3" "$4" "$5" "$SECONDARY_ARCH" "${secondary_packages%,}" <<'PY'
import hashlib
import json
import sys
from pathlib import Path

(
    version, tag, artifacts_dir, manifest_path, checksums_path,
    secondary_arch, secondary_packages_raw,
) = sys.argv[1:]
secondary_packages = set(secondary_packages_raw.split(",")) if secondary_packages_raw else set()
root = Path(artifacts_dir)
manifest = json.loads(Path(manifest_path).read_text())
if manifest.get("release", {}).get("version") != version:
    raise SystemExit("manifest version mismatch")
if manifest.get("release", {}).get("tag") != tag:
    raise SystemExit("manifest tag mismatch")

checksums = {}
for line in Path(checksums_path).read_text().splitlines():
    if not line.strip():
        continue
    digest, filename = line.split(None, 1)
    checksums[filename.strip()] = digest

for artifact in manifest.get("artifacts", []):
    name = artifact["file"]
    path = root / name
    if not path.is_file():
        raise SystemExit(f"missing artifact listed in manifest: {name}")
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    digest = h.hexdigest()
    if digest != artifact.get("sha256"):
        raise SystemExit(f"manifest checksum mismatch for {name}")
    if checksums.get(name) != digest:
        raise SystemExit(f"SHA256SUMS mismatch for {name}")

manifest_digest = hashlib.sha256(Path(manifest_path).read_bytes()).hexdigest()
if checksums.get(Path(manifest_path).name) != manifest_digest:
    raise SystemExit(f"SHA256SUMS mismatch for {Path(manifest_path).name}")

required = {
    "tensorplate-common",
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
    "tensorplate-backend-python-pytorch",
    "tensorplate-apt-source",
    "tensorplate",
}
present = {artifact.get("package") for artifact in manifest.get("artifacts", [])}
missing = sorted(required - present)
if missing:
    raise SystemExit("manifest is missing packages: " + ", ".join(missing))
snapshot = "~dev." in version or tag.startswith("snapshot-")
if not snapshot:
    # The package-name check above is architecture-blind: a manifest holding
    # an arm64 agent and an amd64 serving worker satisfies it. Assert the
    # secondary runtime set by (package, architecture) so a half-published
    # architecture cannot verify clean.
    published = {
        (artifact.get("package"), artifact.get("architecture"))
        for artifact in manifest.get("artifacts", [])
    }
    absent = sorted(
        pkg for pkg in secondary_packages if (pkg, secondary_arch) not in published
    )
    if absent:
        raise SystemExit(
            f"manifest is missing required {secondary_arch} packages: " + ", ".join(absent)
        )
if not any(artifact.get("file") == "install.sh" for artifact in manifest.get("artifacts", [])):
    raise SystemExit("manifest is missing install.sh")
if not snapshot:
    for sdk_kind in ("python-wheel", "python-sdist"):
        if not any(artifact.get("kind") == sdk_kind for artifact in manifest.get("artifacts", [])):
            raise SystemExit(f"manifest is missing the required tensorplate-python {sdk_kind}")
print("manifest verified")
PY
}

cmd_manifest() {
  parse_common_args "$@"
  TAG="${TAG:-v${VERSION}}"
  manifest_python
}

cmd_verify() {
  parse_common_args "$@"
  [[ -n "$TAG" ]] || die "verify requires --tag"
  if [[ "$SKIP_TAG_VERIFY" -eq 1 ]]; then
    note "skipping annotated tag check for build-only artifact validation"
  else
    if [[ "$(git cat-file -t "$TAG" 2>/dev/null || true)" == "tag" ]]; then
      note "annotated tag $TAG exists"
    else
      die "$TAG is missing or is not an annotated tag"
    fi
  fi
  verify_manifest_python "$VERSION" "$TAG" "$ARTIFACTS_DIR" "$MANIFEST" "$CHECKSUMS"
}

cmd_preflight() {
  parse_common_args "$@"
  TAG="${TAG:-v${VERSION}}"
  run_preflight
}

cmd_tag() {
  parse_common_args "$@"
  PREP_BRANCH=""
  assert_expected_branch
  require_clean_worktree

  if [[ "$FINAL" -eq 1 ]]; then
    TAG="v${VERSION}"
  elif [[ -n "$RC" ]]; then
    [[ "$RC" =~ ^[1-9][0-9]*$ ]] || die "--rc must be a positive integer"
    TAG="v${VERSION}-rc.${RC}"
  else
    die "tag requires --rc N or --final"
  fi

  [[ "$CONFIRM" == "CREATE-${TAG}" ]] ||
    die "tag creation requires --confirm CREATE-${TAG}"

  if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
    die "tag $TAG already exists locally; refusing to rewrite"
  fi
  if git remote get-url origin >/dev/null 2>&1; then
    local rc
    set +e
    git ls-remote --exit-code --tags origin "$TAG" >/tmp/tensorplate-release-ls-remote.log 2>&1
    rc=$?
    set -e
    if [[ "$rc" -eq 0 ]]; then
      die "tag $TAG already exists on origin; refusing to rewrite"
    elif [[ "$rc" -ne 2 ]]; then
      die "could not query origin for tag $TAG; see /tmp/tensorplate-release-ls-remote.log"
    fi
  fi

  run_preflight

  local tag_mode=(-a)
  if [[ "${TP_RELEASE_SIGN_TAG:-0}" == "1" ]]; then
    tag_mode=(-s)
  fi
  git tag "${tag_mode[@]}" "$TAG" -m "TensorPlate ${TAG}"
  note "created annotated tag $TAG locally"
  note "review tag metadata with: git show $TAG"
  note "push only after review: git push origin $TAG"
}

cmd_publish() {
  parse_common_args "$@"
  [[ -n "$TAG" ]] || die "publish requires --tag"
  if [[ "$DRY_RUN" -eq 0 && "$EXECUTE" -eq 0 ]]; then
    DRY_RUN=1
  fi
  if [[ "$(git cat-file -t "$TAG" 2>/dev/null || true)" != "tag" ]]; then
    die "$TAG is missing locally or is not an annotated tag"
  fi
  verify_manifest_python "$VERSION" "$TAG" "$ARTIFACTS_DIR" "$MANIFEST" "$CHECKSUMS"
  [[ -f "$RELEASE_NOTES" ]] || die "release notes file is missing: $RELEASE_NOTES"

  local assets=()
  local pkg
  shopt -s nullglob
  local deb_assets=("$ARTIFACTS_DIR"/*.deb)
  ((${#deb_assets[@]} >= ${#REQUIRED_PACKAGES[@]})) ||
    die "expected at least ${#REQUIRED_PACKAGES[@]} Debian package artifacts in $ARTIFACTS_DIR"
  for pkg in "${REQUIRED_PACKAGES[@]}"; do
    local match
    match="$(find "$ARTIFACTS_DIR" -maxdepth 1 -type f -name "${pkg}_*.deb" | head -n 1)"
    [[ -n "$match" ]] || die "missing artifact for $pkg"
  done
  # Package-name checks above accept any architecture; the secondary runtime
  # set is additionally required per package on the publish path.
  for pkg in "${SECONDARY_ARCH_PACKAGES[@]}"; do
    find "$ARTIFACTS_DIR" -maxdepth 1 -type f -name "${pkg}_*_${SECONDARY_ARCH}.deb" | grep -q . ||
      die "missing required ${pkg} ${SECONDARY_ARCH} asset in $ARTIFACTS_DIR"
  done
  assets+=("${deb_assets[@]}")
  local sdk_assets=("$ARTIFACTS_DIR"/tensorplate_python-*.whl "$ARTIFACTS_DIR"/tensorplate_python-*.tar.gz)
  ((${#sdk_assets[@]} == 2)) ||
    die "expected tensorplate-python wheel and sdist release assets in $ARTIFACTS_DIR"
  assets+=("${sdk_assets[@]}")
  [[ -f "$ARTIFACTS_DIR/install.sh" ]] || die "missing installer asset: $ARTIFACTS_DIR/install.sh"
  local checksums_bundle="${CHECKSUMS}.cosign.bundle"
  [[ -f "$checksums_bundle" ]] ||
    die "missing signature bundle ${checksums_bundle}; the release workflow signs SHA256SUMS with cosign. Publish via .github/workflows/release.yml, or place the signed bundle next to SHA256SUMS before publishing"
  assets+=("$ARTIFACTS_DIR/install.sh" "$MANIFEST" "$CHECKSUMS" "$checksums_bundle")

  local gh_args=(release create "$TAG" "${assets[@]}" --verify-tag --draft --notes-file "$RELEASE_NOTES")
  if [[ "$TAG" == *-rc.* ]]; then
    gh_args+=(--prerelease)
  else
    gh_args+=(--latest)
  fi

  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf 'dry-run gh command:\n  gh'
    printf ' %q' "${gh_args[@]}"
    printf '\n'
    return 0
  fi

  [[ "$CONFIRM" == "PUBLISH-${TAG}" ]] ||
    die "publish --execute requires --confirm PUBLISH-${TAG}"
  require_clean_worktree
  command_exists gh || die "gh is required to create a GitHub Release"
  gh "${gh_args[@]}"
}

main() {
  if [[ $# -lt 1 ]]; then
    usage
    exit 2
  fi
  local command="$1"
  shift
  require_repo
  case "$command" in
    preflight) cmd_preflight "$@" ;;
    prepare) cmd_prepare "$@" ;;
    cut) cmd_cut "$@" ;;
    manifest) cmd_manifest "$@" ;;
    verify) cmd_verify "$@" ;;
    tag) cmd_tag "$@" ;;
    publish) cmd_publish "$@" ;;
    --help|-h|help) usage ;;
    *) die "unknown subcommand '$command'" ;;
  esac
}

main "$@"
