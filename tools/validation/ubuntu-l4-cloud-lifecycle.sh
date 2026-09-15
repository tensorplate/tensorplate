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
#   crash-loop    an agent that cannot load its config is retried and then
#                 given up on by systemd rather than restarted forever, and
#                 recovers its deployment once the config is restored
#
# With --baseline-assets-dir, two more stages run after crash-loop. The
# baseline is a published, signed predecessor set, always installed with
# its signature verified:
#   upgrade       over a fresh baseline install serving a deployment, with
#                 an operator edit to /etc/tensorplate/cli.json, the
#                 candidate's installer upgrades every package in place:
#                 the services come back on new pids through the installer
#                 alone, the edit survives, doctor is green, and the
#                 deployment re-warms from the baseline's durable state
#   rollback      the documented procedure -- stop, set state aside as
#                 state.bak, remove (not purge) every TensorPlate package,
#                 install the baseline fresh -- returns exactly the
#                 baseline set, keeps the operator edit and the set-aside
#                 state, starts with no active deployment, and deploys and
#                 serves again
# The five stages above are always about a clean candidate install, and a
# run with a baseline leaves the baseline installed when it finishes.
#
# WHAT IT DOES NOT PROVE
#   The deploy-smoke bundle selects the device-neutral `fixture` backend
#   profile. It exercises admission, worker supervision and the sidecar
#   inference path against the real installed appliance -- it does NOT
#   execute a CUDA kernel, and a passing run is not evidence that the
#   accelerator computed anything. There is no CUDA fixture backend to
#   select yet. Do not describe a run of this harness as GPU validation.
#
# Skipped stages have their reason recorded in the report rather than
# being omitted. Offline is deferred until cloud platform detection can
# resolve this row without querying GCE metadata. No network policy is
# changed and no offline behavior is certified by this harness. Without
# --baseline-assets-dir, upgrade and rollback are skipped as well: there
# is no predecessor set to move between.
#
# Usage:
#   tools/validation/ubuntu-l4-cloud-lifecycle.sh \
#     --assets-dir <candidate artifacts> \
#     [--baseline-assets-dir <published predecessor artifacts>] \
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
# Outside every path the agent's unit hides from it. The unit sets
# PrivateTmp=true, so /tmp and /var/tmp are a private namespace the agent
# cannot see into, and ProtectHome=true, so nothing under /home is
# visible either. ProtectSystem=strict leaves the rest of the filesystem
# readable, which is all a bundle needs. The first real run staged under
# /var/tmp and the agent reported the bundle as nonexistent.
BUNDLE_STAGING_DIR="${TP_CLOUD_BUNDLE_STAGING:-/opt/tensorplate-validation/x86-fixture-smoke}"
# RestartSec is 5 in the shipped unit, so every readiness wait has to sit
# well above it rather than racing a restart.
readonly READY_TIMEOUT_SECONDS=60
readonly AGENT_CONFIG="/etc/tensorplate/agent.json"
# Longer than RestartSec, so a unit that is still looping has restarted
# at least once between two samples. Overridable only so the settling
# logic can be driven without waiting in CI.
CRASH_LOOP_POLL_SECONDS="${TP_CLOUD_CRASH_LOOP_POLL_SECONDS:-7}"
readonly CRASH_LOOP_POLLS=40
# Registered after the backup succeeds and before the config is changed.
# Kept until restoration and service recovery succeed, including on EXIT.
CRASH_LOOP_BACKUP=""
# The documented rollback sets durable state aside under this name
# (docs/install/lifecycle.md) rather than carrying it back.
readonly STATE_DIR="/var/lib/tensorplate/state"
readonly STATE_ASIDE_DIR="/var/lib/tensorplate/state.bak"
# The conffile an operator edits before the upgrade, and whose bytes both
# directions must keep. Nothing reads it by default, so the edit cannot
# change the behavior under test. Read as the operator, which the
# runbook's tensorplate group membership already allows; overridable
# only so a stubbed appliance can supply the file.
OPERATOR_CONFIG="${TP_CLOUD_OPERATOR_CONFIG:-/etc/tensorplate/cli.json}"
OPERATOR_CONFIG_SHA256=""

ROW="$DEFAULT_ROW"
ASSETS_DIR=""
BASELINE_REQUESTED=0
BASELINE_DIR=""
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
  --baseline-assets-dir DIR
                         A published, signed predecessor release set, laid
                         out like --assets-dir with every file SHA256SUMS
                         lists. Runs the upgrade and rollback stages; without
                         it they are skipped. Every runtime package must be
                         older than the candidate's. Always installed with
                         its signature verified, even with --allow-unsigned.
                         Preflight needs public GitHub API and release-asset
                         access, even when the signature bundle is local.
                         The run then ends with the baseline installed.
  --tested-version X.Y.Z The release this run authorizes. Bare version, never
                         a candidate spelling. Required.
  --evidence-dir DIR     Where the report and stage logs are written. Must be
                         new or empty. Required.
  --row ROW_ID           Support row under test. Default: ${DEFAULT_ROW}
  --bundle-dir DIR       deploy-smoke bundle. Default: the repository's
                         test/models/bundles/v0_1/x86_fixture_smoke
  --deployment-id ID     Deployment id for the smoke. Default: ${DEPLOYMENT_ID}
  --allow-unsigned       Pass --allow-unsigned to the installer, for a
                         candidate build with no published signature. Never
                         applied to the baseline.
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
      --baseline-assets-dir) BASELINE_REQUESTED=1; BASELINE_DIR="${2:-}"; shift 2 ;;
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
  # Gated on the option being given, not on its value: an empty value
  # must be refused, not read as a run without a baseline.
  if ((BASELINE_REQUESTED)); then
    preflight_upgrade_path
  fi

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

# The two sets upgrade and rollback move between, as one JSON line:
# {"from": {"release_tag", "packages"}, "to": {...}}, where packages maps
# each runtime package install.sh installs to its Debian version.
UPGRADE_PATH=""
# Publication and signature verification establish different things. The
# public release's checksum digest is captured during preflight; install.sh
# later verifies that the release workflow signed those checksums.
BASELINE_PUBLISHED_DIGEST=""
readonly BASELINE_ALLOW_UNSIGNED=0

read_upgrade_path() {
  python3 - "$BASELINE_DIR" "$ASSETS_DIR" <<'PY'
import json, pathlib, re, subprocess, sys

# The set install.sh installs with --with-python-backend, which is also
# the set rollback has to remove: the backend only Recommends the agent,
# so leaving it out would leave it for the older installer to downgrade.
RUNTIME = (
    "tensorplate-common",
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
    "tensorplate-backend-python-pytorch",
)
# The shape of a Debian version: an optional epoch, then an upstream
# version starting with a digit. dpkg --compare-versions cannot be the
# check: it treats an empty version as older than any other, and only
# warns about some malformed ones before comparing them anyway.
DEBIAN_VERSION = re.compile(r"(?:[0-9]+:)?[0-9][A-Za-z0-9.+~-]*")

def read_set(label, directory):
    manifests = sorted(pathlib.Path(directory).glob("tensorplate-*-artifacts.json"))
    if len(manifests) != 1:
        raise SystemExit(
            f"the {label} set needs exactly one tensorplate-*-artifacts.json; "
            f"found {len(manifests)}"
        )
    manifest = json.loads(manifests[0].read_text(encoding="utf-8"))
    packages = {}
    for package in RUNTIME:
        matches = [
            artifact for artifact in manifest.get("artifacts", [])
            if isinstance(artifact, dict)
            and artifact.get("package") == package
            and str(artifact.get("file", "")).endswith(".deb")
            and artifact.get("architecture") in ("amd64", "all")
        ]
        if len(matches) != 1:
            raise SystemExit(
                f"the {label} set must list exactly one {package} package for amd64 "
                f"or all; found {len(matches)}"
            )
        # The version parsed from the package file name, which is the
        # Debian version dpkg records; release.version is the canonical
        # spelling and is the same for every candidate of a release.
        version = matches[0].get("version")
        if not isinstance(version, str) or not DEBIAN_VERSION.fullmatch(version):
            raise SystemExit(
                f"the {label} set lists {package} at version {version!r}, "
                "which is not a Debian version"
            )
        packages[package] = version
    return manifest.get("release") or {}, packages

baseline_release, baseline = read_set("baseline", sys.argv[1])
candidate_release, candidate = read_set("candidate", sys.argv[2])

# Refuse a snapshot before the separate public-release and signature checks.
# Non-snapshot manifest labels alone do not establish publication.
if baseline_release.get("unreleased") is not False \
        or baseline_release.get("provenance") != "github-release":
    raise SystemExit(
        "the baseline set is not a published release: its manifest records "
        f"unreleased={baseline_release.get('unreleased')!r} "
        f"provenance={baseline_release.get('provenance')!r}"
    )

# Strictly older, package by package. This also refuses the same set
# passed twice. dpkg exits 2 on a version it rejects outright, which is
# refused the same way.
for package in RUNTIME:
    older = subprocess.run(
        ["dpkg", "--compare-versions", baseline[package], "lt", candidate[package]],
        stdout=subprocess.DEVNULL,
    )
    if older.returncode != 0:
        raise SystemExit(
            f"{package}: the baseline's {baseline[package]} is not older than "
            f"the candidate's {candidate[package]}"
        )

print(json.dumps({
    "from": {"release_tag": baseline_release.get("tag"), "packages": baseline},
    "to": {"release_tag": candidate_release.get("tag"), "packages": candidate},
}, sort_keys=True))
PY
}

preflight_upgrade_path() {
  [[ -n "$BASELINE_DIR" && -d "$BASELINE_DIR" ]] ||
    die "--baseline-assets-dir must name a directory"
  [[ -f "${BASELINE_DIR}/install.sh" ]] || die "missing ${BASELINE_DIR}/install.sh"
  [[ -f "${BASELINE_DIR}/SHA256SUMS" ]] || die "missing ${BASELINE_DIR}/SHA256SUMS"
  UPGRADE_PATH="$(read_upgrade_path)" ||
    die "the baseline and candidate sets do not form an upgrade path"
  local baseline_tag publication_helper
  baseline_tag="$(python3 -c 'import json, sys; tag = json.loads(sys.argv[1])["from"]["release_tag"]; assert isinstance(tag, str), "baseline release tag is missing"; print(tag)' "$UPGRADE_PATH")" ||
    die "the baseline manifest must name its release tag"
  publication_helper="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-baseline-publication.py" ||
    die "cannot locate the baseline publication checker"
  BASELINE_PUBLISHED_DIGEST="$(python3 "$publication_helper" \
    --assets-dir "$BASELINE_DIR" --release-tag "$baseline_tag")" ||
    die "the baseline is not a matching publicly available release"
  [[ "$BASELINE_PUBLISHED_DIGEST" =~ ^[0-9a-f]{64}$ ]] ||
    die "the baseline publication checker did not return a checksum digest"
  pass "upgrade path: every runtime package in the baseline set is older than the candidate's"
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

# The baseline set is verified the same way, before anything is
# installed. Its digest is not the run's artifact digest -- the report is
# about the candidate -- so it is filed on its own, in baseline-digest.txt
# in the same `<hex>  SHA256SUMS` shape, and held for install_set and the
# upgrade path.
BASELINE_DIGEST=""

record_baseline_assets() {
  note "verifying the baseline artifact set"
  ( cd "$BASELINE_DIR" && sha256sum -c SHA256SUMS ) >"${EVIDENCE_DIR}/baseline-checksums.txt" 2>&1 ||
    { cat "${EVIDENCE_DIR}/baseline-checksums.txt" >&2; die "the baseline artifact set failed verification"; }
  BASELINE_DIGEST="$( cd "$BASELINE_DIR" && sha256sum SHA256SUMS | awk '{print $1}' )"
  [[ "$BASELINE_DIGEST" =~ ^[0-9a-f]{64}$ ]] ||
    die "could not compute a digest for ${BASELINE_DIR}/SHA256SUMS"
  [[ "$BASELINE_DIGEST" == "$BASELINE_PUBLISHED_DIGEST" ]] ||
    die "baseline SHA256SUMS changed after its public release was verified"
  printf '%s  SHA256SUMS\n' "$BASELINE_DIGEST" >"${EVIDENCE_DIR}/baseline-digest.txt" ||
    die "could not record the baseline digest"
  pass "baseline artifact set verified: ${BASELINE_DIGEST}"
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

# Every TensorPlate package dpkg knows about, one `name status version`
# line each. dpkg-query exits non-zero when nothing matches, which is an
# empty database here rather than a failure; callers decide what an
# empty listing means for them.
tensorplate_packages() {
  dpkg-query -W -f='${binary:Package} ${db:Status-Status} ${Version}\n' 'tensorplate*' 2>/dev/null || true
}

# Remove every trace of a previous install: packages, conffiles and state.
clear_install() {
  note "clearing any previous TensorPlate install"
  sudo systemctl stop "$AGENT_UNIT" "$OBSERVABILITY_UNIT" >/dev/null 2>&1 || true

  # Purge exactly the TensorPlate packages dpkg knows about, and check
  # that it worked.
  #
  # A fixed list names packages that may never have been installed -- the
  # `tensorplate` metapackage comes only from the APT channel, never from
  # install.sh -- and apt-get aborts the WHOLE purge when any name is
  # unknown, leaving every package installed. Deleting /etc/tensorplate
  # after that removes conffiles dpkg still owns, and the reinstall treats
  # them as deliberately deleted and does not put them back. That is what
  # the first re-run on a real L4 host did: the purge failed silently, and
  # the services came up against an empty /etc/tensorplate.
  local purge=() pkg status
  while read -r pkg status _; do
    if [[ -n "$pkg" && "$status" != "not-installed" ]]; then
      purge+=("$pkg")
    fi
  done < <(tensorplate_packages)
  if ((${#purge[@]} > 0)); then
    step "purge ${purge[*]}" \
      sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y "${purge[@]}" || return
  fi

  # Nothing may remain in dpkg -- installed or holding config files --
  # before the state directories go, or the conffiles are stranded as
  # described above.
  local remaining=""
  while read -r pkg status _; do
    if [[ -n "$pkg" && "$status" != "not-installed" ]]; then
      remaining+="${pkg} (${status}) "
    fi
  done < <(tensorplate_packages)
  if [[ -n "$remaining" ]]; then
    printf 'TensorPlate packages remain after the purge: %s\n' "$remaining" >&2
    return 1
  fi
  step "clear installed state" sudo rm -rf \
    /etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate || return
}

# Install one artifact set through its own shipped installer.
#
# Each set was verified before the run started, and a run with a baseline
# installs from the same directories again, minutes apart. So the digest
# recorded then is compared again here, and a set whose SHA256SUMS has
# changed since is refused rather than installed under the old identity.
install_set() {
  local dir="$1" expected_digest="$2" allow_unsigned="$3" digest
  # An unreadable file reads as an empty digest, which never matches.
  digest="$( cd "$dir" && sha256sum SHA256SUMS | awk '{print $1}' )" || digest=""
  if [[ "$digest" != "$expected_digest" ]]; then
    printf '%s/SHA256SUMS changed after it was verified: recorded %s, now %s\n' \
      "$dir" "$expected_digest" "${digest:-unreadable}" >&2
    return 1
  fi
  local flags=(--local-artifacts "$dir" --yes --with-python-backend)
  if ((allow_unsigned)); then
    flags+=(--allow-unsigned)
  fi
  step "install.sh" sudo bash "${dir}/install.sh" "${flags[@]}" || return
}

stage_install() {
  clear_install || return

  note "installing the candidate through the shipped installer"
  install_set "$ASSETS_DIR" "$ARTIFACT_DIGEST" "$ALLOW_UNSIGNED" || return

  note "enabling the services"
  step "enable ${AGENT_UNIT}" sudo systemctl enable --now "$AGENT_UNIT" || return
  step "enable ${OBSERVABILITY_UNIT}" sudo systemctl enable --now "$OBSERVABILITY_UNIT" || return
  step "services ready" await_services_ready || return

  dpkg -l 'tensorplate*' >"${EVIDENCE_DIR}/packages.txt" 2>&1 || true
  step "doctor" bash -c \
    'tensorplate doctor --output json >"$1"' _ "${EVIDENCE_DIR}/doctor.json" || return
  check_doctor_green "${EVIDENCE_DIR}/doctor.json" || return
  pass "installed, services ready, doctor green, ${ROW} resolved by platform_row"
}

# Doctor has nothing failing and resolves this row by live detection.
check_doctor_green() {
  python3 - "$1" "$ROW" <<'PY'
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
}

# Exercise the currently active worker without changing the deployment.
# Both the initial deploy and restart must prove a live endpoint and a
# successful inference: durable active metadata alone survives worker
# failure in the packaged configuration, which has no supervisor block.
# The optional deploy response adds the initial transaction assertions.
check_worker_round_trip() {
  local status_output="$1" result_output="$2" deploy_output="${3:-}"
  local work infer_input infer_output
  work="$(mktemp -d)" || return
  infer_input="${work}/sample_infer.json"
  infer_output="${work}/infer-response.json"

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
    >"$result_output" <<'PY' || return
import json, sys, urllib.parse, urllib.request

deploy_path, status_path, input_path, response_path, expected = sys.argv[1:]
deploy = json.load(open(deploy_path, encoding="utf-8"))["payload"] if deploy_path else None
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
if deploy is not None:
    checks["deployment_phase"] = deploy.get("phase") == "active"
    checks["deployment_id"] = deploy.get("deployment_id") == expected
failed = [name for name, ok in checks.items() if not ok]
if failed:
    raise SystemExit("worker round-trip checks failed: " + ", ".join(failed))
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
  step "remove inference scratch files" rm -rf "$work" || return
}

# --- deploy-smoke ------------------------------------------------------

# Stage the smoke bundle, deploy it as DEPLOYMENT_ID, and prove the new
# worker answers health and inference. The live results go to the named
# file.
deploy_bundle() {
  local result_output="$1"
  local work deploy_output staged_bundle
  work="$(mktemp -d)" || return
  deploy_output="${work}/deploy.json"

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
  step "create the staging parent" sudo mkdir -p "$(dirname "$staged_bundle")" || return
  step "copy the bundle" sudo cp -R "$BUNDLE_DIR" "$staged_bundle" || return
  step "make the bundle readable" sudo chmod -R a+rX "$staged_bundle" || return

  note "deploying"
  step "deploy" bash -c \
    'tensorplate deploy "$1" --deployment-id "$2" --output json >"$3"' \
    _ "$staged_bundle" "$DEPLOYMENT_ID" "$deploy_output" || return

  step "active worker round trip" check_worker_round_trip \
    "${work}/status.json" "$result_output" "$deploy_output" || return
  step "remove deployment scratch files" rm -rf "$work" || return
}

stage_deploy_smoke() {
  deploy_bundle "${EVIDENCE_DIR}/deploy-result.json" || return
  pass "bundle admitted, worker ready, inference round-tripped"
}

# --- status-logs -------------------------------------------------------

capture_current_journal() {
  local unit="$1" output="$2" invocation
  invocation="$(systemctl show -p InvocationID --value "$unit")" || return
  [[ "$invocation" =~ ^[0-9a-f]{32}$ ]] ||
    { printf 'no current invocation ID for %s\n' "$unit" >&2; return 1; }
  # Privilege is needed even when the operator can access the agent
  # socket: membership in tensorplate does not grant journal access.
  step "capture the ${unit} journal" bash -c \
    'sudo journalctl -u "$1" "_SYSTEMD_INVOCATION_ID=$2" -n 100 --no-pager --output=json >"$3"' \
    _ "$unit" "$invocation" "$output" || return
  python3 - "$output" "${unit%.service}.service" "$invocation" <<'PY' || return
import json, sys

path, unit, invocation = sys.argv[1:]
count = 0
with open(path, encoding="utf-8") as journal:
    for line in journal:
        if not line.strip():
            continue
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            raise SystemExit(f"{unit}: journal output is not a JSON record")
        if not isinstance(entry, dict) or (
            entry.get("_SYSTEMD_UNIT") != unit
            or entry.get("_SYSTEMD_INVOCATION_ID") != invocation
        ):
            raise SystemExit(f"{unit}: journal record is not from the current service invocation")
        if not isinstance(entry.get("MESSAGE"), str) or not entry["MESSAGE"].strip():
            raise SystemExit(f"{unit}: journal record has no text message")
        count += 1
if not count:
    raise SystemExit(f"{unit}: no journal records from the current service invocation")
print(f"{unit}: captured {count} journal record(s) from the current invocation")
PY
}

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
  # On a real package install it exits 2 with "no log_source.path
  # configured". The CLI never reads the packaged /etc/tensorplate/cli.json
  # by default -- it loads only --config or $TENSORPLATE_CLI_CONFIG and
  # otherwise uses built-in defaults, which set no log source. Behind that
  # is a second gap: the path the packaged config names is one nothing in
  # the product writes, since both units log to the journal. The first was
  # observed on an L4 host; the second is from reading the tree. Either
  # way the failure is not something the release under test caused, so its
  # status is filed as evidence and the stage requires the log path a
  # packaged Linux install actually has.
  note "recording the operator-facing log command"
  tensorplate logs --component agent --tail 100 \
    >"${EVIDENCE_DIR}/agent-cli.log" 2>&1 || logs_status=$?
  printf '%s\n' "$logs_status" >"${EVIDENCE_DIR}/logs-command.exit"
  if ((logs_status != 0)); then
    note "tensorplate logs exited ${logs_status}: no component writes ${LOG_DIR}/tensorplate-agent.log on a packaged Linux install"
  fi

  # JSON distinguishes actual messages from journalctl diagnostics such
  # as "-- No entries --". Restrict both captures to the current service
  # invocations so old logs cannot certify a silent or inaccessible run.
  # Raw journal metadata must still be sanitized before publication.
  capture_current_journal "$AGENT_UNIT" "${EVIDENCE_DIR}/agent-journal.txt" || return
  capture_current_journal "$OBSERVABILITY_UNIT" "${EVIDENCE_DIR}/observability-journal.txt" || return
  [[ -d "$LOG_DIR" ]] ||
    { printf 'log directory missing at %s\n' "$LOG_DIR" >&2; return 1; }
  pass "status answered and still reports the deployment; both current service journals captured; log command status recorded"
}

# --- restart -----------------------------------------------------------

stage_restart() {
  local before_agent before_observability after_agent after_observability
  before_agent="$(systemctl show -p MainPID --value "$AGENT_UNIT")" || return
  before_observability="$(systemctl show -p MainPID --value "$OBSERVABILITY_UNIT")" || return
  note "restarting both services (agent pid ${before_agent}, observability pid ${before_observability})"

  step "restart both units" sudo systemctl restart "$AGENT_UNIT" "$OBSERVABILITY_UNIT" || return
  step "services ready again" await_services_ready || return

  after_agent="$(systemctl show -p MainPID --value "$AGENT_UNIT")" || return
  after_observability="$(systemctl show -p MainPID --value "$OBSERVABILITY_UNIT")" || return
  [[ "$after_agent" != "$before_agent" ]] ||
    { printf 'agent MainPID did not change across the restart\n' >&2; return 1; }
  [[ "$after_observability" != "$before_observability" ]] ||
    { printf 'observability MainPID did not change across the restart\n' >&2; return 1; }

  # The agent opens its control socket after startup recovery finishes.
  # Service readiness above therefore permits the same bounded live
  # worker checks used after deploy, without submitting another deploy.
  step "recovered worker round trip" check_worker_round_trip \
    "${EVIDENCE_DIR}/status-after-restart.json" "${EVIDENCE_DIR}/restart-result.json" || return
  pass "services restarted with new pids; recovered deployment answered health and inference"
}

# --- crash-loop --------------------------------------------------------

# Cleanup is safe to retry after a partial failure. Keep the original
# config available if restoration or service recovery fails, and report
# its location so the operator can recover it manually if needed.
cleanup_crash_loop() {
  [[ -n "$CRASH_LOOP_BACKUP" ]] || return 0
  local status=0
  note "restoring the agent config"
  step "restore the agent config" sudo cp -p "$CRASH_LOOP_BACKUP" "$AGENT_CONFIG" || status=$?
  if ((status != 0)); then
    printf 'agent config backup retained at %s\n' "$CRASH_LOOP_BACKUP" >&2
    return "$status"
  fi
  step "clear the agent's failed state" sudo systemctl reset-failed "$AGENT_UNIT" || status=$?
  step "start the agent" sudo systemctl start "$AGENT_UNIT" || status=$?
  step "services ready again" await_services_ready || status=$?
  if ((status != 0)); then
    printf 'agent config backup retained at %s\n' "$CRASH_LOOP_BACKUP" >&2
    return "$status"
  fi
  step "remove crash-loop scratch files" rm -rf "$(dirname "$CRASH_LOOP_BACKUP")" || return
  CRASH_LOOP_BACKUP=""
}

# Signal traps exit through this handler while the lifecycle runner still
# knows which stage was active. A second catchable signal must not interrupt
# restoration; SIGKILL and machine failure cannot be handled by a shell.
finish_with_cleanup() {
  local status="$1" cleanup_status=0
  trap - EXIT
  trap '' INT TERM HUP
  # Do not make restoration depend on opening another evidence file.
  cleanup_crash_loop || cleanup_status=$?
  if ((status == 0)); then
    status="$cleanup_status"
    if [[ -n "${_lc_active:-}" && "$status" -eq 0 ]]; then
      status=1
    fi
  fi
  lifecycle_abort "$status"
  exit "$status"
}

install_cleanup_traps() {
  trap 'finish_with_cleanup $?' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  trap 'exit 129' HUP
}

# Break the agent's config so every start fails, and watch what systemd
# does with it. The unit restarts on failure under a start limit, so the
# contract is a unit that is retried and then given up on -- and the
# failures have to be the agent refusing that config, not something else
# failing at the same time.
observe_crash_loop() {
  local since="$1"
  local state="" restarts="" last_restarts="" settled=0 attempt result

  note "breaking the agent config so every start fails"
  step "corrupt the agent config" \
    sudo bash -c 'printf "{ invalid json\n" >"$1"' _ "$AGENT_CONFIG" || return
  # Not checked: whether this command reports success depends on how fast
  # the agent exits. What it caused is observed below, and an agent that
  # was never restarted stays active and fails the settling check.
  sudo systemctl restart "$AGENT_UNIT" >/dev/null 2>&1 || true

  # Settled means not running and no longer being restarted: the count
  # stays the same across a sample longer than RestartSec. A failed state
  # alone does not show that, because a looping unit passes through it
  # between attempts.
  for ((attempt = 0; attempt < CRASH_LOOP_POLLS; attempt++)); do
    sleep "$CRASH_LOOP_POLL_SECONDS"
    state="$(systemctl show -p ActiveState --value "$AGENT_UNIT")" || return
    restarts="$(systemctl show -p NRestarts --value "$AGENT_UNIT")" || return
    if [[ "$state" != "active" && "$state" != "activating" && "$restarts" == "$last_restarts" ]]; then
      settled=1
      break
    fi
    last_restarts="$restarts"
  done
  result="$(systemctl show -p Result --value "$AGENT_UNIT")" || return
  if ((settled != 1)); then
    printf 'the agent never settled: ActiveState=%s NRestarts=%s Result=%s\n' \
      "$state" "$restarts" "$result" >&2
    return 1
  fi

  step "capture the crash-loop journal" bash -c \
    'sudo journalctl -u "$1" --since "@$2" --no-pager --output=json >"$3"' \
    _ "$AGENT_UNIT" "$since" "${EVIDENCE_DIR}/crash-loop-journal.txt" || return
  python3 - "${EVIDENCE_DIR}/crash-loop-journal.txt" "${AGENT_UNIT}.service" \
    "$state" "$restarts" "$result" >"${EVIDENCE_DIR}/crash-loop-result.json" <<'PY'
import json, sys

path, unit, state, restarts, result = sys.argv[1:]
config_errors = 0
with open(path, encoding="utf-8") as journal:
    for line in journal:
        if not line.strip():
            continue
        entry = json.loads(line)
        message = entry.get("MESSAGE")
        if entry.get("_SYSTEMD_UNIT") == unit and isinstance(message, str) \
                and message.startswith("config error"):
            config_errors += 1

checks = {
    # Given up on, not merely stopped.
    "unit_failed": state == "failed",
    # Restart=on-failure fired at least once before the start limit held.
    "restarted_before_giving_up": restarts.isdigit() and int(restarts) >= 1,
    # The starts that failed were the agent rejecting the broken config.
    "agent_rejected_the_config": config_errors >= 2,
}
failed = [name for name, ok in checks.items() if not ok]
if failed:
    raise SystemExit(
        f"crash-loop checks failed: {', '.join(failed)} "
        f"(ActiveState={state} NRestarts={restarts} Result={result} config_errors={config_errors})"
    )
print(json.dumps({
    "active_state": state,
    "restarts": int(restarts),
    "result": result,
    "config_error_records": config_errors,
}, indent=2, sort_keys=True))
PY
}

stage_crash_loop() {
  local work since status=0 cleanup_status=0
  work="$(mktemp -d)" || return
  step "back up the agent config" sudo cp -p "$AGENT_CONFIG" "${work}/agent.json" || return
  CRASH_LOOP_BACKUP="${work}/agent.json"
  since="$(date +%s)" || return

  observe_crash_loop "$since" || status=$?

  # Restored whatever the observation concluded, so a failed stage leaves
  # an appliance that can be inspected rather than one that cannot start.
  cleanup_crash_loop || cleanup_status=$?
  ((status == 0)) || return "$status"
  ((cleanup_status == 0)) || return "$cleanup_status"

  step "recovered worker round trip" check_worker_round_trip \
    "${EVIDENCE_DIR}/status-after-crash-loop.json" "${EVIDENCE_DIR}/crash-loop-recovery.json" || return
  pass "agent retried and given up on under a broken config; recovered deployment answered once restored"
}

# --- upgrade and rollback ------------------------------------------------

# The installed TensorPlate packages are exactly one side of the upgrade
# path: every package in that set installed at its manifest version, and
# nothing else installed or half-installed. tensorplate-apt-source is left
# out on both counts; it configures an APT channel, depends on nothing in
# TensorPlate, and neither install.sh nor a rollback touches it.
check_installed_versions() {
  local side="$1" listing
  listing="${EVIDENCE_DIR}/packages-${2}.txt"
  tensorplate_packages >"$listing" || return
  python3 - "$UPGRADE_PATH" "$side" "$listing" <<'PY' || return
import json, sys

path, side, listing = sys.argv[1:]
expected = json.loads(path)[side]
problems = []
seen = {}
for line in open(listing, encoding="utf-8"):
    fields = line.split()
    if len(fields) < 2 or fields[0] == "tensorplate-apt-source":
        continue
    seen[fields[0]] = (fields[1], fields[2] if len(fields) > 2 else "")
for package, version in sorted(expected["packages"].items()):
    status, installed = seen.pop(package, ("not-installed", ""))
    if status != "installed" or installed != version:
        problems.append(f"{package} is {status} {installed or '-'}, expected installed {version}")
for package, (status, installed) in sorted(seen.items()):
    if status not in ("not-installed", "config-files"):
        problems.append(f"{package} {installed} is {status} but is not in {expected['release_tag']}")
if problems:
    raise SystemExit("installed packages do not match the set: " + "; ".join(problems))
print(f"installed packages are exactly {expected['release_tag']}")
PY
}

unit_pids() {
  local agent observability
  agent="$(systemctl show -p MainPID --value "$AGENT_UNIT")" || return
  observability="$(systemctl show -p MainPID --value "$OBSERVABILITY_UNIT")" || return
  printf '%s %s\n' "$agent" "$observability"
}

operator_config_sha256() {
  python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' \
    "$OPERATOR_CONFIG"
}

check_operator_config_kept() {
  local what="$1" now
  now="$(operator_config_sha256)" || return
  if [[ "$now" != "$OPERATOR_CONFIG_SHA256" ]]; then
    printf '%s did not keep the operator-edited %s: sha256 was %s, now %s\n' \
      "$what" "$OPERATOR_CONFIG" "$OPERATOR_CONFIG_SHA256" "$now" >&2
    return 1
  fi
}

# Doctor on the baseline is filed, not asserted. install.sh already
# refuses a critical finding, and the baseline's own deploy and inference
# are what show it is a working place to move from or return to. Asserting
# more would let a defect the candidate fixed fail the candidate's run.
record_baseline_doctor() {
  local output="$1" status=0
  tensorplate doctor --output json >"$output" || status=$?
  printf '%s\n' "$status" >"${output%.json}.exit" || return
  note "doctor on the baseline exited ${status}; filed as evidence, not asserted"
}

write_upgrade_path() {
  python3 - "$UPGRADE_PATH" "$BASELINE_DIGEST" "$BASELINE_ALLOW_UNSIGNED" \
    "$ARTIFACT_DIGEST" "$ALLOW_UNSIGNED" >"${EVIDENCE_DIR}/upgrade-path.json" <<'PY'
import json, sys

path, from_digest, from_unsigned, to_digest, to_unsigned = sys.argv[1:]
path = json.loads(path)
path["from"].update({"sha256sums_sha256": from_digest, "allow_unsigned": from_unsigned == "1"})
path["to"].update({"sha256sums_sha256": to_digest, "allow_unsigned": to_unsigned == "1"})
print(json.dumps(path, indent=2, sort_keys=True))
PY
}

stage_upgrade() {
  local before after
  # First, so a failed attempt still says what it tried to move between.
  step "record the upgrade path" write_upgrade_path || return

  clear_install || return
  note "installing the baseline through its own installer"
  install_set "$BASELINE_DIR" "$BASELINE_DIGEST" "$BASELINE_ALLOW_UNSIGNED" || return
  step "baseline services ready" await_services_ready || return
  check_installed_versions from baseline || return
  record_baseline_doctor "${EVIDENCE_DIR}/doctor-baseline.json" || return

  note "deploying on the baseline"
  deploy_bundle "${EVIDENCE_DIR}/upgrade-baseline-deploy.json" || return

  # A trailing newline keeps the file valid JSON and its mode unchanged,
  # and makes it differ from the packaged conffile, which is what puts
  # dpkg's conffile handling under --force-confold on the path.
  note "editing ${OPERATOR_CONFIG} as an operator would"
  step "operator edit" sudo bash -c 'printf "\n" >>"$1"' _ "$OPERATOR_CONFIG" || return
  OPERATOR_CONFIG_SHA256="$(operator_config_sha256)" || return

  before="$(unit_pids)" || return
  # No systemctl call of the harness's own from here on: the package
  # scripts stop the services on upgrade and nothing in the packages
  # starts them, so bringing them back is the installer's job, and doing
  # it here would hide an installer that no longer does.
  note "upgrading to the candidate over the running baseline"
  install_set "$ASSETS_DIR" "$ARTIFACT_DIGEST" "$ALLOW_UNSIGNED" || return
  step "services ready after the upgrade" await_services_ready || return
  check_installed_versions to after-upgrade || return
  after="$(unit_pids)" || return
  if [[ "${after% *}" == "${before% *}" ]]; then
    printf 'agent MainPID %s did not change across the upgrade\n' "${after% *}" >&2
    return 1
  fi
  if [[ "${after#* }" == "${before#* }" ]]; then
    printf 'observability MainPID %s did not change across the upgrade\n' "${after#* }" >&2
    return 1
  fi
  check_operator_config_kept "the upgrade" || return

  step "doctor after the upgrade" bash -c \
    'tensorplate doctor --output json >"$1"' _ "${EVIDENCE_DIR}/doctor-after-upgrade.json" || return
  check_doctor_green "${EVIDENCE_DIR}/doctor-after-upgrade.json" || return

  # No deploy: the candidate has to have re-warmed the deployment the
  # baseline recorded in durable state.
  step "surviving deployment round trip" check_worker_round_trip \
    "${EVIDENCE_DIR}/status-after-upgrade.json" "${EVIDENCE_DIR}/upgrade-result.json" || return
  pass "upgraded in place with new pids; operator edit kept; doctor green; the baseline's deployment answered on the candidate"
}

# Removal leaves each package holding only its conffiles, never purged:
# the agent's config-files state is what shows the operator's config is
# still dpkg's to keep.
check_removed() {
  local listing="${EVIDENCE_DIR}/packages-after-remove.txt"
  tensorplate_packages >"$listing" || return
  python3 - "$listing" <<'PY' || return
import sys

problems = []
agent = "absent"
for line in open(sys.argv[1], encoding="utf-8"):
    fields = line.split()
    if len(fields) < 2 or fields[0] == "tensorplate-apt-source":
        continue
    if fields[0] == "tensorplate-agent":
        agent = fields[1]
    if fields[1] not in ("not-installed", "config-files"):
        problems.append(f"{fields[0]} is still {fields[1]}")
if agent != "config-files":
    problems.append(f"tensorplate-agent is {agent}, not config-files: its conffiles were not kept")
if problems:
    raise SystemExit("the removal did not leave only conffiles: " + "; ".join(problems))
print("every TensorPlate package is removed with its conffiles kept")
PY
}

stage_rollback() {
  local remove=() pkg status
  # Checked before anything changes. A state.bak from before the run is
  # already gone -- install and upgrade delete /var/lib/tensorplate while
  # clearing the host -- so this does not keep an earlier copy. It makes
  # sure the move below lands on a name nothing else holds, refused by
  # name before the services are stopped, rather than leaving mv -T to
  # replace an empty directory or refuse a full one halfway through.
  step "refuse to replace an existing ${STATE_ASIDE_DIR}" sudo test ! -e "$STATE_ASIDE_DIR" || return

  note "rolling back by the documented procedure"
  step "stop the services" sudo systemctl stop "$AGENT_UNIT" "$OBSERVABILITY_UNIT" || return
  step "set durable state aside" sudo mv -T "$STATE_DIR" "$STATE_ASIDE_DIR" || return

  # Every installed TensorPlate package, not a fixed list: the backend
  # only Recommends the agent, so a list without it leaves it at the
  # candidate's version for the older installer's apt-get -y to refuse to
  # downgrade.
  while read -r pkg status _; do
    if [[ -n "$pkg" && "$pkg" != "tensorplate-apt-source" &&
          "$status" != "not-installed" && "$status" != "config-files" ]]; then
      remove+=("$pkg")
    fi
  done < <(tensorplate_packages)
  if ((${#remove[@]} > 0)); then
    step "remove ${remove[*]}" \
      sudo DEBIAN_FRONTEND=noninteractive apt-get remove -y "${remove[@]}" || return
  fi
  check_removed || return

  note "installing the baseline fresh through its own installer"
  install_set "$BASELINE_DIR" "$BASELINE_DIGEST" "$BASELINE_ALLOW_UNSIGNED" || return
  step "services ready after the rollback" await_services_ready || return
  check_installed_versions from after-rollback || return
  check_operator_config_kept "the rollback" || return
  step "the set-aside state is preserved" sudo test -f "${STATE_ASIDE_DIR}/state.json" || return
  record_baseline_doctor "${EVIDENCE_DIR}/doctor-after-rollback.json" || return

  # The older agent must not have loaded the newer agent's state: it
  # answers, and has nothing active or previous.
  step "status after the rollback" bash -c \
    'tensorplate status --output json >"$1"' _ "${EVIDENCE_DIR}/status-after-rollback.json" || return
  python3 - "${EVIDENCE_DIR}/status-after-rollback.json" <<'PY' || return
import json, sys

agent = json.load(open(sys.argv[1], encoding="utf-8"))["payload"].get("agent") or {}
assert agent.get("available") is True, f"the agent is not available after the rollback: {agent}"
for key in ("active", "previous_active"):
    assert key in agent, f"status after the rollback does not report {key}"
    assert agent[key] is None, f"the rolled-back agent reports {key} {agent[key]}; it loaded state it should not have"
print("the rolled-back agent answers with no active or previous deployment")
PY

  note "deploying on the rolled-back version"
  deploy_bundle "${EVIDENCE_DIR}/rollback-result.json" || return
  pass "rolled back to the baseline; operator edit kept; state set aside and not loaded; deploy and inference answered"
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
  if ((BASELINE_REQUESTED)); then
    BASELINE_DIR="$(cd "$BASELINE_DIR" && pwd)"
  fi

  record_assets
  if ((BASELINE_REQUESTED)); then
    record_baseline_assets
  fi

  # shellcheck source=tools/validation/lifecycle-stages.sh disable=SC1091
  source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lifecycle-stages.sh"
  lifecycle_begin "$ROW" "$EVIDENCE_DIR" "$TESTED_VERSION" ubuntu-l4-cloud-lifecycle
  install_cleanup_traps

  lifecycle_stage install stage_install
  # Recorded after the install stage passes, so the digest attests an
  # install that happened. The value comes from memory: lifecycle_begin
  # has already cleared this directory's sidecar, and writing it is this
  # call's job.
  lifecycle_artifact_digest "$ARTIFACT_DIGEST" SHA256SUMS

  lifecycle_stage deploy-smoke stage_deploy_smoke
  lifecycle_stage status-logs stage_status_logs
  lifecycle_stage restart stage_restart
  lifecycle_stage crash-loop stage_crash_loop
  lifecycle_skip offline \
    "deferred: GCE platform detection requires live metadata at 169.254.169.254; offline validation needs product support for identity detection without network access"

  # After crash-loop, so every stage above is about a clean candidate
  # install and no crash-loop restore can still be pending once packages
  # start being purged and removed. A failure here exits with the stages
  # above already recorded.
  if ((BASELINE_REQUESTED)); then
    lifecycle_stage upgrade stage_upgrade
    lifecycle_stage rollback stage_rollback
  else
    lifecycle_skip upgrade \
      "no baseline artifact set was supplied with --baseline-assets-dir; upgrade needs a published, signed predecessor runtime set to move from"
    lifecycle_skip rollback \
      "no baseline artifact set was supplied with --baseline-assets-dir; rollback needs a published, signed predecessor runtime set to return to"
  fi

  lifecycle_finish
  printf 'evidence: %s\n' "$EVIDENCE_DIR"
  if ((BASELINE_REQUESTED)); then
    pass "lifecycle run complete; seven stages exercised, one skipped with its reason; the baseline is left installed"
  else
    pass "lifecycle run complete; five stages exercised, three skipped with reasons"
  fi
}

main "$@"
