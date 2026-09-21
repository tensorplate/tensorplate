#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# packaging: shared durable-state preservation guard verifier.
#
# The rollback stage of the Ubuntu x86_64 cloud harness and of the Jetson
# harness make the same claim in the same words -- that the documented
# rollback preserves the operator's durable state, file by file, against
# digests taken from the stopped agent's own directory before the move --
# and they make it with the same five functions: privileged_sha256,
# state_manifest, manifest_digest, capture_state_manifest and
# check_state_preserved.
#
# Two harnesses making one claim by copied code drift. Only one of them
# is driven against a stubbed appliance for any given regression, so a
# fix or a tightening applied to one leaves the other's evidence quietly
# weaker while still reading as if it were not -- and Jetson and the L4
# are two of the four Production rows whose evidence the release rests
# on. Nothing else reads both files: each harness verifier reads only its
# own harness.
#
# So this binds them. The five function bodies must be byte-identical in
# both harnesses, and every harness variable those bodies read must be
# defined identically in both. The variables are DERIVED from the bodies
# rather than listed here, so a body that starts reading something new
# cannot outgrow the binding silently: the new name is required to be
# bound too, in both files, or this fails.
#
# What that buys: verify_ubuntu_l4_cloud_lifecycle.sh drives this code
# against a stubbed appliance through every way the rollback can destroy
# state, and this says the Jetson harness holds that same code. The
# coverage is one harness's, and it reaches both.
#
# It runs in the core group rather than the harness group because it
# drives no appliance and takes no measurable time. A copy-paste
# divergence should fail in the fast job, not fifteen minutes into the
# slow one.
#
# The comments ABOVE each function are deliberately not compared. They
# name facts that legitimately differ -- the L4's boot-bound machine-type
# record has no Jetson counterpart, and one says host where the other
# says device. The bodies are what make the claim.
#
# Runs under bash 3.2: no readarray, no associative arrays. Statuses are
# captured explicitly rather than left to errexit, which bash suspends
# inside a function called from a tested context.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
cloud="${repo_root}/tools/validation/ubuntu-l4-cloud-lifecycle.sh"
jetson="${repo_root}/tools/validation/jetson-lifecycle.sh"

guard_functions="privileged_sha256
state_manifest
manifest_digest
capture_state_manifest
check_state_preserved"

failures=0
checks=0
check() {
  local what="$1" expected="$2" actual="$3"
  checks=$((checks + 1))
  if [[ "$expected" == "$actual" ]]; then
    printf '  ok   %s\n' "$what"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$what" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

for harness in "$cloud" "$jetson"; do
  check "$(basename "$harness") is there" yes \
    "$([[ -f "$harness" ]] && echo yes || echo no)"
done
if ((failures > 0)); then
  printf '\n%s of %s check(s) failed\n' "$failures" "$checks"
  exit "$failures"
fi

td="$(mktemp -d)"
trap 'rm -rf "$td"' EXIT

# --- the five bodies, cut out of each harness and compared.
#
# A body runs from its `name() {` line to the next line that is exactly
# `}`, which is this repo's shell style throughout: every function opens
# at column zero and closes at column zero, and nothing inside these five
# closes a brace there. That assumption is not taken on trust -- each
# extracted body is handed to `bash -n` below, so a cut that ended in the
# wrong place fails here rather than silently comparing fragments.
#
# A function defined zero times, twice, or never closed is an error
# rather than an empty comparison: two harnesses that have both lost the
# guard must not read as two harnesses that agree.
extract() {
  python3 - "$1" "$2" "$guard_functions" <<'PY'
import sys

path, out_dir, wanted = sys.argv[1:]
lines = open(path, encoding="utf-8").read().split("\n")
for name in wanted.split():
    opens = [i for i, line in enumerate(lines) if line == f"{name}() {{"]
    if len(opens) != 1:
        raise SystemExit(
            f"{path}: {name} is defined {len(opens)} times, expected exactly once"
        )
    start = opens[0]
    closes = [j for j in range(start + 1, len(lines)) if lines[j] == "}"]
    if not closes:
        raise SystemExit(f"{path}: {name} is never closed at column zero")
    with open(f"{out_dir}/{name}", "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines[start:closes[0] + 1]) + "\n")
PY
}

parses() {
  local status=0
  bash -n "$1" 2>/dev/null || status=$?
  if ((status == 0)); then printf 'ok'; else printf 'unparsable'; fi
}

mkdir -p "${td}/cloud" "${td}/jetson"
extract "$cloud" "${td}/cloud"
extract "$jetson" "${td}/jetson"

while IFS= read -r name; do
  [[ -n "$name" ]] || continue
  # The cut is a whole function, not a fragment of one.
  check "${name}'s body parses on its own in both" "ok ok" \
    "$(printf '%s %s' "$(parses "${td}/cloud/${name}")" "$(parses "${td}/jetson/${name}")")"
  # And it is the same function in both harnesses, byte for byte.
  check "${name} is the same code in both harnesses" same \
    "$(if cmp -s "${td}/cloud/${name}" "${td}/jetson/${name}"; then
         printf 'same'
       else
         printf 'different: %s' \
           "$(diff "${td}/cloud/${name}" "${td}/jetson/${name}" | head -20 | tr '\n' ' ')"
       fi)"
done <<<"$guard_functions"

# --- every harness variable the shared bodies touch is bound the same way.
#
# Identical bodies still make different claims if the names in them mean
# different things: a STATE_ASIDE_DIR pointing somewhere else in one
# harness would compare the wrong directory while reading as the same
# check. The names are read out of the bodies rather than listed here, so
# this cannot fall behind them.
#
# IFS and LC_ALL are the shell's own, set per-command inside the bodies
# and never read from the harness. Everything else in upper case is
# harness state and must be bound identically in both files -- including
# a name added later, which is the point of deriving rather than listing.
# The python program is single-quoted on purpose: every $ in it belongs to
# the regular expressions, not to the shell.
# shellcheck disable=SC2016
guard_variables="$(cat "${td}"/cloud/* | python3 -c '
import re, sys

shell_own = {"IFS", "LC_ALL"}
body = sys.stdin.read()
names = set(re.findall(r"\$\{([A-Z][A-Z0-9_]*)[}%#:]", body))
names |= set(re.findall(r"\$([A-Z][A-Z0-9_]*)", body))
names |= set(re.findall(r"^\s*([A-Z][A-Z0-9_]*)=", body, re.M))
for name in sorted(names - shell_own):
    print(name)
')"

check "the shared bodies read harness state at all" yes \
  "$([[ -n "$guard_variables" ]] && echo yes || echo no)"

definition_count() {
  # grep -c prints 0 and exits 1 when nothing matches, which is an answer
  # here, not a failure.
  local count=0
  count="$(grep -cE "^(readonly )?${1}=" "$2")" || count=0
  printf '%s' "$count"
}

while IFS= read -r name; do
  [[ -n "$name" ]] || continue
  check "${name} is defined exactly once in each harness" "1 1" \
    "$(printf '%s %s' "$(definition_count "$name" "$cloud")" \
       "$(definition_count "$name" "$jetson")")"
  cloud_definition="$(grep -E "^(readonly )?${name}=" "$cloud" || true)"
  jetson_definition="$(grep -E "^(readonly )?${name}=" "$jetson" || true)"
  check "${name} is bound to the same value in both" "$cloud_definition" "$jetson_definition"
done <<<"$guard_variables"

if ((failures == 0)); then
  printf '\nverify_lifecycle_state_guard: ok (%s checks)\n' "$checks"
else
  printf '\n%s of %s check(s) failed\n' "$failures" "$checks"
fi
exit "$failures"
