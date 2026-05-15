// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/http/http_message.hpp"

#include <algorithm>
#include <cctype>
#include <string>
#include <utility>

namespace tensorplate::http {

std::optional<std::string_view> Request::header(std::string_view name) const noexcept {
  for (const auto& h : headers) {
    if (h.name == name) {
      return std::string_view{h.value};
    }
  }
  return std::nullopt;
}

void Response::set_header(std::string_view name, std::string value) {
  // Header names are stored lowercase to match the request side.
  std::string lname = lower_ascii(name);
  for (auto& h : headers) {
    if (h.name == lname) {
      h.value = std::move(value);
      return;
    }
  }
  headers.push_back(Header{std::move(lname), std::move(value)});
}

Response Response::ok_json(std::string body) noexcept {
  Response r;
  r.status = 200;
  r.set_header("content-type", "application/json");
  r.body = std::move(body);
  return r;
}

Response Response::json(int status, std::string body) noexcept {
  Response r;
  r.status = status;
  r.set_header("content-type", "application/json");
  r.body = std::move(body);
  return r;
}

Response Response::plain(int status, std::string body) noexcept {
  Response r;
  r.status = status;
  r.set_header("content-type", "text/plain; charset=utf-8");
  r.body = std::move(body);
  return r;
}

std::string_view http_reason(int status) noexcept {
  switch (status) {
    case 200:
      return "OK";
    case 202:
      return "Accepted";
    case 204:
      return "No Content";
    case 400:
      return "Bad Request";
    case 401:
      return "Unauthorized";
    case 404:
      return "Not Found";
    case 405:
      return "Method Not Allowed";
    case 408:
      return "Request Timeout";
    case 409:
      return "Conflict";
    case 413:
      return "Payload Too Large";
    case 415:
      return "Unsupported Media Type";
    case 422:
      return "Unprocessable Entity";
    case 429:
      return "Too Many Requests";
    case 500:
      return "Internal Server Error";
    case 501:
      return "Not Implemented";
    case 503:
      return "Service Unavailable";
    case 504:
      return "Gateway Timeout";
    default:
      return "OK";
  }
}

std::string lower_ascii(std::string_view in) {
  std::string out;
  out.reserve(in.size());
  for (char c : in) {
    if (c >= 'A' && c <= 'Z') {
      c = static_cast<char>(c + ('a' - 'A'));
    }
    out.push_back(c);
  }
  return out;
}

}  // namespace tensorplate::http
