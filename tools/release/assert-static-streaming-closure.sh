#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Fails when the serving package or its worker binary depends on a
# distribution library of the gRPC closure: the release links it statically.

set -Eeuo pipefail

readonly USAGE="usage: assert-static-streaming-closure.sh --deb <serving .deb> --binary <worker binary>"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

deb=""
binary=""
while [[ $# -gt 0 ]]; do
  [[ "$1" == --deb || "$1" == --binary ]] || die "unknown argument '$1'; $USAGE"
  [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || die "$1 needs a value; $USAGE"
  if [[ "$1" == --deb ]]; then deb="$2"; else binary="$2"; fi
  shift 2
done
[[ -n "$deb" && -n "$binary" ]] || die "$USAGE"
[[ -f "$deb" ]] || die "no package at $deb"
[[ -f "$binary" ]] || die "no worker binary at $binary"

# A check that could not be made is a failure, never a pass.
depends="$(dpkg-deb -f "$deb" Depends)" || die "dpkg-deb could not read Depends from $deb"
early="$(dpkg-deb -f "$deb" Pre-Depends)" || die "dpkg-deb could not read Pre-Depends from $deb"
found=()
while IFS= read -r entry; do
  entry="${entry#"${entry%%[![:space:]]*}"}"
  case "${entry%%[[:space:](:]*}" in
    libgrpc* | libprotobuf* | libabsl* | libre2* | libc-ares* | libupb* | libssl*)
      found+=("${entry%%[[:space:](:]*}")
      ;;
  esac
done < <(tr ',|' '\n' <<<"${depends},${early}")
if ((${#found[@]} > 0)); then
  die "$deb depends on distribution packages the worker must link statically: ${found[*]}"
fi
printf 'ok: %s depends on no distribution package of the gRPC closure\n' "$deb"

# Not libssl, libcrypto or libz here: a vendor library may load the system's.
linked="$(ldd "$binary")" || die "ldd could not list the libraries of $binary"
while read -r library _; do
  case "${library##*/}" in
    libgrpc* | libgpr* | libprotobuf* | libabsl* | libre2* | libcares* | libupb*) found+=("$library") ;;
  esac
done <<<"$linked"
if ((${#found[@]} > 0)); then
  die "$binary loads shared libraries the worker must link statically: ${found[*]}"
fi
printf 'ok: %s loads no shared library of the gRPC closure\n' "$binary"
