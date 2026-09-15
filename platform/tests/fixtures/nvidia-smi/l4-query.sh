#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# The test invokes a symlink in its TempDir, so $0 keeps output private.
printf '%s' "$*" > "$0.argv"
echo 'NVIDIA L4, 23034, 550.54.15, GPU-a, Disabled'
