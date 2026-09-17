#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Lifecycle validation for the Jetson Orin Nano row, run natively on the
# device.
#
# Run by hand on a Jetson that already carries JetPack 6.x (L4T R36) on
# Ubuntu 22.04, against a candidate artifact set already downloaded by
# tag with `tools/validation/jetson-clean-room.sh download`. It purges
# TensorPlate packages and state, installs the candidate through the
# release's own install.sh -- which verifies the SHA256SUMS signature --
# and writes the canonical lifecycle report itself through
# tools/validation/lifecycle-stages.sh.
#
# It replaces, for this row's evidence, the clean-room harness plus the
# step adapter and stage converter. That chain reported the weakest of
# the mapped steps that happened to be present, so a stage whose decisive
# step never ran could still read as a pass. Here each canonical stage is
# one function whose failure is recorded against that stage.
# jetson-clean-room.sh is unchanged and remains the release clean-room
# smoke.
#
# WHAT A PASSING RUN PROVES
#   install       the candidate installs through its shipped installer
#                 with signature verification, every runtime package is
#                 installed at the version its candidate .deb declares,
#                 the services come up, and doctor resolves this row by
#                 live detection with nothing failing
#   deploy-smoke  the control plane admits a TensorRT engine bundle, runs a
#                 worker for it, reports it active on the tensorrt backend,
#                 and an inference returns the input tensor unchanged
#   status-logs   status answers and still reports the deployment, and
#                 the services' journal carries their output
#   restart       both services restart and the agent re-warms its
#                 deployment from durable state
#   crash-loop    an agent that cannot load its config is retried and then
#                 given up on by systemd rather than restarted forever, and
#                 recovers its deployment once the config is restored
#
# With --baseline-tag and --baseline-assets-dir, two more stages run after
# crash-loop. The baseline is the last published arm64 runtime set, always
# installed through its own installer with its signature verified.
#
#   upgrade       over a fresh baseline install serving a deployment, the
#                 candidate's installer upgrades every package in place:
#                 both services come back under new pids, an operator's
#                 conffile edit survives, doctor is green, and the
#                 baseline's deployment re-warms from durable state with
#                 no deploy call of the harness's own
#   rollback      the documented procedure -- stop, set durable state
#                 aside, remove every TensorPlate package except the apt
#                 channel's bootstrap, install the baseline fresh --
#                 returns exactly the baseline set, keeps the operator
#                 edit and the set-aside state byte for byte, leaves the
#                 older agent with no deployment to misread, and serves
#                 a fresh one
#
# A run with a baseline leaves the BASELINE installed when it finishes.
#
# WHAT IT DOES NOT PROVE
#   The deploy-smoke bundle is a TensorRT identity engine, built on the
#   device by tools/validation/create_trt_identity_bundle.sh unless a
#   pre-built bundle is supplied. Its output must equal its input, so a
#   pass shows the TensorRT serving path loads an engine and returns a
#   tensor unchanged. It does NOT establish model accuracy, throughput or
#   accelerator performance, and the recorded result says so as
#   compute_claim "none". The Python/PyTorch backend is not installed and
#   is not exercised on this row by this harness.
#
# Offline under per-unit network denial is skipped, with its reason
# recorded in the report rather than omitted. It is follow-up harness
# work, and it belongs between crash-loop and upgrade when it lands: it
# is about the candidate install every stage above exercises, and the
# stages below replace that install with the baseline. No network policy
# is changed and no offline behaviour is certified by this harness.
# Without a baseline set, upgrade and rollback are skipped as well.
#
# Usage:
#   tools/validation/jetson-lifecycle.sh \
#     --candidate-tag <vX.Y.Z-rc.N> \
#     --candidate-assets-dir <downloaded assets> \
#     [--baseline-tag <vX.Y.Z> --baseline-assets-dir <downloaded assets>] \
#     --tested-version <X.Y.Z> \
#     --evidence-dir <new or empty dir> \
#     --confirm RESET-TENSORPLATE

# Stage steps redirect output inside capture_cli_json or `bash -c`, so
# the redirection belongs to the command whose status is being captured,
# rather than to `step` itself. The `bash -c` bodies are single-quoted on
# purpose: the child shell expands the positional parameters.
# shellcheck disable=SC2016

set -Eeuo pipefail

# Several checks below are Python `assert` statements, and a Python
# started with PYTHONOPTIMIZE set skips every one of them: the check
# passes without running. An operator's environment is not allowed to
# turn a failing check into a pass.
unset PYTHONOPTIMIZE

readonly DEFAULT_ROW="jetson-orin-nano-8gb-jp62"
readonly CONFIRM_TOKEN="RESET-TENSORPLATE"
readonly AGENT_UNIT="tensorplate-agent"
readonly OBSERVABILITY_UNIT="tensorplate-observability"
# The apt channel's bootstrap package depends on nothing in TensorPlate
# and is what a lab image uses to reach the channel, so the run leaves it
# as it found it: installed where it was installed, absent where it was
# absent. install.sh never installs it, so a device set up by the
# runbook's own install.sh run does not carry it. Every other
# tensorplate* package is purged.
readonly APT_SOURCE_PACKAGE="tensorplate-apt-source"
# The published predecessor this row's upgrade and rollback are validated
# against: the last published arm64 runtime set. The harness accepts no
# other baseline, because nothing in the report could tell a gate which
# one a run used -- the report's subject names the candidate alone -- and
# an earlier candidate of the same release is not the upgrade path a
# Jetson in the field takes. A later release moves this pin on purpose.
readonly BASELINE_RELEASE_TAG="v0.1.5"
# The runtime packages that ship a file under /etc, which dh marks as a
# conffile: agent.json, serving_worker.json, observability.json and
# cli.json (packaging/debian/tensorplate-*.install). These are the
# packages an `apt remove` must leave in dpkg's config-files state, and
# the ones whose conffiles a purge would take with them.
#
# tensorplate-common is deliberately absent: it installs nothing under
# /etc, so dpkg has no conffiles to keep for it and a removal drops it
# straight to not-installed. Requiring config-files of it would fail
# every correct rollback on a real device.
CONFFILE_PACKAGES=(tensorplate-agent tensorplate-serving
                   tensorplate-observability tensorplate-cli)
readonly DEB_ARCH="arm64"
# Overridable so the stage bodies can be driven against a stubbed
# appliance in CI. A validation harness whose stages only ever run on a
# machine CI cannot reach is a harness whose assertions nobody has seen
# fire, which is how a stage that certifies failures as passes ships.
AGENT_SOCKET_PATH="${TP_JETSON_AGENT_SOCKET:-/run/tensorplate/agent.sock}"
LOG_DIR="${TP_JETSON_LOG_DIR:-/var/log/tensorplate}"
# Outside every path the agent's unit hides from it. The unit sets
# PrivateTmp=true, so /tmp and /var/tmp are a private namespace the agent
# cannot see into, and ProtectHome=true, so nothing under /home is
# visible either. ProtectSystem=strict leaves the rest of the filesystem
# readable, which is all a bundle needs.
BUNDLE_STAGING_DIR="${TP_JETSON_BUNDLE_STAGING:-/opt/tensorplate-validation/trt-identity}"
# RestartSec is 5 in the shipped unit, so every readiness wait has to sit
# well above it rather than racing a restart. Overridable only so a unit
# that never becomes ready can be driven without waiting in CI.
READY_TIMEOUT_SECONDS="${TP_JETSON_READY_TIMEOUT_SECONDS:-60}"
readonly AGENT_CONFIG="/etc/tensorplate/agent.json"
# What the install stage deletes once the purge has succeeded. An input
# under any of these, or under the bundle staging directory, would be
# gone by the time the run reads it, so preflight refuses one.
CLEARED_STATE_DIRS=(/etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate)
# The group the packages create and the agent's control socket belongs to.
readonly SERVICE_GROUP="tensorplate"
# Longer than RestartSec, so a unit that is still looping has restarted
# at least once between two samples. Overridable only so the settling
# logic can be driven without waiting in CI.
CRASH_LOOP_POLL_SECONDS="${TP_JETSON_CRASH_LOOP_POLL_SECONDS:-7}"
readonly CRASH_LOOP_POLLS=40
# Registered after the backup succeeds and before the config is changed.
# Kept until restoration and service recovery succeed, including on EXIT.
CRASH_LOOP_BACKUP=""
# The documented rollback sets durable state aside under this name
# (docs/install/lifecycle.md) rather than carrying it back.
readonly STATE_DIR="/var/lib/tensorplate/state"
readonly STATE_ASIDE_DIR="/var/lib/tensorplate/state.bak"
# The agent's deployment state, the one file in that directory this stage
# is written around: without it there is nothing for the rollback to
# preserve. It is NOT the only durable file there, which is why the check
# below is about the directory and not about this name.
readonly STATE_FILE_NAME="state.json"
# Every file the directory held with its digest, taken with the services
# stopped and before the rollback moves the directory. What the rollback
# claims about the set-aside state is that its CONTENTS survive the
# removal and the baseline install, which a pathname cannot show.
STATE_MANIFEST=""
# The conffile an operator edits before the upgrade, and whose bytes both
# directions must keep. Every CLI call this run makes is pinned to its own
# private config, so nothing here reads this file and the edit cannot
# change the behaviour under test. Overridable only so a stubbed appliance
# can supply the file.
OPERATOR_CONFIG="${TP_JETSON_OPERATOR_CONFIG:-/etc/tensorplate/cli.json}"
OPERATOR_CONFIG_SHA256=""
# The windows in which a run can end with this device neither serving
# the candidate nor carrying a working baseline, named so an interrupted
# run says what it left rather than leaving the operator to discover it:
#
#   upgrade           from clearing the candidate until the baseline
#                     install returns
#   rollback-stopped  from stopping the candidate's services until the
#                     removal starts; the candidate is still installed
#   rollback          from the removal until the baseline install returns
#
# Set on entering one and cleared once that baseline install returns.
# They leave different devices behind, which is why the name is recorded
# rather than a flag: the upgrade's clear_install DELETES durable state
# along with the packages, while the rollback sets it aside first.
STRANDED_WINDOW=""
# What the run did inside the window, as opposed to what dpkg says, which
# report_stranded_device reads again when it reports. A listing taken
# before the baseline installer ran says nothing about what that
# installer left: v0.1.5's install.sh exits non-zero AFTER installing
# every package when the services do not come up or doctor reports a
# critical finding.
#
#   STRANDED_STATE_CLEARED  clear_install deleted the state directories
#   STRANDED_STATE_ASIDE    the rollback moved durable state to state.bak
#   STRANDED_CONFFILES      what check_removed read: `kept`, `lost <pkgs>`,
#                           or `unread` when it never read a listing
#   STRANDED_INSTALL_LOG    the baseline installer's output, once started
STRANDED_STATE_CLEARED=0
STRANDED_STATE_ASIDE=0
STRANDED_CONFFILES=unread
STRANDED_INSTALL_LOG=""
# Scratch directory holding a bundle this run built, removed on exit.
BUNDLE_SCRATCH=""
# Private CLI config pins every command to the installed local agent; an
# operator profile must never redirect this run to another control plane
# or override which worker receives the identity inference.
CLI_SCRATCH=""
CLI_CONFIG=""

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly REPO_ROOT
readonly BUNDLE_BUILDER="${REPO_ROOT}/tools/validation/create_trt_identity_bundle.sh"
readonly IDENTITY_VERIFIER="${REPO_ROOT}/tools/validation/verify_trt_identity_response.py"

ROW="$DEFAULT_ROW"
CANDIDATE_TAG=""
ASSETS_DIR=""
# Either baseline option asks for the upgrade and rollback stages, so a
# half-specified baseline is refused rather than read as a run without
# one: a typo in one option would otherwise silently skip both stages.
BASELINE_REQUESTED=0
BASELINE_TAG=""
BASELINE_DIR=""
BUNDLE_DIR=""
EVIDENCE_DIR=""
TESTED_VERSION=""
DEPLOYMENT_ID="jetson-lifecycle-smoke"
# Distinct ids, so the evidence says which install created the deployment
# a stage found active. The id the candidate reports after the upgrade
# can only have been written by the baseline, and the one the baseline
# reports after the rollback can only be the fresh deploy that followed
# it. Derived from --deployment-id in main.
BASELINE_DEPLOYMENT_ID=""
ROLLBACK_DEPLOYMENT_ID=""
CONFIRM_VALUE=""
PREFLIGHT_ONLY=0

# Host facts, read through overridable paths so the eligibility rules can
# be exercised off a Jetson. The same seams the release installer uses,
# for the same reason: these branches decide whether a run may start, and
# a rule nobody can test is a rule nobody can trust.
HOST_ARCH="${TP_JETSON_ARCH:-$(uname -m)}"
OS_RELEASE_FILE="${TP_JETSON_OS_RELEASE:-/etc/os-release}"
NV_TEGRA_RELEASE_FILE="${TP_JETSON_NV_TEGRA_RELEASE:-/etc/nv_tegra_release}"

usage() {
  cat <<EOF
Usage:
  jetson-lifecycle.sh [options] --confirm ${CONFIRM_TOKEN}

Validates a candidate artifact set natively on a Jetson running JetPack 6.x
(L4T R36) on Ubuntu 22.04, and writes a canonical lifecycle report. Purges
TensorPlate packages and state, so it refuses to start without the
confirmation token. It leaves the candidate installed and does not
restore what it removes.

Options:
  --candidate-tag TAG         Release tag the candidate assets were downloaded
                              for, such as v0.2.1-rc.2. Its X.Y.Z must equal
                              --tested-version. Required.
  --candidate-assets-dir DIR  The downloaded candidate set: install.sh, exactly
                              one artifact manifest naming TAG, SHA256SUMS and
                              its cosign bundle, and the .deb packages.
                              Required.
  --baseline-tag TAG          Release tag of the published predecessor set to
                              upgrade from and roll back to. Must be
                              ${BASELINE_RELEASE_TAG}, this row's baseline, and
                              strictly older than --candidate-tag, with its
                              set's runtime packages strictly older than the
                              candidate set's, package by package.
  --baseline-assets-dir DIR   The downloaded baseline set, laid out like
                              --candidate-assets-dir. With --baseline-tag,
                              runs the upgrade and rollback stages; without
                              either, both are skipped. Always installed with
                              its signature verified. The run then ends with
                              the BASELINE installed.
  --tested-version X.Y.Z      The release this run authorizes. Bare version,
                              never a candidate spelling. Required.
  --evidence-dir DIR          Where the report and stage logs are written. Must
                              be new or empty. Required.
  --row ROW_ID                Support row under test. Default: ${DEFAULT_ROW}
  --bundle-dir DIR            A pre-built TensorRT identity bundle. Default:
                              build one on this device with
                              tools/validation/create_trt_identity_bundle.sh
                              before anything is purged.
                              The assets, evidence and bundle directories
                              must lie outside what the run deletes:
                              /etc, /var/lib, /var/log and /run tensorplate,
                              and ${BUNDLE_STAGING_DIR}.
  --deployment-id ID          Deployment id for the smoke. Default: ${DEPLOYMENT_ID}
  --preflight-only            Check host eligibility and the inputs, and build
                              the bundle in a temporary directory, then stop
                              without installing anything or writing a report.
  --confirm TOKEN             Required. Must equal ${CONFIRM_TOKEN}.
  --help                      Show this help text.
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
      --candidate-tag) CANDIDATE_TAG="${2:-}"; shift 2 ;;
      --candidate-assets-dir) ASSETS_DIR="${2:-}"; shift 2 ;;
      --baseline-tag) BASELINE_REQUESTED=1; BASELINE_TAG="${2:-}"; shift 2 ;;
      --baseline-assets-dir) BASELINE_REQUESTED=1; BASELINE_DIR="${2:-}"; shift 2 ;;
      --tested-version) TESTED_VERSION="${2:-}"; shift 2 ;;
      --evidence-dir) EVIDENCE_DIR="${2:-}"; shift 2 ;;
      --row) ROW="${2:-}"; shift 2 ;;
      --bundle-dir) BUNDLE_DIR="${2:-}"; shift 2 ;;
      --deployment-id) DEPLOYMENT_ID="${2:-}"; shift 2 ;;
      --preflight-only) PREFLIGHT_ONLY=1; shift ;;
      --confirm) CONFIRM_VALUE="${2:-}"; shift 2 ;;
      --help|-h) usage; exit 0 ;;
      *) usage >&2; die "unknown option: $1" ;;
    esac
  done
}

# The digest that identifies the artifacts under test, and the checksum
# verification output, both taken in preflight before anything is
# installed. SHA256SUMS is the file hashed: no run installs a single
# package, and this is the file install.sh verifies the signature of and
# checks the selected packages against. Hashed from inside the assets
# directory so the recorded name is the bare file name.
#
# The digest is held in memory rather than written beside the evidence
# here: lifecycle_begin clears that sidecar so a retry cannot inherit a
# previous attempt's digest, and it is lifecycle_artifact_digest, after
# the install stage passes, that writes the run's own.
ARTIFACT_DIGEST=""
CHECKSUM_OUTPUT=""
# The baseline set is verified the same way, before anything is
# installed. Its digest is not what the report is about -- the report
# attests the candidate -- so it is filed on its own, in
# baseline-digest.txt beside the stage logs, together with the upgrade
# path the run moved along.
BASELINE_DIGEST=""
BASELINE_CHECKSUM_OUTPUT=""
# The upgrade path preflight admitted, as
# {"from": {"release_tag", "packages"}, "to": {...}}, where packages maps
# each runtime package this row installs to the Debian version its set
# declares. write_upgrade_path merges the digests in and files it.
UPGRADE_PATH=""

# A set's SHA256SUMS verification output, and the digest of the file
# itself. Kept as two calls rather than one that prints both, so a
# caller reads exactly what it asked for.
assets_checksum_output() {
  ( cd "$1" && sha256sum -c SHA256SUMS 2>&1 )
}

assets_digest() {
  ( cd "$1" && sha256sum SHA256SUMS | awk '{print $1}' )
}

# install.sh accepts any release tag's signature, so a signed set is not
# thereby the set for this tag. The manifest's release.tag is what binds
# the directory to the tag the operator named.
#
# A baseline is additionally refused when its own manifest records it as
# an unreleased local snapshot, which keeps a set somebody built out of
# the run. That is a snapshot filter and not a proof of publication: the
# fields it reads are written into the very directory under test. What
# establishes publication on this row is where the set came from --
# jetson-clean-room.sh download fetches it unauthenticated from the
# public release URL, which a draft's assets are not reachable at. The
# stronger binding is tools/validation/check-baseline-publication.py,
# which the Ubuntu cloud harness calls; adopting it here would make this
# preflight depend on reaching GitHub from the device, and is left as
# follow-up work rather than decided in passing.
check_assets_manifest() {
  local dir="$1" tag="$2" reject_snapshot="$3"
  python3 - "$dir" "$tag" "$reject_snapshot" <<'PY'
import json, pathlib, sys

assets, tag, reject_snapshot = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3] == "1"
manifests = sorted(assets.glob("tensorplate-*-artifacts.json"))
if len(manifests) != 1:
    raise SystemExit(
        f"expected exactly one tensorplate-*-artifacts.json in the assets directory; found {len(manifests)}"
    )
release = json.loads(manifests[0].read_text(encoding="utf-8")).get("release") or {}
if release.get("tag") != tag:
    raise SystemExit(f"{manifests[0].name} names release tag {release.get('tag')!r}, not {tag!r}")
if reject_snapshot and (release.get("unreleased") is not False
                        or release.get("provenance") != "github-release"):
    raise SystemExit(
        f"{manifests[0].name} records a local snapshot rather than a release build: it records "
        f"unreleased={release.get('unreleased')!r} provenance={release.get('provenance')!r}"
    )
PY
}

# The upgrade path the two sets describe, refused unless every runtime
# package this row installs is strictly older in the baseline set.
#
# Tag order does not establish this. The tag is release metadata; what
# apt orders is the Debian version each .deb carries. A baseline whose
# tag is older but whose packages are not makes the candidate install in
# the upgrade stage a downgrade, which the `apt-get -y` inside install.sh
# refuses without --allow-downgrades -- the same hazard the rollback
# avoids by removing tensorplate-common, met in the other direction and
# only after the device has already been rebuilt twice.
#
# The versions compared are read from each .deb's own control field with
# dpkg-deb, not from the manifest. The manifest's `version` is parsed out
# of the file name by the release driver (tools/release/tensorplate-release.sh)
# and never read from the package, so a .deb whose control Version says
# something else -- an epoch, which a file name cannot carry, or a
# hand-assembled directory -- would be ordered on a string apt does not
# use. check_installed_versions already reads the control field after
# each install; reading it here moves that truth to preflight, before
# the device has been rebuilt at all.
#
# Only the five packages install.sh selects for this row are compared:
# the run passes neither --with-python-backend nor --cli-only, so the
# backend package is never installed here, and tensorplate-apt-source is
# left alone throughout.
read_upgrade_path() {
  python3 - "$BASELINE_DIR" "$ASSETS_DIR" "$DEB_ARCH" <<'PY'
import json, pathlib, re, subprocess, sys

RUNTIME = ("tensorplate-common", "tensorplate-agent", "tensorplate-serving",
           "tensorplate-observability", "tensorplate-cli")
# The shape of a Debian version: an optional epoch, then an upstream
# version starting with a digit. dpkg --compare-versions cannot be the
# check: it treats an empty version as older than any other, and only
# warns about some malformed ones before comparing them anyway.
DEBIAN_VERSION = re.compile(r"(?:[0-9]+:)?[0-9][A-Za-z0-9.+~-]*")

deb_arch = sys.argv[3]

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
        # The same selection install.sh makes: this package's .deb for
        # the host architecture or for all architectures.
        matches = [
            artifact for artifact in manifest.get("artifacts", [])
            if isinstance(artifact, dict)
            and artifact.get("package") == package
            and str(artifact.get("file", "")).endswith(".deb")
            and artifact.get("architecture") in (deb_arch, "all")
        ]
        if len(matches) != 1:
            raise SystemExit(
                f"the {label} set must list exactly one {package} package for "
                f"{deb_arch} or all; found {len(matches)}"
            )
        # The manifest has to declare a version for every package it
        # publishes; a set that does not is malformed whatever the .deb
        # says, and dpkg --compare-versions would read a missing one as
        # older than anything.
        declared = matches[0].get("version")
        if not isinstance(declared, str) or not DEBIAN_VERSION.fullmatch(declared):
            raise SystemExit(
                f"the {label} set lists {package} at version {declared!r}, "
                "which is not a Debian version"
            )
        # What apt will order on. dpkg-deb is a required command, checked
        # in preflight before this runs.
        read = subprocess.run(
            ["dpkg-deb", "-f", str(pathlib.Path(directory) / matches[0]["file"]), "Version"],
            capture_output=True, text=True,
        )
        version = read.stdout.strip()
        if read.returncode != 0 or not DEBIAN_VERSION.fullmatch(version):
            raise SystemExit(
                f"the {label} set's {matches[0]['file']} does not carry a readable "
                f"Debian Version in its control file: dpkg-deb exited {read.returncode} "
                f"and reported {version!r}"
            )
        packages[package] = version
    return manifest.get("release") or {}, packages

baseline_release, baseline = read_set("baseline", sys.argv[1])
candidate_release, candidate = read_set("candidate", sys.argv[2])

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

# Everything about the baseline that must be true before the run starts.
# Checked in preflight for the same reason the candidate is: the upgrade
# and rollback stages run after the candidate install has already
# replaced the device, and a baseline found wanting there would leave the
# device mid-procedure over an input that was wrong from the start.
preflight_baseline() {
  [[ -n "$BASELINE_TAG" ]] ||
    die "--baseline-assets-dir needs --baseline-tag naming the release it holds"
  [[ -n "$BASELINE_DIR" ]] ||
    die "--baseline-tag needs --baseline-assets-dir holding that release's set"
  [[ -d "$BASELINE_DIR" ]] || die "--baseline-assets-dir must name a directory"
  [[ "$BASELINE_TAG" == "$BASELINE_RELEASE_TAG" ]] ||
    die "--baseline-tag must be ${BASELINE_RELEASE_TAG}, this row's published predecessor; got ${BASELINE_TAG}"

  # Strictly older, with a release candidate sorting below the release it
  # leads to, so a candidate that is not newer than the pinned baseline is
  # refused. The tags are what the operator names, what the manifests
  # bind each directory to, and what the runbook records; the installed
  # package versions are asserted separately against each set's own .deb
  # files once that set is installed.
  python3 - "$BASELINE_TAG" "$CANDIDATE_TAG" <<'PY' || die "the baseline is not older than the candidate"
import re, sys

def sort_key(tag):
    match = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)(?:-rc\.([1-9]\d*))?", tag)
    if match is None:
        raise SystemExit(f"{tag} is not a release tag")
    major, minor, patch, candidate = match.groups()
    return (int(major), int(minor), int(patch), 0 if candidate else 1, int(candidate or 0))

baseline, candidate = sys.argv[1:]
if not sort_key(baseline) < sort_key(candidate):
    raise SystemExit(
        f"the baseline {baseline} must be strictly older than the candidate {candidate}; "
        "rolling back to the same or a newer set proves nothing about either"
    )
PY

  [[ -f "${BASELINE_DIR}/install.sh" ]] || die "missing ${BASELINE_DIR}/install.sh"
  [[ -f "${BASELINE_DIR}/SHA256SUMS" ]] || die "missing ${BASELINE_DIR}/SHA256SUMS"
  check_assets_manifest "$BASELINE_DIR" "$BASELINE_TAG" 1 ||
    die "the baseline assets are not a ${BASELINE_TAG} release build"

  # After the tags, so a pair the operator named the wrong way round is
  # refused by the option they got wrong rather than by the packages.
  UPGRADE_PATH="$(read_upgrade_path)" ||
    die "the baseline and candidate sets do not form an upgrade path"

  BASELINE_CHECKSUM_OUTPUT="$(assets_checksum_output "$BASELINE_DIR")" ||
    { printf '%s\n' "$BASELINE_CHECKSUM_OUTPUT" >&2; die "the baseline artifact set failed verification"; }
  BASELINE_DIGEST="$(assets_digest "$BASELINE_DIR")"
  [[ "$BASELINE_DIGEST" =~ ^[0-9a-f]{64}$ ]] ||
    die "could not compute a digest for ${BASELINE_DIR}/SHA256SUMS"
  pass "baseline ${BASELINE_TAG} is not a snapshot and is older than ${CANDIDATE_TAG} by tag and by every runtime package version, verified"
}

# Everything that must be true before the run starts.
#
# These are checked before lifecycle_begin, so a host that was never
# eligible produces a refusal and no report at all, rather than a report
# whose install stage failed for a reason that is not about the release.
preflight() {
  [[ "$CONFIRM_VALUE" == "$CONFIRM_TOKEN" ]] ||
    die "this purges TensorPlate packages and state; re-run with --confirm ${CONFIRM_TOKEN}"
  [[ -n "$ASSETS_DIR" && -d "$ASSETS_DIR" ]] || die "--candidate-assets-dir must name a directory"
  [[ -n "$EVIDENCE_DIR" ]] || die "--evidence-dir is required"
  [[ -n "$TESTED_VERSION" ]] || die "--tested-version is required"
  [[ "$TESTED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
    die "--tested-version must be a bare X.Y.Z release version, not a candidate spelling"
  [[ "$CANDIDATE_TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[1-9][0-9]*)?$ ]] ||
    die "--candidate-tag must be a release tag, vX.Y.Z or vX.Y.Z-rc.N"
  # The report is filed against --tested-version while the packages come
  # from the tag, so the two must name the same release. Without this a
  # candidate for a later release could authorize an earlier one.
  local tag_version="${CANDIDATE_TAG#v}"
  tag_version="${tag_version%%-*}"
  [[ "$tag_version" == "$TESTED_VERSION" ]] ||
    die "--candidate-tag ${CANDIDATE_TAG} is not a build of ${TESTED_VERSION}"
  [[ ! -e "$EVIDENCE_DIR" || -z "$(find "$EVIDENCE_DIR" -mindepth 1 -print -quit)" ]] ||
    die "--evidence-dir must be new or empty: ${EVIDENCE_DIR}"

  [[ "${EUID}" -ne 0 ]] ||
    die "run as a normal user; this script calls sudo for privileged steps"
  require_command sudo
  require_command systemctl
  require_command journalctl
  require_command python3
  require_command sha256sum
  require_command dpkg-query
  require_command dpkg-deb
  # Compares the two sets' package versions when a baseline is given.
  # Required unconditionally: every host carrying dpkg-query has it, and
  # a run that discovered it missing only once a baseline was named would
  # refuse later than it can.
  require_command dpkg

  # Every release installer this run starts reads TP_INSTALL_* from its
  # environment, and several of those switch its verification off or
  # point it elsewhere: TP_INSTALL_ALLOW_UNSIGNED skips the signature
  # check exactly as --allow-unsigned does, TP_INSTALL_SKIP_SELF_CHECK
  # skips the installer's own, TP_INSTALL_COSIGN replaces the verifier and
  # TP_INSTALL_REPO the identity it accepts -- in v0.1.5's install.sh as
  # in the candidate's. sudo's env_reset drops them only where the sudo
  # policy does not keep them, so a run carrying any is refused by name.
  local name installer_knobs=""
  for name in $(compgen -e); do
    case "$name" in
      TP_INSTALL_*) installer_knobs="${installer_knobs:+${installer_knobs} }${name}" ;;
      *) ;;
    esac
  done
  [[ -z "$installer_knobs" ]] ||
    die "unset ${installer_knobs} first: this run installs every set with the installer's own defaults, signature verification included"

  # Every CLI call the harness makes as the operator goes through the
  # agent's group-only control socket. The group exists only once a
  # TensorPlate package has installed, and it survives the purge, so a
  # session that predates joining it is refused here rather than failing
  # doctor after the purge. Membership is read from this session, which
  # is what the CLI calls inherit, not from the group database.
  if getent group "$SERVICE_GROUP" >/dev/null 2>&1; then
    [[ " $(id -nG 2>/dev/null) " == *" ${SERVICE_GROUP} "* ]] ||
      die "this session is not in the ${SERVICE_GROUP} group; run sudo usermod -aG ${SERVICE_GROUP} \"\$USER\" and start a new session"
  fi

  [[ "$HOST_ARCH" == "aarch64" ]] ||
    die "this harness validates aarch64 Jetson rows; host reports ${HOST_ARCH}"

  local os_id os_version
  [[ -r "$OS_RELEASE_FILE" ]] || die "cannot read ${OS_RELEASE_FILE}"
  os_id="$(read_os_release_field ID)"
  os_version="$(read_os_release_field VERSION_ID)"
  [[ "$os_id" == "ubuntu" && "$os_version" == 22.04* ]] ||
    die "expected Ubuntu 22.04; host reports ID=${os_id:-unknown} VERSION_ID=${os_version:-unknown}"

  # The installer's own test for L4T R36, so the harness admits exactly
  # the hosts the installer will install on.
  [[ -r "$NV_TEGRA_RELEASE_FILE" ]] ||
    die "cannot read ${NV_TEGRA_RELEASE_FILE}; expected NVIDIA Jetson L4T release metadata"
  grep -Eq '(^|[^0-9])R36([^0-9]|$)' "$NV_TEGRA_RELEASE_FILE" ||
    die "${NV_TEGRA_RELEASE_FILE} is not L4T R36.x"

  [[ -f "${ASSETS_DIR}/install.sh" ]] || die "missing ${ASSETS_DIR}/install.sh"
  [[ -f "${ASSETS_DIR}/SHA256SUMS" ]] || die "missing ${ASSETS_DIR}/SHA256SUMS"

  check_assets_manifest "$ASSETS_DIR" "$CANDIDATE_TAG" 0 ||
    die "the candidate assets are not the ${CANDIDATE_TAG} release set"

  CHECKSUM_OUTPUT="$(assets_checksum_output "$ASSETS_DIR")" ||
    { printf '%s\n' "$CHECKSUM_OUTPUT" >&2; die "the candidate artifact set failed verification"; }
  ARTIFACT_DIGEST="$(assets_digest "$ASSETS_DIR")"
  [[ "$ARTIFACT_DIGEST" =~ ^[0-9a-f]{64}$ ]] ||
    die "could not compute a digest for ${ASSETS_DIR}/SHA256SUMS"

  # Checked before the bundle's contents, so a bundle already gone from
  # a deleted directory is still refused for where it is. The clean-room
  # smoke builds its bundle under /var/lib/tensorplate, and this run
  # leaves its staged copy in place, so both are paths an operator may
  # reasonably pass.
  python3 - "${CLEARED_STATE_DIRS[@]}" "$BUNDLE_STAGING_DIR" -- \
    --candidate-assets-dir "$ASSETS_DIR" --evidence-dir "$EVIDENCE_DIR" \
    ${BASELINE_DIR:+--baseline-assets-dir "$BASELINE_DIR"} \
    ${BUNDLE_DIR:+--bundle-dir "$BUNDLE_DIR"} <<'PY' || die "an input lies in a directory this run deletes"
import os, sys

separator = sys.argv.index("--")
deleted, given = sys.argv[1:separator], sys.argv[separator + 1:]
for option, value in zip(given[::2], given[1::2]):
    path = os.path.realpath(value)
    for directory in deleted:
        root = os.path.realpath(directory)
        if path == root or path.startswith(root.rstrip("/") + "/"):
            raise SystemExit(
                f"{option} {value} is under {directory}, which this run deletes; "
                "use a directory outside it"
            )
PY

  if [[ -n "$BUNDLE_DIR" ]]; then
    [[ -f "${BUNDLE_DIR}/manifest.json" && -f "${BUNDLE_DIR}/sample_infer.json" ]] ||
      die "--bundle-dir must contain manifest.json and sample_infer.json"
  fi

  # After the location check above, so a baseline in a directory this run
  # deletes is refused for where it is before it is read.
  if ((BASELINE_REQUESTED)); then
    preflight_baseline
  fi
  pass "host is eligible: Ubuntu ${os_version} on ${HOST_ARCH}, L4T R36, ${CANDIDATE_TAG} assets verified"
}

remove_bundle_scratch() {
  [[ -n "$BUNDLE_SCRATCH" ]] || return 0
  rm -rf "$BUNDLE_SCRATCH" || return
  BUNDLE_SCRATCH=""
}

# Build the smoke bundle before anything is purged, so a device that
# cannot build one is refused while its install is still intact. The
# generator compiles a small C++ TensorRT builder and runs it here, which
# needs a C++ compiler, the CUDA headers and libnvinfer on the device.
prepare_bundle() {
  [[ -z "$BUNDLE_DIR" ]] || return 0
  BUNDLE_SCRATCH="$(mktemp -d)" || die "could not create a scratch directory for the bundle"
  trap 'remove_bundle_scratch' EXIT
  note "building the TensorRT identity bundle on this device"
  sh "$BUNDLE_BUILDER" "${BUNDLE_SCRATCH}/trt-identity" ||
    die "could not build the TensorRT identity bundle on this device; it needs a C++ compiler, CUDA headers and libnvinfer, or pass --bundle-dir with a bundle built for this device"
  BUNDLE_DIR="${BUNDLE_SCRATCH}/trt-identity"
}

# Recorded, not asserted: facts that differ legitimately between devices
# on this row. No host name, serial or machine id is read.
record_host_facts() {
  local output="${EVIDENCE_DIR}/host-facts.txt"
  : >"$output" || return
  host_fact "kernel" uname -r >>"$output" || return
  host_fact "os" read_os_release_field PRETTY_NAME >>"$output" || return
  host_fact "l4t" head -n 1 "$NV_TEGRA_RELEASE_FILE" >>"$output" || return
  host_fact "systemd" bash -c 'set -o pipefail; systemctl --version | head -n 1' >>"$output" || return
  host_fact "power mode" bash -c 'set -o pipefail; nvpmodel -q 2>/dev/null | tr "\n" " "' >>"$output" || return
}

host_fact() {
  local label="$1" value
  shift
  value="$("$@" 2>/dev/null)" || value="unavailable"
  printf '%s: %s\n' "$label" "$value"
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

prepare_cli_config() {
  CLI_SCRATCH="$(mktemp -d)" || return
  CLI_CONFIG="${CLI_SCRATCH}/cli.json"
  python3 - "$CLI_CONFIG" "$AGENT_SOCKET_PATH" <<'PYCONFIG'
import json, pathlib, sys

path, socket = sys.argv[1:]
pathlib.Path(path).write_text(json.dumps({
    "schema_version": "0.1",
    "default_profile": "local",
    "profiles": {"local": {"mode": "local", "socket_path": socket}},
}) + "\n", encoding="utf-8")
PYCONFIG
}

remove_cli_scratch() {
  [[ -n "$CLI_SCRATCH" ]] || return 0
  rm -rf "$CLI_SCRATCH" || return
  CLI_SCRATCH=""
  CLI_CONFIG=""
}

run_cli() {
  tensorplate --config "$CLI_CONFIG" "$@"
}

# Keep the redirection inside the command whose status step captures, so
# failure to create an evidence file fails the stage as well.
capture_cli_json() {
  local output="$1"
  shift
  run_cli "$@" >"$output"
}

# Every tensorplate* package dpkg knows about, one `name status` per
# line. A failed query cannot establish that nothing is installed:
# deleting conffiles after such a query would strand them in dpkg's
# database. The documented no-match exit is accepted only with no output
# and its exact diagnostic.
#
# With no argument the apt channel's bootstrap package and every
# `not-installed` row are left out, which is what the purge and the
# removal need: a name apt-get was never given aborts the whole
# operation, leaving every package installed. `all` keeps every row,
# which is what the rollback's evidence needs -- in the filtered listing
# a package dpkg purged and one it merely no longer names are the same
# absence, and those are not the same outcome.
installed_tensorplate_packages() {
  python3 - "$APT_SOURCE_PACKAGE" "${1:-installed-only}" <<'PYPACKAGES'
import os, subprocess, sys

query = subprocess.run(
    ["dpkg-query", "-W", "-f=${binary:Package} ${db:Status-Status}\n", "tensorplate*"],
    capture_output=True, text=True, env={**os.environ, "LC_ALL": "C"},
)
if query.stderr:
    sys.stderr.write(query.stderr)
if query.returncode:
    no_matches = (
        query.returncode == 1
        and not query.stdout
        and query.stderr.rstrip("\n") == "dpkg-query: no packages found matching tensorplate*"
    )
    if no_matches:
        raise SystemExit(0)
    sys.stderr.write(f"could not determine installed TensorPlate packages: dpkg-query exited {query.returncode}\n")
    raise SystemExit(query.returncode if query.returncode > 0 else 1)
for line in query.stdout.splitlines():
    fields = line.split()
    if len(fields) != 2:
        raise SystemExit(f"invalid dpkg-query package inventory record: {line!r}")
    package, status = fields
    if sys.argv[2] == "all" or (package != sys.argv[1] and status != "not-installed"):
        print(package, status)
PYPACKAGES
}

# Remove every trace of the candidate install: packages, conffiles and
# state.
#
# Called by the upgrade stage before the baseline is installed fresh: an
# upgrade has to start from a device that carries the baseline and
# nothing else, or what the candidate's installer is asked to upgrade is
# not the baseline. It is the install stage's own clearing, command for
# command; that stage keeps its body as it was, so the two are kept in
# step by hand, and the verifier fails a purge, a leftover package and a
# failed query in each.
clear_install() {
  note "clearing any previous TensorPlate install"
  sudo systemctl stop "$AGENT_UNIT" "$OBSERVABILITY_UNIT" >/dev/null 2>&1 || true

  # Purge exactly the TensorPlate packages dpkg knows about, and check
  # that it worked.
  #
  # A fixed list names packages that may never have been installed, and
  # apt-get aborts the WHOLE purge when any name is unknown, leaving every
  # package installed. Deleting /etc/tensorplate after that removes
  # conffiles dpkg still owns, and the reinstall treats them as
  # deliberately deleted and does not put them back. The cloud harness
  # met exactly that on a real host.
  local purge=() listing pkg status
  listing="$(installed_tensorplate_packages)" || return
  while read -r pkg status; do
    if [[ -n "$pkg" ]]; then
      purge+=("$pkg")
    fi
  done <<<"$listing"
  if ((${#purge[@]} > 0)); then
    step "purge ${purge[*]}" \
      sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y "${purge[@]}" || return
  fi

  # Nothing may remain in dpkg -- installed or holding config files --
  # before the state directories go, or the conffiles are stranded as
  # described above.
  listing="$(installed_tensorplate_packages)" || return
  if [[ -n "$listing" ]]; then
    printf 'TensorPlate packages remain after the purge: %s\n' "$(printf '%s' "$listing" | tr '\n' ' ')" >&2
    return 1
  fi
  step "clear installed state" sudo rm -rf "${CLEARED_STATE_DIRS[@]}" || return
  # Completed, not assumed: only now may a report from this window say the
  # state directories went with the packages.
  STRANDED_STATE_CLEARED=1
}

# The prefix every installer this function starts runs behind: it drops
# each TP_INSTALL_* variable that sudo's policy or PAM still handed over,
# for the reasons preflight refuses them in the run's own environment,
# then replaces itself with the installer. Enumerated rather than listed,
# so a knob a later install.sh adds is dropped too.
readonly INSTALLER_ENV_SCRUB='for name in $(compgen -e); do case "$name" in TP_INSTALL_*) unset "$name" ;; esac; done; exec bash "$@"'
# What both releases' install.sh print once cosign has accepted the
# SHA256SUMS signature as the TensorPlate release workflow's, and print
# only then (packaging/scripts/install.sh, verify_checksums_signature, at
# v0.1.5 and now).
readonly SIGNATURE_VERIFIED_LINE="==> SHA256SUMS signature verified: signed by tensorplate/tensorplate release workflow"

# Install one artifact set through its own shipped installer, for the
# upgrade and rollback stages. The install stage keeps its own call.
#
# No --allow-unsigned, for either set: install.sh verifies the SHA256SUMS
# signature with the cosign bundle that was downloaded with it, and its
# output has to say so -- an installer that skipped verification exits 0
# all the same. No --with-python-backend: this row's smoke is the
# TensorRT engine.
#
# A run with a baseline installs from the same directories again, minutes
# apart, so the digest recorded in preflight is compared again here and a
# set whose SHA256SUMS has changed since is refused rather than installed
# under the identity the earlier verification gave it.
install_set() {
  local dir="$1" expected_digest="$2" output="$3" digest
  # An unreadable file reads as an empty digest, which never matches.
  digest="$(assets_digest "$dir")" || digest=""
  if [[ "$digest" != "$expected_digest" ]]; then
    printf '%s/SHA256SUMS changed after it was verified: recorded %s, now %s\n' \
      "$dir" "$expected_digest" "${digest:-unreadable}" >&2
    return 1
  fi
  # Set before the installer starts: from here what the device carries
  # is whatever the installer left, whether or not it succeeded.
  STRANDED_INSTALL_LOG="$output"
  step "install.sh" bash -c \
    'set -o pipefail; sudo bash -c "$1" tensorplate-install "$2/install.sh" --local-artifacts "$2" --yes 2>&1 | tee "$3"' \
    _ "$INSTALLER_ENV_SCRUB" "$dir" "$output" || return
  if ! grep -Fxq -- "$SIGNATURE_VERIFIED_LINE" "$output"; then
    printf '%s/install.sh succeeded without reporting a verified SHA256SUMS signature; expected the line: %s\n' \
      "$dir" "$SIGNATURE_VERIFIED_LINE" >&2
    return 1
  fi
}

stage_install() {
  step "pin CLI commands to the local agent" prepare_cli_config || return
  note "clearing any previous TensorPlate install"
  sudo systemctl stop "$AGENT_UNIT" "$OBSERVABILITY_UNIT" >/dev/null 2>&1 || true

  # Purge exactly the TensorPlate packages dpkg knows about, and check
  # that it worked.
  #
  # A fixed list names packages that may never have been installed, and
  # apt-get aborts the WHOLE purge when any name is unknown, leaving every
  # package installed. Deleting /etc/tensorplate after that removes
  # conffiles dpkg still owns, and the reinstall treats them as
  # deliberately deleted and does not put them back. The cloud harness
  # met exactly that on a real host.
  local purge=() listing pkg status
  listing="$(installed_tensorplate_packages)" || return
  while read -r pkg status; do
    if [[ -n "$pkg" ]]; then
      purge+=("$pkg")
    fi
  done <<<"$listing"
  if ((${#purge[@]} > 0)); then
    step "purge ${purge[*]}" \
      sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y "${purge[@]}" || return
  fi

  # Nothing may remain in dpkg -- installed or holding config files --
  # before the state directories go, or the conffiles are stranded as
  # described above.
  listing="$(installed_tensorplate_packages)" || return
  if [[ -n "$listing" ]]; then
    printf 'TensorPlate packages remain after the purge: %s\n' "$(printf '%s' "$listing" | tr '\n' ' ')" >&2
    return 1
  fi
  step "clear installed state" sudo rm -rf "${CLEARED_STATE_DIRS[@]}" || return

  # No --allow-unsigned: install.sh verifies the SHA256SUMS signature
  # with the cosign bundle that was downloaded with the set. No
  # --with-python-backend: this row's smoke is the TensorRT engine.
  note "installing the candidate through the shipped installer"
  step "install.sh" sudo bash "${ASSETS_DIR}/install.sh" --local-artifacts "$ASSETS_DIR" --yes || return

  note "enabling the services"
  step "enable ${AGENT_UNIT}" sudo systemctl enable --now "$AGENT_UNIT" || return
  step "enable ${OBSERVABILITY_UNIT}" sudo systemctl enable --now "$OBSERVABILITY_UNIT" || return
  step "services ready" await_services_ready || return

  step "installed versions" check_installed_versions "${EVIDENCE_DIR}/packages.txt" || return
  # Doctor writes its report and then exits non-zero when anything fails,
  # so the report is read before the exit status decides the stage: the
  # log then names the failing findings rather than only the exit code.
  local doctor_status=0
  step "doctor" capture_cli_json "${EVIDENCE_DIR}/doctor.json" \
    doctor --output json || doctor_status=$?
  check_doctor_green "${EVIDENCE_DIR}/doctor.json" || return
  ((doctor_status == 0)) || return "$doctor_status"
  pass "installed at the candidate's package versions, services ready, doctor green, ${ROW} resolved by platform_row"
}

# Every runtime package is installed at exactly the version the .deb in
# the given set declares. A service answering doctor does not show that:
# an install.sh that left an older package in place, or a set whose
# packages were not the ones selected, would otherwise pass. After an
# upgrade or a rollback it is also what shows the device carries one set
# and not a mixture of both.
#
# Only these five are compared, because only these five are ever
# installed on this row: the run passes neither --with-python-backend nor
# --cli-only, so the Python backend package is never installed and cannot
# survive at the other set's version, and tensorplate-apt-source is left
# alone deliberately. The rollback's own check_removed is what shows
# nothing else is left installed.
#
# Usage: check_installed_versions <output> [<assets dir> <set name>]
# The set defaults to the candidate's, which is what the install stage
# checks.
check_installed_versions() {
  python3 - "${2:-$ASSETS_DIR}" "$DEB_ARCH" "${3:-candidate}" >"$1" <<'PY'
import json, pathlib, subprocess, sys

assets, deb_arch, set_name = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
# install.sh selects these five for a runtime install without the Python
# backend; see write_install_deb_list there.
packages = ["tensorplate-common", "tensorplate-agent", "tensorplate-serving",
            "tensorplate-observability", "tensorplate-cli"]
manifest_path = sorted(assets.glob("tensorplate-*-artifacts.json"))[0]
artifacts = json.loads(manifest_path.read_text(encoding="utf-8")).get("artifacts", [])

problems = []
for package in packages:
    # The same selection install.sh makes: this package's .deb for the
    # host architecture or for all architectures. install.sh has already
    # refused a manifest naming a file outside the assets directory.
    debs = [
        a["file"] for a in artifacts
        if isinstance(a, dict) and a.get("package") == package
        and isinstance(a.get("file"), str) and a["file"].endswith(".deb")
        and a.get("architecture") in ("all", deb_arch)
    ]
    if len(debs) != 1:
        problems.append(f"{package}: the manifest selects {len(debs)} .deb files, not one")
        continue
    expected = subprocess.run(
        ["dpkg-deb", "-f", str(assets / debs[0]), "Version"],
        capture_output=True, text=True,
    )
    if expected.returncode != 0 or not expected.stdout.strip():
        problems.append(f"{package}: could not read the Version of {debs[0]}")
        continue
    installed = subprocess.run(
        ["dpkg-query", "-W", "-f=${db:Status-Status} ${Version}", package],
        capture_output=True, text=True,
    )
    wanted = f"installed {expected.stdout.strip()}"
    found = installed.stdout.strip() if installed.returncode == 0 else "not installed"
    if found != wanted:
        problems.append(f"{package}: expected {wanted!r}, dpkg reports {found!r}")
        continue
    print(f"{package} {expected.stdout.strip()} {debs[0]}")
if problems:
    raise SystemExit(f"installed package versions do not match the {set_name} set: " + "; ".join(problems))
PY
}

# Doctor has nothing failing and resolves this row by live detection.
check_doctor_green() {
  python3 - "$1" "$ROW" <<'PY'
import json, sys

path, expected_row = sys.argv[1:]
payload = json.load(open(path, encoding="utf-8"))["payload"]
by_id = {f["id"]: f for f in payload["findings"]}

assert payload["failing"] == 0, "doctor reports {} failing finding(s): {}".format(
    payload["failing"], ", ".join(f["id"] for f in payload["findings"] if f.get("status") == "fail"))

# platform_row is the finding that resolves host identity AND accelerator
# to one row; platform_profile answers from host identity alone and is
# deliberately a candidate set. Both are asserted, because platform_row's
# refusals are Warnings rather than Fails and so are invisible to
# `failing == 0`.
row = by_id.get("platform_row") or {}
assert row.get("status") == "ok", f"platform_row is {row.get('status')}: {row.get('message')}"
assert expected_row in row.get("message", ""), \
    f"platform_row did not name {expected_row}: {row.get('message')}"

profile = by_id.get("platform_profile") or {}
assert expected_row in profile.get("message", ""), \
    f"{expected_row} is not among the host's candidate rows: {profile.get('message')}"

for required_ok in ("platform_registry", "agent_reachable", "agent_socket",
                    "serving_binary_installed", "path_layout", "config_files"):
    finding = by_id.get(required_ok) or {}
    assert finding.get("status") == "ok", \
        f"{required_ok} is {finding.get('status')}: {finding.get('message')}"

# Recorded rather than asserted. The Python backend is not installed on
# this run, so its findings are informational; the TensorRT and CUDA
# runtime findings are what the deploy-smoke round trip then exercises.
for recorded in ("host_os", "accelerator_facts", "tensorrt_runtime", "cuda_runtime"):
    finding = by_id.get(recorded)
    if finding is None:
        print(f"{recorded}: absent")
    else:
        print(f"{recorded}: {finding.get('status')}: {finding.get('message')}")
print(f"doctor: {len(payload['findings'])} findings, 0 failing, row {expected_row}")
PY
}

# Exercise the currently active worker without changing the deployment.
# Both the initial deploy and each recovery must prove a live endpoint and
# a successful identity inference: durable active metadata alone survives
# worker failure. The optional deploy response adds the initial
# transaction assertions.
#
# The worker must be serving the smoke deployment, unless it is called
# through check_deployment_round_trip, which names another.
check_trt_round_trip() {
  local status_output="$1" result_output="$2" deploy_output="${3:-}"
  local expected_id="${round_trip_deployment_id:-$DEPLOYMENT_ID}"
  local work infer_output infer_command
  work="$(mktemp -d)" || return
  infer_output="${work}/infer-response.json"
  infer_command="${work}/infer-command.json"

  note "issuing the identity inference request"
  # The request the bundle ships, from the staged copy: the same
  # invocation the clean-room smoke has run on this device class.
  step "infer" capture_cli_json "$infer_command" infer \
    --input "${BUNDLE_STAGING_DIR}/sample_infer.json" \
    --output-file "$infer_output" --output json || return
  cat "$infer_command" || return
  step "the engine returned its input unchanged" \
    python3 "$IDENTITY_VERIFIER" "$infer_output" || return
  step "status" capture_cli_json "$status_output" status --output json || return

  python3 - "$deploy_output" "$status_output" "$expected_id" "$infer_command" >"$result_output" <<'PY' || return
import json, sys, urllib.parse, urllib.request

deploy_path, status_path, expected, infer_path = sys.argv[1:]
deploy = json.load(open(deploy_path, encoding="utf-8"))["payload"] if deploy_path else None
status = json.load(open(status_path, encoding="utf-8"))["payload"]
inference = json.load(open(infer_path, encoding="utf-8"))["payload"]

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
    except (OSError, ValueError):
        health = {}

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
    "active_backend": active.get("backend") == "tensorrt",
    "active_serving_url": serving_url_valid,
    # The verified tensor must come from the same active worker whose
    # health is checked here, never a profile's serving_url override.
    "inference_endpoint_source": inference.get("endpoint_source") == "agent-discovered",
    "inference_endpoint": serving_url_valid and inference.get("endpoint") == serving_url,
    "serving_health_state": health.get("state") == "ready",
    "serving_health_deployment": health.get("active_model_id") == expected,
    "supervision_healthy_when_configured": supervision_healthy,
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
    "backend": "tensorrt",
    "supervision_state": "not_configured" if supervision is None else supervision["serving_state"],
    "inference_round_trip": "tensorrt_identity",
    # An identity engine returning its input shows the serving path
    # works; it is no statement about accuracy, speed or compute.
    "compute_claim": "none",
}, indent=2, sort_keys=True))
PY
  step "remove inference scratch files" rm -rf "$work" || return
}

# check_trt_round_trip against the deployment named first: the upgrade's
# and the rollback's, which are not the smoke's. The name is local, so it
# reaches only this call.
check_deployment_round_trip() {
  local round_trip_deployment_id="$1"
  shift
  check_trt_round_trip "$@"
}

# --- deploy-smoke ------------------------------------------------------

stage_deploy_smoke() {
  local work deploy_output
  work="$(mktemp -d)" || return
  deploy_output="${work}/deploy.json"

  note "validating the deploy-smoke bundle before deploying it"
  python3 - "$BUNDLE_DIR" <<'PY' || return
import hashlib, json, pathlib, sys

bundle = pathlib.Path(sys.argv[1]).resolve()
manifest = json.loads((bundle / "manifest.json").read_text(encoding="utf-8"))
if manifest.get("backend_hint") != "tensorrt":
    raise SystemExit("deploy-smoke bundle must declare backend_hint=tensorrt")
models = [a for a in manifest.get("artifacts", [])
          if isinstance(a, dict) and a.get("role") == "model"]
if len(models) != 1:
    raise SystemExit("deploy-smoke bundle must declare exactly one model artifact")
artifact = models[0]
if artifact.get("kind") != "tensorrt_engine":
    raise SystemExit("deploy-smoke model artifact must be a tensorrt_engine")
path = (bundle / artifact["path"]).resolve()
if bundle not in path.parents:
    raise SystemExit("deploy-smoke model artifact escapes the bundle root")
digest = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
if digest != artifact.get("digest"):
    raise SystemExit("deploy-smoke model artifact digest does not match its manifest")
json.loads((bundle / "sample_infer.json").read_text(encoding="utf-8"))
print(json.dumps({"bundle": manifest.get("name"), "backend_hint": "tensorrt",
                  "engine_digest": digest}, sort_keys=True))
PY

  # The agent opens the bundle itself, as the tensorplate user, from the
  # path the CLI sends -- the CLI does not upload it. The bundle is staged
  # somewhere that user can read and the unit's sandbox does not hide.
  note "staging the bundle at ${BUNDLE_STAGING_DIR} for the agent to read"
  step "stage the bundle" sudo rm -rf "$BUNDLE_STAGING_DIR" || return
  step "create the staging parent" sudo mkdir -p "$(dirname "$BUNDLE_STAGING_DIR")" || return
  step "copy the bundle" sudo cp -R "$BUNDLE_DIR" "$BUNDLE_STAGING_DIR" || return
  step "make the bundle readable" sudo chmod -R a+rX "$BUNDLE_STAGING_DIR" || return

  note "deploying"
  step "deploy" capture_cli_json "$deploy_output" deploy \
    "$BUNDLE_STAGING_DIR" --deployment-id "$DEPLOYMENT_ID" --output json || return

  step "active worker round trip" check_trt_round_trip \
    "${work}/status.json" "${EVIDENCE_DIR}/deploy-result.json" "$deploy_output" || return
  step "remove deployment scratch files" rm -rf "$work" || return
  pass "TensorRT bundle admitted, worker ready, identity inference round-tripped"
}

# --- status-logs -------------------------------------------------------

# Kept in the same shape as the cloud harness's capture, so a later change
# to what the evidence copy retains applies to both harnesses together.
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
  step "status" capture_cli_json "${EVIDENCE_DIR}/status.json" status --output json || return
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

  # The operator-facing log command is recorded, not required. On a
  # packaged install it exits non-zero with no log_source.path configured:
  # the CLI reads only --config or $TENSORPLATE_CLI_CONFIG, and the path
  # the packaged config names is one nothing writes, since both units log
  # to the journal. Its status is filed as evidence and the stage requires
  # the log path a packaged Linux install actually has.
  note "recording the operator-facing log command"
  run_cli logs --component agent --tail 100 \
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
  step "recovered worker round trip" check_trt_round_trip \
    "${EVIDENCE_DIR}/status-after-restart.json" "${EVIDENCE_DIR}/restart-result.json" || return
  pass "services restarted with new pids; recovered deployment answered health and identity inference"
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
  # Says so rather than fixing it: a device left without a working install
  # is a decision for the operator, not something to undo behind them.
  # Guarded, because this runs under errexit and a terminal that has gone
  # away fails every write to it; the report below must still be written.
  report_stranded_device || true
  if ((status == 0)); then
    status="$cleanup_status"
    if [[ -n "${_lc_active:-}" && "$status" -eq 0 ]]; then
      status=1
    fi
  fi
  lifecycle_abort "$status"
  # A scratch bundle left behind is untidy, not a validation result, so
  # its removal is reported without changing the run's status.
  remove_bundle_scratch ||
    printf 'warning: could not remove the scratch bundle at %s\n' "$BUNDLE_SCRATCH" >&2
  remove_cli_scratch ||
    printf 'warning: could not remove the private CLI config at %s\n' "$CLI_SCRATCH" >&2
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

  step "recovered worker round trip" check_trt_round_trip \
    "${EVIDENCE_DIR}/status-after-crash-loop.json" "${EVIDENCE_DIR}/crash-loop-recovery.json" || return
  pass "agent retried and given up on under a broken config; recovered deployment answered once restored"
}

# --- upgrade and rollback ----------------------------------------------

deploy_bundle() {
  local output="$1" id="$2"
  note "deploying ${id} from the staged bundle"
  step "deploy ${id}" capture_cli_json "$output" deploy \
    "$BUNDLE_STAGING_DIR" --deployment-id "$id" --output json || return
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

# The sha256 of a file only root can read. /var/lib/tensorplate is
# root-owned, so every read of the durable state goes through sudo, the
# way the journal captures and the crash-loop config backup do, rather
# than through a sudo path of this check's own.
#
# A missing or unreadable file fails here: sha256sum names it and exits
# non-zero, and a digest that is not sha256 hex is refused rather than
# carried forward, so two unreadable files can never compare equal.
privileged_sha256() {
  local path="$1" line digest
  line="$(sudo sha256sum "$path")" || return
  digest="${line%% *}"
  if [[ ! "$digest" =~ ^[0-9a-f]{64}$ ]]; then
    printf 'could not compute a sha256 of %s: sha256sum printed %s\n' "$path" "$line" >&2
    return 1
  fi
  printf '%s\n' "$digest"
}

# Every file in a durable-state directory, one "<name> <sha256>" line per
# file, sorted so that two directories compare as text.
#
# The directory is the unit, not one pathname in it. The agent persists
# state.json AND the same-directory copy state.json.bak it falls back to
# when the primary fails to decode (agent/src/state.rs), and the
# observability unit writes its snapshot beside them
# (packaging/conf/observability.json). A check on a single name would
# leave the rest of the recoverable state unguarded, and would see
# nothing at all when a file was added or removed.
#
# /var/lib/tensorplate is root-owned, so the listing and every digest go
# through sudo, the way the journal captures and the crash-loop config
# backup do, rather than through a sudo path of this check's own.
# Anything in there that cannot be digested -- a subdirectory, a dangling
# symlink -- fails here by name rather than being skipped, and a
# directory with nothing in it fails rather than comparing equal to
# another empty one.
state_manifest() {
  local dir="$1" names name digest manifest=""
  names="$(sudo ls -A "$dir")" || return
  names="$(printf '%s\n' "$names" | LC_ALL=C sort)" || return
  while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    digest="$(privileged_sha256 "${dir}/${name}")" || return
    manifest="${manifest}${name} ${digest}"$'\n'
  done <<<"$names"
  if [[ -z "$manifest" ]]; then
    printf '%s holds no files; there is no durable state here to preserve\n' "$dir" >&2
    return 1
  fi
  printf '%s' "$manifest"
}

# The digest of one entry in a manifest, empty when the manifest does not
# list it. An exact field compare, so a name is never matched as a
# pattern -- every name here carries a `.`.
manifest_digest() {
  printf '%s\n' "$2" | awk -v name="$1" '$1 == name { print $2 }'
}

# The manifest the rollback has to carry across, taken with the agent
# stopped and before anything moves or removes the directory: after the
# move there is no original left to compare the saved copy with.
#
# A state directory with no state.json is not the device this stage is
# written for -- the upgrade deployed before the rollback started -- and
# a run that accepted one would be comparing two directories that hold
# nothing the rollback is about.
capture_state_manifest() {
  STATE_MANIFEST="$(state_manifest "$STATE_DIR")" || return
  if [[ -z "$(manifest_digest "$STATE_FILE_NAME" "$STATE_MANIFEST")" ]]; then
    printf '%s holds no %s: there is no deployment state for the rollback to preserve\n' \
      "$STATE_DIR" "$STATE_FILE_NAME" >&2
    return 1
  fi
}

# The set-aside state, byte for byte as the stopped agent left it.
#
# `test -f` proves only that a pathname is a regular file, so a removal
# or an install that emptied, truncated or rewrote a saved file would
# pass it while the recoverable state this stage claims to preserve was
# gone. Nothing later reads these files back -- the empty-agent and
# fresh-deploy checks below exist to show the older agent did NOT load
# them -- so the manifest taken before the move is the only thing that
# can tell preserved state from destroyed state.
#
# The first entry the two disagree on is named, whether it changed, went
# missing, or was never there before.
check_state_preserved() {
  local now name digest saved
  now="$(state_manifest "$STATE_ASIDE_DIR")" || return
  if [[ "$now" == "$STATE_MANIFEST" ]]; then
    return 0
  fi
  while IFS=' ' read -r name digest; do
    [[ -n "$name" ]] || continue
    saved="$(manifest_digest "$name" "$now")"
    if [[ -z "$saved" ]]; then
      printf 'the rollback did not preserve %s/%s: it was in the durable state when the services were stopped and the set-aside copy does not hold it\n' \
        "$STATE_ASIDE_DIR" "$name" >&2
      return 1
    fi
    if [[ "$saved" != "$digest" ]]; then
      printf 'the rollback did not preserve %s/%s: sha256 was %s when the services were stopped, now %s\n' \
        "$STATE_ASIDE_DIR" "$name" "$digest" "$saved" >&2
      return 1
    fi
  done <<<"$STATE_MANIFEST"
  while IFS=' ' read -r name _; do
    [[ -n "$name" ]] || continue
    if [[ -z "$(manifest_digest "$name" "$STATE_MANIFEST")" ]]; then
      printf 'the rollback did not preserve %s: it holds %s, which the durable state did not when the services were stopped\n' \
        "$STATE_ASIDE_DIR" "$name" >&2
      return 1
    fi
  done <<<"$now"
  # Unreachable while the manifests are the sorted "<name> <sha256>" lines
  # both sides build the same way, and a fail-closed backstop if they are
  # ever not: two manifests that differ must never pass this check.
  printf 'the rollback did not preserve %s: the set-aside state does not match what the stopped services left\n' \
    "$STATE_ASIDE_DIR" >&2
  return 1
}

# Doctor on the baseline is filed, not asserted. The baseline's own
# install.sh already refuses a critical finding, and the baseline deploy
# and inference below are what show it is a working place to move from or
# return to. Asserting more would let a finding the candidate added, or
# one the candidate fixed, decide the candidate's own run. The baseline
# predates platform_row, so check_doctor_green does not apply to it.
record_baseline_doctor() {
  local output="$1" status=0
  run_cli doctor --output json >"$output" || status=$?
  printf '%s\n' "$status" >"${output%.json}.exit" || return
  note "doctor on the baseline exited ${status}; filed as evidence, not asserted"
}

# The two sets this run moves between: the path preflight admitted, with
# the digest of the file whose signature each installer verified merged
# in. It carries the package versions preflight compared rather than a
# restatement of the options, so the evidence says why the path was
# admitted and not only which tags were named. Neither side is ever
# installed unsigned, and the report's own subject.artifact_digest stays
# the candidate's: this file is what says where the candidate was reached
# from and returned to.
write_upgrade_path() {
  python3 - "$UPGRADE_PATH" "$BASELINE_DIGEST" "$ARTIFACT_DIGEST" \
    >"${EVIDENCE_DIR}/upgrade-path.json" <<'PY'
import json, sys

path, from_digest, to_digest = sys.argv[1:]
path = json.loads(path)
path["from"].update({"sha256sums_sha256": from_digest, "allow_unsigned": False})
path["to"].update({"sha256sums_sha256": to_digest, "allow_unsigned": False})
print(json.dumps(path, indent=2, sort_keys=True))
PY
}

stage_upgrade() {
  local before after doctor_status=0
  # First, so a failed attempt still says what it tried to move between.
  step "record the upgrade path" write_upgrade_path || return

  # An upgrade has to start from a device carrying the baseline and
  # nothing else. The candidate installed above is cleared for the same
  # reason the run cleared whatever preceded it.
  #
  # From here until the baseline install returns this device serves
  # nothing, and clear_install DELETES its conffiles and durable state
  # rather than setting them aside. That is a worse place to be left than
  # the rollback's window, so it is reported the same way.
  STRANDED_WINDOW=upgrade
  clear_install || return
  note "installing the ${BASELINE_TAG} baseline through its own installer"
  install_set "$BASELINE_DIR" "$BASELINE_DIGEST" "${EVIDENCE_DIR}/install-baseline.txt" || return
  STRANDED_WINDOW=""
  # No systemctl start or enable of the harness's own around either
  # install here: the release installer enables and starts both units
  # itself, and bringing them up here would hide an installer that no
  # longer does. Stopping them is another matter -- clear_install above
  # does, and so does the rollback -- and the pair is always stopped
  # together, never one alone: the baseline's observability unit shares
  # the agent's RuntimeDirectory, so stopping one of them alone strands
  # the other. The candidate's unit no longer shares it; stopping both
  # together is right whichever set is installed.
  step "baseline services ready" await_services_ready || return
  step "baseline versions" check_installed_versions \
    "${EVIDENCE_DIR}/packages-baseline.txt" "$BASELINE_DIR" "$BASELINE_TAG" || return
  record_baseline_doctor "${EVIDENCE_DIR}/doctor-baseline.json" || return

  # The baseline starts with no deployment -- its state directory was
  # cleared with the rest of the install -- so the deployment the upgrade
  # has to carry across is one the baseline itself created and served.
  deploy_bundle "${EVIDENCE_DIR}/upgrade-baseline-deploy.json" "$BASELINE_DEPLOYMENT_ID" || return
  step "baseline worker round trip" check_deployment_round_trip "$BASELINE_DEPLOYMENT_ID" \
    "${EVIDENCE_DIR}/status-baseline.json" "${EVIDENCE_DIR}/upgrade-baseline-result.json" \
    "${EVIDENCE_DIR}/upgrade-baseline-deploy.json" || return

  # A trailing newline keeps the file valid JSON and its mode unchanged,
  # and makes it differ from the packaged conffile, which is what puts
  # dpkg's conffile handling under --force-confold on the path. Nothing
  # in this run reads it: every CLI call is pinned to a private config.
  #
  # What this establishes is that neither installer replaces an edited
  # conffile. It is not a test of dpkg's conflict prompt: the packaged
  # bytes of this file are the same in both sets, which is the case dpkg
  # resolves by keeping the operator's copy without asking.
  note "editing ${OPERATOR_CONFIG} as an operator would"
  step "operator edit" sudo bash -c 'printf "\n" >>"$1"' _ "$OPERATOR_CONFIG" || return
  OPERATOR_CONFIG_SHA256="$(operator_config_sha256)" || return

  before="$(unit_pids)" || return
  note "upgrading to ${CANDIDATE_TAG} over the running baseline"
  install_set "$ASSETS_DIR" "$ARTIFACT_DIGEST" "${EVIDENCE_DIR}/install-upgrade.txt" || return
  step "services ready after the upgrade" await_services_ready || return
  step "candidate versions" check_installed_versions \
    "${EVIDENCE_DIR}/packages-after-upgrade.txt" "$ASSETS_DIR" candidate || return
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

  step "doctor after the upgrade" capture_cli_json "${EVIDENCE_DIR}/doctor-after-upgrade.json" \
    doctor --output json || doctor_status=$?
  check_doctor_green "${EVIDENCE_DIR}/doctor-after-upgrade.json" || return
  ((doctor_status == 0)) || return "$doctor_status"

  # No deploy: the candidate has to have re-warmed the deployment the
  # baseline wrote to durable state, and to serve it.
  step "surviving deployment round trip" check_deployment_round_trip "$BASELINE_DEPLOYMENT_ID" \
    "${EVIDENCE_DIR}/status-after-upgrade.json" "${EVIDENCE_DIR}/upgrade-result.json" || return
  pass "upgraded in place with new pids; operator edit kept; doctor green and ${ROW} resolved; the baseline's deployment answered on the candidate"
}

# Removal leaves each conffile-owning package holding only its conffiles,
# never purged. The dpkg status is what distinguishes the two: the
# packaged conffile bytes are identical in both sets, so comparing the
# files could not. A package the listing does not name at all was purged,
# not removed, and its conffiles are gone -- so the check is against the
# packages that must be there, not against the rows that happen to
# appear. A purge that swallowed tensorplate-observability's
# /etc/tensorplate/observability.json is exactly the operator-config loss
# this stage exists to rule out, and it shows up as an absence.
#
# Every other tensorplate* package must be out of dpkg's installed set,
# which means not-installed or config-files: half-installed, unpacked and
# half-configured are packages a failed removal left behind as surely as
# installed is.
#
# The apt channel's bootstrap package must be exactly as the removal found
# it, which the caller read from the same unfiltered listing. The runbook's
# device never had it, and a lab image that reaches the channel through it
# has it installed; the removal must change neither, and reading the
# words the harness passed to apt-get would not show that it did not.
#
# Records whether the conffiles were kept, for report_stranded_device.
check_removed() {
  local apt_source_before="$1" listing verdict status=0
  STRANDED_CONFFILES=unread
  listing="$(installed_tensorplate_packages all)" || return
  printf '%s\n' "$listing" >"${EVIDENCE_DIR}/packages-after-remove.txt" || return
  verdict="$(removal_verdict "${EVIDENCE_DIR}/packages-after-remove.txt" "$apt_source_before")" ||
    status=$?
  # Whatever the verdict, it was read: a failed check still says which
  # conffiles are gone.
  if [[ "$verdict" == kept || "$verdict" == "lost "* ]]; then
    STRANDED_CONFFILES="$verdict"
  fi
  ((status == 0)) || return "$status"
  printf 'every removed package kept its conffiles, none is left installed, and %s is still %s\n' \
    "$APT_SOURCE_PACKAGE" "$apt_source_before"
}

# check_removed's judgement of one listing. Prints `kept` or `lost <pkgs>`
# for the conffiles, and fails with every problem it found.
removal_verdict() {
  python3 - "$1" "$APT_SOURCE_PACKAGE" "$2" "${CONFFILE_PACKAGES[@]}" <<'PY'
import sys

path, apt_source, apt_source_before = sys.argv[1:4]
expected = sys.argv[4:]
status = {}
for line in open(path, encoding="utf-8"):
    fields = line.split()
    if len(fields) == 2:
        status[fields[0]] = fields[1]

REMOVED = ("not-installed", "config-files")
problems = []
lost = []
for package in expected:
    found = status.get(package, "absent")
    if found == "config-files":
        continue
    if found in ("absent", "not-installed"):
        lost.append(package)
        problems.append(f"{package} is {found}, not config-files: its conffiles were not kept")
    else:
        problems.append(f"{package} is still {found}")
# tensorplate-common ships nothing under /etc, so dpkg may report it
# either config-files or not-installed; what it may not be is anything
# else, which would make the baseline install a downgrade apt-get -y
# refuses.
for package in sorted(status):
    if package in expected or package == apt_source:
        continue
    if status[package] not in REMOVED:
        problems.append(f"{package} is still {status[package]}")
apt_source_after = status.get(apt_source, "absent")
if apt_source_after != apt_source_before:
    problems.append(
        f"{apt_source} was {apt_source_before} before the removal and is "
        f"{apt_source_after} after it: the removal must leave the apt channel's "
        "bootstrap package as it found it"
    )
# The one line the caller keeps: whether /etc/tensorplate still holds
# every conffile, whatever else went wrong.
print("lost " + " ".join(lost) if lost else "kept")
if problems:
    sys.stderr.write("the removal did not leave only conffiles: " + "; ".join(problems) + "\n")
    raise SystemExit(1)
PY
}

# What report_stranded_device says, as text on stdout.
#
# It says what dpkg reports when it runs, not what an earlier listing
# said or what a command was asked to do. A removal that left a package
# behind is not a device with nothing installed, and the recovery command
# is then the very downgrade `apt-get -y` refuses without
# --allow-downgrades; an installer that failed after installing is not a
# device with nothing installed either. Nothing is reinstalled
# automatically: the operator decides which set this device should carry,
# and a harness that quietly reinstalled would hide that it had left the
# device without a working install.
stranded_device_text() {
  local listing="" read_ok=1 installed="" pkg status query saved_state=""
  query='dpkg-query -W -f='"'"'${binary:Package} ${db:Status-Status}\n'"'"' '"'"'tensorplate*'"'"
  listing="$(installed_tensorplate_packages all 2>/dev/null)" || read_ok=0
  if ((read_ok)); then
    while read -r pkg status; do
      if [[ -n "$pkg" && "$pkg" != "$APT_SOURCE_PACKAGE" &&
            "$status" != not-installed && "$status" != config-files ]]; then
        installed="${installed:+${installed}, }${pkg} ${status}"
      fi
    done <<<"$listing"
  fi

  case "$STRANDED_WINDOW" in
    upgrade)
      printf 'the upgrade did not finish: it had started clearing the candidate and had not completed the %s baseline install.\n' \
        "$BASELINE_TAG"
      ;;
    rollback-stopped)
      printf 'the rollback did not finish: it had started stopping the candidate'"'"'s services and had not removed any package.\n'
      ;;
    rollback)
      printf 'the rollback did not finish: it had started removing the candidate and had not completed the %s baseline install.\n' \
        "$BASELINE_TAG"
      ;;
    *) ;;
  esac

  if ((!read_ok)); then
    printf 'what this device carries could NOT be read. List the packages with:\n  %s\n' "$query"
  elif [[ -z "$installed" ]]; then
    printf 'this device has NO TensorPlate installed: dpkg lists no tensorplate* package as installed, %s aside.\n' \
      "$APT_SOURCE_PACKAGE"
  else
    printf 'dpkg lists these TensorPlate packages as present: %s.\n' "$installed"
  fi

  if [[ -n "$STRANDED_INSTALL_LOG" ]]; then
    printf 'the %s installer was started and its install was not accepted; its output is in %s. It can fail after installing every package, so what it left is what dpkg lists above.\n' \
      "$BASELINE_TAG" "$STRANDED_INSTALL_LOG"
  else
    printf 'the %s installer was not run.\n' "$BASELINE_TAG"
  fi

  case "$STRANDED_WINDOW" in
    upgrade)
      if ((STRANDED_STATE_CLEARED)); then
        printf 'the clearing step deleted %s with the packages, durable state included.\n' \
          "${CLEARED_STATE_DIRS[*]}"
      else
        printf 'the clearing step stopped before deleting %s.\n' "${CLEARED_STATE_DIRS[*]}"
      fi
      ;;
    rollback-stopped)
      printf 'no package was removed, so /etc/tensorplate is as the candidate left it.\n'
      ;;
    rollback)
      case "$STRANDED_CONFFILES" in
        kept) printf '/etc/tensorplate conffiles are kept.\n' ;;
        "lost "*)
          printf 'the removal did NOT keep the /etc/tensorplate conffiles of: %s.\n' \
            "${STRANDED_CONFFILES#lost }"
          ;;
        *) printf 'whether the removal kept the /etc/tensorplate conffiles was NOT read.\n' ;;
      esac
      ;;
    *) ;;
  esac
  if ((STRANDED_STATE_ASIDE)); then
    # The pathname alone is what an operator would recover from, and this
    # report is printed in exactly the window where a baseline install
    # that failed may already have destroyed what is behind it -- the
    # stage's own preservation check runs only after that install
    # returns 0. The manifest taken before the move is what can tell them
    # which they have, so it is read back here rather than left unused.
    #
    # Best-effort, and nothing below may fail: this runs from the EXIT
    # trap, which still has the report to write.
    saved_state="$(state_manifest "$STATE_ASIDE_DIR" 2>/dev/null || true)"
    if [[ -z "$STATE_MANIFEST" ]]; then
      printf 'durable state is at %s.\n' "$STATE_ASIDE_DIR"
    elif [[ -z "$saved_state" ]]; then
      printf 'durable state is at %s, but it could not be read back; whether it still matches the digests taken before the move is NOT known.\n' \
        "$STATE_ASIDE_DIR"
    elif [[ "$saved_state" == "$STATE_MANIFEST" ]]; then
      printf 'durable state is at %s and still matches the digests taken before the move.\n' \
        "$STATE_ASIDE_DIR"
    else
      printf 'durable state is at %s but NO LONGER matches the digests taken before the move; treat it as damaged.\n' \
        "$STATE_ASIDE_DIR"
    fi
  elif [[ "$STRANDED_WINDOW" != upgrade ]]; then
    printf 'durable state is still at %s.\n' "$STATE_DIR"
  fi

  if [[ "$STRANDED_WINDOW" == rollback-stopped ]]; then
    printf 'if the candidate is still installed, return to it with:\n'
    if ((STRANDED_STATE_ASIDE)); then
      printf '  sudo mv -T %s %s\n' "$STATE_ASIDE_DIR" "$STATE_DIR"
    fi
    printf '  sudo systemctl start %s %s\n' "$AGENT_UNIT" "$OBSERVABILITY_UNIT"
  elif ((!read_ok)); then
    printf 'once that listing shows no newer TensorPlate package installed, install %s by hand with:\n' \
      "$BASELINE_TAG"
    printf '  sudo bash %s/install.sh --local-artifacts %s --yes\n' "$BASELINE_DIR" "$BASELINE_DIR"
  elif [[ -n "$installed" && -z "$STRANDED_INSTALL_LOG" ]]; then
    # Nothing the baseline installer did, so these are the candidate's.
    printf 'while a newer package is installed, installing %s is a downgrade apt-get refuses. Remove those packages first, then install it by hand with:\n' \
      "$BASELINE_TAG"
    printf '  sudo bash %s/install.sh --local-artifacts %s --yes\n' "$BASELINE_DIR" "$BASELINE_DIR"
  else
    printf 'install %s by hand with:\n  sudo bash %s/install.sh --local-artifacts %s --yes\n' \
      "$BASELINE_TAG" "$BASELINE_DIR" "$BASELINE_DIR"
  fi
  printf 'or re-run this harness, which installs the candidate from scratch -- but its install stage deletes %s first, including any %s. Copy anything you want to keep elsewhere before re-running.\n' \
    "${CLEARED_STATE_DIRS[*]}" "$STATE_ASIDE_DIR"
}

# Printed when the run ends inside one of the windows above, including
# from a signal, and filed beside the stage logs first, so the recovery
# instructions survive a terminal that is gone -- which is how an SSH
# session that drops mid-run ends it. Never fails: the EXIT handler that
# calls it still has the report to write.
report_stranded_device() {
  [[ -n "$STRANDED_WINDOW" ]] || return 0
  local report="${EVIDENCE_DIR}/stranded-device.txt"
  if stranded_device_text >"$report"; then
    cat "$report" >&2 || true
  else
    stranded_device_text >&2 || true
  fi
  return 0
}

stage_rollback() {
  local remove=() listing pkg status apt_source_before=absent
  # What the upgrade left: the candidate serving the deployment the
  # baseline created. Read before anything is stopped, so a device that
  # is not in that state is refused rather than half rolled back.
  step "status before the rollback" capture_cli_json \
    "${EVIDENCE_DIR}/status-before-rollback.json" status --output json || return
  python3 - "${EVIDENCE_DIR}/status-before-rollback.json" "$BASELINE_DEPLOYMENT_ID" <<'PY' || return
import json, sys

path, expected = sys.argv[1:]
active = ((json.load(open(path, encoding="utf-8"))["payload"].get("agent") or {}).get("active")) or {}
if active.get("deployment_id") != expected:
    raise SystemExit(
        f"the rollback must start from {expected}, which the upgrade left active; "
        f"status reports {active.get('deployment_id')}"
    )
print(f"rolling back from the candidate serving {expected}")
PY

  # Checked before anything changes. A state.bak from before the run is
  # already gone -- install and upgrade delete /var/lib/tensorplate while
  # clearing the device -- so this does not keep an earlier copy. It
  # makes sure the move below lands on a name nothing else holds, refused
  # by name before the services are stopped, rather than leaving mv -T to
  # replace an empty directory or refuse a full one halfway through.
  step "refuse to replace an existing ${STATE_ASIDE_DIR}" sudo test ! -e "$STATE_ASIDE_DIR" || return

  note "rolling back to ${BASELINE_TAG} by the documented procedure"
  # From the stop on, the candidate serves nothing until the rollback
  # completes, so an interrupted run says so. The upgrade's installs have
  # already set STRANDED_INSTALL_LOG; nothing in this window has started
  # the baseline installer yet.
  STRANDED_WINDOW=rollback-stopped
  STRANDED_INSTALL_LOG=""
  step "stop the services" sudo systemctl stop "$AGENT_UNIT" "$OBSERVABILITY_UNIT" || return
  # Taken with the agent stopped, so it is the state the rollback has to
  # carry across, and before anything moves or removes it -- after the
  # move there is no original left to compare the saved copy with.
  step "digest the durable state before setting it aside" capture_state_manifest || return
  step "set durable state aside" sudo mv -T "$STATE_DIR" "$STATE_ASIDE_DIR" || return
  STRANDED_STATE_ASIDE=1

  # Every installed TensorPlate package, not a fixed list, and including
  # tensorplate-common: every runtime package Pre-Depends on common at
  # exactly its own version, so a common left at the candidate's version
  # turns the baseline install into a downgrade, which the apt-get -y
  # inside install.sh refuses without --allow-downgrades. The apt
  # channel's bootstrap package is excluded, as everywhere else in this
  # run, and a package already reduced to its conffiles, or that dpkg
  # knows but never installed, has nothing left to remove. The listing is
  # the unfiltered one, so it also says what the bootstrap package was
  # before the removal, which check_removed holds the removal to.
  listing="$(installed_tensorplate_packages all)" || return
  printf '%s\n' "$listing" >"${EVIDENCE_DIR}/packages-before-remove.txt" || return
  while read -r pkg status; do
    if [[ "$pkg" == "$APT_SOURCE_PACKAGE" ]]; then
      apt_source_before="$status"
    elif [[ -n "$pkg" && "$status" != not-installed && "$status" != config-files ]]; then
      remove+=("$pkg")
    fi
  done <<<"$listing"
  # Entered before the command runs: a removal that fails part way
  # through leaves the device in this window just as a completed one
  # does.
  STRANDED_WINDOW=rollback
  if ((${#remove[@]} > 0)); then
    step "remove ${remove[*]}" \
      sudo DEBIAN_FRONTEND=noninteractive apt-get remove -y "${remove[@]}" || return
  fi
  check_removed "$apt_source_before" || return

  note "installing the baseline fresh through its own installer"
  install_set "$BASELINE_DIR" "$BASELINE_DIGEST" "${EVIDENCE_DIR}/install-rollback.txt" || return
  STRANDED_WINDOW=""
  step "services ready after the rollback" await_services_ready || return
  step "baseline versions" check_installed_versions \
    "${EVIDENCE_DIR}/packages-after-rollback.txt" "$BASELINE_DIR" "$BASELINE_TAG" || return
  check_operator_config_kept "the rollback" || return
  step "the set-aside state is preserved, file by file" check_state_preserved || return
  record_baseline_doctor "${EVIDENCE_DIR}/doctor-after-rollback.json" || return

  # The older agent must not have loaded the newer agent's state: it
  # answers, and has nothing active or previous. This is the documented
  # procedure's outcome, not a defect -- the state was set aside on
  # purpose, and what the operator restores from it is their decision.
  step "status after the rollback" capture_cli_json \
    "${EVIDENCE_DIR}/status-after-rollback.json" status --output json || return
  python3 - "${EVIDENCE_DIR}/status-after-rollback.json" <<'PY' || return
import json, sys

agent = json.load(open(sys.argv[1], encoding="utf-8"))["payload"].get("agent") or {}
if agent.get("available") is not True:
    raise SystemExit(f"the agent is not available after the rollback: {agent}")
for key in ("active", "previous_active"):
    if key not in agent:
        raise SystemExit(f"status after the rollback does not report {key}")
    if agent[key] is not None:
        raise SystemExit(
            f"the rolled-back agent reports {key} {agent[key]}; it loaded state that was set aside"
        )
print("the rolled-back agent answers with no active or previous deployment")
PY

  deploy_bundle "${EVIDENCE_DIR}/rollback-deploy.json" "$ROLLBACK_DEPLOYMENT_ID" || return
  step "rolled-back worker round trip" check_deployment_round_trip "$ROLLBACK_DEPLOYMENT_ID" \
    "${EVIDENCE_DIR}/status-after-redeploy.json" "${EVIDENCE_DIR}/rollback-result.json" \
    "${EVIDENCE_DIR}/rollback-deploy.json" || return
  pass "rolled back to ${BASELINE_TAG}; operator edit kept; state set aside unchanged and not loaded; a fresh deployment answered"
}

# --- run ---------------------------------------------------------------

main() {
  parse_args "$@"
  BASELINE_DEPLOYMENT_ID="${DEPLOYMENT_ID}-baseline"
  ROLLBACK_DEPLOYMENT_ID="${DEPLOYMENT_ID}-rollback"
  preflight
  prepare_bundle
  if ((PREFLIGHT_ONLY)); then
    # Nothing has been installed, removed or written outside the scratch
    # bundle, which the EXIT trap removes, so a preflight run leaves the
    # host as it found it and files no evidence for a run that did not
    # happen.
    pass "preflight only; bundle built, no install attempted and no report written"
    exit 0
  fi
  mkdir -p "$EVIDENCE_DIR"
  EVIDENCE_DIR="$(cd "$EVIDENCE_DIR" && pwd)"
  ASSETS_DIR="$(cd "$ASSETS_DIR" && pwd)"
  BUNDLE_DIR="$(cd "$BUNDLE_DIR" && pwd)"
  if ((BASELINE_REQUESTED)); then
    BASELINE_DIR="$(cd "$BASELINE_DIR" && pwd)"
  fi

  printf '%s\n' "$CHECKSUM_OUTPUT" >"${EVIDENCE_DIR}/checksums.txt"
  if ((BASELINE_REQUESTED)); then
    printf '%s\n' "$BASELINE_CHECKSUM_OUTPUT" >"${EVIDENCE_DIR}/baseline-checksums.txt"
    # The report attests the candidate, so the baseline's digest is filed
    # here rather than through lifecycle_artifact_digest, which takes the
    # one value a run is about.
    printf '%s  SHA256SUMS\n' "$BASELINE_DIGEST" >"${EVIDENCE_DIR}/baseline-digest.txt"
  fi
  record_host_facts || die "could not record host facts"

  # shellcheck source=tools/validation/lifecycle-stages.sh disable=SC1091
  source "${REPO_ROOT}/tools/validation/lifecycle-stages.sh"
  lifecycle_begin "$ROW" "$EVIDENCE_DIR" "$TESTED_VERSION" jetson-lifecycle
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
  # Offline belongs here, between crash-loop and upgrade: it is about the
  # candidate install every stage above exercises, and the stages below
  # replace that install with the baseline. Keep it in this position when
  # it lands.
  lifecycle_skip offline \
    "not implemented yet: offline under per-unit network denial allowing only 127.0.0.1/32 and ::1/128, with a probe proving enforcement and every CLI call run under denial, is follow-up harness work"

  # After crash-loop, so every stage above is about a clean candidate
  # install and no crash-loop restore can still be pending once packages
  # start being removed. A failure here exits with the stages above
  # already recorded.
  if ((BASELINE_REQUESTED)); then
    lifecycle_stage upgrade stage_upgrade
    lifecycle_stage rollback stage_rollback
  else
    lifecycle_skip upgrade \
      "no baseline artifact set was supplied with --baseline-tag and --baseline-assets-dir; upgrade needs the published predecessor runtime set to move from"
    lifecycle_skip rollback \
      "no baseline artifact set was supplied with --baseline-tag and --baseline-assets-dir; rollback needs the published predecessor runtime set to return to"
  fi

  lifecycle_finish
  printf 'evidence: %s\n' "$EVIDENCE_DIR"
  if ((BASELINE_REQUESTED)); then
    pass "lifecycle run complete; seven stages exercised, offline skipped with its reason; ${BASELINE_TAG} is left installed"
  else
    pass "lifecycle run complete; five stages exercised, three skipped with reasons"
  fi
}

main "$@"
