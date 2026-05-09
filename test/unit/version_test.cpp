// SPDX-License-Identifier: Apache-2.0
//
// V01-E01-F06-T01 unit coverage. Verifies that the four versioning surfaces
// exposed by include/tensorplate/version.hpp are all populated and that the
// composed strings agree with the component fields.

#include "tensorplate/version.hpp"

#include <gtest/gtest.h>

#include <string>

namespace {

std::string compose_runtime() {
  std::string base = std::to_string(tensorplate::kRuntimeVersionMajor) + '.' +
                     std::to_string(tensorplate::kRuntimeVersionMinor) + '.' +
                     std::to_string(tensorplate::kRuntimeVersionPatch);
  if (!tensorplate::kRuntimeVersionSuffix.empty()) {
    base += '-';
    base += tensorplate::kRuntimeVersionSuffix;
  }
  return base;
}

std::string compose_two(std::uint32_t major, std::uint32_t minor) {
  return std::to_string(major) + '.' + std::to_string(minor);
}

}  // namespace

TEST(Version, RuntimeStringMatchesComponents) {
  EXPECT_EQ(compose_runtime(), std::string(tensorplate::kRuntimeVersion));
}

TEST(Version, ProtocolStringMatchesComponents) {
  EXPECT_EQ(compose_two(tensorplate::kProtocolVersionMajor, tensorplate::kProtocolVersionMinor),
            std::string(tensorplate::kProtocolVersion));
}

TEST(Version, BundleFormatStringMatchesComponents) {
  EXPECT_EQ(
      compose_two(tensorplate::kBundleFormatVersionMajor, tensorplate::kBundleFormatVersionMinor),
      std::string(tensorplate::kBundleFormatVersion));
}

TEST(Version, RuntimeIsNonZero) {
  // Once the runtime ships any non-pre-release artifact, kRuntimeVersionMajor
  // becomes 1 or higher. Until then, v0.x is allowed but the string must
  // still be populated.
  EXPECT_FALSE(std::string(tensorplate::kRuntimeVersion).empty());
}
