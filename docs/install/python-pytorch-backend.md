# Installing the Python/PyTorch backend

The Python/PyTorch sidecar backend is required for SmolVLA validation
(release validation) and for any bundle whose `manifest.json` declares
`backend_hint: python_pytorch`. It is **not** part of the core
TensorPlate install:

- `tensorplate-agent`, `tensorplate-serving`, `tensorplate-observability`,
  and `tensorplate-cli` do not depend on it.
- PyTorch is not declared as a Debian dependency — Jetson aarch64 hosts
  need NVIDIA's PyTorch wheel, CPU hosts get the upstream wheel, and the
  Debian dependency machinery cannot express that choice.
- `tensorplate doctor` reports `python_pytorch_backend = missing` when
  the package or its runtime are absent. Deploys of a `python_pytorch`
  bundle fail before staging with a typed `BackendUnrunnable` error.

## 1. Install the backend package

```bash
sudo apt install tensorplate-backend-python-pytorch
```

This installs:

| Path | Purpose |
| --- | --- |
| `/usr/lib/tensorplate/backends/python_pytorch/` | Sidecar Python package source. |
| `/usr/lib/python3/dist-packages/tensorplate_pytorch_backend.pth` | Makes the sidecar package importable from the descriptor's `/usr/bin/python3`. |
| `/usr/bin/tensorplate-backend-python-pytorch` | Console entrypoint wrapper for direct diagnostics. |
| `/usr/share/tensorplate/backends/python_pytorch/backend.json` | Backend descriptor read by `tensorplate doctor` and the agent, and, for its runner profiles, by the serving worker's sidecar launcher. |
| `/usr/share/doc/tensorplate-backend-python-pytorch/` | README mirror. |

The descriptor is intentionally a separate file so doctor probes do not
have to walk arbitrary Python environments to discover the backend. Its
schema is `protocol/schemas/backend_descriptor.json`.

The schema also allows a `runner_profiles` list: installed runner profiles,
each naming the interpreter environment its sidecar runs in, apart from
`python.interpreter` (profiles may share one), with the environment's root,
any library directories for the sidecar's shared-library search path, the
packages that install the profile and the compute types it can load. A descriptor without the list declares no runner profiles, and
`python.interpreter` keeps serving every model. The packaged descriptor lists
none: a package that installs a profile declares it in a file of its own
under `runner_profiles.d/` beside the descriptor, and readers merge those
files into the list (see
[Runner profile declarations](speech-runtime.md#runner-profile-declarations)).

## 2. Install PyTorch into the descriptor's interpreter

The descriptor pins which Python interpreter the sidecar uses. Open it:

```bash
jq . /usr/share/tensorplate/backends/python_pytorch/backend.json
```

Look for the `python.interpreter` field (default `/usr/bin/python3`).
Install PyTorch into that interpreter. The exact command depends on
the platform:

```bash
# Jetson Orin (aarch64, CUDA): use NVIDIA's PyTorch wheel matrix.
# See https://forums.developer.nvidia.com/t/pytorch-for-jetson/72048.
sudo apt install \
  libcudnn9-cuda-12 \
  libcufile-12-6 \
  cuda-cupti-12-6 \
  cuda-libraries-12-6
sudo /usr/bin/python3 -m pip install --upgrade pip wheel
sudo /usr/bin/python3 -m pip install <jetson torch wheel URL>

# x86_64 CPU host (development, and the ubuntu*-x86-cpu rows): use the CPU
# index. Plain `pip install torch` on Linux resolves to the CUDA build and
# pulls gigabytes of NVIDIA wheels onto a host with no accelerator.
sudo /usr/bin/python3 -m pip install \
  --index-url https://download.pytorch.org/whl/cpu 'torch>=2.1'
```

For the JetPack 6.2 / CUDA 12.6 release validation target, the tested wheel
source was the Jetson AI Lab JP6 CUDA 12.6 index:

```bash
sudo /usr/bin/python3 -m pip install --no-cache-dir \
  --index-url https://pypi.jetson-ai-lab.io/jp6/cu126 \
  torch==2.8.0
```

If `import torch` fails with a missing CUDA shared library
(`libcudnn.so.9`, `libcufile.so.0`, `libcupti.so.12`, `libcusparse.so.12`,
etc.), install the apt packages above and rerun `tensorplate doctor`.

The descriptor's `pytorch.minimum_version` field is what `doctor` and
the agent compare against. Override the descriptor if the wheel you
chose pins a different `torch.__version__`:

```bash
sudo $EDITOR /usr/share/tensorplate/backends/python_pytorch/backend.json
```

The descriptor file is not a dpkg conffile (it ships with the optional
backend package). If you rewrite it, keep a copy alongside your
deploy notes; the next package upgrade will overwrite it.

## 3. Verify with `tensorplate doctor`

```bash
tensorplate doctor
```

The relevant findings are stable strings (the release validation harness asserts
on them):

| Finding ID | Status meanings |
| --- | --- |
| `python_pytorch_backend` | `ok` (descriptor + PyTorch present), `missing` (descriptor missing), `fail` (descriptor malformed), `warning` (Python/PyTorch version below minimum). |
| `python_pytorch_runtime` | `ok` / `missing` / `warning` for the PyTorch import and its declared minimum version. |

The default agent config already lists `python_pytorch` as an available
backend. When the descriptor is absent the startup probe records a
typed missing-backend report; once the backend package and PyTorch are
installed a new agent start probes the same backend as runnable.

When the doctor reports green for both, restart the agent so it refreshes
its startup probe cache, then run a `python_pytorch` deploy:

```bash
sudo systemctl restart tensorplate-agent
tensorplate deploy /var/lib/tensorplate/bundles/staging/smolvla.tpmodel
```

If the descriptor is missing or the interpreter cannot import `torch`,
the deploy fails with a typed `BackendUnrunnable` error **before** any
files are staged. It will not silently fall through to first inference.

## Async-policy route support

The v0.1.0 Python/PyTorch adapter advertises `supports_async=false`.
Use `/infer` for Python-backed serving. The LeRobot-compatible
`/policy/infer`, `/policy/result/<id>`, and `/policy/cancel/<id>` route
family requires a backend with native async/cancel support and returns
501 with `error.code = "unsupported"` for Python-backed sessions.

## What the probe does and does not do

- **Does**: `stat` the descriptor file; run `python3 -c 'import sys; ...'`
  and `python3 -c 'import torch; print(torch.__version__)'` against
  the interpreter pinned in the descriptor; compare versions.
- **Does not**: install anything, run model code, mutate the descriptor,
  scan the filesystem, or reach the network. The probe is read-only
  and bounded (it returns within seconds even when Python is missing).

## Candidate speech runner profile: `faster_whisper`

The `faster_whisper` runner profile transcribes speech with a
Whisper-family model converted for CTranslate2, through faster-whisper. It
is a candidate: it runs on the tensor-only `/infer` path through a
candidate-only convention that goes away when a speech bundle format
lands, so do not publish a bundle that depends on it (see
`test/models/bundles/v0_1/README.md`, "Candidate speech fixtures").

Its runner entry, the bundle's `python_pytorch_entry` JSON, selects it with
`"backend_profile": "faster_whisper"`. The profile refuses an entry that
does not name it, so the sidecar's default-backend setting cannot load a
model through it. The entry declares everything about the model the
profile uses; nothing about a particular checkpoint is built in:

| Field | Meaning |
| --- | --- |
| `model_directory` | The converted model directory, relative to the entry. Every file in it, `tokenizer.json` included, is listed in `artifact_set`; without `tokenizer.json` faster-whisper would fetch a tokenizer from the Hugging Face Hub. |
| `device` | `cuda` or `cpu`. |
| `compute_type` | The CTranslate2 compute type to load with, such as `float16` or `int8_float16`, as CTranslate2 names it once loaded. `auto`, `default` and `int8` are refused: for those CTranslate2 chooses the type, or for `int8` its float half, from the device and the model. |
| `languages` | The Whisper language codes the deployment serves, such as `["en", "ar"]`. |
| `sample_rate_hz` | The rate of the PCM requests send, which must be the model's own input rate. |
| `artifact_set` | Every file the profile reads, by relative path and `sha256:` digest. |

A load verifies every listed digest before the model is built, and fails
with `unsupported` when CTranslate2 sees no CUDA device for `cuda` or the
device lacks the compute type, or when the model spec's `precision_hint`
is neither `auto` nor the precision of the entry's compute type (`fp32`,
`fp16` and `bfloat16` name the float type of that name, `int8` any
`int8_*` type). It fails with `config_invalid` when the model's input rate
differs from `sample_rate_hz`, or when a declared language is one the
model's tokenizer lacks or, for an English-only model, anything but `en`,
which faster-whisper would otherwise quietly decode as English. It fails
with `load_failed` when CTranslate2 reports a compute type other than the
entry's after loading.

A request carries two tensors: `audio_frames`, a one-dimensional `int16`
tensor of mono little-endian PCM at the entry's rate, from one sample to
one model input window (30 seconds for Whisper); and `text_utf8`, the
request's language code, which must be one the entry declares. Every
request decodes the same way: greedily (beam size 1) at temperature 0
with no fallback, without conditioning on earlier text, with word
timestamps, and with faster-whisper's voice-activity filter off. The
response is one `result_json` tensor:

```text
{
  "language": "<the request's language code>",
  "text": "<the segments' texts, joined>",
  "segments": [
    {"start_us": <int>, "end_us": <int>, "text": "<text>", "tokens": [<int>, ...],
     "words": [{"start_us": <int>, "end_us": <int>, "text": "<word>", "probability": <number>}, ...]},
    ...
  ],
  "sample_rate_hz": <the entry's rate>,
  "sample_count": <samples in audio_frames>,
  "decode_options": {"beam_size": 1, "temperature": 0.0, "condition_on_previous_text": false,
                     "vad_filter": false, "word_timestamps": true},
  "runtime": {"device": "<cuda or cpu>", "compute_type": "<as CTranslate2 reports it>",
              "faster_whisper": "<version>", "ctranslate2": "<version>"},
  "timings_us": {"load": <int>, "artifact_verify": <int>, "model_build": <int>,
                 "decode": <int>}
}
```

Times are faster-whisper's, rounded to whole microseconds from the start
of the clip. Whisper decodes a zero-padded window, so an end past the clip
is cut to the clip's end; an interval that starts after the clip or ends
before it starts fails the request with `inference_failed`.
`runtime.compute_type` is the type CTranslate2 reports after loading.
`timings_us` gives the profile's whole load (`load`), its digest
verification (`artifact_verify`) and model build (`model_build`), and the
request's `decode`, which includes consuming faster-whisper's lazy segment
generator: the model decodes as the generator is consumed.

A load runs within `TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS`, which also
covers the adapter's side of the exchange. A request runs within both
`TP_PYTHON_PYTORCH_INFER_TIMEOUT_MS` and the serving worker's
`http.request_timeout_ms` (8 seconds in the packaged configuration), which
bounds a whole `/infer` exchange: a decode that outlasts it loses its
response. The sidecar serves one request at a time and answers nothing
else while it decodes.

A request fails with `shape_mismatch` when its tensors are not exactly
those two or the audio is not a one-dimensional `int16` tensor of one
sample to one window, `config_invalid` when `text_utf8` is not UTF-8, and
`unsupported` for an undeclared language. A CTranslate2 out-of-memory
failure, which it raises as a plain `RuntimeError`, is `oom_error` in a
load or a request; any other failure is `load_failed` or
`inference_failed`.

The profile imports `faster_whisper`, `ctranslate2` and `numpy` only when
a model loads. `pip install ".[speech-stt]"` in `backends/python_pytorch/`
installs faster-whisper and CTranslate2, unpinned, for development.

## Candidate speech runner profile: `kokoro`

`kokoro` synthesizes one bounded text input on the same candidate-only
batch path as `faster_whisper`. Its entry must name `backend_profile:
"kokoro"`; the default-backend setting alone cannot enable it. The runner
is a Kokoro-family adapter: its checkpoint, config, language and voice are
bundle assets. The reference fixture declares en-US and af_heart; those
values are not built into the runner.

| Entry field | Meaning |
| --- | --- |
| `model_config`, `model_weights` | Relative paths to the local JSON config and checkpoint. |
| `device`, `compute_type` | `cuda` or `cpu`, and `float32`. An explicit model-spec precision other than `fp32` is refused; the legacy `auto` hint is accepted but never chooses the loaded precision. |
| `languages` | Nonempty list of distinct language tags (at most 16 UTF-8 bytes each), recognized by the installed Kokoro pipeline's language registry. |
| `voices` | Map of voice IDs (1–64 UTF-8 bytes) to `{"path": "<local .pt>", "language": "<declared tag>"}`. Voice paths cannot contain commas, which upstream interprets as mixing voices. |
| `language`, `voice` | Static selections from those declarations; the selected voice must declare this language. Requests cannot change either. |
| `sample_rate_hz` | Positive integer output rate, checked against the loaded decoder's excitation source. Kokoro's JSON config does not carry a sample rate. |
| `artifact_set` | Every local file the entry references, each with a SHA-256 digest. |

All fields are required and unknown fields reject. Model and voice files
are reached through the shared artifact-set verifier. The runner constructs
`KModel(config=<local config>, model=<local checkpoint>)`, converts its
parameters to float32 on the declared device, checks parameter/buffer
precision and device, and passes that instance to `KPipeline`. The selected
voice loads from its verified local `.pt` path; finite CPU float32 style
tensor dimensions must agree with the model config. A missing packaged
`en_core_web_sm` dependency rejects before pipeline construction, preventing
misaki's automatic spaCy download. Missing English espeak fallback also
fails load instead of silently skipping out-of-vocabulary words. Other
language dependencies must already be installed by the chosen runtime.

One request carries exactly one `text_utf8` tensor: strict UTF-8, nonblank,
no NUL, at most 4,096 bytes and 4,096 characters. Phonemization runs before
GPU synthesis. It must produce 1–200 phoneme characters, fit the loaded
model context and voice table, and use only the model's vocabulary. This
uses `g2p` followed by `generate_from_tokens` with a phoneme string, avoiding
the upstream text pipeline's silent truncation. Speed is fixed at 1.0.

The result contains an `audio_frames` tensor of mono PCM16LE and a
`result_json` tensor. Metadata includes `language`, `voice`,
`model_digest`, `config_digest`, `voice_digest`, `sample_rate_hz`,
`sample_count`, `duration_us` (duration rounded up to whole microseconds),
`dtype: "float32"` for the source waveform, and `clipped_samples` for source
samples outside [-1, 1]. `runtime` records the checked device/compute type
and Kokoro/PyTorch versions. `timings_us` reports whole-profile `load`,
`artifact_verify`, `model_build`, `phonemize`, `synthesize` (including full
generator consumption and the CPU copy), and `pcm` conversion.

A waveform must be finite, one-dimensional float32, nonempty and at most
30 seconds at the declared rate. Conversion clamps to [-1, 1], multiplies
by 32768, rounds ties to even, saturates to [-32768, 32767], then writes
little-endian int16. No resampling or streaming chunks are implied.
Transient memory includes the float waveform, bounded conversion copies
and PCM output. Unload drops the pipeline, model and cached voice, collects
Python garbage, then empties the CUDA allocator cache. Device-allocation
release beyond the allocator baseline still needs hardware measurement.

Malformed entries/text are `config_invalid`, invalid tensors are
`shape_mismatch`, undeclared selections or unrepresentable phonemes are
`unsupported`, and dependency/load/synthesis failures are `load_failed` or
`inference_failed`. Out-of-memory classes and torch's out-of-memory
`RuntimeError` become `oom_error`. Errors and logs never contain upstream
exception text, voice names or request text.

The same startup, infer and whole-HTTP-exchange timeouts described above
apply. Health and cancellation wait behind synchronous synthesis; this
profile does not advertise responsive job control or streaming. Real
Kokoro model loading, L4 synthesis, memory release, cancellation/crash
behavior and listening quality are pending candidate qualification.

### Deserialization threat review

Checkpoint `.pth` and voice `.pt` files are pickle-backed. The approved
bundle entry and its immutable, operator-provisioned artifact directory
are the trust root: SHA-256 verification establishes identity, not safety
of an arbitrary checkpoint. All digests are checked before deserialization;
resolved paths must stay inside that root and appear in the allowlist.
Writable bundle roots are outside this trust boundary: verification does
not eliminate a concurrent replacement between hashing and opening a file.

Kokoro 0.9.4's [model loader](https://github.com/hexgrad/kokoro/blob/1c7bdd971d2e32981f0f2b92d1fe5b0051e21457/kokoro/model.py)
and [voice loader](https://github.com/hexgrad/kokoro/blob/1c7bdd971d2e32981f0f2b92d1fe5b0051e21457/kokoro/pipeline.py)
explicitly use `torch.load(weights_only=True)`; model weights map to CPU.
The runner also sets `TORCH_FORCE_WEIGHTS_ONLY_LOAD=1` in its dedicated
sidecar before importing the engine. It never registers custom pickle
safe globals or retries with unrestricted loading. A load requiring custom
objects fails. Restricted loading still permits resource exhaustion and
is not a sandbox for untrusted model code or native libraries; the
[PyTorch security policy](https://github.com/pytorch/pytorch/blob/main/SECURITY.md)
describes that boundary. Qualify only approved checkpoints with the packaged,
patched dependency lock and service isolation. CPU fakes verify the loading
path and rejection guards; real artifact deserialization remains part of
the L4 run.

For local development, `pip install ".[speech-tts]"` installs unpinned
Kokoro, torch, NumPy and misaki English dependencies. Kokoro 0.9.4 supports
Python 3.10–3.12; the sidecar's dependency-free CI also runs on 3.14. The
extra does not install spaCy model data or distribution espeak assets and
does not replace the appliance's locked runtime packages.

## Errors and logs

The sidecar keeps request content out of everything it reports. A failed
load, prime or inference returns one of the typed error codes with a short
message the backend itself wrote, or, when that message would repeat an
upstream library's exception text, a fixed message for the code; an error
never carries a `context`. An exception a backend did not type becomes
`internal` (`oom_error` for an out-of-memory class) with the fixed message.
Messages name no file path, request text or voice and no name a request
chose, because upstream libraries can put exactly those in their exception
text; the sidecar's health payload keeps the same message as `last_error`.

The sidecar's own log lines name an exception's class and the file, line
and function it was raised from, never its message, and drop tracebacks.
Records from other libraries' loggers and Python warnings are replaced by a
line naming the logger and level. The sidecar's log is the only thing that
reaches the stdout and stderr it shares with the serving worker: text that
Python code writes to its standard streams is discarded after one line
saying so, and what native libraries write to those descriptors is
discarded without one. The upstream detail is recorded nowhere; reproduce
the failure outside the service to see it.

## Tuning environment variables

The adapter and the SmolVLA backend both read environment variables for
host-specific tuning. Set them in the agent's environment (e.g.
`Environment=` in `tensorplate-agent.service` or the doctor's `env-file`)
so they are applied at process start.

### Adapter (consumed by the C++ runtime and Python runner)

| Variable | Default | Purpose |
| --- | --- | --- |
| `TP_PYTHON_PYTORCH_EXECUTABLE` | `/usr/bin/python3` (from descriptor) | Interpreter the sidecar is launched with. Override when running from a virtualenv (e.g. the release validation venv). Falls back to `TP_TEST_PYTHON_EXE` then `TP_TEST_PYTHON` for the C++ test fixtures. Not consulted for a bundle that names a `runner_profile`: its sidecar runs under the interpreter the installed profile declares (see [Launching a runner profile's sidecar](../architecture/backend-registry.md#launching-a-runner-profiles-sidecar)). |
| `TP_PYTHON_PYTORCH_DEFAULT_BACKEND` | `fixture` | Selects the in-process backend factory. Set to `smolvla` to enable the LeRobot SmolVLA path. |
| `TP_PYTHON_PYTORCH_STARTUP_TIMEOUT_MS` | `15000` | Deadline for sidecar `start`/`load`/`prime`/`unload` exchanges. Increase on cold-cache or HuggingFace-download-heavy startups (90000 has been tested for Orin SmolVLA). |
| `TP_PYTHON_PYTORCH_INFER_TIMEOUT_MS` | `30000` | Per-request inference deadline (clamped by the caller's deadline). |
| `TP_PYTHON_PYTORCH_HEALTH_TIMEOUT_MS` | `2000` | Health-probe deadline. Sidecar runners that handle health off the inference thread can keep this tight; otherwise raise it to avoid spurious flaps. |

### SmolVLA backend (consumed when `default_backend = smolvla`)

| Variable | Default | Purpose |
| --- | --- | --- |
| `TP_SMOLVLA_MODEL_ID` | `lerobot/smolvla_base` | HuggingFace model id passed to `PreTrainedConfig.from_pretrained`. |
| `TP_SMOLVLA_CACHE_DIR` | `/var/lib/tensorplate/hf-cache` | HF cache directory shared by the policy, tokenizer, and config. |
| `TP_SMOLVLA_DEVICE` | `cuda` | Torch device string. Set to `cpu` for non-CUDA hosts. |
| `TP_SMOLVLA_NUM_STEPS` | _(model default)_ | Optional positive integer that overrides `PreTrainedConfig.num_steps` (inference rollout length). |
| `TP_SMOLVLA_TASK` | `pick up the cube\n` | Default language task when the inference frame omits explicit token inputs. |

Values from a per-bundle JSON config (`artifact_path`) take precedence
over the environment, which takes precedence over the built-in default.

## Removing the backend

```bash
sudo apt remove tensorplate-backend-python-pytorch
```

Removal does **not** touch the PyTorch install in the Python
environment. Operators who want to free that space should follow up
with `pip uninstall torch` in the descriptor's interpreter.
