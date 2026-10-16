#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
for binary in "$@"; do
  dependencies=$(ldd "$binary")
  ! grep -q 'not found' <<<"$dependencies" || exit 1
  mapfile -t closure < <(awk '/=> \// {print $3} /^[[:space:]]*\// {print $1}' <<<"$dependencies")
  for object in "$binary" "${closure[@]}"; do
    dynamic=$(readelf -d "$object")
    symbols=$(nm -D --undefined-only "$object")
    ! grep -qiE 'NEEDED.*\[lib(nvinfer|cuda|cublas|cudnn|nvrtc)' <<<"$dynamic" || exit 1
    ! grep -qE '[[:space:]](cuda|nv|createInferRuntime_INTERNAL|getInferLibVersion)' <<<"$symbols" || exit 1
  done
done
