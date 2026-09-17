#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The Linux offline-runtime mechanism: the per-unit denial drop-in, the
# readback that refuses a unit systemd is not running, the classification
# that needs a control that completed every operation, and the identity
# checks that require the machine type to have come from the boot-bound
# record. Writes no unit file outside a temporary directory, runs no
# systemd command and opens no socket.
#
# The harness end -- the offline stage body, its cleanup and its signal
# handling -- is exercised against the stubbed appliance in
# verify_ubuntu_l4_cloud_lifecycle.sh.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
module="${repo_root}/tools/validation/linux_offline_runtime.py"
tests="${repo_root}/test/packaging/verify_linux_offline_runtime.py"

for file in "$module" "$tests"; do
  [[ -f "$file" ]] || {
    printf 'FAIL: missing %s\n' "$file" >&2
    exit 1
  }
done
[[ -x "$module" ]] || {
  printf 'FAIL: %s must be executable; the harnesses run it by name\n' "$module" >&2
  exit 1
}

python3 -m py_compile "$module" "$tests"
python3 "$tests"

printf 'verify_linux_offline_runtime: ok\n'
