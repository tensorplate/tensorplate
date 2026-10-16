# Private TensorRT SDK loader

`runtime/src/adapters/tensorrt/sdk_loader.hpp` is internal to the adapter and
its tests. It does not change registration, public headers, error codes or the
session lifecycle. The adapter does not yet consume it: SDK-enabled workers
still link NVIDIA libraries directly, and the amd64 release profile still
sets TensorRT OFF. Jetson retains its existing SDK and direct linkage.

On Linux x86_64, the separate `tp_tensorrt_loader` target compiles with TensorRT
and CUDA headers and the platform dynamic-loader library. It does not link
nvinfer or cudart. CUDA's CRT headers are needed too. Exact CI package pins
live only in `tools/ci/tensorrt-sdk-versions.sh`.

`SdkLibraries::load()` opens the CUDA and TensorRT sonames for the header
majors using `RTLD_NOW | RTLD_LOCAL`. It resolves the C factory
`createInferRuntime_INTERNAL`, `getInferLibVersion`, and the CUDA runtime
version, driver/device query, error-string, allocation, stream and copy
functions. The SDK declarations supply the function-pointer types. The inline
`createInferRuntime` wrapper is not a symbol to resolve; a future caller must
pass its logger and `NV_TENSORRT_VERSION` to the internal C factory.

Missing libraries or exports return `Unsupported`, naming the failing item.
The TensorRT major/minor/patch and CUDA major/minor must match the headers;
these checks do not establish engine compatibility or compare package build
revisions. Mismatches return `Unsupported`; a failed CUDA runtime-version
query returns `LoadFailed`. No device/driver probe or factory call occurs here.
A successful load means the function table is available, not that inference
is usable. Driver preflight and session integration remain separate work.

The returned immutable shared owner keeps both library handles alive. Its
functions must never outlive that owner: retain it until every vendor object,
stream and device buffer has been destroyed, including on early returns.
TensorRT closes before CUDA. Partial loads close acquired handles. There is no
process-global cache or eager startup initialization; each load owns its own
loader references. `LibraryOps` is a private synthetic-test seam, not a user
configuration surface; supplied operations must be valid for the owner's life.

A separate unit executable tests absence of each library/export, both version
checks, version-query failure, and ownership through the last shared reference.
The SDK job also exercises the real POSIX loader with the pinned packages and
no GPU. It never calls the TensorRT factory. Worker startup without NVIDIA
libraries, safe driverless adapter load, and successful GPU inference are not
claims of these tests.
