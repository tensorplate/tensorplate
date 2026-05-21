#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# V01-E14-F07: upgrade preflight.
#
# Called from `*.preinst upgrade` (and `*.postinst configure` with the
# previous version argument). Refuses an upgrade if:
#   - the installed config file's schema_version field is unknown to
#     the new build (a future schema bump must explicitly add itself
#     to the supported list below);
#   - durable state lives under /var/lib/tensorplate but is not
#     readable by the tensorplate group (operator overrode perms);
#   - the new package version is older than the installed one
#     (downgrades are not supported in v0.1.0).
#
# Exits 0 on pass. Exits 1 with a typed message on fail; dpkg will
# refuse to configure the package.

set -eu

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
. "${here}/path-constants.sh"
. "${here}/version-utils.sh"

PREV_VERSION="${1-}"
NEW_VERSION="${2-}"

# v0.1.0 supported schema_versions. Future builds extend this list.
SUPPORTED_SCHEMAS="0.1"

check_schema() {
  cfg="$1"
  [ -r "${cfg}" ] || return 0
  observed="$(grep -o '"schema_version"[[:space:]]*:[[:space:]]*"[^"]*"' "${cfg}" | head -n 1 | sed 's/.*"\([^"]*\)"$/\1/')"
  if [ -z "${observed}" ]; then
    echo "tensorplate: upgrade aborted: ${cfg} missing schema_version" >&2
    exit 1
  fi
  for s in ${SUPPORTED_SCHEMAS}; do
    if [ "${s}" = "${observed}" ]; then
      return 0
    fi
  done
  echo "tensorplate: upgrade aborted: ${cfg} declares unknown schema_version=${observed}" >&2
  echo "tensorplate: supported schema versions: ${SUPPORTED_SCHEMAS}" >&2
  exit 1
}

# 1) Schema-version preflight.
for cfg in ${TP_REQUIRED_CONFIG_FILES}; do
  check_schema "${cfg}"
done

# 2) Permission preflight (state dir must remain group-readable so the
#    upgraded agent can still load durable state without a chmod cycle).
if [ -d "${TP_STATE_DIR}" ]; then
  group="$(stat -c '%G' "${TP_STATE_DIR}" 2>/dev/null || echo '')"
  case "${group}" in
    "${TP_SYSTEM_GROUP}"|"")
      ;;
    *)
      echo "tensorplate: upgrade aborted: ${TP_STATE_DIR} group=${group} (expected ${TP_SYSTEM_GROUP})" >&2
      echo "tensorplate: run \`chgrp -R ${TP_SYSTEM_GROUP} ${TP_STATE_DIR}\` and retry" >&2
      exit 1
      ;;
  esac
fi

# 3) Downgrade rejection. v0.1.0 does not support running an older
#    package against a newer durable state.
if [ -n "${PREV_VERSION}" ] && [ -n "${NEW_VERSION}" ]; then
  if tensorplate_version_lt "${NEW_VERSION}" "${PREV_VERSION}"; then
    echo "tensorplate: upgrade aborted: downgrade from ${PREV_VERSION} to ${NEW_VERSION} is not supported" >&2
    echo "tensorplate: see docs/install/lifecycle.md for the manual reset procedure" >&2
    exit 1
  fi
fi

exit 0
