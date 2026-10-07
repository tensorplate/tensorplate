#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Fails unless the worker was configured with the streaming feature and neither
# it nor the serving package asks for a distribution library of the gRPC closure.

set -Eeuo pipefail

readonly USAGE="usage: assert-static-streaming-closure.sh --deb <serving .deb> --binary <worker binary> --cmake-cache <CMakeCache.txt>"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

deb=""
binary=""
cache=""
while [[ $# -gt 0 ]]; do
  [[ "$1" == --deb || "$1" == --binary || "$1" == --cmake-cache ]] ||
    die "unknown argument '$1'; $USAGE"
  [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || die "$1 needs a value; $USAGE"
  case "$1" in
    --deb) deb="$2" ;;
    --binary) binary="$2" ;;
    *) cache="$2" ;;
  esac
  shift 2
done
[[ -n "$deb" && -n "$binary" && -n "$cache" ]] || die "$USAGE"
[[ -f "$deb" ]] || die "no package at $deb"
[[ -f "$binary" ]] || die "no worker binary at $binary"

# A check that could not be made is a failure, never a pass.
# A worker built without the feature has a clean closure too, so this comes first.
grep -qxF 'TP_ENABLE_STREAMING_GRPC:BOOL=ON' "$cache" ||
  die "$cache does not record TP_ENABLE_STREAMING_GRPC:BOOL=ON: the worker was not configured with the streaming feature"
printf 'ok: %s records the streaming feature ON\n' "$cache"
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

# The worker's own NEEDED entries, not what a vendor library loads in turn.
# zlib is in neither list: other libraries need the distribution's, under one soname on every release.
dynamic="$(readelf -d "$binary")" || die "readelf could not read the dynamic section of $binary"
needed=0
while IFS= read -r line; do
  [[ "$line" == *"(NEEDED)"*"["*"]"* ]] || continue
  needed=$((needed + 1))
  library="${line##*\[}"
  library="${library%%\]*}"
  case "${library##*/}" in
    libgrpc* | libgpr* | libprotobuf* | libabsl* | libre2* | libcares* | libupb* | libssl* | libcrypto*)
      found+=("$library")
      ;;
  esac
done <<<"$dynamic"
((needed > 0)) || die "readelf listed no NEEDED entry for $binary"
if ((${#found[@]} > 0)); then
  die "$binary needs shared libraries the worker must link statically: ${found[*]}"
fi
printf 'ok: %s needs no shared library of the gRPC closure\n' "$binary"
