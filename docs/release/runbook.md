# TensorPlate release runbook

This runbook owns the public release process for TensorPlate versioned
releases. It is reusable across patch, minor, and later release lines. Set
the release variables first; examples below use `0.1.0`.

```bash
export TP_VERSION=0.1.0
# A release is cut as a candidate first and as a final tag second, and
# every step below names the tag it is operating on. Set TP_RC to the
# candidate number for a candidate pass; unset it for the final pass.
# Re-export the block when you switch passes. Defining TP_TAG as
# `v${TP_VERSION}` alone made steps 4-6 name a tag that does not exist
# while the release is on the candidate path: `git rev-parse` exits 128,
# `git push` reports "src refspec does not match any", and the download
# step asks for a release nobody cut.
export TP_RC=1                 # unset TP_RC for the final tag
export TP_TAG="v${TP_VERSION}${TP_RC:+-rc.${TP_RC}}"
# Releases are tagged off the trunk. No release branch is cut, ever:
# keeping a maintenance line in sync with the trunk, and patching an old
# line while the trunk moves on, is the cost this trades away. The trunk
# is protected and moves only by merged pull request, which is why the
# release commit below is a merged PR and `cut` only tags it.
export TP_TRUNK_BRANCH=develop
export TP_RELEASE_DIR="dist/release/${TP_TAG}"
export TP_MANIFEST="${TP_RELEASE_DIR}/tensorplate-${TP_TAG}-artifacts.json"
export TP_CHECKSUMS="${TP_RELEASE_DIR}/SHA256SUMS"
export TP_SIGNOFF="${TP_RELEASE_DIR}/signoff.md"
export TP_PREFLIGHT="${TP_RELEASE_DIR}/preflight.md"
# Release notes are per version, never per tag: every candidate and the
# final tag publish the same file, which is why `release.yml` and the
# release driver both derive it from the version.
export TP_RELEASE_NOTES="docs/release/notes/v${TP_VERSION}.md"
```

Each candidate gets its own `TP_RELEASE_DIR`, and the manifest inside it
carries the candidate's tag — `tensorplate-v0.2.1-rc.1-artifacts.json`, not
the final name. Step 6 downloads by that name, so re-export the block
before switching passes rather than editing one variable.

The release has three separated phases:

1. Add or update release tooling/docs in a normal implementation PR.
2. Finalize the version surfaces in a **preparation PR** and merge it to
   the trunk (step 3a). The trunk is protected, so the release commit can
   only arrive this way.
3. Cut an annotated tag on that merged trunk commit and publish from it
   (steps 3b onward).

Neither PR is the release. The tag is.

## Required Owners

Record sign-offs in a copy of
[`signoff-template.md`](./signoff-template.md).

| Role | Responsibility |
| --- | --- |
| Release owner | Drives the release commit, tag, GitHub Release, and evidence archive. |
| Runtime reviewer | Reviews C++ runtime, serving worker, version surfaces, and adapter risk. |
| Agent CLI reviewer | Reviews agent, CLI, deploy, rollback, doctor, and status/log command posture. |
| Packaging reviewer | Reviews `.deb` artifacts, package metadata, maintainer scripts, and checksums. |
| Validation reviewer | Reviews release-gate and clean-room evidence. |
| Security reviewer | Reviews `SECURITY.md`, local endpoint posture, and advisory risk. |
| Docs reviewer | Reviews install guide, quickstart, release notes, and known limitations. |

## Implementation PR

The implementation PR may add or update release tooling, release docs,
install docs, validation procedure, support posture, and changelog notes.

### Version surfaces

The implementation PR does **not** finalize the version surfaces. That is
the preparation PR in step 3a, and `prepare` writes every one of them, so
they are reviewed together, merged together, and tagged as one commit:

- `CMakeLists.txt` — `project(... VERSION X.Y.Z)` and an empty
  `TP_RUNTIME_VERSION_SUFFIX`. The protocol/bundle `TP_*_VERSION_*` macros
  stay on the protocol track (e.g. `0` / `1` for `0.1`) and are never
  derived from the runtime version.
- `Cargo.toml` — `[workspace.package] version` and the `tensorplate-protocol`
  path-dependency version.
- `Cargo.lock` — every `tensorplate-*` crate version (third-party crates
  untouched).
- `vcpkg.json` — `version-string`.
- `packaging/VERSION`.
- `packaging/scripts/install.sh` — the installer's default version.
- `packaging/debian/changelog` — a new top stanza `tensorplate (X.Y.Z-1)`
  above the previous entries.
- `CHANGELOG.md` — open `## [X.Y.Z] - YYYY-MM-DD` under `[Unreleased]`,
  and move whatever `[Unreleased]` holds into it, leaving `[Unreleased]`
  empty. The tag is cut from a trunk commit, so everything under
  `[Unreleased]` at that commit ships in it. Entries merged after an
  earlier preparation PR therefore make the changelog pending again, and
  the cut refuses until a further preparation PR folds them in.

`sdk/python/pyproject.toml` keeps its `.dev0` development version; no
release step rewrites it, because the wheel version is injected at build.

### Release notes

`docs/release/notes/vX.Y.Z.md` **does** ship in the implementation PR. It
is not a version surface: `prepare` does not write it, it is absent from
the approved-file list `prepare` is allowed to touch, and
`ensure_prepare_diff_scope` would reject it if the preparation PR carried
it. Writing release notes is also review work, not a mechanical bump, so
it wants the ordinary review cycle rather than the narrow one in step 3a.
Leave it out of both PRs and the cut fails closed with
`release notes file is missing: docs/release/notes/vX.Y.Z.md (required by
the publish path)`.

It is a **hard tag prerequisite** the publish path requires
(`release.yml` `--notes-file`; the preflight). Its
supported-environment section must **link
[`docs/release/support-matrix.md`](support-matrix.md)**, not restate it.
That file is generated from `config/platform/` and guarded by a golden
test, so the platforms a release claims are the rows `doctor` matches and
deploy admission enforces. Prose restating them drifts, and a release note
that overstates supported hardware is a documented release blocker below.
A row change invalidates more than this one file, so regenerate both
goldens after any edit under `config/platform/`:

```
UPDATE_GOLDEN=1 cargo test -p tensorplate-platform --test support_matrix
UPDATE_GOLDEN=1 cargo test -p tensorplate-cli --test doctor_host_section
```

The second guards the `doctor` host section, whose candidate lists change
whenever a row is added, removed, or rescoped.

### Validating the implementation PR

Validate with `prepare --version X.Y.Z --dry-run` and `test/release/run.sh`.
`preflight`/`cut` run `check_version_files`, which fails closed on any stale
surface (including a partially bumped `Cargo.lock`), so a missed surface stops
the cut rather than shipping a misversioned build.

Allowed validation:

```bash
tools/release/tensorplate-release.sh --help
tools/release/tensorplate-release.sh prepare --version "${TP_VERSION}" --dry-run
test/release/run.sh
```

Forbidden in the implementation PR:

- Creating a release branch. Releases are tagged off the trunk; no release
  branch is cut.
- Finalizing version metadata. Removing development suffixes and promoting
  the changelog belong to the preparation PR in step 3a, which is reviewed
  on its own and merged immediately before the tag. Carrying them in an
  implementation PR leaves the trunk claiming a release nobody has cut, for
  as long as it takes to cut it.
- Creating release-candidate or final tags.
- Publishing a GitHub Release.
- Treating local build output as public release evidence.

## Final Release Operation

### 1. Confirm Prerequisites

The release owner stops immediately unless all prerequisites are true:

- Required validation gate is `pass` or signed `conditional-pass`.
- Required CI for the release commit is green.
- Security review is complete.
- Packaging artifacts can be built from the release commit.
- Public install guide and quickstart are reviewed.
- `docs/release/notes/vX.Y.Z.md` exists (a hard tag prerequisite, enforced by
  `preflight`/`cut` and required by the publish path's `--notes-file`).
- The self-hosted release runner is available for the build, and all four
  publish environments (`pypi`, `apt`, `homebrew`, `github-release`) each have
  a required reviewer (a reviewer-less environment publishes without a hold).
- Clean-room validation target is ready.
- No release blocker is open without a signed conditional pass.
- **Every Production row rests on a recorded run.** Verify with:

  ```
  cargo test -p tensorplate-platform --test registry_fixtures -- --ignored
  ```

  This is deliberately not a PR-blocking check, because a row may sit at
  Production with spec-authored values for an entire development cycle
  while its evidence run is scheduled. It is blocking *here*: a Production
  claim whose match key nobody has observed on the hardware must not ship.
  The failure names each offending row.

  Two ways to clear it, both honest — record the evidence, or downgrade the
  row until it exists. Downgrading is cheaper than it sounds:
  `is_supported_combination` admits Production **and** Preview, so a Preview
  row still deploys. It changes the published claim, not what runs.

### 2. Verify The Release Runner

The publish path is tag-driven:

1. The version metadata for `${TP_VERSION}` is already final on the trunk,
   put there by the merged preparation PR (step 3a).
2. The maintainer runs `tools/release/tensorplate-release.sh cut` from a
   refreshed trunk checkout. It verifies the checked-out commit is one
   `origin/${TP_TRUNK_BRANCH}` already contains and that its release
   metadata is final, then creates the annotated source tag on that exact
   commit. It edits nothing, commits nothing, and never pushes the trunk.
3. `.github/workflows/release.yml` builds the `.deb` packages from that
   tag, generates the manifest/checksums, and creates the GitHub Release
   with those assets attached after the tag is pushed.

RC tags create public prereleases. Final tags create draft GitHub
Releases by default so assets can be verified and clean-room validation
can run before publication.

The workflow must run on the release target architecture and must build
the real TensorRT execution path for v0.1.x packages. GitHub's hosted
`ubuntu-22.04-arm` runner is acceptable only if the workflow also
provides a JetPack-compatible CUDA/TensorRT development SDK and the CMake
configure log contains `TensorRT SDK detected; building real TensorRT
adapter execution path`. A hosted ARM build without that SDK is not a
publishable v0.1.x release build, because it produces a TensorRT adapter
that advertises the backend but returns `Unsupported` at engine load.

Final publication still requires clean-room validation on the Jetson Orin
Nano 8GB Super / JetPack 6.x floor, because a hosted runner is not a
JetPack/L4T system.

Future release lines should provide a dedicated self-hosted runner labeled:

```json
["self-hosted", "linux", "ARM64", "tensorplate-release"]
```

The package build runner must have:

- `sudo` access for installing Debian build dependencies.
- Rust via `rustup`, CMake, Ninja, a C++ compiler, debhelper, `dh-exec`,
  `dpkg-buildpackage`, `nlohmann-json3-dev`, and GitHub CLI `gh`.
- The vcpkg checkout and binary cache of "Provision the vcpkg checkout and
  binary cache", provisioned ahead for the release's `vcpkg.json`. The
  release job reads `VCPKG_ROOT` and the cache from `vcpkg-env`; nothing has
  to be exported in the runner service's environment. The worker compiles
  against the manifest's `nlohmann-json`, not the distribution's
  `nlohmann-json3-dev`.
- JetPack-compatible CUDA/TensorRT development headers and libraries when
  building v0.1.x release packages with `TP_ENABLE_TENSORRT=ON`. Release
  artifact builds default `TP_REQUIRE_TENSORRT_SDK=ON` so they fail during
  CMake configure instead of producing non-functional TensorRT packages.
- Outbound network access to Sigstore (Fulcio/Rekor) and the GitHub
  attestation API so the publish path can keyless-sign `SHA256SUMS` and
  record build provenance. The repository must allow artifact attestations.
  `cosign` itself is installed by the workflow.

The future dedicated self-hosted runner should also have the target SDK
stack needed for release validation. For v0.1.x that means
JetPack/CUDA/TensorRT on `arm64`.

For the current Jetson release runner, keep the runner offline and
unprivileged except during trusted release builds. The operator helper is
secret-free and may be copied to `/usr/local/sbin/tensorplate-runner` on
the Jetson, or run from the checked-out repository:

```bash
sudo tools/release/jetson-runner-control.sh status
sudo tools/release/jetson-runner-control.sh on
```

After the build-only or publish workflow finishes, turn the runner back
off. This stops and disables the systemd service and removes the temporary
release sudoers allowance for the `gha-runner` account, and the bounded
apt wrapper that allowance names:

```bash
sudo tools/release/jetson-runner-control.sh off
```

`on` installs `/usr/local/sbin/tensorplate-apt`, a root-owned wrapper that
runs apt under a hard time bound, and grants the runner account `NOPASSWD`
on that path and `install` -- not on `apt-get` itself, and deliberately not
on `timeout`, which would be a root shell. **Re-run `off` then `on` after
updating this checkout** if the wrapper or the allowance changed, or the
runner keeps whatever the last `on` installed.

Do not leave this persistent self-hosted runner online for general OSS PR
CI. Normal pull-request CI should remain on GitHub-hosted runners; the
Jetson runner is for trusted release jobs that require JetPack/CUDA/
TensorRT on the target architecture.

#### Provision the vcpkg checkout and binary cache

Both release builds configure the serving worker with
`TP_ENABLE_STREAMING_GRPC=ON` and link gRPC and protobuf statically from the
vcpkg manifest feature `streaming-grpc`, at the vcpkg commit `vcpkg.json`
pins as its `builtin-baseline`. A cold build of that feature takes this
runner about two hours and a hosted runner about three quarters of an hour
or more, so a release
that publishes never starts one: it restores the packages from a binary
cache, and a package the cache lacks fails the configure step.

- **ARM64 job.** It appends what `jetson-runner-control.sh vcpkg-env`
  prints to its environment and fails there when the runner is not
  provisioned for the checkout's `vcpkg.json`. Its build always runs with
  `TP_VCPKG_BINARY_ONLY=1`, build-only dispatches included: provision the
  runner first, as below.
- **amd64 job.** `.github/actions/release-vcpkg` checks vcpkg out at the
  baseline and restores an Actions cache keyed on the baseline and on a
  digest of three things: what the manifest says, in the canonical form
  described below, the first line of `clang --version`, and the SHA-256 of
  the `clang++` binary. A new or rebuilt compiler therefore opens a new key.
  It sets `VCPKG_FORCE_DOWNLOADED_BINARIES=1`, so vcpkg runs the CMake and
  Ninja its checkout pins and not the hosted image's. A run that publishes
  only restores the cache: it builds nothing and saves nothing. A build-only
  dispatch builds what is missing, within 180 minutes instead of 60, and
  saves the cache only when the job succeeds.

`.github/workflows/release-dependencies.yml` warms that cache on `develop`:
on a push that changes `vcpkg.json`, the action or the build profile, twice a
week, and on dispatch. A cache saved on `develop` is restored on any tag;
one saved on another ref only on that ref. After a miss, dispatch that
workflow on `develop`, or `release.yml` with `publish=false` from `develop`
or from the tag, then re-run the release. That workflow too saves the cache
only when its job succeeds.

The runner needs `git`, `curl`, `zip`, `unzip`, `tar`, `make`, Perl with
`IPC::Cmd`, the Linux kernel headers (`linux-libc-dev`), Ninja, and the
CMake and the C and C++ compilers the release build uses; vcpkg's scripts
need CMake 3.21 or newer and this tree needs 3.25. The OpenSSL port that
gRPC depends on is what asks for `make`, Perl and the kernel headers. The
runner also needs network access to clone vcpkg and download the sources it
builds, and room in the cache directory for two install trees beside the
archives: the build's and the proof's exist at the same time. The command
itself runs `timeout` and `flock` from `/usr/bin`, where coreutils and
util-linux install them, and refuses to start without either. It reads
`vcpkg.json` with `python3` from `PATH`, as `status` and `vcpkg-env` do, and
none of the three takes a baseline or reports the runner ready without it.
The release build needs `python3` already:
`tools/release/build-release-artifacts.sh` runs it to check the source
version, and the Jetson job installs it with its other packages. The
commands trust the `PATH` they are run with: `git`, `find`, `sha256sum`,
`uname` and `python3` are whatever it resolves. `python3 -I` keeps the
Python variables, the directory the command is run from and the account's
own site directory out of the read; it does not keep out a `PATH` that
names another interpreter.

The record of readiness is for the vcpkg baseline and for the
**dependencies digest** of `vcpkg.json`: the SHA-256 of the manifest's
content without its own version. `python3` reads the file as JSON, drops
the top-level `version`, `version-string`, `version-semver` and
`version-date`, and hashes everything else in one canonical form: keys
sorted, no blanks, ASCII. The digest therefore follows what the manifest
says and not how the file is laid out. vcpkg builds nothing from the
project's own version, and the preparation PR of step 3a changes nothing
else that the manifest says: `prepare` writes the release's version into
`vcpkg.json` and writes the whole file back in its own layout, which may
move lines the commit before it had laid out another way. So a runner
provisioned from any commit with the release's baseline and dependencies
stays ready for the release commit, and no provisioning run belongs to a
release. Provision again when:

- anything `vcpkg.json` says other than its own version changes: the
  `builtin-baseline`, a dependency, a feature, an override, the name, the
  order of a list. A `version` below the top level is not the manifest's
  own and counts: an override's, or a dependency's minimum.
  `status` then reports `vcpkg_cache_ready: no`, provisioned for another
  vcpkg baseline or for another dependencies digest. With the cache kept,
  the packages the change left alone should be restored, not built.
- vcpkg rewrites `vcpkg.json`: `vcpkg format-manifest`, or any vcpkg
  command that writes the manifest back. Its formatter drops the two empty
  `dependencies` lists the committed file holds. vcpkg builds the same
  without them, and it is still a change in what the file says, so the
  digest changes; the provisioning run after it should restore every
  package from the cache.
- the manifest's `vcpkg-configuration` member names a directory under
  `overlay-ports` or `overlay-triplets` and something inside that directory
  changes, which `status` cannot see: the digest holds the path the
  manifest names and not what the directory contains. The manifest has no
  such member today.
- the compiler or the CMake the release build resolves on the runner
  changes, which `status` cannot see; "Readiness does not compare
  toolchains" below says how to find out.
- the proof of `provision-vcpkg --check` fails, which removes the stamp.

Re-indenting `vcpkg.json`, reordering its keys or laying a list out over
more or fewer lines changes nothing it says, and leaves the runner ready.

A `vcpkg.json` that no baseline and no digest can be taken from is refused,
by every command and with the reason: a file that is not one JSON object in
UTF-8 with no byte order mark before it, an object that names a key twice
at any depth, a number that is not an integer, a `builtin-baseline` that is
not 40 lowercase hex digits. vcpkg itself reads a manifest behind a byte
order mark; these commands do not. Nothing is guessed from such a file:
`status` reports `vcpkg_baseline: unreadable (...)` and `vcpkg_cache_ready:
no (...)`, `vcpkg-env` prints nothing, and `provision-vcpkg` changes
nothing. The same holds where `python3` is not on `PATH`, and where it
fails: the reason then names `python3` and its exit status and not the
manifest.

A checkout with a `vcpkg-configuration.json` beside its `vcpkg.json` is
refused the same way, whatever the file holds. vcpkg reads that file with
the manifest -- registries, overlay ports, overlay triplets -- and the
digest is of `vcpkg.json` alone, so the runner would answer ready for
packages its proof never restored. This repository has no such file. If it
ever needs what one says, put it into `vcpkg.json` as the
`vcpkg-configuration` member, where the digest holds it.

Run the provisioning **as the runner account**, from a checkout that the
account can read and cannot write, under directories it can search -- a
clone owned by root outside every home directory, for example: `status`,
`on` and `off` are run from the same checkout as root. Provisioning needs
no root and no sudo allowance, so the runner can stay `off`; it refuses to
run as root or as any other account, and prints the command to use when it
does:

```bash
sudo -u gha-runner -H env PATH="<runner_path>" \
  <checkout>/tools/release/jetson-runner-control.sh provision-vcpkg
```

`sudo -u` starts the command with a clean environment and a `PATH` of its
own, and vcpkg keys every cached package on the compiler and the CMake it
finds. `<runner_path>` is the `PATH` the runner service starts jobs with,
which `status` prints as `runner_path`: a cache built with another CMake or
compiler than a release job resolves is a cache that job misses. Set
everything else on the far side of `sudo` in the same way: `CC` and `CXX`
if the release build sets them, `VCPKG_MAX_CONCURRENCY=<n>` to limit how
many jobs vcpkg runs at once (the script sets no job count of its own), and
`TP_JETSON_RUNNER_USER=<account>` with any other `TP_JETSON_RUNNER_*`
setting when the runner does not use the defaults. Every command of this
section that runs as the runner account takes the same assignments, and
`status` takes the `TP_JETSON_RUNNER_*` ones:
`sudo env TP_JETSON_RUNNER_USER=<account> <checkout>/tools/release/jetson-runner-control.sh status`.
A command run through `sudo -u` starts in the operator's current directory;
each of these works even when the runner account cannot enter it, as with
another account's home directory.

The build runs downloaded build scripts as the runner account, for hours,
on the operator's terminal. Check that `sudo -l` lists `use_pty` among the
defaults, so that those processes get a terminal of their own and cannot
type into the operator's. Where it does not, give the command one:

```bash
sudo -u gha-runner -H env PATH="<runner_path>" script -qec \
  '<checkout>/tools/release/jetson-runner-control.sh provision-vcpkg' /dev/null
```

Start the command in a session that outlives a dropped connection, such as
`tmux`. Ctrl-C, a hangup or a `TERM` stops the build along with the
command, and the runner is then not recorded as ready; run the command
again and the packages that were finished are restored from the cache. A
command that is killed outright cannot stop its build, which goes on
running without it for up to the bound.

Only one `provision-vcpkg`, with or without `--check`, runs at a time. Each
takes a lock on the file `provision.lock` in the cache directory without
waiting for it, and one that finds the lock held is refused before it
touches the checkout, the cache or the stamp. A build left behind by a
killed command holds the lock as well, so nothing can be provisioned over
it: find that build with `pgrep -u gha-runner -fa 'vcpkg install'`, send
`TERM` to the `timeout` process among the matches, and remove the
`install.*` directory left next to the `archives` directory. Anything else
a run started and left running holds the lock too, until it ends. When
`pgrep` finds no build, `sudo fuser -v` on the lock file (from `psmisc`), or
`sudo lsof` on it, lists every process that still has it open; `lslocks`
may show nothing for it. The lock file is empty and stays; do not remove it
while a run is under way. `status` and `vcpkg-env` take no lock and answer
while a run holds it.

The lock is a file of the cache directory, so it keeps apart the runs that
are given the same cache directory. Two runs given different
`TP_JETSON_RUNNER_VCPKG_CACHE_DIR` values and the same vcpkg checkout are
not kept apart and would move that checkout under each other: give every
run the same `TP_JETSON_RUNNER_*` settings. With the defaults both
directories follow the runner account.

The command reads the baseline from `vcpkg.json` in the checkout it is run
from, so run it from the checkout, not from a copy of the script. The same
holds for the vcpkg lines of `status`, for `vcpkg-env` and for
`provision-vcpkg --check`: a copy of the helper outside a checkout reports
`vcpkg_baseline: unreadable` and the cache as not ready. The command then:

1. clones vcpkg into the runner account's home directory, or reuses the
   vcpkg checkout there, and checks out the baseline commit. A checkout with
   local modifications or untracked files is refused, before the move and
   again after it, and so is a repository that is not vcpkg.
2. bootstraps the vcpkg tool of that commit, with
   `VCPKG_FORCE_SYSTEM_BINARIES=1` so vcpkg uses the system CMake and Ninja.
3. builds the manifest with the `streaming-grpc` feature for the machine's
   triplet into a binary cache under the account's `.cache` directory. Each
   vcpkg run is bounded by `TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT` seconds,
   six hours unless set.
4. proves the cache: installs the same manifest a second time, into an
   empty directory, with the cache read-only and vcpkg forbidden to build.
   That install succeeds only if every package is restored from the cache.
5. checks that the checkout is still unmodified at the baseline, and
   records a stamp beside the cache: the baseline, the dependencies digest
   of `vcpkg.json`, the triplet, the feature, the vcpkg tool version, the
   compiler, the time, and a digest of the names of the cache's archives.

A run that gets as far as moving the checkout leaves a stamp only if it
completes: a stamp from an earlier run is removed before the checkout
moves, so a failed bootstrap, build or proof never leaves the runner
reported as ready. A run that is refused before that -- the wrong account,
a missing compiler, `python3`, `timeout` or `flock`, an unusable
`vcpkg.json`, a `vcpkg-configuration.json` beside it, another run holding
the lock, a directory that is not a vcpkg
checkout, a modified checkout -- exits non-zero and changes nothing but the
cache directory and the empty lock file it may have created, so an earlier
stamp stands.

When provisioning refuses the vcpkg checkout, look at it before changing
it. `<vcpkg_root>` is the directory `status` prints:

```bash
sudo -u gha-runner -H git -C <vcpkg_root> status --porcelain --untracked-files=all
```

A clone or a checkout that was cut off, by a full disk for example, leaves
files of its own behind, and git then reports them as changes; so does a
file the runner account could not write. If nothing listed is worth
keeping, put the checkout back as the runner account with git's own
`reset --hard` and `clean -fd`, or remove the directory. Removing it loses
the clone and nothing else: the next `provision-vcpkg` clones vcpkg again
and restores the packages from the cache.

The build leaves vcpkg's `buildtrees` and `packages` directories in the
vcpkg checkout. git ignores both and nothing reads what they hold once the
cache is proved, so they can be removed when the disk is needed.

Before a release, check the runner, with the same assignments as the
provisioning command:

```bash
sudo <checkout>/tools/release/jetson-runner-control.sh status
sudo -u gha-runner -H env PATH="<runner_path>" \
  <checkout>/tools/release/jetson-runner-control.sh provision-vcpkg --check
```

`status` reports `vcpkg_root`, `vcpkg_baseline`, `vcpkg_commit`,
`vcpkg_checkout` (`current`, `stale` or `absent`), `vcpkg_tool`,
`vcpkg_binary_cache`, `vcpkg_cache_archives` and `vcpkg_cache_ready`, which
is `yes` or `no` with the reason, followed by the stamp's baseline, triplet,
compiler and time. Run without `sudo` by an account the runner's home
directory is closed to, it reports what it cannot see as `unknown` or
`unreadable`, not as absent. `status` reads files only: the `vcpkg.json` of
the checkout it is run from, the stamp, the vcpkg checkout's `HEAD`, the
tool, and the names of the cache's archives, which must be the ones the
stamp recorded. It does not read the archives, and it does not ask git
whether the checkout was modified, because root must not run git in a
checkout another account can write. `provision-vcpkg --check` does both: it
requires the same readiness and an unmodified checkout, then repeats step 4
against the existing cache. A proof that passes changes nothing: the stamp
and the cache are as they were, and the install directory it used is
removed. The command exits non-zero if the checkout is stale or modified,
if another run holds the lock, if a package is missing from the cache, or
if the install could not run, could not write its directory or did not end
within the bound.

`--check` fails closed. A check that is refused before its proof -- the
wrong account, a runner that is not ready, a stale or modified checkout, a
held lock -- changes nothing. A proof that ran and failed has shown the
stamp to be wrong, so the command removes the stamp before it exits and
says so. From then on `status` reports `vcpkg_cache_ready: no (not
provisioned: ...)` and `vcpkg-env` prints nothing, until `provision-vcpkg`
succeeds again; the archives stay, so that run restores what is intact and
builds only what is not. A stamp the command cannot remove it empties
instead, and says so: an empty stamp records nothing, and `status` reports
`vcpkg_cache_ready: no` for it as well. Only where it can do neither, on a
file system that went read-only for one, does the stamp stand: the command
says that, `status` and `vcpkg-env` go on answering ready, and the stamp
has to be removed by hand. A check stopped by Ctrl-C, a hangup or a `TERM`
has proved nothing either way and leaves the stamp.

Readiness does not compare toolchains. vcpkg keys each cached package on
the compiler and on the CMake it ran with, so a compiler or CMake that
changed after provisioning makes the cache miss while `status` still says
`yes`. `status` shows the compiler line the stamp recorded; after a
toolchain upgrade run `provision-vcpkg --check` with the release build's
`PATH`, `CC` and `CXX`. If its proof fails it removes the stamp; provision
again.

`vcpkg-env` is for the ARM64 release job, which uses the cache. It prints
`VCPKG_ROOT`, a read-only `VCPKG_BINARY_SOURCES` and
`VCPKG_FORCE_SYSTEM_BINARIES=1`, one per line, for the job to append to its
environment file. It prints them only when `status` would report the cache
as ready for the `vcpkg.json` of the checkout it is run from and git
reports the vcpkg checkout unmodified; otherwise it prints nothing and
exits non-zero, so a job cannot pick up a cache made for another baseline
or other dependencies, or a checkout whose tracked files were edited after
the proof. git does not report what vcpkg's own `.gitignore` covers, which
includes the bootstrapped tool and a triplet file placed directly in
`triplets/`: only `provision-vcpkg --check`, which runs the tool, can
notice a change there.
`vcpkg-env` asks git as the runner account, with the variables that would
point git at another repository removed from its environment, and refuses
to run as root.

### 3. Put The Release Commit On The Trunk, Then Tag It

The trunk is protected: `develop` requires a pull request and the ruleset
names no bypass actor, so nothing in this step pushes a commit to it. The
release commit arrives the ordinary way, as a merged PR, and `cut` only
tags what is already there.

The order is load-bearing. Preparing version metadata locally and tagging
it before that PR merged put the tag on a commit the squash merge then
replaced with a different SHA: the tag pointed at a commit no branch
contained, and the workflow's trunk-ancestry check (step 5) rejected it.

#### 3a. Finalize the version surfaces in a preparation PR

The preparation PR is per version, not per tag: it finalizes the
`${TP_VERSION}` surfaces once, and every candidate and the final tag are
cut from that same commit.

Skip to 3b when the trunk already carries final metadata for
`${TP_VERSION}` — `prepare` is then a no-op and there is nothing to merge.
`cut` checks this itself and stops with the remaining files named, so
guessing wrong costs one command, not a bad tag. The PR can also be
smaller than the file list below when earlier work already finalized some
surfaces: `prepare` writes whatever is still pending and leaves the rest
alone, so a preparation PR that promotes only `CHANGELOG.md` is a normal
outcome, not a sign something was missed.

```bash
git switch --create "release-prep-v${TP_VERSION}" "origin/${TP_TRUNK_BRANCH}"
tools/release/tensorplate-release.sh prepare \
  --version "${TP_VERSION}" \
  --prep-branch "release-prep-v${TP_VERSION}" \
  --execute \
  --confirm "PREPARE-v${TP_VERSION}"
git add -- CMakeLists.txt Cargo.toml Cargo.lock vcpkg.json \
  packaging/VERSION packaging/debian/changelog \
  packaging/scripts/install.sh CHANGELOG.md
git commit -m "Prepare v${TP_VERSION} release"
gh pr create --base "${TP_TRUNK_BRANCH}" --title "Prepare v${TP_VERSION} release"
```

`prepare` refuses to touch anything outside that file list, so the PR is
exactly the version surfaces and nothing else. `docs/release/notes/vX.Y.Z.md`
is deliberately not in it — that file ships in the implementation PR, above.

Merge it under the ordinary review and CI rules. Squash or merge commit
both work: the tag is created afterwards, on whatever commit the merge
actually produced. **Record that commit** — step 3b tags it by name, and
it is the only thing that distinguishes the release commit from whatever
else lands on the trunk next:

```bash
export TP_RELEASE_COMMIT="$(gh pr view "release-prep-v${TP_VERSION}" \
  --json mergeCommit --jq '.mergeCommit.oid')"
echo "${TP_RELEASE_COMMIT}"
```

When 3a was skipped because the metadata was already final, there is no
merge commit to read. The release commit is then the reviewed trunk commit
whose CI you confirmed green in step 1; set `TP_RELEASE_COMMIT` to that SHA
explicitly rather than letting step 3b take whatever the trunk holds.

#### 3b. Stand on the release commit and tag it

```bash
git switch "${TP_TRUNK_BRANCH}"
git pull --ff-only
git rev-parse HEAD           # compare against ${TP_RELEASE_COMMIT}
```

`git pull` brings the trunk's **head**, which is not necessarily the
release commit. Any PR that merged between the preparation merge and this
pull is now HEAD, and downstream may well not notice: that commit is still
an ancestor of the trunk, so the ancestry check passes, and unless the
intervening PR added a `[Unreleased]` entry — which a docs, test, or
CI-only PR does not — `prepare` finds nothing left to fold, so the
metadata gate still reads as final. The tag would land on a commit nobody
reviewed for this release and CI would publish it.
That is why 3a recorded `TP_RELEASE_COMMIT`. If HEAD is not that commit,
stand on it — the trunk is never pushed by this procedure, so the reset is
local only:

```bash
git reset --hard "${TP_RELEASE_COMMIT}"
```

`cut` tags a commit behind the trunk head deliberately and says so
(`HEAD is behind origin/${TP_TRUNK_BRANCH}; tagging that older trunk
commit`). Pass `--expect-commit` so the check is enforced rather than
remembered.

Preview the local operation:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --release-branch "${TP_TRUNK_BRANCH}" \
  --expect-commit "${TP_RELEASE_COMMIT}" \
  --final \
  --dry-run
```

Cut the tag locally. Use `--final` for the final tag and `--rc N` for a
candidate; `${TP_TAG}` already carries whichever one the variable block at
the top selected, so the confirmation token is the same line either way:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --release-branch "${TP_TRUNK_BRANCH}" \
  --expect-commit "${TP_RELEASE_COMMIT}" \
  --final \
  --execute \
  --confirm "CUT-${TP_TAG}"
```

For a release candidate, with `TP_RC` set to its number:

```bash
tools/release/tensorplate-release.sh cut \
  --version "${TP_VERSION}" \
  --release-branch "${TP_TRUNK_BRANCH}" \
  --expect-commit "${TP_RELEASE_COMMIT}" \
  --rc "${TP_RC}" \
  --execute \
  --confirm "CUT-${TP_TAG}"
```

`cut` refuses a dirty worktree, a checkout that is not on the trunk, a
commit `origin/${TP_TRUNK_BRANCH}` does not already contain, a HEAD that
is not `--expect-commit`, an existing tag, and release metadata that is
not yet final. The ancestry condition is the same one the publish workflow
asserts in step 5, so a tag CI would reject is never created in the first
place.

Nothing is pushed here, and nothing needs to be: the preparation PR already
moved the trunk. Confirm the commit the tag names — the next step builds
from it:

```bash
git rev-list -n1 "${TP_TAG}"
```

### 4. Build Release Assets Without Publishing

Before pushing a final tag or creating a public prerelease, run the
`Release` workflow manually:

- `tag`: `${TP_TAG}`
- `publish`: `false`
- `speech_runtime`: `off`, the default. `stub` also rehearses the speech
  runtime package job: it builds that family from stub wheels and adds the
  eight stand-in packages to the unsigned bundle, which then holds 21
  `.deb` files instead of 13. The stand-ins serve no model, and the mode is
  refused with `publish: true`.
- `source_ref`: the tag's commit, **not** a branch name:

  ```bash
  git rev-parse "${TP_TAG}^{commit}"
  ```

A branch name moves. Dispatching on `${TP_TRUNK_BRANCH}` validates
whatever the trunk holds when each job starts, so a PR merging mid-run
rehearses a tree that is not the one the tag names — and the jobs resolve
the ref independently, so two of them can build two different trees under
one release identity. The workflow resolves `source_ref` once and hands
every dependent job the resolved commit, and passing the tag's own commit
makes the rehearsal build exactly what the tag will publish.

Leaving `source_ref` empty checks out `${TP_TAG}` instead, which requires
the tag to be pushed already — the opposite of what this step is for.

The build-only run must:

- Build Rust release binaries.
- Build the C++ serving worker.
- Build the complete `amd64` runtime package set — agent, serving worker,
  observability, CLI, and the metapackage (hosted job).
- Build the `tensorplate-python` wheel + sdist at the release version.
- Run `test/packaging/run.sh`.
- Build all required `.deb` packages.
- Copy `install.sh`.
- Generate `tensorplate-${TP_TAG}-artifacts.json` and `SHA256SUMS`.
- Upload the `tensorplate-${TP_TAG}-release-assets-unsigned` workflow
  artifact (the signed `…-release-assets` name is produced only on the
  publish path).
- Skip signing and provenance, which run only on the publish path; the
  uploaded assets are unsigned.
- Stop before creating a GitHub Release.

Download the workflow artifact and smoke-test the installer from the
artifact directory before publication. The installer does **not** auto-detect
sibling artifacts: without `--local-artifacts` it downloads the pinned
published release, which does not exist yet for the tag under validation.
Build-only assets are also unsigned, so pass `--allow-unsigned`:

```bash
sudo bash install.sh --local-artifacts "$(pwd)" --allow-unsigned
sudo bash install.sh --local-artifacts "$(pwd)" --cli-only --allow-unsigned
```

Publish-grade bundles always carry the `amd64` runtime set, so run both the
full-runtime and `--cli-only` smokes on an Ubuntu x86_64 host. Only a
single-architecture local-source snapshot can lack a matching package.

### 5. Watch CI Build And Publish Assets

After build-only validation passes, push the annotated tag. The tag push is
what starts the publish workflow from the tag ref:

```bash
git push origin "${TP_TAG}"
```

Open the `Release` workflow run for `${TP_TAG}`. It must:

- Verify `${TP_TAG}` is annotated.
- Verify annotated tag and trunk ancestry: the tag commit is an ancestor
  of `origin/${TP_TRUNK_BRANCH}`.
- Build Rust release binaries.
- Build the C++ serving worker.
- Build the complete `amd64` runtime package set and the
  `tensorplate-python` wheel + sdist.
- Run `test/packaging/run.sh`.
- Build all required `.deb` packages.
- Generate `tensorplate-${TP_TAG}-artifacts.json` and `SHA256SUMS`.
- Sign `SHA256SUMS` with keyless cosign and record SLSA build provenance
  for the packages, installer, wheel/sdist, manifest, and checksums.
- Create the GitHub Release and attach the `.deb` packages, `install.sh`,
  the wheel + sdist, manifest, checksum file, and `SHA256SUMS.cosign.bundle`.
  RC tags are public prereleases; final tags are created as **drafts**, then
  the approval-gated `publish-github` job un-drafts them (Step 9).
- Read back the asset names the new release serves and fail unless they
  are exactly the names `SHA256SUMS` lists, plus `SHA256SUMS` and its
  bundle. GitHub stores an asset under a name of its own choosing (it
  serves `~` as `.`), and a release whose signed list names a file it does
  not serve cannot be installed or verified. A final release is still a
  draft at this point; a candidate is already public, and a failure here
  means it must be superseded by the next RC.

The workflow refuses to replace an existing GitHub Release. If it fails
after creating no release, fix the trunk, cut a new RC tag, or delete only
the failed unpublished tag according to the tag policy.

### 6. Download And Verify CI Assets

After the workflow succeeds, download the assets from the GitHub Release
and verify checksums from a clean machine:

```bash
mkdir -p "${TP_RELEASE_DIR}"
gh release download "${TP_TAG}" \
  --dir "${TP_RELEASE_DIR}" \
  --pattern '*.deb' \
  --pattern 'install.sh' \
  --pattern 'tensorplate_python-*.whl' \
  --pattern 'tensorplate_python-*.tar.gz' \
  --pattern 'SHA256SUMS' \
  --pattern 'SHA256SUMS.cosign.bundle' \
  --pattern "tensorplate-${TP_TAG}-artifacts.json"
cd "${TP_RELEASE_DIR}"
cosign verify-blob \
  --bundle SHA256SUMS.cosign.bundle \
  --certificate-identity-regexp "^https://github.com/tensorplate/tensorplate/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS
sha256sum -c SHA256SUMS
# A candidate's package file names spell its `~rc.N` as `.rc.N`, the name
# GitHub serves; the package's own Version keeps the tilde.
gh attestation verify "tensorplate-agent_${TP_VERSION}${TP_RC:+.rc.${TP_RC}}-1_arm64.deb" \
  --repo tensorplate/tensorplate
```

Review the manifest for package names, versions, architecture, release
commit, source tag, checksums, and validation links. Verify the cosign
signature (authenticity) and provenance attestation before checksums; a
checksum match alone does not prove the assets came from the release
workflow.

### 7. Run Clean-Room Validation

Run [`docs/validation/clean-room-release-smoke.md`](../validation/clean-room-release-smoke.md)
from the GitHub Release assets. The validation must download release
assets and verify checksums before install. Local source-tree binaries or
local package build directories invalidate the evidence.

Record the result in `${TP_RELEASE_DIR}/clean-room.md`. If the result is
`block`, do not publish or announce the release. For an RC, fix the
blocker and cut the next RC tag. For a final release, leave the draft
unpublished and cut a corrected release tag. If the result is
`conditional-pass`, the risk, mitigation, owner, and follow-up issue must
also appear in release notes.

### 8. Record Sign-Off And Evidence

```bash
cp docs/release/signoff-template.md "${TP_SIGNOFF}"
cp docs/release/evidence-template.md "${TP_RELEASE_DIR}/evidence.md"
```

Fill the copied files with final decisions and links to:

- The `Release` workflow run.
- The GitHub Release URL.
- The downloaded manifest/checksum verification transcript.
- The clean-room validation report.
- Reviewer approvals.

### 9. Publish And Announce

For a final tag, the `Release` workflow signs and attests the build, creates
the GitHub Release as a **draft**, and then fans out to four parallel publish
jobs — `publish-pypi`, `publish-apt`, `publish-homebrew`, and
`publish-github` — each **paused on its own protected deployment environment**
(`pypi`, `apt`, `homebrew`, `github-release`). Every channel publishes the same
cosign-signed, checksum-covered artifacts from this one build; release
candidates are gated out of all four.

Approve each channel from the Actions run page only after release evidence is
accepted. Channels are independent — approve in any order, or hold any one:

- **`publish-pypi`** (env `pypi`) — uploads the signed wheel + sdist via PyPI
  Trusted Publishing. PyPI is **immutable**: approve last-mile and with full
  intent; a published version cannot be replaced.
- **`publish-apt`** (env `apt`) — builds, signs, and syncs the stable APT
  repository from this run's signed assets, then validate per
  [`apt-repository.md`](./apt-repository.md). Re-runnable on failure.
- **`publish-homebrew`** (env `homebrew`) — renders the five component
  formulas and the `tensorplate` meta-formula from
  `packaging/homebrew/Formula/`, then opens one auto-merge PR in
  [`tensorplate/homebrew-tap`](https://github.com/tensorplate/homebrew-tap).
  Every formula points at the same tagged source archive and checksum. The job
  finishing means "PR opened with auto-merge armed", not "merged": the tap CI
  (audit + build-from-source + `brew test` on Apple Silicon) gates the merge,
  so the tap goes live eventually-consistently. Re-runnable.
- **`publish-github`** (env `github-release`) — un-drafts the GitHub Release
  (`gh release edit --draft=false --latest`), making the assets public.

Holding one channel does not block the others. A held or failed `publish-apt` /
`publish-homebrew` can be re-run to completion from the run page; `publish-apt`
also has the manual `apt-repo.yml` (`workflow_dispatch`) republish/recovery
path. PyPI is not retried for a version that already published.

Then publish the announcement using the release notes as the source of truth.
The announcement must not claim support beyond release evidence.

#### Release publishing environments (one-time external setup)

The parallel flow requires these to exist before the first final tag, mirroring
the external-setup discipline of earlier releases:

- Protected environments with **required reviewers**: `pypi` (already present),
  `apt`, `homebrew`, and `github-release`. The reviewer approval on each is the
  per-channel go-live gate; an environment left without a reviewer publishes
  without a hold.
- A **`HOMEBREW_TAP_TOKEN`** secret — a fine-grained PAT or (preferred) GitHub
  App token with `contents` + `pull_requests` write **scoped to
  `tensorplate/homebrew-tap` only**.
- In `tensorplate/homebrew-tap`: **auto-merge enabled** and a **required status
  check** set, so the bump PR waits for the tap CI before merging. The tap
  `main` must require **0 approving reviews** (or grant the bump bot a bypass
  actor): the automation cannot approve its own PR, so a required review would
  leave the bump open forever and the Homebrew channel would silently never go
  live after approval. The required CI check stays the merge gate.
- Opt-in repository variables: `PUBLISH_SDK_TO_PYPI=true` (PyPI),
  `PUBLISH_HOMEBREW_FORMULA=true` (Homebrew); APT runs when `TP_APT_REPO_DEST`
  is set. The existing `TP_APT_*` vars/secrets carry over unchanged.

### 10. Monitor After Release

For the first release window:

- Watch install failures and `tensorplate doctor` reports.
- Triage security reports privately according to `SECURITY.md`.
- Track artifact download or checksum problems.
- Open follow-up issues for conditional risks and docs corrections.
- Use the hotfix process in [`post-release.md`](./post-release.md).

## Stop-The-Release Criteria

Stop and mark the release blocked if any item is true:

- Required validation gate is not pass or signed conditional-pass.
- Required CI is unavailable or not green.
- The trunk carries unreviewed commits at or below the tag commit.
- The tag commit is not already contained in `origin/${TP_TRUNK_BRANCH}`.
- The tag commit is not `${TP_RELEASE_COMMIT}`, the commit step 3a
  recorded. Pass `cut --expect-commit` so this is enforced rather than
  eyeballed: ancestry alone does not catch a trunk that moved between the
  preparation merge and the pull.
- Version metadata or changelog is inconsistent.
- The final tag already exists.
- Artifacts are missing, checksums mismatch, or manifest commit/tag data
  does not match the release.
- The `cosign verify-blob` signature over `SHA256SUMS`, or any
  `gh attestation verify`, fails — a checksum match alone does not prove the
  assets came from the release workflow.
- Required sign-off is missing.
- Clean-room validation uses local build-tree paths.
- Clean-room install, doctor, service start, deploy, inference, status,
  logs, metrics, or rollback fails without signed conditional-pass.
- Release notes overstate supported hardware, model classes, backends, or
  security posture.
