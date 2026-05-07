# Contributing to TensorPlate

TensorPlate is an edge AI and robotics inference platform. Contributions should preserve the runtime/control-plane boundaries, hardware-adjacent reliability guarantees, and test discipline described below.

## Issue Workflow

Work is planned as:

- Epic: a cohesive roadmap outcome.
- Feature: a deliverable slice under an Epic.
- Task: a concrete implementation, test, documentation, or validation item under a Feature.
- Bug: incorrect behavior, regressions, crashes, or contract violations.

Before implementation starts, every Feature and Task should have clear acceptance criteria, technical tasks, and a definition of done.

## Architecture Rules

### Language boundaries

- Runtime hot-path and serving-worker code is C++20.
- Device agent, watchdog, and primary CLI code is Rust.
- Python SDK code is a thin HTTP API wrapper and must not use C++ FFI.
- Rust and C++ communicate through versioned IPC, HTTP, or protocol messages by default.
- In-process interop requires a narrow C ABI with explicit ownership. Do not pass C++ STL types, Rust-owned memory, vendor SDK types, or `BufferRef` internals across that boundary.

### Layering

Dependencies flow downward only:

```text
agent -> serving_worker -> input -> buffer -> serving -> scheduler -> ModelLoader -> adapter internals
```

The watchdog observes serving-worker health independently. Do not introduce upward dependencies between these layers.

### Design patterns

Use the established pattern for the component being changed:

- `ModelLoader` uses Non-Virtual Interface (NVI). Public methods remain non-virtual and delegate to private virtual adapter hooks.
- Backend adapters are created through the registry/factory. Do not branch on backend names outside the registry.
- `InferScheduler` implementations use the Strategy pattern. Executors should depend on the scheduler interface, not concrete scheduler types.
- Fallback model chains and lifecycle hooks use configured chains of responsibility, not hardcoded branches.
- Telemetry and safety signals use fire-and-forget event emission. Slow subscribers must not block inference.
- Cross-layer data uses value objects such as `ModelSpec`, `InferRequest`, `InferResult`, `WarmUpConfig`, `BufferRef`, `TensorView`, and `Error`.
- Hardware resources are managed through RAII wrappers private to adapter internals.
- Agent deploy and rollback behavior uses desired-state reconciliation, not command replay.

## Error Handling

- Use `Result<T>` for fallible operations at hardware and interface boundaries.
- Do not throw exceptions at or below the `ModelLoader` interface.
- Use typed error codes such as `LoadFailed`, `NotReady`, `InferenceFailed`, `ShapeMismatch`, `OOMError`, `Unsupported`, `Timeout`, and `ConfigInvalid`.
- Propagate errors upward until a real handling boundary logs context and translates the failure.
- Panic-level failures must emit structured fatal logs, trigger the safety path, and terminate cleanly.
- New error codes require a changelog entry.

## Configuration and Feature Flags

- Runtime behavior that varies by deployment belongs in config, not hardcoded logic.
- Config schema changes must include documentation and migration or compatibility notes when breaking.
- Experimental serving modes, schedulers, and adapters must be gated with `TP_ENABLE_<FEATURE>` feature flags.
- Experimental feature flags default off; stable feature flags default on.

## Tests and Quality Gates

Choose the narrowest test tier that proves the behavior, then add broader tests when the change crosses layers or hardware boundaries.

| Tier | Scope | Gate |
| --- | --- | --- |
| T1 unit | Single class, no hardware | Every PR |
| T2 integration | Multiple classes with mocks, no hardware | Every PR |
| T3 adapter contract | Real adapters through `ModelLoader*` | Nightly and release |
| T4 hardware-in-loop | Full stack on target hardware | Release branch |
| T5 benchmark regression | Latency, throughput, power, memory | Release branch |

General expectations:

- Unit-test new logic.
- Add T2 tests for cross-layer behavior.
- Add T3 evidence when adding or changing an adapter.
- Keep adapter mocks in `test/mocks/`, not inline in individual test files.
- Use Google Test and Google Mock for C++ tests.
- Use `pytest`, `ruff`, and `mypy` for Python SDK work.

Static checks expected by area:

- C++: `clang-format`, `clang-tidy`, ASAN, and UBSAN.
- Rust: `cargo fmt` and `cargo clippy`.
- Performance-sensitive changes: benchmark evidence when applicable.

## Documentation

- Public C++ headers need Doxygen comments for public types and methods.
- New config schema fields must be documented.
- New feature flags must be documented.
- Public behavior changes, interface changes, and new error codes require `CHANGELOG.md` entries.

## Local Build and Test

The build is split between CMake/vcpkg for C++ (`runtime/`, `serving_worker/`,
C++ tests) and Cargo for Rust (`agent/`, `cli/`, `observability/`,
`protocol/rust/`). Cargo wiring lands in V01-E01-F03; the C++ commands below
are available now.

### C++ (runtime, serving worker, T1 tests)

```bash
# One-time: install vcpkg and export VCPKG_ROOT.
git clone https://github.com/microsoft/vcpkg "$HOME/vcpkg"
"$HOME/vcpkg/bootstrap-vcpkg.sh"
export VCPKG_ROOT="$HOME/vcpkg"

# Configure (Ninja generator, vcpkg manifest mode).
cmake -S . -B build \
  -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake" \
  -DVCPKG_CHAINLOAD_TOOLCHAIN_FILE="$PWD/cmake/toolchains/x86_64-linux-gnu.cmake"

# Build the runtime, serving worker, and unit tests.
cmake --build build

# Run T1 unit tests.
ctest --test-dir build --output-on-failure -L T1

# ASAN/UBSAN dev configure.
cmake -S . -B build-asan \
  -G Ninja \
  -DCMAKE_BUILD_TYPE=Debug \
  -DTP_ENABLE_SANITIZERS=ON \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake"
cmake --build build-asan
ctest --test-dir build-asan --output-on-failure -L T1
```

`vcpkg.json` declares the C++ dependency baseline (currently GoogleTest).
Adapter SDKs (TensorRT, ONNX Runtime, CUDA) are not vendored; they are picked
up from the host environment when the corresponding `cmake/modules/Find*`
support lands.

`clang-format` and `clang-tidy` are configured at the repo root via
`.clang-format` and `.clang-tidy`. CI invokes them through the workflows in
`.github/workflows/` (V01-E01-F04).

### Rust (agent, CLI, observability, protocol crate)

The Rust workspace is rooted at `Cargo.toml`. The toolchain is pinned in
`rust-toolchain.toml`; rustup will install it on first invocation.

```bash
# Build all workspace members.
cargo build --workspace

# Run all unit and integration tests.
cargo test --workspace

# Format and lint.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Workspace-wide lints (clippy + rustc) are declared under `[workspace.lints]`
in the root `Cargo.toml`. Per-crate `[lints] workspace = true` makes each
crate inherit them. Bumping the pinned toolchain in `rust-toolchain.toml`
requires a CHANGELOG entry.

## Pull Request Expectations

Every PR should include:

- Linked issue.
- Scope summary.
- Acceptance criteria covered.
- Test evidence or an explanation for skipped tests.
- Changelog entry when public behavior, interfaces, config schema, feature flags, or error codes change.

Additional review gates:

- Changes under `include/tensorplate/` require tech lead approval.
- Changes to `ModelLoader` methods require written justification and tech lead review.
- New adapters require passing T3 adapter contract tests before merge.
- No PR may merge with failing T1 or T2 tests.

## Definition of Done

A contribution is done when:

- The implementation matches the required architectural pattern.
- No upward layer dependency is introduced.
- Fallible hardware-boundary operations return `Result<T>`.
- Hardware resources are managed through RAII where applicable.
- Cross-layer payloads use value objects and `BufferRef` / `TensorView` where applicable.
- No backend names, device paths, or magic numbers are hardcoded.
- Required tests and static checks pass.
- Public APIs, config, feature flags, and changelog entries are updated where applicable.
- Reviewer approval is complete.
