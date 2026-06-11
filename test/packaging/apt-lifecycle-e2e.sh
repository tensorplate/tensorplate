#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# End-to-end APT channel lifecycle rehearsal:
#
#   1. build the previous release's packages from its tag and install
#      them as the baseline (the install.sh-era host),
#   2. prove the stock-state failures (no channel knowledge,
#      tensorplate-ready-check fails closed),
#   3. build the current tree's packages, publish a signed local
#      repository with an ephemeral key,
#   4. bootstrap with tensorplate-apt-source and run the public
#      two-command install, which must upgrade the baseline in place
#      and preserve /etc/tensorplate and state,
#   5. publish a higher staging version onto the same channel and
#      discover it with plain `apt update`.
#
# THIS SCRIPT MUTATES THE HOST (installs system packages with dpkg/apt).
# Run it only on a disposable host: the CI runner
# (.github/workflows/apt-lifecycle.yml) or a container. It refuses to
# run unless CI=true or TP_APT_LIFECYCLE_ALLOW=1.
#
# Runtime binaries are stubbed: this rehearses packaging, channel, and
# upgrade behavior, not runtime execution (that is Jetson T4 territory).

set -Eeuo pipefail

BASELINE_TAG="${TP_APT_LIFECYCLE_BASELINE_TAG:-v0.1.1}"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '==> %s\n' "$*"; }
pass() { printf 'PASS: %s\n' "$*"; }

[[ "${CI:-}" == "true" || "${TP_APT_LIFECYCLE_ALLOW:-0}" == "1" ]] ||
  die "this rehearsal installs system packages; run on a disposable host with TP_APT_LIFECYCLE_ALLOW=1"
[[ "$(id -u)" -eq 0 ]] || die "run as root (dpkg/apt operations)"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git repository"
cd "$repo_root"
git rev-parse -q --verify "refs/tags/${BASELINE_TAG}" >/dev/null ||
  die "baseline tag ${BASELINE_TAG} is not available; fetch tags first"

for tool in dpkg-buildpackage debhelper_placeholder gpg apt-ftparchive dpkg-scanpackages; do
  case "$tool" in debhelper_placeholder) command -v dh >/dev/null 2>&1 || die "debhelper is required" ;;
    *) command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool" ;;
  esac
done

work="$(mktemp -d)"
cleanup() { git worktree remove --force "${work}/baseline" >/dev/null 2>&1 || true; rm -rf "$work"; }
trap cleanup EXIT

stub_binaries() {
  local tree="$1" marker="$2" stub
  for stub in target/release/tensorplate target/release/tensorplate-agent \
              target/release/tensorplate-observability build/release/tensorplate-serving; do
    mkdir -p "${tree}/$(dirname "$stub")"
    printf '#!/bin/sh\necho %s\n' "$marker" > "${tree}/${stub}"
    chmod +x "${tree}/${stub}"
  done
}

note "A. build baseline packages from ${BASELINE_TAG}"
git worktree add --detach "${work}/baseline" "$BASELINE_TAG" >/dev/null
stub_binaries "${work}/baseline" "stub-baseline"
(cd "${work}/baseline" && packaging/scripts/build-deb.sh >"${work}/build-baseline.log" 2>&1) ||
  { tail -5 "${work}/build-baseline.log"; die "baseline package build failed"; }
baseline_ver="$(dpkg-parsechangelog -l "${work}/baseline/packaging/debian/changelog" -S Version)"

note "B. install the baseline runtime set (install.sh-era host)"
dpkg -i "${work}/tensorplate-common_${baseline_ver}_all.deb" >/dev/null
dpkg -i "${work}"/tensorplate-{agent,serving,observability,cli}_"${baseline_ver}"_*.deb >/dev/null
echo "custom-operator-setting" > /etc/tensorplate/marker.conf
mkdir -p /var/lib/tensorplate/state && echo "desired-state" > /var/lib/tensorplate/state/marker
pass "baseline ${baseline_ver} installed"

note "C. stock-state negatives"
if apt-get install -y tensorplate >"${work}/stock.log" 2>&1; then
  die "stock host must not resolve tensorplate"
fi
grep -q "Unable to locate package tensorplate" "${work}/stock.log" ||
  die "stock failure did not match the documented error"
pass "stock host fails with the documented error"
if tools/validation/tensorplate-ready-check.sh >"${work}/rc-stock.log" 2>&1; then
  die "tensorplate-ready-check must fail on a stock host"
fi
pass "tensorplate-ready-check fails closed on a stock host"

note "D. build current packages and publish a signed local channel"
stub_binaries "$repo_root" "stub-current"
packaging/scripts/build-deb.sh >"${work}/build-current.log" 2>&1 ||
  { tail -5 "${work}/build-current.log"; die "current package build failed"; }
cur_ver="$(dpkg-parsechangelog -l packaging/debian/changelog -S Version)"
repo_parent="$(dirname "$repo_root")"
mkdir -p "${work}/assets"
cp "${repo_parent}"/tensorplate*_"${cur_ver}"_*.deb "${work}/assets/"
(cd "${work}/assets" && sha256sum -- *.deb > SHA256SUMS)
export GNUPGHOME="${work}/gnupg"
mkdir -p "$GNUPGHOME" && chmod 700 "$GNUPGHOME"
gpg --batch --quiet --pinentry-mode loopback --passphrase '' --quick-generate-key \
  "TensorPlate Lifecycle Test <t@tensorplate.com>" ed25519 sign never 2>/dev/null
gpg --batch --quiet --armor --export t@tensorplate.com > "${work}/pub.asc"
gpg --batch --quiet --pinentry-mode loopback --passphrase '' --armor --export-secret-keys \
  t@tensorplate.com > "${work}/priv.asc"
tools/release/publish-apt-repo.sh \
  --assets-dir "${work}/assets" --output /srv/tensorplate-apt \
  --signing-key "${work}/priv.asc" --verify-keyring "${work}/pub.asc" \
  --allow-unverified-assets >/dev/null

note "E. one-time bootstrap, then the public two-command install (in-place upgrade)"
dpkg -i "${work}/assets/tensorplate-apt-source_${cur_ver}_all.deb" >/dev/null
# Test plumbing only: point the shipped source at the local signed
# repository and pair the keyring with the ephemeral signing key.
gpg --batch --yes --dearmor --output /usr/share/keyrings/tensorplate-archive-keyring.gpg "${work}/pub.asc"
sed -i "s|^URIs: .*|URIs: file:/srv/tensorplate-apt|" /etc/apt/sources.list.d/tensorplate.sources
TP_READY_EXPECTED_URI=file:/srv/tensorplate-apt \
  tools/validation/tensorplate-ready-check.sh --online >"${work}/rc-ready.log" 2>&1 ||
  { cat "${work}/rc-ready.log" >&2; die "tensorplate-ready-check --online failed after bootstrap"; }
pass "tensorplate-ready-check --online green after bootstrap"
apt-get update -qq
apt-get install -y -qq tensorplate >"${work}/upgrade.log" 2>&1 ||
  { tail -15 "${work}/upgrade.log" >&2; die "two-command install failed"; }
stale="$(dpkg-query -W -f '${binary:Package} ${Version}\n' 'tensorplate-*' | grep -F "$baseline_ver" || true)"
[[ -z "$stale" ]] || die "packages left on the baseline version: $stale"
dpkg -s tensorplate >/dev/null 2>&1 || die "tensorplate metapackage not installed"
[[ "$(cat /etc/tensorplate/marker.conf)" == "custom-operator-setting" ]] ||
  die "/etc/tensorplate was not preserved across the upgrade"
[[ "$(cat /var/lib/tensorplate/state/marker)" == "desired-state" ]] ||
  die "state was not preserved across the upgrade"
pass "two-command install upgraded ${baseline_ver} -> ${cur_ver} in place; config and state preserved"

note "F. future-version discovery without re-bootstrap"
# Build the FULL staging set (stubs are still staged from step D): the
# discovery claim in docs/install/tensorplate-ready.md is about the
# runtime metapackage, which is Architecture: arm64 and therefore not
# covered by an arch-independent-only build.
base="${cur_ver%%~*}"; base="${base%-*}"
IFS=. read -r major minor patch <<<"$base"
staging_ver="${major}.${minor}.$((patch + 1))~staging-1"
sed -i "1s/${cur_ver}/${staging_ver}/" packaging/debian/changelog
packaging/scripts/build-deb.sh >"${work}/build-staging.log" 2>&1 ||
  { tail -5 "${work}/build-staging.log"; die "staging package build failed"; }
git checkout -- packaging/debian/changelog
mkdir -p "${work}/assets-staging"
cp "${repo_parent}"/tensorplate*_"${staging_ver}"_*.deb "${work}/assets-staging/"
(cd "${work}/assets-staging" && sha256sum -- *.deb > SHA256SUMS)
tools/release/publish-apt-repo.sh \
  --assets-dir "${work}/assets-staging" --output "${work}/apt-next" \
  --existing-pool /srv/tensorplate-apt/pool \
  --signing-key "${work}/priv.asc" --verify-keyring "${work}/pub.asc" \
  --allow-unverified-assets >/dev/null
cp -a "${work}/apt-next/pool/." /srv/tensorplate-apt/pool/
cp -a "${work}/apt-next/dists/." /srv/tensorplate-apt/dists/
apt-get update -qq
candidate="$(apt-cache policy tensorplate | awk '/Candidate:/{print $2}')"
[[ "$candidate" == "$staging_ver" ]] ||
  die "future tensorplate runtime version not discovered; candidate=$candidate expected=$staging_ver"
installed="$(apt-cache policy tensorplate | awk '/Installed:/{print $2}')"
[[ "$installed" == "$cur_ver" ]] ||
  die "installed runtime version changed without an upgrade command; installed=$installed"
pass "future runtime version ${staging_ver} discovered by plain apt update (installed stays ${cur_ver}); bootstrap untouched"

note "apt lifecycle rehearsal complete"
