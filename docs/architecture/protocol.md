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
| Bundle manifest (V01-E13) | JSON Schema Draft 7 | Same audience as config. |
| Cross-component control payloads (desired_state, worker_status, health_event, deploy_transaction) | JSON Schema Draft 7 | Crosses Rust/C++ language boundaries; JSON keeps the schema human-readable; the volume is low (status / event ticks, not request hot-path). |
| HTTP `/infer` payload | JSON Schema Draft 7 (header) + raw bytes | The header documented in `infer_request.json` / `infer_result.json` rides as JSON; tensor payloads ride as raw bytes per `BufferRef` / `TensorView` metadata. v0.1.0 does not negotiate an alternative encoding; V01-E07 lands the HTTP server. |
| Python/PyTorch sidecar IPC | JSON header + raw tensor bytes | Schema captured in `python_pytorch_ipc.json`. Wire format: `[4-byte big-endian header_length][JSON header][raw tensor payload bytes]`. JSON-encoding tensors was an explicit non-goal (V01-E05). |

We deliberately do **not** introduce protobuf in v0.1.0. The v0.1.0 hot
path runs in-process within `tensorplate-serving`; cross-process
payloads are control-plane (low volume) plus the sidecar IPC where
the tensor payload is raw bytes regardless of the header encoding.
Adding a binary header format earns no measurable throughput on the
v0.1.0 critical path and would split the operator-facing config and
the IPC headers across two encodings.

## Versioning

Every payload carries a `schema_version` string of the form
`"MAJOR.MINOR"`, value-fixed (`const`) to the protocol version.
v0.1.0 is `"0.1"`.

Decoders **must** call
`tensorplate_protocol::decode_with_version_check` (Rust) — or its
forthcoming C++ equivalent — instead of `serde_json::from_str` /
`nlohmann::json::parse` directly. The helper rejects unknown
versions with a typed error so that the runtime maps them to
`Error::Code::Unsupported` and surfaces a stable error code to the
operator. Bypassing the helper loses the guarantee.

Bumping the protocol version requires touching:

- `CMakeLists.txt` (`TP_PROTOCOL_VERSION_MAJOR/MINOR`)
- `protocol/rust/src/lib.rs` (`PROTOCOL_VERSION_MAJOR/MINOR`,
  `SCHEMA_VERSION`)
- All schemas under `protocol/schemas/`
- `docs/architecture/versioning.md`
- `CHANGELOG.md`

Within a major version, additive (backwards-compatible) field
additions are minor bumps. Renames, removals, and meaning changes
are major bumps and require migration tooling.

## Bindings

Bindings are **hand-written**, not code-generated, in v0.1.0.

| Component | Path | Status |
|-----------|------|--------|
| Rust serde mirror | `protocol/rust/src/<schema>.rs` | Authoritative reference binding. Round-trip tested via `protocol/rust/tests/round_trip.rs`. |
| C++ runtime value objects (Error, Result, ModelSpec, BufferRef, TensorView, InferRequest, InferResult) | `include/tensorplate/core/`, `include/tensorplate/buffer/` | Lands with the runtime types in V01-E02-F01..F06. JSON parsing for these objects lands when the HTTP server (V01-E07) imports a JSON parser. |
| C++ control-plane value objects (desired_state, worker_status, health_event, deploy_transaction) | `protocol/cpp/` | **Deferred to V01-E07/V01-E10** alongside the components that emit/consume them. The Rust mirror plus the committed JSON fixtures under `protocol/rust/tests/fixtures/` are the v0.1.0 cross-language contract. |
| C++ Python sidecar IPC binding | `protocol/cpp/python_pytorch_ipc.hpp` | **Deferred to V01-E05** alongside the sidecar adapter. |
| Python sidecar IPC binding | `backends/python_pytorch/src/tensorplate_pytorch_backend/protocol.py` | **Deferred to V01-E05** alongside the sidecar adapter. |

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
   `round_trip` entry in `protocol/rust/tests/round_trip.rs`.
6. Update `protocol/schemas/README.md` and this document if the
   policy changes.
