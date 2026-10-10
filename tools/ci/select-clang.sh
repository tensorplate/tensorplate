#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Selects the Clang the hosted jobs compile with and fails unless clang and
# clang++ both report it.
#
# Clang 14 is the runner image's default and the compiler every published
# amd64 worker and both dependency caches were built with. The caches key on
# the resolved compiler binary, so another version opens new keys; moving to
# one is a deliberate change made here and nowhere else. A version registered
# as an alternative at the priority of the image's own never changed the
# choice, which is why the selection is explicit.
#
# Usage: tools/ci/select-clang.sh          select, then check
#        tools/ci/select-clang.sh --check  check only, as later steps see PATH

set -Eeuo pipefail

readonly MAJOR=14

fail() {
  printf '::error::%s\n' "$*"
  exit 1
}

select_clang() {
  local tool target
  for tool in clang clang++; do
    target="/usr/bin/${tool}-${MAJOR}"
    if ! sudo update-alternatives --install "/usr/bin/${tool}" "$tool" "$target" 100 ||
      ! sudo update-alternatives --set "$tool" "$target"; then
      fail "could not select ${target}: the runner image no longer ships Clang ${MAJOR}, and the dependency caches are keyed on it"
    fi
  done
}

check_clang() {
  local tool output version
  for tool in clang clang++; do
    command -v "$tool" >/dev/null || fail "${tool} is not on PATH"
    output="$("$tool" --version)"
    version="${output%%$'\n'*}"
    printf '%s: %s (%s)\n' "$tool" "$(readlink -f "$(command -v "$tool")")" "$version"
    [[ "$version" =~ clang\ version\ ${MAJOR}\. ]] ||
      fail "${tool} is ${version}, not Clang ${MAJOR}; the runner image changed its compiler, which opens new dependency cache keys"
  done
}

case "$#:${1-}" in
  0:)
    select_clang
    check_clang
    ;;
  1:--check) check_clang ;;
  *)
    printf 'usage: select-clang.sh [--check]\n' >&2
    exit 2
    ;;
esac
