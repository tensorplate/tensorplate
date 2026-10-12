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

### Conformance cases and the fixture bridge

`test/contract/bounded_job_bridge_conformance.hpp` holds what the bridge
header promises the caller of a bridge whose backend runs `stt_decode` and
`tts_synthesis`, as one parameterised suite: thirty-seven cases, each under
the header text it holds. A binding (`BridgeUnderTest`) supplies the bridge
with its backend, the session's side of it (prime, unload, a backend that
is lost) and a hold on started jobs; the cases force every order they need
with that hold and never wait on the clock. A test binary includes the
header in one source file and instantiates the suite with the bridges it
builds.

Where the header leaves a backend a choice, the binding states what its
backend does (`BridgeUnderTest::Choices`), and the comment above a case
names each choice the case relies on:

| Choice | For a binding that does not state it |
| ------ | ------------------------------------ |
| `holds_started_jobs`: a job that has started gets no terminal event while the binding holds its backend | the cases that need an open job are skipped |
| `refuses_vad_frames`: the backend does not run `vad_frames` | the cases that submit a class the backend refuses are skipped |
| `runs_one_job_at_a_time`: a job submitted while a started one is held has not started | the second job may be running: it may end only with the first, and an unload may let it complete |
| `ends_a_waiting_job_at_its_cancel`: a job that has not started ends as soon as it is cancelled | the job may end only once the held one was released |
| `fails_a_held_job_that_was_cancelled`: a job cancelled while it is held ends `failed` with `cancelled` | the job may end `completed` |
| `open_jobs`: how many jobs the held backend has open at once | the two cases at the backend's bound are skipped |

The binding's unload returns with nothing owed and its lost backend
releases nothing before the unload; either ends a held job. Where the
header allows two outcomes the cases accept both: a cancelled job ends
`failed` (`cancelled`) or `completed`, with or without
`cancel_acknowledged`; a job that fails may do so without `accepted`; a job
beyond the bound is refused by `submit` or ended by `failed`; a session
release before the prime succeeds or answers `backend_unavailable`; a
backend lost before its prime answers `backend_not_ready` or
`backend_unavailable`, because the header lists "before its session is
primed" under the first and does not say whether that still holds for a
backend that is gone. Which of them a backend produces is for its
binding's own tests. A session unloaded before its prime is not among
them: the header names an unloaded session under `backend_unavailable`,
and the cases accept nothing else.

The order of `submit`'s refusals is held one pair at a time, with two
entries of the header's list applying at once: a missing sink ahead of a
backend that is not ready; a backend that is not ready, and one that is
unavailable, ahead of a class it does not run; that class ahead of a
releasing session; a releasing session ahead of a duplicate job id; a
duplicate job id ahead of exhausted capacity.

Not held here, because a binding has no hook for them, and left to the
adapter's binding and its own tests: `job_not_permitted` and its place in
that order, `health_timeout`, the `failed` that replaces a backend event a
job's order refuses, a late or foreign event being dropped, bridge calls
beside a running lifecycle call, and calls from several threads at once.
The destruction of the session is for a binding's own tests.

`FixtureJobBridge` (`test/mocks/fixture_job_bridge.hpp`, the test library
`tp::fixture_job_bridge`) is the one binding today: a bridge with a
backend in the test's own process, for tests of what sits above a bridge.
No production binary contains it. It keeps the refusals and each job's
event order through `FakeBoundedJobBridge`, which it wraps, and plays the
backend by the in-tree sidecar runner's rules: one job runs and eight wait
in submission order; one more ends with `failed`
(`job_capacity_exhausted`) without `accepted`; a job is `accepted` when it
is taken in; a cancellation is acknowledged at once and ends a waiting job
then, the running one when its work returns, both as `cancelled`; a session
release cancels the session's unfinished jobs, the running one included; a
lost backend fails its jobs with `backend_reset` and releases them only at
the unload. Before the prime it takes a session release and acknowledges
it, and a backend lost or a session unloaded before the prime answers
`backend_unavailable`. Its results are those of the runner's fixture
backend (the word `hello` ending a quarter of the payload's bytes after the
window's start; 160 bytes of silence for each byte of text) unless the test
passes a result function, which receives the request and runs with the
bridge's lock held.

Two options (`FixtureJobBridgeOptions`) make it produce the other outcome
the header allows, so that what is developed against it meets both:
`submit_refuses_at_capacity` has `submit` itself refuse a job while nine
are unreleased, as the adapter's job table refuses one beyond its bound,
and `cancels_arrive_late` lets a cancelled job, started or not, run to its
end and complete with no `cancel_acknowledged`. The T1 binary runs the
suite against the fixture four times: as it is; refusing at capacity; with
late cancels, stating no choice but its hold and the class it refuses, so
that the cases pass on the header alone; and without its hold.

One thread runs the jobs and makes the callbacks while the backend lives;
the unload returns once every callback owed has run, on its caller's
thread or on that one, so `prime`, `lose_backend`, `unload` and the
destructor are never called from a sink callback. A test can hold started
jobs, fail the next one, set what `health` answers, ask whether everything
submitted so far was worked through and every callback for it has
returned, an unload's on another thread included (`settled`), and read the
events of the fixture's own backend that a job's order refused, which are
none unless the fixture is wrong.

It differs from the adapter with a real sidecar in six ways. A job starts
at its submit when nothing runs, so the eight that wait are exact, where
the runner counts a job its backend thread has not taken yet among them. A
job's work runs in one step with the bridge's lock held: a cancellation
finds a started job before its work, which then does not run, or after its
end, never in between; it comes before the work for certain while a test
holds the jobs, and for a job cancelled by the callback that submitted it.
An unload fails every unfinished job with `session_unloaded` at once, where
the runner fails the waiting ones itself and lets the running one end. The
fake it wraps refuses a submit while 64 jobs are unreleased. A prime after
an unload or a loss does not reopen it: there is no second load of one
session. And `release_session` on a lost or unloaded backend answers
`backend_unavailable` before it looks at the sink or the arguments.

The fixture's results are held to the golden frames of the sidecar's job
messages and its other rules by its own tests, both in
`test/unit/bounded_job_bridge_conformance_test.cpp`; no test runs the same
cases against the runner yet.

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
messages on one sidecar connection. It is internal. The adapter's session
starts it on a load that enabled `speech_jobs_v1` and stops it (next
section); a session not built for jobs never starts it. `start` refuses a
socket that is not open (`internal`, `socket_closed`) and a load that
declared neither class a lane runs (`unsupported`,
`job_class_unsupported`), as the runner refuses a runner with no such class.

- **Two threads own the socket.** A reader reads one frame at a time, routes
  it and calls the `JobEventSink`; while the connection lives nothing else
  calls the sink. A writer sends whole frames in the order they were queued
  and is the only thread that writes. `submit`, `cancel`, `release_session`,
  the unary `exchange` and the health probe only queue, so a sink callback
  may call the first three while the reader keeps reading. The sidecar needs
  that of its peer:
  it writes a job message under the lock its own reader takes to act on one,
  and ends a connection on which its write makes no progress for five
  seconds.
- **Everything queued is bounded, and the queue is exact.** The channel
  holds at most 16 unreleased jobs and 2,048 sessions whose release is
  unacknowledged; one more is refused with `resource_exhausted`
  (`job_capacity_exhausted`, `session_release_capacity_exhausted`). Those two
  tables are the only bounds a caller meets. A frame in the queue the writer
  drains belongs to a record: a job in the table, a pending release, the
  exchange that waits, or the health probe that is unanswered. A record has
  each of its frames queued at most once, and a frame still queued when its
  record goes leaves with it: a job's submit and cancel when the job is
  released, a release when it is acknowledged, a unary request when its
  exchange is answered, gives up or fails with the connection, the probe's
  request when a frame answers it or the connection ends. So the queue never
  holds more than 2,082 frames (a submit and a cancel for each of 16 jobs, a
  release for each of 2,048 sessions, one unary request, one health probe),
  nobody is refused or waits for room in it, and `cancel` and
  `release_session` are never refused for capacity by it. The writer takes a
  frame out of the queue before its first byte, so a frame it has begun is
  written whole whatever becomes of its record.
  The `message_id` of a job's submit and cancel is kept until the job is
  released, because the sidecar's `error_event` names only that id.
- **Who can find the pending releases full.** 2,048 is the protocol's
  ceiling on logical sessions. A session keeps its slot in the session
  manager until its release is acknowledged, and the session manager admits
  at most that many sessions, so it never has more releases pending. Only a
  caller that is not bounded that way is refused
  (`session_release_capacity_exhausted`), which is why the bridge's list of
  refusals above does not name it. The refusal is not reported as
  `backend_unavailable`: that would tell the caller the session is released.
- **A released session stays released.** The sidecar forgets a session it
  released and would answer a repeated `session_release` again, so the
  channel records every session key whose release was acknowledged, for as
  long as it lives, a stop and a new start included: a repeat is not sent,
  and a job for the key is refused (`session_releasing`). The record holds
  runs of consecutive keys, not single keys. Between two runs lies a key
  that is not in the record, so the runs number at most the keys below the
  highest recorded one that are not in it, plus one. A session manager
  issues its keys in increasing order and reuses none, so those keys are
  its sessions that still hold a slot, which it bounds, and the keys never
  recorded here. A release made while the channel has no live connection is
  refused with `backend_unavailable`, and the bridge's header tells the
  caller to treat the session as released and never to submit its key
  again; a channel that was ever started records the key as released all
  the same (unless the key or the generation is one a live channel refuses
  as an argument), so a job for it is refused on the next connection as
  well and a lost connection adds no run, however many sessions were open
  when it
  went. What still adds one for good is a key that never reaches the
  record: a session its holder did not release through this channel, or one
  released before the channel's first start, when there was no backend to
  hold a session; that state comes once, so those keys number at most the
  sessions that existed then. Each such key keeps the runs on its two sides
  apart, so the record grows by at most one run for each.
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
  exchange's request. A frame of either kind that carries the id of the
  health probe goes to neither: it is the probe's answer (next point).
  The exchange does not match a response to its request,
  so a caller whose exchange timed out stops the channel before the next
  one. An unreadable header ends the connection, and so does a frame that
  declares a payload larger than the longest synthesized segment a job
  returns (`kMaxAudioChunkBytes`): its body is neither allocated nor read. A
  sidecar whose jobs an
  unload has closed refuses a `session_release`; the refusal is counted and
  the release stays pending until `stop` acknowledges it.
- **Health is a probe of the channel's own.** `probe_health` answers what
  `submit` would for the channel's state, in the same order: before the
  channel admits jobs `backend_not_ready`, and once admission closed, the
  connection ended or the channel stopped `backend_unavailable`, without
  asking the sidecar. While the channel admits it sends a `health_check`
  with an id of its own and reads the answer's `health.ready`: false is
  `backend_not_ready`, and an answer that cannot be read as a health
  report, or an `error_event` naming the probe, is `backend_unavailable`.
  The channel's state is read again when the call's wait ends and comes
  first: an unload that closes admission meanwhile wakes the caller, which
  returns `backend_unavailable` before its deadline, whatever the sidecar
  said and also when it said nothing. While the probe is unanswered it is
  matched by its `message_id` ahead of the exchange slot, so it is answered
  beside a `prime` or an `unload` that waits and its answer is not taken
  for theirs. The id guards the slot no longer than that: a second frame
  carrying the id of a probe already answered, which only a faulty sidecar
  sends, is a stray frame like any other, handed to the exchange that waits
  (the session then ends the connection over the id) and dropped and counted
  when none does. At most one probe is unanswered. It belongs to the
  channel, not
  to a caller: while the channel admits, a call made beside a probe that
  stands sends nothing and waits for its answer, each caller with its own
  deadline, and a caller whose deadline
  passes gets `health_timeout` and leaves the probe standing. An answer
  that comes after every caller gave up is dropped by its id and counted,
  and only then does the next call send a new probe. So against a sidecar
  that never answers, every later call ends `health_timeout` until the
  connection ends, which ends the probe as `backend_unavailable`, and the
  queue holds at most one probe frame however many callers time out. The
  session's own health check inside `prime` stays a unary exchange.
- **A connection ends once.** End of file, a read error, an unreadable
  header, a write that fails or takes longer than the write bound, and
  `stop` all lead the reader out of its loop, where it fails every job that
  has no terminal event with `unavailable` and `backend_reset`, or with the
  reason `stop` was given. A writer whose write failed shuts the socket down
  and leaves, which is what wakes the reader, and its error is then the
  connection's. A reader that ends the connection itself shuts the socket
  down too, so a sidecar that is still running reads end of file and a
  writer inside a frame is woken. `stop` wakes both threads with
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
the channel sent; it does not bound the unary requests that wait, it
writes a unary answer under its job lock where the runner writes it
outside, and it declares the job classes a test gives it, where the runner
declares only those a lane runs (four tests rest on that; the head of the
file names them). It holds every frame of the adapter's to the runner's
envelope rules (`schema_version` "0.1", a `message_id`, a `kind`) and
refuses one that breaks them as the runner does. It reports a frame of the
adapter's as seen only once it has acted
on it, a test that plays a message for a job it does not hold fails there,
and `stop_reading` returns only once its reader has parked: a test that
waits for "sent" knows the job is admitted, and one that stops the reading
knows nothing more is read. It can hold its health answers back, which no
runner does, so that a test has a probe unanswered for as long as it needs:
a deadline that has already passed then times a call out without a sleep,
and a count of the callers that wait tells a test when a call on another
thread has joined the probe. Both ends' socket buffers are set smaller than
the largest frame, whatever the host's default. Six golden
files are replayed whole: the channel writes the recorded adapter frames,
the scripted sidecar writes the recorded answers, and the sink receives what
the codec reads from them. `test/integration/job_channel_runner_test.cpp`
runs the channel against the in-tree runner and its fixture backend. Nothing
on the wire says that a job is running (`job_accepted` is admission), so
those tests never assume it: the first job submitted is "held, with no
terminal event", whether the runner has started it or not, and later jobs
wait behind it. The harness reads the recording sink's violations in its
destructor, after its own `stop`, so what a stop delivers is checked in
every test.

### The session and its job channel

`python_pytorch_session.{hpp,cpp}` owns the channel's lifecycle. Whether a
session runs speech jobs is a field of the adapter's internal configuration
(`PythonPytorchConfig::speech_jobs`, off by default); the serving
configuration has no key for it, and the registered backend does not set it.
`make_python_pytorch_session_with_channel` returns the session together with
its channel: the holder of the channel sets the sink and submits, cancels
and releases, and never starts or stops.

- **Negotiation.** A session built for jobs requires the sidecar's
  `ready_event` to list `speech_jobs_v1`: a list that is not one fails the
  load, and a sidecar that does not list the capability is not asked to
  load (`unsupported`, `speech_jobs_not_offered`). The `load_model` then
  carries `capabilities: ["speech_jobs_v1"]`, the response's `job_classes`
  are read, and the channel is started on the socket. Any failure on the way
  fails the load and ends the sidecar. A session not built for jobs enables
  nothing, whatever the sidecar lists. A session built for jobs without a
  buffer manager is refused at load (`config_invalid`), before a sidecar is
  started: the channel would refuse every job for good, and the tensor path
  refuses a request without one the same way.
- **One slot.** From the load on, the channel's reader owns the socket and
  the session's `prime`, health check and `unload` go through the channel's
  one exchange slot. The slot hands over the next frame that is neither a
  job message nor the channel's own health answer, so the session compares
  the response's `message_id` with its request's and treats another as a
  failed connection. On the socket, which
  carries every exchange of a session without jobs and the load of one with
  them, ids are not compared, as before. `infer` is refused on
  such a session (`unsupported`, `tensor_inference_unsupported`) and nothing
  is sent: a tensor request would hold the slot against the lifecycle, and
  its response could exceed what the reader takes.
- **Admission.** Jobs are admitted once `prime` has returned and its health
  check has passed, not before. `unload` closes admission before it sends
  the unload, because the sidecar fails the jobs that wait when the unload
  arrives; it then stops the channel with `session_unloaded`. It cancels
  nothing: the sidecar answers the unload once the job it is running has
  ended, and the session waits for that answer up to its startup timeout
  (15 seconds by default).
- **Every end is a stop.** `unload`, the destructor and every failed
  exchange end the connection through the channel's `stop`, with
  `session_unloaded` or `backend_reset` as the reason open jobs fail with,
  and with the end of the sidecar process as the reap. The channel's threads
  have left the socket before the process object closes it. A failed
  exchange stops the channel whatever the failure: after a timeout a late
  answer could be taken for the next one. The same helper serves a session
  without jobs, where the stop only runs the reap.

What a caller of the session, and the holder of its channel, must not
assume:

- `prime`, `unload` and the destructor can stop the channel, and a stop
  waits for a sink callback that is running and delivers `released` on the
  calling thread, without a time bound. None of them is called from a sink
  callback, nor while holding a lock a callback takes.
- A job's `released` after a stop means the session's reap call has
  returned. Today that call blocks until the sidecar process has been
  waited for; it reports no result, and the channel releases whatever the
  reap did.
- `unload` does not end a running job. It waits behind it for up to the
  startup timeout; a job that outlasts the bound makes the unload fail with
  `timeout`, the open jobs fail with `backend_reset`, not
  `session_unloaded`, and the sidecar is ended. A holder that wants its jobs
  to end as unloaded ends or cancels them before it unloads.
- Nothing announces that the connection ended. Open jobs fail at once; a
  session with none learns of it at its next exchange, which fails and
  stops the channel: with the connection's error, or with `sidecar process
  exited` when the process itself is gone, which the session finds before
  it sends.
- A cancel waits its turn. Frames are sent in the order they were queued, so
  a `job_cancel` leaves behind every frame queued before it, the submits
  among them, and a large submit the sidecar is slow to read delays it.
  Nothing sends a cancel ahead of them today.
- A sink callback that does not return stops the reading: the session's
  health check then times out, and the sidecar gives up a peer that has not
  read for five seconds.
- A frame that is neither a job message nor an `error_event`, arriving
  while an exchange waits, is taken as that exchange's answer unless it
  carries the id of the unanswered health probe; when it is not the
  answer, the exchange fails and the backend is reset, as a response of
  the wrong kind always has. With no exchange waiting it is
  dropped and counted, as is an `error_event` that names neither a job's
  message nor the waiting request. Besides the unary responses, the
  schema's frames of that class are the `ready_event`, which the runner
  writes once, before it reads a request, and `metric_event`, which no
  runner sends today.

`test/unit/python_pytorch_job_session_test.cpp` replays the two recorded
negotiation files through the session's load against the scripted sidecar
and then forces each order above: the health check held unread while a
submit is still refused, a health check that answers not ready, the unload
held behind a running job while a submit is already refused and the slot
is taken, the reap observed only after the open job has failed, a sidecar
that dies, a connection that ends under a live process, an answer to
another request, an exchange that times out, an unload that times out
behind a job the sidecar never ends. It also loads a session built for jobs
on a sidecar whose `ready_event` has no `capabilities` key and one without
a buffer manager, and a session not built for jobs on an answer that
carries another request's id.
`test/integration/python_pytorch_job_session_test.cpp` runs both speech
classes on a channel the session started against the in-tree runner.

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
