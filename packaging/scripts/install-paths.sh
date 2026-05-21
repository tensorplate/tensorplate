#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# Create the v0.1.0 installed filesystem layout with the documented
# ownership and permissions. Idempotent. Used by maintainer scripts
# (`*.postinst`) and by the packaging verification suite's dry-run
# fixture under a writable prefix.
#
# Usage:
#   install-paths.sh                 install under /
#   install-paths.sh --prefix DIR    install under DIR (used by tests)

set -eu

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
. "${here}/path-constants.sh"

prefix=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --prefix)
      shift
      [ "$#" -gt 0 ] || { echo "--prefix requires an argument" >&2; exit 2; }
      prefix="$1"
      ;;
    *)
      echo "install-paths.sh: unknown argument: $1" >&2
      exit 2
      ;;
  esac
  shift
done

# Resolve owner. Test mode (--prefix) keeps the running user since
# /var/lib/tensorplate is being staged outside the real filesystem.
real_install=1
if [ -n "${prefix}" ]; then
  real_install=0
fi

ensure_dir() {
  path="$1"
  mode="$2"
  group="$3"
  full="${prefix}${path}"
  mkdir -p "${full}"
  chmod "${mode}" "${full}"
  if [ "${real_install}" -eq 1 ]; then
    chgrp "${group}" "${full}" 2>/dev/null || true
    case "${path}" in
      "${TP_STATE_DIR}"*|"${TP_LOG_DIR}"*|"${TP_RUN_DIR}"*|"${TP_WORKER_CONFIG_DIR}"*)
        chown "${TP_SYSTEM_USER}:${group}" "${full}" 2>/dev/null || true
        ;;
      *)
        # config and share roots stay root-owned but group-readable.
        chown "root:${group}" "${full}" 2>/dev/null || true
        ;;
    esac
  fi
}

ensure_dir "${TP_ETC_DIR}"               "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_STATE_DIR}"             "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_STATE_INNER_DIR}"       "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_STAGING_DIR}"    "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_ACTIVE_DIR}"     "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_PREVIOUS_DIR}"   "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_QUARANTINE_DIR}" "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_WORKER_CONFIG_DIR}"     "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_LOG_DIR}"               "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BACKEND_DESCRIPTOR_DIR}" "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"

# /run/tensorplate is created at boot by systemd RuntimeDirectory= for
# the agent unit. For real installs we still create it now so that
# manual launches of the agent before its unit starts also work.
if [ "${real_install}" -eq 1 ]; then
  ensure_dir "${TP_RUN_DIR}"             "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
fi
