# TensorPlate root CMake build.
#
# This file establishes the v0.1 C++20 build graph for the runtime, the
# serving worker process, and the C++ test tree. Implementation lands in
# V01-E02 and later; this file only wires the targets and quality gates.

cmake_minimum_required(VERSION 3.25)

project(
  TensorPlate
  VERSION 0.1.0
  DESCRIPTION "TensorPlate edge AI inference platform"
  HOMEPAGE_URL "https://github.com/tensorplate/tensorplate"
  LANGUAGES CXX
)

# Enforce out-of-source builds.
if(CMAKE_SOURCE_DIR STREQUAL CMAKE_BINARY_DIR)
  message(FATAL_ERROR
    "In-source builds are not supported. Run cmake from a separate build/ "
    "directory, e.g. `cmake -S . -B build`.")
endif()

# C++20 across all targets, no compiler-specific extensions.
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# Sensible defaults that affect ergonomics, not behavior.
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
  set(CMAKE_BUILD_TYPE RelWithDebInfo CACHE STRING
      "Build type (Debug, Release, RelWithDebInfo, MinSizeRel)" FORCE)
  set_property(CACHE CMAKE_BUILD_TYPE PROPERTY STRINGS
               Debug Release RelWithDebInfo MinSizeRel)
endif()

list(APPEND CMAKE_MODULE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/cmake/modules")

# Project-wide options.
option(TP_BUILD_TESTS       "Build the C++ test tree" ON)
option(TP_BUILD_SERVING_WORKER
       "Build the tensorplate-serving worker binary" ON)
option(TP_ENABLE_SANITIZERS "Enable ASAN/UBSAN on tp_* targets" OFF)
option(TP_WARNINGS_AS_ERRORS
       "Treat C++ warnings as errors on tp_* targets" OFF)

include(cmake/features/warnings.cmake)
include(cmake/features/sanitizers.cmake)

# Subprojects. Order encodes downward-only layer dependencies.
add_subdirectory(runtime)

if(TP_BUILD_SERVING_WORKER)
  add_subdirectory(serving_worker)
endif()

if(TP_BUILD_TESTS)
  enable_testing()
  add_subdirectory(test)
endif()

# Status banner so misconfigurations are obvious in CI logs.
message(STATUS "TensorPlate ${PROJECT_VERSION}")
message(STATUS "  build type        : ${CMAKE_BUILD_TYPE}")
message(STATUS "  C++ standard      : ${CMAKE_CXX_STANDARD}")
message(STATUS "  tests             : ${TP_BUILD_TESTS}")
message(STATUS "  serving worker    : ${TP_BUILD_SERVING_WORKER}")
message(STATUS "  sanitizers (A/U)  : ${TP_ENABLE_SANITIZERS}")
message(STATUS "  warnings as errors: ${TP_WARNINGS_AS_ERRORS}")
