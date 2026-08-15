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

ensure_config_file() {
  path="$1"
  mode="$2"
  full="${prefix}${path}"
  [ -e "${full}" ] || return 0
  chmod "${mode}" "${full}"
  if [ "${real_install}" -eq 1 ]; then
    chgrp "${TP_SYSTEM_GROUP}" "${full}" 2>/dev/null || true
    chown "root:${TP_SYSTEM_GROUP}" "${full}" 2>/dev/null || true
  fi
}

ensure_dir "${TP_ETC_DIR}"               "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_STATE_DIR}"             "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_STATE_INNER_DIR}"       "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_STAGING_DIR}"    "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_ACTIVE_DIR}"     "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_PREVIOUS_DIR}"   "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_QUARANTINE_DIR}" "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BUNDLE_IMPORT_DIR}"     "${TP_IMPORT_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_WORKER_CONFIG_DIR}"     "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_LOG_DIR}"               "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_BACKEND_DESCRIPTOR_DIR}" "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
ensure_dir "${TP_PLATFORM_REGISTRY_DIR}" "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"

# /run/tensorplate is created at boot by systemd RuntimeDirectory= for
# the agent unit. For real installs we still create it now so that
# manual launches of the agent before its unit starts also work.
if [ "${real_install}" -eq 1 ]; then
  ensure_dir "${TP_RUN_DIR}"             "${TP_DIR_MODE}" "${TP_SYSTEM_GROUP}"
fi

# Package payloads land before postinst runs. Normalize metadata on
# whichever configs are present without overwriting conffile contents.
ensure_config_file "${TP_AGENT_CONFIG_PATH}"          "${TP_CONF_FILE_MODE}"
ensure_config_file "${TP_OBSERVABILITY_CONFIG_PATH}"  "${TP_CONF_FILE_MODE}"
ensure_config_file "${TP_SERVING_WORKER_CONFIG_PATH}" "${TP_CONF_FILE_MODE}"
ensure_config_file "${TP_CLI_CONFIG_PATH}"            "${TP_CLI_FILE_MODE}"

# Verify what was just applied, and fail closed if it did not take. The
# chown/chgrp calls above are deliberately tolerant — they run before the
# system group is guaranteed to exist on some paths — so without this pass a
# failed ownership change leaves the agent unable to read its own state and
# says nothing. dpkg surfaces a non-zero postinst as a configure failure,
# which is what makes this actionable instead of silent.
if [ "${real_install}" -eq 1 ]; then
  layout_errors=0
  report() {
    echo "tensorplate: install layout check failed: $1" >&2
    layout_errors=$((layout_errors + 1))
  }

  for d in ${TP_REQUIRED_DIRECTORIES}; do
    # /run/tensorplate is systemd's (RuntimeDirectory=) and may legitimately
    # be absent until the unit first starts.
    if [ "${d}" = "${TP_RUN_DIR}" ]; then
      continue
    fi
    if [ ! -d "${d}" ]; then
      report "${d} was not created"
      continue
    fi
    observed_group="$(stat -c '%G' "${d}" 2>/dev/null || echo '?')"
    if [ "${observed_group}" != "${TP_SYSTEM_GROUP}" ]; then
      report "${d} group=${observed_group} (expected ${TP_SYSTEM_GROUP}); run \`chgrp -R ${TP_SYSTEM_GROUP} ${d}\`"
    fi
    case "${d}" in
      "${TP_BUNDLE_IMPORT_DIR}") expected_mode="${TP_IMPORT_DIR_MODE}" ;;
      *) expected_mode="${TP_DIR_MODE}" ;;
    esac
    observed_mode="$(stat -c '%a' "${d}" 2>/dev/null || echo '?')"
    if [ "${observed_mode}" != "${expected_mode#0}" ] && [ "${observed_mode}" != "${expected_mode}" ]; then
      report "${d} mode=${observed_mode} (expected ${expected_mode}); run \`chmod ${expected_mode} ${d}\`"
    fi
    # A world-writable install root is never acceptable, whatever the mode
    # bookkeeping says.
    if [ -n "$(find "${d}" -maxdepth 0 -perm -0002 2>/dev/null)" ]; then
      report "${d} is world-writable"
    fi
  done

  if [ "${layout_errors}" -ne 0 ]; then
    echo "tensorplate: ${layout_errors} install layout problem(s); refusing to leave a half-applied layout" >&2
    exit 1
  fi
fi
