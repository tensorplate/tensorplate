# `test/`

Test tree for TensorPlate. Tests are organized by tier so PR-gated checks
stay fast and hardware/release-only checks stay separate.

## Layout

| Directory | Tier | Scope | PR gate? |
| --- | --- | --- | --- |
| `unit/` | T1 | Single class or function. No hardware. No process startup. | Yes |
| `integration/` | T2 | Multiple classes, possibly across layers, using mocks for hardware. | Yes |
| `contract/` | T3 | Real adapters exercised through `ExecutionSession*` interfaces. | Nightly / release |
| `hil/` | T4 | Full stack on target hardware (e.g., Jetson Orin). | Release branch only |
| `benchmark/` | T5 | Latency, throughput, memory, and power regression. | Release branch only |
| `mocks/` | — | Shared fakes and mocks consumed by `unit/`, `integration/`, and `contract/`. | n/a |
| `models/` | — | Small model fixtures used by tests. Large fixtures are fetched, not committed. | n/a |

## Conventions

- Shared mocks live under `mocks/`. **Do not** define mocks inline in
  individual test files; reuse keeps adapter contract surfaces consistent.
- Model fixtures committed to `models/` must be small (typically &lt;= 1 MB)
  and have a documented provenance. Larger fixtures are fetched on demand by
  test setup scripts and not committed.
- C++ tests use Google Test and Google Mock.
- Rust tests use the standard `cargo test` harness.
- T1 and T2 tests must run without a connected device and without elevated
  privileges.
- T3 contract tests may require a backend SDK at runtime (TensorRT, ONNX
  Runtime). They must skip cleanly when the SDK is unavailable rather than
  fail.
- T4 hardware-in-loop tests require a physical Jetson and are gated to
  release branches.
- T5 benchmarks publish results as artifacts; their pass/fail thresholds are
  reviewed per release.
- Manual Jetson target validation for adapter/release readiness is documented
  in [`docs/contributing/jetson-target-validation.md`](../docs/contributing/jetson-target-validation.md).

## Running

The CMake (V01-E01-F02) and Cargo (V01-E01-F03) baselines wire the runnable
test targets. Until then, this directory documents intent only.

```bash
# C++ unit and integration tests (lands in V01-E01-F02-T03)
cmake --build build --target tp_test_unit
ctest --test-dir build --output-on-failure -L T1

# Rust workspace tests (lands in V01-E01-F03-T02)
cargo test --workspace
```

For on-device Jetson validation of T1/T2/T3 with TensorRT enabled, use
[`docs/contributing/jetson-target-validation.md`](../docs/contributing/jetson-target-validation.md).

## Adding a new test

1. Pick the narrowest tier that proves the behavior.
2. Place T1 next to the unit it exercises under `unit/<package>/`.
3. Place T2 under `integration/<scenario>/`.
4. New adapter? Add a T3 case under `contract/` and reuse the conformance
   harness rather than reinventing it.
5. Update relevant `README.md` files if the directory layout changes.
