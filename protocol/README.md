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
  speech-to-text and one text-to-speech. One holds the remaining bodies and
  four ends with a cause. One holds frames of `extension.proto`, a
  fixture-only schema with a field, a body and a mode stream/v1 does not
  have. Every value in them is synthetic. After a schema change, record the
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
  not all show; holds the two sessions to the ordering and unit rules the
  schema states; pins the service, the envelope numbering, the reserved body
  range and the reserved names; and reads the `extension.proto` frames to
  show what a receiver sees of an addition it does not know. A few of its
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
- A receiver ignores a field it does not know, so a field may be added.
- A receiver refuses an event whose body it does not know and an `Open`
  whose mode it does not know. Numbers 40 to 99 of both envelopes are
  reserved for the bodies of modes not defined yet.

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

What may still change before the freeze: field numbers and names; the shape
of input credit and of `Status`; the shape of `EndCause`, a string reason
beside the `FailureReason` mirror; whether server events carry the session
id; whether a transcript segment carries text; the format of identifiers;
the set of limits `Ready` reports; how a client learns that input credit
has returned; which answer belongs to which `Finalize`; and how a receiver
treats an enum value it does not know.
