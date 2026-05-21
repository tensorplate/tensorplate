#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# Create the `tensorplate` system user/group used by the agent,
# observability, and serving worker. Idempotent.
#
# Called from maintainer scripts (`*.postinst`) with no arguments.
# Returns 0 when the user/group exist after the call, non-zero
# otherwise so dpkg surfaces a failed install rather than half-applying.

set -eu

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
. "${here}/path-constants.sh"

if command -v getent >/dev/null 2>&1; then
  if ! getent group "${TP_SYSTEM_GROUP}" >/dev/null 2>&1; then
    if command -v addgroup >/dev/null 2>&1; then
      addgroup --quiet --system "${TP_SYSTEM_GROUP}"
    elif command -v groupadd >/dev/null 2>&1; then
      groupadd --system "${TP_SYSTEM_GROUP}"
    else
      echo "tensorplate: cannot create system group; no addgroup or groupadd" >&2
      exit 1
    fi
  fi

  if ! getent passwd "${TP_SYSTEM_USER}" >/dev/null 2>&1; then
    if command -v adduser >/dev/null 2>&1; then
      adduser --quiet --system --ingroup "${TP_SYSTEM_GROUP}" \
        --home "${TP_STATE_DIR}" --no-create-home \
        --shell /usr/sbin/nologin "${TP_SYSTEM_USER}"
    elif command -v useradd >/dev/null 2>&1; then
      useradd --system --gid "${TP_SYSTEM_GROUP}" \
        --home-dir "${TP_STATE_DIR}" --no-create-home \
        --shell /usr/sbin/nologin "${TP_SYSTEM_USER}"
    else
      echo "tensorplate: cannot create system user; no adduser or useradd" >&2
      exit 1
    fi
  fi
else
  echo "tensorplate: getent not available; cannot manage system user safely" >&2
  exit 1
fi
