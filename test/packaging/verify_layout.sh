#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: filesystem layout verifier.
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

mkdir -p "${td}${TP_ETC_DIR}"
for cfg in ${TP_REQUIRED_CONFIG_FILES}; do
  : > "${td}${cfg}"
done

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
  # GNU stat accepts `-f` but interprets it as filesystem output. Try
  # its mode formatter first, then fall back to BSD stat for macOS.
  mode="$(stat -c '%a' "${full}" 2>/dev/null || stat -f '%Lp' "${full}")"
  if [ "${mode}" != "${TP_DIR_MODE#0}" ] && [ "${mode}" != "${TP_DIR_MODE}" ]; then
    echo "FAIL: ${d} mode=${mode} (expected ${TP_DIR_MODE})" >&2
    fail=1
  fi
done

for cfg in ${TP_AGENT_CONFIG_PATH} ${TP_OBSERVABILITY_CONFIG_PATH} ${TP_SERVING_WORKER_CONFIG_PATH}; do
  full="${td}${cfg}"
  mode="$(stat -c '%a' "${full}" 2>/dev/null || stat -f '%Lp' "${full}")"
  if [ "${mode}" != "${TP_CONF_FILE_MODE#0}" ] && [ "${mode}" != "${TP_CONF_FILE_MODE}" ]; then
    echo "FAIL: ${cfg} mode=${mode} (expected ${TP_CONF_FILE_MODE})" >&2
    fail=1
  fi
done
cli_mode="$(stat -c '%a' "${td}${TP_CLI_CONFIG_PATH}" 2>/dev/null || stat -f '%Lp' "${td}${TP_CLI_CONFIG_PATH}")"
if [ "${cli_mode}" != "${TP_CLI_FILE_MODE#0}" ] && [ "${cli_mode}" != "${TP_CLI_FILE_MODE}" ]; then
  echo "FAIL: ${TP_CLI_CONFIG_PATH} mode=${cli_mode} (expected ${TP_CLI_FILE_MODE})" >&2
  fail=1
fi

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
