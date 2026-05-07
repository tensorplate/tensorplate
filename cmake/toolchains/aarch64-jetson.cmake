# Cross toolchain stub for NVIDIA Jetson Orin (aarch64) targets.
#
# v0.1 does not ship a vendored cross toolchain. Contributors point this
# file at a JetPack-provided sysroot and aarch64 GCC by setting:
#
#   TP_JETSON_SYSROOT     - absolute path to the Jetson sysroot (with
#                            /usr/include, /usr/lib/aarch64-linux-gnu, etc.)
#   TP_JETSON_CC          - aarch64 C compiler (e.g., aarch64-linux-gnu-gcc-11)
#   TP_JETSON_CXX         - aarch64 C++ compiler (e.g., aarch64-linux-gnu-g++-11)
#
# Then configure with:
#
#   cmake -S . -B build-jetson \
#     -DCMAKE_TOOLCHAIN_FILE=$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake \
#     -DVCPKG_CHAINLOAD_TOOLCHAIN_FILE=$PWD/cmake/toolchains/aarch64-jetson.cmake \
#     -DVCPKG_TARGET_TRIPLET=arm64-linux \
#     -G Ninja
#
# TensorRT, CUDA, and JetPack libraries are NOT vendored. They must already
# exist inside the sysroot. See docs/architecture/ownership.md and
# CONTRIBUTING.md for the supported cross-compile workflow.

set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)

if(NOT DEFINED TP_JETSON_SYSROOT AND DEFINED ENV{TP_JETSON_SYSROOT})
  set(TP_JETSON_SYSROOT "$ENV{TP_JETSON_SYSROOT}")
endif()
if(NOT DEFINED TP_JETSON_CC AND DEFINED ENV{TP_JETSON_CC})
  set(TP_JETSON_CC "$ENV{TP_JETSON_CC}")
endif()
if(NOT DEFINED TP_JETSON_CXX AND DEFINED ENV{TP_JETSON_CXX})
  set(TP_JETSON_CXX "$ENV{TP_JETSON_CXX}")
endif()

if(NOT TP_JETSON_SYSROOT OR NOT TP_JETSON_CC OR NOT TP_JETSON_CXX)
  message(FATAL_ERROR
    "aarch64-jetson toolchain requires TP_JETSON_SYSROOT, TP_JETSON_CC, "
    "and TP_JETSON_CXX to be set (env vars or -D options). See "
    "cmake/toolchains/aarch64-jetson.cmake for usage.")
endif()

set(CMAKE_SYSROOT "${TP_JETSON_SYSROOT}")
set(CMAKE_C_COMPILER   "${TP_JETSON_CC}")
set(CMAKE_CXX_COMPILER "${TP_JETSON_CXX}")

set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
