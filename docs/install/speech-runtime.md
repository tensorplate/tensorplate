# Speech runtime packages

The `tensorplate-speech-runtime-*` packages install the Python environment
the two speech runner profiles run in: `faster_whisper` (CTranslate2) and
`kokoro` (PyTorch). They are `amd64` packages built on and for Ubuntu 24.04.

They are built from source today (see
[`packaging/speech-runtime/README.md`](../../packaging/speech-runtime/README.md));
no release publishes them yet, and the `tensorplate` metapackage neither
depends on nor recommends them.

## Packages

| Package | Ships |
| --- | --- |
| `tensorplate-speech-runtime-base` | The virtual environment, the TensorPlate sidecar module installed into it, and the distributions both profiles share. |
| `tensorplate-speech-runtime-cublas` | The CUDA 12 cuBLAS library, used by CTranslate2 and PyTorch. |
| `tensorplate-speech-runtime-vad` | ONNX Runtime (CPU) and the soxr resampler. |
| `tensorplate-speech-runtime-ct2` | CTranslate2, faster-whisper and the audio decoder. |
| `tensorplate-speech-runtime-cuda` | The other CUDA 12 user libraries PyTorch loads, cuDNN and NVRTC included. |
| `tensorplate-speech-runtime-torch` | PyTorch built for CUDA 12. |
| `tensorplate-speech-runtime-kokoro` | Kokoro and its English grapheme-to-phoneme stack, with the spaCy model it needs. |
| `tensorplate-speech-runtime` | Metapackage: all seven. |

Package dependencies give each profile its closure:

| Runner profile | Install | Brings in |
| --- | --- | --- |
| `faster_whisper` | `tensorplate-speech-runtime-ct2` | `base`, `cublas`, `vad` |
| `kokoro` | `tensorplate-speech-runtime-kokoro` | `base`, `cublas`, `cuda`, `torch` |

Every package depends on `tensorplate-serving` at exactly its own version, so
the sidecar module in the environment always matches the serving worker that
talks to it. The base package also depends on
`tensorplate-backend-python-pytorch`, which ships the backend descriptor.

## Installing from a release

`install.sh --with-speech-runtime` installs the family with the core runtime
and `tensorplate-backend-python-pytorch`, in one `apt` transaction. It takes
the packages the release's signed manifest lists for `amd64` and needs an
`amd64` host on Ubuntu 24.04. A release that publishes no speech runtime
package refuses the option before anything is installed. That is every
release so far.

## What installing does

- Unpacks files under `/usr/lib/tensorplate/speech-runtime/` and a license
  manifest under `/usr/share/doc/<package>/`. The environment was built from
  a hash-locked wheelhouse when the package was built: installing downloads
  nothing and runs no pip, and the environment contains no pip.
- Uses the distribution's interpreter. The environment's `bin/python` is a
  link to `/usr/bin/python3.12` (the base package depends on
  `python3.12 (<< 3.13)`); no interpreter is redistributed. Bytecode caches
  ship in the packages as hash-checked caches, so nothing is compiled or
  written at run time.
- Uses the distribution's espeak-ng. The Kokoro package depends on
  `libespeak-ng1` and `espeak-ng-data`, and the environment's
  `espeakng_loader` module returns their paths.
- Declares the runner profile. `tensorplate-speech-runtime-ct2` and
  `tensorplate-speech-runtime-kokoro` each install one file beside the
  backend descriptor; see
  [Runner profile declarations](#runner-profile-declarations).
- Restarts a running `tensorplate-agent.service`, because the agent reads the
  installed package set and probes its backends only when it starts. Every
  `dpkg` run that installs, upgrades or removes a package of the family asks
  for one restart at its end, however many packages it handled; `apt` may
  use two runs for one transaction. Installing or reinstalling the base
  package alone restarts nothing. A stopped agent stays stopped. Each
  restart counts toward the unit's limit of five starts in 300 seconds.

## What it does not do

- It ships no speech model: no Whisper or Kokoro weights, voices or
  tokenizer files. Provision those with `tensorplate bundle provision`. Two
  auxiliary models do come inside the wheels: the Silero voice activity
  model in faster-whisper, and spaCy's `en_core_web_sm`, which Kokoro's
  English grapheme-to-phoneme step needs.
- Nothing runs in the environment yet. The installed profiles are declared,
  but the sidecar launcher and `tensorplate doctor`'s runtime probe still use
  the descriptor's `python.interpreter`.

## Runner profile declarations

The backend descriptor,
`/usr/share/tensorplate/backends/python_pytorch/backend.json`, belongs to
`tensorplate-backend-python-pytorch` and is the same file on every host, so
it cannot say which runner profiles are installed. Each profile's package
says so itself, in a file of its own beside the descriptor:

| Package | Installs |
| --- | --- |
| `tensorplate-speech-runtime-ct2` | `/usr/share/tensorplate/backends/python_pytorch/runner_profiles.d/faster_whisper.json` |
| `tensorplate-speech-runtime-kokoro` | `/usr/share/tensorplate/backends/python_pytorch/runner_profiles.d/kokoro.json` |

A declaration names the backend and holds one `runner_profiles` entry: the
profile id, its interpreter and environment root, its library search paths,
the packages that install it and the compute types it can load. The agent and
`tensorplate doctor` read the descriptor through one reader, which appends
every `*.json` file of that directory, in file name order, to the
descriptor's `runner_profiles`. A host with only the `ct2` package therefore
reads as having `faster_whisper` and not `kokoro`. Other files in the
directory are not declarations and are not read. Removing a profile's package
removes its declaration.

The reader refuses the whole descriptor, and with it every `python_pytorch`
deployment on the host, rather than drop one profile:

| Refused when | Reported as |
| --- | --- |
| A declaration is not valid JSON, has an unknown or missing member, another `schema_version`, or a profile that breaks a `runner_profiles` rule | the declaration's path and the fault |
| A declaration names another backend than the descriptor beside it | the declaration's path and both backend names |
| Two files declare one profile id, or a declaration repeats a profile the descriptor lists itself | both paths and the id |
| A profile names a package that is not installed | the declaration's path, the profile and the package |
| `dpkg-query` is missing, fails or does not answer within five seconds | the descriptor's path and the failure |

A package counts as installed in the dpkg states `installed`,
`triggers-pending`, `triggers-awaited` and `half-configured`. The last is
deliberate. The agent is restarted from the base package's own trigger, and
while that trigger runs dpkg reports the base package as `half-configured`,
so requiring `installed` would refuse the descriptor at the restart that is
meant to pick the family up. It is also the state of a package whose own
post-installation script failed; the family's component packages have no
such script, and the base package's does nothing when it is configured.

A package that is only `unpacked` does not count. dpkg leaves a package there
when it cannot configure it, as with a dependency that is not met or a
`tensorplate-serving` of another version, and after a `dpkg --unpack` that no
configure run has followed. The descriptor is refused until the packages are
configured, which restarts the agent again. A host with no declaration is
never asked about its packages, so nothing changes where the family is not
installed.

`tensorplate doctor` reports a refusal as a failed `python_pytorch_backend`
finding that carries the reason. The agent logs it at startup in its
`backend probe:` line, as `state=RunnerProfilePackageMissing` for a package
that is not installed and `state=DescriptorMalformed` for every other
refusal, and refuses `python_pytorch` deployments with the reason
`missing_backend_package` or `accelerator_runtime_unavailable` respectively.

## Licenses

The distributions keep their own licenses. Each package's
`/usr/share/doc/<package>/third-party-licenses.json` lists every distribution
it ships with its version, the SHA-256 of the wheel it was installed from,
its declared license and the license files it carries, which stay in place
under the environment. A distribution that declares no license is listed with
the recorded reason it was accepted. The Kokoro package includes a phonemizer
under GPL-3.0-or-later, the CTranslate2 package an audio library that bundles
FFmpeg with x264 and x265, and the cuBLAS and CUDA packages NVIDIA libraries
under NVIDIA's proprietary license. The family is not in the public package
repository; a redistribution review comes first.
