# Backend capability model and registry

> **Status:** v0.1.0 V01-E05-F01.
> **Source:** [`include/tensorplate/backend/capability.hpp`](../../include/tensorplate/backend/capability.hpp),
> [`include/tensorplate/backend/registry.hpp`](../../include/tensorplate/backend/registry.hpp),
> [`include/tensorplate/backend/builtin.hpp`](../../include/tensorplate/backend/builtin.hpp).

V01-E05-F01 introduces the vendor-neutral capability record and the
adapter registry. Every concrete adapter (TensorRT in V01-E05-F02,
LibTorch in V01-E05-F03, Python/PyTorch sidecar in V01-E05-F05, future
Vitis AI) registers a `BackendEntry` carrying:

1. a stable backend key string (e.g. `tensorrt`, `libtorch`, `python_pytorch`),
2. a `BackendCapability` value object that publishes precision support,
   shape support, async/generation/streaming/KV-cache flags, op-coverage
   percentage, and memory estimate/limit, and
3. an `ExecutionSessionFactory` closure that builds a fresh, unloaded
   `ExecutionSession` when the registry is asked to create one.

## Why a separate capability record?

A single `backend_name` string is not enough for the bundle pipeline,
status reporting, or the conformance harness:

- The agent's deploy transaction has to reject bundles whose declared
  `backend_hint` is missing on the device — and it has to do so
  *before* it stages anything.
- The agent has to reject bundles whose declared `precision_hint` is
  not supported by the backend. v0.1.0 deliberately does not silently
  downgrade FP16-only requests to FP32 (or any other path).
- `tensorplate status` and `tensorplate doctor` have to enumerate which
  adapters are compiled into this build and what each can run.
- The adapter conformance harness has to know whether to expect
  `Error::Code::Unsupported` from `infer_async`, because the public
  method shape is always present but adapter support is opt-in.

`BackendCapability` is plain data; it has no vendor SDK includes and no
raw hardware handles. It can be serialized via
`protocol/schemas/backend_capability.json` so the agent and the
serving worker can exchange capability records across process
boundaries without leaking adapter-specific types.

## Registry semantics

`BackendRegistry` is thread-safe and stores entries by value. Lookup,
registration, and deregistration take a mutex; the factory closure is
*invoked outside the mutex* so adapter initialization (which may open
files, spawn sidecars, or probe a GPU) cannot deadlock the registry.

Errors:

| Situation                                       | Error code      |
| ------------------------------------------------ | --------------- |
| Empty backend name on registration              | `config_invalid`|
| Null factory closure                            | `config_invalid`|
| Capability `backend_name` mismatches the entry  | `config_invalid`|
| Duplicate registration of the same key          | `internal`      |
| Lookup / `create_session` / `capability` miss   | `unsupported`   |
| Declared precision not in supported list        | `unsupported`   |

`validate_backend_hint(spec)` is the entry point the bundle pipeline
calls. It rejects unknown backends and rejects unsupported precisions;
it never picks a different backend on the caller's behalf and never
falls back at inference time. The error context carries
`backend=<name>, requested=<precision>` so deploy failures point at the
exact bundle field to fix.

`BackendRegistry::global()` is a process-wide instance used by
production code. Tests build local registries so they do not leak
state across unit tests.

## Built-in adapter registration

`register_builtin_backends(BackendRegistry&)` registers every adapter
whose CMake feature flag is set in the current build of `tp_runtime`:

| Flag                                | Adapter registered             |
| ----------------------------------- | ------------------------------ |
| `TP_ENABLE_TENSORRT`                | `tensorrt` (V01-E05-F02)       |
| `TP_ENABLE_LIBTORCH`                | `libtorch` (V01-E05-F03)       |
| `TP_ENABLE_PYTHON_PYTORCH_SIDECAR`  | `python_pytorch` (V01-E05-F05) |

Adapter registration is explicit rather than static-init based so tests
can construct an empty registry and bring up only the subset they need
to exercise. The flags are OFF by default in V01-E05-F01; each
subsequent feature flips its own flag on once its adapter implementation
lands.

## Sessions with a job bridge

Some backends run work that does not fit `ExecutionSession::infer`: synthesis
input is text, a decode of silence legitimately returns an empty transcript,
and neither is a tensor. Such a backend runs **typed jobs** through a
`BoundedJobBridge`
([`bounded_job_bridge.hpp`](../../include/tensorplate/backend/bounded_job_bridge.hpp))
beside its session. The session's public methods, its `do_*` hooks,
`infer_async` and `AsyncInferHandle` are unchanged, and jobs never use them.
No concrete bridge, JSON codec, capability flag or dispatch strategy is part
of this interface; each backend and the serving layer supply their own.

### Registration

| `BackendEntry` field           | Required | Purpose                                             |
| ------------------------------ | -------- | --------------------------------------------------- |
| `factory`                      | yes      | Builds a session; every deployment without jobs.    |
| `session_with_bridge_factory`  | no       | Builds a session and its bridge in one call.        |

`create_session_with_bridge(name, hooks)` calls the entry's
`session_with_bridge_factory` outside the registry mutex and returns a
`SessionWithBridge{session, bridge}`. Each call builds a new pair; the bridge
serves only that session, and nothing looks a bridge up or downcasts a
session. `create_session` never builds a bridge. Existing three-field
`BackendEntry{name, capability, factory}` initializations compile unchanged.

| Situation                                              | Error code    | Context                  |
| ------------------------------------------------------ | ------------- | ------------------------ |
| Backend not registered                                 | `unsupported` | none                     |
| Entry has no `session_with_bridge_factory`             | `unsupported` | `job_bridge_unsupported` |
| Factory error                                          | unchanged     | unchanged                |
| Factory succeeded without a session or without a bridge | `internal`   | `null_session`, `null_bridge` |

### Jobs

| Job class       | Payload       | Options                       | Result             |
| --------------- | ------------- | ----------------------------- | ------------------ |
| `stt_decode`    | `AudioFrames` | `language`                    | `TranscriptResult` |
| `tts_synthesis` | `TextSegment` | `language`, `voice`, `speed_milli` | `AudioChunkResult` |
| `vad_frames`    | `VadFrames`   | none                          | `VadResult`        |

Every request carries a `JobIdentity`: the submitter's `job_id`, the serving
layer's opaque `session_key` for the logical session, and the deployment
`generation`, all nonzero; every event echoes it. PCM rides in a `PcmWindow`
(a byte window of a `BufferRef`), 16 kHz mono PCM16 in and 24 kHz mono PCM16
out; telephony input is decoded and resampled by the serving layer before a
job exists. `VadFrames` state is kept per session key and utterance: a new
`utterance_id` resets it and `release_session` discards it.

A job's events follow one order:

```text
[accepted] (progress | cancel_acknowledged)* (completed | failed) released
```

`progress` carries a result fragment of the job's kind and a
`progress_sequence` of 1, 2, 3, ... up to the request's `progress_limit`.
Fragments are increments; `completed` carries the rest. A deployment that
expects no progress submits every job with `progress_limit` 0, so any
progress from its backend is a fault. `released` means the backend physically released the
job: input buffers and reservations are reusable only then.
`JobEventSequence` checks one job's events and refuses a violation with
`inference_failed` and a stable reason; bridges run it on every event before
delivery and end the job with the refusal instead of delivering it.

### Bridge guarantees

- `submit`, `cancel`, `release_session` and `health` never call or wait for
  the session lifecycle. An implementation serializes bridge work against its
  own load, prime and unload.
- Callbacks are serialized, never run on a thread that is inside a bridge
  method, and may arrive before the `submit`, `cancel` or
  `release_session` that caused them returns; register a job before
  submitting it, and note a cancellation before requesting it when
  checking events again. A callback may call `submit`, `cancel` and
  `release_session`.
- Every submitted job gets exactly one terminal event, also on reset or
  unload (`failed`, `unavailable`). `released` follows only after actual
  release or a confirmed process reap; an unconfirmed reap leaves the job
  unreleased and its reservations held.
- `release_session` cancels the session's jobs and discards its backend
  state; `on_session_released` follows the `released` of each of its jobs,
  at most once, and never while one of them is unreleased. Repeating the
  call, before or after the acknowledgement, changes nothing.
- When the session object is destroyed, every job without a terminal event
  gets `failed`, confirmed releases are delivered, and then no callback
  runs.
- The bridge fixes no executor width, queue depth, fairness or deadline and
  has no per-token call. A backend may admit one job per lane, or several
  whose progress interleaves through the same sink.

| Method            | Error code / context |
| ----------------- | -------------------- |
| `set_event_sink`  | `config_invalid` / `event_sink_null`, `event_sink_already_set` |
| `submit`          | `not_ready` / `event_sink_missing`, `backend_not_ready`, `session_releasing`; `unavailable` / `backend_unavailable`; `unsupported` / `job_class_unsupported`, `job_not_permitted`; `config_invalid` / `duplicate_job_id`; `resource_exhausted` / `job_capacity_exhausted` |
| `cancel`          | `not_ready` / `unknown_job` |
| `release_session` | `config_invalid` / `session_key_zero`, `generation_zero`; `not_ready` / `event_sink_missing`; `unavailable` / `backend_unavailable` |
| `health`          | `not_ready` / `backend_not_ready`; `unavailable` / `backend_unavailable`; `timeout` / `health_timeout` |

### Limits

Frozen ceilings live in the headers; a backend or deployment may admit less.

| Ceiling | Value | Source |
| ------- | ----- | ------ |
| `AudioFrames` window | 960,000 B | 30 s maximum utterance at 16 kHz PCM16 |
| `TextSegment` | 4,096 B | one text segment |
| `VadFrames` frames (K) | 32 | 32 x 512-sample frames; a 320 ms network frame yields at most 10 |
| `VadFrames` window | 32,768 B | 32 frames of 512 samples at PCM16 |
| Transcript text, and word texts together | 8,192 B each | one job's transcript |
| Transcript tokens | 448 | per-decode token limit; words never outnumber tokens |
| `AudioChunkResult` window | 1,440,000 B | 30 s per synthesized segment at 24 kHz PCM16 |
| VAD probabilities | 32 | one per frame |
| `progress_limit` | 1,500 | 20 ms chunks of a 30 s segment, the last in `completed` |
| Language tag / voice id | 16 B / 64 B | `en`, `ar`, `en-US`; `af_heart` |
| `failed` message / context | 512 B each | failure detail bound |

Per deployment, not in any header: `progress_limit` per job, the streaming
decode window, the VAD frame shape and K actually used, the permitted languages, voices and speeds, admission
width and queue depths, job and health deadlines, and model limits such as
the phoneme count, which a backend reports as `failed`.

### Cross-language vectors

[`protocol/fixtures/job_seam.json`](../../protocol/fixtures/job_seam.json)
holds the ceilings, the stable names, every validation reason, construction
vectors and per-job event traces. `test/unit/job_value_objects_test.cpp`
replays all of them, reading the file's notation through
`test/mocks/job_seam_vectors.hpp`. Another implementation of these objects
replays every vector whose `scope` is `all` and holds its own bounds to
`limits`. The
sidecar's job messages in `python_pytorch_ipc.json` carry these objects over
the socket; `protocol/rust/tests/python_pytorch_ipc_speech_jobs.rs` replays
the vectors through that schema and its Rust mirror, and the golden trace
frames beside it are the traces on the socket. A change to a ceiling, name
or rule edits the header and the vectors in the same commit. There is no
`protocol/schemas/job_*.json`: no process exchanges these objects as
standalone JSON, and a socket transport carries PCM as frame payload bytes,
not as buffer handles.

### Job messages in the C++ adapter

`runtime/src/adapters/python_pytorch/job_wire.{hpp,cpp}` is the adapter's
codec for the same messages: an internal unit with no socket, no thread and
no buffer manager. It builds the header of a `job_submit`, a `job_cancel`
and a `session_release` from the objects above, and reads one sidecar frame
into exactly one of:

- a `JobEvent`, for the six `job_*` kinds the sidecar sends;
- a session release, for `session_released`;
- a refusal that names one of the adapter's own messages by `message_id`,
  for an `error_event` with a readable, non-empty id: the sidecar names no
  job in it, so whoever sent the message knows which job, if any, it was.
  An `error_event` whose `schema_version` is not `0.1`, or whose error
  object cannot be read, still names its message; the refusal then carries
  `internal` with `job_message_malformed`, so the job behind the message
  ends instead of waiting for an event that never comes;
- a fault, for a job or session message that cannot be delivered as it
  stands, for a `job_*` kind the sidecar does not send, and for a
  `session_release`, which only the adapter sends. It names the job when the
  message's `job_id` can be read, and a faulty `session_released` names its
  session when the key and the generation can both be read and are nonzero.
  Its text starts with the message's kind and, for a malformed field, names
  the field;
- nothing of the job protocol's, for a frame the unary exchange judges: a
  unary response, a `ready_event`, a `metric_event` or any other `session_*`
  kind;
- an unreadable header, for a frame the adapter cannot attribute: its header
  is not a JSON object with a string `kind`, or it is an `error_event`
  without a readable, non-empty `message_id`, which no sidecar writes.
  Nothing says whom such a frame was for, so its reader ends the connection,
  as the unary exchange does for a response it cannot parse.

The decoder reads the fields it uses by name and ignores any other, as the
unary exchange does. What it reads it first holds to the schema's type and
range (an identity is an integer of at most 2^53 - 1, never a boolean or a
fraction; a token fits 32 bits; only an `audio_chunk` has payload bytes, and
exactly `payload_length` of them), and a miss is the fault
`job_message_malformed`. The value then goes through the same factories as
any other event, so a refusal carries the seam's own reason (`job_id_zero`,
`token_count_exceeded`, `pcm_misaligned`), with the code `inference_failed`
that a backend protocol violation carries. An error's texts are cut to the
seam's 512 bytes at a character boundary instead of being refused, and an
error code the adapter does not know reads as `internal`. The PCM of an
`audio_chunk` is published through a callable the caller supplies, once, and
only after the chunk passed every check against a handle that names no
buffer, so a chunk the sidecar got wrong is never published. The bytes the
callable is given are the frame's and are valid during the call only. A
handle it returns that is released or cannot hold the chunk is the caller's
fault, reported as `internal` with `pcm_publisher_handle_unusable`. No event
carries that handle, so a caller that gets a fault after its callable ran
releases what the callable returned.

It also reads the two negotiation fields: whether a `ready_event` lists
`speech_jobs_v1`, and the `job_classes` of a load response, leaving out a
capability or class name it does not know and refusing a list of more than
sixteen names, known or not. `test/unit/job_wire_test.cpp` replays every golden
file it finds beside the Rust replay and all four sections of the vectors
through it; the six vectors the socket cannot carry (an in-process buffer
handle, ill-formed UTF-8, a non-finite probability) are listed there by
name.

### The job channel in the C++ adapter

`runtime/src/adapters/python_pytorch/job_channel.{hpp,cpp}` carries those
messages on one sidecar connection. It is internal, and nothing starts it
yet: the session still makes its synchronous exchange on the socket, and the
change that enables `speech_jobs_v1` on a load hands the socket over to the
channel.

- **Two threads own the socket.** A reader reads one frame at a time, routes
  it and calls the `JobEventSink`; while the connection lives nothing else
  calls the sink. A writer sends whole frames in the order they were queued
  and is the only thread that writes. `submit`, `cancel`, `release_session`
  and the unary `exchange` only queue, so a sink callback may call the first
  three while the reader keeps reading. The sidecar needs that of its peer:
  it writes a job message under the lock its own reader takes to act on one,
  and ends a connection on which its write makes no progress for five
  seconds.
- **Everything queued is bounded.** The channel holds at most 16 unreleased
  jobs and 64 sessions whose release is unacknowledged; one more is refused
  with `resource_exhausted` (`job_capacity_exhausted`,
  `session_release_capacity_exhausted`). The queue the writer drains holds at
  most 97 frames, which is room for a submit and a cancel for each job, a
  release for each session and one unary request; a caller that finds it
  full is refused with `resource_exhausted` (`send_queue_full`), and no
  caller waits for room. The queue has its own count because a frame can
  outlive what it was queued for: a cancel whose job the sidecar released
  first, the request of an exchange that timed out.
  The `message_id` of a job's submit and cancel is kept until the job is
  released, because the sidecar's `error_event` names only that id. The last
  1,024 acknowledged sessions are remembered: the sidecar forgets a session
  it released and would answer a repeated `session_release` again, so a
  repeat for a remembered session is not sent. That memory is the whole of
  the channel's "once per session". After 1,024 later acknowledgements the
  channel has forgotten the session as well and takes its key as a new
  session's: a repeated release is sent, answered and delivered to the sink
  again, and a job submitted for it is taken. Holding "at most once" beyond
  that is the caller's, which knows its sessions.
- **Each frame goes one way.** A job event is checked by its job's
  `JobEventSequence` and delivered; one the check refuses fails the job with
  the refusal in its place when the job has no terminal event yet, and a
  release the sidecar did report still follows. A fault the codec raises
  fails the job it names the same way. A refusal that names a job's submit
  ends the job and releases it at once, since the sidecar never took it. A
  `session_released` is acknowledged to the sink once the session's last job
  is released. Anything about a job or session the channel does not hold, or
  that follows a job's terminal event, is dropped and counted, and a chunk
  no delivered event carries is released. A frame that is not a job message
  goes to the waiting exchange; so does an `error_event` naming that
  exchange's request. The exchange does not match a response to its request,
  so a caller whose exchange timed out stops the channel before the next
  one. An unreadable header ends the connection. A sidecar whose jobs an
  unload has closed refuses a `session_release`; the refusal is counted and
  the release stays pending until `stop` acknowledges it.
- **A connection ends once.** End of file, a read error, an unreadable
  header, a write that fails or takes longer than the write bound, and
  `stop` all lead the reader out of its loop, where it fails every job that
  has no terminal event with `unavailable` and `backend_reset`, or with the
  reason `stop` was given. A writer whose write failed shuts the socket down
  and leaves, which is what wakes the reader, and its error is then the
  connection's. `stop` wakes both threads with
  `shutdown(2)` on the socket's descriptor (the reader out of its wait, the
  writer out of a write the sidecar is not taking; a writer that waits for a
  frame is woken by the reader as it leaves), joins the reader, then the
  writer, and drops what was still queued: the jobs behind those frames
  have just been failed, and an exchange that ended gets the connection's
  error. A job is released, and a pending session release acknowledged, only
  after the caller's reap of the sidecar process has returned.

`test/unit/job_channel_test.cpp` drives it against
`test/mocks/scripted_sidecar.hpp`, a sidecar played by a thread that keeps
the runner's rules itself (what a load refuses, what an `error_event`
answers, the one lock around job messages, the eight-job queue, the write
stall). Of a submit's input it checks the type and that `payload_length` is
the payload's length, and it keeps the payload, so a test reads the bytes
the channel sent; it does not bound the unary requests that wait, and it
writes a unary answer under its job lock where the runner writes it
outside. It reports a frame of the adapter's as seen only once it has acted
on it, a test that plays a message for a job it does not hold fails there,
and `stop_reading` returns only once its reader has parked: a test that
waits for "sent" knows the job is admitted, and one that stops the reading
knows nothing more is read. Both ends' socket buffers are set smaller than
the largest frame, whatever the host's default. Six golden
files are replayed whole: the channel writes the recorded adapter frames,
the scripted sidecar writes the recorded answers, and the sink receives what
the codec reads from them. `test/integration/job_channel_runner_test.cpp`
runs the channel against the in-tree runner and its fixture backend. Nothing
on the wire says that a job is running (`job_accepted` is admission), so
those tests never assume it: the first job submitted is "held, with no
terminal event", whether the runner has started it or not, and later jobs
wait behind it.

### Job messages in the Python sidecar

The sidecar (`backends/python_pytorch/`) acts on job and session messages on
a connection whose `load_model` enabled `speech_jobs_v1`. Its
`job_objects.py` mirrors the objects above: `tests/test_job_seam_replay.py`
replays the `scope: all` vectors through it, and
`tests/test_speech_jobs_golden_replay.py` replays the golden frames against a
live runner. A `job_submit` is checked in the order `JobRequest::create`
checks it, and every message the sidecar sends for a job passes the
`JobEventSequence` rules first.

Of the runner profiles, the fixture profiles run `stt_decode` and
`tts_synthesis`. `faster_whisper` runs `stt_decode` and `kokoro` runs
`tts_synthesis`, each as one batch decode or synthesis per job. A Whisper
transcript carries the text and the tokens and no word units; the runner
permits a job in a language its entry declares whose audio fits one model
input window. A Kokoro job returns the segment as one `audio_chunk`; the
runner permits only its entry's language and voice at speed 1000. Either
declares its class only when the model's sample rate is the job seam's, so a
load that enables the capability for a model at another rate is answered
`unsupported`. Both mappings are provisional. No lane runs `vad_frames`, so a
submit for it fails with `job_class_unsupported`, and no job sends
`job_progress`. An integer `job_delay_ms` (0 to 60000) in a fixture
profile's entry file holds each of its jobs open that long: a test hook,
and one that any entry file of those profiles can set.

- **Threads.** A reader thread reads frames, answers `health_check`, and
  admits, cancels and releases jobs in the job table (`jobs.py`). The thread
  that runs the loop, the process's main thread, makes every backend call:
  unary requests and admitted jobs wait for it in one FIFO, so calls never
  overlap and run in arrival order. The reader's one backend call is
  `permits_job`, which reads only what the load established.
- **Bounds.** One job runs and at most 8 wait; a submit beyond that fails
  with `resource_exhausted` and `job_capacity_exhausted`. At most 8 unary
  requests wait; one more is answered `resource_exhausted`.
- **Admission.** `job_accepted` is sent when the job is admitted, not when it
  starts. A message the sidecar cannot read, or a submit that reuses the id
  of an unreleased job or carries a zero in its identity, is answered with an
  `error_event` (`config_invalid`) that carries the message's `message_id`,
  and with no job message. A job the sidecar refuses gets `job_failed` with
  the reason as the error's context, then `job_released`; one whose
  `permits_job` raises fails with the error edge's code and no context.
- **Cancellation.** A waiting job is removed, and `job_cancel_acknowledged`,
  `job_failed` (`cancelled`) and `job_released` follow at once. A running job
  is acknowledged at once; nothing interrupts the backend call, and when it
  returns its output is discarded and the job fails `cancelled`. A repeated
  cancel, or one for a job that is unknown or already ended, gets no message.
- **Sessions.** `session_release` cancels the session's unfinished jobs in
  submission order, and `session_released` follows the last `job_released`,
  at once when the session has no job. A submit for the session in between
  fails with `session_releasing`. After `session_released` the sidecar has
  forgotten the session: the same key later is a new session to it.
- **Unload.** When `unload` or another `load_model` arrives, waiting jobs fail
  with `unavailable` and `backend_unavailable`, the running job finishes
  ahead of the unload, and a `job_submit` is refused as on a connection
  without the capability until a load enables it again. A `job_cancel` or
  `session_release` still acts while that job is unreleased, so its
  `job_failed` (`cancelled`), `job_released` and `session_released` come
  before the `unload_response`; once it is released they are refused too.
- **Writes.** A frame is written whole under one lock, and a job's messages
  in the order of its state changes. A write that fails, or makes no progress
  for 5 s, ends the connection, as a frame error does, and EOF once a load
  has enabled jobs: the socket is shut down, waiting work is dropped
  unanswered, and the loop returns once the backend call in progress has.
  When the peer half-closes a connection that never enabled jobs, the
  requests already read are run and answered in order, and then the socket
  is shut down. The rule requires the other side to read continuously, while
  it writes too. Job messages are written under the job table's lock, so
  while a large result drains, health and cancel answers wait behind it. A
  peer that takes bytes slowly keeps a write alive; one that stops reading,
  for instance to write a second large submit without reading, has its
  connection ended after the 5 s.
- **Message ids.** A message the sidecar originates, the `ready_event`
  included, carries `s<n>` from one counter per connection.

## Installed backends and runner profiles

The registry above is what a build of the serving worker can run. What a
host has *installed* is a separate record on the management plane: a backend
package ships a descriptor at
`/usr/share/tensorplate/backends/<backend_name>/backend.json`
([`protocol/schemas/backend_descriptor.json`](../../protocol/schemas/backend_descriptor.json)),
and the agent's startup probe and `tensorplate doctor` read it through one
reader, `BackendDescriptor::read_from` in the protocol crate. The serving
worker's sidecar launcher reads one part of it, the runner profile list, with
a reader of its own ([below](#launching-a-runner-profiles-sidecar)); nothing
else parses the file.

A runner profile is an environment a sidecar runs in, installed by packages
other than the one that owns the descriptor. Such a package declares its
profile in `<backend_name>/runner_profiles.d/<name>.json`, one
`runner_profile_declaration` document per file, and the reader merges the
declarations into the descriptor's `runner_profiles`: the descriptor's own
entries first, then each `*.json` file in file name order. Consumers see only
the merged list. It is the list `DeploymentDescriptor::derive` takes as the
installed runner profiles, so a bundle's `runner_profile` resolves exactly
when a package that declares it is installed. The agent's deploy path does
not derive a deployment descriptor yet.

The merge fails closed. One refused declaration refuses the descriptor, and
the backend is then not runnable for any bundle:

| Situation | `BackendDescriptorError` |
| --- | --- |
| Declaration unreadable, or its entry not a regular file | `Io` |
| Declaration is not valid JSON, or has a missing, unknown or mistyped member | `Malformed` |
| Declaration carries another `schema_version` | `UnsupportedSchemaVersion` |
| Declared profile breaks a `runner_profiles` rule, or names another backend | `Invalid` |
| Profile id declared by two sources | `DuplicateRunnerProfile` |
| Profile names a package that is not installed | `PackageNotInstalled` |
| The package database cannot be asked | `PackageInventoryUnavailable` |

Installed means dpkg has configured the package or is running one of its
scripts: the states `installed`, `triggers-pending`, `triggers-awaited` and
`half-configured`, and not `unpacked`. The reader asks through the
`PackageInventory` trait; the default implementation runs one bounded
`dpkg-query` and is not consulted when no runner profile exists, so a host
without profile packages needs no dpkg.
[`docs/install/speech-runtime.md`](../install/speech-runtime.md#runner-profile-declarations)
has the operator's view and the reason for that reading of dpkg's states.

The startup probe reports `PackageNotInstalled` as its own state,
`runner_profile_package_missing`, which the platform reason vocabulary
classifies as `missing_backend_package`. Every other refusal above is a
`descriptor_malformed` probe state and `accelerator_runtime_unavailable`.

### Probing the interpreters

`probe_backend` (`protocol/rust/src/backend_probe.rs`) is shared by the
agent's startup probe and `tensorplate doctor`. Its report holds two kinds
of state:

- `state` is a backend-wide refusal (an absent or refused descriptor, a
  declared package that is not installed, a runtime version below the
  descriptor's minimum) or else the state of the descriptor's own
  interpreter: `python.interpreter` exists and runs, meets the minimum
  version, imports the backend module, and imports PyTorch where the
  descriptor requires it. A bundle that names no runner profile is served
  there.
- `runner_profiles` holds one entry per installed profile, in the merged
  order: the declared record, a state and what the interpreter printed for
  `sys.version` and `sys.prefix`. A profile's interpreter must run (one the
  system will not execute is missing, as the launcher refuses it) and is
  held to the descriptor's `python` requirements. It is run with the three
  variables of the
  [launcher's table](#launching-a-runner-profiles-sidecar), which the probe
  builds from a Rust mirror of the launcher's function. It is never held to
  the descriptor's `pytorch` requirement: a profile's engines are its own.
  The list is empty under a backend-wide refusal, when nothing is run.

`BackendProbeReport::serving_state` picks the state that decides one bundle:
the backend-wide refusal if there is one, otherwise the named profile's
state, or the descriptor's own when the manifest names no profile. The
agent's deploy gate (`verify_before_staging` in `agent/src/bundle.rs`) refuses
before staging on anything but `Runnable`, and refuses a bundle whose
profile has no entry as `missing_backend_package`; the reason for either
comes from `PlatformReason::for_serving_state`. A bundle whose profile is
installed and runnable is still refused, as `unsupported` with
`bundle_r6_compute_type`, when its `compute_type` is not one of the
profile's `compute_types`, and a runner profile named under a backend that
has no descriptor is refused as `config_invalid` with
`bundle_r6_runner_selector`. Every query the probe
runs is started from `/` and killed at the probe's limit, five seconds
by default; the PyTorch import, which reads far more from disk, gets 120. Before this split the
gate required PyTorch in the descriptor's interpreter for every bundle.

The probe stops at the interpreter and the sidecar module. Whether a
profile's engines import, which copy of a library the loader maps and
whether the sidecar's temporary directory allows execution are
`tensorplate doctor`'s `runner_profile_dependencies` and
`runner_launch_environment` findings, which reuse the same mirror of the
launcher's environment. The mirror is pinned by Rust tests and shares no
fixture with the launcher. Doctor reports the launcher's refusal of a search
path holding `:` or `;` and of a `noexec` temporary directory; whether the
worker can write to that directory is not checked outside the launcher,
because doctor runs as another user.

### Launching a runner profile's sidecar

A bundle that names a `runner_profile` is served by a sidecar started in
that profile's environment, and nowhere else. The name travels in three
steps:

1. The agent reads it from the staged bundle's manifest when it renders the
   worker's configuration and writes it as `deployment.model.runner_profile`
   ([`config/schemas/serving_worker.json`](../../config/schemas/serving_worker.json)).
   A bundle that names none gets no such member. A staged bundle whose
   manifest cannot be read fails the prepare; no configuration is written.
2. The worker carries it on the model's `ModelSpec` as `runner_profile()`.
   A member that is present and not a non-empty string is a configuration
   error. The `model_spec` message to the sidecar does not carry it.
3. At load, the `python_pytorch` adapter reads the installed descriptor's
   merged `runner_profiles` (the descriptor directory is
   `TP_BACKEND_DESCRIPTOR_DIR` when set, as for the agent) and starts the
   sidecar from the entry with that id.

The sidecar's `argv[0]` is the entry's `interpreter`. Its environment is the
worker's, with three variables set:

| Variable | Value | Why |
| --- | --- | --- |
| `LD_LIBRARY_PATH` | The entry's `library_search_paths`, joined with `:`; empty when it has none | Libraries the profile loads by name resolve to its own copies. A value the worker inherited does not reach the sidecar. |
| `ORT_DISABLE_TELEMETRY` | `1` | With it unset, the onnxruntime 1.30 the speech runtime locks was observed resolving a telemetry host and attempting connections to it within about ten seconds of import. |
| `TMPDIR` | The worker's temporary directory: the first of `TMPDIR`, `TMP`, `TEMP` and `TEMPDIR` in its environment, otherwise `/tmp` | Named explicitly because the profile writes and loads a shared library there. |

There is no fallback for such a model. `TP_PYTHON_PYTORCH_EXECUTABLE`,
`TP_TEST_PYTHON_EXE`, `TP_TEST_PYTHON` and `PATH` select the interpreter only
for a model that names no runner profile, which is launched as before. The
load is refused before any process starts, with a fixed message that names
neither the profile nor a path:

| Situation | `Error::Code` |
| --- | --- |
| `TP_BACKEND_DESCRIPTOR_DIR` is relative; the descriptor or a declaration cannot be read or breaks a rule above; an id is declared twice; a search path contains `:` or `;` | `ConfigInvalid` |
| No installed entry has the model's runner profile id | `Unsupported` |
| The entry's interpreter is not an executable file | `Unavailable` |
| The temporary directory is not a directory the worker can write to and search, or its filesystem is mounted `noexec` | `Unavailable` |

The launcher applies the declaration and `runner_profiles` rules of the
table above to every entry, and refuses the whole list on one fault, as the
Rust reader does. The two readers run the same documents in their tests
(`protocol/rust/tests/fixtures/runner_profile_declarations/`), a repeated
JSON key, a byte order mark and bytes after a NUL among them. They are not
the same reader, and differ in three ways:

- The launcher does not ask the package database. That check is the agent's,
  whose probe refuses every deploy for the backend while a declared package
  is missing.
- Of the descriptor itself the launcher reads `backend_name`,
  `schema_version` and `runner_profiles`. The Rust reader also validates the
  package, interpreter, sidecar and capability members, so a descriptor it
  refuses for one of those is still read by a worker started without the
  agent.
- The launcher refuses a NUL inside `interpreter`, `environment_root` or a
  search path, which would end the path early where it is handed to the
  system. The Rust reader accepts one.

The sidecar still inherits the rest of the worker's environment, including
any `PYTHONPATH`, and the profile's interpreter is started without Python's
isolated mode.

## Non-goals

V01-E05-F01 explicitly does *not* implement:

- adapter initialization-time probing (the factory may surface
  `LoadFailed` itself, but the registry does not),
- capability serialization out of process (the JSON Schema is
  declared so the agent/IPC layers in V01-E07/V01-E08 can adopt it
  without revisiting this header),
- backend selection or scheduling decisions on top of capability data,
- runtime feature flag flipping outside the build system.
