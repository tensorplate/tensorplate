# Speech runtime environment

Sources for the `tensorplate-speech-runtime-*` packages: a locked Python
environment for the `faster_whisper` and `kokoro` runner profiles, built into
`/usr/lib/tensorplate/speech-runtime` when the packages are built. What the
packages install and how they behave is in
[`docs/install/speech-runtime.md`](../../docs/install/speech-runtime.md).

| Path | What it is |
| --- | --- |
| `build-environment.py` | `fetch` downloads the locked artifacts into a wheelhouse; `build` installs them into the environment and splits it into one package tree per lock file. Only `fetch` uses the network. |
| `lock/<component>.txt` | The pins of one package, one `name==version --hash=sha256:<digest>` per line. The file a pin sits in decides the package that ships it. |
| `lock/index.txt` | The pip index options `fetch` uses. |
| `lock/sources.txt` | Pins that do not come from an index: a source archive built into a wheel, a wheel published only as a release asset, and the loader built from this tree. |
| `lock/self-test.py` | Run inside the built environment before it is packaged. |
| `lock/undeclared-licenses.txt` | Distributions accepted although they declare no license and ship no license file, each with its reason. Any other such distribution fails the build, and so does an entry for one that declares a license. |
| `espeakng-loader/` | A replacement for the upstream `espeakng-loader` wheel, which bundles its own espeak-ng. This one returns the paths of the distribution's `libespeak-ng1` and `espeak-ng-data`. |
| `requirements.in` | The input the lock was resolved from. |

## Building

On Ubuntu 24.04 amd64, with the packages `debian/control` names under the
`pkg.tensorplate.speech-runtime` profile installed:

```bash
python3.12 packaging/speech-runtime/build-environment.py fetch \
  --lock-dir packaging/speech-runtime/lock \
  --wheelhouse /var/tmp/speech-wheelhouse --work-dir /var/tmp/speech-fetch
```

```bash
DEB_BUILD_PROFILES=pkg.tensorplate.speech-runtime \
TP_SPEECH_RUNTIME_WHEELHOUSE=/var/tmp/speech-wheelhouse \
  packaging/scripts/build-deb.sh -B
```

The profile builds the family and nothing else; without it the family is not
built. The package build itself reads only the wheelhouse.

The packages take their version from `packaging/debian/changelog`, and each
depends on `tensorplate-serving` at exactly that version. To build for a
release candidate rather than the version the tree carries, stage the
candidate's package version before the build:

```bash
tools/release/stage-debian-changelog.sh 0.3.1~rc.1
```

The script edits the changelog in the working tree; the edit is not committed.

## Changing the lock

Every pin is a candidate until the release freezes its model set. To move
one, resolve `requirements.in` again for CPython 3.12 on
`x86_64-manylinux_2_28` with the PyTorch CUDA 12.9 index beside PyPI:

```bash
uv pip compile packaging/speech-runtime/requirements.in --python-version 3.12 \
  --python-platform x86_64-manylinux_2_28 --generate-hashes --no-header \
  --extra-index-url https://download.pytorch.org/whl/cu129 \
  --index-strategy unsafe-best-match
```

Then, for each pin, keep the digest of the one file pip selects on Ubuntu
24.04, in the lock file of the package that should ship it. One CUDA major is
shared by PyTorch and CTranslate2, so the two move together. Two digests
cannot be copied from an index: `docopt` is published as a source archive
only, and the loader is built from this tree. Both are built by `build` under
the locked setuptools with a fixed timestamp and umask, and their lock lines carry the
digest of the wheel that build produces;
`test/packaging/verify_speech_runtime_packages.sh` rebuilds both and fails
when either stops matching.
