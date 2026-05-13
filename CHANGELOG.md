# Changelog

All notable changes to TensorPlate will be documented in this file.

This project follows the spirit of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic versioning once public releases begin.

## [Unreleased]

### Added

- Developer-facing C++ example `tensorplate-example-buffer-plane` under
  `examples/buffer_plane/` that walks the V01-E03 buffer plane end to
  end: ingress copy → `BufferRef` + `TensorView` → `InferRequest` →
  mock policy → `InferResult` → cancellation cleanup → double-release
  diagnosis → pressure-event draining. Toggle with the
  `TP_BUILD_EXAMPLES` CMake option (defaults ON).
- T2 integration test `tp_test_integration` exercising the same loop
  under GoogleTest (`test/integration/buffer_plane_e2e_test.cpp`).
- Memory-pressure event shape and emission: `MemoryPressure` level
  (normal / warning / critical), `BufferPressureEvent` payload (pool
  name, previous + current level, capacity, in-use bytes, active count,
  high-water mark, allocation failures), and a bounded event ring drained
  through `BufferManager::drain_pressure_events`. The buffer manager
  records one event per threshold crossing without invoking callbacks or
  I/O on allocation/release paths. Mirrored on the wire in
  `protocol/schemas/buffer_pressure_event.json` and in the Rust
  `tensorplate-protocol` crate (V01-E03-F06).
- Session output helpers `allocate_output_buffer`, `build_named_output`,
  and `build_named_outputs`. The execution session allocates one owned
  buffer per output, pairs it with a validated `TensorView` byte window,
  and assembles `NamedOutput` value objects (including chunk-shaped VLA
  action outputs). Multi-output builds reject duplicate names and
  release any partial allocations on later failure (V01-E03-F05).
- Ingress copy helpers `copy_payload_into_buffer` and
  `build_named_inputs` that turn caller-owned byte payloads into
  buffer-plane-owned `BufferRef` storage with a single copy. Multi-input
  builds reject duplicate names, oversized payloads, and tensor-window
  metadata that does not fit the allocated buffer; partial allocations
  are released before an error is returned. Shared vision and
  SmolVLA-style payload fixtures live in
  `test/mocks/ingress_fixtures.hpp` and will be reused by the V01-E07
  HTTP router (V01-E03-F04).
- Buffer cleanup helpers `release_request_buffers`,
  `release_partial_outputs`, and the `RequestBufferGuard` RAII wrapper.
  Helpers release every unique buffer id at most once, never throw, avoid
  allocation on the successful cleanup path, preserve original request
  errors, and report release failures through a `CleanupReport`. Used by
  scheduler cancellation, deadline expiry, and execution-session error
  paths (V01-E03-F03).
- `BufferManager` v0.1.0 CPU buffer plane: capacity-bounded allocator with
  monotonic ids, aligned heap-backed storage, validated configuration,
  thread-safe allocate/release/data/view access, accounting snapshot
  (in-use bytes, active count, high-water mark, allocation/release failure
  counters), and derived `MemoryPressure` level. Storage is freed exactly
  once; double-release and stale-handle release return typed
  `Error::Code::Internal` (V01-E03-F01, V01-E03-F02).
- `docs/architecture/buffer-plane.md` describing the buffer-plane
  ownership model, copy/move semantics, cleanup-path contracts, and the
  scope boundaries that V01-E03 deliberately respects (V01-E03-F02).
- Top-level package skeleton for v0.1.0: `include/tensorplate/`, `runtime/`,
  `serving_worker/`, `agent/`, `cli/`, `observability/`, `protocol/schemas/`,
  `protocol/rust/`, `config/schemas/`, `test/`, `cmake/`, and
  `docs/architecture/` (V01-E01-F01).
- `docs/architecture/ownership.md` documenting per-package owners, allowed
  dependencies, and forbidden upward dependencies (V01-E01-F01-T02).
- Test tree layout for tiers T1 through T5 plus shared mocks and model
  fixtures, documented in `test/README.md` (V01-E01-F01-T03).
- Root CMake build with `tp_runtime` (alias `tp::runtime`) static library,
  `tp_serving_worker` binary (output `tensorplate-serving`), and CTest
  wiring with T1 label (V01-E01-F02-T01).
- vcpkg manifest (`vcpkg.json`) declaring the GoogleTest dependency and
  reserving feature flags for adapter SDKs; toolchain stubs
  `cmake/toolchains/x86_64-linux-gnu.cmake` and
  `cmake/toolchains/aarch64-jetson.cmake` (V01-E01-F02-T02).
- `cmake/features/warnings.cmake` and `cmake/features/sanitizers.cmake`
  helpers; `TP_ENABLE_SANITIZERS` and `TP_WARNINGS_AS_ERRORS` options;
  `tp_test_unit` GoogleTest target with smoke coverage (V01-E01-F02-T03).
- `.clang-format` and `.clang-tidy` baseline configurations.
- Cargo workspace at the repository root with members `tensorplate-agent`,
  `tensorplate-cli`, `tensorplate-observability`, and `tensorplate-protocol`,
  pinned `rust-toolchain.toml` (1.78.0), `rustfmt.toml` baseline, and
  workspace-wide rustc and clippy lints (V01-E01-F03-T01).
- Crate entrypoints with version banners and a baseline test in
  `tensorplate-protocol` proving workspace builds end to end without
  device hardware (V01-E01-F03-T02).
- Documented Rust quality commands (`cargo build`, `cargo test`,
  `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`) in `CONTRIBUTING.md` (V01-E01-F03-T03).
- `.github/workflows/cpp.yml` running the C++ build, T1 unit tests in a
  release and ASAN/UBSAN matrix, `clang-format --dry-run -Werror`, and
  `clang-tidy` against the exported compile commands (V01-E01-F04-T01).
- `.github/workflows/rust.yml` running `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo test --workspace` against the pinned toolchain (V01-E01-F04-T02).
- vcpkg and Cargo dependency caching, per-workflow concurrency, and a
  documented PR / nightly / release-branch status policy in
  `CONTRIBUTING.md` (V01-E01-F04-T03).
- `.devcontainer/Dockerfile` and `.devcontainer/devcontainer.json`
  delivering a reproducible Ubuntu 22.04 dev image with CMake, Ninja,
  Clang 15, clang-format/-tidy, vcpkg, and the pinned Rust toolchain;
  named volumes mount the vcpkg and cargo caches across rebuilds
  (V01-E01-F05-T01).
- `docs/contributing/jetson-cross-compile.md` documenting the supported
  cross-compile path: `cmake/toolchains/aarch64-jetson.cmake`,
  TP_JETSON_SYSROOT/CC/CXX inputs, and JetPack/TensorRT/CUDA system
  ownership (V01-E01-F05-T02).
- `docs/contributing/local-validation.md` enumerating the canonical
  CMake/CTest/clang-format/clang-tidy and Cargo commands that mirror CI
  (V01-E01-F05-T03).
- `include/tensorplate/version.hpp` (generated from `.hpp.in` by CMake's
  `configure_file`) exposing four independent version surfaces:
  `kRuntimeVersion`, `kProtocolVersion`, `kBundleFormatVersion`, and the
  per-component MAJOR/MINOR/PATCH constants (V01-E01-F06-T01).
- `tensorplate_protocol::PROTOCOL_VERSION_*` and
  `BUNDLE_FORMAT_VERSION_*` Rust constants mirroring the C++ surface
  (V01-E01-F06-T01).
- T1 unit tests (C++ `version_test.cpp`, Rust `tests` module in
  `protocol/rust`) verifying that composed version strings agree with
  their components on each side (V01-E01-F06-T01).
- `docs/architecture/versioning.md` documenting the runtime / protocol /
  schema / bundle-format surfaces, bump rules, and the planned
  compatibility-validation path (V01-E01-F06-T02).
- `CONTRIBUTING.md` "Release and Changelog Policy" section listing the
  changes that require a `CHANGELOG.md` entry plus a version bump
  (V01-E01-F06-T03).
- `backends/python_pytorch/` package skeleton for the out-of-process
  PyTorch backend per the V01-E01 scope expansion: PEP 621
  `pyproject.toml`, namespace package
  `tensorplate_pytorch_backend` with mirrored protocol/bundle-format
  version constants, `py.typed` marker, ruff/ruff-format/mypy/pytest
  configuration, and a smoke test suite (V01-E01-F01).
- `.github/workflows/python.yml` running `ruff check`,
  `ruff format --check`, `mypy src tests`, and `pytest -q` against
  Python 3.10 and 3.12 on Ubuntu 22.04 with pip caching.
- `docs/architecture/ownership.md` updated with the new package row,
  out-of-process IPC dependency arrow, and forbidden-dependency rule
  preventing the Python backend from linking against any C++ runtime
  module.
- `include/tensorplate/core/error.hpp` defining the `tensorplate::Error`
  value object and the stable `Error::Code` taxonomy
  (`ConfigInvalid`, `LoadFailed`, `NotReady`, `ShapeMismatch`,
  `Unsupported`, `OOMError`, `Timeout`, `InferenceFailed`, `Internal`)
  with snake_case `to_string` / `error_code_from_string` helpers
  (V01-E02-F01-T01).
- `include/tensorplate/core/result.hpp` providing
  `tensorplate::Result<T>` (and `Result<void>`) with std::expected-shaped
  semantics and a `tp` namespace alias for the planning-doc API surface
  (V01-E02-F01-T01).
- `protocol/schemas/error.json` (JSON Schema Draft 7) and Rust mirror
  `tensorplate_protocol::ProtocolError` / `ErrorCode`, plus
  `decode_with_version_check` and `DecodeError` enforcing typed
  rejection of unknown `schema_version` values (V01-E02-F01-T02).
- T1 unit tests for `Error`, `Result<T>`, the protocol round-trip, and
  unknown-schema-version rejection (V01-E02-F01-T03).
- `include/tensorplate/core/model_spec.hpp` defining the
  `tensorplate::ModelSpec` value object with `ModelClass`
  (`vision`, `speech`, `language`, `vla`, `embedding`, `custom`) and
  `PrecisionHint` (`auto`, `fp32`, `fp16`, `bfloat16`, `int8`, `int4`)
  taxonomies and a validating `create()` factory returning
  `Result<ModelSpec>` (V01-E02-F02-T01).
- `protocol/schemas/model_spec.json` and the Rust mirror
  `tensorplate_protocol::ModelSpec` with serde round-trip and
  `decode_with_version_check` support (V01-E02-F02-T02).
- T1 unit tests for `ModelSpec` validation (empty model_id,
  artifact_path, backend_hint, present-but-empty profile_id), enum
  string round-trip, equality, and Rust round-trip
  (V01-E02-F02-T03).
- `include/tensorplate/buffer/buffer_ref.hpp` defining the
  `tensorplate::BufferRef` opaque buffer-handle value object with the
  `BufferOwnership` (`Owned` / `Borrowed` / `Released`) state machine,
  documented copy/move contract, `kNullId` released sentinel, and
  `mark_released()` idempotent tombstone; the underlying allocator
  lands in V01-E03 (V01-E02-F05-T01).
- Documented copy/move/release semantics in the public header and
  through T1 unit tests, including the convention that holders needing
  unique-ptr-style invalidation must call `mark_released()` on the
  source explicitly (V01-E02-F05-T02).
- `protocol/schemas/buffer_ref.json` and Rust mirror
  `tensorplate_protocol::BufferRef` for protocol/test fixtures that
  compare buffer identity without transferring memory
  (V01-E02-F05-T03).
- `include/tensorplate/buffer/tensor_view.hpp` defining
  `tensorplate::TensorView` with `DType`
  (`float32`, `float16`, `bfloat16`, `int64`, `int32`, `int16`,
  `int8`, `uint8`, `bool`) and `Layout` (`row_major`, `col_major`)
  enums, locked dtype byte-width table, and a validating `create()`
  factory that auto-computes `byte_size` and rejects rank-0 / non-
  positive dims / size underflow / size-overflow with typed errors
  (V01-E02-F06-T01, T02).
- `protocol/schemas/tensor_view.json` and Rust mirror
  `tensorplate_protocol::TensorView` with serde round-trip,
  defaults compression for layout / byte_offset / byte_size, and
  matching `TensorViewError` taxonomy (V01-E02-F06-T03).
- T1 unit tests for dtype/layout name round-trip, locked byte-width
  table, valid construction, automatic byte_size, padding-allowed
  explicit byte_size, underflow rejection, empty/zero/negative shape
  rejection, SmolVLA-style chunk shape `[chunk_size, action_dim]`,
  byte_offset preservation, and equality.
- `include/tensorplate/core/infer_request.hpp` defining the
  `tensorplate::InferRequest` value object with a vector of named
  inputs, request metadata, and an optional monotonic
  `std::chrono::steady_clock::time_point` deadline. `NamedInput`
  binds a stable name to a `BufferRef` and a `TensorView`; the
  request supports single-input vision (n=1) and SmolVLA-class
  multi-input (image_front, image_wrist, state, instruction)
  through the same type. Validating `create()` and
  `create_with_relative_deadline()` factories return
  `Result<InferRequest>` (V01-E02-F03-T01).
- `tensorplate::RequestMetadata` carries explicit
  `correlation_id`, `action_chunk_id`, `action_chunk_sequence`, and
  `stale_after_sequence` fields preserving the LeRobot
  PolicyServer async-inference contract, plus a free-form
  string/string `extra` map for caller metadata
  (V01-E02-F03-T02).
- `protocol/schemas/infer_request.json` (JSON Schema Draft 7) with
  `$ref` references to `buffer_ref.json` and `tensor_view.json`,
  optional `metadata`, and a relative `deadline_ms` field that
  receivers convert to a monotonic absolute deadline by sampling
  their own steady clock.
- Rust mirror `tensorplate_protocol::InferRequest` with
  `RequestMetadata`, `NamedInput`, `InferRequestError`, and the
  same validation rules as the C++ factory.
- T1 unit tests for single-input and SmolVLA-style multi-input
  construction, LeRobot async metadata preservation, validation
  rejection (empty request_id / endpoint / inputs / input name and
  duplicate input names), no-deadline / future-deadline / past-
  deadline / clamped-to-zero behavior, the relative-deadline
  factory's negative-value rejection and monotonic conversion,
  equality, and the requirement that fixtures build without a
  buffer-pool or adapter (V01-E02-F03-T03).
- `include/tensorplate/core/infer_result.hpp` defining
  `tensorplate::InferResult` as a discriminated value carrying
  either a non-empty vector of `NamedOutput`s or a typed
  `tensorplate::Error`, plus optional `InferenceTiming`
  breakdowns (queue / execution / total latency in nanoseconds)
  populated by the V01-E04 ExecutionSession NVI wrapper. Chunk-
  shaped VLA action output is one pattern of `outputs` and does
  not require a VLA-specific result type. Success construction
  validates output naming the same way `InferRequest` validates
  inputs (V01-E02-F04-T01).
- `protocol/schemas/infer_result.json` (JSON Schema Draft 7) with
  $ref-composed `error.json` / `buffer_ref.json` / `tensor_view.json`
  fragments and an `allOf` constraint that enforces the
  status / outputs / error invariant on the wire. Rust mirror
  `tensorplate_protocol::InferResult` with `InferResultStatus`,
  `NamedOutput`, `InferenceTiming`, and `InferResultError`
  taxonomy (V01-E02-F04-T02).
- T1 unit tests covering success construction with chunk-shaped
  output, multi-named-output ordering, validation rejection
  (empty / duplicate / empty-name outputs), failure construction
  preserving the typed error code, ingress-time empty-request_id
  failures, safe-default accessors on wrong-state lookups,
  optional timing field preservation, equality, and explicit
  compatibility of every `Error::Code` with the result taxonomy
  (V01-E02-F04-T03).
- `docs/architecture/protocol.md` documenting the v0.1.0 protocol
  format selection (JSON Schema Draft 7), versioning policy
  (`schema_version` const-fixed, mandatory
  `decode_with_version_check`), hand-written-binding strategy, and
  the round-trip contract between Rust serde mirrors and the
  shared fixtures (V01-E02-F07-T01).
- `protocol/schemas/desired_state.json` (V01-E02-F07-T02),
  `protocol/schemas/worker_status.json` carrying the V01-E10 ROS 2
  health-publisher fields (`agent_state`, `serving_state`,
  `observability_state`, `active_deployment`, `backend`,
  `missed_heartbeat_count`, `missed_deadline_rate`, `queue_depth`,
  `last_error_code`) (V01-E02-F07-T03),
  `protocol/schemas/health_event.json` with the V01-E12-reserved
  control-loop telemetry block (jitter p50/p95/p99/max, mean
  frequency, frequency stddev, frequency-error percent, rolling
  window) and monotonic-only timestamps (V01-E02-F07-T04),
  `protocol/schemas/deploy_transaction.json` covering the
  received -> verified -> staged -> capacity_checked -> prepared ->
  warmed -> promoted -> active state machine plus terminal
  failed / rolled_back states with typed-and-recoverable failure
  metadata (V01-E02-F07-T05), and
  `protocol/schemas/python_pytorch_ipc.json` defining the JSON
  header for the Unix domain socket IPC (LoadModel / Prime /
  Infer / InferAsync / Cancel / Unload / HealthCheck plus
  ready / error / metric events) with raw tensor bytes carried
  after the header rather than JSON-encoded (V01-E02-F07-T06).
- Rust mirrors `tensorplate_protocol::DesiredState`,
  `WorkerStatus`, `HealthEvent`, `ControlLoopMetrics`,
  `DeployTransaction` / `DeployFailure` / `DeployState`, and
  `IpcMessage` with serde round-trip, validating constructors
  (e.g. `DesiredState::new` rejects malformed bundle digests,
  `WorkerStatus::new` rejects out-of-range
  `missed_deadline_rate`, `DeployTransaction::new` enforces the
  failure-metadata invariant, `IpcMessage::validate` enforces the
  JSON Schema `allOf` rules), and `decode_with_version_check`
  semantic validation plus acceptance/rejection tests.
- `protocol/rust/tests/round_trip.rs` integration suite with nine
  canonical JSON fixtures (vision and SmolVLA desired-state,
  ready / degraded worker-status, missed-deadline health event
  with full control-loop metrics, active and failed deploy
  transactions, sidecar load-model header, and a
  schema-version-rejection negative fixture) covering the Rust side
  of the fixture contract. C++ / Python binding round trips remain
  deferred until those bindings land in V01-E07 / V01-E05.
- `protocol/schemas/README.md` updated to reflect the realized
  schema set and conventions; per-schema ownership table.

### Changed

- `README.md` repository layout block now reflects the realized v0.1.0
  package skeleton and links to the ownership document.
- `tensorplate_protocol::decode_with_version_check` now rejects
  current-version payloads that deserialize structurally but violate
  constructor-level invariants, returning `DecodeError::InvalidPayload`
  mapped to `ErrorCode::ConfigInvalid`.
- `InferRequest` construction now rejects released / missing input
  buffers, present-but-empty metadata IDs, and already-expired
  deadlines.

### Deprecated

### Removed

### Fixed

### Security
