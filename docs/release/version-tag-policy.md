# TensorPlate version and tag policy

This policy applies to TensorPlate public release branches, release
candidate tags, final tags, and hotfix tags.

Examples use:

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
export TP_RELEASE_BRANCH="release/${TP_TAG}"
```

## Version Surfaces

TensorPlate keeps the four version surfaces defined in
[`docs/architecture/versioning.md`](../architecture/versioning.md):

| Surface | Example value | Files checked before final tag |
| --- | --- | --- |
| Runtime release version | `0.1.0` | `CMakeLists.txt`, `Cargo.toml`, `Cargo.lock`, `vcpkg.json`, `packaging/VERSION`, `packaging/debian/changelog` |
| Protocol version | `0.1` | `CMakeLists.txt`, `protocol/rust/src/lib.rs`, protocol schemas |
| Schema version | `0.1` | `config/schemas/*.json`, `protocol/schemas/*.json` |
| Bundle format version | `0.1` | `CMakeLists.txt`, `protocol/rust/src/lib.rs`, bundle manifest docs and fixtures |

For a final release, development suffixes must be removed from release
version surfaces:

- `TP_RUNTIME_VERSION_SUFFIX` is empty.
- Cargo workspace version equals `${TP_VERSION}`.
- `tensorplate-protocol` dependency version equals `${TP_VERSION}`.
- TensorPlate workspace package entries in `Cargo.lock` have no `-dev`
  suffix.
- `vcpkg.json` uses `"version-string": "${TP_VERSION}"`.
- `packaging/VERSION` equals `${TP_VERSION}`.
- `packaging/debian/changelog` starts with
  `tensorplate (${TP_VERSION}-1) unstable; urgency=medium`.

Protocol, schema, and bundle format surfaces change only when their public
contracts change. A runtime patch release does not automatically imply a
protocol or bundle-format bump.

## Changelog Promotion

`CHANGELOG.md` is the source for release notes. The release owner promotes
the current `[Unreleased]` entries into a dated section:

```markdown
## [Unreleased]

## [X.Y.Z] - YYYY-MM-DD
```

The release script enforces the dated section before the final tag path.
Release notes must not claim support beyond validation evidence.

## Branches

| Branch | Purpose |
| --- | --- |
| `release/vX.Y.Z` | Clean release branch used to finalize metadata, build artifacts, tag, and publish. |
| `hotfix/vX.Y.Z` | Patch-release branch for a targeted fix after a public release. |
| Feature/tooling branch | Implementation PR branch for release tooling and docs only. |

The implementation PR branch may run `prepare --dry-run`, manifest fixture
checks, and documentation review. It must not create release tags, publish
assets, or announce the release.

## Tags

| Tag type | Format | Rules |
| --- | --- | --- |
| Release candidate | `vX.Y.Z-rc.N` | Annotated tag from `release/vX.Y.Z`; supersede by incrementing `N`, never by rewriting. |
| Final release | `vX.Y.Z` | Annotated tag from a clean release commit after final preflight passes. |
| Patch release | `vX.Y.Z` | Annotated tag from a hotfix release branch after targeted validation. |

Final tags are immutable after publication. Maintainers must not
force-push, delete, move, or recreate a published final tag. If an
artifact is defective, publish an advisory and cut a new patch tag instead
of replacing assets under the same final tag.

Where maintainer key material is available, set `TP_RELEASE_SIGN_TAG=1`
before running tag creation so the release script uses a signed annotated
tag.

## GitHub Protection Expectations

Repository settings should protect:

- `main`, `develop`, and `release/*` branches with required review and CI.
- `v*` tags against deletion and force updates.
- Release publication permissions to maintainers responsible for release
  engineering.

The release script checks local and remote tag existence before tag
creation, but repository protection is still required because local
automation cannot prevent every server-side mutation.

## Abort And Supersede

Failed release candidates remain in history. Do not retag them.

1. Record the blocker in the release evidence.
2. Route the fix to the owning issue.
3. Merge the fix into the release branch through review.
4. Rebuild artifacts, regenerate manifest/checksums, rerun the required
   validation slice, and create the next RC tag.

If the final tag exists remotely, the release process switches to
post-release verification or hotfix mode. It must not create another final
tag with the same name.
