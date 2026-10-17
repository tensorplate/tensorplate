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

CI compiles, formats and runs clang-tidy with version 14 of the tools, the
hosted runner image's own, and fails a job whose tool reports another major
version (`tools/ci/select-clang.sh`). The dev container and the host above use
15, which can format a file or report a finding differently: when a format or
tidy job fails on code that passes locally, run version 14 of the tool.

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

# 5. clang-tidy on runtime + serving_worker translation units; the stream
#    listener's sources build only with TP_ENABLE_STREAMING_GRPC=ON.
mapfile -t tidy_files < <(
  git ls-files runtime serving_worker | grep -E '\.cpp$' |
    grep -v '^runtime/src/serving/stream/'
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

# 7. Optional: ThreadSanitizer, in its own build tree (it cannot be combined
#    with ASAN; configure refuses TP_ENABLE_TSAN with TP_ENABLE_SANITIZERS).
cmake -S . -B build-tsan \
  -G Ninja \
  -DCMAKE_BUILD_TYPE=Debug \
  -DTP_ENABLE_TSAN=ON \
  -DTP_WARNINGS_AS_ERRORS=ON \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake"
cmake --build build-tsan --parallel
# T2 starts the Python sidecar: install it first, as CI does, with
# `pip install -e backends/python_pytorch` for the interpreter named here.
TSAN_OPTIONS=halt_on_error=1:second_deadlock_stack=1 TP_TEST_PYTHON=python3 \
  ctest --test-dir build-tsan --output-on-failure -L 'T1|T2'
```

On some recent Linux kernels, test binaries built with clang 15's sanitizers
crash at startup: ASAN binaries intermittently, TSAN binaries on every run.
Running them with address-space randomization off,
`setarch "$(uname -m)" -R ctest ...`, avoids it.

## Rust

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

## Python SDK streams through a real agent (optional)

`sdk/python/tests/test_e2e_agent_stream.py` runs the streaming client
against a real agent, which deploys with `--streaming` to a serving worker
built with the stream listener. The worker starts the Python sidecar, whose
fixture backend serves speech without a model. No GPU is needed. The cases
are skipped unless the three binaries are named:

```bash
cmake -S . -B build-streaming -G Ninja -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DTP_ENABLE_STREAMING_GRPC=ON -DVCPKG_MANIFEST_FEATURES=streaming-grpc \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake" \
  -DVCPKG_CHAINLOAD_TOOLCHAIN_FILE="$PWD/cmake/toolchains/x86_64-linux-gnu.cmake"
cmake --build build-streaming --target tp_serving_worker --parallel
cargo build -p tensorplate-agent -p tensorplate-cli
export TENSORPLATE_STREAMING_WORKER_BIN=$PWD/build-streaming/serving_worker/tensorplate-serving
export TENSORPLATE_AGENT_BIN=$PWD/target/debug/tensorplate-agent
export TENSORPLATE_CLI_BIN=$PWD/target/debug/tensorplate
cd sdk/python && pytest -q tests/test_e2e_agent_stream.py
```

Run it with the interpreter the SDK is installed in, with its `speech`
extra. The sidecar runs on that interpreter too. Each case starts its own
agent on six free loopback ports between 20000 and 30000. The four cases take
about half a minute; the redeploy case waits 20 seconds of that on purpose.
The agent's platform gate matches the host against the rows in
`config/platform`. A host with a GPU no row names matches none. A plain
Ubuntu 24.04 CPU development machine does match one, `ubuntu2404-x86-cpu`
("Any x86_64 Ubuntu 24.04 host"), but a matched row can still refuse the
deploy by its support level or by the admission posture in force. The
deploy's error then gives the reason, as
``platform cannot admit this deploy: platform is outside the validated environment for row `<row>` ``,
or with ``platform row `<row>` is planned but not validated`` or
``platform row `<row>` is experimental and not deployable`` after the
colon. On such a host, set `TENSORPLATE_PLATFORM_REGISTRY_DIR` to a copy of
that directory with a row that admits the host.

## Public hygiene (before every push)

The repository is public and a push is permanent, so the scan
`.github/workflows/evidence-publication.yml` runs on every pull request
also runs before every push. It covers the commits the push would publish
and the branch checked out, plus the pull request title and body you are
about to submit. The tree scan and the scanner's test follow it:

```bash
tools/validation/check-public-hygiene.sh --base origin/develop --local \
  --message pr-title.txt --message pr-body.md
tools/validation/check-public-hygiene.sh --tree
test/validation/public_hygiene_test.sh
```

`--local` adds this machine's user name, host name and gcloud project as
literals the scan must not find; add `--literals FILE` for a private
literal file kept outside the repository. The tree scan fails when a
change leaves an allowlist count wrong, for example by replacing a test
value an entry accepted. The test takes about two minutes. CI runs it on
every pull request, so running it locally matters most when the scanner,
its fixtures, its allowlist or a file the allowlist names changes. Rule 10
of [`fixture-and-evidence-rules.md`](../validation/fixture-and-evidence-rules.md)
says what is checked and how to resolve a finding.

## One-shot script equivalent

The above can be wrapped in a script per developer preference; it is
deliberately not committed because the canonical sequence is encoded in
the CI workflows themselves. If you want to compare a local run to CI,
rerun the corresponding workflow job locally with
[`act`](https://github.com/nektos/act) or read the YAML in
[`.github/workflows/`](../../.github/workflows/).

## TensorRT SDK build without a GPU

The optional `tensorrt-sdk` job in `tensorrt-sdk.yml` requires the real SDK with
`TP_ENABLE_TENSORRT=ON` and `TP_REQUIRE_TENSORRT_SDK=ON`. It builds the
worker and tests, checks the worker's ELF dependencies on `nvinfer` and
`cudart`, and runs `--version` to prove only that the loader resolves the
libraries: this flag returns before worker initialization. It also runs
T1/T2/T3 tests. TensorRT tests cover registration, capabilities, empty-file
rejection, and a non-empty invalid-engine case that expects `LoadFailed`.
The junk-engine case runs only where a usable CUDA driver and device exist;
otherwise it reports `GTEST_SKIP` because the TensorRT runtime needs a driver
to get past `createInferRuntime`. Thus `createInferRuntime` without a driver
is unverified on the hosted runner. This is compile/link coverage, not evidence
of successful engine creation or inference. The existing no-SDK and sanitizer
jobs remain separate.

The workflow runs on every push to main/develop. Pull requests run it only
when they touch `runtime/`, `serving_worker/`, `include/`, `cmake/`,
`CMakeLists.txt`, `vcpkg.json`, `test/`, `tools/ci/*tensorrt*`, or the workflow
file itself. Keep this check optional in branch protection: a skipped,
path-filtered required workflow blocks unrelated pull requests. An SDK
build regression caused outside those paths is caught on the develop push.

The exact NVIDIA package pins and repository URL live in
[`tools/ci/tensorrt-sdk-versions.sh`](../../tools/ci/tensorrt-sdk-versions.sh), including
CUDA development dependencies. The installer uses NVIDIA's checksum-pinned
keyring package, checks its download/checksum/install status explicitly, and
uses the authenticated apt repository. APT verifies the signed repository
index and checks package hashes as it downloads them into a fresh temporary
archive directory. No SDK archive cache is restored or saved; the temporary
directory is removed on exit. It installs neither a GPU
driver nor the full CUDA toolkit. `cuda-driver-dev` supplies link stubs, not
a kernel driver. On x86, CMake discovers headers only: neither nvcc/nvvm nor NVIDIA libraries
are link inputs. The adapter loads nvinfer and cudart at session load. Jetson keeps its existing SDK discovery and linkage.

The x86 SDK now follows the managed speech runtime's CUDA family, as recorded
in `packaging/speech-runtime/lock/cuda.txt`. It uses a newer TensorRT 10 API
than the [recorded Jetson build](../validation/sdk-e2e-validation.md#observed-evidence-yolov8n-on-jetson-orin-nano-2026-06-18).
The adapter still uses TensorRT 10 deletion, named-tensor, shape/address and
`enqueueV3` APIs. Jetson's installed SDK is unchanged. Serialized engines still
need to match their target GPU and runtime.

The job also tests the [private loader contract](../architecture/tensorrt-loader.md)
in a separate executable with no NVIDIA link dependencies. Synthetic tests
exercise missing libraries/exports, version mismatches and handle lifetime;
`TP_TEST_TENSORRT_SDK=1` additionally requires loading the installed SDK and
querying its versions, without creating a runtime or probing a device. This
loader is used by the adapter with process-lifetime cached ownership. A required
real driverless test rejects before runtime creation. Clean Ubuntu 22.04 and
24.04 container jobs require a real Python worker response and TensorRT
`Unsupported` without NVIDIA libraries, plus negative ELF assertions. The
sanitizer job uses vendor-module-only LeakSanitizer suppressions and retains
leak detection. The release profile still disables TensorRT.

To reproduce the job, use a disposable Ubuntu 22.04 amd64 container with
CMake 3.25+, Clang 14, Ninja, curl, CA certificates, Python and vcpkg at the
manifest baseline. The installer modifies the container's apt sources and
installed packages. Run `tools/ci/install-tensorrt-sdk.sh`,
source `tools/ci/tensorrt-sdk-versions.sh`, and use the job's configure/build/test
commands with your own build directory and `-DCUDAToolkit_ROOT="$CUDAToolkit_ROOT"`.
A fresh install of the current SDK closure downloaded approximately 1.4 GB
and used 3.6 GB of installed space in a local Ubuntu container. Allow additional
build space. Every fresh job downloads those packages again; the job summary
records APT's actual download bytes/time and total download/install time,
excluding repository and keyring setup. Archive caching remains disabled.
