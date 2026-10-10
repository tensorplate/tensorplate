// SPDX-License-Identifier: Apache-2.0
#pragma once

// The synthetic session dispatch the stream listener tests run behind the
// listener: the one place the tests name it.

#include "serving/session/synthetic_dispatch.hpp"

namespace tensorplate::testing {
using StreamTestDispatch = serving::SyntheticSessionDispatch;
}  // namespace tensorplate::testing
