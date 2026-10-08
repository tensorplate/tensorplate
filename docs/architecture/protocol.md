# Protocol format and binding strategy

This document records the v0.1.0 decisions for cross-component
protocol payloads. The schemas under `protocol/schemas/` are the
authoritative source; bindings under `protocol/rust/` and
`include/tensorplate/` mirror them.

See also:

- `protocol/schemas/README.md` — per-schema ownership.
- `docs/architecture/versioning.md` — runtime / protocol / schema /
  bundle-format version surfaces.

## Format

| Payload family | Format | Justification |
|----------------|--------|---------------|
| Configuration  | JSON Schema Draft 7 | Operator-edited; needs to be human-readable and tool-friendly. |
| Bundle manifest (bundle format) | JSON Schema Draft 7 | Same audience as config. |
| Cross-component control payloads (desired_state, worker_status, health_event, deploy_transaction) | JSON Schema Draft 7 | Crosses Rust/C++ language boundaries; JSON keeps the schema human-readable; the volume is low (status / event ticks, not request hot-path). |
| HTTP `/infer` payload | JSON Schema Draft 7 (header) + raw bytes | The header documented in `infer_request.json` / `infer_result.json` rides as JSON; tensor payloads ride as raw bytes per `BufferRef` / `TensorView` metadata. v0.1.0 does not negotiate an alternative encoding; V01-E07 lands the HTTP server. |
| Python/PyTorch sidecar IPC | JSON header + raw payload bytes | Schema captured in `python_pytorch_ipc.json`. Wire format: a 16-byte big-endian prefix (magic, wire version, header length, payload length), the JSON header, then the payload bytes: tensors, or a job's PCM or text input or its synthesized audio. JSON-encoding tensors was an explicit non-goal. |
| External stream session envelope (draft) | Protocol Buffers (proto3), for gRPC | `protocol/proto/tensorplate/stream/v1/session.proto`: typed events on one ordered bidirectional stream per session, with audio as bytes. A draft that no listener serves yet; see [`protocol/README.md`](../../protocol/README.md#streaming-session-envelope). |
| Agent-to-worker runtime control | Newline-delimited compact JSON | `worker_control.json` defines six runtime operations. The Rust and C++ codecs use the same byte-exact golden frames under `protocol/rust/tests/fixtures/worker_control_*.jsonl`. Each request and response names the member, transaction and correlation id. Frames are limited to 65,536 bytes including the newline. The serving worker answers them on its control thread when it is started as a member of a deployment generation. |

We deliberately did **not** introduce protobuf in v0.1.0, and the JSON
payload families above still do not use it. The v0.1.0 hot
path runs in-process within `tensorplate-serving`; cross-process
payloads are control-plane (low volume) plus the sidecar IPC where
the tensor payload is raw bytes regardless of the header encoding.
Adding a binary header format earns no measurable throughput on the
v0.1.0 critical path and would split the operator-facing config and
the IPC headers across two encodings.

The external stream session envelope is the one protobuf contract. It is
a draft, it changes none of the JSON contracts, and no listener, server
or client uses it yet.

## Versioning

Every JSON payload carries a `schema_version` string of the form
`"MAJOR.MINOR"`, value-fixed (`const`) to the protocol version.
v0.1.0 is `"0.1"`. The stream session envelope carries none: it is
versioned by its protobuf package name, `tensorplate.stream.v1`, and is
outside the protocol version. Its evolution rules are in
[`protocol/README.md`](../../protocol/README.md#streaming-session-envelope).

Decoders **must** call
`tensorplate_protocol::decode_with_version_check` (Rust) — or a
version-checking C++ codec such as the worker-control codec — instead of
`serde_json::from_str` / `nlohmann::json::parse` directly. The helper rejects unknown
versions with a typed error so that the runtime maps them to
`Error::Code::Unsupported` and surfaces a stable error code to the
operator. Bypassing the helper loses the guarantee.

Bundle manifests decode through `tensorplate_protocol::bundle::parse_bundle`
instead: format 0.2 fields are checked against the manifest's own text, so
the helper refuses a format 0.2 manifest rather than return it unchecked.
Manifest-local rule failures expose `BundleRuleCode` through the parser and
agent, with its stable string in the existing error context; the shared wire
error enum and protocol version remain unchanged. A format 0.2 manifest may
declare variant lineage (`base_model_ref`, `variant_identity`); the parser
validates the declaration and refuses the bundle, since every variant kind is
reserved.

Both readers require the exact bundle format allowlist `0.1` or `0.2`;
the deployment-descriptor reader applies that same allowlist to its bundle
reference before checking the configuration and descriptor digests.

Deployment descriptors decode through
`tensorplate_protocol::DeploymentDescriptor::from_json`, which also requires
the text's canonical JSON form, the manifest decoder's verdict on its format
0.2 fields and both digests (see
[`docs/bundles/integrity.md`](../bundles/integrity.md#deployment-descriptor-digests)).
Converted to a `ProtocolError`, its `schema_version` and
`canonical_json_version` refusals are `Unsupported`, like the helper's.

Bumping the protocol version requires touching:

- `CMakeLists.txt` (`TP_PROTOCOL_VERSION_MAJOR/MINOR`)
- `protocol/rust/src/lib.rs` (`PROTOCOL_VERSION_MAJOR/MINOR`,
  `SCHEMA_VERSION`)
- All schemas under `protocol/schemas/`
- `docs/architecture/versioning.md`
- `CHANGELOG.md`

Within a major version, additive public or cross-language field additions
are minor bumps. Renames, removals, and meaning changes are major bumps and
require migration tooling.

There is one narrow pre-1.0 exception for the local Rust agent control
response. An optional, output-only `AgentStatus` field may be added under
`0.1` when old Rust readers ignore it and new readers default its absence.
The agent and CLI currently share one exact-match version decoder with every
other protocol payload; bumping that global constant for a local status field
would reject mixed-version agent/CLI installs and every unrelated `0.1`
payload rather than provide minor-version compatibility. `supervision`,
`serving_url`, `platform_telemetry`, `resident_set` and `control_features`
follow this rule. The schema, Rust binding, and round-trip tests still
change together, and the exception does not apply to changed meanings.
Schema validators pinned to the previous file remain strict because
`additionalProperties` is false; compatibility here is the deployed
serde-reader contract. Retire this exception when per-schema minor-version
negotiation replaces the global exact-equality check.

Request additions to the same local API follow a stricter rule, because
agents before the set-mutation shapes ignore request fields they do not
know. From that release on the agent refuses any request field, payload or
`null` it does not know, so a later addition is refused rather than
misread. An added request field must mean exactly today's behaviour when
absent and be omitted at that default, so existing requests stay
byte-for-byte what earlier agents read (a recorded golden exchange pins
this). A client sends a non-default value only to an agent that lists the
matching `control_features` entry, since an older agent would act on the
rest of the request. A new operation needs no such check: an older agent
refuses an unknown `op`. `set_operation`, `admission_mode`, `test_count`,
`evidence_ref`, `rollback.deployment_id`, `undeploy` and `recover` follow
this rule, which retires with the response exception.

A second narrow pre-1.0 exception covers the shared closed enums: a value
may be appended under `0.1` to the error-code enum (`error.json` and every
schema that inlines a copy of it) and to the `reason` and `category` enums
of `failure_reason.json`. Existing values are never renamed, removed,
reordered or given a new meaning, and every schema copy and every language
mirror (C++ `Error::Code`, the Rust `ErrorCode`, `FailureReason` and
`FailureCategory`, the Python sidecar's `ERR_*` constants, the SDK's
`ErrorCode`, and the `ErrorCode` and `FailureReason` enums of the draft
stream session envelope) moves in the same change;
`protocol/rust/tests/schema_enum_drift.rs`, `test/unit/error_test.cpp`, a
test in each Python package and, in builds with `TP_ENABLE_STREAMING_GRPC`,
`test/contract/stream_envelope_conformance_test.cpp` fail if one does not.
An appended value is safe
only while nothing sends it to a reader that predates it. The Rust readers
(agent, CLI, observability) decode these enums into closed types and reject
a value they do not know; the C++ adapter maps an unknown sidecar error code
to `internal`; the Python SDK reports an unknown code as
`ErrorCode.INTERNAL` (and an unknown health `last_error_code` as `None`).
Mixed versions are supported: the component packages only recommend one
another, and a CLI or SDK can run on another host against a device of a
different release. The change that first emits an appended value must
therefore keep it away from readers that may predate it, or negotiate first.
Retire this exception with the first.

A third narrow pre-1.0 exception covers the scheduler's metrics snapshot and
policy key. Under `0.1`, an optional, output-only property may be added to
`scheduler_metrics.json`, and a value may be appended to the scheduler policy
enum, which `config/schemas/scheduler.json`, `scheduler_metrics.json` and
`scheduler_event.json` each carry. An added property is never required and is
absent rather than `null` when a writer has no value for it. Existing
properties and values are never renamed, removed, retyped, reordered, made
required or given a new meaning; in particular `in_flight` keeps counting
logical in-flight requests (dispatched requests whose outcome is still open to
the caller) whatever counts are added beside it. The three copies of the
policy enum move in the same change, and `SchedulerMetrics` moves with
`scheduler_metrics.json`; `test/unit/scheduler_schema_test.cpp` fails if they
do not, or if a listed policy is neither registered nor refused with
`Error::Code::Unsupported`. Schema validators pinned to the previous files
remain strict, because these objects set `additionalProperties` to false and
the enums are closed, so an added property or value is safe only while nothing
sends it to a reader that predates it. Nothing does yet: no shipped component
writes a `scheduler_metrics.json` or `scheduler_event.json` payload, and the
one reader of the policy key, the serving worker, refuses a key that no
scheduler is registered under with `Error::Code::Unsupported` and does not
start; only an operator-edited worker config can carry the key. The exception
does not cover new properties of `config/schemas/scheduler.json`, which the
worker's config parser ignores rather than refuses. The change that first
writes these payloads, or first registers an appended policy, must keep them
away from readers that may predate it, or negotiate first. Retire this
exception with the first. Naming a key inside an open map, such as `gauges` in
`serving_metrics.json`, with the same constraint as the map's other values
accepts exactly the same payloads and needs no exception.

A fourth narrow pre-1.0 exception covers the Python/PyTorch sidecar IPC,
`python_pytorch_ipc.json`: under `0.1` it may gain message kinds and
optional fields that only negotiated peers exchange. The speech job and
session messages are the first. A sidecar lists the capabilities it
implements on its `ready_event`; the adapter enables `speech_jobs_v1` on
`load_model` only when that list names it, and neither peer sends a job or
session message on a connection whose load did not enable it. So no peer
receives them unless it asked for them. An adapter that predates the
capability reads the `ready_event` fields it uses by name and ignores the
rest. A sidecar that predates it lists nothing and answers a job message with
an `unsupported` `error_event` that carries the message's `message_id`, which
`backends/python_pytorch/tests/test_speech_jobs_messages.py` holds the
current runner to. The sidecar's `error` objects carry the `schema_version`
that `error.json` requires; sidecars released before 0.3.1 omit it, so a
reader takes a sidecar error's code and message by name. Existing kinds and
fields are never renamed, removed or given a new meaning. The schema, the
Rust mirror and the sidecar's `protocol.py` literals move in the same change;
`protocol/rust/tests/python_pytorch_ipc_speech_jobs.rs` holds them to the
golden frames and to the typed job seam's vectors
(`protocol/fixtures/job_seam.json`). Retire this exception with the first.

A fifth narrow pre-1.0 exception covers the telemetry events and the serving
worker's metrics. Under `0.1`, `log_event.json` and `metric_event.json` may
gain an optional property; a metric label key held to a registered value list
may be added, and a value appended to such a list; and a boundary may be added
to the worker's latency histograms. The `priority` property, the label keys
`row`, `mode`, `outcome` and `stage`, and the 300, 500, 600 and 1500 ms
boundaries are the first. An added property or key is never required and is
absent rather than `null`. Existing properties, keys and values are never
renamed, removed, retyped or given a new meaning, and the upper bound of an
existing boundary never moves. Every schema copy
of a list moves with its Rust constant, and the boundary list with
`kLatencyBucketsMs`; `protocol/rust/tests/telemetry_event_fixtures.rs` and
`test/unit/serving_health_metrics_test.cpp` fail if they do not. What a reader
that predates an addition does depends on the addition:

- The Rust mirrors of the two events ignore a property they do not know, so
  they decode an event that carries `priority`.
- The Rust mirror of the metric event, and the Rust metrics registry, reject
  an event whose labels carry a key they do not know. An event with one of
  the four new keys is therefore refused by a reader built before them.
- The same two readers, and the schema enums, hold a registered key to the
  values listed when they were built. An event that carries a value appended
  to a list later is refused by a reader built before the value.
- The schema files are closed objects, so a validator pinned to the previous
  files rejects an added property.
- A scraper that stored the worker's boundaries sees more `le` values. In the
  JSON body, where counts are per bucket, the bucket above an added boundary
  no longer counts observations at or below it; the Prometheus body is
  cumulative and its existing buckets keep their counts.

The boundaries take effect with the change that adds them. No component sets
`priority` or any of the four label keys yet, and nothing in this repository
reads another component's metric events. The change that first emits one of
those keys, or a value appended to a registered list, must keep it away from
readers that may predate it. Retire this exception with the first.

The agent's durable state file (`agent_state.json`) is the one document
with its own version track, because it is read by nothing but the agent
that wrote it and must stay safe across agent upgrades and downgrades: an
agent reads every state version it knows and refuses a newer one.
Its `schema_version` is a state version: `"0.1"` is the singleton layout
every agent through 0.2.x reads, and `"0.2"` adds the deployment generation
counter and the resident set. The agent decodes it with
`tensorplate_protocol::decode_agent_state`, which accepts exactly those
versions and rejects any other with the same typed unsupported-version
error; `decode_with_version_check` and every other payload stay at the
protocol version. The agent writes the oldest state version whose readers
decode the file without loss, so an agent that never allocates a generation
keeps writing `"0.1"`, and an older agent meeting a `"0.2"` file refuses it
rather than misreading it.

## Bindings

Bindings of the JSON contracts are **hand-written**, not code-generated.
The stream session envelope's C++ bindings are the one generated set.

| Component | Path | Status |
|-----------|------|--------|
| Rust serde mirror | `protocol/rust/src/<schema>.rs` | Authoritative reference binding. Round-trip tested via `protocol/rust/tests/round_trip.rs`. |
| C++ runtime value objects (Error, Result, ModelSpec, BufferRef, TensorView, InferRequest, InferResult) | `include/tensorplate/core/`, `include/tensorplate/buffer/` | Lands with the runtime types in V01-E02-F01..F06. JSON parsing for these objects lands when the HTTP server (V01-E07) imports a JSON parser. |
| C++ control-plane value objects (desired_state, worker_status, health_event, deploy_transaction) | `protocol/cpp/` | **Deferred to V01-E07/V01-E10** alongside the components that emit/consume them. The Rust mirror plus the committed JSON fixtures under `protocol/rust/tests/fixtures/` are the v0.1.0 cross-language contract. |
| C++ Python sidecar IPC binding | `runtime/src/adapters/python_pytorch/python_pytorch_session.cpp` | Reads the header fields it uses by name and ignores the rest. It sends and reads no job or session message yet. |
| Python sidecar IPC binding | `backends/python_pytorch/src/tensorplate_pytorch_backend/protocol.py` | Kind, status, error-code, capability and job literals. `codec.py` frames headers without interpreting them. |
| C++ stream session envelope binding (draft) | build tree only, target `tp::stream_proto` | Generated from `protocol/proto/tensorplate/stream/v1/session.proto` by the pinned `protoc` and gRPC plugin when `TP_ENABLE_STREAMING_GRPC` is ON; never committed. Only `test/contract/stream_envelope_conformance_test.cpp` links it. |

We chose hand-written bindings over code generation because:

- The schema set is small (single-digit count) and stable for v0.1.0.
- Hand-written bindings carry richer documentation and have explicit
  validation factories that match the C++ side line-by-line.
- A code generator would force a build-time tooling decision that
  affects every component; v0.1.0 keeps the toolchain minimal
  (cargo + cmake + vcpkg).

If bindings outgrow the hand-written pattern in v0.2+, the schema
files are stable enough that a generator can be added without
schema churn.

## Round-trip contract

`protocol/rust/tests/fixtures/` holds canonical JSON payloads. The
v0.1.0 Rust fixture contract is:

1. Each fixture parses cleanly via
   `decode_with_version_check::<T>` for its schema type.
2. The deserialized value re-serializes to JSON.
3. The re-serialized JSON parses back to a value structurally
   equal to the first.

The C++ side picks up the same fixtures in V01-E07 / V01-E05 once
JSON parsing is wired in. Until then, this PR does not claim the
V01-E02-F07-T07 Rust/C++ round-trip acceptance criterion as complete;
it establishes the fixture set and validates the Rust binding against
the same semantic rules used by the C++ value-object factories.

## Adding a new payload

1. Create `protocol/schemas/<name>.json` with `schema_version`
   `const="0.1"` and `additionalProperties: false`.
2. Add `protocol/rust/src/<name>.rs` with serde-derived structs and
   a validating `<T>::new` factory that enforces the schema's
   semantic constraints (rules that JSON Schema cannot express).
3. Re-export the new types from `protocol/rust/src/lib.rs`.
4. If the payload crosses the C++ runtime boundary, hand-write a
   C++ mirror under `include/tensorplate/...` or `protocol/cpp/`
   and pair the validation factory.
5. Add a fixture under `protocol/rust/tests/fixtures/` plus a
   `round_trip` entry in `protocol/rust/tests/round_trip.rs`. Vectors
   that more than one language replays, such as the job seam's or
   canonical JSON's, go under `protocol/fixtures/` instead, with the test
   that replays them in each language.
6. Update `protocol/schemas/README.md` and this document if the
   policy changes.

## Memory observations

`memory_observation.json` and its Rust mirror add a sample record inside
protocol 0.1 without changing existing envelopes or version constants. It is
currently consumed only by the platform crate and its tests; no C++ worker,
Python sidecar or SDK receives the record. Its field and availability rules
are in [memory observations](memory-observation.md).

## Operator provisioning manifest

`provisioning_manifest.json` is consumed only by the Rust operator CLI, not
exchanged over IPC. Its Rust binding validates the same file-path, digest and
fetch-source grammar. Under schema 0.1, optional `url` and `source_path` fields
extend local-directory provisioning with operator-requested fetch; they cannot
coexist or be null. An older local-only CLI rejects these fields, so it fails
closed on a new fetch manifest. Existing local manifests retain their behavior.
The CLI retrieves before deployment; the agent and sidecar keep their separate
bundle and artifact-set verification. See [bundle provisioning](../cli/bundle.md)
for cache isolation, atomic publication and retry semantics.
