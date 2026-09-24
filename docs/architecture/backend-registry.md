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
replays all of them. Another implementation of these objects, such as a
future sidecar job message set, replays every vector whose `scope` is `all`
and holds its own bounds to `limits`. A change to a ceiling, name or rule
edits the header and the vectors in the same commit. There is no
`protocol/schemas/job_*.json`: no process exchanges these objects as
standalone JSON, and a socket transport carries PCM as frame payload bytes,
not as buffer handles.

## Non-goals

V01-E05-F01 explicitly does *not* implement:

- adapter initialization-time probing (the factory may surface
  `LoadFailed` itself, but the registry does not),
- capability serialization out of process (the JSON Schema is
  declared so the agent/IPC layers in V01-E07/V01-E08 can adopt it
  without revisiting this header),
- backend selection or scheduling decisions on top of capability data,
- runtime feature flag flipping outside the build system.
