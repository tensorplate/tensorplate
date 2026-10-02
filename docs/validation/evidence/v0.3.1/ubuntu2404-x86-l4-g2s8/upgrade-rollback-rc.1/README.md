# Upgrade and rollback run on `v0.3.1-rc.1` for the `ubuntu2404-x86-l4-g2s8` row

**This is a run record, not the row's release evidence bundle.** The 0.3
release gate requires this row's report to carry a
[`reboot` stage](../../../../cloud-row-runbooks.md#the-reboot-stage-l4-row-from-030),
and the Ubuntu cloud harness does not run that stage yet. The report here
has the eight canonical stages only, so it is filed one level below the
row's directory, where `tools/release/check-evidence-bundles.sh` does not
read it, and the checker keeps reporting the row incomplete for 0.3.1:

```
INCOMPLETE  ubuntu2404-x86-l4-g2s8
            - no lifecycle-report.json under docs/validation/evidence/v0.3.1/ubuntu2404-x86-l4-g2s8/
```

The row's bundle is recorded again, with the reboot stage, before the
final 0.3.1 release.

Recorded 2026-10-01 on a disposable `g2-standard-8` with one NVIDIA L4,
booted from `common-cu129-ubuntu-2404-nvidia-580`
(`deeplearning-platform-release`): Ubuntu 24.04.5, kernel `7.0.0-1013-gcp`,
NVIDIA driver 580.178.04 (host facts from the retained raw run; no filed
file carries them). The candidate is the published `v0.3.1-rc.1`
release set and the baseline the published `v0.2.1` set; both were
signature-verified, neither run used `--allow-unsigned`. Identifiers are
the [evidence README](../../../v0.2.1/README.md)'s synthetic forms and the
raw run is retained privately. One edit goes beyond that README's table:
the regional Ubuntu mirror's host name in apt's source list has its region
replaced with `REDACTED`.

## `lifecycle/`

One run of `tools/validation/ubuntu-l4-cloud-lifecycle.sh` at the
candidate's commit with `--tested-version 0.3.1`, the candidate in
`--assets-dir` and the baseline in `--baseline-assets-dir`. First attempt,
no harness change: outcome `pass`, all eight stages pass,
19:06:42Z to 19:09:19Z. The instance was not fresh for it: the candidate
had been installed once by hand to check the host, which is why
`install.log` opens by removing and purging those packages before the
unattended install.

| File | What it holds |
| --- | --- |
| `lifecycle-report.json` | the report: subject, the eight stages and the log each cites |
| `artifact-digest.txt`, `baseline-digest.txt` | the SHA-256 of each set's `SHA256SUMS` |
| `checksums.txt`, `baseline-checksums.txt` | each set's files as verified against it |
| `upgrade-path.json` | the release tags and package versions the upgrade moved between |
| `install.log` … `rollback.log` | the eight stage logs |
| `packages-baseline.txt`, `packages-after-upgrade.txt`, `packages-after-remove.txt`, `packages-after-rollback.txt` | the TensorPlate packages `dpkg` listed at each step |
| `upgrade-baseline-deploy.json`, `upgrade-result.json`, `status-after-upgrade.json` | the deployment made on the baseline, and the same deployment answering on the candidate |
| `rollback-result.json`, `status-after-rollback.json` | the baseline after the rollback: no deployment carried over, then a fresh deploy answering |

The harness's other files (doctor output at each step, the journal
captures, the denied-egress stage's per-probe files) are in the retained
raw run; `offline.log` carries that stage's results in full.

The upgrade stage moved from the published 0.2.1 packages to the
candidate over a live deployment, and the rollback stage went back by the
documented procedure: stop the services, set the state directory aside,
restore the machine-type record from it, remove the packages, install the
baseline fresh. The harness
compares the restored record with the set-aside one before any agent
starts and requires them byte-identical.

## `predecessor-restart-under-denial/`

A check the harness does not make. Its upgrade and rollback stages run
online, and the 0.2.1 agent queries the metadata service on every online
start, so they cannot show that the rolled-back agent starts from the
restored record alone. After the lifecycle run left the baseline
installed, `predecessor-restart-under-denial.sh` (the script as run)
stopped the agent, copied the set-aside record back, denied the agent
unit all IP traffic except the two loopback host addresses with the
lifecycle helper's own drop-in, and started it.

| File | What it holds |
| --- | --- |
| `packages.txt` | the 0.2.1 packages installed for the check |
| `record-shape.json` | both copies of the record are schema 2 with the same six fields |
| `record-digests.txt` | the record's SHA-256 at six points, all equal: in place, set aside, after an undenied restart, restored, after the denied start, after the denial was removed |
| `journal-undenied-restart.txt` | the control: the agent's identity line reads `source=gce_metadata record=unchanged` |
| `journal-denied-start.txt` | the check: the identity line reads `source=recorded_gce_metadata record=not_applicable`, and the row is admitted |
| `drop-in.conf`, `drop-in-check.txt`, `unit-policy-denied.txt` | the denial as written and as systemd reports it on the running unit |
| `unit-control.json`, `unit-probe.json`, `unit-classification.json` and their logs | the helper's probe inside the agent's own control group before and under the denial: the operations that completed in the control are refused under it |
| `identity-check.log`, `identity-denied-start.json` | the helper's identity check over the denied start's journal, exit 0 |
| `status-denied.json` | `tensorplate status` under the denial: agent ready, the smoke deployment active |
| `unit-policy-restored.txt` | the drop-in removed and the unit active again |

In `record-digests.txt`, `in_place_before` is not the record as the
rollback left it: the two earlier attempts below had already copied the
set-aside record back, and every online start rewrites the same bytes.
The evidence that the rollback itself restored the record is the
harness's comparison in `lifecycle/rollback.log`, made before any agent
starts.

This is the third attempt, and the only one filed. The first counted
packets to the metadata address with a host-wide capture, which cannot
attribute a packet to a process: the cloud guest agent talks to the same
address continuously, so the count proves nothing either way. The second
failed for an unrelated reason: the agent unit allows five starts in 300
seconds, and back-to-back attempts had used them. The script now clears
the unit's start counter first. Both earlier attempts are in the retained
raw run.

The check shows one start on one machine. It is not the reboot boundary:
the record is bound to the boot, and this start was in the boot that
wrote it.
