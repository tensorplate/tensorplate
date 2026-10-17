#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
reject_matches() {
  local object=$1 pattern=$2 input=$3 matches
  if matches=$(grep -iE "$pattern" <<<"$input"); then
    printf '%s: forbidden pattern %s matched:\n%s\n' "$object" "$pattern" "$matches" >&2
    exit 1
  fi
}
for binary in "$@"; do
  dependencies=$(ldd "$binary")
  reject_matches "$binary" 'not found' "$dependencies"
  mapfile -t closure < <(awk '/=> \// {print $3} /^[[:space:]]*\// {print $1}' <<<"$dependencies")
  for object in "$binary" "${closure[@]}"; do
    dynamic=$(readelf -d "$object" 2>/dev/null)
    symbols=$(nm -D --undefined-only "$object")
    reject_matches "$object" 'NEEDED.*\[lib(nvinfer|cuda|cublas|cudnn|nvrtc)' "$dynamic"
    reject_matches "$object" '[[:space:]](cuda|nv|createInferRuntime_INTERNAL|getInferLibVersion)' "$symbols"
  done
done
