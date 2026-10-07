# Streaming dependencies

`TP_ENABLE_STREAMING_GRPC` defaults ON only when discovery validates the
pinned static vcpkg gRPC, protobuf and OpenSSL archives and the two code
generators. Missing dependencies,
shared libraries or non-vcpkg installations default OFF with a status line
explaining why. Explicit OFF skips discovery. Explicit ON treats any failed
validation as a configure error and requires the `streaming-grpc` manifest
feature from the `builtin-baseline` commit in `vcpkg.json`; distribution
gRPC/protobuf packages are not supported for this feature.

```sh
cmake -S . -B build -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake" \
  -DVCPKG_MANIFEST_FEATURES=streaming-grpc \
  -DVCPKG_TARGET_TRIPLET=x64-linux \
  -DTP_ENABLE_STREAMING_GRPC=ON
```

The pinned `x64-linux` and `arm64-linux` triplets explicitly set
`VCPKG_LIBRARY_LINKAGE static`; use `arm64-linux` for a native ARM64 build.
Configure also checks the imported gRPC, protobuf and OpenSSL targets and
their archives: they must be static and inside that vcpkg triplet's install
prefix. This keeps the worker independent of distribution gRPC/protobuf
sonames. Runtime headers expose no gRPC types. The runtime links `gRPC::grpc++`
privately; it reaches the worker through CMake's static-library link closure.
Until the stream listener is implemented, the linker can discard unused
transport objects; this build is not a transport-footprint measurement.

The baseline resolves gRPC 1.81.1, protobuf 6.33.4 (port revision 2), OpenSSL
3.6.4 (port revision 1), GoogleTest 1.18.0 and nlohmann-json 3.12.0 (port
revision 2). The gRPC port supplies its host code generator and host protobuf
from that same baseline. Configure requires `protobuf::protoc` and
`gRPC::grpc_cpp_plugin` to be imported from the vcpkg install tree, and a
feature-ON build generates the draft stream session envelope's C++ bindings
with them into the build tree (target `tp::stream_proto`, see
[`protocol/README.md`](../../protocol/README.md#streaming-session-envelope)).
Generated files are never committed.
Source: the ports and triplets at the
[pinned vcpkg revision](https://github.com/microsoft/vcpkg/tree/f907dc21e0e8699955b002d0fe7673de5db55fab).

For a build with no streaming dependencies, omit `VCPKG_MANIFEST_FEATURES`
and set `-DTP_ENABLE_STREAMING_GRPC=OFF`. Worker config `streaming.enabled`
defaults to false, mirrored by C++ `ServingConfig::streaming` of type
`StreamingConfig` with `bool enabled = false`. Setting it to true in that
build returns `unsupported`.
The feature currently prepares dependencies, config validation and those
bindings, with no stream listener. Unary HTTP and sidecar UDS keep their
existing transports.

Hosted C++ tests enable the feature; adapter-shell tests explicitly disable
it and check both the typed refusal and unary serving. The cold dependency
build has a 120-minute job budget. Release builds set
`TP_ENABLE_STREAMING_GRPC=ON` on both ARM64 and AMD64, and a release that
publishes restores the dependencies from a binary cache for the pinned
baseline, compiler and triplet and fails on a miss; see
[`docs/release/runbook.md`](../../docs/release/runbook.md). vcpkg builds the
ports without the project's `-gdwarf-4`, and the worker compiles against the
manifest's `nlohmann-json` rather than the distribution's package.
Required before the final release tag, though not before a release
candidate: the worker SBOM and vulnerability disposition must cover the
static native closure, including OpenSSL. The supply-chain workflow's
native leg records and scans that closure from the release configuration's
install tree on every pull request; recording it for the worker each
release job builds, and refusing a final tag without that record, is not
done yet. See `docs/release/artifacts.md`, "The native closure".
