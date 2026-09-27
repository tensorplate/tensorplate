#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The recorded memory-observation shape, against the publication scanner.
#
# test/platform/memory_observation holds an `nvidia-smi -q -x` XML
# recording and the CSV, meminfo and /proc/<pid>/status captures taken
# beside it. That XML carries a device UUID, a serial and a PCI bus id, so
# which of them the scanner catches by pattern and which it does not is a
# property this directory depends on. It is pinned here because the answer
# is not uniform, and an operator who believes the scanner guards all three
# will publish the two it does not.
#
# Every injected value is generated at runtime, so this file carries no
# value with the shape of a live identifier.

set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scanner="${repo_root}/tools/validation/check-evidence-publication.sh"
recorded="${repo_root}/test/platform/memory_observation"
xml="nvidia-smi-q-x.xml"
failures=0
case_count=0
d=""

unset TP_EVIDENCE_REPO_ROOT

die() {
  printf 'memory_observation_publication_test: %s\n' "$1" >&2
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

[[ -d "$recorded" ]] || die "the recorded directory is missing: ${recorded}"
[[ -f "${recorded}/${xml}" ]] || die "the XML recording is missing"

work="$(mktemp -d)" || die "mktemp failed"
trap 'rm -rf "$work"' EXIT

random_hex() {
  python3 -c 'import secrets, sys; print(secrets.token_hex(int(sys.argv[1])))' "$1"
}
random_digits() {
  python3 -c 'import secrets, sys
n = int(sys.argv[1])
print(secrets.randbelow(10 ** n - 10 ** (n - 1)) + 10 ** (n - 1))' "$1"
}

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

new_case() {
  case_count=$((case_count + 1))
  d="${work}/case-${case_count}"
  cp -R "$recorded" "$d" || die "could not copy the recorded directory"
}

# replace_in_xml <from> <to>: substitutes once in the case's XML.
replace_in_xml() {
  python3 - "${d}/${xml}" "$1" "$2" <<'PY' || die "could not rewrite the XML"
import pathlib, sys
p = pathlib.Path(sys.argv[1])
text = p.read_text(encoding="utf-8")
if sys.argv[2] not in text:
    raise SystemExit(f"the recording no longer contains {sys.argv[2]!r}")
p.write_text(text.replace(sys.argv[2], sys.argv[3], 1), encoding="utf-8")
PY
}

printf 'the recorded shape as committed\n'
new_case
check "passes with patterns only" "0" "$(scan "${d}.out" --patterns-only "$d")"

literals="${work}/literals.txt"

printf 'a device UUID is caught by pattern\n'
new_case
uuid="GPU-$(random_hex 4)-$(random_hex 2)-$(random_hex 2)-$(random_hex 2)-$(random_hex 6)"
replace_in_xml "GPU-00000000-0000-0000-0000-000000000001" "$uuid"
check "is a finding" "1" "$(scan "${d}.out" --patterns-only "$d")"
check "  classed device-uuid" "yes" "$(has ": device-uuid" "${d}.out")"
check "  without printing the value" "no" "$(has "$uuid" "${d}.out")"

printf 'a cloud project is caught by pattern\n'
new_case
project="$(random_digits 12)"
printf 'machineType projects/%s/zones/somewhere\n' "$project" >>"${d}/proc-meminfo.txt"
check "is a finding" "1" "$(scan "${d}.out" --patterns-only "$d")"
check "  classed cloud-project" "yes" "$(has ": cloud-project" "${d}.out")"
check "  without printing the value" "no" "$(has "$project" "${d}.out")"

# The scanner's serial rule needs a `:` or `=` after the label, which is the
# form `nvidia-smi -q` prints. Its XML form tags the value instead, so to the
# patterns it is a bare number. The evidence README already assigns a device
# serial to the operator's literal file for exactly this reason; these two
# cases hold that division so nobody re-derives the wrong half of it.
printf 'a serial in the XML tag form is NOT caught by pattern\n'
new_case
serial="$(random_digits 13)"
replace_in_xml "<serial>REDACTED</serial>" "<serial>${serial}</serial>"
check "passes with patterns only" "0" "$(scan "${d}.out" --patterns-only "$d")"

printf 'the same serial IS caught when the operator lists it\n'
printf '%s\n' "$serial" >"$literals"
check "is a finding with --literals" "1" "$(scan "${d}.out" --literals "$literals" "$d")"
check "  without printing the value" "no" "$(has "$serial" "${d}.out")"

# A GCE PCI bus id enumerates the virtual topology and is identical on every
# instance of a shape, so the scanner has no class for it and the recorded
# value is published as measured. A physical host's bus id goes in the
# literal file like its serial.
printf 'a PCI bus id is NOT caught by pattern\n'
new_case
replace_in_xml "<pci_bus_id>00000000:00:03.0</pci_bus_id>" "<pci_bus_id>00000000:65:00.0</pci_bus_id>"
check "passes with patterns only" "0" "$(scan "${d}.out" --patterns-only "$d")"

printf 'every capture the directory claims is present\n'
new_case
for name in \
  "$xml" \
  nvidia-smi-query-gpu.csv \
  nvidia-smi-query-compute-apps.csv \
  proc-meminfo.txt \
  proc-status-agent.txt \
  proc-status-serving-worker.txt \
  proc-status-python-sidecar.txt; do
  check "  ${name}" "yes" "$([[ -s "${recorded}/${name}" ]] && echo yes || echo no)"
done

# The two-query CSV fallback is a separate recording from the XML, not a
# projection of it, so each must carry the fields its own consumer reads.
check "the gpu CSV names its columns" "yes" \
  "$(grep -q 'memory.total' "${recorded}/nvidia-smi-query-gpu.csv" && echo yes || echo no)"
check "the compute-apps CSV records a process" "yes" \
  "$(grep -qE '^[0-9]+, ' "${recorded}/nvidia-smi-query-compute-apps.csv" && echo yes || echo no)"
check "a sidecar status capture names its process" "yes" \
  "$(grep -qE '^Name:' "${recorded}/proc-status-python-sidecar.txt" && echo yes || echo no)"

printf '\n'
if ((failures > 0)); then
  printf 'memory_observation_publication_test: %s check(s) failed\n' "$failures" >&2
  exit 1
fi
printf 'memory_observation_publication_test: all checks passed (%s cases)\n' "$case_count"
