# Private TensorRT SDK loader

`runtime/src/adapters/tensorrt/sdk_loader.hpp` is internal to the adapter and
its tests. It does not change registration, public headers, error codes or the
session lifecycle. SDK-enabled Linux x86_64 workers use it at session load, so
they can start and serve Python without NVIDIA libraries. The amd64 release
profile still sets TensorRT OFF; the shipped worker is unchanged. Jetson retains
its TensorRT 10.3 / CUDA 12.6 SDK and direct linkage through the same function
table.

On Linux x86_64, the separate `tp_tensorrt_loader` target compiles with TensorRT
and CUDA headers and the platform dynamic-loader library. It does not link
nvinfer or cudart. CUDA's CRT headers are needed too. Exact CI package pins live
only in `tools/ci/tensorrt-sdk-versions.sh`.

`SdkLibraries::load()` opens the CUDA and TensorRT sonames for the header majors
using `RTLD_NOW | RTLD_LOCAL`. It resolves the C factory
`createInferRuntime_INTERNAL`, `getInferLibVersion`, and the CUDA runtime
version, driver/device query, error-string, allocation, stream and copy
functions. The SDK declarations supply the function-pointer types. The inline
`createInferRuntime` wrapper is not a symbol to resolve; the adapter passes its
logger and `NV_TENSORRT_VERSION` to the internal C factory. TensorRT object
methods and destruction dispatch through SDK objects; every CUDA call, including
RAII cleanup, uses the table.

Missing libraries or exports return `Unsupported`, naming the failing item.
Library-open failures include the dynamic loader's diagnostic, distinguishing
absence from an installed library with a missing dependency or incompatible
format. The TensorRT major/minor/patch and CUDA major/minor must match the
headers; these checks do not establish engine compatibility or compare package
build revisions. Mismatches return `Unsupported`; a failed CUDA runtime-version
query returns `LoadFailed`. After library/export and runtime-version validation,
each adapter load probes `cudaDriverGetVersion`, then `cudaGetDeviceCount`, then
calls the factory. A failed status, non-positive driver version or non-positive
device count returns `LoadFailed` naming the probe and its status/value, before
the factory. A null factory result also returns `LoadFailed`. The session
remains not ready; no public error enum or protocol changes. These probes do not
prove GPU inference.

The returned immutable shared owner keeps both library handles alive. Its
functions must never outlive that owner: retain it until every vendor object,
stream and device buffer has been destroyed, including on early returns. Partial
loads close acquired handles, TensorRT before CUDA. The adapter uses
`acquire()`: a lazy, mutex-protected process-lifetime cache of successful loads.
Failed loads are not cached. Session unload destroys its objects and buffers,
but successful library handles remain mapped until OS process teardown,
including through static destruction and sanitizer checks. A driverless refusal
leaves these libraries loaded and cached: only runtime construction is refused.
The cached real-SDK test peaks at approximately 241 MB max RSS versus 9 MB without
the SDK, about 232 MB resident per process holding the owner. This retention
avoids repeating a vendor leak of exactly 3 x 72,704 bytes per load/unload cycle:
the libstdc++ exception emergency pools from the statically linked libstdc++
copies inside the vendor modules are orphaned by `dlclose`. `load()` remains the
uncached primitive for isolated loader-lifetime tests. `LibraryOps`
is a private synthetic-test seam, not a user configuration surface; supplied
operations must be valid for the owner's life.

A separate unit executable tests absence of each library/export, both version
checks, version-query failure, and ownership through the last shared reference.
The SDK job also exercises the real POSIX loader with the pinned packages and no
GPU, then uses `RTLD_NOLOAD` to verify handles were released unless already
mapped before the test. Injected preflight tests record every probe/factory call
and are mutation-checked. The
SDK job requires a non-skipped real driverless load of a non-empty artifact,
checks that libnvinfer remains mapped after session destruction, and
copies the worker to clean Ubuntu 22.04/24.04 containers: no NVIDIA ELF
references, mandatory Python backend import and a real fixture-sidecar response,
then TensorRT `Unsupported`. This is transport evidence, not model accuracy.
Real-SDK ASan/UBSan runs keep leak detection enabled with only NVIDIA-module
suppressions in `tools/ci/tensorrt-lsan.supp`. They exercise cached ownership;
the uncached real dlclose test runs without sanitizers because unloading removes
module attribution. A GPU-conditional malformed-engine test requires the
deserialization error after successful preflight. Successful GPU inference
remains hardware validation.
