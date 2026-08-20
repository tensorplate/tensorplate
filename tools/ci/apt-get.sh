#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Bounded, retrying apt-get for CI.
#
# The failure this exists for is a stalled mirror, not a failing one: apt
# holds a connection that trickles bytes, so `Acquire::*::Timeout` — which
# fires on an *idle* socket — never trips, and the job burns its entire
# budget without ever failing.
#
# Two details matter, both learned the hard way:
#
#   * `timeout` must run UNDER sudo, not over it. `timeout N sudo apt-get`
#     signals sudo, which does not pass it on; `timeout` then waits forever
#     for a child that never exits, so the bound arms, fires, and hangs.
#     A CI run showed the `timeout` process still alive 45 minutes into a
#     600-second bound. Killing sudo instead would orphan apt-get still
#     holding the step's stdout, so the step hangs either way — the
#     intermediary has to go, and `-k` alone cannot substitute for that.
#
#   * A bound is what makes a retry meaningful. Retrying a hang just hangs
#     again; retrying an *error* can succeed, which is the whole point.
#
# Usage: tools/ci/apt-get.sh update
#        tools/ci/apt-get.sh install -y --no-install-recommends pkg ...

set -Eeuo pipefail

# Roughly ten times a healthy `update` and several times a healthy install;
# tight enough that a stall fails while the job still has budget to retry.
readonly BOUND_SECONDS="${APT_BOUND_SECONDS:-240}"
readonly ATTEMPTS="${APT_ATTEMPTS:-3}"

# Overridable so the behaviour can be exercised without root or a network.
readonly APT_GET="${APT_GET_BIN:-apt-get}"

# Nothing to elevate when already root. The packaging rehearsal runs that
# way, and a container with no sudo installed is a normal place to run it,
# so hard-coding sudo would make this unusable exactly where the unbounded
# apt calls live. An explicit SUDO_BIN still wins, so the behaviour stays
# exercisable without root.
if [ -n "${SUDO_BIN:-}" ]; then
  privileged=("$SUDO_BIN")
elif [ "$(id -u)" -eq 0 ]; then
  privileged=()
else
  privileged=(sudo)
fi

# `::warning::` is GitHub Actions syntax and renders as an annotation
# there. This also runs from the rehearsal on an operator's own container,
# where the same string is just noise.
annotate() {
  local level="$1"
  shift
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    printf '::%s::%s\n' "$level" "$*" >&2
  else
    printf '%s: %s\n' "$level" "$*" >&2
  fi
}

if (( $# == 0 )); then
  echo "usage: $(basename "$0") <apt-get arguments>" >&2
  exit 64
fi

attempt=1
while true; do
  status=0
  # `-k 30`: if apt ignores SIGTERM (mid-dpkg, say), SIGKILL follows. With
  # timeout as apt's direct parent both signals reach apt itself.
  ${privileged[@]+"${privileged[@]}"} timeout -k 30 "$BOUND_SECONDS" "$APT_GET" "$@" || status=$?

  if (( status == 0 )); then
    exit 0
  fi

  if (( status == 124 || status == 137 )); then
    annotate warning "apt-get $1 exceeded ${BOUND_SECONDS}s and was terminated (attempt ${attempt}/${ATTEMPTS})"
  else
    annotate warning "apt-get $1 failed with status ${status} (attempt ${attempt}/${ATTEMPTS})"
  fi

  if (( attempt >= ATTEMPTS )); then
    annotate error "apt-get $1 failed after ${ATTEMPTS} attempts (last status ${status})"
    exit "$status"
  fi

  # A bound that fires mid-unpack leaves dpkg needing a hand before the
  # next attempt can do anything. Advisory: it fails harmlessly when there
  # is nothing interrupted to configure.
  ${privileged[@]+"${privileged[@]}"} dpkg --configure -a || true

  sleep $(( attempt * 10 ))
  attempt=$(( attempt + 1 ))
done
