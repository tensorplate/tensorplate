#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# V01-E14-F08: debhelper metadata linter.

set -eu

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
debian="${repo_root}/packaging/debian"

fail=0

# Expected binary packages.
PACKAGES="tensorplate-common tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli tensorplate-backend-python-pytorch"

for pkg in ${PACKAGES}; do
  if ! grep -q "^Package: ${pkg}\$" "${debian}/control"; then
    echo "FAIL: ${pkg} missing Package: stanza in debian/control" >&2
    fail=1
    continue
  fi
  if [ ! -f "${debian}/${pkg}.install" ]; then
    echo "FAIL: ${pkg} missing ${pkg}.install" >&2
    fail=1
  fi
done

# Maintainer scripts must be executable.
for f in "${debian}"/*.preinst "${debian}"/*.postinst "${debian}"/*.prerm "${debian}"/*.postrm; do
  [ -e "${f}" ] || continue
  if [ ! -x "${f}" ]; then
    echo "FAIL: maintainer script not executable: ${f}" >&2
    fail=1
  fi
  # Strip the DEBHELPER token before syntax-checking with sh -n.
  if ! sed 's/#DEBHELPER#//' "${f}" | sh -n; then
    echo "FAIL: maintainer script has shell syntax error: ${f}" >&2
    fail=1
  fi
done

# Conffile assertions: each core package's config file must be marked
# as a conffile so upgrades preserve operator edits.
for cfg_pkg in tensorplate-agent tensorplate-observability tensorplate-serving tensorplate-cli; do
  if [ ! -f "${debian}/${cfg_pkg}.conffiles" ]; then
    echo "FAIL: ${cfg_pkg} missing ${cfg_pkg}.conffiles" >&2
    fail=1
  fi
done

# systemd units: agent + observability must have one, serving must not.
for unit_pkg in tensorplate-agent tensorplate-observability; do
  if [ ! -f "${debian}/${unit_pkg}.service" ]; then
    echo "FAIL: ${unit_pkg} missing ${unit_pkg}.service" >&2
    fail=1
  fi
done
if [ -f "${debian}/tensorplate-serving.service" ]; then
  echo "FAIL: tensorplate-serving.service must not exist (agent supervises the serving worker, V01-E09)" >&2
  fail=1
fi

# debian/rules and source/format must exist.
for f in "${debian}/rules" "${debian}/source/format" "${debian}/changelog" "${debian}/compat" "${debian}/copyright"; do
  if [ ! -f "${f}" ]; then
    echo "FAIL: missing ${f}" >&2
    fail=1
  fi
done
if [ ! -x "${debian}/rules" ]; then
  echo "FAIL: debian/rules must be executable" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_debian_metadata: ok"
fi
exit "${fail}"
