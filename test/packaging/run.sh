#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging verification suite orchestrator.

set -eu

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

ran=0
for v in "${here}/verify_layout.sh" \
         "${here}/verify_debian_metadata.sh" \
         "${here}/verify_apt_source.sh" \
         "${here}/verify_metapackage.sh" \
         "${here}/verify_ready_check.sh" \
         "${here}/verify_systemd_units.sh" \
         "${here}/verify_lifecycle_scripts.sh" \
         "${here}/verify_macos_homebrew_lifecycle.sh" \
         "${here}/verify_descriptor.sh" \
         "${here}/verify_installer.sh"; do
  ran=$((ran + 1))
  echo "==> $(basename "${v}")"
  "${v}"
done
echo "packaging suite: ${ran} verifiers green"
