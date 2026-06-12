# TensorPlate release evidence and handoff template

Copy this template into `dist/release/vX.Y.Z/evidence.md` during the
release operation. The evidence archive must be complete before
announcement.

## Release Identity

```text
Release: vX.Y.Z
Release branch: release/X.Y
Release commit: TODO
Final tag: vX.Y.Z
Tag object: TODO
GitHub Release URL: TODO
Release decision: TODO
```

## Required Evidence

| Evidence | Path or URL | Owner | Status |
| --- | --- | --- | --- |
| Release preflight report | `dist/release/vX.Y.Z/preflight.md` | Release owner | TODO |
| CI checks for release commit | TODO | Release owner | TODO |
| Release validation report | TODO | Validation reviewer | TODO |
| Clean-room validation | `dist/release/vX.Y.Z/clean-room.md` | Validation reviewer | TODO |
| Release sign-off | `dist/release/vX.Y.Z/signoff.md` | Release owner | TODO |
| Artifact manifest | `dist/release/vX.Y.Z/tensorplate-vX.Y.Z-artifacts.json` | Packaging reviewer | TODO |
| SHA256SUMS | `dist/release/vX.Y.Z/SHA256SUMS` | Packaging reviewer | TODO |
| Git tag metadata | `git show vX.Y.Z` transcript | Release owner | TODO |
| Release notes | GitHub Release body | Docs reviewer | TODO |
| Security review | sign-off record | Security reviewer | TODO |

## Published Assets

```text
tensorplate-common_X.Y.Z-1_all.deb
tensorplate-agent_X.Y.Z-1_arm64.deb
tensorplate-serving_X.Y.Z-1_arm64.deb
tensorplate-observability_X.Y.Z-1_arm64.deb
tensorplate-cli_X.Y.Z-1_arm64.deb
tensorplate-backend-python-pytorch_X.Y.Z-1_all.deb
tensorplate-vX.Y.Z-artifacts.json
SHA256SUMS
SHA256SUMS.cosign.bundle
```

If a sample bundle is attached, list it here with checksum and validation
scope.

## Clean-Room Summary

```text
Device model: TODO
OS / runtime floor: TODO
Package source: GitHub Release assets
Checksums verified: TODO
Core packages installed: TODO
tensorplate doctor: TODO
Agent service: TODO
Observability service: TODO
Bundle source: TODO
Deploy: TODO
Inference: TODO
Status/logs/metrics: TODO
Rollback: TODO
Uninstall/reinstall: TODO
Decision: TODO
```

## Redaction Review

Confirm the archive does not include:

- credentials, tokens, SSH host material, or private URLs.
- raw images or unbounded tensor payloads.
- full unbounded journals.
- unrelated user data.

## Announcement Checklist

- Release notes match `CHANGELOG.md`.
- Release notes link install guide, quickstart, validation reports,
  manifest, checksums, support policy, and security policy.
- Supported hardware/OS statement is explicit.
- Known limitations are listed.
- Conditional-pass risks have follow-up issues and owners.

## Follow-Up Issues

| Issue | Owner | Reason | Target |
| --- | --- | --- | --- |
| TODO | TODO | TODO | TODO |
