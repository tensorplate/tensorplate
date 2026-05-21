# SPDX-License-Identifier: Apache-2.0
#
# Sourced by lifecycle scripts. POSIX sh.

# tensorplate_version_lt A B
#   Exits 0 (true) iff A < B under Debian-like ordering (digits
#   compared numerically, non-digit suffixes lexically). Sufficient
#   for the small set of versions v0.1.0 packages will see.
# shellcheck shell=sh

tensorplate_version_lt() {
  if [ "$1" = "$2" ]; then
    return 1
  fi
  if command -v dpkg >/dev/null 2>&1; then
    if dpkg --compare-versions "$1" lt "$2"; then
      return 0
    fi
    return 1
  fi
  # Fallback: split on dots/dashes/pluses, compare numeric then
  # lexical. Pre-release suffix on the right side is treated as less
  # than the bare version on the left (semver-style).
  _a="$1"
  _b="$2"
  _ai=1
  while :; do
    _ap="$(printf '%s' "${_a}" | awk -F'[.+~-]' -v i="${_ai}" '{print $i}')"
    _bp="$(printf '%s' "${_b}" | awk -F'[.+~-]' -v i="${_ai}" '{print $i}')"
    if [ -z "${_ap}" ] && [ -z "${_bp}" ]; then
      return 1
    fi
    if [ -z "${_ap}" ]; then
      case "${_bp}" in
        ''|*[!0-9]*) return 1;;
        *) return 0;;
      esac
    fi
    if [ -z "${_bp}" ]; then
      case "${_ap}" in
        ''|*[!0-9]*) return 0;;
        *) return 1;;
      esac
    fi
    case "${_ap}${_bp}" in
      *[!0-9]*)
        if [ "${_ap}" \< "${_bp}" ]; then return 0; fi
        if [ "${_ap}" \> "${_bp}" ]; then return 1; fi
        ;;
      *)
        if [ "${_ap}" -lt "${_bp}" ]; then return 0; fi
        if [ "${_ap}" -gt "${_bp}" ]; then return 1; fi
        ;;
    esac
    _ai=$((_ai + 1))
  done
}
