#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# V01-E14-F08: filesystem layout verifier.
#
# Stages the install layout under a tempdir using the shared helper
# and asserts every documented directory exists with the documented
# permissions. The helper's --prefix mode keeps us out of /.

set -eu

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
. "${repo_root}/packaging/scripts/path-constants.sh"

td="$(mktemp -d)"
cleanup() { rm -rf "${td}"; }
trap cleanup EXIT

"${repo_root}/packaging/scripts/install-paths.sh" --prefix "${td}"

fail=0
for d in ${TP_REQUIRED_DIRECTORIES}; do
  # /run/tensorplate is only created on real installs (systemd owns
  # it via RuntimeDirectory=). The --prefix helper deliberately skips
  # it, so the verifier mirrors that contract.
  if [ "${d}" = "${TP_RUN_DIR}" ]; then
    continue
  fi
  full="${td}${d}"
  if [ ! -d "${full}" ]; then
    echo "FAIL: missing directory ${d}" >&2
    fail=1
    continue
  fi
  mode="$(stat -f '%Lp' "${full}" 2>/dev/null || stat -c '%a' "${full}")"
  if [ "${mode}" != "${TP_DIR_MODE#0}" ] && [ "${mode}" != "${TP_DIR_MODE}" ]; then
    echo "FAIL: ${d} mode=${mode} (expected ${TP_DIR_MODE})" >&2
    fail=1
  fi
done

# Check no path is world-writable. Use perm bit 0002.
ww="$(find "${td}" -type d -perm -0002 2>/dev/null || true)"
if [ -n "${ww}" ]; then
  echo "FAIL: world-writable directories detected:" >&2
  printf '%s\n' "${ww}" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_layout: ok (${td})"
fi
exit "${fail}"
