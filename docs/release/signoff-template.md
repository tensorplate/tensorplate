# TensorPlate release sign-off template

Copy this file to `dist/release/vX.Y.Z/signoff.md` during the release
operation. `dist/` is ignored by git, so release artifacts and evidence
can exist beside a clean release commit. The release script reads the
copied file and fails closed while any required field is unset.

Do not fill this template in a tooling PR.

```text
Release: vX.Y.Z
Release branch: release/vX.Y.Z
Release commit: <git-sha>
Final tag: vX.Y.Z

Release decision: TODO
Validation gate: TODO
Conditional risk: none
Conditional mitigation: none
Conditional owner: none
Conditional follow-up issue: none

Release owner: TODO
Runtime reviewer: TODO
Agent CLI reviewer: TODO
Packaging reviewer: TODO
Validation reviewer: TODO
Security reviewer: TODO
Docs reviewer: TODO

Artifact manifest: TODO
Security review: TODO
Docs review: TODO

CI evidence: TODO
Artifact manifest path: TODO
SHA256SUMS path: TODO
Validation gate report: TODO
Clean-room report: dist/release/vX.Y.Z/clean-room.md
GitHub Release URL: TODO
```

Allowed final values:

- `Release decision`: `pass`, `conditional-pass`, or `block`.
- `Validation gate`: `pass` or `conditional-pass` for publication.
- Reviewer fields: maintainer handles or names.
- Approval fields: `approved`.

Conditional pass is allowed only when the risk is user-visible, bounded,
documented in release notes or install guide, and linked to a follow-up
issue with an owner.

## Stop-The-Release Checklist

Set `Release decision: block` and stop if any item is true:

- Required validation gate is missing, failed, or unsigned.
- Required CI is red or unavailable.
- Version metadata is still at a development suffix.
- `CHANGELOG.md` is missing the dated release section.
- Package artifacts are missing or were built from a different commit.
- Manifest or checksums do not verify.
- Final tag already exists remotely.
- Any required reviewer withholds approval.
- Clean-room install uses local paths instead of GitHub Release assets.
- Clean-room install, doctor, service start, deploy, inference, status,
  logs, metrics, or rollback fails without an explicit conditional pass.
