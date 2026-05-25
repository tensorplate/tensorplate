# TensorPlate release docs

This directory owns the public release process for TensorPlate v0.1.0.
The v0.1.0 release has two separated phases:

1. Add and review the release machinery in a normal implementation PR.
2. After that PR merges, cut and publish `v0.1.0` from a clean
   `release/v0.1.0` commit and annotated tag.

The implementation PR must not be treated as the release. Final
publication requires the release branch, green CI, E15 release-gate
evidence, E16 clean-room evidence, package artifacts, checksums,
manifest, and maintainer sign-off.

| Document | Purpose |
| --- | --- |
| [`v0.1.0-runbook.md`](./v0.1.0-runbook.md) | Maintainer release flow from preflight through post-release monitoring. |
| [`v0.1.0-version-tag-policy.md`](./v0.1.0-version-tag-policy.md) | Version, changelog, release branch, RC tag, final tag, and immutability policy. |
| [`v0.1.0-artifacts.md`](./v0.1.0-artifacts.md) | Artifact manifest, checksum format, build and GitHub Release attachment procedure. |
| [`v0.1.0-signoff-template.md`](./v0.1.0-signoff-template.md) | Machine-checkable release sign-off and decision record template. |
| [`v0.1.0-evidence-template.md`](./v0.1.0-evidence-template.md) | Release evidence archive and handoff template. |
| [`v0.1.0-post-release.md`](./v0.1.0-post-release.md) | Support, hotfix, advisory, artifact deprecation, and rollback procedure. |
| [`v0.1.0-release-notes.md`](./v0.1.0-release-notes.md) | Draft GitHub Release notes source for the final release owner. |

Release automation entry point:

```bash
tools/release/tensorplate-release.sh --help
```
