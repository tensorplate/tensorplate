// SPDX-License-Identifier: Apache-2.0
//
// Typed error code -> HTTP status mapping used by the serving router.

#include <gtest/gtest.h>

#include <string_view>

#include "tensorplate/core/error.hpp"
#include "tensorplate/http/http_message.hpp"

#include "serving/error_status.hpp"

namespace {

using tensorplate::Error;
using tensorplate::serving::http_status_for_error;

TEST(ServingErrorStatus, EveryCodeHasItsStatus) {
  EXPECT_EQ(http_status_for_error(Error::Code::ConfigInvalid), 400);
  EXPECT_EQ(http_status_for_error(Error::Code::ShapeMismatch), 400);
  EXPECT_EQ(http_status_for_error(Error::Code::Unsupported), 415);
  EXPECT_EQ(http_status_for_error(Error::Code::OOMError), 429);
  EXPECT_EQ(http_status_for_error(Error::Code::Timeout), 504);
  EXPECT_EQ(http_status_for_error(Error::Code::NotReady), 503);
  EXPECT_EQ(http_status_for_error(Error::Code::LoadFailed), 500);
  EXPECT_EQ(http_status_for_error(Error::Code::InferenceFailed), 500);
  EXPECT_EQ(http_status_for_error(Error::Code::Internal), 500);
  EXPECT_EQ(http_status_for_error(Error::Code::Cancelled), 499);
  EXPECT_EQ(http_status_for_error(Error::Code::Unavailable), 503);
  EXPECT_EQ(http_status_for_error(Error::Code::ResourceExhausted), 429);
}

TEST(ServingErrorStatus, EveryMappedStatusHasAReasonPhrase) {
  // The status line is built from http_reason, whose fallback is "OK"; a
  // mapped error status must never go out as "<status> OK".
  for (auto code :
       {Error::Code::ConfigInvalid, Error::Code::LoadFailed, Error::Code::NotReady,
        Error::Code::ShapeMismatch, Error::Code::Unsupported, Error::Code::OOMError,
        Error::Code::Timeout, Error::Code::InferenceFailed, Error::Code::Internal,
        Error::Code::Cancelled, Error::Code::Unavailable, Error::Code::ResourceExhausted}) {
    const int status = http_status_for_error(code);
    EXPECT_NE(tensorplate::http::http_reason(status), std::string_view{"OK"})
        << "status " << status << " for " << tensorplate::to_string(code);
  }
  EXPECT_EQ(tensorplate::http::http_reason(499), std::string_view{"Client Closed Request"});
}

}  // namespace
