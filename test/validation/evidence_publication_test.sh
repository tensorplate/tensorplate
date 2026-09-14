#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The evidence publication scanner.
#
# Both directions matter. A bundle that uses only the self-describing
# synthetic values must pass, so every allowlist entry is exercised by the
# passing bundle and removing one fails it. Each identifier class must
# fail with exactly one injected value, and the value must never appear in
# the scanner's output, because CI logs outlive a history rewrite.
#
# Every injected value is generated at runtime, so this file carries
# nothing that looks like a live identifier and no planning identifier.
# The passing report comes from the real lifecycle runner, so a report
# shape the producer emits is the shape that is scanned.

set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scanner="${repo_root}/tools/validation/check-evidence-publication.sh"
runner="${repo_root}/tools/validation/lifecycle-stages.sh"
failures=0
case_count=0
d=""

unset TP_LIFECYCLE_SOURCE_REVISION TP_EVIDENCE_REPO_ROOT

die() {
  printf 'evidence_publication_test: %s\n' "$1" >&2
  exit 2
}

check() {
  local what="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    printf '  ok   %s\n' "$what"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$what" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

work="$(mktemp -d)" || die "mktemp failed"
# chmod first: the unreadable-input cases leave mode 000 entries behind
# when a check fails before restoring them.
trap 'chmod -R u+rwx "$work" 2>/dev/null; rm -rf "$work"' EXIT

random_word() {
  python3 -c 'import secrets, string, sys
print("".join(secrets.choice(string.ascii_lowercase) for _ in range(int(sys.argv[1]))))' "$1"
}
random_hex() {
  python3 -c 'import secrets, sys; print(secrets.token_hex(int(sys.argv[1])))' "$1"
}
random_alnum() {
  python3 -c 'import secrets, string, sys
print("".join(secrets.choice(string.ascii_letters + string.digits) for _ in range(int(sys.argv[1]))))' "$1"
}
random_uuid() {
  python3 -c 'import uuid; print(uuid.uuid4())'
}
random_octet() {
  printf '%s' "$((1 + RANDOM % 254))"
}

host="vm-$(random_word 10)" || die "could not generate a host name"
account="op$(random_word 8)" || die "could not generate an account name"
id32="$(random_hex 16)" || die "could not generate an id"
bare_uuid="$(random_uuid)" || die "could not generate a uuid"
literal="lit$(random_word 9)" || die "could not generate a literal"
short_literal="qz7"

# scan <out> <scanner args...>: prints the scanner's exit status.
scan() {
  local out="$1" status=0
  shift
  "$scanner" "$@" >"$out" 2>&1 || status=$?
  printf '%s' "$status"
}

has() {
  if grep -qF -- "$1" "$2"; then echo yes; else echo no; fi
}

# A fresh copy of the passing bundle in $d.
new_case() {
  case_count=$((case_count + 1))
  d="${work}/case-${case_count}"
  cp -R "$pass" "$d" || die "could not copy the passing bundle"
}

add_line() {
  printf '%s\n' "$1" >>"${d}/notes.log" || die "could not write notes.log"
}

# expect_finding <what> <class> <value> [scanner args...]: scans $d. An
# empty value is for findings about a file rather than a value in it.
expect_finding() {
  local what="$1" class="$2" value="$3" out="${d}.out"
  shift 3
  if [[ $# -eq 0 ]]; then
    set -- --patterns-only
  fi
  check "$what is a finding" "1" "$(scan "$out" "$@" "$d")"
  check "  classed ${class}" "yes" "$(has ": ${class}" "$out")"
  if [[ -n "$value" ]]; then
    check "  without printing the value" "no" "$(has "$value" "$out")"
  fi
}

# journal_record <file> <json object>: appends one JSON Lines record.
journal_record() {
  printf '%s\n' "$2" >>"$1" || die "could not append a journal record"
}

# byte_message_record <file> <text>: a journal record whose MESSAGE is
# the integer-array form journalctl uses for non-printable values.
byte_message_record() {
  python3 - "$1" "$2" <<'PY' || die "could not append a byte-array journal record"
import json, sys
record = {
    "MESSAGE": list((sys.argv[2] + "\x1b[0m").encode()),
    "_PID": "42",
    "_SYSTEMD_UNIT": "tensorplate-agent.service",
}
with open(sys.argv[1], "a") as handle:
    handle.write(json.dumps(record) + "\n")
PY
}

# shellcheck disable=SC2329 # Invoked by lifecycle_stage in produce_report.
emit_install_log() {
  printf 'installing from /home/tp-synthetic-operator/assets\n'
  printf 'Sep 14 01:42:03 tp-synthetic-host systemd[1]: Started tensorplate-agent.service.\n'
}

# shellcheck disable=SC2329 # Invoked by lifecycle_stage in produce_report.
emit_failing_log() {
  local name="$1" n
  for n in 1 2 3; do
    printf 'Sep 14 01:42:0%s %s tensorplate-agent[42]: attempt %s refused\n' "$n" "$name" "$n"
  done
  return 1
}

# produce_report <dir> <failing host or empty>: the real runner's report.
produce_report() (
  local dir="$1" failing_host="$2" stage
  # shellcheck disable=SC1090
  source "$runner"
  lifecycle_begin synthetic-row "$dir" 0.2.1 test-harness
  trap 'lifecycle_abort $?' EXIT
  for stage in install upgrade deploy-smoke status-logs rollback restart crash-loop offline; do
    if [[ "$stage" == offline ]]; then
      lifecycle_skip offline "cloud detection needs metadata.google.internal at 169.254.169.254"
    elif [[ "$stage" == install && -n "$failing_host" ]]; then
      lifecycle_stage install emit_failing_log "$failing_host" || :
    elif [[ "$stage" == install ]]; then
      lifecycle_stage install emit_install_log
    else
      lifecycle_stage "$stage" true
    fi
  done
  lifecycle_finish
)

printf 'evidence publication scanner\n'

# --- The passing bundle: every synthetic form the scanner allows.
pass="${work}/pass"
produce_report "$pass" "" >/dev/null 2>&1 || die "the lifecycle runner failed"

zeros="00000000000000000000000000000000"
cat >"${pass}/host-facts.txt" <<EOF || die "could not write host-facts.txt"
Static hostname: tp-synthetic-host
Transient hostname: tp-synthetic-host
Pretty hostname: tp-synthetic-host
Machine ID: ${zeros}
Boot ID: ${zeros}
Linux tp-synthetic-host 6.8.0-1015-gcp #17-Ubuntu SMP PREEMPT_DYNAMIC x86_64 GNU/Linux
Darwin tp-synthetic-host 25.6.0 Darwin Kernel Version 25.6.0
-- Boot ${zeros} --
/var/log/journal/${zeros}/system.journal
Sep 14 01:42:03.123456 tp-synthetic-host tensorplate-agent[42]: short-precise
2026-09-14T01:42:03+0000 tp-synthetic-host tensorplate-agent[42]: short-iso
2026-09-14T01:42:03.123456+00:00 tp-synthetic-host tensorplate-agent[42]: short-iso-precise
Mon 2026-09-14 01:42:03 UTC tp-synthetic-host tensorplate-agent[42]: short-full
evidence under /Users/tp-synthetic-operator/evidence and /Users/Shared/tensorplate
GPU-00000000-0000-0000-0000-000000000001 MIG-00000000-0000-0000-0000-000000000002
bundle 00000000-0000-0000-0000-000000000003
machine type projects/REDACTED/zones/us-central1-a/machineTypes/g2-standard-8
metadata from metadata.google.internal
listening on 127.0.0.1:8080, 0.0.0.0:9090 and [::]:9091
documentation addresses 192.0.2.10 198.51.100.7 203.0.113.9 ::1 2001:db8::10
link/ether 00:00:5e:00:53:01 brd ff:ff:ff:ff:ff:ff
link/none 00:00:00:00:00:00
contacts ops@example.com ops@example.org ops@example.net
Serial Number: REDACTED
Hardware Serial: 0000000000
ii  tensorrt  10.3.0.30-1+cuda12.6  arm64
tensorrt==10.3.0.30
tensorrt-10.3.0.30-cp310-none-linux_aarch64.whl
nvidia_cudnn_cu12-9.3.0.75-py3-none-manylinux2014_aarch64.whl
/usr/lib/python3/dist-packages/tensorrt-10.3.0.30.dist-info/METADATA
firmware 36.4.3.1.2
EOF

cli_uuid="$(random_uuid)" || die "could not generate a uuid"
tx_uuid="$(random_uuid)" || die "could not generate a uuid"
deploy_uuid="$(random_uuid)" || die "could not generate a uuid"
cat >"${pass}/status.json" <<EOF || die "could not write status.json"
{
  "cli_request": "cli-${cli_uuid}",
  "transition": "tx-${tx_uuid}",
  "deployment": "deploy-${deploy_uuid}",
  "endpoint": "http://127.0.0.1:8080"
}
EOF

journal="${pass}/agent-journal.txt"
journal_record "$journal" "{\"MESSAGE\":\"Started tensorplate-agent.service.\",\"PRIORITY\":\"6\",\"SYSLOG_IDENTIFIER\":\"systemd\",\"UNIT\":\"tensorplate-agent.service\",\"_PID\":\"1\",\"_SYSTEMD_UNIT\":\"init.scope\",\"_SYSTEMD_INVOCATION_ID\":\"${zeros}\",\"__REALTIME_TIMESTAMP\":\"1757814123000000\"}"
byte_message_record "$journal" "ready on tp-synthetic-host"
printf 'nothing to see\n' >"${pass}/notes.log" || die "could not write notes.log"

literals_dir="${work}/private"
mkdir -p "$literals_dir" || die "could not create the literal directory"
printf '# operator literals\n\n%s\nnever-present-literal\n' "$short_literal" \
  >"${literals_dir}/literals.txt" || die "could not write the literal file"

check "the synthetic bundle passes --patterns-only" "0" \
  "$(scan "${work}/pass.out" --patterns-only "$pass")"
check "the synthetic bundle passes --literals" "0" \
  "$(scan "${work}/pass-literals.out" --literals "${literals_dir}/literals.txt" "$pass")"
check "  and says it scanned files" "yes" "$(has "publishable: " "${work}/pass-literals.out")"

# --- Journal fields.
new_case
journal_record "${d}/agent-journal.txt" "{\"MESSAGE\":\"x\",\"_PID\":\"42\",\"_HOSTNAME\":\"${host}\"}"
expect_finding "a journal record carrying _HOSTNAME" journal-field "$host"

new_case
code_file="src/$(random_word 12).c"
journal_record "${d}/agent-journal.txt" "{\"MESSAGE\":\"x\",\"_PID\":\"42\",\"CODE_FILE\":\"${code_file}\"}"
expect_finding "a journal record carrying CODE_FILE" journal-field "$code_file"

new_case
cursor="s=${id32};i=1"
# Pretty-printed, so only decoding the whole document can see the key.
printf '{\n  "__CURSOR": "%s"\n}\n' "$cursor" >"${d}/cursor.json" || die "could not write cursor.json"
expect_finding "a non-journal object carrying __CURSOR" journal-field "$cursor"

for field in "_HOSTNAME=${host}" "_MACHINE_ID=${id32}" "_BOOT_ID=${id32}" "__CURSOR=s=${id32};i=1"; do
  new_case
  add_line "    ${field}"
  expect_finding "a verbose-format ${field%%=*} field" journal-field "${field#*=}"
done

# write_json <file> <mode> <record>: appends two journal records. Records:
# machine (carries _MACHINE_ID), bytes (a byte-array MESSAGE quoting an
# address) and code (carries CODE_FILE). Modes: pretty (jq's default
# output: concatenated indented records), prefixed (each record on a log
# line after other text), detail (records quoted inside one report string)
# and truncated (one record cut short, as a log tail can leave it).
write_json() {
  python3 - "$1" "$2" "$3" "$host" "$id32" "$json_ipv4" <<'PY' || die "could not write $1"
import json, sys
path, mode, kind, host, id32, ipv4 = sys.argv[1:]
record = {
    "machine": {"MESSAGE": "started", "_PID": "42", "_MACHINE_ID": id32},
    "bytes": {"MESSAGE": list(("peer " + ipv4 + " up").encode()), "_PID": "42"},
    "code": {"MESSAGE": "started", "_PID": "42", "CODE_FILE": "src/" + host + ".c"},
}[kind]
records = [record, record]
with open(path, "a") as handle:
    if mode == "pretty":
        handle.write("".join(json.dumps(r, indent=2) + "\n" for r in records))
    elif mode == "prefixed":
        handle.write("".join("record: " + json.dumps(r) + "\n" for r in records))
    elif mode == "detail":
        report = {"stages": [{"stage": "install", "detail": " ".join(json.dumps(r) for r in records)}]}
        handle.write(json.dumps(report, indent=2) + "\n")
    elif mode == "truncated":
        handle.write("tail: " + json.dumps(records[0])[:-12] + "\n")
PY
}
json_ipv4="198.18.$(random_octet).$(random_octet)"

new_case
write_json "${d}/agent-journal.txt" pretty machine
expect_finding "concatenated pretty records carrying _MACHINE_ID" journal-field "$id32"

new_case
write_json "${d}/agent-journal.txt" pretty bytes
expect_finding "concatenated pretty records with a byte-array MESSAGE" ipv4 "$json_ipv4"

new_case
write_json "${d}/agent-journal.txt" pretty code
expect_finding "concatenated pretty records carrying CODE_FILE" journal-field "$host"

new_case
write_json "${d}/notes.log" prefixed bytes
expect_finding "a record quoted after log text with a byte-array MESSAGE" ipv4 "$json_ipv4"

new_case
write_json "${d}/detail.json" detail bytes
expect_finding "a record quoted in a report string with a byte-array MESSAGE" ipv4 "$json_ipv4"

new_case
write_json "${d}/notes.log" truncated machine
expect_finding "a record cut short carrying _MACHINE_ID" journal-field "$id32"

# scan_within <seconds> <dir>: the --patterns-only exit status, or
# "timeout". The scanner runs in its own process group so a timeout stops
# its python as well as the wrapper.
scan_within() {
  python3 - "$scanner" "$2" "$1" <<'PY'
import os, signal, subprocess, sys
process = subprocess.Popen([sys.argv[1], "--patterns-only", sys.argv[2]],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           start_new_session=True)
try:
    print(process.wait(timeout=int(sys.argv[3])), end="")
except subprocess.TimeoutExpired:
    os.killpg(process.pid, signal.SIGKILL)
    process.wait()
    print("timeout", end="")
PY
}

# --- Decoded JSON members, field context and bounded decoding.
json_literal="json$(random_word 12)"
json_literal_file="${work}/private/json-literals.txt"
printf '%s\n' "$json_literal" >"$json_literal_file" || die "could not write JSON literals"

new_case
python3 - "${d}/duplicate.json" "$json_literal" <<'PY' || die "could not write duplicate JSON"
import json, sys
value = "".join("\\u%04x" % ord(char) for char in sys.argv[2])
with open(sys.argv[1], "w") as handle:
    handle.write('{"MESSAGE":"' + value + '","MESSAGE":"ready"}\n')
PY
expect_finding "an earlier duplicate JSON member retains its decoded literal" \
  "operator-literal literal #1" "$json_literal" --literals "$json_literal_file"

json_token="ghp_$(random_alnum 30)"
new_case
python3 - "${d}/duplicate.json" "$json_token" <<'PY' || die "could not write duplicate credential"
import sys
value = "\\u%04x" % ord(sys.argv[2][0]) + sys.argv[2][1:]
with open(sys.argv[1], "w") as handle:
    handle.write('{"MESSAGE":"' + value + '","MESSAGE":"ready"}\n')
PY
expect_finding "an earlier duplicate JSON member retains its decoded credential" credential "$json_token"

json_serial="SN$(random_alnum 14)"
new_case
python3 - "${d}/serial.json" "$json_serial" <<'PY' || die "could not write escaped serial"
import json, sys
with open(sys.argv[1], "w") as handle:
    handle.write('{\n  "ser\\u0069al":\n  ' + json.dumps(sys.argv[2]) + '\n}\n')
PY
expect_finding "an escaped JSON serial key retains its value context" serial "$json_serial"

new_case
python3 - "${d}/serial.json" <<'PY' || die "could not write numeric serial"
import secrets, sys
with open(sys.argv[1], "w") as handle:
    handle.write('{"ser\\u0069al":%d}\n' % (100000000 + secrets.randbelow(900000000)))
PY
expect_finding "a decoded numeric serial retains its field context" serial ""

new_case
python3 - "${d}/serial.json" "$json_serial" <<'PY' || die "could not write byte-array UDID"
import json, sys
with open(sys.argv[1], "w") as handle:
    handle.write('{"\\u0055DID":' + json.dumps(list(sys.argv[2].encode())) + '}\n')
PY
expect_finding "a decoded byte-array UDID retains its field context" serial "$json_serial"

# Padding shifts valid tokens across every initial and doubling-window
# boundary. Each independent record carries an escaped literal, so checking
# the finding count detects any record silently skipped during decoding.
new_case
python3 - "${d}/boundaries.json" "$json_literal" <<'PY' || die "could not write boundary records"
import json, sys
secret = "".join("\\u%04x" % ord(char) for char in sys.argv[2])
quoted = json.dumps('safe"\\\n\t\b\f\ré漢😀' + 'x' * 1000)[1:-1]
with open(sys.argv[1], "w") as handle:
    for pad in range(128):
        space = " " * pad
        handle.write('{' + space + '"MESSAGE":"' + quoted + ' ' + secret + '"}\n')
        handle.write('{' + space + '"n":-1.25e-123,"b":false,"MESSAGE":"' + secret + '"}\n')
        handle.write('[' + space + json.dumps('safe ' * 200) + ',1.25E+100,"' + secret + '"]\n')
PY
check "valid JSON tokens crossing decode windows are all scanned" 1 \
  "$(scan "${d}.out" --literals "$json_literal_file" "$d")"
check "  every boundary record retains its decoded literal" 384 \
  "$(grep -c ': operator-literal literal #1 ' "${d}.out")"
check "  without printing the decoded literal" no "$(has "$json_literal" "${d}.out")"

new_case
python3 - "${d}/long-json.json" <<'PY' || die "could not write long JSON"
import json, sys
with open(sys.argv[1], "w") as handle:
    handle.write(json.dumps({"message": "safe text " * 100000}) + "\n")
    handle.write(json.dumps({"items": ["safe"] * 2000}, indent=2) + "\n")
PY
check "long JSON strings and concatenated pretty arrays remain publishable" 0 \
  "$(scan_within 30 "$d")"

new_case
python3 -c 'import sys; open(sys.argv[1], "w").write("[INFO] {{{{ request [x] {y " * 160000)' \
  "${d}/single-line.log" || die "could not write single-line brackets"
check "a long single-line log of non-JSON brackets is scanned within 30 seconds" 0 \
  "$(scan_within 30 "$d")"

new_case
cat >"${d}/synthetic-json.json" <<'EOF' || die "could not write synthetic JSON controls"
{"ser\u0069al":"REDACTED","UDID":0}
{"MESSAGE":"ready","MESSAGE":"still ready"}
EOF
check "synthetic decoded field values and harmless duplicate members pass" "0" \
  "$(scan "${d}.out" --patterns-only "$d")"

# Every bracket is a decode attempt, and a failed attempt on the whole text
# counts every line before it: that took about a minute per megabyte here,
# against a second or two when attempts are bounded to the lines they need.
new_case
python3 -c 'import sys; open(sys.argv[1], "w").write("[INFO] {{{{ request [x] {y\n" * 80000)' \
  "${d}/brackets.log" || die "could not write brackets.log"
check "a large log of brackets that are not JSON is scanned within 30 seconds" "0" "$(scan_within 30 "$d")"

# --- Journal host prefixes, in every short format journalctl prints.
new_case
add_line "Sep 14 01:42:03 ${host} tensorplate-agent[42]: started"
expect_finding "a short-format journal host" journal-host "$host"

new_case
add_line "Sep 14 01:42:03.123456 ${host} tensorplate-agent[42]: started"
expect_finding "a short-precise journal host" journal-host "$host"

new_case
add_line "2026-09-14T01:42:03+0000 ${host} tensorplate-agent[42]: started"
expect_finding "a short-iso journal host" journal-host "$host"

new_case
add_line "2026-09-14T01:42:03.123456+00:00 ${host} tensorplate-agent[42]: started"
expect_finding "a short-iso-precise journal host" journal-host "$host"

new_case
add_line "Mon 2026-09-14 01:42:03 UTC ${host} tensorplate-agent[42]: started"
expect_finding "a short-full journal host" journal-host "$host"

new_case
add_line "-- Boot ${id32} --"
expect_finding "a journal boot separator" journal-host "$id32"

for journal_dir in /var/log/journal /run/log/journal; do
  new_case
  add_line "${journal_dir}/${id32}/system.journal"
  expect_finding "a machine id in ${journal_dir}" journal-host "$id32"
done

# The failing-stage detail joins log lines with spaces, so the host rule
# must match mid-line: the report itself is flagged, not only its log.
detail="${work}/detail-synthetic"
produce_report "$detail" tp-synthetic-host >/dev/null 2>&1 || die "the lifecycle runner failed"
check "a failing report quoting only the synthetic host passes" "0" \
  "$(scan "${detail}.out" --patterns-only "$detail")"
detail="${work}/detail-host"
produce_report "$detail" "$host" >/dev/null 2>&1 || die "the lifecycle runner failed"
check "a failing report quoting another host fails" "1" \
  "$(scan "${detail}.out" --patterns-only "$detail")"
check "  in the report's space-joined detail" "yes" \
  "$(if grep -qE '^lifecycle-report\.json:[0-9]+: journal-host ' "${detail}.out"; then echo yes; else echo no; fi)"
check "  without printing the host" "no" "$(has "$host" "${detail}.out")"

# --- hostnamectl and uname.
new_case
add_line "Static hostname: ${host}"
expect_finding "a static hostname" hostname "$host"

new_case
add_line "Transient hostname: ${host}"
expect_finding "a transient hostname" hostname "$host"

new_case
add_line "Pretty hostname: ${host} workstation"
expect_finding "a pretty hostname" hostname "$host"

new_case
add_line "Linux ${host} 6.8.0-1015-gcp #17-Ubuntu SMP PREEMPT_DYNAMIC x86_64 GNU/Linux"
expect_finding "a Linux uname host" hostname "$host"

new_case
add_line "Darwin ${host} 25.6.0 Darwin Kernel Version 25.6.0"
expect_finding "a Darwin uname host" hostname "$host"

new_case
add_line "Machine ID: ${id32}"
expect_finding "a machine id" machine-id "$id32"

new_case
add_line "Boot ID: ${id32}"
expect_finding "a boot id" machine-id "$id32"

# --- Accounts, devices and random ids.
new_case
add_line "installing from /home/${account}/assets"
expect_finding "a Linux home path" home-path "$account"

new_case
add_line "evidence under /Users/${account}/evidence"
expect_finding "a macOS home path" home-path "$account"

new_case
printf '{"\\u002fhome\\u002f%s\\u002fassets": "ok"}\n' "$account" >"${d}/paths.json" || die "could not write paths.json"
expect_finding "a home path in a JSON key written with escaped slashes" home-path "$account"

new_case
add_line "NVIDIA L4, 23034, 580.173.02, GPU-${bare_uuid}, [N/A]"
expect_finding "a GPU UUID outside the zero namespace" device-uuid "$bare_uuid"

new_case
add_line "MIG-${bare_uuid}"
expect_finding "a MIG UUID outside the zero namespace" device-uuid "$bare_uuid"

new_case
add_line "request ${bare_uuid} accepted"
expect_finding "a bare UUID" uuid "$bare_uuid"

new_case
add_line "request mycli-${bare_uuid} accepted"
expect_finding "a UUID behind a longer word ending in a random-id prefix" uuid "$bare_uuid"

# --- Addresses.
public_ipv4="$((11 + RANDOM % 100)).$(random_octet).$(random_octet).$(random_octet)"
new_case
add_line "peer ${public_ipv4} connected"
expect_finding "a public IPv4 address" ipv4 "$public_ipv4"

private_ipv4="10.$(random_octet).$(random_octet).$(random_octet)"
new_case
add_line "inet ${private_ipv4}/32"
expect_finding "a private IPv4 address" ipv4 "$private_ipv4"

link_local="169.254.$((1 + RANDOM % 168)).$(random_octet)"
new_case
add_line "route via ${link_local}"
expect_finding "a link-local IPv4 address other than the metadata server" ipv4 "$link_local"

near_documentation="203.0.114.$(random_octet)"
new_case
add_line "egress (${near_documentation})"
expect_finding "an address just outside a documentation range" ipv4 "$near_documentation"

local_ipv4="10.$(random_octet).$(random_octet).$(random_octet)"
new_case
add_line "Started sshd@3-${local_ipv4}:22-203.0.113.9:$((1024 + RANDOM)).service"
expect_finding "an address with a port behind a dash, as in an sshd connection unit" ipv4 "$local_ipv4"

dash_ipv4="10.$(random_octet).$(random_octet).$(random_octet)"
new_case
add_line "peer-${dash_ipv4} connected"
expect_finding "an address after a word and a dash, with no file name after it" ipv4 "$dash_ipv4"

new_case
add_line "allowed ${dash_ipv4}-192.0.2.20"
expect_finding "a range whose other end was already replaced" ipv4 "$dash_ipv4"

new_case
add_line "PTR ${dash_ipv4}.in-addr.arpa"
expect_finding "an address followed by a domain with no package name before it" ipv4 "$dash_ipv4"

new_case
exact_ipv4="198.18.$(random_octet).$(random_octet)"
add_line "peer ${exact_ipv4}"
check "findings print as ref:line: class (n chars)" "notes.log:2: ipv4 (${#exact_ipv4} chars)" \
  "$(scan "${d}.out" --patterns-only "$d" >/dev/null; grep -E '^notes\.log:' "${d}.out")"

# --- Values behind terminal styling or a backslash escape: the sequence
# ends in a letter, which would otherwise read as part of a word.
esc=$'\033'
styled_ipv4="10.$(random_octet).$(random_octet).$(random_octet)"
new_case
byte_message_record "${d}/agent-journal.txt" "addr=${esc}[1m${styled_ipv4}"
expect_finding "an address styled inside a byte-array MESSAGE" ipv4 "$styled_ipv4"

new_case
byte_message_record "${d}/agent-journal.txt" "request${esc}[1m${bare_uuid}"
expect_finding "a UUID styled directly after a word" uuid "$bare_uuid"

new_case
add_line "${esc}[2mSep 14 01:42:03${esc}[0m ${host} tensorplate-agent[42]: started"
expect_finding "a styled short-format journal timestamp" journal-host "$host"

new_case
add_line "stderr: 'peer \\u001b[1m${styled_ipv4}\\u001b[0m'"
expect_finding "an address behind an escaped control sequence" ipv4 "$styled_ipv4"

new_case
add_line "error: \"refused\\nSep 14 01:42:03 ${host} tensorplate-agent[42]: refused\""
expect_finding "a journal host behind an escaped newline" journal-host "$host"

new_case
add_line "Err(\"peers\\t${styled_ipv4}\")"
expect_finding "an address behind an escaped tab" ipv4 "$styled_ipv4"

ula="fd$(random_hex 1):$(random_hex 2):$(random_hex 2)::$(random_hex 2)"
new_case
add_line "inet6 ${ula}/64"
expect_finding "a unique local IPv6 address" ipv6 "$ula"

mac="02:$(random_hex 1):$(random_hex 1):$(random_hex 1):$(random_hex 1):$(random_hex 1)"
new_case
add_line "link/ether ${mac} brd ff:ff:ff:ff:ff:ff"
expect_finding "a MAC address" mac "$mac"

email="${account}@$(random_word 8).com"
new_case
add_line "owner ${email}"
expect_finding "an email address" email "$email"

project="$(random_word 8)-$((100000 + RANDOM))"
new_case
add_line "machine type projects/${project}/zones/us-central1-a/machineTypes/g2-standard-8"
expect_finding "a cloud project" cloud-project "$project"

new_case
add_line "resolved ${host}.c.${project}.internal"
expect_finding "a GCE internal DNS name" internal-dns "$host"

new_case
add_line "ssh to ${host}.local"
expect_finding "an mDNS host name" internal-dns "$host"

# --- Serials.
serial="$(random_alnum 12)"
new_case
add_line "Serial Number (system): ${serial}"
expect_finding "a system serial number" serial "$serial"

new_case
add_line "Hardware Serial: ${serial}"
expect_finding "a hardware serial" serial "$serial"

new_case
add_line "Provisioning UDID: ${serial}"
expect_finding "a UDID" serial "$serial"

new_case
add_line "    Serial Number                         : ${serial}"
expect_finding "a column-padded nvidia-smi -q serial" serial "$serial"

# --- Credentials, built at runtime so no scanner flags this file.
new_case
key_header="$(printf -- '-----BEGIN %s %s KEY-----' OPENSSH PRIVATE)"
add_line "$key_header"
expect_finding "a private key header" credential "$key_header"

for prefix in ghp_ gho_ ghu_ ghs_ ghr_ github_pat_ ya29.; do
  token="${prefix}$(random_alnum 40)"
  new_case
  add_line "Authorization: Bearer ${token}"
  expect_finding "a ${prefix} token" credential "$token"
done

# --- HTTP credentials, journal text allowlists, and package-name ambiguity.
# Uses the passing bundle and helpers above. Every candidate value is synthetic.
opaque_token="$(random_alnum 40)"
for auth_header in \
  "Authorization: Bearer ${opaque_token}" \
  "authorization: basic ${opaque_token}" \
  "Proxy-Authorization: bEaReR ${opaque_token}" \
  "Authorization=Basic ${opaque_token}" \
  "{\"aUtHoRiZaTiOn\":\"bEaReR ${opaque_token}\"}" \
  "{'Authorization': 'Basic ${opaque_token}'}"; do
  new_case
  add_line "$auth_header"
  expect_finding "an opaque credential in a header or serialized field" credential "$opaque_token"
done

new_case
printf '{\n  "Authoriz\\u0061tion":\n    "Bearer %s"\n}\n' "$opaque_token" \
  >"${d}/headers.json" || die "could not write escaped auth header"
expect_finding "an opaque credential in a multiline escaped JSON field" credential "$opaque_token"

for field in _UID _GID _CMDLINE _AUDIT_LOGINUID _SYSTEMD_CGROUP \
  CODE_FILE CODE_LINE CODE_FUNC SYSLOG_FACILITY SYSLOG_PID; do
  new_case
  metadata="meta-$(random_word 12)"
  add_line "    ${field}=${metadata}"
  expect_finding "a text ${field} field outside the journal allowlist" journal-field "$metadata"
done

new_case
cat >>"${d}/notes.log" <<EOF || die "could not append journal allowlist controls"
MESSAGE=ready
PRIORITY=6
SYSLOG_IDENTIFIER=tensorplate-agent
UNIT=tensorplate-agent.service
_PID=42
_SYSTEMD_UNIT=tensorplate-agent.service
_SYSTEMD_INVOCATION_ID=${zeros}
__REALTIME_TIMESTAMP=1757814123000000
CC=clang CXX=clang++ VCPKG_ROOT=/opt/vcpkg
EOF
check "allowed text journal fields and ordinary shell assignments pass" "0" \
  "$(scan "${d}.out" --patterns-only "$d")"

ambiguous_ipv4="198.18.$(random_octet).$(random_octet)"
for suffix in -disconnected .log .service .dist-info-backup -cp310-none-linux_aarch64.whl.log; do
  new_case
  add_line "peer-${ambiguous_ipv4}${suffix}"
  expect_finding "an address before a non-package suffix ${suffix}" ipv4 "$ambiguous_ipv4"
done

new_case
cat >>"${d}/notes.log" <<'EOF' || die "could not append package version controls"
ii  tensorrt  10.3.0.30-1+cuda12.6  arm64
tensorrt==10.3.0.30
tensorrt-10.3.0.30-cp310-none-linux_aarch64.whl
tensorrt-10.3.0.30-1-py2.py3-none-any.whl
nvidia_cudnn_cu12-9.3.0.75-py3-none-manylinux2014_aarch64.whl
/usr/lib/python3/dist-packages/tensorrt-10.3.0.30.dist-info/METADATA
tensorrt-10.3.0.30.dist-info
EOF
check "complete wheel and dist-info names, Debian revisions and pip pins pass" "0" \
  "$(scan "${d}.out" --patterns-only "$d")"

# --- Planning identifiers belong only in CHANGELOG.md.
planning_id="$(printf 'V%03d-E%02d-F%02d-T%02d' 21 5 1 1)"
new_case
add_line "implements ${planning_id}"
expect_finding "a planning identifier" planning-id "$planning_id"

planning_id="$(printf 'V%03d-F%02d-T%02d' 21 5 1)"
new_case
add_line "ii  tensorplate-agent  0.2.1  amd64  agent (${planning_id})"
expect_finding "a planning identifier without an epic segment" planning-id "$planning_id"

# --- Operator literals.
lit_file="${work}/private/case-literals.txt"
printf '%s\n%s\n' "$literal" "$short_literal" >"$lit_file" || die "could not write the literal file"

new_case
add_line "deployed by ${literal}"
expect_finding "a literal in file contents" "operator-literal literal #1" "$literal" --literals "$lit_file"

new_case
add_line "deployed by $(printf '%s' "$literal" | tr '[:lower:]' '[:upper:]')"
expect_finding "a literal in another case" "operator-literal literal #1" "$literal" --literals "$lit_file"

new_case
add_line "deployed by x${literal}y"
expect_finding "a long literal inside a longer word" "operator-literal literal #1" "$literal" --literals "$lit_file"

new_case
add_line "user ${short_literal} ran"
expect_finding "a short literal on word boundaries" "operator-literal literal #2" "$short_literal" --literals "$lit_file"

new_case
add_line "user x${short_literal}y ran"
check "a short literal inside a longer word is not a finding" "0" \
  "$(scan "${d}.out" --literals "$lit_file" "$d")"

new_case
byte_message_record "${d}/agent-journal.txt" "started by ${literal}"
expect_finding "a literal inside a byte-array MESSAGE" "operator-literal literal #1" "$literal" --literals "$lit_file"

new_case
mkdir "${d}/${literal}-run" || die "could not create a directory"
printf 'deployed by %s\n' "$literal" >"${d}/${literal}-run/install.log" || die "could not write a file"
expect_finding "a literal in a directory name" "operator-literal literal #1" "$literal" --literals "$lit_file"
check "  and the path is referenced by number" "yes" "$(has "path#" "${d}.out")"
entry="$(sed -n 's/^path#\([0-9]*\):0: operator-literal.*/\1/p' "${d}.out" | head -n 1)"
check "  whose number is that entry in the sorted listing" "./${literal}-run" \
  "$(cd "$d" && find . -mindepth 1 | LC_ALL=C sort | sed -n "${entry:-0}p")"

# --- Directory context must survive path scanning and output masking.
for prefix in logs/home home logs/Users; do
  new_case
  mkdir -p "${d}/${prefix}/${account}" || die "could not create an account directory"
  printf 'ready\n' >"${d}/${prefix}/${account}/agent.log" || die "could not write a path probe"
  expect_finding "an account in a ${prefix} directory path" home-path "$account"
  check "  and the account path is masked" "yes" "$(has 'path#' "${d}.out")"
done

new_case
mkdir -p "${d}/logs/home/${account}" || die "could not create an account directory"
printf '%s\n' "$key_header" >"${d}/logs/home/${account}/agent.log" || die "could not write a path probe"
expect_finding "a credential reported under an account path" credential "$account"
check "  without printing the credential" "no" "$(has "$key_header" "${d}.out")"

new_case
mkdir -p "${d}/logs/projects/${project}" || die "could not create a project directory"
printf 'ready\n' >"${d}/logs/projects/${project}/run.log" || die "could not write a path probe"
expect_finding "a cloud project in a directory path" cloud-project "$project"
check "  and the project path is masked" "yes" "$(has 'path#' "${d}.out")"

new_case
mkdir -p "${d}/projects/${project}" || die "could not create an empty project directory"
expect_finding "an empty cloud project directory" cloud-project "$project"

new_case
mkdir -p "${d}/home/tp-synthetic-operator/projects/REDACTED" || die "could not create synthetic directories"
printf 'ready\n' >"${d}/home/tp-synthetic-operator/projects/REDACTED/run.log" || die "could not write a synthetic path probe"
check "synthetic account and project paths remain publishable" "0" \
  "$(scan "${d}.out" --patterns-only "$d")"

# --- Files that cannot be reviewed as text.
new_case
ln -s notes.log "${d}/link.log" || die "could not create a symlink"
expect_finding "a symlink" symlink ""

new_case
gzip -c "${d}/notes.log" >"${d}/logs.gz" || die "could not create an archive"
expect_finding "a gzip archive" binary ""

new_case
printf 'ok\0ok\n' >"${d}/nul.log" || die "could not write a NUL file"
expect_finding "a file containing NUL" binary ""

new_case
printf 'caf\351\n' >"${d}/latin1.log" || die "could not write a non-UTF-8 file"
expect_finding "a file that is not UTF-8" binary ""

new_case
mkfifo "${d}/pipe" || die "could not create a FIFO"
expect_finding "a FIFO" special-file ""

# --- Faults: no verdict is exit 2, never 0 and never 1.
out="${work}/fault.out"
check "no mode is a fault" "2" "$(scan "$out" "$pass")"
check "both modes is a fault" "2" "$(scan "$out" --patterns-only --literals "$lit_file" "$pass")"
check "no paths is a fault" "2" "$(scan "$out" --patterns-only)"
check "a missing path is a fault" "2" "$(scan "$out" --patterns-only "${work}/absent")"
check "an unknown option is a fault" "2" "$(scan "$out" --patterns-only --strict "$pass")"
check "--literals without a file is a fault" "2" "$(scan "$out" --literals)"
check "--help is not a verdict" "2" "$(scan "$out" --help --patterns-only "$pass")"
check "a missing literal file is a fault" "2" "$(scan "$out" --literals "${work}/private/absent.txt" "$pass")"

printf '# only a comment\n\n' >"${work}/private/empty.txt" || die "could not write a literal file"
check "a literal file with no literals is a fault" "2" \
  "$(scan "$out" --literals "${work}/private/empty.txt" "$pass")"

printf 'ab\n' >"${work}/private/short.txt" || die "could not write a literal file"
check "a literal shorter than 3 characters is a fault" "2" \
  "$(scan "$out" --literals "${work}/private/short.txt" "$pass")"

fake_repo="${work}/fake-repo"
mkdir -p "${fake_repo}/sub" || die "could not create a fake repository"
printf '%s\n' "$literal" >"${fake_repo}/sub/literals.txt" || die "could not write a literal file"
ln -s "$fake_repo" "${work}/repo-alias" || die "could not create a repository alias"
check "a literal file inside the repository is a fault" "2" \
  "$(TP_EVIDENCE_REPO_ROOT="$fake_repo" scan "$out" --literals "${fake_repo}/sub/literals.txt" "$pass")"
check "  also when reached through a symlink" "2" \
  "$(TP_EVIDENCE_REPO_ROOT="$fake_repo" scan "$out" --literals "${work}/repo-alias/sub/literals.txt" "$pass")"
mkdir -p "${fake_repo}-private" || die "could not create a sibling directory"
printf '%s\n' "$literal" >"${fake_repo}-private/literals.txt" || die "could not write a literal file"
check "a literal file beside the repository with a shared name prefix is accepted" "0" \
  "$(TP_EVIDENCE_REPO_ROOT="$fake_repo" scan "$out" --literals "${fake_repo}-private/literals.txt" "$pass")"
outer_checkout="${work}/outer-checkout"
mkdir -p "${outer_checkout}/.git" "${outer_checkout}/worktrees/inner" || die "could not create nested checkouts"
printf '%s\n' "$literal" >"${outer_checkout}/literals.txt" || die "could not write a literal file"
check "a literal file in a checkout enclosing the repository is a fault" "2" \
  "$(TP_EVIDENCE_REPO_ROOT="${outer_checkout}/worktrees/inner" scan "$out" --literals "${outer_checkout}/literals.txt" "$pass")"
check "a repository root that does not exist is a fault" "2" \
  "$(TP_EVIDENCE_REPO_ROOT="${work}/no-such-repo" scan "$out" --literals "$lit_file" "$pass")"

if [[ "$(id -u)" -ne 0 ]]; then
  new_case
  chmod 000 "${d}/notes.log" || die "could not chmod"
  check "an unreadable file is a fault" "2" "$(scan "${d}.out" --patterns-only "$d")"
  chmod 644 "${d}/notes.log" || die "could not chmod"

  new_case
  mkdir "${d}/sealed" || die "could not create a directory"
  printf 'ok\n' >"${d}/sealed/inner.log" || die "could not write a file"
  chmod 000 "${d}/sealed" || die "could not chmod"
  check "an unreadable directory is a fault" "2" "$(scan "${d}.out" --patterns-only "$d")"
  chmod 755 "${d}/sealed" || die "could not chmod"
else
  printf '  skip unreadable inputs (running as root)\n'
fi

mkdir "${work}/empty-dir" || die "could not create a directory"
check "a scan of nothing is a fault" "2" "$(scan "$out" --patterns-only "${work}/empty-dir")"

# Nearly valid overlapping candidates must stop with no verdict rather
# than consume unbounded work or print any of the text being scanned.
new_case
python3 - "${d}/ambiguous.json" "$json_literal" <<'PY' || die "could not write ambiguous JSON"
import sys
with open(sys.argv[1], "w") as handle:
    handle.write("[" * 96 + '"' + "safe " * 1000 + sys.argv[2])
PY
check "exhausted JSON work is no verdict" "2" "$(scan "${d}.out" --patterns-only "$d")"
check "  identifies work exhaustion" "yes" "$(has 'JSON scan work limit exceeded' "${d}.out")"
check "  without printing scanned text" "no" "$(has "$json_literal" "${d}.out")"

# A crash must not read as a verdict: Python exits 1 on an uncaught
# exception, and JSON nested deeper than the decoder's recursion limit
# raises one.
new_case
python3 -c 'print("[" * 100000 + "]" * 100000)' >"${d}/deep.json" || die "could not write deep.json"
check "an internal error is a fault, not a finding" "2" "$(scan "${d}.out" --patterns-only "$d")"
check "  reported without a traceback that could quote scanned text" "no" "$(has "Traceback" "${d}.out")"

if [[ "$failures" -eq 0 ]]; then
  printf '\nall checks passed\n'
  exit 0
fi
# Not the count: an exit status is taken modulo 256.
printf '\n%s check(s) failed\n' "$failures"
exit 1
