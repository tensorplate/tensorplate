#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
if [[ $# -eq 0 ]]; then
  echo 'usage: memory-sample.sh /path/to/memory_sample [sampling options]' >&2
  exit 1
fi
sampler=$1
shift
exec "$sampler" "$@"
