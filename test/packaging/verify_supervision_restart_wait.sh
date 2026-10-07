#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Replays the supervision contract test's crash-recovery check against a
# fake systemctl. The real check runs right after SIGKILL, and systemd may
# not have noticed the death yet when it first looks; a slow runner hits
# that window by chance, this replays it every time. Needs no root and no
# systemd.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
script="${repo_root}/test/packaging/verify_service_supervision.sh"
bash -n "$script"

fail=0
failure() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

# The recovery check is called right after the kill, so the functions
# extracted below are the ones the real run executes.
grep -A1 '^kill -9 "[$]first_pid"$' "$script" | grep -q 'check_crash_recovery "[$]first_pid"' ||
  failure "the crash case does not call check_crash_recovery right after the kill"

# Every multi-line function the script defines; the one-line die/note/pass
# helpers are replaced below so a failure returns instead of exiting.
functions="$(sed -n '/^[a-z_]*() {$/,/^}$/p' "$script")"
[[ -n "$functions" ]] || { failure "no functions extracted from ${script}"; exit 1; }

td="$(mktemp -d)"
trap 'rm -rf "$td"' EXIT
fakebin="${td}/bin"
mkdir -p "$fakebin"

# `systemctl show -p PROP --value UNIT` answers one line per call from the
# file PROP under TP_FAKE_ANSWERS, repeating the last line once the file
# runs out; any other invocation is refused.
cat >"${fakebin}/systemctl" <<'FAKE'
#!/bin/sh
[ "${1-}" = show ] && [ "${2-}" = -p ] && [ "${4-}" = --value ] && [ -n "${5-}" ] || exit 9
file="${TP_FAKE_ANSWERS}/$3"
[ -f "$file" ] || exit 1
n=$(cat "${file}.n" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" >"${file}.n"
total=$(wc -l <"$file")
[ "$n" -gt "$total" ] && n=$total
sed -n "${n}p" "$file"
FAKE
printf '#!/bin/sh\nexit 0\n' >"${fakebin}/journalctl"
chmod 0755 "${fakebin}/systemctl" "${fakebin}/journalctl"

# run_case NAME WANT-STATUS RESTART_WAIT ANSWERS...   (ANSWERS: PROP=line;line;...)
# Sets `out` and `status`.
run_case() {
  local name="$1" want="$2" wait="$3" answers spec prop
  shift 3
  answers="${td}/${name}"
  rm -rf "$answers"
  mkdir -p "$answers"
  for spec in "$@"; do
    prop="${spec%%=*}"
    printf '%s\n' "${spec#*=}" | tr ';' '\n' >"${answers}/${prop}"
  done
  status=0
  out="$(
    cd "$td" &&
    PATH="${fakebin}:${PATH}" TP_FAKE_ANSWERS="$answers" bash -c '
      set -Eeuo pipefail
      die() { printf "FAIL: %s\n" "$*" >&2; exit 1; }
      note() { :; }
      pass() { printf "PASS: %s\n" "$*"; }
      AGENT_UNIT=tensorplate-agent.service
      RESTART_WAIT='"$wait"'
      '"$functions"'
      check_crash_recovery 4242
    ' 2>&1
  )" || status=$?
  if [[ "$status" -ne "$want" ]]; then
    failure "${name}: exit ${status}, expected ${want}; output: ${out}"
  fi
}

# The race: the first look lands before systemd has processed the death,
# the second after the restart.
run_case race 0 10 \
  'ActiveState=active;active;active' 'MainPID=0;4243' 'NRestarts=0;1'
[[ "$out" == *'PASS: recovered as PID 4243 (NRestarts=1)'* ]] ||
  failure "race: the restart was not reported (output: ${out})"

# The same, with the restart only visible a few polls later.
run_case late-restart 0 10 \
  'ActiveState=active;activating;activating;active' 'MainPID=0;0;0;4250' 'NRestarts=0;0;0;1'
[[ "$out" == *'PASS: recovered as PID 4250 (NRestarts=1)'* ]] ||
  failure "late-restart: the restart was not reported (output: ${out})"

# No restart inside the bound: the failure must name what was seen.
SECONDS=0
run_case never-restarted 1 2 'ActiveState=failed' 'MainPID=0' 'NRestarts=0'
elapsed=$SECONDS
[[ "$out" == *'NRestarts=0'* && "$out" == *'ActiveState=failed'* ]] ||
  failure "never-restarted: the failure does not name the state seen (output: ${out})"
(( elapsed <= 6 )) || failure "never-restarted: took ${elapsed}s against a bound of 2"

# Active with a new process but never restarted by systemd: not a recovery.
run_case replaced-not-restarted 1 2 'ActiveState=active' 'MainPID=4300' 'NRestarts=0'
[[ "$out" == *'NRestarts=0'* ]] ||
  failure "replaced-not-restarted: the failure does not name NRestarts (output: ${out})"

# Restart counted but the main process is the one that was killed.
run_case same-pid 1 2 'ActiveState=active' 'MainPID=4242' 'NRestarts=1'
[[ "$out" == *'MainPID unchanged'* ]] ||
  failure "same-pid: the unchanged pid was not refused (output: ${out})"

# systemctl answering nothing is not a recovery.
run_case show-fails 1 2 'ActiveState=active' 'MainPID=4243'
[[ "$out" != *'PASS:'* ]] || failure "show-fails: passed with no NRestarts answer (output: ${out})"

if ((fail)); then
  printf 'verify_supervision_restart_wait: FAIL\n' >&2
  exit 1
fi
printf 'verify_supervision_restart_wait: ok\n'
