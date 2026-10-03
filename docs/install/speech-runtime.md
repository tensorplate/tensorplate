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
- Nothing selects the environment yet. The backend descriptor declares no
  runner profile, so the sidecar launcher and `tensorplate doctor` still use
  the descriptor's `python.interpreter`.

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
