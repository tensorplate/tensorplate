#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Stage a package version into the head of packaging/debian/changelog.
#
# dpkg takes every package's version from that head, and the tree carries
# the final release's. Packages built by separate jobs depend on each other
# at `(= ${binary:Version})`, so every job that builds a candidate stages
# the same version first. The edit is to the working tree and is never
# committed.

set -Eeuo pipefail

usage() {
  cat <<'EOF'
Usage:
  stage-debian-changelog.sh DEB_VERSION [DISTRIBUTION]

  DEB_VERSION   Package version without the Debian revision: X.Y.Z,
                X.Y.Z~rc.N or X.Y.Z~dev.YYYYMMDD.gitsha. X.Y.Z must be the
                version in packaging/VERSION.
  DISTRIBUTION  Changelog distribution. Defaults to unstable.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

case "${1:-}" in
  --help|-h) usage; exit 0 ;;
esac
[[ $# -ge 1 && $# -le 2 ]] || { usage >&2; exit 2; }

deb_version="$1"
distribution="${2:-unstable}"

[[ "$deb_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(~rc\.[1-9][0-9]*|~dev\.[0-9]{8}\.[0-9a-f]+)?$ ]] ||
  die "DEB_VERSION must be X.Y.Z, X.Y.Z~rc.N or X.Y.Z~dev.YYYYMMDD.gitsha; got '${deb_version}'"
[[ "$distribution" =~ ^[A-Za-z][A-Za-z0-9-]*$ ]] ||
  die "DISTRIBUTION must be a changelog distribution name; got '${distribution}'"

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
changelog="${repo_root}/packaging/debian/changelog"
[[ -f "$changelog" ]] || die "missing ${changelog}"

source_version="$("${repo_root}/packaging/version.sh")"
[[ "${deb_version%%~*}" == "${source_version%%~*}" ]] ||
  die "${deb_version} is not a version of this tree, which is at ${source_version%%~*} (packaging/VERSION)"

IFS= read -r head <"$changelog" || [[ -n "$head" ]] || die "${changelog} is empty"
[[ "$head" =~ ^tensorplate\ \([^\)]+\)\ [^\;]+\;\ urgency=[a-z]+$ ]] ||
  die "${changelog} does not begin with a tensorplate changelog entry: '${head}'"

staged="tensorplate (${deb_version}-1) ${distribution}; urgency=medium"
rest="$(mktemp)"
trap 'rm -f -- "$rest"' EXIT
tail -n +2 -- "$changelog" >"$rest"
{ printf '%s\n' "$staged"; cat -- "$rest"; } >"$changelog"
printf "staged debian/changelog: '%s' -> '%s'\n" "$head" "$staged"
