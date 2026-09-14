# SPDX-License-Identifier: Apache-2.0
#
# The x86_64 serving worker's C++ build configuration. Sourced, never run,
# by every build that produces it: the release workflow's amd64 job,
# tools/release/build-release-artifacts.sh with --arch amd64, and the
# Ubuntu x86_64 CPU-only smoke. A snapshot therefore takes its compiler,
# debug format and backend selection from the same lines as the release.
# test/release/test_build_configuration.py runs both configure commands
# against a stub cmake and requires identical arguments.
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
# back with TensorRT ON and no version suffix. CMake reads the environment
# only when a build directory is first configured, which is why the
# builder refuses a directory configured with another compiler.

# shellcheck shell=bash
# shellcheck disable=SC2034

TP_AMD64_CC=clang
TP_AMD64_CXX=clang++
TP_AMD64_CMAKE_ARGS=(
  -DCMAKE_CXX_FLAGS=-gdwarf-4
  -DTP_ENABLE_TENSORRT=OFF
  -DTP_REQUIRE_TENSORRT_SDK=OFF
  -DTP_ENABLE_LIBTORCH=OFF
  -DTP_ENABLE_PYTHON_PYTORCH_SIDECAR=ON
)
