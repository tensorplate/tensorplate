// SPDX-License-Identifier: Apache-2.0
//
// Typed error code -> HTTP status mapping for the serving router. Private to
// the runtime; declared here so the mapping can be tested directly.

#pragma once

#include "tensorplate/core/error.hpp"

namespace tensorplate::serving {

/// HTTP status the router returns for an error that ends a request before or
/// instead of an inference result. `cancelled`, `unavailable` and
/// `resource_exhausted` take the HTTP mapping google.rpc.Code documents for
/// CANCELLED (499), UNAVAILABLE (503) and RESOURCE_EXHAUSTED (429)
/// (googleapis, google/rpc/code.proto); the older codes keep their statuses.
[[nodiscard]] int http_status_for_error(Error::Code code) noexcept;

}  // namespace tensorplate::serving
