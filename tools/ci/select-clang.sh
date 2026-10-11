#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Selects the Clang the hosted jobs compile with and fails unless clang and
# clang++ both report it. The format and tidy jobs check their own tool against
# the same version, so their results do not change under a runner image update.
#
# Clang 14 is the runner image's default and the compiler every published
# amd64 worker and both dependency caches were built with. The caches key on
# the resolved compiler binary, so another version opens new keys; moving to
# one is a deliberate change made here and nowhere else. A version registered
# as an alternative at the priority of the image's own never changed the
# choice, which is why the selection is explicit.
#
# Usage: tools/ci/select-clang.sh                  select, then check
#        tools/ci/select-clang.sh --check          check only, as later steps see PATH
#        tools/ci/select-clang.sh --tool <name>    check clang-format or clang-tidy

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

check_tool() {
  local tool=$1 output line="" candidate
  command -v "$tool" >/dev/null || fail "${tool} is not on PATH"
  output="$("$tool" --version)"
  while IFS= read -r candidate; do
    if [[ "$candidate" =~ version\ [0-9]+\. ]]; then
      line=$candidate
      break
    fi
  done <<<"$output"
  printf '%s: %s (%s)\n' "$tool" "$(readlink -f "$(command -v "$tool")")" "$line"
  [[ "$line" =~ version\ ${MAJOR}\. ]] ||
    fail "${tool} is ${line:-of unknown version}, not version ${MAJOR}; the runner image changed it, and so would the formatting and findings it reports"
}

case "$#:${1-}" in
  0:)
    select_clang
    check_clang
    ;;
  1:--check) check_clang ;;
  2:--tool)
    case "$2" in
      clang-format | clang-tidy) check_tool "$2" ;;
      *)
        printf 'usage: select-clang.sh --tool clang-format|clang-tidy\n' >&2
        exit 2
        ;;
    esac
    ;;
  *)
    printf 'usage: select-clang.sh [--check | --tool clang-format|clang-tidy]\n' >&2
    exit 2
    ;;
esac
