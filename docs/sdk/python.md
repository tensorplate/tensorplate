# TensorPlate Python SDK

`tensorplate-python` (import package `tensorplate`) is the first-party
Python SDK for calling models you have **already deployed** with
TensorPlate, over the v0.1 serving `/infer` HTTP envelope. It is a
client-side library: it does not deploy, run, or change runtime serving,
and it provides no in-runtime camera/video ingest or DeepStream sink. Those
remain separate, deferred runtime capabilities (see
[Scope and non-goals](#scope-and-non-goals)). With the `speech` extra it
also holds an asyncio [streaming speech client](#streaming-speech-client)
for a worker's stream listener.

- **Distribution:** `tensorplate-python` (Apache-2.0, Python 3.10+)
- **Import package:** `tensorplate`
- **Wire contract:** the v0.1 serving envelope, `schema_version` `0.1`
  ([`protocol/schemas/serving_http_envelope.json`](../../protocol/schemas/serving_http_envelope.json))

## Install

```bash
pip install tensorplate-python            # core client (no third-party deps)
pip install "tensorplate-python[numpy]"   # + numpy for tensor array access
pip install "tensorplate-python[vision]"  # + numpy & Pillow for VisionClient.detect
pip install "tensorplate-python[speech]"  # + grpcio & protobuf, see "Speech extra"
```

`import tensorplate` and constructing a `ServingClient` with raw-bytes
tensors never import numpy or Pillow; the optional dependencies load lazily
only when you call an array or vision helper.

### Verified / air-gapped install

The same wheel + sdist are attached to each GitHub Release, cosign-signed and
covered by the release `SHA256SUMS`. Use this path to verify provenance or
install offline. Download the wheel and signature material:

```bash
export TP_VERSION=0.1.3
export TP_TAG="v${TP_VERSION}"
export TP_REPO=tensorplate/tensorplate
export TP_RELEASE_URL="https://github.com/${TP_REPO}/releases/download/${TP_TAG}"

curl -fL -O "${TP_RELEASE_URL}/tensorplate_python-${TP_VERSION}-py3-none-any.whl"
curl -fL -O "${TP_RELEASE_URL}/SHA256SUMS"
curl -fL -O "${TP_RELEASE_URL}/SHA256SUMS.cosign.bundle"
```

Verify the cosign signature over `SHA256SUMS` (authenticity) and then the
checksum (integrity) before installing — the same flow as the runtime
[external install](../install/external-install.md#verify-signature-and-checksums)
and [`SECURITY.md`](../../SECURITY.md):

```bash
cosign verify-blob \
  --bundle SHA256SUMS.cosign.bundle \
  --certificate-identity-regexp "^https://github.com/${TP_REPO}/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS
sha256sum -c SHA256SUMS 2>/dev/null | grep tensorplate_python
```

The signature must report `Verified OK` and the wheel must report `OK`.
Then install it (append `[vision]` for the detection helpers):

```bash
pip install "./tensorplate_python-${TP_VERSION}-py3-none-any.whl[vision]"
```

### Speech extra

The `speech` extra declares what the streaming speech transport needs:
`grpcio>=1.81.1,<2` and `protobuf>=6.33.5,<7`. The package's
[streaming speech client](#streaming-speech-client) is built on them; the
extra also fixes the dependency range and the
failure mode, and the wheel carries the
[stream schema's modules](#stream-schema-modules). On the server side, a
feature-ON serving worker with `streaming.enabled` opens a loopback gRPC
stream listener, but it refuses every `Open` until the worker runs speech
jobs (see
[serving-worker.md](../architecture/serving-worker.md#stream-listener)).
`import tensorplate`
never imports `tensorplate.speech`, `tensorplate.stream`, grpc or protobuf,
so the unary client keeps no third-party dependency. Importing
`tensorplate.speech` or `tensorplate.stream` without the extra raises
`MissingDependencyError`, which names the extra to install.

grpcio 1.81.1 is the gRPC version the serving worker links. The worker links
protobuf 6.33.4; the Python floor is one patch higher because
`grpcio-tools` 1.81.1 generates code with protoc 33.5, and generated code
refuses an older runtime. The wire format is the same.

[`sdk/python/constraints/speech.txt`](../../sdk/python/constraints/speech.txt)
pins those floors and grpcio's one dependency, each with the SHA-256 of
every file published for it; CI installs from it. It is compiled from the
extra (the command is in its header), and a test fails when a pin and the
extra's floor differ. To install exactly those versions, take the file from
the repository at the release tag and install it before the SDK:

```bash
python3 -m venv .venv
.venv/bin/pip install --require-hashes -r speech.txt
.venv/bin/pip install --no-index "./tensorplate_python-${TP_VERSION}-py3-none-any.whl[speech]"
```

The first command fails when a downloaded file does not match its pinned
hash. It is a command of its own because a hashed file puts pip in
hash-required mode for the whole command, and pip then refuses the SDK
wheel given beside it. `--no-index` in the second command lets pip use only
the wheel and what is already installed, so it fails rather than fetch
anything the pins do not cover.

### Stream schema modules

The wheel carries the Python modules of the stream session schema,
[`session.proto`](../../protocol/proto/tensorplate/stream/v1/session.proto):
`tensorplate.stream.v1.session_pb2` (the messages, with a `.pyi` stub) and
`tensorplate.stream.v1.session_pb2_grpc` (the `SessionService` stub and
servicer). They need the `speech` extra. The schema is a
[draft](../../protocol/README.md#streaming-session-envelope) that nothing
serves yet: until it is frozen, a release may change these modules
incompatibly.

The modules are generated when the package is built, by `grpcio-tools`
1.81.1, and are never committed: a source tree that was neither built nor
installed has none, and an editable install writes them beside the package
in `src/`, where git ignores them. They are generated once, at install:
after a change to the schema, reinstall an editable install, or the stale
modules keep importing without any error. The sdist carries the schema under
`proto/` and generates the same modules, so it builds the wheel with no
repository around it.

Every build from source, of a checkout or of the sdist and with or without
an extra, therefore needs `grpcio-tools` in its build environment.
`pyproject.toml` requires version 1.81.1, and
[`sdk/python/constraints/build.txt`](../../sdk/python/constraints/build.txt)
pins that environment (the generator, what it requires, and setuptools),
each with the SHA-256 of every file published for it. pip installs the build
environment from those hashes when given the file:

```bash
# pip 25.3 or later, from the repository root
pip install --build-constraint sdk/python/constraints/build.txt ./sdk/python
# python -m build fills its build environment with a plain pip install
PIP_CONSTRAINT="$PWD/sdk/python/constraints/build.txt" python -m build sdk/python
```

A build given neither takes the required versions from the index without a
hash check. CI uses the first form and the release build the second; the
release build also runs `tools/release/check-sdk-distribution.py`, which
fails unless the wheel holds exactly the package's source files and the
three generated modules, and the sdist holds the schema as committed and no
generated module.

## Quickstart

Detect objects on an image against a deployed YOLO detector (requires the
`[vision]` extra):

```python
import tensorplate

client = tensorplate.VisionClient("http://127.0.0.1:18080")
for det in client.detect("frame.jpg", endpoint="yolov8n"):
    print(det.class_id, det.score, det.box)
```

`endpoint` is the **deployed model/endpoint name**; the constructor's URL
(or auto-resolution — see [Endpoint resolution](#endpoint-resolution)) is
the worker address. `box` is `(x1, y1, x2, y2)` in source-image pixels.

To call `/infer` directly with your own tensors, use `ServingClient`:

```python
from tensorplate import ServingClient, DType

client = ServingClient("http://127.0.0.1:18080")
# payload is your raw, little-endian tensor data as bytes.
tensor = client.tensor_input("images", payload, DType.UINT8, [1, 480, 640, 3])
result = client.infer("yolov8n", [tensor])

print(result.request_id)
for out in result.outputs:
    print(out.name, out.dtype, out.shape)
```

## API reference

### `ServingClient`

`ServingClient(serving_url=None, *, profile=None, config_path=None, timeout=30.0, discover=True)`

A client for one serving worker. With no `serving_url` the endpoint is
resolved with CLI-parity precedence (see
[endpoint-resolution.md](./endpoint-resolution.md)); construction raises
`EndpointUnavailableError` when the agent reports a resident set with no
single serving URL. `discover=False` skips the agent-discovery tier;
`timeout` is in seconds.

- `infer(endpoint, inputs, *, deadline_ms=None, correlation_id=None) -> InferResult`
  — POST a request to the resolved worker. `endpoint` (required) is the
  deployed model name; `inputs` is a sequence of `TensorInput`. Raises
  `ValueError` on an empty `endpoint`, empty `inputs`, a non-positive
  `deadline_ms`, or an empty `correlation_id`.
- `health() -> HealthSnapshot` — GET `/health`; returns the readiness
  snapshot even when the worker reports a degraded HTTP status.
- `tensor_input(name, data, dtype, shape, *, layout=Layout.ROW_MAJOR) -> TensorInput`
  (static) — build an input from raw `bytes` with no numpy dependency.
- `endpoint` — the `ResolvedEndpoint` this client targets.

`InferResult` exposes `request_id: str`, `outputs: tuple[TensorOutput, ...]`,
`correlation_id`, `timing`, and `output(name) -> TensorOutput`.

`TensorOutput` exposes `name`, `dtype`, `shape`, `data: bytes`, `layout`,
`semantic_tag`, and `to_numpy()` (requires the `[numpy]` extra; `bfloat16`
has no numpy mapping and raises). `TensorInput.from_numpy(name, array)`
builds an input from an ndarray (also `[numpy]`).

`HealthSnapshot` exposes `state`, `endpoint`, `backend`, `active_model_id`,
`last_error_code`, `last_error_message`, `queue_depth`, `in_flight`, and the
`is_ready` property (true only when `state == "ready"`).

### `VisionClient`

`VisionClient(serving_url=None, *, profile=None, config_path=None, timeout=30.0, discover=True, client=None)`

A detection-focused wrapper over `ServingClient` (pass an existing
`client=` to share one; `.serving` exposes it). Without `client=` it
resolves the endpoint as `ServingClient` does, with the same
`EndpointUnavailableError`. Requires the `[vision]` extra.

- `detect(image, *, endpoint, input_name="images", output_name=None, score_threshold=0.25, nms_threshold=0.45, labels=None, transposed=False, contract="yolo_v8_single_output", preprocess_config=None) -> list[Detection]`
  — preprocess `image` (a path, `bytes`, `Path`, or HWC `uint8` ndarray),
  call `infer`, and decode detections. `endpoint` is keyword-only and
  required. Built-in contracts are `yolo_v8_single_output` and
  `yolo26_e2e_detections`. See [detection.md](./detection.md) for output
  selection and pre/post options.

`Detection` is a frozen dataclass: `class_id: int`, `score: float`,
`box: (x1, y1, x2, y2)` in **source-image pixels**, and `label: str | None`
(set from `labels[class_id]` only when `labels` is given).

### Errors

Every SDK error derives from `TensorPlateError`, so one `except` covers the
whole surface:

| Exception | Raised when |
| --- | --- |
| `EndpointResolutionError` | A serving URL or CLI profile is malformed or unresolvable. |
| `EndpointUnavailableError` (an `EndpointResolutionError`) | The agent reports a resident set but no single serving URL to discover; pass `serving_url` or set one on the CLI profile. |
| `TransportError` | The worker is unreachable. |
| `RequestTimeoutError` (a `TransportError`) | The request exceeds `timeout`. |
| `ProtocolError` | The response is not a valid v0.1 envelope. |
| `UnsupportedSchemaVersionError` (a `ProtocolError`) | The worker's `schema_version` is not `0.1`. |
| `ServingError` | The worker returned a typed `failure`. Carries `.code` (an `ErrorCode`), `.message`, `.context`, and `.request_id`. |
| `MissingDependencyError` (also an `ImportError`) | An optional part of the SDK, today `tensorplate.speech`, is imported without its extra. Carries `.extra` and `.dependency`. |

```python
from tensorplate import ServingClient, ServingError, TransportError

try:
    result = ServingClient().infer("yolov8n", inputs)
except ServingError as exc:
    print(exc.code, exc.message)
except TransportError:
    print("serving worker unreachable")
```

## Streaming speech client

`tensorplate.speech` (the [`speech` extra](#speech-extra)) is an asyncio
client for a serving worker's stream endpoint: one gRPC call per session,
speaking the [stream session schema](../../protocol/README.md#streaming-session-envelope).
It is asyncio only; there is no synchronous wrapper. Today it opens
speech-to-text sessions. Text-to-speech sessions and discovery of the stream
endpoint through the agent are not in the package yet, so the caller names
the endpoint and the deployment. The schema is a draft, and this client has
been run against an in-process server built from the same schema, not yet
against a worker: no worker serves speech sessions over it yet.

```python
from tensorplate.speech import AudioFormat, SpeechClient, Target


async def transcribe(endpoint: str, frames: list[bytes]) -> str:
    target = Target("my-deployment")
    audio = AudioFormat("pcm_s16le", sample_rate_hz=16000)
    async with SpeechClient(endpoint) as client:
        async with client.transcribe(target, language="en", input_format=audio) as session:
            for frame in frames:  # 20 ms of audio each
                await session.send_audio(frame)
            final = await session.finalize()
    return final.text
```

- **Endpoint and target.** `SpeechClient(endpoint)` takes the `host:port` of
  the worker's stream endpoint, on loopback or forwarded to it, and opens one
  cleartext channel; create it inside a running event loop. A `Target` is a
  deployment id with the generation and descriptor digest the worker serves,
  or the id alone for a worker started without a generation.
- **Sessions.** `client.transcribe(target, *, language, input_format,
  trace_id="", turn_id="", open_timeout=10.0)` returns a session that
  `async with` opens. Leaving the block closes the sending half and waits for
  the server to finish what it accepted; `close()` does the same and returns
  the session's `SessionTotals`. Leaving it on an exception cancels the
  session, as `cancel()` does: the server is told, and every event not yet
  taken is dropped. `session.limits` holds the timers and frame bounds the
  server reported.
- **Input and credit.** `send_audio(data)` sends one frame in the session's
  input format and waits while the server's input credit does not admit it.
  The client takes its credit from the server's messages only, never from
  time. A frame outside the session's frame bounds raises `ValueError` before
  anything is sent; a frame shorter than the least one is allowed only as the
  last before `finalize()` or `close()`.
- **Events.** Iterating a session (`async for event in session`) yields
  `PartialTranscript`, `EndpointDetected` and `FinalTranscript` in the order
  the server sent them, and ends when the session does. They are frozen
  dataclasses of the SDK; no generated class appears in the client's
  interface. `finalize()` ends the open utterance and returns the
  `FinalTranscript` that names it in `finalize_sequence`; iteration yields
  that transcript too. A server event of a kind this client does not know is
  passed over and counted in `session.unknown_events`.
- **Unread events.** A session holds up to 16 KiB of events its caller has
  not taken and then stops reading from the call, so take events as they
  arrive: the server ends a session whose output stays unread.
- **Liveness.** The session sends the schema's `Ping` at the interval the
  server gave when the session became ready.
- **No retry.** The client never reopens, resends or replays. A session that
  was refused, failed or lost is reported once, and opening another is the
  caller's decision.

Every error is a `SpeechError` (a `TensorPlateError`), never a gRPC
exception. Its class follows the final status of the call:

| Exception | Raised when |
| --- | --- |
| `InvalidEventError` | The server found an event or its input invalid. |
| `PreconditionFailedError` | The deployment does not serve what was asked: a language or format it does not declare, another deployment or descriptor, a stale generation. |
| `ResourceExhaustedError` | A bound was reached: the session count, the input credit, or output left unread too long. |
| `SessionTimeoutError` | One of the server's timers ended the session, or the client's own wait for the open or for a `finalize()` passed. |
| `ServiceUnavailableError` | Admission is closed, the worker is shutting down or retiring the deployment, the backend was lost, or the endpoint cannot be reached. |
| `SessionCancelledError` | The call was cancelled by something other than the session's own `cancel()`. |
| `ServerInternalError` | The server failed, or ended the call with a status no other class stands for. |
| `StreamProtocolError` (also a `ProtocolError`) | The server sent something the schema does not allow; the client ended the call. |
| `SessionClosedError` | The session was used after it ended. |

A `SpeechError` carries `reason` and `code` when the server stated a cause
(for example `"worker_shutdown"` and `ErrorCode.UNAVAILABLE`), `grpc_status`,
and `admitted`, which has three values. `True`: the server reserved a session
slot, so the call ran. `False`: the server stated that it reserved none.
`None`: unknown. An open that ends with a status and no event is always
`None` today: the schema has not fixed how a refusal carries its cause, so a
refusal cannot be told from a session that was admitted and then lost, and
`reason` is `None` too. Do not treat such a call as one that never ran.

The channel is opened with gRPC's bandwidth probe off
(`grpc.http2.bdp_probe` 0), which keeps HTTP/2's receive window at its 64 KiB
default, and with sent and received messages capped at 64 KiB. With the probe
on, gRPC widens the window to megabytes, and a session that stopped reading
would not hold the server back until that much had been sent.

## Endpoint resolution

`ServingClient` and `VisionClient` resolve the worker URL exactly as
`tensorplate infer`: explicit URL → CLI profile `serving_url` →
agent-discovered active deployment → loopback `http://127.0.0.1:18080`. An
agent that reports a resident set but no `serving_url` raises
`EndpointUnavailableError` rather than reaching the loopback default. The
full precedence and URL canonicalization rules are in
[endpoint-resolution.md](./endpoint-resolution.md).

## Examples

Runnable, user-space samples live in
[`examples/vision_detection_sdk/`](../../examples/vision_detection_sdk/):
`yolo_detect.py` (single image) and `camera_infer.py` (a per-frame
`camera → SDK → /infer` reference loop). They are learning samples, not a
supported runtime surface.

## Scope and non-goals

v0.1.3 calls **already-deployed** models. The SDK and its examples
deliberately do **not** provide:

- in-runtime camera or video ingest;
- a DeepStream sink or any GStreamer integration;
- a synchronous streaming client, text-to-speech sessions, or discovery of
  a stream endpoint: the [streaming speech client](#streaming-speech-client)
  is asyncio and opens speech-to-text sessions on an endpoint it is given;
- any change to runtime serving behavior or ownership.

These are separate, deferred capabilities tracked outside v0.1.3. The
client-side preprocessing and postprocessing helpers are a convenience for
calling a detector; they are not an in-runtime pre/post contract.
