# T3 Adapter contract tests

Real backend adapters exercised through the `ExecutionSession`
interface. Validates lifecycle, readiness, shape mismatch, bad path, unload,
and `BufferRef` lifetime invariants.

Runs nightly and on release branches. Must skip cleanly (not fail) when the
required backend SDK is not available on the runner.

## V01-E04 ExecutionSession conformance suite

`execution_session_conformance.hpp` is the V01-E04-F07-T01 shared
adapter conformance suite. It drives an `ExecutionSession*` pointer
through:

- `backend_name()` identity check
- initial `Unloaded` state
- load -> prime -> infer -> unload happy path
- `infer` before `prime` (NotReady)
- `prime` before `load` (NotReady)
- bad model path (LoadFailed / ConfigInvalid)
- shape mismatch (ShapeMismatch)
- `infer_async` shape — either typed Unsupported or a valid handle
- unload then infer (NotReady)
- BufferRef lifetime invariants through V01-E03 buffer manager fixtures

Real backend adapters (TensorRT, LibTorch, Python/PyTorch sidecar, and
the future Vitis AI adapter from the V01-E05 work) reuse this suite by
supplying a `tensorplate::testing::SessionFactory` closure and a
`ConformanceConfig` populated with the adapter's backend name, model
artifact path, and sample input fixture. The mock-conformance T1 test
in `test/unit/execution_session_conformance_test.cpp` runs the same
suite through `tensorplate::testing::MockSession` to keep the suite
self-testing.
