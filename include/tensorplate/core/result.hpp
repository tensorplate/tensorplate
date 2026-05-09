// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01: Result<T> alias used by fallible runtime interfaces.
//
// Result<T> mirrors the C++23 std::expected interface so the runtime can
// transition to std::expected with no API churn once the codebase moves to
// C++23. Until then, this self-contained implementation avoids vendor SDK
// dependencies in the public runtime headers.
//
// Construction:
//   tensorplate::Result<int> ok_v   = 42;
//   tensorplate::Result<int> err_v  = tensorplate::unexpected(
//       tensorplate::Error::make(tensorplate::Error::Code::Internal, "boom"));
//
// Inspection:
//   if (r) { use(r.value()); } else { log(r.error()); }

#pragma once

#include <exception>
#include <optional>
#include <type_traits>
#include <utility>
#include <variant>

#include "tensorplate/core/error.hpp"

namespace tensorplate {

/// Tagged wrapper for an error-state Result<T>. Use the free function
/// tensorplate::unexpected() rather than constructing this directly.
struct Unexpected {
  Error error;
};

/// Build an error-state Result<T> from an Error value.
[[nodiscard]] inline Unexpected unexpected(Error err) noexcept {
  return Unexpected{std::move(err)};
}

/// Build an error-state Result<T> from a code/message pair.
[[nodiscard]] inline Unexpected unexpected(Error::Code code, std::string message) {
  return Unexpected{Error::make(code, std::move(message))};
}

/// Thrown by Result<T>::value() / Result<T>::error() when called on the wrong
/// state. Calling .value() on an error Result is a programmer error,
/// equivalent to dereferencing a default-constructed std::optional. The
/// runtime never throws this from hardware-boundary operations; see the
/// architecture guideline on Result<T>-vs-exceptions.
class BadResultAccess : public std::exception {
 public:
  const char* what() const noexcept override { return "tensorplate::BadResultAccess"; }
};

/// Result<T> is the standard return type for fallible TensorPlate operations.
///
/// Semantics:
///   - Holds either a T (success) or an Error (failure) by value.
///   - No vendor SDK dependency; only the C++ standard library.
///   - Equality compares the held variant alternative.
///
/// This type is explicitly [[nodiscard]] so that hardware-boundary callers
/// cannot drop the success/failure signal.
template <typename T>
class [[nodiscard]] Result {
  static_assert(!std::is_reference_v<T>, "Result<T&> is not supported");
  static_assert(!std::is_same_v<std::remove_cv_t<T>, Error>,
                "Result<Error> is ambiguous; use tensorplate::Error directly");

 public:
  using value_type = T;

  Result(const T& value) : storage_(std::in_place_index<0>, value) {}  // NOLINT
  Result(T&& value) noexcept(std::is_nothrow_move_constructible_v<T>)  // NOLINT
      : storage_(std::in_place_index<0>, std::move(value)) {}
  Result(Unexpected u) noexcept : storage_(std::in_place_index<1>, std::move(u.error)) {}  // NOLINT

  Result(const Result&) = default;
  Result(Result&&) noexcept = default;
  Result& operator=(const Result&) = default;
  Result& operator=(Result&&) noexcept = default;
  ~Result() = default;

  [[nodiscard]] bool has_value() const noexcept { return storage_.index() == 0; }
  [[nodiscard]] explicit operator bool() const noexcept { return has_value(); }

  T& value() & {
    if (!has_value())
      throw BadResultAccess{};
    return std::get<0>(storage_);
  }
  const T& value() const& {
    if (!has_value())
      throw BadResultAccess{};
    return std::get<0>(storage_);
  }
  T&& value() && {
    if (!has_value())
      throw BadResultAccess{};
    return std::move(std::get<0>(storage_));
  }

  Error& error() & {
    if (has_value())
      throw BadResultAccess{};
    return std::get<1>(storage_);
  }
  const Error& error() const& {
    if (has_value())
      throw BadResultAccess{};
    return std::get<1>(storage_);
  }
  Error&& error() && {
    if (has_value())
      throw BadResultAccess{};
    return std::move(std::get<1>(storage_));
  }

  template <typename U>
  T value_or(U&& default_value) const& {
    if (has_value())
      return std::get<0>(storage_);
    return T{std::forward<U>(default_value)};
  }

  T* operator->() { return &value(); }
  const T* operator->() const { return &value(); }
  T& operator*() & { return value(); }
  const T& operator*() const& { return value(); }
  T&& operator*() && { return std::move(value()); }

  friend bool operator==(const Result& lhs, const Result& rhs) {
    return lhs.storage_ == rhs.storage_;
  }
  friend bool operator!=(const Result& lhs, const Result& rhs) { return !(lhs == rhs); }

 private:
  std::variant<T, Error> storage_;
};

/// Specialization for Result<void> (operations that return nothing on success).
template <>
class [[nodiscard]] Result<void> {
 public:
  using value_type = void;

  Result() noexcept = default;
  Result(Unexpected u) noexcept : error_(std::move(u.error)) {}  // NOLINT

  [[nodiscard]] bool has_value() const noexcept { return !error_.has_value(); }
  [[nodiscard]] explicit operator bool() const noexcept { return has_value(); }

  void value() const {
    if (!has_value())
      throw BadResultAccess{};
  }

  Error& error() & {
    if (has_value())
      throw BadResultAccess{};
    return *error_;
  }
  const Error& error() const& {
    if (has_value())
      throw BadResultAccess{};
    return *error_;
  }
  Error&& error() && {
    if (has_value())
      throw BadResultAccess{};
    return std::move(*error_);
  }

  friend bool operator==(const Result& lhs, const Result& rhs) noexcept {
    return lhs.error_ == rhs.error_;
  }
  friend bool operator!=(const Result& lhs, const Result& rhs) noexcept { return !(lhs == rhs); }

 private:
  std::optional<Error> error_;
};

}  // namespace tensorplate
