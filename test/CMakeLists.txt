# C++ test tree. Tier-organized so PR-gated tests stay fast and
# device/release-only tiers remain off the default path.
#
# T1 unit:        every PR.
# T2 integration: every PR (added when cross-layer code lands).
# T3 contract:    nightly / release.
# T4 hil:         release branch only; requires hardware.
# T5 benchmark:   release branch only.

# GoogleTest is provided by vcpkg (see vcpkg.json) for T1/T2.
find_package(GTest CONFIG QUIET)

if(NOT TARGET GTest::gtest_main)
  # Fall back to module mode in case the developer installs GoogleTest via a
  # system package manager rather than vcpkg.
  find_package(GTest QUIET MODULE)
endif()

if(NOT TARGET GTest::gtest_main)
  message(WARNING
    "GoogleTest not found. Skipping T1/T2 C++ test targets. "
    "Run via the vcpkg toolchain (see CONTRIBUTING.md) to enable them.")
  return()
endif()

include(GoogleTest)

add_subdirectory(unit)

# T2/T3/T4/T5 subdirectories are added when their first test lands.
