#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Which device identifiers in the recorded memory-observation shape the
# publication scanner catches by pattern, and which only the operator's
# literal file or this test guards. Injected values are generated at runtime.

set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scanner="${repo_root}/tools/validation/check-evidence-publication.sh"
recorded="${repo_root}/test/platform/memory_observation"
xml="nvidia-smi-q-x.xml"
synthetic_uuid="GPU-00000000-0000-0000-0000-000000000005"
synthetic_bus_id="00000000:00:00.0"
synthetic_board_id="0x0"
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
random_bus_id() {
  python3 -c 'import secrets
print("%08x:%02x:%02x.%x" % (0, secrets.randbelow(255) + 1, secrets.randbelow(32), secrets.randbelow(8)))'
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

# pci_location_is <bus id> <board id>: prints yes only when every committed
# field that locates the device carries those values; never prints a value.
pci_location_is() {
  python3 - "$recorded" "$1" "$2" <<'PY'
import csv, pathlib, sys
import xml.etree.ElementTree as ET
root, bus_id, board_id = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
domain, bus, rest = bus_id.split(":")
gpu = ET.parse(root / "nvidia-smi-q-x.xml").getroot().find("gpu")
with open(root / "nvidia-smi-query-gpu.csv", encoding="utf-8") as handle:
    rows = [{k.strip(): v.strip() for k, v in row.items()} for row in csv.DictReader(handle)]
observed = [
    gpu.get("id"),
    gpu.findtext("pci/pci_bus_id"),
    gpu.findtext("pci/pci_domain"),
    gpu.findtext("pci/pci_bus"),
    gpu.findtext("pci/pci_device"),
    gpu.findtext("board_id"),
] + [row["pci.bus_id"] for row in rows]
expected = [bus_id, bus_id, domain[-4:], bus, rest.split(".")[0], board_id]
expected += [bus_id] * len(rows)
print("yes" if rows and observed == expected else "no")
PY
}

printf 'the recorded shape as committed\n'
new_case
check "passes with patterns only" "0" "$(scan "${d}.out" --patterns-only "$d")"
check "carries only the synthetic PCI location" "yes" \
  "$(pci_location_is "$synthetic_bus_id" "$synthetic_board_id")"

literals="${work}/literals.txt"

printf 'a device UUID is caught by pattern\n'
new_case
uuid="GPU-$(random_hex 4)-$(random_hex 2)-$(random_hex 2)-$(random_hex 2)-$(random_hex 6)"
replace_in_xml "$synthetic_uuid" "$uuid"
check "is a finding" "1" "$(scan "${d}.out" --patterns-only "$d")"
check "  classed device-uuid" "yes" "$(has ": device-uuid" "${d}.out")"
check "  without printing the value" "no" "$(has "$uuid" "${d}.out")"

printf 'a GPU PDI in the XML tag form is caught by pattern\n'
new_case
pdi="0x$(random_hex 8)"
replace_in_xml "<pdi>REDACTED</pdi>" "<pdi>${pdi}</pdi>"
check "is a finding" "1" "$(scan "${d}.out" --patterns-only "$d")"
check "  classed gpu-pdi" "yes" "$(has ": gpu-pdi" "${d}.out")"
check "  without printing the value" "no" "$(has "$pdi" "${d}.out")"

printf 'a cloud project is caught by pattern\n'
new_case
project="$(random_digits 12)"
printf 'machineType projects/%s/zones/somewhere\n' "$project" >>"${d}/proc-meminfo.txt"
check "is a finding" "1" "$(scan "${d}.out" --patterns-only "$d")"
check "  classed cloud-project" "yes" "$(has ": cloud-project" "${d}.out")"
check "  without printing the value" "no" "$(has "$project" "${d}.out")"

printf 'a serial in the XML tag form is caught by pattern\n'
new_case
serial="$(random_digits 13)"
replace_in_xml "<serial>REDACTED</serial>" "<serial>${serial}</serial>"
check "is a finding" "1" "$(scan "${d}.out" --patterns-only "$d")"
check "  classed serial" "yes" "$(has ": serial" "${d}.out")"
check "  without printing the value" "no" "$(has "$serial" "${d}.out")"

# The CSV names the serial column once, in its header, and prints the value
# unlabelled on the device's row.
printf 'a serial in the CSV serial column is caught by pattern\n'
new_case
python3 - "${d}/nvidia-smi-query-gpu.csv" "$serial" <<'PY' || die "could not rewrite the CSV"
import pathlib, sys
p = pathlib.Path(sys.argv[1])
text = p.read_text(encoding="utf-8")
if text.count(", REDACTED, ") != 1:
    raise SystemExit("the recording no longer carries one REDACTED serial")
p.write_text(text.replace(", REDACTED, ", ", " + sys.argv[2] + ", "), encoding="utf-8")
PY
check "is a finding" "1" "$(scan "${d}.out" --patterns-only "$d")"
check "  classed serial" "yes" "$(has ": serial" "${d}.out")"
check "  without printing the value" "no" "$(has "$serial" "${d}.out")"

# No pattern class covers a PCI bus id, so the recordings carry a synthetic
# one and the literal file is what catches a real one.
printf 'a PCI bus id is NOT caught by pattern\n'
new_case
bus_id="$(random_bus_id)"
replace_in_xml "<pci_bus_id>${synthetic_bus_id}</pci_bus_id>" "<pci_bus_id>${bus_id}</pci_bus_id>"
check "passes with patterns only" "0" "$(scan "${d}.out" --patterns-only "$d")"

printf 'the same PCI bus id IS caught when the operator lists it\n'
printf '%s\n' "$bus_id" >"$literals"
check "is a finding with --literals" "1" "$(scan "${d}.out" --literals "$literals" "$d")"
check "  without printing the value" "no" "$(has "$bus_id" "${d}.out")"

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
