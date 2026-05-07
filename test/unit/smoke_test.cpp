// SPDX-License-Identifier: Apache-2.0
//
// V01-E01-F02-T03 smoke test. Exists only to prove that the C++ build,
// link, GoogleTest discovery, and CTest wiring all work end to end before
// any real runtime logic lands.
//
// Real T1 tests for the runtime value types arrive in V01-E02.

#include <gtest/gtest.h>

#include <string_view>

#include "tensorplate/internal/skeleton.hpp"

TEST(RuntimeSkeleton, MarkerIsStable) {
  EXPECT_EQ(std::string_view(tensorplate::internal::runtime_skeleton_marker()),
            "tensorplate-runtime-skeleton");
}
