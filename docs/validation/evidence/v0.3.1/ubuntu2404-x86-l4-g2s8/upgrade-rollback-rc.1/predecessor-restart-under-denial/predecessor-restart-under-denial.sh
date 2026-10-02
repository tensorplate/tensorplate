#!/usr/bin/env bash
# After the lifecycle run has rolled back to the published baseline: put the
# set-aside machine-type record back with the agent stopped, start the
# baseline agent with all IP traffic but loopback denied, and record what it
# established its identity from. Enforcement of the denial inside the agent's
# own control group is shown by the lifecycle helper's unit probe, classified
# against a control taken in the same control group before the denial.
set -Eeuo pipefail
OUT="${1:?evidence dir}"
SRC=/opt/tp/src
HELPER="${SRC}/tools/validation/linux_offline_runtime.py"
UNIT=tensorplate-agent
REC=/var/lib/tensorplate/state/machine-type.json
ASIDE=/var/lib/tensorplate/state.bak/machine-type.json
mkdir -p "$OUT"; cd "$OUT"

digest() { sudo sha256sum "$1" | cut -d' ' -f1; }
invocation() { systemctl show -p InvocationID --value "$UNIT"; }
await() {
  for _ in $(seq 60); do
    [[ "$(systemctl is-active "$UNIT")" == active && -S /run/tensorplate/agent.sock ]] && return 0
    sleep 1
  done
  return 1
}
journal_of() { # one invocation's records, projected to the fields the harness keeps
  sudo journalctl -u "$UNIT" "_SYSTEMD_INVOCATION_ID=$1" -o json --no-pager \
    | python3 -c 'import json,sys
keep=("MESSAGE","PRIORITY","SYSLOG_IDENTIFIER","UNIT","_PID","_SYSTEMD_UNIT","_SYSTEMD_INVOCATION_ID","__REALTIME_TIMESTAMP")
for line in sys.stdin:
    r=json.loads(line)
    if isinstance(r.get("MESSAGE"),str): print(json.dumps({k:r[k] for k in keep if k in r},sort_keys=True))'
}
unit_probe() { # unit_probe <control|probe>
  local group; group="$(systemctl show -p ControlGroup --value "$UNIT")"
  sudo python3 "$HELPER" "$1-unit" --unit "$UNIT" --control-group "$group" \
    --uid "$(id -u)" --gid "$(id -g)" --out "${OUT}/unit-$1.json"
}

# Three starts follow; clear the unit's start-rate counter so earlier restarts on this host do not count against them.
sudo systemctl reset-failed "$UNIT" 2>/dev/null || true
systemctl is-active --quiet "$UNIT" || { sudo systemctl start "$UNIT"; await; }
dpkg-query -W -f '${Package} ${Version} ${db:Status-Abbrev}\n' 'tensorplate*' >packages.txt
sudo python3 - "$REC" "$ASIDE" >record-shape.json <<'PY'
import json, sys
out = {}
for label, path in (("in_place", sys.argv[1]), ("set_aside", sys.argv[2])):
    data = json.load(open(path, encoding="utf-8"))
    out[label] = {"schema_version": data.get("schema_version"), "fields": sorted(data)}
print(json.dumps(out, indent=2, sort_keys=True))
PY
{
  echo "in_place_before  $(digest "$REC")"
  echo "set_aside        $(digest "$ASIDE")"
} >record-digests.txt

# Control: an undenied restart, and the control probe inside its control group.
sudo systemctl restart "$UNIT"; await
journal_of "$(invocation)" >journal-undenied-restart.txt
unit_probe control >unit-control.log 2>&1
echo "after_undenied_restart  $(digest "$REC")" >>record-digests.txt

# The check: stop, restore the set-aside record, deny, start.
sudo systemctl stop "$UNIT"
sudo cp -p "$ASIDE" "$REC"
sudo cmp "$ASIDE" "$REC"
echo "restored_from_set_aside  $(digest "$REC")" >>record-digests.txt
python3 "$HELPER" drop-in-text >drop-in.conf
path="$(python3 "$HELPER" drop-in-path --unit "$UNIT")"
cleanup() {
  sudo rm -f "$path"
  sudo systemctl daemon-reload
  sudo systemctl restart "$UNIT" || true
}
trap cleanup EXIT
sudo install -D -m 0644 drop-in.conf "$path"
python3 "$HELPER" check-drop-in --unit "$UNIT" --print "$path" >drop-in-check.txt 2>&1
sudo systemctl daemon-reload
sudo systemctl start "$UNIT"; await
systemctl show -p IPAddressDeny -p IPAddressAllow -p ActiveState -p SubState "$UNIT" >unit-policy-denied.txt
journal_of "$(invocation)" >journal-denied-start.txt
unit_probe probe >unit-probe.log 2>&1
status=0
python3 "$HELPER" classify --scope unit --probe unit-probe.json --control unit-control.json \
  --out unit-classification.json >unit-classification.log 2>&1 || status=$?
echo "classify exit=${status}" >>unit-classification.log
status=0
python3 "$HELPER" identity-check --agent-journal journal-denied-start.txt \
  --out identity-denied-start.json >identity-check.log 2>&1 || status=$?
echo "identity-check exit=${status}" >>identity-check.log
echo "after_denied_start  $(digest "$REC")" >>record-digests.txt
status=0
tensorplate status --output json >status-denied.json 2>status-denied.err || status=$?
echo "$status" >status-denied.exit

trap - EXIT
cleanup; await
systemctl show -p IPAddressDeny -p IPAddressAllow -p ActiveState "$UNIT" >unit-policy-restored.txt
sudo test ! -e "$path" && echo "drop-in removed" >>unit-policy-restored.txt
echo "after_policy_removed  $(digest "$REC")" >>record-digests.txt

cat record-digests.txt unit-classification.log identity-check.log
grep -h '"MESSAGE": "platform' journal-denied-start.txt journal-undenied-restart.txt | python3 -c 'import json,sys
for l in sys.stdin: print(json.loads(l)["MESSAGE"])'
cat unit-policy-denied.txt record-shape.json
echo PREDECESSOR-DENIED-RESTART-DONE
