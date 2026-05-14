# Local validation commands

The exact set of commands a contributor should run before opening a PR.
These mirror what `.github/workflows/cpp.yml` and
`.github/workflows/rust.yml` execute, so a clean local pass should produce
a clean CI pass.

> Hardware-dependent tiers (T3 contract, T4 hardware-in-loop, T5 benchmark)
> are not part of this baseline and are not required for ordinary PRs.
> See [`test/README.md`](../../test/README.md) for tier scope and gating.
> For the manual Jetson target-device pass used by adapter/release
> readiness, see
> [`jetson-target-validation.md`](jetson-target-validation.md).

## Prerequisites

- The TensorPlate dev container (recommended): open the repo in VS Code or
  GitHub Codespaces and use "Reopen in Container". Everything below is
  preinstalled.
- Or a host with: CMake 3.25+, Ninja, Clang 15 (or GCC 11+), `clang-format`,
  `clang-tidy`, vcpkg with `$VCPKG_ROOT` set, and rustup. The pinned Rust
  toolchain is installed automatically by `rust-toolchain.toml` on first
  invocation.

## C++

```bash
# 1. Configure with vcpkg + the project x86_64 toolchain.
cmake -S . -B build \
  -G Ninja \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DTP_WARNINGS_AS_ERRORS=ON \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake" \
  -DVCPKG_CHAINLOAD_TOOLCHAIN_FILE="$PWD/cmake/toolchains/x86_64-linux-gnu.cmake"

# 2. Build.
cmake --build build --parallel

# 3. Run T1 unit tests.
ctest --test-dir build --output-on-failure -L T1

# 4. Format check.
mapfile -t cpp_files < <(
  git ls-files include runtime serving_worker test \
    | grep -E '\.(cpp|cc|cxx|hpp|hh|h)$'
)
clang-format --dry-run -Werror "${cpp_files[@]}"

# 5. clang-tidy on runtime + serving_worker translation units.
mapfile -t tidy_files < <(
  git ls-files runtime serving_worker | grep -E '\.cpp$'
)
clang-tidy --quiet -p build --warnings-as-errors='*' "${tidy_files[@]}"

# 6. Optional: ASAN/UBSAN configure and re-run T1.
cmake -S . -B build-asan \
  -G Ninja \
  -DCMAKE_BUILD_TYPE=Debug \
  -DTP_ENABLE_SANITIZERS=ON \
  -DTP_WARNINGS_AS_ERRORS=ON \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake"
cmake --build build-asan --parallel
ASAN_OPTIONS=detect_leaks=1:halt_on_error=1 \
  UBSAN_OPTIONS=print_stacktrace=1:halt_on_error=1 \
  ctest --test-dir build-asan --output-on-failure -L T1
```

## Rust

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

## One-shot script equivalent

The above can be wrapped in a script per developer preference; it is
deliberately not committed because the canonical sequence is encoded in
the CI workflows themselves. If you want to compare a local run to CI,
rerun the corresponding workflow job locally with
[`act`](https://github.com/nektos/act) or read the YAML in
[`.github/workflows/`](../../.github/workflows/).
