# Host toolchain for x86_64 Linux developer builds.
#
# Use as a vcpkg chainload toolchain so vcpkg picks the same compiler that
# the project sees:
#
#   cmake -S . -B build \
#     -DCMAKE_TOOLCHAIN_FILE=$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake \
#     -DVCPKG_CHAINLOAD_TOOLCHAIN_FILE=$PWD/cmake/toolchains/x86_64-linux-gnu.cmake \
#     -G Ninja
#
# This file deliberately keeps host paths out of source control. Compilers
# are picked up from PATH or the standard CMake variables.

set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR x86_64)

# Default to the system compiler discovered on PATH unless the developer
# overrides via CC / CXX env vars or -DCMAKE_C_COMPILER.
if(NOT CMAKE_C_COMPILER AND DEFINED ENV{CC})
  set(CMAKE_C_COMPILER "$ENV{CC}")
endif()
if(NOT CMAKE_CXX_COMPILER AND DEFINED ENV{CXX})
  set(CMAKE_CXX_COMPILER "$ENV{CXX}")
endif()
