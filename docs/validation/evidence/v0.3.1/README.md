# Release evidence bundles for 0.3.1

One directory per Production support row, named by row id, holding the
evidence that row's support claim rests on for 0.3.1. Each Production
row's `evidence.location` in `config/platform/rows/` names its directory
here, and the release workflow's evidence gate reads it with
`tools/release/check-evidence-bundles.sh --version 0.3.1`.

A report authorizes only the version it exercised, so the bundles in
[`../v0.2.1/`](../v0.2.1/) are not evidence for this release and every
Production row runs again for it; those bundles stay as they were
recorded. Until a row's directory here holds a complete bundle, the
checker reports the row incomplete and the final 0.3.1 release cannot
publish. Release candidates and build-only runs are not blocked, so a
candidate's artifacts can be used to collect the evidence.

A bundle has the layout of the v0.2.1 ones: a `lifecycle-report.json`
whose `subject.tested_version` is `0.3.1`, the stage logs it cites, and
the `artifact-digest.txt` naming the artifact the harness installed. The
`ubuntu2404-x86-l4-g2s8` report also carries the
[reboot stage](../../cloud-row-runbooks.md#the-reboot-stage-l4-row-from-030)
and the logs it cites; no other row's report may carry one. The
[cloud-row](../../cloud-row-runbooks.md) and
[physical-row](../../physical-row-runbooks.md) runbooks say how each row
is run and filed.

A run that is not a row's bundle is filed in a named directory below the
row's, with a README that says what it is. The checker reads a report
only at the row's own directory, so such a record never completes a row:
[`ubuntu2404-x86-l4-g2s8/upgrade-rollback-rc.1/`](ubuntu2404-x86-l4-g2s8/upgrade-rollback-rc.1/README.md)
is the upgrade from the published 0.2.1 set to the first 0.3.1 candidate
and the rollback, recorded before the harness has the reboot stage.

## Sanitize and scan before the first commit

The rules in [`../v0.2.1/README.md`](../v0.2.1/README.md) apply to these
bundles unchanged: what to remove, the synthetic value that replaces each
identifier, and the scan with `tools/validation/check-evidence-publication.sh`
and a private literal file before `git add`. They are kept in that one
file, which the scanner and its pull request job cite, so the synthetic
values the scanner accepts are listed once.
