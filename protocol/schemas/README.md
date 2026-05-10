# TensorPlate Protocol Schemas

Source-of-truth language-neutral contracts consumed by `tensorplate-agent`
(Rust), `tensorplate-serving` (C++), `tensorplate-cli` (Rust),
`tensorplate-observability` (Rust), and the Python/PyTorch sidecar backend.

Schemas in this directory are the authoritative shape; bindings under
`protocol/rust/` and `include/tensorplate/` mirror them. Do **not**
duplicate field definitions in language-specific source without updating
the schema first.

## Format

v0.1.0 uses **JSON Schema Draft 7** for all control-plane and
configuration payloads. Binary tensor payloads in the Python/PyTorch
sidecar IPC ride as raw bytes following a JSON header; see
`python_pytorch_ipc.json`.

JSON field names are **snake_case** to match the Python ecosystem and
keep parity with the C++/Rust wire format.

## Versioning

Every payload carries a top-level `schema_version` string of the form
`"MAJOR.MINOR"`, value-fixed (`const`) to the protocol version
(`"0.1"` for v0.1.0). Bumping the protocol version requires touching:

- `CMakeLists.txt` (`TP_PROTOCOL_VERSION_MAJOR/MINOR`)
- `protocol/rust/src/lib.rs` (`PROTOCOL_VERSION_MAJOR/MINOR`,
  `SCHEMA_VERSION`)
- All schemas under `protocol/schemas/`
- `docs/architecture/versioning.md`
- `CHANGELOG.md`

Decoders **must** reject unknown `schema_version` values with the typed
error `Error::Code::Unsupported` (C++) /
`tensorplate_protocol::ErrorCode::Unsupported` (Rust). The Rust helper
`tensorplate_protocol::decode_with_version_check` provides this; the C++
binding will follow the same shape when JSON parsing lands.

## Bindings

Bindings are **hand-written** in v0.1.0:

| Binding | Path |
|---------|------|
| Rust    | `protocol/rust/src/<name>.rs` |
| C++     | `include/tensorplate/core/*.hpp` and `include/tensorplate/buffer/*.hpp` (value objects) |

The Rust crate is the authoritative serde mirror. Each root payload
implements `ValidatePayload` so `decode_with_version_check` rejects
current-version data that violates constructor-level invariants. C++
value objects mirror the same fields and use stable string mappings
declared in runtime translation units; C++ JSON round trips start once
the V01-E07 / V01-E05 bindings land.

## Adding a new payload

1. Add `protocol/schemas/<name>.json` with `schema_version`
   `const="0.1"` and `additionalProperties: false`.
2. Add `protocol/rust/src/<name>.rs` with serde-derived structs and a
   `ValidatePayload` implementation for semantic validation.
3. Add a hand-written C++ mirror if the payload crosses the C++
   runtime boundary.
4. Add a round-trip test under `protocol/rust/tests/` or
   `test/integration/`.
5. Update `protocol/rust/src/lib.rs` to re-export the new module.

## Schemas in this directory

| File                       | Owner Feature   |
|----------------------------|-----------------|
| `error.json`               | V01-E02-F01     |
| `model_spec.json`          | V01-E02-F02     |
| `infer_request.json`       | V01-E02-F03     |
| `infer_result.json`        | V01-E02-F04     |
| `buffer_ref.json`          | V01-E02-F05     |
| `tensor_view.json`         | V01-E02-F06     |
| `desired_state.json`       | V01-E02-F07     |
| `worker_status.json`       | V01-E02-F07     |
| `health_event.json`        | V01-E02-F07     |
| `deploy_transaction.json`  | V01-E02-F07     |
| `python_pytorch_ipc.json`  | V01-E02-F07     |
| `bundle_manifest.json`     | V01-E13 (later) |
