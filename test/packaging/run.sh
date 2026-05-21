#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# V01-E14-F08 packaging verification suite orchestrator.

set -eu

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

ran=0
for v in "${here}/verify_layout.sh" \
         "${here}/verify_debian_metadata.sh" \
         "${here}/verify_systemd_units.sh" \
         "${here}/verify_lifecycle_scripts.sh" \
         "${here}/verify_descriptor.sh"; do
  ran=$((ran + 1))
  echo "==> $(basename "${v}")"
  "${v}"
done
echo "packaging suite: ${ran} verifiers green"
