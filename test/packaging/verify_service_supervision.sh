#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Drives the shipped systemd units against a real systemd and asserts the
# supervision contract the units promise:
#
#   1. the agent reaches active and its logs land at the documented path,
#   2. a hard crash is recovered (Restart=on-failure),
#   3. a crash LOOP is given up on rather than restarted forever
#      (StartLimitBurst/StartLimitIntervalSec),
#   4. a clean stop is not treated as a failure and is not restarted,
#   5. observability runs independently of the agent, and
#   6. no serving unit is registered — the agent owns the worker.
#
# The units are architecture-independent, so this exercises the same contract
# on every runner. It is the parity claim's teeth: verify_systemd_units.sh
# reads the unit files as text, this one observes what systemd does with them.
#
# THIS SCRIPT MUTATES THE HOST (creates a system user, writes into
# /etc/systemd/system, /usr/bin, /etc/tensorplate, /var/lib/tensorplate).
# Run it only on a disposable host: the CI runner or a container. It refuses
# unless CI=true or TP_SUPERVISION_ALLOW=1.
#
# Binaries are stubbed. This rehearses the unit's supervision behavior, not
# the agent's own logic — the agent's in-process worker supervision is its
# own test tier.

set -Eeuo pipefail

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
note() { printf '==> %s\n' "$*"; }
pass() { printf 'PASS: %s\n' "$*"; }

[[ "${CI:-}" == "true" || "${TP_SUPERVISION_ALLOW:-0}" == "1" ]] ||
  die "this test installs system units; run on a disposable host with TP_SUPERVISION_ALLOW=1"
[[ "$(id -u)" -eq 0 ]] || die "run as root (systemd and user creation)"

command -v systemctl >/dev/null 2>&1 || die "systemctl is required"
systemctl is-system-running >/dev/null 2>&1 ||
  note "systemd reports a degraded state; continuing (unit behavior is still observable)"
[[ -d /run/systemd/system ]] || die "systemd is not the init system on this host"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git repository"
cd "$repo_root"
. packaging/scripts/path-constants.sh

AGENT_UNIT="tensorplate-agent.service"
OBS_UNIT="tensorplate-observability.service"
MODE_FILE="${TP_STATE_DIR}/supervision-stub-mode"
# RestartSec=5, so a restart cycle needs >5s; keep waits generously above it.
RESTART_WAIT=45
STOP_WAIT=30

cleanup() {
  systemctl stop "$AGENT_UNIT" "$OBS_UNIT" >/dev/null 2>&1 || true
  systemctl reset-failed "$AGENT_UNIT" "$OBS_UNIT" >/dev/null 2>&1 || true
  rm -f "/etc/systemd/system/${AGENT_UNIT}" "/etc/systemd/system/${OBS_UNIT}"
  systemctl daemon-reload >/dev/null 2>&1 || true
  rm -f /usr/bin/tensorplate-agent /usr/bin/tensorplate-observability "$MODE_FILE"
}
trap cleanup EXIT

# Wait for a path to appear. Type=simple reports `active` as soon as systemd
# has forked ExecStart, which is BEFORE the process runs its first line — so
# anything the service itself produces has to be waited for, not sampled.
await_file() {
  local path="$1" deadline="$2" i
  for ((i = 0; i < deadline; i++)); do
    [[ -e "$path" ]] && return 0
    sleep 1
  done
  return 1
}

# Wait until `systemctl show -p <prop>` reports one of the given values.
await_property() {
  local unit="$1" prop="$2" deadline="$3"; shift 3
  local want=("$@") observed i w
  for ((i = 0; i < deadline; i++)); do
    observed="$(systemctl show -p "$prop" --value "$unit" 2>/dev/null || echo '')"
    for w in "${want[@]}"; do
      [[ "$observed" == "$w" ]] && { printf '%s' "$observed"; return 0; }
    done
    sleep 1
  done
  printf '%s' "$observed"
  return 1
}

note "staging users, directories, and stub binaries"
packaging/scripts/create-users.sh
packaging/scripts/install-paths.sh
for cfg in packaging/conf/agent.json packaging/conf/observability.json; do
  install -m 0640 -o "$TP_SYSTEM_USER" -g "$TP_SYSTEM_GROUP" "$cfg" \
    "${TP_ETC_DIR}/$(basename "$cfg")"
done

# One stub serves both units. It reads its behavior from a file inside the
# unit's ReadWritePaths, so the test can flip crash/run without touching the
# unit or reinstalling — and writing there also exercises ProtectSystem=strict.
for binary in tensorplate-agent tensorplate-observability; do
  cat >"/usr/bin/${binary}" <<STUB
#!/bin/sh
mode="\$(cat "${MODE_FILE}" 2>/dev/null || echo run)"
printf '%s ${binary} start mode=%s\n' "\$(date -u +%H:%M:%S)" "\$mode" \
  >> "${TP_LOG_DIR}/supervision.log"
case "\$mode" in
  crash) exit 7 ;;
  *) exec sleep 3600 ;;
esac
STUB
  chmod 0755 "/usr/bin/${binary}"
done
printf 'run\n' > "$MODE_FILE"
chown "$TP_SYSTEM_USER:$TP_SYSTEM_GROUP" "$MODE_FILE"
chmod 0640 "$MODE_FILE"

install -m 0644 "packaging/debian/${AGENT_UNIT}" "/etc/systemd/system/${AGENT_UNIT}"
install -m 0644 "packaging/debian/${OBS_UNIT}" "/etc/systemd/system/${OBS_UNIT}"
systemctl daemon-reload

note "1. the agent reaches active and logs to the documented path"
systemctl start "$AGENT_UNIT"
state="$(await_property "$AGENT_UNIT" ActiveState 30 active)" ||
  die "agent did not reach active (state=${state}); $(systemctl status "$AGENT_UNIT" --no-pager 2>&1 | tail -5)"
first_pid="$(systemctl show -p MainPID --value "$AGENT_UNIT")"
[[ -n "$first_pid" && "$first_pid" != "0" ]] || die "agent has no MainPID while active"
if ! await_file "${TP_LOG_DIR}/supervision.log" 20; then
  printf 'ls -la %s:\n' "$TP_LOG_DIR" >&2
  ls -la "$TP_LOG_DIR" >&2 || true
  printf 'journal for %s:\n' "$AGENT_UNIT" >&2
  journalctl -u "$AGENT_UNIT" --no-pager -n 30 >&2 || true
  die "no log written under ${TP_LOG_DIR}; LogsDirectory= must make the documented path writable by the service"
fi
pass "agent active as PID ${first_pid}, logs at ${TP_LOG_DIR}"

note "2. a hard crash is recovered"
kill -9 "$first_pid"
# Wait for systemd to notice, back off RestartSec, and come back up.
state="$(await_property "$AGENT_UNIT" ActiveState "$RESTART_WAIT" active)" ||
  die "agent did not recover from SIGKILL (state=${state}); Restart=on-failure must recover a hard crash"
second_pid="$(systemctl show -p MainPID --value "$AGENT_UNIT")"
[[ "$second_pid" != "$first_pid" ]] ||
  die "MainPID unchanged after SIGKILL; the unit was not actually restarted"
restarts="$(systemctl show -p NRestarts --value "$AGENT_UNIT")"
[[ "${restarts:-0}" -ge 1 ]] || die "NRestarts=${restarts} after a crash; expected at least 1"
pass "recovered as PID ${second_pid} (NRestarts=${restarts})"

note "3. a crash loop is given up on, not restarted forever"
printf 'crash\n' > "$MODE_FILE"
systemctl restart "$AGENT_UNIT" >/dev/null 2>&1 || true
# The contract is that repeated crashes stop being retried, so assert THAT
# rather than a particular systemd Result string: wait until the restart count
# stops growing across a window longer than RestartSec while the unit is not
# active. A unit that is still looping keeps incrementing NRestarts.
#
# Note this cannot poll ActiveState=failed: a crashing unit passes through
# failed after EVERY attempt before backing off and trying again, so a bare
# state check reports success while the loop is still running.
settled=0
last_restarts=-1
for _ in $(seq 1 40); do
  state="$(systemctl show -p ActiveState --value "$AGENT_UNIT")"
  restarts="$(systemctl show -p NRestarts --value "$AGENT_UNIT")"
  if [[ "$state" != "active" && "$state" != "activating" && "$restarts" == "$last_restarts" ]]; then
    settled=1
    break
  fi
  last_restarts="$restarts"
  sleep 7  # longer than RestartSec=5, so a live loop advances the counter
done
if ((settled != 1)); then
  printf 'ActiveState=%s Result=%s NRestarts=%s\n' \
    "$(systemctl show -p ActiveState --value "$AGENT_UNIT")" \
    "$(systemctl show -p Result --value "$AGENT_UNIT")" \
    "$(systemctl show -p NRestarts --value "$AGENT_UNIT")" >&2
  printf 'StartLimitBurst=%s StartLimitIntervalUSec=%s RestartSec=%s\n' \
    "$(systemctl show -p StartLimitBurst --value "$AGENT_UNIT")" \
    "$(systemctl show -p StartLimitIntervalUSec --value "$AGENT_UNIT")" \
    "$(systemctl show -p RestartUSec --value "$AGENT_UNIT")" >&2
  journalctl -u "$AGENT_UNIT" --no-pager -n 40 >&2 || true
  die "the agent kept restarting after repeated crashes; the start limit must stop the loop"
fi
state="$(systemctl show -p ActiveState --value "$AGENT_UNIT")"
result="$(systemctl show -p Result --value "$AGENT_UNIT")"
[[ "$state" == "failed" ]] ||
  die "expected the given-up unit to be failed, got ${state} (Result=${result})"
pass "crash loop stopped at NRestarts=${last_restarts} (state=${state}, Result=${result})"

note "4. a clean stop is not a failure and is not restarted"
printf 'run\n' > "$MODE_FILE"
systemctl reset-failed "$AGENT_UNIT"
systemctl start "$AGENT_UNIT"
await_property "$AGENT_UNIT" ActiveState 30 active >/dev/null ||
  die "agent did not restart after reset-failed"
systemctl stop "$AGENT_UNIT"
state="$(await_property "$AGENT_UNIT" ActiveState "$STOP_WAIT" inactive)" ||
  die "agent did not stop cleanly (state=${state})"
sleep 8  # longer than RestartSec; a clean stop must not come back
state="$(systemctl show -p ActiveState --value "$AGENT_UNIT")"
[[ "$state" == "inactive" ]] ||
  die "agent came back after a clean stop (state=${state}); Restart=on-failure must not restart a requested stop"
pass "clean stop stayed stopped"

note "5. observability runs independently of the agent"
systemctl start "$OBS_UNIT"
await_property "$OBS_UNIT" ActiveState 30 active >/dev/null ||
  die "observability did not reach active with the agent stopped; it must not depend on the agent"
obs_pid="$(systemctl show -p MainPID --value "$OBS_UNIT")"
systemctl start "$AGENT_UNIT"
await_property "$AGENT_UNIT" ActiveState 30 active >/dev/null || die "agent did not start"
systemctl stop "$AGENT_UNIT"
await_property "$AGENT_UNIT" ActiveState "$STOP_WAIT" inactive >/dev/null || die "agent did not stop"
state="$(systemctl show -p ActiveState --value "$OBS_UNIT")"
[[ "$state" == "active" ]] ||
  die "observability went ${state} when the agent stopped; the two must be independent"
[[ "$(systemctl show -p MainPID --value "$OBS_UNIT")" == "$obs_pid" ]] ||
  die "observability was restarted by the agent's lifecycle"
pass "observability survived the agent stopping"

note "6. the serving worker has no unit"
if systemctl list-unit-files 'tensorplate-serving*' 2>/dev/null | grep -q 'tensorplate-serving'; then
  die "a tensorplate-serving unit is registered; the agent owns the worker lifecycle"
fi
pass "no serving unit registered"

printf 'verify_service_supervision: ok\n'
