// SPDX-License-Identifier: Apache-2.0
//
// Internal runtime skeleton marker. Not part of the public API.
//
// This header exists only so the v0.1.0 scaffolding has at least one symbol
// per CMake target and can be linked end-to-end before V01-E02 lands the
// real value types and ExecutionSession interface.

#pragma once

namespace tensorplate::internal {

// Returns a build-id string identifying the runtime skeleton. Replaced by
// real runtime entrypoints in V01-E02 onwards.
const char* runtime_skeleton_marker() noexcept;

}  // namespace tensorplate::internal
