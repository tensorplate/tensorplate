# SPDX-License-Identifier: Apache-2.0
#
# The x86_64 serving worker's C++ build configuration. Sourced, never run,
# by every build that produces it: the release workflow's amd64 job,
# tools/release/build-release-artifacts.sh with --arch amd64, and the
# Ubuntu x86_64 CPU-only smoke. A snapshot therefore takes its compiler,
# debug format and backend selection from the same lines as the release.
# test/release/test_build_configuration.py runs both configure commands
# against a stub cmake and requires identical arguments. The streaming
# feature and its vcpkg toolchain file are set here, from VCPKG_ROOT, so
# no caller adds a toolchain file of its own.
#
# TensorRT stays OFF. A hosted runner has no CUDA/TensorRT SDK, and
# building the adapter without one produces a backend that registers,
# passes deploy admission, and only then returns Unsupported at engine
# load. Leaving it out means a TensorRT deploy fails at lookup instead --
# early, and for the real reason. The x86_64 deploy-smoke path is the
# python_pytorch sidecar, which is unconditional and needs no vendor SDK.
#
# -gdwarf-4: clang emits DWARF 5, whose .debug_addr section jammy's dwz
# (0.14) cannot read, and dh_dwz turns that into a hard dpkg-buildpackage
# failure. Pin the DWARF version rather than relying on a compiler
# default, and keep it scoped to this build so the Jetson packaging path
# is untouched.
#
# The compilers go through the environment of the configure command, not
# through -DCMAKE_CXX_COMPILER. Changing that variable on an existing build
# directory makes CMake delete its cache and re-run configure without the
# other -D values of the same invocation, so a stale directory would come
# back without -gdwarf-4, which dh_dwz then rejects, and without the
# version suffix. CMake reads the environment only until a build directory
# records a compiler, which is why the builder refuses a directory
# configured with another compiler.

# shellcheck shell=bash
# shellcheck disable=SC2034

# `.` with no argument hands this file the caller's own: source it with none left.
TP_AMD64_STREAMING=ON
case "$#:${1:-}" in
  0:) ;;
  1:--without-streaming) TP_AMD64_STREAMING=OFF ;;
  *)
    printf 'error: amd64-build-profile.sh takes no argument but --without-streaming; got "%s"\n' "$*" >&2
    return 1
    ;;
esac

# The streaming feature configures only through a vcpkg toolchain file.
if [[ "$TP_AMD64_STREAMING" == ON ]] &&
  [[ -z "${VCPKG_ROOT:-}" || ! -f "${VCPKG_ROOT}/scripts/buildsystems/vcpkg.cmake" ]]; then
  printf 'error: VCPKG_ROOT must name a vcpkg checkout at the builtin-baseline of vcpkg.json; no scripts/buildsystems/vcpkg.cmake under "%s"\n' \
    "${VCPKG_ROOT:-}" >&2
  return 1
fi

TP_AMD64_CC=clang
TP_AMD64_CXX=clang++
TP_AMD64_CMAKE_ARGS=(
  -DCMAKE_CXX_FLAGS=-gdwarf-4
  -DTP_ENABLE_TENSORRT=OFF
  -DTP_REQUIRE_TENSORRT_SDK=OFF
  -DTP_ENABLE_LIBTORCH=OFF
  "-DTP_ENABLE_STREAMING_GRPC=${TP_AMD64_STREAMING}"
  -DTP_ENABLE_PYTHON_PYTORCH_SIDECAR=ON
)
if [[ "$TP_AMD64_STREAMING" == ON ]]; then
  TP_AMD64_CMAKE_ARGS+=(
    -DVCPKG_MANIFEST_FEATURES=streaming-grpc
    -DVCPKG_TARGET_TRIPLET=x64-linux
    "-DCMAKE_TOOLCHAIN_FILE=${VCPKG_ROOT}/scripts/buildsystems/vcpkg.cmake"
  )
fi
# A publishing build restores every dependency from the binary cache or fails.
case "${TP_VCPKG_BINARY_ONLY:-0}:${TP_AMD64_STREAMING}" in
  0:*) ;;
  1:ON) TP_AMD64_CMAKE_ARGS+=(-DVCPKG_INSTALL_OPTIONS=--only-binarycaching) ;;
  1:OFF)
    printf 'error: TP_VCPKG_BINARY_ONLY=1 cannot be combined with --without-streaming\n' >&2
    return 1
    ;;
  *)
    printf 'error: TP_VCPKG_BINARY_ONLY must be 0 or 1; got "%s"\n' "$TP_VCPKG_BINARY_ONLY" >&2
    return 1
    ;;
esac
