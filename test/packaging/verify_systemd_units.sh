#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# V01-E14-F08: systemd unit-content verifier.
#
# Asserts the v0.1.0 invariants on every shipped unit:
#   - runs as User=tensorplate, Group=tensorplate
#   - RuntimeDirectory=tensorplate
#   - ReadWritePaths covers /var/lib/tensorplate and /run/tensorplate
#   - hardening directives present
#   - serving worker has no unit

set -eu

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
debian="${repo_root}/packaging/debian"

fail=0

require_line() {
  unit="$1"
  pattern="$2"
  if ! grep -qE "${pattern}" "${unit}"; then
    echo "FAIL: ${unit} missing required line matching: ${pattern}" >&2
    fail=1
  fi
}

forbid_line() {
  unit="$1"
  pattern="$2"
  if grep -qE "${pattern}" "${unit}"; then
    echo "FAIL: ${unit} contains forbidden line matching: ${pattern}" >&2
    fail=1
  fi
}

for unit in "${debian}/tensorplate-agent.service" "${debian}/tensorplate-observability.service"; do
  require_line "${unit}" '^User=tensorplate$'
  require_line "${unit}" '^Group=tensorplate$'
  require_line "${unit}" '^Type=simple$'
  require_line "${unit}" '^RuntimeDirectory=tensorplate$'
  require_line "${unit}" '^ProtectSystem=strict$'
  require_line "${unit}" '^ReadWritePaths=.*/var/lib/tensorplate'
  require_line "${unit}" '^ReadWritePaths=.*/var/log/tensorplate'
  require_line "${unit}" '^ReadWritePaths=.*/run/tensorplate'
  require_line "${unit}" '^NoNewPrivileges=true$'
  require_line "${unit}" '^ProtectHome=true$'
  require_line "${unit}" '^PrivateTmp=true$'
  require_line "${unit}" '^Restart=on-failure$'
  forbid_line "${unit}" '^Restart=always$'
done

# The agent supervises the TensorRT/CUDA serving worker; hiding /dev from the
# agent service also hides GPU device nodes from the worker it spawns.
require_line "${debian}/tensorplate-agent.service" '^PrivateDevices=false$'
forbid_line "${debian}/tensorplate-agent.service" '^PrivateDevices=true$'
require_line "${debian}/tensorplate-observability.service" '^PrivateDevices=true$'

# Observability must NOT order itself after the agent.
forbid_line "${debian}/tensorplate-observability.service" '(After|Requires|Wants|BindsTo)=tensorplate-agent'

# No serving unit anywhere in the tree.
if find "${repo_root}/packaging" -name 'tensorplate-serving.service' -print -quit | grep -q .; then
  echo "FAIL: tensorplate-serving.service exists somewhere under packaging/" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_systemd_units: ok"
fi
exit "${fail}"
