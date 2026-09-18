#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests test/packaging/run.sh's groups against stub verifiers: each group
# runs exactly its own verifiers, host-mutating verifiers never run, an
# unknown group is a usage error, and a verifier in no group fails the
# suite before anything runs. Also checks that every verifier the real
# run.sh names exists, so a renamed file cannot leave a stale entry.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
real="${repo_root}/test/packaging/run.sh"

failures=0
check() {
  local what="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    printf '  ok   %s\n' "$what"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$what" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

# The names in one of run.sh's newline-separated lists, space-joined.
group_names() {
  awk -v name="$1" '
    $0 ~ "^" name "=\"" { on = 1; sub("^" name "=\"", "") }
    on {
      done_ = sub(/"$/, "")
      if (length($0)) printf "%s%s", sep, $0; sep = " "
      if (done_) exit
    }
  ' "$real"
}

core="$(group_names core)"
harness="$(group_names harness)"
host_mutating="$(group_names host_mutating)"

check "run.sh declares core verifiers" yes "$([[ -n "$core" ]] && echo yes || echo no)"
check "run.sh declares harness verifiers" yes "$([[ -n "$harness" ]] && echo yes || echo no)"

missing=""
for name in $core $harness $host_mutating; do
  [[ -f "${repo_root}/test/packaging/${name}" ]] || missing="${missing} ${name}"
done
check "every verifier run.sh names exists" "" "$missing"

td="$(mktemp -d)"
trap 'rm -rf "$td"' EXIT

stage() {
  local dir="$1"
  rm -rf "$dir"
  mkdir -p "$dir"
  cp "$real" "${dir}/run.sh"
  for name in $core $harness $host_mutating; do
    # The stub expands RUN_LOG when it runs, not here.
    # shellcheck disable=SC2016
    printf '#!/bin/sh\nprintf "%%s\\n" %s >>"$RUN_LOG"\n' "$name" >"${dir}/${name}"
    chmod +x "${dir}/${name}"
  done
}

# Runs the staged suite; prints its exit status, then the verifiers it ran.
drive() {
  local dir="$1"
  shift
  : >"${dir}/ran.log"
  local status=0
  RUN_LOG="${dir}/ran.log" sh "${dir}/run.sh" "$@" >"${dir}/out.log" 2>&1 || status=$?
  printf '%s|%s' "$status" "$(tr '\n' ' ' <"${dir}/ran.log" | sed 's/ $//')"
}

suite="${td}/suite"
stage "$suite"
check "core runs exactly the core verifiers" "0|${core}" "$(drive "$suite" core)"
check "harness runs exactly the harness verifiers" "0|${harness}" "$(drive "$suite" harness)"
check "no argument runs core, then harness" "0|${core} ${harness}" "$(drive "$suite")"
check "all is the same as no argument" "0|${core} ${harness}" "$(drive "$suite" all)"
check "an unknown group is a usage error that runs nothing" "2|" "$(drive "$suite" everything)"

for name in $host_mutating; do
  check "host-mutating ${name} is never run" no \
    "$(drive "$suite" >/dev/null; grep -Fqx "$name" "${suite}/ran.log" && echo yes || echo no)"
done

cp "${suite}/verify_layout.sh" "${suite}/verify_unlisted_new.sh"
check "a verifier in no group fails the suite before anything runs" "1|" "$(drive "$suite" core)"
check "  and names the verifier" yes \
  "$(grep -Fq 'verify_unlisted_new.sh' "${suite}/out.log" && echo yes || echo no)"

failing="${td}/failing"
stage "$failing"
first_core="${core%% *}"
printf '#!/bin/sh\nexit 7\n' >"${failing}/${first_core}"
check "a failing verifier stops the suite with its status" "7|" "$(drive "$failing" core)"

if [[ "$failures" -eq 0 ]]; then
  printf 'run_groups_test: ok\n'
else
  printf 'run_groups_test: %s check(s) failed\n' "$failures"
fi
exit "$failures"
