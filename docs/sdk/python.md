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
stream listener. With a speech model on a backend that runs speech jobs it
serves stream sessions. With a deployment of another model class, or a
speech model on a backend registered without a job bridge factory, it
refuses every `Open`. With a speech model whose runner offers no speech
jobs the worker is not built at all (see
[serving-worker.md](../architecture/serving-worker.md#the-speech-dispatch-in-the-worker)).
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
It is asyncio only; there is no synchronous wrapper. It opens
speech-to-text sessions, described first, and
[text-to-speech sessions](#text-to-speech-sessions). The caller names the
endpoint and the deployment, or asks the agent for both
([finding the stream endpoint](#finding-the-stream-endpoint)). The schema is
a draft. A serving worker
built with streaming support serves these sessions for a speech deployment
when its config sets `streaming.enabled`, which the agent renders for a
deployment made with `tensorplate deploy --streaming`; `tensorplate status`
then prints its `stream_endpoint` and the `descriptor_digest` an Open names
beside its generation. This client's own tests run
against an in-process server built from the same schema, not against a
worker.

```python
import asyncio

from tensorplate.speech import (
    AudioFormat,
    PartialTranscript,
    SpeechClient,
    SpeechToTextSession,
    Target,
)


async def transcribe(endpoint: str, frames: list[bytes]) -> str:
    target = Target("my-deployment")
    audio = AudioFormat("pcm_s16le", sample_rate_hz=16000)
    async with SpeechClient(endpoint) as client:
        session = client.transcribe(target, language="en", input_format=audio)
        async with session:
            # Events are read while audio is sent, each in a task of its own.
            reader = asyncio.create_task(show_partials(session))
            sender = asyncio.create_task(send_frames(session, frames))
            try:
                # Until both are done or one has failed. A reader that failed takes no
                # more events, and the sender would wait behind the events nobody takes.
                await asyncio.wait({reader, sender}, return_when=asyncio.FIRST_EXCEPTION)
            finally:
                # Take both outcomes on every path.
                reader.cancel()
                sender.cancel()
                outcomes = await asyncio.gather(reader, sender, return_exceptions=True)
            # The reader's error first: the sender's, if it has one too, came of it.
            for outcome in outcomes:
                if isinstance(outcome, Exception):
                    raise outcome
    return sender.result()


async def send_frames(session: SpeechToTextSession, frames: list[bytes]) -> str:
    for frame in frames:  # 20 ms of audio each
        await session.send_audio(frame)
    final = await session.finalize()
    await session.close()
    return final.text


async def show_partials(session: SpeechToTextSession) -> None:
    async for event in session:
        if isinstance(event, PartialTranscript):
            print(event.text)
```

- **Read events while sending.** This is the one rule of a session. Input
  credit, the answer to a `finalize()` and the end of the session all arrive
  on the stream the events arrive on, behind the events sent before them.
  A session holds up to 16 KiB of events its caller has not taken and then
  reads nothing more from the server, so `send_audio()`, `finalize()` and
  `close()` wait until some are taken. Iterate the session in a task of its
  own, as `transcribe()` does, and take that task's outcome when the session
  fails, since the iteration fails with it. When that task fails by itself,
  stop the sender and raise the task's error, as `transcribe()` also does: a
  sender left alone would wait behind the events nobody takes and raise
  `ResourceExhaustedError` in place of the error that caused it. A session
  whose held events stay untaken for the server's output stall time
  (`session.limits.output_stall_timeout_ms`, five seconds by default) is
  ended by the client with
  `ResourceExhaustedError`, the error the server's own limit on unread
  output ends it with; no call waits for ever on events nobody reads. That
  timer is the client's own: it runs only while the session has stopped
  reading because of events its caller has not taken, starts again with
  each event taken, and never runs for a caller that has nothing to read.
  It can end a session before the server's timer would, since the server's
  clock stops once its output is delivered, and the transport's window
  takes that output before the session reads it. A `Ready` that gives no
  stall time is refused at open with `StreamProtocolError`, since the schema
  allows no limit of zero and nothing else ends a session whose events
  nobody reads. The iteration ends when the session does, and raises the
  session's error if it failed.
- **Endpoint and target.** `SpeechClient(endpoint)` takes the `host:port` of
  the worker's stream endpoint, on loopback or forwarded to it, and opens one
  cleartext channel, which it closes with itself; create it inside a running
  event loop. `SpeechClient(channel=...)` takes a `grpc.aio` channel the
  caller made instead: the client never closes it, and its options are the
  caller's to set, `("grpc.http2.bdp_probe", 0)` among them (see the last
  paragraph of this section). A `Target` is a deployment id with the
  generation and descriptor digest the worker serves, or the id alone for a
  worker started without a generation. `aclose()`, or leaving `async with`,
  ends every session made from the client: an open one has its call
  cancelled, and all of them raise `SessionClosedError` from then on.
- **Sessions.** `client.transcribe(target, *, language, input_format,
  trace_id="", turn_id="", open_timeout=10.0, close_timeout=30.0,
  cancel_timeout=5.0)` returns a session that `async with` opens; a session
  is never constructed directly. Leaving the block closes the sending half
  and waits for the server to finish what it accepted; `close()` does the
  same and returns the session's `SessionTotals`. Leaving it on an exception
  or a cancellation cancels the session, as `cancel()` does: the server is
  told, and every event not yet taken is dropped. `session.limits` holds the
  timers and frame bounds the server reported. `trace_id` and `turn_id` are
  at most 64 printable ASCII characters each; the client sends them as
  given, and the server refuses an open with any other
  (`InvalidEventError`).
- **Bounds.** Every wait for the server but one has a bound, in seconds,
  counted from the call on the event loop's clock and covering all of it:
  the wait behind another task's write, the write, and the answer.
  `open_timeout` bounds the open. `close(timeout=...)` and a clean exit are
  bounded by `close_timeout` unless a timeout is given. `finalize(timeout=...)`
  is unbounded unless one is given. Past any of them the session is ended:
  the call is cancelled and `SessionTimeoutError` raised, then and from every
  later use, since a Finalize without its answer or a drain without its end
  leaves nothing to continue. `cancel(timeout=...)` and an exit on an
  exception wait `cancel_timeout` for the server to end the session and then
  cancel the call. `send_audio()` has no timeout of its own: it ends with
  credit or with the session, and can be cancelled like any other await.
- **Cancellation.** A task cancelled while it is inside a session method gets
  its `CancelledError`, and the session stays open. A frame or a Finalize it
  was writing at that moment is still sent, because gRPC cancels a call
  whose write is cancelled; one it was still waiting to send is not. A task
  cancelled on the turn of the event loop in which its call fails gets
  `CancelledError` too, and that call's error is not raised. An error that
  ended the session is raised by the session's next use; after an open that
  failed, by iterating the session, while its other calls raise
  `SessionClosedError`. A `ValueError` for a frame or a text that was not
  sent is not reported at all.
- **Input and credit.** `send_audio(data)` sends one frame in the session's
  input format and waits while the server's input credit does not admit it.
  `data` is `bytes` or a contiguous view of bytes (a `bytearray`, a
  `memoryview` of one), which is copied once. Anything else raises
  `TypeError` before anything is sent: an integer, a list, and a view of
  items other than bytes, which `memoryview.cast("B")` turns into one of
  bytes. A frame that is empty or not a whole number of samples raises
  `ValueError`. Both are raised at once, before any wait and whatever the
  state of the session. The client takes its credit from the
  server's messages only, never from time. Once the frame's turn has come,
  a frame longer than the session's frame bound or larger than its whole
  input credit raises `ValueError` before anything is sent; a frame shorter
  than the least one is allowed only as the last before `finalize()` or
  `close()`.
- **Events.** Iterating a session (`async for event in session`) yields
  `PartialTranscript`, `EndpointDetected` and `FinalTranscript` in the order
  the server sent them, and ends when the session does. They are frozen
  dataclasses of the SDK; no generated class and no gRPC type appears in the
  client's interface. The server may end an utterance itself (an endpoint
  with reason `"silence"` or `"duration_limit"`); its final transcript has a
  `finalize_sequence` of zero, and the next utterance's endpoint may arrive
  before it. A server event of a kind this client does not know, audio in a
  speech-to-text session included, is passed over and counted in
  `session.unknown_events`.
- **Finalize.** `finalize()` ends the open utterance and returns the
  `FinalTranscript` that names it in `finalize_sequence`; iteration yields
  that transcript too. The server refuses audio sent before that answer, so
  `send_audio()` waits for it, also when the `finalize()` that asked was
  cancelled meanwhile, and a second `finalize()` waits its turn the same way.
- **Liveness.** The session sends the schema's `Ping` at the interval the
  server gave when the session became ready
  (`session.limits.heartbeat_interval_ms`). A `Ready` that gives no interval
  is refused at open with `StreamProtocolError`, as one that gives no stall
  time is: the schema allows no limit of zero, and a session that sent no
  `Ping` would be ended by the server for it.
- **One more Open, and no other retry.** An `open()` whose call ends before
  any server event, with `UNAVAILABLE` or with `INTERNAL` after the client's
  write of the Open failed (gRPC's status for a write to a call that has
  ended), makes one more call within the same `open_timeout`. A connection
  lost as the Open is sent ends a call that way, and the call carried nothing
  but the Open: the client sends no input before `Ready`. A refusal with
  status `UNAVAILABLE` (admission closed) is therefore sent twice, and the
  worker's logs show both. So can be a refusal the worker sends before it
  reads the Open (when 32 calls are already waiting to open, for example),
  when the refusal races the Open's write and gRPC reports that write's
  `INTERNAL`. An
  endpoint that cannot be reached is tried twice. The session is not ended
  between the two calls: a `cancel()`, the client's `aclose()` or the
  `open_timeout` in between ends it, with no second call, and a task
  already iterating the session takes the second call's events.
  A client made by `SpeechClient.from_agent()` may send that second call to
  another target ([finding the stream endpoint](#finding-the-stream-endpoint));
  an `open()` never makes more than two calls. Otherwise the client never
  reopens, resends or replays. A session that was refused, failed or lost is
  reported once, and opening another is the caller's decision.

Every error is a `SpeechError` (a `TensorPlateError`), never a gRPC
exception. Its class follows the final status of the call:

| Exception | Raised when |
| --- | --- |
| `InvalidEventError` | The server found an event or its input invalid. |
| `PreconditionFailedError` | The deployment does not serve what was asked: a language, format or voice it does not declare, another deployment or descriptor, a stale generation, input the session's state does not permit; or the endpoint does not serve stream sessions at all (`UNIMPLEMENTED`). |
| `ResourceExhaustedError` | A bound was reached: the session count, the input credit, or output left unread too long, by the server's limit or by the client's own. |
| `SessionTimeoutError` | One of the server's timers ended the session, or one of the client's own bounds passed. |
| `ServiceUnavailableError` | Admission is closed, the worker is shutting down or retiring the deployment, the backend was lost, or the endpoint cannot be reached. |
| `SessionCancelledError` | The call was cancelled by something other than the session's own `cancel()`. |
| `ServerInternalError` | The server failed, or ended the call with a status no other class stands for. `PERMISSION_DENIED` and `UNAUTHENTICATED` land here: the stream listener has no authorization, so no rule of the schema produces them. |
| `StreamProtocolError` (also a `ProtocolError`) | The server sent something the schema does not allow; the client ended the call. |
| `SessionClosedError` | The session was used after it ended, opened twice, or its client or channel was closed. |

One case takes the class from the cause instead. A write that reaches gRPC
after the server ended the call, and before the client read that end, makes
gRPC report the call as `INTERNAL` in place of the server's status. The
error's class is then the one the cause's error code stands for, by the rule
the stream listener applies to a cause whose reason it does not name, and
`grpc_status` is `"INTERNAL"`. For the few reasons whose status is not
their error code's, the class is then the code's: `illegal_transition`
reads as `ServiceUnavailableError`, where its status gives
`PreconditionFailedError`. Rarely that write also keeps the client from
reading the server's last event; the error is then a `ServerInternalError`
with no `reason`.

A `SpeechError` carries `reason` and `code` when the server stated a cause
(for example `"worker_shutdown"` and `ErrorCode.UNAVAILABLE`), `grpc_status`
(the status's name, or `None` when the client ended the session itself),
and `admitted`, which has three values. `True`: the server reserved a session
slot, so the call ran. `False`: the server stated that it reserved none.
`None`: unknown. An open that ends with a status and no event is always
`None` today: the schema has not fixed how a refusal carries its cause, so a
refusal cannot be told from a session that was admitted and then lost, and
`reason` is `None` too. Do not treat such a call as one that never ran.

None of them is a `TransportError` or a `RequestTimeoutError`. Those belong
to the unary HTTP client, where they mean that the worker was not reached or
did not answer in time. A session's status does not separate those cases:
`UNAVAILABLE` ends a call to an endpoint that cannot be reached and a call
the worker refused or drained, and `DEADLINE_EXCEEDED` is one of the
server's timers. There is no `retryable` flag either. The schema states one
thing about sending a call again, that an Open the server did not admit
(`admitted is False`) may be sent to another server, and gives no rule for
which reasons pass; a call that was admitted, or whose admission is unknown,
must not be sent again as if it had not run. The client's one more Open is
not such a resend: the call it replaces carried only an Open, and had ended.

The client's own channel is opened with gRPC's bandwidth probe off
(`grpc.http2.bdp_probe` 0), which keeps HTTP/2's receive window at its
default of about 64 KiB, and with sent and received messages capped at 64
KiB. With the probe on, gRPC widens the window to megabytes, and a session
that stopped reading would not hold the server back until that much had been
sent. A channel passed in with `channel=` has whatever options its owner
gave it. The worker accepts at most 256 connections at one time; a session
opened on a connection past them ends with `ServiceUnavailableError` once
its second call is refused too. The worker closes a connection with no call
open 60 to 147 minutes after its last call, after the client's own channel
has closed it (30 to 60 minutes). A channel passed in that turns gRPC's
client idle limit off, or that was connected without a call, can meet the
worker's close. A session opened at that instant sends its Open once more,
on a new connection.

### Finding the stream endpoint

The agent's status names where the active deployment serves streams: its
`stream_endpoint`, with the deployment id, `generation` and
`descriptor_digest` an Open names there. They are present while the
generation serving the active deployment was started with `tensorplate
deploy --streaming` (see [`tensorplate status`](../cli/status.md)).
`resolve_stream_target()` reads that status once and returns a
`StreamTarget`, the `endpoint` and the `Target`:

```python
from tensorplate.speech import SpeechClient, resolve_stream_target

endpoint, target = await resolve_stream_target()
async with SpeechClient(endpoint) as client:
    session = client.transcribe(target, language="en", input_format=audio)
```

`SpeechClient.from_agent()` reads it for the client, which then opens every
session on the target last read and takes no `target` argument:

```python
async with await SpeechClient.from_agent() as client:
    async with client.transcribe(language="en", input_format=audio) as session:
        ...
```

The agent is found as `tensorplate` and the unary clients find it
([endpoint resolution](./endpoint-resolution.md)): the CLI profile named by
`profile`, else the config's default, else `local`, the agent's socket at
`/var/run/tensorplate/agent.sock`. On a packaged install that socket and its
directory belong to the `tensorplate` group (modes 0660 and 0750), so the
application's user must be in that group. The SDK reads only the status the
agent's control socket answers; it reads no CLI output, state file or worker
configuration. `status_timeout` (5 seconds) bounds each socket operation of
a read: the connection, the request's write and each wait for more of the
answer.

Each generation is a worker of its own. A deploy or a rollback normally
starts the new generation on another stream port and stops the old worker; a
restart starts a new generation on the same port. So the target a client
read goes stale, and a client made by `from_agent()` follows it:

- **A session already open stays where it is.** It runs until its worker
  ends it, which a worker going away does with `ServiceUnavailableError`
  and `reason` `worker_shutdown`, or `deployment_retired` once the agent
  retires workers. It is never moved to another generation.
- **After such an end, the next open reads the status first**, as it does
  after an open whose read of the status failed.
- **An open whose first call ends before any server event reads the status
  once.** That is how an Open ends that reached the old worker as it
  drains (`admission_closed`), a port nothing listens on any more, or a
  worker that serves another generation now (`stale_generation`). When the
  agent names another endpoint or target, the second call goes there.
  When it names the same one, the second call is made only where the
  [one more Open](#streaming-speech-client) rule allows it, and otherwise
  the first call's error is raised. Either way an `open()` makes at most
  two calls, all within its `open_timeout`, which also bounds the read.
- **Each target has a channel of its own.** A later generation may take the
  port an earlier one left, and a channel that met the port closed is still
  backing off from it, so the client opens a new channel for every target
  the agent names. The channel of a target no unended session uses is
  closed at the next open.

When the status names no stream target, discovery raises a
`DiscoveryError`, which is a `SpeechError` and an `EndpointResolutionError`,
with `admitted` `False` when no Open was sent. When the error ends an open
whose first call was lost, `admitted` is that call's and the call's error is
its `__cause__`. There is no default endpoint to fall back on.

| Exception | Raised when |
| --- | --- |
| `NotStreamingError` | The generation serving the active deployment does not serve streams: it was deployed without `--streaming`, or the agent starts no deployment with streaming (it does not list the `deploy_streaming` control feature). |
| `NotServingError` | No generation serves the active deployment now, or nothing is deployed. A deploy, a rollback or a restart may be between its steps. |
| `ResidentSetError` | The agent serves more than one deployment, so no single one is active. Give the client the member's endpoint and target. |
| `RemoteAgentError` | The profile reaches the agent at another host. Its stream endpoints are on that host's loopback interface; forward one and give the client that endpoint and the target. |
| `DiscoveryError` | The agent cannot be reached or its answer read, the profile names no agent the SDK reads a status from, or the status names a malformed target. |

The client does not wait for a deployment to start serving. A caller that
wants to wait calls `resolve_stream_target()`, or opens again, at an
interval of its own choosing until the error is gone.

### Text-to-speech sessions

`client.synthesize(target, *, language, voice, ...)` returns a
`TextToSpeechSession`: text segments go in, and each segment's audio comes
back in chunks of 20 ms. What the section above says of a session holds here
too: the one rule, the bounds, cancellation, liveness, the one more Open,
and the errors. What differs is below.

```python
import asyncio
from collections.abc import Awaitable, Callable

from tensorplate.speech import AudioChunk, SpeechClient, Target, TextToSpeechSession


async def speak(
    endpoint: str, sentences: list[str], play: Callable[[bytes], Awaitable[None]]
) -> int:
    target = Target("my-deployment")
    async with SpeechClient(endpoint) as client:
        session = client.synthesize(target, language="en", voice="my-voice")
        async with session:
            # Audio is taken while text is sent, each in a task of its own.
            player = asyncio.create_task(play_audio(session, play))
            sender = asyncio.create_task(send(session, sentences))
            try:
                # Until both are done or one has failed. A player that failed takes no
                # more audio, and the sender would wait behind the audio nobody takes.
                await asyncio.wait({player, sender}, return_when=asyncio.FIRST_EXCEPTION)
            finally:
                # Take both outcomes on every path.
                player.cancel()
                sender.cancel()
                outcomes = await asyncio.gather(player, sender, return_exceptions=True)
            # The player's error first: the sender's, if it has one too, came of it.
            for outcome in outcomes:
                if isinstance(outcome, Exception):
                    raise outcome
    return sender.result()


async def send(session: TextToSpeechSession, sentences: list[str]) -> int:
    for sentence in sentences:
        await session.send_text(sentence)
    done = await session.finalize()
    await session.close()
    return done.total_samples


async def play_audio(
    session: TextToSpeechSession, play: Callable[[bytes], Awaitable[None]]
) -> None:
    async for event in session:
        if isinstance(event, AudioChunk):
            await play(event.data)  # 20 ms of session.output_format
```

- **Read audio while text is sent.** The one rule has less room here,
  because audio is large. The credit `send_text()` waits for, the answer to
  `finalize()` and the end `close()` waits for all arrive behind the audio
  sent before them, and the session stops reading at two seconds of audio
  its caller has not taken. A caller that sends first and reads afterwards
  gets through only while everything fits. Against the in-process server,
  `finalize()` or `close()` with nobody reading returns for up to two
  seconds of audio in all (2.00 s returns, 2.02 s does not), and
  `send_text()` returns for three segments of ten seconds, the one being
  delivered and the two that may wait, and waits with a fourth. Past that the call waits, and
  once the audio has gone untaken for the output stall time
  (`session.limits.output_stall_timeout_ms`, five seconds by default) the
  session ends and the call raises `ResourceExhaustedError`. So take the
  audio in a task of its own, as `speak()` does. When that task fails, stop
  the sender and raise the task's error, as `speak()` also does: a sender
  left alone would wait out the stall time and raise
  `ResourceExhaustedError` in place of the error that caused it.
- **Sessions.** `client.synthesize(target, *, language, voice, trace_id="",
  turn_id="", open_timeout=10.0, close_timeout=Follow.FINALIZE_DEADLINE,
  cancel_timeout=5.0)`; `trace_id` and `turn_id` are bounded as in
  `transcribe()`.
  The deployment declares its voices, and the server chooses the audio
  format for the voice: `session.output_format` is an `AudioFormat`, and
  `session.limits.text_to_speech` holds the text bounds. `close()`, or
  leaving the block, has the server synthesize every segment it accepted and
  ends once the session has read all of that audio, which takes as long as
  the caller takes to play it less what the playback buffer holds. The
  server allows itself `session.limits.finalize_deadline_ms` to deliver what
  it accepted, two minutes by default. The default `close_timeout`,
  `Follow.FINALIZE_DEADLINE` (`Follow` is in `tensorplate.speech`), follows
  that limit: it is the deadline the session's `Ready` declared plus a
  margin of 30 seconds, so 150 seconds by default. A `close_timeout` given
  as a number of seconds, or `None` for no bound, is used as it is.
- **Input and credit.** `send_text(text, *, turn_id="")` sends one complete
  segment, a sentence or a clause, and returns its id: ids grow by one from
  1, and a segment's events carry its id. It waits while the server's input
  credit does not admit the segment. That credit has two budgets, bytes of
  text and segments whose synthesis has not started (the worker lets two
  wait), and both return as a segment's synthesis starts, which the server
  reports behind the audio it sent first. Text that is not a `str` raises
  `TypeError`; text that is empty, only ASCII whitespace (the server
  refuses that, and serves other whitespace) or not encodable as UTF-8, and
  a `turn_id` that is not at most 64 printable ASCII characters, raise
  `ValueError`. Those are raised at once, before any wait and whatever the
  state of the session. Once the segment's turn has come, text
  above a bound of `session.limits.text_to_speech` (`max_segment_text_bytes`
  for one segment, `max_synthesis_text_bytes` for the segments between two
  `finalize()` calls, both in bytes of UTF-8) and a segment larger than the
  session's whole input credit raise `ValueError` before anything is sent.
- **Events.** Iterating the session yields each segment's `AudioChunk`
  events and then its `SegmentCompleted`, segment after segment in the order
  sent, and a `SynthesisCompleted` for each `finalize()`. A chunk carries
  `segment_id`, `chunk_index`, `segment_sample_offset`, `format` and `data`:
  20 ms of audio, less only in a segment's last chunk. The client holds the
  server to that before it yields anything: a chunk in another format than
  the session's, out of its place, of another segment than the oldest
  unfinished one, or of another length, and a completion or closing total
  that is not what was read, end the session with `StreamProtocolError`. A
  session that ended with an error may have yielded part of a segment: a
  segment is whole only once its `SegmentCompleted` was read.
- **The playback buffer.** Audio the caller has not taken is held in the
  session, up to two seconds of the output format counted in bytes of audio
  (`PLAYBACK_BUFFER_MS` in `tensorplate.speech.tts`, 2000: 96,000 bytes of
  24 kHz 16-bit mono). Other events are held up to 16 KiB,
  as in every session. At either bound the session reads nothing more from
  the server until something is taken. The transport's window then fills
  and the server's writes stop. A worker holds no more than two seconds of
  undelivered audio itself
  ([serving-worker.md](../architecture/serving-worker.md#logical-sessions)),
  so it stops synthesizing and drops nothing, and its credit stops
  returning, so `send_text()` waits. A caller that plays audio as it takes
  it therefore keeps the session at the pace of playback. The two seconds
  are the session's buffer alone, not all the audio ahead of the player.
  Beyond it the transport holds what its window let through, which is a
  number of bytes and so a longer time in a format of fewer bytes a second:
  in runs on one machine, 1.3 to 1.4 seconds more of 24 kHz 16-bit mono and
  6.5 seconds more of 8 kHz mu-law. The server holds up to two seconds
  beyond that. Taking nothing for the server's output stall time ends the
  session with `ResourceExhaustedError`, and audio already held is still
  yielded before the error. That end is the client's timer's unless the
  caller takes audio again first: a session that has stopped reading does
  not read the server's end either, so the end the server's own limit makes
  (`reason == "slow_consumer"`) is seen only by a caller that resumes.
- **Finalize.** `finalize()` completes the segments sent so far and returns
  the `SynthesisCompleted` that names it, which counts those segments and
  their samples; iteration yields it too, after their audio. The server
  refuses text sent before that answer, so `send_text()` waits for it. The
  answer arrives behind the audio, so it comes no sooner than the caller
  takes that audio. Closing without a `finalize()` delivers the same audio
  and no `SynthesisCompleted`.
- **Stopping speech.** The schema has no cancel for a single segment yet.
  To stop audio that is being synthesized, `cancel()` the session, which
  drops what the session holds and has the server stop at once, and open
  another.

## Endpoint resolution

`ServingClient` and `VisionClient` resolve the worker URL exactly as
`tensorplate infer`: explicit URL → CLI profile `serving_url` →
agent-discovered active deployment → loopback `http://127.0.0.1:18080`. An
agent that reports a resident set but no `serving_url` raises
`EndpointUnavailableError` rather than reaching the loopback default. The
full precedence and URL canonicalization rules are in
[endpoint-resolution.md](./endpoint-resolution.md). The streaming speech
client asks the agent alone, with no default; see
[finding the stream endpoint](#finding-the-stream-endpoint).

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
- a synchronous streaming client: the
  [streaming speech client](#streaming-speech-client) is asyncio only;
- any change to runtime serving behavior or ownership.

These are separate, deferred capabilities tracked outside v0.1.3. The
client-side preprocessing and postprocessing helpers are a convenience for
calling a detector; they are not an in-runtime pre/post contract.
