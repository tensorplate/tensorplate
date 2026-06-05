#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Operator helper for the Jetson GitHub Actions release runner.
#
# This script intentionally contains no GitHub registration token, password,
# repository secret, or private network address. It only toggles a previously
# configured self-hosted runner service and its temporary release sudoers
# allowance.

set -Eeuo pipefail

readonly DEFAULT_RUNNER_USER="gha-runner"
readonly DEFAULT_RUNNER_SERVICE="actions.runner.tensorplate-tensorplate.ubuntu.service"
readonly DEFAULT_REQUIRED_LABELS="self-hosted,linux,ARM64,tensorplate-release"

RUNNER_USER="${TP_JETSON_RUNNER_USER:-$DEFAULT_RUNNER_USER}"
RUNNER_DIR="${TP_JETSON_RUNNER_DIR:-/home/${RUNNER_USER}/actions-runner}"
RUNNER_SERVICE="${TP_JETSON_RUNNER_SERVICE:-$DEFAULT_RUNNER_SERVICE}"
SUDOERS_FILE="${TP_JETSON_RUNNER_SUDOERS_FILE:-/etc/sudoers.d/gha-runner-tensorplate-release}"
REQUIRED_LABELS="${TP_JETSON_RUNNER_LABELS:-$DEFAULT_REQUIRED_LABELS}"
CARGO_BIN="${TP_JETSON_RUNNER_CARGO_BIN:-/home/${RUNNER_USER}/.cargo/bin}"

APT_GET="${TP_JETSON_RUNNER_APT_GET:-/usr/bin/apt-get}"
INSTALL="${TP_JETSON_RUNNER_INSTALL:-/usr/bin/install}"
VISUDO="${TP_JETSON_RUNNER_VISUDO:-/usr/sbin/visudo}"

usage() {
  cat <<EOF
Usage:
  jetson-runner-control.sh on
  jetson-runner-control.sh off
  jetson-runner-control.sh status

Controls the TensorPlate Jetson self-hosted release runner.

Environment overrides:
  TP_JETSON_RUNNER_USER          default: ${DEFAULT_RUNNER_USER}
  TP_JETSON_RUNNER_DIR           default: /home/\$TP_JETSON_RUNNER_USER/actions-runner
  TP_JETSON_RUNNER_SERVICE       default: ${DEFAULT_RUNNER_SERVICE}
  TP_JETSON_RUNNER_SUDOERS_FILE  default: /etc/sudoers.d/gha-runner-tensorplate-release
  TP_JETSON_RUNNER_LABELS        default: ${DEFAULT_REQUIRED_LABELS}
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

write_sudoers() {
  local tmp
  [[ -x "$VISUDO" ]] || die "visudo not found at $VISUDO"
  [[ -x "$APT_GET" ]] || die "apt-get not found at $APT_GET"
  [[ -x "$INSTALL" ]] || die "install not found at $INSTALL"

  tmp="$(mktemp)"
  {
    printf '# Managed by TensorPlate jetson-runner-control.sh\n'
    printf '# Temporary release-build allowance for the Jetson self-hosted runner.\n'
    printf '%s ALL=(root) NOPASSWD: %s, %s\n' "$RUNNER_USER" "$APT_GET" "$INSTALL"
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
  cmd_status
}

print_sudoers_status() {
  if [[ -f "$SUDOERS_FILE" ]]; then
    printf 'sudoers: enabled (%s)\n' "$SUDOERS_FILE"
    if [[ "${EUID}" -eq 0 ]]; then
      "$VISUDO" -cf "$SUDOERS_FILE" >/dev/null &&
        printf 'sudoers_valid: yes\n'
      if sudo -u "$RUNNER_USER" sudo -n "$APT_GET" --version >/dev/null 2>&1; then
        printf 'runner_can_sudo_apt_get: yes\n'
      else
        printf 'runner_can_sudo_apt_get: no\n'
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

main() {
  local command="${1:-}"
  case "$command" in
    on) cmd_on ;;
    off) cmd_off ;;
    status) cmd_status ;;
    -h|--help|help) usage ;;
    "") usage; exit 1 ;;
    *) usage >&2; die "unknown command: $command" ;;
  esac
}

main "$@"
