# TensorPlate version and tag policy

This policy applies to TensorPlate release commits, release candidate
tags, final tags, and hotfix tags.

Releases are **tagged off the trunk**. No release branch is cut, ever:
keeping a maintenance line in sync with the trunk, and patching an old
line while the trunk moves on, is the cost this model trades away. Every
`vX.Y.Z` and `vX.Y.Z-rc.N` tag commit must be an ancestor of `develop`,
and `.github/workflows/release.yml` asserts exactly that before it
publishes anything.

Examples use:

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
# All X.Y.Z tags are created on the trunk, which is also what the release
# driver selects by default.
export TP_TRUNK_BRANCH=develop
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
| `develop` | The trunk. Every `X.Y.Z` release is prepared and tagged here. |
| `main` | Integration target for the trunk; carries no release-only commits. |
| `fix/<id>-<slug>` | A single fix, branched from and merged back to `develop`. A fix reaches a release by being tagged from the trunk, not by being back-ported. |
| Feature/tooling branch | Implementation PR branch for release tooling and docs only. |

No `release/X.Y` branch is created. The historical release branches and
per-version branches (`release/0.1`, `release/v0.1.0`, `release/v0.1.1`)
remain as immutable markers behind their tags; do not develop on them and
do not extend the pattern. A tag created on such a branch is not an
ancestor of `develop` and the release workflow rejects it.

The implementation PR branch may run `prepare --dry-run`, manifest fixture
checks, and documentation review. It must not create release tags, publish
assets, or announce the release.

### If the trunk can no longer deliver a patch

The prohibition above is free only while the trunk and the last release are
the same line. They are today. They stop being the same line the moment work
for the next minor lands on `develop`: from then on, the only patch the trunk
can publish for a shipped release is one that carries that work with it.

**A maintenance branch from a shipped release's tag is permitted when that
happens.** It is not created in advance and it is not a standing branch. The
conditions, decided now rather than during an incident:

- It branches from the tag of the release being patched, never from `develop`.
- It carries only the fix. The same fix lands on `develop` first, or
  simultaneously, so the trunk never regresses relative to a patch.
- It is authorized explicitly, per release. "We might need one" does not
  create one.
- The release workflow's trunk-ancestry check rejects a tag from such a
  branch by design. **Extending it to accept an authorized maintenance ref is
  a reviewed change made at that time** — never an emergency edit, and never a
  bypass added in advance. That gate is what stops an arbitrary commit being
  published as an official release; a path through it that nobody has
  exercised is worth less than no path at all.

Nothing here is lost by waiting. A branch from a tag is one command, and the
tag is immutable, so the capability exists whether or not the branch does.
What is decided in advance is only that the answer is yes, and on what terms.

Note that a maintenance branch does not reduce what a release costs to
publish. Evidence binds to the version under test, so a patch release needs
its own evidence for every Production row exactly as a trunk release does.
The branch narrows what the patch contains; it does not narrow what must be
validated before it ships.

## Tags

| Tag type | Format | Rules |
| --- | --- | --- |
| Release candidate | `vX.Y.Z-rc.N` | Annotated tag from the trunk; supersede by incrementing `N`, never by rewriting. The Release workflow publishes RC tags as public prereleases. |
| Final release | `vX.Y.Z` | Annotated tag from a clean release commit on the trunk. Pushing the tag triggers the Release workflow to build artifacts and create a draft GitHub Release for final verification. |
| Patch release | `vX.Y.Z` | Annotated tag from the trunk after the fix has merged there and targeted validation has run. |

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

- `main` and `develop` with required review and CI.
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
3. Merge the fix into the trunk through review.
4. Rebuild artifacts, regenerate manifest/checksums, rerun the required
   validation slice, and create the next RC tag.

If the final tag exists remotely, the release process switches to
post-release verification or hotfix mode. It must not create another final
tag with the same name.
