# TensorPlate Release Docs

This directory owns the public release process for TensorPlate. The
release flow has two separated phases:

1. Add and review release machinery changes in a normal implementation PR.
2. After that PR merges, cut and publish the release from a clean
   `release/vX.Y.Z` commit and annotated `vX.Y.Z` tag.

The implementation PR must not be treated as the release. Final
publication requires the release branch, green CI, release-gate evidence,
clean-room evidence, package artifacts, checksums, manifest, and
maintainer sign-off.

| Document | Purpose |
| --- | --- |
| [`runbook.md`](./runbook.md) | Maintainer release flow from preflight through post-release monitoring. |
| [`version-tag-policy.md`](./version-tag-policy.md) | Version, changelog, release branch, RC tag, final tag, and immutability policy. |
| [`artifacts.md`](./artifacts.md) | Artifact manifest, checksum format, build and GitHub Release attachment procedure. |
| [`signoff-template.md`](./signoff-template.md) | Machine-checkable release sign-off and decision record template. |
| [`evidence-template.md`](./evidence-template.md) | Release evidence archive and handoff template. |
| [`post-release.md`](./post-release.md) | Support, hotfix, advisory, artifact deprecation, and rollback procedure. |
| [`notes/`](./notes/) | Per-release GitHub Release notes sources. |

Release automation entry point:

```bash
tools/release/tensorplate-release.sh --help
```

Create one release note file per final tag, for example
`docs/release/notes/v0.1.0.md`, while keeping the process docs above
version-neutral.
