# `protocol/`

Language-neutral schemas and generated/hand-written bindings shared between
C++ and Rust components.

## Layout

- `schemas/` — schema source-of-truth for the JSON contracts (JSON Schema).
- `proto/` — protobuf source-of-truth for the external stream session
  envelope, a draft (see [Streaming session envelope](#streaming-session-envelope)).
- `rust/` — `tensorplate-protocol` Rust crate consuming the schemas.
- `fixtures/` — language-neutral test vectors that every implementation of
  the objects they describe replays; not wire schemas.
- C++ bindings live in [`include/tensorplate/`](../include/tensorplate/) and
  [`runtime/`](../runtime/) and are generated from or kept in sync with
  `schemas/`. The stream session envelope's C++ binding is the exception:
  it is generated from `proto/` into the build tree.

## Ownership

- **Layer:** cross-cutting (data plane <-> management plane contract)
- **Owner:** runtime tech lead with Rust agent reviewer

## Rules

- Schemas are versioned. Unknown schema versions are rejected with typed
  errors per V01-E02.
- Schema breaking changes require a `CHANGELOG.md` entry and bump of
  the protocol or schema version per V01-E01-F06.
- Protocol IDLs are the source of truth. Hand-written bindings are
  acceptable in v0.1.0 but must be documented against the schema.

## Streaming session envelope

`proto/tensorplate/stream/v1/session.proto` is a **draft** of the messages a
client and the serving worker will exchange on one bidirectional gRPC call
per logical session, and the source of truth for them. Nothing uses it yet:
no listener, server or client is built from it, and it is not a
compatibility promise until it is frozen. The JSON contracts under
`schemas/` are unchanged by it.

- **Bindings.** A build with `TP_ENABLE_STREAMING_GRPC=ON` generates the C++
  messages and the gRPC service into the build tree, with the `protoc` and
  gRPC plugin of the pinned vcpkg baseline (target `tp::stream_proto`).
  Generated files are never committed. A build without the feature
  generates nothing and gains no target.
- **Golden frames.** `test/contract/fixtures/stream/v1/` holds recorded
  frames as text, one per line: a name, the message type, the bytes `protoc`
  emitted in lowercase hex, and the message in protobuf text format. Two
  files are whole sessions in the order their events are sent, one
  speech-to-text and one text-to-speech. One holds the remaining bodies,
  five ends with a cause, an `Open` with a selector and three refusals. One
  holds frames of `extension.proto`, a fixture-only schema with fields, a
  body, a mode and a capability stream/v1 does not have. Every value in
  them is synthetic. After a schema change, record the
  frames again with the pinned `protoc` and commit the result; `record.sh`
  also writes that `protoc`'s version line to `versions.txt`:

  ```bash
  test/contract/fixtures/stream/v1/record.sh \
    build/vcpkg_installed/x64-linux/tools/protobuf/protoc
  ```

  `record.sh --check <protoc>` changes nothing and fails, naming the frame,
  when a hex column is not what `protoc` encodes from its text column, and
  naming `versions.txt` when that file names another `protoc`. The T3 test
  `contract.stream_frames_recorded` runs it with the build's `protoc`.
- **Conformance.** `test/contract/stream_envelope_conformance_test.cpp` (T3,
  feature-ON builds) decodes every stream/v1 frame with the generated types
  and encodes it again byte for byte; requires a frame for every field,
  every body and every non-zero value of the enums defined only here; lists
  every field with its type, number and label, which the bytes of a frame do
  not all show; holds every sequence, generation and 64-bit id to its bound,
  every end cause to a set reason with the taxonomy's code, and every
  refusal to a reason the schema lists as preceding admission; holds the
  two sessions to the ordering, credit, limit, total and unit rules the
  schema states; pins the service, the envelope numbering, the reserved
  body numbers and the reserved names; and reads the `extension.proto`
  frames to show what a receiver sees of an addition it does not know. A
  frame of `extension.proto` counts toward field coverage as stream/v1
  reads it, which is what covers the two capability lists while no
  capability is defined. A few of its
  checks are properties of the recordings and not rules of the schema, and
  a comment says so at each; for example, a session keeps the credit limits
  its `Ready` reported, each spoken utterance's transcript ends at its
  endpoint, and each end with a cause carries the code and outcome the
  session layer gives that reason.

Once frozen, the envelope evolves by these rules:

- The package name is the API major: `tensorplate.stream.v1`. An
  incompatible change is a new package. The envelope carries no
  `schema_version` and is outside `PROTOCOL_VERSION`.
- A number is never reused. A deleted field's number stays reserved.
- A receiver ignores a field it does not know, so a field may be added, and
  reads an enum value it does not know as that enum's unspecified value, so
  a value may be appended.
- A client ignores a server event whose body it does not know. A server
  sends an event that belongs to a capability only when the `Open` declared
  that capability, so an event added later never reaches a client that
  would drop it unseen.
- A server refuses a client event whose body it does not know and an `Open`
  whose mode it does not know. A client sends an event that belongs to a
  capability only when `Ready` lists it. Numbers 40 to 99 of both envelopes
  are reserved for the bodies of modes not defined yet.

Three of its enums mirror definitions that live elsewhere: the same names,
upper-cased behind a prefix, in the same order, numbered from 1 so that 0
stays unspecified. `ErrorCode` mirrors the `code` enum of
`schemas/error.json`, `FailureReason` the `reason` enum of
`schemas/failure_reason.json`, and `SessionState` the states of
`tensorplate::serving::LogicalSessionState`
([`session.hpp`](../include/tensorplate/serving/session.hpp)). When it
runs, the conformance test reads the two JSON schemas and asks the runtime
for the name of each state, and compares names, numbers and counts, so a
value added to a schema's enum or to a mirror alone fails it. A state added
to the header stops the test compiling until the test lists it, and then
fails it until `SessionState` has it. C++ `Error::Code` is not read here:
`test/unit/error_test.cpp` holds it to `error.json`.

What the draft now fixes:

- **Credit.** `Accepted` counts input only and is cumulative. The server
  sends one whenever input credit returns; a client never infers credit
  from time or from other events.
- **Answers.** An utterance's `EndpointDetected` precedes its
  `FinalTranscript`, and no partial follows the endpoint. A
  `FinalTranscript` or `SynthesisCompleted` names the sequence of the
  `Finalize` it answers. An utterance's text is its segments' text, sent
  once. Closing the sending half finalizes the open utterance, with an
  endpoint reason of its own.
- **Limits.** `Ready` reports the deployment's bounds for the session's
  mode beside the timers, so a client needs no descriptor to know what it
  may send.
- **Capabilities.** `Open` declares the client's capabilities and `Ready`
  returns those in force. None is defined yet.
- **End causes.** A cause is a `FailureReason`, always set, with the code
  the taxonomy pairs with it; the string beside it is detail for logs. The
  reasons a session ends with, or an `Open` is refused with, are values of
  `schemas/failure_reason.json`.
- **Refusal and admission.** A refused `Open` ends the call with a status
  whose detail is an `OpenRefused`, and every terminal outcome states
  whether a session slot was ever reserved. The schema lists the reasons
  that precede admission.
- **Target.** `Open` names a resolved target (deployment id, generation,
  descriptor digest) or a model selector, and is the only client event that
  names a generation. The server refuses a selector as unresolved, and a
  digest that is not the served descriptor's. The request metadata names
  `tensorplate-model` and `tensorplate-model-version` are reserved for the
  selector; the server does not read them.
- **Bounds.** Sequences, generations and 64-bit ids stop at 2^53 - 1. A client value
  that disagrees with what earlier events determine fails the session.
- **Totals and recovery.** `SessionClosed` carries the session's totals.
  Input that was accepted but not answered when a session ended was not
  processed, and nothing replays it.
- **Reserved.** A cancel scoped to text segments has a body number in each
  envelope and a capability number, and no behaviour.

What may still change before the freeze: field numbers and names; how the
status of a refused call carries its `OpenRefused`, which is decided with
the server; the fields of the selector; whether the cancel scoped to text
segments is defined; the shape of input credit and of `SessionStatus`;
whether server events carry the session id; and the format of identifiers.
