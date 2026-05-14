# Changelog

All notable changes to TensorPlate will be documented in this file.

This project follows the spirit of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic versioning once public releases begin.

## [Unreleased]

### Added

- Deadline-aware admission and queued-expiry coverage (V01-E06-F03)
  at `test/unit/scheduler_deadline_test.cpp`. The `FifoScheduler`
  uses the injected `SchedulerClock` (monotonic only) for every
  deadline decision and rejects new admission with
  `Error::Code::Timeout` when a request is already past its deadline
  or when its estimated completion exceeds `deadline +
  deadline_margin`. The estimate accounts for current queue depth
  and in-flight count using the configured
  `default_service_estimate`; per-request `ServiceEstimate` overrides
  the default. `expire_due()` and `next()` both sweep stale queued
  requests, releasing input buffers through `release_request_buffers`
  when a `BufferManager` is wired into runtime hooks. 12 T1 cases
  cover boundary admission, monotonic-time isolation from wall
  clock, queue-depth-aware rejection, and deterministic
  buffer release on rejection. The shared `FakeSchedulerClock`
  default origin is now anchored to real `steady_clock` + 1 hour so
  deadlines composed against the fake clock also satisfy
  `InferRequest::create`'s validation gate.
- FIFO scheduler ordering and capacity coverage (V01-E06-F02) at
  `test/unit/scheduler_fifo_test.cpp`. The v0.1.0 default
  `FifoScheduler` (registered under the stable `fifo` policy key)
  preserves enqueue order among admitted requests, enforces
  `queue_capacity` with `Error::Code::OOMError`, gates dispatch on
  `in_flight_capacity`, increments in-flight on dispatch (not on
  enqueue), and exposes queue depth / in-flight count / wait-time
  high water through the `metrics()` snapshot without leaking the
  internal `std::deque` to callers. 13 T1 cases assert the dispatch
  order, capacity behavior, completion-frees-slot semantics, and
  that mock executor code only holds `InferScheduler*` (not
  `FifoScheduler*`).
- `InferScheduler` public interface (V01-E06-F01) at
  `include/tensorplate/scheduler/scheduler.hpp` plus the supporting
  envelope (`SchedulerRequest`), monotonic clock abstraction
  (`SchedulerClock` / `SystemSchedulerClock`), pressure value
  objects (`PressureSignal`, `PressureSource`, `PressureSeverity`),
  and the `SchedulerEvent` / `SchedulerEventSink` /
  `SchedulerMetrics` types. Includes the `InferSchedulerConcept`
  compile-time interface check. Strategy pattern is mediated by a
  `SchedulerPolicyRegistry` and the `make_scheduler` /
  `validate_scheduler_config` factory entry points in
  `include/tensorplate/scheduler/factory.hpp`. v0.1.0 registers the
  built-in `fifo` policy; unknown policies return
  `Error::Code::Unsupported`. New config schema at
  `config/schemas/scheduler.json`. Architecture doc at
  `docs/architecture/scheduler.md`. T1 coverage at
  `test/unit/scheduler_interface_test.cpp` plus shared mocks at
  `test/mocks/fake_scheduler_clock.hpp` and
  `test/mocks/scheduler_fixtures.hpp`.
- Kria / Vitis AI adapter design-review document at
  `docs/architecture/kria-vitis-ai-review.md` (V01-E05-F07). Maps a
  future Xilinx/AMD Kria adapter using Vitis AI and DPU execution
  against the published v0.1.0 contracts (`ExecutionSession` NVI,
  `BackendCapability`, `BackendRegistry`, `ModelSpec` and the
  `backend_hint` enum, `BufferRef` / `TensorView`, the bundle
  envelope, and the session event taxonomy). The review concludes
  that **no public interface change is required for v0.1.0 freeze**;
  the only work required for a future Vitis AI adapter is a new
  `runtime/src/adapters/vitis_ai/` directory, a bundle sibling block
  for Vitis-style calibration metadata (addable through the V01-E13
  schema-evolution rules), a T1 unit-test set mirroring the
  TensorRT / LibTorch pattern, and Kria K26 / K24 HIL validation.
- Real-adapter conformance harness (V01-E05-F06-T01) at
  `test/contract/real_adapter_conformance_test.cpp` (T3). Reuses the
  V01-E04 `ExecutionSession` conformance suite from
  `test/contract/execution_session_conformance.hpp` and runs it
  against every adapter compiled into this build of `tp_runtime`. The
  `python_pytorch` adapter passes the full lifecycle suite on host CI
  via the `FixtureBackend`; the TensorRT and LibTorch variants run
  only when their SDKs are detected (HIL/release tier per
  V01-E05-F02 / F03). `test/CMakeLists.txt` now compiles
  `tp_test_contract` and labels its tests `T3`.
- Sidecar failure-injection tests (V01-E05-F06-T03) at
  `backends/python_pytorch/tests/test_failure_injection.py`. The
  `FixtureBackend` exposes `fail_load` / `fail_prime` / `fail_infer`
  hooks; the new tests cover typed `load_failed`, `inference_failed`,
  `config_invalid` (missing model_spec, malformed tensor entry), and
  the cancel-then-recordable-by-backend path. Combined with the
  V01-E05-F04 runner tests and C++ supervisor shutdown behavior, the
  host baseline covers typed runner errors, timeout/cancel message
  handling, malformed request rejection, and deterministic cleanup
  paths; heartbeat-driven liveness and externally killed sidecar
  recovery remain scheduler/supervision follow-up work.
- Golden-output fixture matrix and tolerance documentation at
  `test/models/GOLDEN_FIXTURES.md`. Defines what a golden fixture
  means for each adapter family, where each runs in the CI tier, how
  it is generated, what its expected output and tolerance are, and
  how the JSON comparison helper will work when the first real
  fixture lands. The fixture backend round-trip already covers exact
  bytewise correctness for `python_pytorch`; vision-TensorRT and
  LibTorch golden artifacts land in V01-E05-F02-T03 / F03-T03 /
  V01-E15.
- Python/PyTorch sidecar execution-backend adapter and supervisor
  (V01-E05-F05) under `runtime/src/adapters/python_pytorch/`,
  registered as `python_pytorch`. The adapter forks one Python sidecar
  subprocess per execution session (the V01-E05 closed decision), binds
  a Unix-domain socket under `TMPDIR`, accepts the child's connection,
  reads its `ready_event`, and translates `ExecutionSession::load /
  prime / infer / unload` into the sidecar IPC schema (V01-E05-F04).
  The sidecar wire protocol includes `infer_async`, but the C++ adapter
  keeps native async disabled until V01-E06 provides a real completion
  channel; `ExecutionSession::infer_async` therefore returns typed
  `Unsupported` without dispatching or allocating outputs. Capability
  record declares dynamic-shape support; async, generation, streaming,
  and KV-cache remain false.
- `SidecarProcess` (in `runtime/src/adapters/python_pytorch/`) owns the
  subprocess + socket pair, terminates the child with SIGTERM (or
  SIGKILL after a 500 ms grace period) on unload / error, and unlinks
  the socket path. `SidecarLauncher` is injectable so tests can run the
  Python runner via a non-default interpreter without touching the
  adapter code. The built-in factory honors
  `TP_PYTHON_PYTORCH_EXECUTABLE`, `TP_TEST_PYTHON_EXE`, then
  `TP_TEST_PYTHON` before falling back to `python3`; the default
  launcher does `fork()` +
  `execvp(python3, "-m", "tensorplate_pytorch_backend", "--socket",
  path)`.
- Input/output tensor marshaling: inputs are read out of `BufferManager`
  via `manager->view(buffer, tensor_view)`, packed into the sidecar
  payload region, and described in the JSON header's `tensors[]` array.
  Outputs are sliced back out of the response payload by
  `payload_offset / payload_length`, written into freshly allocated
  output `BufferRef`s, and surfaced through `NamedOutput`. The adapter
  refuses to construct a session without a `BufferManager` hook
  (`Error::Code::ConfigInvalid`).
- Timeout, cancellation, and health handling: per-operation deadlines
  use `std::chrono::steady_clock` clamped against `InferRequest`'s
  monotonic deadline; sidecar timeouts surface as
  `Error::Code::Timeout`; malformed response frames map to
  `Error::Code::InferenceFailed`. `prime` performs a real
  `health_check` round-trip and requires a ready health payload before
  publishing the session as ready. The adapter terminates the sidecar on
  unload, load failure, and transport failure so the OS does not retain
  a zombie. Adapter-owned heartbeat polling and `Cancel` dispatch are
  reserved for the scheduler/supervision wiring that owns async result
  delivery.
- `TP_ENABLE_PYTHON_PYTORCH_SIDECAR` is flipped on by default in
  `runtime/CMakeLists.txt`. When the flag is on the runtime links
  `nlohmann_json::nlohmann_json` (header-only) for JSON header
  encode/decode.
- T2 integration test in
  `test/integration/python_pytorch_adapter_test.cpp` exercises the full
  C++ ↔ Python lifecycle through the `FixtureBackend`: registration,
  capability publication, end-to-end echo (load → prime → infer →
  unload through real Unix-socket IPC), wrapper-level `infer_async`
  returning typed `Unsupported` without output allocation, and
  infer-before-prime returning `NotReady`. C++ CI now installs
  `backends/python_pytorch` before running T2/T3 so the round-trip is
  exercised instead of silently skipped.
- C++ CI now includes an adapter-shell job that builds with
  `TP_ENABLE_TENSORRT=ON` and `TP_ENABLE_LIBTORCH=ON` on a host without
  proprietary SDKs, then runs T1. This keeps the no-SDK registration and
  typed `Unsupported` paths compiling even when the default host matrix
  leaves hardware adapters disabled.
- Python/PyTorch sidecar IPC contract and Python backend runner
  (V01-E05-F04). The on-wire envelope is documented in
  `include/tensorplate/ipc/sidecar_codec.hpp`:
  `[u32 magic 'TPSC'][u32 wire_version][u32 header_len][u32 payload_len]
   [JSON header][raw tensor payload]`, all u32 fields big-endian, with
  generous-but-bounded maxima (1 MiB header, 256 MiB payload). The
  schema for the JSON header lives in
  `protocol/schemas/python_pytorch_ipc.json` and covers the seven
  request kinds (`load_model`, `prime`, `infer`, `infer_async`,
  `cancel`, `unload`, `health_check`) plus matching `*_response`
  kinds and the unsolicited `ready_event` / `error_event` /
  `metric_event` events. Successful `infer_async_response` headers carry
  `async_id`; successful `health_check_response` headers carry a bounded
  `health` payload (`ready`, `backend_factory`, `uptime_ns`,
  `last_error`). The Rust protocol mirror models these fields so schema
  fixtures do not drift from the Python runner.
- C++ codec helpers (`encode_frame`, `decode_frame`, `decode_frames`)
  that distinguish typed `Error::Code::NotReady` ("need more bytes")
  from `Error::Code::ConfigInvalid` ("malformed frame") so adapters can
  loop on streaming reads safely. Implemented in
  `runtime/src/ipc/sidecar_codec.cpp`; covered by
  `test/unit/sidecar_codec_test.cpp` for round-trips, partial-prefix
  / partial-body, bad magic, bad wire version, oversized header /
  payload, and multi-frame pipelines stopping at a partial frame.
- `include/tensorplate/ipc/unix_socket.hpp` plus
  `runtime/src/ipc/unix_socket.cpp`: minimal RAII `UnixSocket` wrapper
  around POSIX stream sockets with monotonic-deadline-aware
  `connect`, `bind_and_listen`, `accept`, `read_exact`, and
  `write_all`. Returns `Error::Code::Timeout` on deadline exhaustion
  and `Error::Code::ConfigInvalid` for paths exceeding `sun_path`.
  Covered by `test/integration/sidecar_socket_e2e_test.cpp` which
  forks a child and round-trips one frame end-to-end.
- Python backend runner under
  `backends/python_pytorch/src/tensorplate_pytorch_backend/`:
  `codec.py` mirrors the C++ wire format; `protocol.py` enumerates
  the schema constants; `backends/` ships the dependency-free
  `FixtureBackend` (echoes inputs as `echo_<name>` outputs) plus the
  `Backend` Protocol that the V01-E05-F05 TorchScript / SmolVLA
  backend will implement; `runner.py` runs a synchronous
  request/response loop with typed `BackendError` mapping to
  `*_response status: error` frames, and is wired as the
  `tensorplate-backend-python-pytorch` console script in
  `pyproject.toml`. Twenty-one pytest tests under
  `backends/python_pytorch/tests/` cover the codec round-trips, the
  lifecycle happy path through the fixture backend, infer-before-load
  (`not_ready`), unknown / bad-version (`unsupported`),
  cancel-then-infer (`timeout`), health-check, async-infer
  identification, and payload-window overflow (`shape_mismatch`).
- `nlohmann-json` added to `vcpkg.json` (header-only) so the V01-E05-F05
  C++ adapter can parse the JSON sidecar header without re-implementing
  a JSON decoder.
- LibTorch native execution backend adapter shell under
  `runtime/src/adapters/libtorch/` (registered as `libtorch`). Loads
  TorchScript (`torch::jit::load`) modules and is positioned as a
  reference / native-C++ backend, *not* a fallback for
  `python_pytorch` bundles. Capability record advertises
  FP32/FP16/BFloat16 precision and dynamic-shape support; async,
  generation, streaming, and KV-cache flags remain false. The adapter
  source compiles when `TP_ENABLE_LIBTORCH=ON`; when CMake also locates
  a LibTorch C++ distribution (`Torch_DIR` -> `find_package(Torch)`),
  it defines `TP_HAS_LIBTORCH_SDK=1` and the adapter loads the
  TorchScript module, maps row-major `BufferManager` inputs to CPU
  tensors, executes synchronous `forward`, and materializes Tensor or
  Tuple[Tensor, ...] outputs back into owned output buffers. Without
  the SDK the adapter still registers and
  surfaces typed `Error::Code::Unsupported` from `do_load` with an
  actionable rebuild hint. T1 unit tests in
  `test/unit/libtorch_adapter_test.cpp` cover registration, capability
  publication, the no-SDK `Unsupported` path, and explicit verification
  that `backend_hint: python_pytorch` does not silently redirect to
  LibTorch (V01-E05-F03-T01 / T02). Exported-graph fixture generation,
  SDK-enabled T3 evidence, and Jetson T4 conformance land in
  V01-E05-F03-T03 and V01-E05-F06.
- TensorRT execution backend adapter shell under
  `runtime/src/adapters/tensorrt/` (registered as `tensorrt`). The
  adapter publishes its `BackendCapability` (FP32/FP16/INT8, fixed-shape
  binding, sync execution only) and owns TensorRT and CUDA SDK handles
  privately through RAII wrappers (`TensorRTState`, `CudaStreamHandle`,
  `CudaDeviceBuffer`). The adapter compiles when `TP_ENABLE_TENSORRT=ON`;
  if the CMake configuration detects an installed TensorRT/CUDA SDK it
  defines `TP_HAS_TENSORRT_SDK=1` and the adapter deserializes the
  engine file and creates the runtime/engine/execution context. Without
  the SDK the adapter still registers and surfaces typed
  `Error::Code::Unsupported` from `do_load` with an actionable message
  so `tensorplate doctor` can enumerate it (V01-E05-F02-T01 / T02).
- T1 unit tests under `test/unit/tensorrt_adapter_test.cpp` covering
  registration under the stable key, capability-record consistency,
  backend_name on the constructed session, load-without-SDK returning
  `Unsupported`, and `validate_backend_hint` precision filtering.
  Vision golden conformance (T3) and Orin HIL validation (T4) land in
  V01-E05-F02-T03 / V01-E05-F06.
- `include/tensorplate/backend/capability.hpp` and
  `include/tensorplate/backend/registry.hpp` defining the vendor-neutral
  `tensorplate::BackendCapability` value object and the thread-safe
  `tensorplate::BackendRegistry` used by bundle validation and
  execution-session creation. Capability records publish backend name,
  optional profile id, supported precision list, shape-support tier,
  async / generation / streaming / KV-cache flags, op-coverage
  percentage, and memory estimate/limit. The registry rejects empty
  keys, null factories, and capability/name mismatches with
  `Error::Code::ConfigInvalid`; duplicate registration returns
  `Error::Code::Internal`; unknown backends surface as
  `Error::Code::Unsupported`. `validate_backend_hint` rejects unknown
  backends and declared precisions that the adapter does not advertise
  without falling back at inference time (V01-E05-F01).
- `include/tensorplate/backend/builtin.hpp` exposes
  `register_builtin_backends(BackendRegistry&)` so callers (the serving
  worker, conformance tests, doctor checks) can opt their registry into
  the adapter set compiled into `tp_runtime`. Adapter availability is
  driven by the new `TP_ENABLE_TENSORRT`, `TP_ENABLE_LIBTORCH`, and
  `TP_ENABLE_PYTHON_PYTORCH_SIDECAR` CMake options (all OFF by default
  for host CI; flipped on per adapter in V01-E05-F02 / F03 / F05).
- `protocol/schemas/backend_capability.json` mirrors `BackendCapability`
  so capability records can cross process boundaries without leaking
  adapter-specific types.
- `include/tensorplate/core/execution_session.hpp` defining the canonical
  public `tensorplate::ExecutionSession` lifecycle interface. The public
  method set is `load`, `prime`, `infer`, `infer_async`, `unload`,
  `is_ready`, and `backend_name`; lifecycle methods are non-virtual NVI
  wrappers and adapters override protected `do_*` implementation methods.
  No vendor SDK type appears in the header (V01-E04-F01-T02).
- `docs/architecture/execution-session.md` documenting the canonical
  `ExecutionSession` name decision (selected over the alternate
  `ModelLoader` spelling carried in the older implementation guidelines),
  the NVI pattern, the lifecycle state machine, the async method shape,
  the event taxonomy, and the non-GPU compatibility review notes
  (V01-E04-F01-T01).
- `tensorplate::SessionState` enum (`unloaded`, `loaded`, `ready`,
  `failed`) with `to_string` / `session_state_from_string` helpers, and
  `tensorplate::AsyncInferHandle` carrying `request_id` plus a
  session-scoped monotonically increasing `async_id`.
- `tensorplate::SessionEventKind` enum and `tensorplate::SessionEvent`
  record with `to_string` / `session_event_kind_from_string` helpers,
  plus the `tensorplate::SessionEventSink` interface used by the NVI
  wrapper to emit lifecycle and inference events.
- Session lifecycle state machine wiring the V01-E04-F01 public methods
  through the protected `do_*` adapter override points: `load`
  transitions `unloaded -> loaded` (or `unloaded -> failed`), `prime`
  transitions `loaded -> ready` (or `loaded -> loaded` on
  `ConfigInvalid`, otherwise `loaded -> failed`), `unload` returns any
  state to `unloaded` (or transitions to `failed` on adapter failure),
  and `infer` / `infer_async` surface `Error::Code::NotReady` before
  any adapter dispatch unless the session is `Ready`. The state
  machine is adapter-neutral and intentionally general enough for
  TensorRT engine setup, LibTorch model load, Python sidecar startup,
  and a future Vitis AI `.xmodel` / DPU lifecycle (V01-E04-F02).
- Shared mock `tensorplate::testing::MockSession` under `test/mocks/`
  that drops into `ExecutionSession*` and lets tests program adapter
  success/failure and inspect adapter dispatch counts and last-seen
  request/spec. Used by the V01-E04 lifecycle, validation, timing,
  async, event-emission, and conformance test suites.
- NVI readiness and validation gates in `ExecutionSession::infer` and
  `ExecutionSession::infer_async`: requests are rejected before any
  adapter dispatch when the session is not `Ready` (`NotReady`), when
  `request_id` / `endpoint` / `inputs` are empty (`ConfigInvalid`), on
  empty or duplicate input names (`ConfigInvalid`), on released or
  missing input buffers (`ConfigInvalid`), on tensor byte windows that
  do not fit inside their owning buffers (`ShapeMismatch`), and on
  already-expired monotonic deadlines (`Timeout`). The gates apply
  uniformly to sync and async paths so adapter `do_infer` /
  `do_infer_async` implementations cannot bypass them (V01-E04-F03).
- Monotonic latency stamping in `ExecutionSession::infer`: the wrapper
  measures `execution_latency` around the adapter `do_infer` call using
  `std::chrono::steady_clock` (no wall-clock dependency) and stamps it
  into the returned `InferResult` on both success and adapter-failure
  paths. Readiness and validation failures bypass the adapter entirely
  and surface as `Result::error` rather than a failure `InferResult`
  (V01-E04-F04-T01).
- Output validation in `ExecutionSession::infer`: empty outputs vectors,
  empty or duplicate output names, released output buffers, and tensor
  byte windows that overflow their buffers are all rejected before
  success is returned. When a `BufferManager` is supplied through
  adapter construction hooks, partial adapter-published outputs are
  released via `release_partial_outputs` so a failed `infer` does not
  leak buffer capacity (V01-E04-F04-T02).
- `ExecutionSession::infer_async` typed unsupported path: the default
  wrapper path returns `Error::Code::Unsupported` so v0.1.0 adapters
  without native async satisfy the public method shape without
  pretending to be async. Readiness (`NotReady`) and request validation
  errors (`ConfigInvalid`, `ShapeMismatch`, `Timeout`) are surfaced
  **before** the unsupported capability is considered, and the
  unsupported path allocates no output buffers and never dispatches to
  adapter execution. Native-async adapters opt in through the protected
  capability hook and override `do_infer_async` to return an
  `AsyncInferHandle` whose `async_id` is session-scoped and
  monotonically increasing through the `next_async_id()` helper
  (V01-E04-F05).
- Shared V01-E04 ExecutionSession conformance suite at
  `test/contract/execution_session_conformance.hpp`. A
  `tensorplate::testing::SessionFactory` closure plus a
  `ConformanceConfig` drives any `ExecutionSession*` adapter through
  backend-name identity, initial not-ready state, the load -> prime ->
  infer -> unload happy path, infer-before-prime, prime-before-load,
  bad model path, shape mismatch, infer_async (typed Unsupported or
  handle), unload-then-infer, and `BufferRef` lifetime invariants. Real backend
  adapters (TensorRT, LibTorch, Python/PyTorch sidecar, future Vitis
  AI) reuse the same suite without rewriting it. A T1 mock-conformance
  test in `test/unit/execution_session_conformance_test.cpp` runs the
  suite through `MockSession` so the suite is self-testing
  (V01-E04-F07-T01).
- `docs/architecture/non-gpu-lifecycle-review.md` recording the
  V01-E04-F07-T02 non-GPU lifecycle compatibility review and sign-off.
  The review walks `ExecutionSession`, `ModelSpec`, `BufferRef`,
  `TensorView`, and the event taxonomy against a future Kria/Vitis AI
  adapter (`.xmodel` discovery, DPU runner instantiation, fixed-shape
  binding, INT8 calibration metadata, adapter-owned memory copies) and
  confirms the V01-E04 interface is implementable without public
  interface revision before V01-E05 adapter work begins. A compile-time
  macro guard in the T1 interface test mechanically enforces that no
  CUDA / TensorRT / LibTorch / Vitis AI / XRT / DPU SDK type leaks into
  the public ExecutionSession header (V01-E04-F07-T02).
- Lifecycle and inference event emission from every public NVI wrapper.
  `load`, `prime`, `infer`, `infer_async`, and `unload` emit paired
  `*_start` / `*_end` events on success and `*_failed` (or
  `validation_failed` for pre-dispatch rejection, `unsupported_async`
  for the typed Unsupported async path) on failure. Each event carries
  bounded fields (`backend_name`, optional `model_id`, optional
  `request_id`, optional `Error::Code`, monotonic `duration`, and
  `state_after`) and no raw payload bytes. Emission is wrapped in a
  defensive `try { ... } catch (...) {}` so a throwing sink cannot
  corrupt session state; the wrapper continues the lifecycle path
  unchanged. Tests use the new `tensorplate::testing::RecordingEventSink`
  and `tensorplate::testing::ThrowingEventSink` shared mocks
  (V01-E04-F06).
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
