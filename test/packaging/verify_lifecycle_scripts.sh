#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# V01-E14-F08: lifecycle script verifier.
#
# Exercises version-utils.sh and a subset of upgrade-preflight.sh
# logic. Does not require root or dpkg.

set -eu

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"

. "${repo_root}/packaging/scripts/version-utils.sh"

fail=0
expect_lt() {
  if tensorplate_version_lt "$1" "$2"; then
    return 0
  fi
  echo "FAIL: expected tensorplate_version_lt('$1', '$2') = true" >&2
  fail=1
}
expect_not_lt() {
  if tensorplate_version_lt "$1" "$2"; then
    echo "FAIL: expected tensorplate_version_lt('$1', '$2') = false" >&2
    fail=1
  fi
}

expect_lt 0.1.0 0.2.0
expect_lt 0.1.0 0.1.1
# Debian package versions use `~` for pre-release ordering. Do not use
# `-dev0` here: dpkg treats `-` as the Debian revision separator.
expect_lt 0.1.0~dev0 0.1.0
expect_not_lt 0.2.0 0.1.0
expect_not_lt 0.1.0 0.1.0
expect_not_lt 1.0.0 0.99.99

# Preflight schema check against the shipped configs.
preflight="${repo_root}/packaging/scripts/upgrade-preflight.sh"
if [ ! -x "${preflight}" ]; then
  echo "FAIL: upgrade-preflight.sh not executable" >&2
  fail=1
fi

# Smoke: invoke preflight in a tempdir with a stub install whose
# configs declare the supported schema; it must exit 0.
td="$(mktemp -d)"
cleanup() { rm -rf "${td}"; }
trap cleanup EXIT
# Synthesize a packaging/scripts dir pointing at the tempdir's etc.
mkdir -p "${td}/etc/tensorplate" "${td}/var/lib/tensorplate" "${td}/scripts"
for c in agent observability serving_worker cli; do
  cp "${repo_root}/packaging/conf/${c}.json" "${td}/etc/tensorplate/"
done
# Stage a parallel scripts dir so the preflight's `here` discovers our
# rewritten path-constants alongside the real version-utils. The real
# preflight sources both via "${here}/<name>.sh".
sed "s|/etc/tensorplate|${td}/etc/tensorplate|g; s|/var/lib/tensorplate|${td}/var/lib/tensorplate|g" \
  "${repo_root}/packaging/scripts/path-constants.sh" >"${td}/scripts/path-constants.sh"
cp "${repo_root}/packaging/scripts/version-utils.sh" "${td}/scripts/version-utils.sh"
cp "${preflight}" "${td}/scripts/upgrade-preflight.sh"
chmod +x "${td}/scripts/upgrade-preflight.sh"
if ! "${td}/scripts/upgrade-preflight.sh" "" ""; then
  echo "FAIL: preflight rejected a clean install staging" >&2
  fail=1
fi

# Corrupt one config and expect rejection.
sed -i.bak 's/"schema_version": "0.1"/"schema_version": "9.9"/' "${td}/etc/tensorplate/agent.json"
if "${td}/scripts/upgrade-preflight.sh" "" "" 2>/dev/null; then
  echo "FAIL: preflight accepted an unsupported schema_version" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_lifecycle_scripts: ok"
fi
exit "${fail}"
