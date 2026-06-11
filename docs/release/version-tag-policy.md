# TensorPlate version and tag policy

This policy applies to TensorPlate public release branches, release
candidate tags, final tags, and hotfix tags.

Examples use:

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
# One maintenance branch per minor line; all X.Y.Z tags are created there.
export TP_RELEASE_BRANCH="release/${TP_VERSION%.*}"
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
| `release/X.Y` | Long-lived maintenance line for a minor version (for example `release/0.1`), cut once from the first released tag of that line. Every `X.Y.Z` patch is committed and tagged here; no per-patch branches are created. |
| `fix/<id>-<slug>` | A single maintenance fix, branched from `release/X.Y` and merged back with a forward-port label. |
| Feature/tooling branch | Implementation PR branch for release tooling and docs only. |

The historical per-version branches (`release/v0.1.0`, `release/v0.1.1`)
remain as immutable markers behind their tags; do not develop on them. See
the internal release-and-branching strategy for the full model, including
the forward-port rule to `develop`.

The implementation PR branch may run `prepare --dry-run`, manifest fixture
checks, and documentation review. It must not create release tags, publish
assets, or announce the release.

## Tags

| Tag type | Format | Rules |
| --- | --- | --- |
| Release candidate | `vX.Y.Z-rc.N` | Annotated tag from `release/X.Y`; supersede by incrementing `N`, never by rewriting. The Release workflow publishes RC tags as public prereleases. |
| Final release | `vX.Y.Z` | Annotated tag from a clean release commit. Pushing the tag triggers the Release workflow to build artifacts and create a draft GitHub Release for final verification. |
| Patch release | `vX.Y.Z` | Annotated tag from a hotfix release branch after targeted validation. |

The release workflow publishes only annotated tags. Lightweight tags are
rejected. Public prerelease tags use the RC form above; `alpha` tags are
not supported until the Debian prerelease-version policy is added.

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
