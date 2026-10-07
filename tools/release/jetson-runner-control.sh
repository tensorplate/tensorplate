#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Operator helper for the Jetson GitHub Actions release runner.
#
# This script intentionally contains no GitHub registration token, password,
# repository secret, or private network address. It toggles a previously
# configured self-hosted runner service and its temporary release sudoers
# allowance, and provisions the runner account's vcpkg checkout and cache.

set -Eeuo pipefail

readonly DEFAULT_RUNNER_USER="gha-runner"
readonly DEFAULT_RUNNER_SERVICE="actions.runner.tensorplate-tensorplate.ubuntu.service"
readonly DEFAULT_REQUIRED_LABELS="self-hosted,linux,ARM64,tensorplate-release"
readonly DEFAULT_VCPKG_GIT_URL="https://github.com/microsoft/vcpkg.git"
readonly DEFAULT_VCPKG_BUILD_TIMEOUT="21600"
readonly VCPKG_FEATURE="streaming-grpc"
readonly PLAIN_PATHS_RULE="the vcpkg directories must be absolute paths of [A-Za-z0-9._/+@-] with no empty, '.' or '..' component"
readonly NO_CHECKOUT="this script was not started from its file in a repository checkout"
readonly STAMP_KEYS=(
  baseline dependencies_sha256 triplet feature vcpkg_version compiler provisioned_at
  archives_sha256
)
# Refuses duplicate keys and non-integer numbers: manifests that say different
# things must not share a digest. No exception may reach a crash reporter.
readonly MANIFEST_FACTS='
import hashlib
import json
import sys


def members(pairs):
    found = dict(pairs)
    if len(found) != len(pairs):
        raise ValueError("an object names a key twice")
    return found


def refused(text):
    raise ValueError("a manifest holds no such number: " + text)


try:
    with open(sys.argv[1], "rb") as manifest_file:
        manifest = json.loads(
            manifest_file.read().decode("utf-8"),
            object_pairs_hook=members,
            parse_constant=refused,
            parse_float=refused,
        )
    if type(manifest) is not dict:
        raise ValueError("the manifest is not an object")
    baseline = manifest.get("builtin-baseline")
    for key in ("version", "version-string", "version-semver", "version-date"):
        manifest.pop(key, None)
    canonical = json.dumps(manifest, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    digest = hashlib.sha256(canonical.encode("ascii")).hexdigest()
    sys.stdout.write("%s\n%s\n" % (json.dumps(baseline), digest))
except Exception:
    sys.exit(1)
'

RUNNER_USER="${TP_JETSON_RUNNER_USER:-$DEFAULT_RUNNER_USER}"
RUNNER_DIR="${TP_JETSON_RUNNER_DIR:-/home/${RUNNER_USER}/actions-runner}"
RUNNER_SERVICE="${TP_JETSON_RUNNER_SERVICE:-$DEFAULT_RUNNER_SERVICE}"
SUDOERS_FILE="${TP_JETSON_RUNNER_SUDOERS_FILE:-/etc/sudoers.d/gha-runner-tensorplate-release}"
REQUIRED_LABELS="${TP_JETSON_RUNNER_LABELS:-$DEFAULT_REQUIRED_LABELS}"
CARGO_BIN="${TP_JETSON_RUNNER_CARGO_BIN:-/home/${RUNNER_USER}/.cargo/bin}"

APT_GET="${TP_JETSON_RUNNER_APT_GET:-/usr/bin/apt-get}"
INSTALL="${TP_JETSON_RUNNER_INSTALL:-/usr/bin/install}"
VISUDO="${TP_JETSON_RUNNER_VISUDO:-/usr/sbin/visudo}"
TIMEOUT_BIN="${TP_JETSON_RUNNER_TIMEOUT:-/usr/bin/timeout}"
FLOCK_BIN="${TP_JETSON_RUNNER_FLOCK:-/usr/bin/flock}"
DPKG_BIN="${TP_JETSON_RUNNER_DPKG:-/usr/bin/dpkg}"
APT_WRAPPER="${TP_JETSON_RUNNER_APT_WRAPPER:-/usr/local/sbin/tensorplate-apt}"
ID_BIN="${TP_JETSON_RUNNER_ID:-/usr/bin/id}"

VCPKG_CHECKOUT="${TP_JETSON_RUNNER_VCPKG_DIR:-/home/${RUNNER_USER}/vcpkg}"
VCPKG_CACHE_DIR="${TP_JETSON_RUNNER_VCPKG_CACHE_DIR:-/home/${RUNNER_USER}/.cache/tensorplate-vcpkg}"
VCPKG_GIT_URL="${TP_JETSON_RUNNER_VCPKG_GIT_URL:-$DEFAULT_VCPKG_GIT_URL}"
VCPKG_BUILD_TIMEOUT="${TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT:-$DEFAULT_VCPKG_BUILD_TIMEOUT}"
VCPKG_ARCHIVES="${VCPKG_CACHE_DIR}/archives"
VCPKG_STAMP="${VCPKG_CACHE_DIR}/provisioned.stamp"
VCPKG_LOCK="${VCPKG_CACHE_DIR}/provision.lock"
VCPKG_TOOL="${VCPKG_CHECKOUT}/vcpkg"

# No setting replaces the pin: it is this checkout's builtin-baseline.
SCRIPT_PATH=""
REPO_ROOT=""
MANIFEST=""
MANIFEST_CONFIGURATION=""
if [[ -n "${BASH_SOURCE[0]:-}" ]]; then
  SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  SCRIPT_PATH="${SCRIPT_DIR}/$(basename -- "${BASH_SOURCE[0]}")"
  REPO_ROOT="$(dirname -- "$(dirname -- "$SCRIPT_DIR")")"
  MANIFEST="${REPO_ROOT}/vcpkg.json"
  MANIFEST_CONFIGURATION="${REPO_ROOT}/vcpkg-configuration.json"
fi

MANIFEST_BASELINE=""
DEPENDENCIES_SHA256=""
MANIFEST_ERROR=""
NOT_READY_REASON=""
PROOF_FAILURE=""
THROWAWAY_ROOT=""
THROWAWAY_PATHS=()

usage() {
  cat <<EOF
Usage:
  jetson-runner-control.sh on
  jetson-runner-control.sh off
  jetson-runner-control.sh status
  jetson-runner-control.sh provision-vcpkg [--check]
  jetson-runner-control.sh vcpkg-env

Controls the TensorPlate Jetson self-hosted release runner.

provision-vcpkg runs as the runner account and needs no root. It puts the
vcpkg checkout at the builtin-baseline of this checkout's vcpkg.json, builds the
${VCPKG_FEATURE} manifest feature into a binary cache on the runner, proves
that a second install restores every package from that cache without
building, and only then records the runner as ready. --check repeats the
proof against the existing cache. A proof that passes changes nothing; one
that fails removes the record, so the runner stops being reported as ready.
Only one provision-vcpkg, with or without --check, runs at a time: a second
one is refused while the first, or a build it left behind, holds the lock.

The record is for the vcpkg baseline and for the dependencies digest of this
checkout's vcpkg.json: the SHA-256 of the manifest's content without its own
version. python3 reads the file as JSON, drops the top-level version,
version-string, version-semver and version-date, and hashes the rest in one
canonical form. A new version, or the same content laid out another way,
leaves the runner ready; any other change to what the manifest says does
not. A vcpkg.json that is not one JSON object in UTF-8 with no byte order
mark, names a key twice, holds a number other than an integer or has no
builtin-baseline of 40 lowercase hex digits is refused. So is a checkout
with a vcpkg-configuration.json beside its vcpkg.json: vcpkg would read
that file, and the digest covers vcpkg.json alone.

vcpkg-env prints the three environment lines a release job needs to use that
checkout and cache. It prints nothing and fails unless the runner is ready
for this checkout's vcpkg.json and git reports the vcpkg checkout unmodified.
It asks git as the runner account and refuses to run as root.

Environment overrides:
  TP_JETSON_RUNNER_USER          default: ${DEFAULT_RUNNER_USER}
  TP_JETSON_RUNNER_DIR           default: /home/\$TP_JETSON_RUNNER_USER/actions-runner
  TP_JETSON_RUNNER_SERVICE       default: ${DEFAULT_RUNNER_SERVICE}
  TP_JETSON_RUNNER_SUDOERS_FILE  default: /etc/sudoers.d/gha-runner-tensorplate-release
  TP_JETSON_RUNNER_LABELS        default: ${DEFAULT_REQUIRED_LABELS}
  TP_JETSON_RUNNER_VCPKG_DIR            default: /home/\$TP_JETSON_RUNNER_USER/vcpkg
  TP_JETSON_RUNNER_VCPKG_CACHE_DIR      default: /home/\$TP_JETSON_RUNNER_USER/.cache/tensorplate-vcpkg
  TP_JETSON_RUNNER_VCPKG_GIT_URL        default: ${DEFAULT_VCPKG_GIT_URL}
  TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT  default: ${DEFAULT_VCPKG_BUILD_TIMEOUT} (seconds per vcpkg run)
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

note() {
  printf '==> %s\n' "$*"
}

require_root() {
  if [[ "${EUID}" -ne 0 ]]; then
    die "run this command with sudo"
  fi
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

service_exists() {
  systemctl list-unit-files "$RUNNER_SERVICE" --no-legend 2>/dev/null |
    awk '{print $1}' |
    grep -Fxq "$RUNNER_SERVICE"
}

runner_configured() {
  [[ -f "${RUNNER_DIR}/.runner" && -x "${RUNNER_DIR}/svc.sh" ]]
}

ensure_runner_configured() {
  [[ -d "$RUNNER_DIR" ]] || die "runner directory not found: $RUNNER_DIR"
  runner_configured ||
    die "runner is not configured in $RUNNER_DIR; configure it with GitHub before using this helper"
}

install_service_if_needed() {
  if service_exists; then
    return
  fi
  note "installing runner service ${RUNNER_SERVICE} as ${RUNNER_USER}"
  (cd "$RUNNER_DIR" && ./svc.sh install "$RUNNER_USER")
}

ensure_runner_path() {
  local path_file="${RUNNER_DIR}/.path"
  local fallback_path="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin"
  local current_path

  [[ -d "$CARGO_BIN" ]] || return
  current_path="$fallback_path"
  if [[ -f "$path_file" ]]; then
    current_path="$(<"$path_file")"
  fi
  if [[ ":${current_path}:" != *":${CARGO_BIN}:"* ]]; then
    note "adding ${CARGO_BIN} to runner service PATH"
    printf '%s:%s\n' "$CARGO_BIN" "$current_path" >"$path_file"
    chown "$RUNNER_USER:$RUNNER_USER" "$path_file"
  fi
}

# The runner needs apt under a hard time bound, and the bound has to be
# apt's direct parent -- `timeout N sudo apt-get` signals sudo, which does
# not pass it on. Granting `timeout` would solve that and hand the runner
# account a root shell: `sudo timeout 1 /bin/sh`. So the bound moves into a
# root-owned wrapper and the grant names the wrapper, which is both narrower
# than the old `apt-get` grant and the only path that needs to exist.
write_apt_wrapper() {
  local tmp
  [[ -x "$TIMEOUT_BIN" ]] || die "timeout not found at $TIMEOUT_BIN"
  [[ -x "$APT_GET" ]] || die "apt-get not found at $APT_GET"
  [[ -x "$DPKG_BIN" ]] || die "dpkg not found at $DPKG_BIN"

  tmp="$(mktemp)"
  cat >"$tmp" <<WRAPPER
#!/bin/sh
# Managed by TensorPlate jetson-runner-control.sh. Do not edit; \`off\`
# removes it and \`on\` rewrites it.
#
# Usage: tensorplate-apt <bound-seconds> <apt-get argument>...
#        tensorplate-apt configure-pending
#
# Exists so the runner account can run a time-bounded apt without holding
# NOPASSWD on \`timeout\`, which would be equivalent to unrestricted root.
set -eu

if [ "\${1:-}" = "probe" ]; then
  # Reachable only if the sudoers grant permits this path. \`status\` uses it
  # instead of inferring the grant from a binary that is no longer in it.
  exit 0
fi

if [ "\${1:-}" = "configure-pending" ]; then
  [ "\$#" -eq 1 ] || { echo "tensorplate-apt: configure-pending takes no arguments" >&2; exit 64; }
  exec "$DPKG_BIN" --configure -a
fi

bound="\${1:-}"
case "\$bound" in
  ''|*[!0-9]*) echo "tensorplate-apt: first argument must be a bound in seconds" >&2; exit 64 ;;
esac
shift

# Only the subcommands the release path uses. A wrapper that forwards any
# apt-get verb would re-admit \`apt-get source\` and friends as root.
case "\${1:-}" in
  update|install) ;;
  *) echo "tensorplate-apt: refusing apt-get '\${1:-}'" >&2; exit 64 ;;
esac

exec "$TIMEOUT_BIN" -k 30 "\$bound" "$APT_GET" "\$@"
WRAPPER
  "$INSTALL" -m 0755 -o root -g root "$tmp" "$APT_WRAPPER" || {
    rm -f "$tmp"
    return 1
  }
  rm -f "$tmp"
  note "installed bounded apt wrapper at ${APT_WRAPPER}"
}

remove_apt_wrapper() {
  rm -f "$APT_WRAPPER"
}

write_sudoers() {
  local tmp
  [[ -x "$VISUDO" ]] || die "visudo not found at $VISUDO"
  [[ -x "$INSTALL" ]] || die "install not found at $INSTALL"
  [[ -x "$APT_WRAPPER" ]] || die "apt wrapper not found at $APT_WRAPPER"

  tmp="$(mktemp)"
  {
    printf '# Managed by TensorPlate jetson-runner-control.sh\n'
    printf '# Temporary release-build allowance for the Jetson self-hosted runner.\n'
    printf '# The wrapper replaces a bare apt-get grant: it bounds apt in time as\n'
    printf '# root and accepts only the subcommands the release path uses.\n'
    printf '%s ALL=(root) NOPASSWD: %s, %s\n' "$RUNNER_USER" "$APT_WRAPPER" "$INSTALL"
  } >"$tmp"
  "$VISUDO" -cf "$tmp" >/dev/null || {
    rm -f "$tmp"
    return 1
  }
  "$INSTALL" -m 0440 -o root -g root "$tmp" "$SUDOERS_FILE" || {
    rm -f "$tmp"
    return 1
  }
  rm -f "$tmp"
}

remove_sudoers() {
  rm -f "$SUDOERS_FILE"
}

cmd_on() {
  require_root
  require_command systemctl
  ensure_runner_configured
  install_service_if_needed
  ensure_runner_path
  write_apt_wrapper
  write_sudoers
  note "starting ${RUNNER_SERVICE}"
  systemctl enable --now "$RUNNER_SERVICE"
  cmd_status
}

cmd_off() {
  require_root
  require_command systemctl
  if service_exists; then
    note "stopping ${RUNNER_SERVICE}"
    systemctl disable --now "$RUNNER_SERVICE" || true
  else
    note "runner service is not installed: ${RUNNER_SERVICE}"
  fi
  remove_sudoers
  remove_apt_wrapper
  cmd_status
}

print_sudoers_status() {
  if [[ -f "$SUDOERS_FILE" ]]; then
    printf 'sudoers: enabled (%s)\n' "$SUDOERS_FILE"
    if [[ "${EUID}" -eq 0 ]]; then
      "$VISUDO" -cf "$SUDOERS_FILE" >/dev/null &&
        printf 'sudoers_valid: yes\n'
      # The grant names the wrapper, not apt-get: a bare apt-get grant let
      # the runner run every apt subcommand as root, and `timeout` could not
      # be granted at all without handing it a root shell. Probing apt-get
      # here would report `no` on a correctly provisioned runner.
      if sudo -u "$RUNNER_USER" sudo -n "$APT_WRAPPER" probe >/dev/null 2>&1; then
        printf 'runner_can_sudo_apt_wrapper: yes\n'
      else
        printf 'runner_can_sudo_apt_wrapper: no\n'
      fi
      if [[ -x "$APT_WRAPPER" ]]; then
        printf 'apt_wrapper: present (%s)\n' "$APT_WRAPPER"
      else
        printf 'apt_wrapper: ABSENT (%s) -- run off then on from a current checkout\n' "$APT_WRAPPER"
      fi
      if sudo -u "$RUNNER_USER" sudo -n "$INSTALL" --version >/dev/null 2>&1; then
        printf 'runner_can_sudo_install: yes\n'
      else
        printf 'runner_can_sudo_install: no\n'
      fi
    fi
  else
    printf 'sudoers: disabled (%s absent)\n' "$SUDOERS_FILE"
  fi
}

cmd_status() {
  require_command systemctl
  printf 'runner_user: %s\n' "$RUNNER_USER"
  printf 'runner_dir: %s\n' "$RUNNER_DIR"
  printf 'runner_service: %s\n' "$RUNNER_SERVICE"
  printf 'required_labels: %s\n' "$REQUIRED_LABELS"
  if runner_configured; then
    printf 'runner_configured: yes\n'
  else
    printf 'runner_configured: no\n'
  fi
  if service_exists; then
    printf 'service_enabled: %s\n' "$(systemctl is-enabled "$RUNNER_SERVICE" 2>/dev/null || true)"
    printf 'service_active: %s\n' "$(systemctl is-active "$RUNNER_SERVICE" 2>/dev/null || true)"
  else
    printf 'service_installed: no\n'
  fi
  if [[ -f "${RUNNER_DIR}/.path" ]]; then
    printf 'runner_path: %s\n' "$(<"${RUNNER_DIR}/.path")"
  fi
  print_sudoers_status
}

# python3 runs isolated (-I), so the caller's environment and directory and
# the account's site directory cannot hand it other code.
load_manifest() {
  local facts baseline status=0
  MANIFEST_BASELINE=""
  DEPENDENCIES_SHA256=""
  MANIFEST_ERROR=""
  [[ -n "$MANIFEST" ]] || {
    MANIFEST_ERROR="cannot read a vcpkg.json: ${NO_CHECKOUT}"
    return 1
  }
  # Only a regular file is read: opening anything else could block for good.
  [[ -f "$MANIFEST" && -r "$MANIFEST" ]] || {
    MANIFEST_ERROR="cannot read ${MANIFEST}; run this command from a repository checkout"
    return 1
  }
  [[ ! -e "$MANIFEST_CONFIGURATION" && ! -L "$MANIFEST_CONFIGURATION" ]] || {
    MANIFEST_ERROR="${MANIFEST_CONFIGURATION} is not covered by the dependencies digest and vcpkg would read it; move what it says into ${MANIFEST} as its vcpkg-configuration member"
    return 1
  }
  command -v python3 >/dev/null 2>&1 || {
    MANIFEST_ERROR="missing required command: python3, which reads ${MANIFEST}"
    return 1
  }
  facts="$(python3 -I -c "$MANIFEST_FACTS" "$MANIFEST")" || status=$?
  # 1 is the program's refusal; any other status is the interpreter's own.
  [[ "$status" -ne 1 ]] || {
    MANIFEST_ERROR="python3 could not read ${MANIFEST} (exit ${status}): a manifest is one JSON object in UTF-8 with no byte order mark that names no key twice and holds no number other than an integer"
    return 1
  }
  [[ "$status" -eq 0 ]] || {
    MANIFEST_ERROR="python3 failed (exit ${status}) before it answered for ${MANIFEST}"
    return 1
  }
  [[ "${facts%%$'\n'*}" =~ ^\"([0-9a-f]{40})\"$ ]] || {
    MANIFEST_ERROR="${MANIFEST} does not name a builtin-baseline of 40 lowercase hex digits"
    return 1
  }
  baseline="${BASH_REMATCH[1]}"
  [[ "${facts#*$'\n'}" =~ ^[0-9a-f]{64}$ ]] || {
    MANIFEST_ERROR="cannot compute the dependencies digest of ${MANIFEST}"
    return 1
  }
  MANIFEST_BASELINE="$baseline"
  DEPENDENCIES_SHA256="${facts#*$'\n'}"
}

host_triplet() {
  local machine
  machine="$(uname -m 2>/dev/null)" || return 1
  case "$machine" in
    aarch64) printf 'arm64-linux\n' ;;
    x86_64) printf 'x64-linux\n' ;;
    *) return 1 ;;
  esac
}

# False under a parent this account cannot search: absence there is a guess.
known_missing() {
  local parent="$1"
  [[ ! -e "$1" ]] || return 1
  while [[ ! -e "$parent" ]]; do
    parent="$(dirname -- "$parent")"
  done
  [[ -x "$parent" ]]
}

# Reads HEAD from its file rather than through git: `status` is run by root
# on a checkout the runner account owns, which git refuses to open.
checkout_head() {
  local head_file="${VCPKG_CHECKOUT}/.git/HEAD" head=""
  if known_missing "$head_file"; then
    printf 'absent\n'
    return
  fi
  if [[ -f "$head_file" ]]; then
    { read -r head <"$head_file"; } 2>/dev/null || true
  fi
  if [[ "$head" =~ ^[0-9a-f]{40}$ ]]; then
    printf '%s\n' "$head"
  elif [[ "$head" == "ref: "* ]]; then
    printf 'not-detached\n'
  else
    printf 'unreadable\n'
  fi
}

stamp_value() {
  local key="$1" line value="" seen=0
  {
    while IFS= read -r line || [[ -n "$line" ]]; do
      if [[ "$line" == "${key}="* ]]; then
        value="${line#*=}"
        seen=$((seen + 1))
      fi
    done <"$VCPKG_STAMP"
  } 2>/dev/null || true
  [[ "$seen" -eq 1 ]] || return 1
  printf '%s\n' "$value"
}

# Both directories end up in VCPKG_BINARY_SOURCES and in a job's environment
# file, where a comma, a semicolon or a line break would be read as syntax.
plain_vcpkg_paths() {
  local path
  for path in "$VCPKG_CHECKOUT" "$VCPKG_CACHE_DIR"; do
    [[ "$path" =~ ^(/[A-Za-z0-9._+@-]+)+$ ]] || return 1
    [[ "${path}/" != */./* && "${path}/" != */../* ]] || return 1
  done
}

# The stamp records this, so a cache that later loses an archive is not ready.
cache_listing_sha256() {
  local digest
  # From /: GNU find exits 1 after a complete listing when it cannot return
  # to a start directory this account cannot enter.
  digest="$(cd / && find "${VCPKG_ARCHIVES}/" -type f -printf '%P\n' 2>/dev/null |
    LC_ALL=C sort | sha256sum)" || return 1
  digest="${digest%% *}"
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || return 1
  printf '%s\n' "$digest"
}

# Every check names its own failure: one that cannot be made is a refusal.
vcpkg_ready() {
  local triplet head key listing
  NOT_READY_REASON=""
  plain_vcpkg_paths || {
    NOT_READY_REASON="$PLAIN_PATHS_RULE"
    return 1
  }
  load_manifest || {
    NOT_READY_REASON="$MANIFEST_ERROR"
    return 1
  }
  triplet="$(host_triplet)" || {
    NOT_READY_REASON="this machine has no vcpkg triplet (expected aarch64 or x86_64)"
    return 1
  }
  [[ -f "$VCPKG_STAMP" && -r "$VCPKG_STAMP" ]] || {
    NOT_READY_REASON="cannot read a stamp at ${VCPKG_STAMP} as this account"
    ! known_missing "$VCPKG_STAMP" ||
      NOT_READY_REASON="not provisioned: no stamp at ${VCPKG_STAMP}"
    return 1
  }
  for key in "${STAMP_KEYS[@]}"; do
    stamp_value "$key" >/dev/null || {
      NOT_READY_REASON="the stamp at ${VCPKG_STAMP} does not record ${key} exactly once"
      return 1
    }
  done
  [[ "$(stamp_value baseline)" == "$MANIFEST_BASELINE" ]] || {
    NOT_READY_REASON="provisioned for another vcpkg baseline than ${MANIFEST_BASELINE}"
    return 1
  }
  [[ "$(stamp_value dependencies_sha256)" == "$DEPENDENCIES_SHA256" ]] || {
    NOT_READY_REASON="provisioned for another dependencies digest than ${MANIFEST} has now"
    return 1
  }
  [[ "$(stamp_value triplet)" == "$triplet" ]] || {
    NOT_READY_REASON="provisioned for another triplet than ${triplet}"
    return 1
  }
  [[ "$(stamp_value feature)" == "$VCPKG_FEATURE" ]] || {
    NOT_READY_REASON="provisioned for another manifest feature than ${VCPKG_FEATURE}"
    return 1
  }
  head="$(checkout_head)"
  [[ "$head" == "$MANIFEST_BASELINE" ]] || {
    NOT_READY_REASON="the vcpkg checkout is ${head}, not at the baseline ${MANIFEST_BASELINE}"
    return 1
  }
  [[ -f "$VCPKG_TOOL" && -x "$VCPKG_TOOL" ]] || {
    NOT_READY_REASON="no executable vcpkg tool at ${VCPKG_TOOL}"
    return 1
  }
  listing="$(cache_listing_sha256)" || {
    NOT_READY_REASON="cannot list the binary cache at ${VCPKG_ARCHIVES}"
    return 1
  }
  [[ "$(stamp_value archives_sha256)" == "$listing" ]] || {
    NOT_READY_REASON="the binary cache at ${VCPKG_ARCHIVES} does not hold exactly the archives it was provisioned with"
    return 1
  }
  return 0
}

require_runner_account() {
  local uid name hint account=""
  uid="$("$ID_BIN" -u 2>/dev/null)" || die "cannot read the current user id from ${ID_BIN}"
  name="$("$ID_BIN" -un 2>/dev/null)" || die "cannot read the current user name from ${ID_BIN}"
  # EUID is the shell's own answer: the id override cannot lift this refusal.
  if [[ "${EUID}" -eq 0 || "$uid" == "0" || "$name" != "$RUNNER_USER" ]]; then
    # The hinted command gets a clean environment, so it names the account.
    [[ "$RUNNER_USER" == "$DEFAULT_RUNNER_USER" ]] ||
      account="env TP_JETSON_RUNNER_USER=${RUNNER_USER} "
    hint="$(printf 'sudo -u %s -H %s%s provision-vcpkg' "$RUNNER_USER" "$account" "$SCRIPT_PATH")"
    die "provision-vcpkg runs as the runner account ${RUNNER_USER}, never as root; use: ${hint}${1:+ $1}"
  fi
}

remove_throwaway_paths() {
  local path
  for path in ${THROWAWAY_PATHS[@]+"${THROWAWAY_PATHS[@]}"}; do
    rm -rf -- "$path" ||
      printf 'warning: could not remove %s; remove it by hand\n' "$path" >&2
  done
}

# timeout puts vcpkg in a process group of its own: pass the signal on to it.
stop_on_signal() {
  local name="$1" status="$2" pid
  trap '' INT TERM HUP
  for pid in $(jobs -p); do
    kill -TERM "$pid" 2>/dev/null || true
  done
  wait
  printf 'error: stopped by SIG%s before the command finished\n' "$name" >&2
  exit "$status"
}

new_throwaway_root() {
  THROWAWAY_ROOT="$(mktemp -d "${VCPKG_CACHE_DIR}/install.XXXXXX")" ||
    die "cannot create an install root under ${VCPKG_CACHE_DIR}"
  THROWAWAY_PATHS+=("$THROWAWAY_ROOT")
}

# The lock is on a descriptor every child inherits, so a build that outlives
# a killed command still holds it.
take_provisioning_lock() {
  local status=0
  mkdir -p "$VCPKG_CACHE_DIR" || die "cannot create ${VCPKG_CACHE_DIR}"
  # Through `command`: in POSIX mode a plain exec that fails ends the script.
  { command exec 9>>"$VCPKG_LOCK"; } 2>/dev/null || die "cannot open the lock at ${VCPKG_LOCK}"
  "$FLOCK_BIN" -n 9 || status=$?
  [[ "$status" -ne 1 ]] ||
    die "another provision-vcpkg is running, or the build of one that was killed still is: ${VCPKG_LOCK} is locked. Wait for it to end, or stop that build, and run this command again; 'fuser -v ${VCPKG_LOCK}' lists what holds the lock"
  [[ "$status" -eq 0 ]] || die "cannot lock ${VCPKG_LOCK} (${FLOCK_BIN} exit ${status})"
}

# GIT_DIR and the like override -C, and a job's environment may set them.
forget_git_environment() {
  local names name
  names="$(git rev-parse --local-env-vars)" ||
    die "cannot ask git which environment variables name a repository"
  while IFS= read -r name; do
    [[ -z "$name" ]] || unset "$name"
  done <<<"$names"
}

require_clean_checkout() {
  local changes
  # Asked for explicitly: a user setting can hide untracked files.
  changes="$(git -C "$VCPKG_CHECKOUT" status --porcelain --untracked-files=all)" ||
    die "cannot inspect ${VCPKG_CHECKOUT} with git"
  [[ -z "$changes" ]] ||
    die "${VCPKG_CHECKOUT} has local modifications or untracked files; refusing to build from it"
}

prepare_vcpkg_checkout() {
  if [[ ! -e "$VCPKG_CHECKOUT" ]]; then
    note "cloning ${VCPKG_GIT_URL} into ${VCPKG_CHECKOUT}"
    git clone -- "$VCPKG_GIT_URL" "$VCPKG_CHECKOUT"
  fi
  [[ -d "${VCPKG_CHECKOUT}/.git" ]] ||
    die "${VCPKG_CHECKOUT} exists and is not a git checkout; refusing to replace it"
  [[ -f "${VCPKG_CHECKOUT}/.vcpkg-root" ]] ||
    die "${VCPKG_CHECKOUT} is not a vcpkg checkout (it has no .vcpkg-root); refusing to move it"
  require_clean_checkout
  if ! git -C "$VCPKG_CHECKOUT" cat-file -e "${MANIFEST_BASELINE}^{commit}" 2>/dev/null; then
    note "fetching ${VCPKG_GIT_URL}: the checkout does not hold ${MANIFEST_BASELINE}"
    git -C "$VCPKG_CHECKOUT" fetch -- "$VCPKG_GIT_URL" '+refs/heads/*:refs/remotes/origin/*'
    git -C "$VCPKG_CHECKOUT" cat-file -e "${MANIFEST_BASELINE}^{commit}" 2>/dev/null ||
      die "${MANIFEST_BASELINE} is not a commit of ${VCPKG_GIT_URL}"
  fi
}

# git checkout exits 0 with files it could not write; $1 says what just ran.
require_checkout_at_baseline() {
  local head
  head="$(checkout_head)"
  [[ "$head" == "$MANIFEST_BASELINE" ]] ||
    die "${VCPKG_CHECKOUT} is ${head} after the $1, not ${MANIFEST_BASELINE}"
  require_clean_checkout
}

move_checkout_to_baseline() {
  note "checking out ${MANIFEST_BASELINE} in ${VCPKG_CHECKOUT}"
  git -C "$VCPKG_CHECKOUT" checkout --quiet --detach "$MANIFEST_BASELINE"
  require_checkout_at_baseline checkout
}

bootstrap_vcpkg() {
  note "bootstrapping the vcpkg tool in ${VCPKG_CHECKOUT}"
  # Removed first, so a tool an earlier baseline left is not taken for new.
  rm -f "$VCPKG_TOOL"
  VCPKG_FORCE_SYSTEM_BINARIES=1 "${VCPKG_CHECKOUT}/bootstrap-vcpkg.sh" -disableMetrics ||
    die "the vcpkg bootstrap failed"
  [[ -f "$VCPKG_TOOL" && -x "$VCPKG_TOOL" ]] ||
    die "the vcpkg bootstrap left no executable ${VCPKG_TOOL}"
}

first_version_line() {
  local out
  out="$("$@" 2>/dev/null)" || return 1
  out="${out%%$'\n'*}"
  [[ -n "$out" ]] || return 1
  printf '%s\n' "$out"
}

# Waited for in the background, so a signal is acted on while vcpkg runs.
run_vcpkg_install() {
  local triplet="$1" root="$2" access="$3" status=0 started="$SECONDS"
  shift 3
  VCPKG_BINARY_SOURCES="clear;files,${VCPKG_ARCHIVES},${access}" \
    VCPKG_FORCE_SYSTEM_BINARIES=1 \
    "$TIMEOUT_BIN" -k 30 "$VCPKG_BUILD_TIMEOUT" "$VCPKG_TOOL" install \
    "--vcpkg-root=${VCPKG_CHECKOUT}" \
    "--x-manifest-root=${REPO_ROOT}" \
    "--x-install-root=${root}" \
    "--triplet=${triplet}" \
    "--host-triplet=${triplet}" \
    "--x-feature=${VCPKG_FEATURE}" \
    "$@" &
  wait "$!" || status=$?
  # 137 is vcpkg killed by timeout after the bound, or killed by the system
  # before it; the clock tells them apart.
  if [[ "$status" -eq 137 && "$((SECONDS - started))" -ge "$VCPKG_BUILD_TIMEOUT" ]]; then
    status=124
  fi
  return "$status"
}

warm_vcpkg_cache() {
  local triplet="$1" status=0
  mkdir -p "$VCPKG_ARCHIVES"
  new_throwaway_root
  note "building ${VCPKG_FEATURE} for ${triplet} into ${VCPKG_ARCHIVES} (bound: ${VCPKG_BUILD_TIMEOUT}s)"
  run_vcpkg_install "$triplet" "$THROWAWAY_ROOT" readwrite || status=$?
  [[ "$status" -ne 124 ]] ||
    die "vcpkg did not finish within ${VCPKG_BUILD_TIMEOUT}s and was stopped"
  [[ "$status" -eq 0 ]] || die "the vcpkg build failed (exit ${status})"
}

# The build exiting 0 does not show that the cache holds every package.
prove_vcpkg_cache() {
  local triplet="$1" status=0
  PROOF_FAILURE=""
  new_throwaway_root
  note "proving the binary cache: restoring every package from it with building disabled"
  run_vcpkg_install "$triplet" "$THROWAWAY_ROOT" read --only-binarycaching || status=$?
  [[ "$status" -ne 0 ]] || return 0
  if [[ "$status" -eq 124 ]]; then
    PROOF_FAILURE="vcpkg did not finish within ${VCPKG_BUILD_TIMEOUT}s and was stopped"
  else
    PROOF_FAILURE="the binary-only install failed (exit ${status}): a package of ${VCPKG_FEATURE} for ${triplet} is missing from the cache, or vcpkg could not run or write under ${VCPKG_CACHE_DIR}; see its output above"
  fi
  return 1
}

write_vcpkg_stamp() {
  local triplet="$1" tool_version="$2" compiler="$3" listing="$4" tmp
  tmp="$(mktemp "${VCPKG_STAMP}.XXXXXX")" || die "cannot write under ${VCPKG_CACHE_DIR}"
  THROWAWAY_PATHS+=("$tmp")
  {
    printf 'baseline=%s\n' "$MANIFEST_BASELINE"
    printf 'dependencies_sha256=%s\n' "$DEPENDENCIES_SHA256"
    printf 'triplet=%s\n' "$triplet"
    printf 'feature=%s\n' "$VCPKG_FEATURE"
    printf 'vcpkg_version=%s\n' "$tool_version"
    printf 'compiler=%s\n' "$compiler"
    printf 'provisioned_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'archives_sha256=%s\n' "$listing"
  } >"$tmp"
  mv -f "$tmp" "$VCPKG_STAMP"
}

# A failed proof shows the stamp is wrong; an empty stamp is not ready either.
check_vcpkg_cache() {
  local triplet="$1" outcome
  vcpkg_ready || die "the runner's vcpkg is not ready for this checkout: ${NOT_READY_REASON}"
  take_provisioning_lock
  forget_git_environment
  require_clean_checkout
  prove_vcpkg_cache "$triplet" || {
    if rm -f "$VCPKG_STAMP"; then
      outcome="Removed the stamp at ${VCPKG_STAMP}"
    elif { true >"$VCPKG_STAMP"; } 2>/dev/null; then
      outcome="Could not remove the stamp at ${VCPKG_STAMP} and emptied it instead"
    else
      die "${PROOF_FAILURE}. The stamp at ${VCPKG_STAMP} could be neither removed nor emptied, so status and vcpkg-env still report the runner as ready. It is not: remove the stamp by hand"
    fi
    die "${PROOF_FAILURE}. ${outcome}: the runner is no longer reported as ready until provision-vcpkg succeeds again"
  }
  note "the vcpkg checkout and binary cache are ready for ${MANIFEST_BASELINE} (${triplet})"
}

cmd_provision_vcpkg() {
  local mode="provision" triplet compiler tool_version started_sha256 listing
  case "$#:${1:-}" in
    0:) ;;
    1:--check) mode="check" ;;
    *) die "provision-vcpkg takes no argument other than --check" ;;
  esac
  [[ -n "$SCRIPT_PATH" ]] || die "$NO_CHECKOUT"
  require_runner_account "$@"
  plain_vcpkg_paths || die "$PLAIN_PATHS_RULE"
  [[ -x "$TIMEOUT_BIN" ]] || die "timeout not found at $TIMEOUT_BIN"
  [[ -x "$FLOCK_BIN" ]] || die "flock not found at $FLOCK_BIN"
  [[ "$VCPKG_BUILD_TIMEOUT" =~ ^[1-9][0-9]*$ ]] ||
    die "TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT must be a whole number of seconds above zero"
  load_manifest || die "$MANIFEST_ERROR"
  triplet="$(host_triplet)" || die "this machine has no vcpkg triplet (expected aarch64 or x86_64)"
  trap remove_throwaway_paths EXIT
  trap 'stop_on_signal INT 130' INT
  trap 'stop_on_signal TERM 143' TERM
  trap 'stop_on_signal HUP 129' HUP

  if [[ "$mode" == "check" ]]; then
    check_vcpkg_cache "$triplet"
    return
  fi

  compiler="$(first_version_line "${CXX:-c++}" --version)" ||
    die "no C++ compiler: '${CXX:-c++} --version' failed"
  started_sha256="$DEPENDENCIES_SHA256"
  take_provisioning_lock
  note "vcpkg baseline ${MANIFEST_BASELINE} from ${MANIFEST}"
  forget_git_environment
  prepare_vcpkg_checkout
  # The checkout, tool and cache change from here: an earlier stamp is stale.
  rm -f "$VCPKG_STAMP"
  move_checkout_to_baseline
  bootstrap_vcpkg
  tool_version="$(first_version_line "$VCPKG_TOOL" version)" ||
    die "cannot read the version of ${VCPKG_TOOL}"
  warm_vcpkg_cache "$triplet"
  prove_vcpkg_cache "$triplet" || die "$PROOF_FAILURE"
  listing="$(cache_listing_sha256)" || die "cannot list the binary cache at ${VCPKG_ARCHIVES}"
  load_manifest || die "$MANIFEST_ERROR"
  [[ "$DEPENDENCIES_SHA256" == "$started_sha256" ]] ||
    die "${MANIFEST} changed while the cache was being built; run provision-vcpkg again"
  require_checkout_at_baseline build
  write_vcpkg_stamp "$triplet" "$tool_version" "$compiler" "$listing"
  note "provisioned: vcpkg ${MANIFEST_BASELINE} with ${VCPKG_FEATURE} for ${triplet}"
}

# Not as root: git obeys the configuration of the repository it inspects.
cmd_vcpkg_env() {
  [[ "$#" -eq 0 ]] || die "vcpkg-env takes no arguments"
  [[ "${EUID}" -ne 0 ]] ||
    die "vcpkg-env asks git about the vcpkg checkout and does not do that as root; run it as the runner account"
  vcpkg_ready ||
    die "the runner's vcpkg is not ready for this checkout: ${NOT_READY_REASON}"
  forget_git_environment
  require_clean_checkout
  printf 'VCPKG_ROOT=%s\n' "$VCPKG_CHECKOUT"
  printf 'VCPKG_BINARY_SOURCES=clear;files,%s,read\n' "$VCPKG_ARCHIVES"
  printf 'VCPKG_FORCE_SYSTEM_BINARIES=1\n'
}

# Counted from /, as in cache_listing_sha256.
count_cache_archives() {
  local count
  if known_missing "$VCPKG_ARCHIVES"; then
    printf '0\n'
  elif count="$(cd / && find "${VCPKG_ARCHIVES}/" -type f 2>/dev/null | wc -l)"; then
    printf '%s\n' "$((count))"
  else
    printf 'unreadable\n'
  fi
}

print_vcpkg_status() {
  local head key value
  printf 'vcpkg_root: %s\n' "$VCPKG_CHECKOUT"
  if load_manifest; then
    printf 'vcpkg_baseline: %s\n' "$MANIFEST_BASELINE"
  else
    printf 'vcpkg_baseline: unreadable (%s)\n' "$MANIFEST_ERROR"
  fi
  head="$(checkout_head)"
  printf 'vcpkg_commit: %s\n' "$head"
  if [[ "$head" == "absent" ]]; then
    printf 'vcpkg_checkout: absent\n'
  elif [[ -z "$MANIFEST_BASELINE" || "$head" == "unreadable" ]]; then
    # Nothing to compare on one side or the other; `stale` would be a guess.
    printf 'vcpkg_checkout: unknown\n'
  elif [[ "$head" == "$MANIFEST_BASELINE" ]]; then
    printf 'vcpkg_checkout: current\n'
  else
    printf 'vcpkg_checkout: stale\n'
  fi
  if [[ -f "$VCPKG_TOOL" && -x "$VCPKG_TOOL" ]]; then
    printf 'vcpkg_tool: present\n'
  elif [[ -e "$VCPKG_TOOL" ]] || known_missing "$VCPKG_TOOL"; then
    printf 'vcpkg_tool: absent\n'
  else
    printf 'vcpkg_tool: unknown\n'
  fi
  printf 'vcpkg_binary_cache: %s\n' "$VCPKG_ARCHIVES"
  printf 'vcpkg_cache_archives: %s\n' "$(count_cache_archives)"
  if vcpkg_ready; then
    printf 'vcpkg_cache_ready: yes\n'
  else
    printf 'vcpkg_cache_ready: no (%s)\n' "$NOT_READY_REASON"
  fi
  if [[ -f "$VCPKG_STAMP" ]]; then
    # Root prints a stamp the runner account wrote: no control bytes.
    for key in baseline triplet compiler provisioned_at; do
      value="$(stamp_value "$key")" || value="unreadable"
      printf 'vcpkg_stamp_%s: %s\n' "$key" "${value//[^[:print:]]/?}"
    done
  fi
}

main() {
  local command="${1:-}"
  case "$command" in
    on) cmd_on ;;
    off) cmd_off ;;
    status)
      cmd_status
      print_vcpkg_status
      ;;
    provision-vcpkg)
      shift
      cmd_provision_vcpkg "$@"
      ;;
    vcpkg-env)
      shift
      cmd_vcpkg_env "$@"
      ;;
    -h|--help|help) usage ;;
    "") usage; exit 1 ;;
    *) usage >&2; die "unknown command: $command" ;;
  esac
}

main "$@"
