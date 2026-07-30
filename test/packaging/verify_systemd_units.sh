#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: systemd unit-content verifier.
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
  require_line "${unit}" '^ProtectSystem=strict$'
  require_line "${unit}" '^ReadWritePaths=.*/var/lib/tensorplate'
  require_line "${unit}" '^ReadWritePaths=.*/var/log/tensorplate'
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

# /run/tensorplate belongs to the agent: it holds the agent control socket, and
# systemd deletes a RuntimeDirectory= when its unit stops. Observability must
# therefore neither declare it nor name it in ReadWritePaths=, or the two units
# share a path whose lifetime one of them controls — stopping the agent left
# observability unable to start at all (226/NAMESPACE). The asymmetry is the
# fix, so assert both halves of it.
require_line "${debian}/tensorplate-agent.service" '^RuntimeDirectory=tensorplate$'
require_line "${debian}/tensorplate-agent.service" '^ReadWritePaths=.*/run/tensorplate'
forbid_line "${debian}/tensorplate-observability.service" '^RuntimeDirectory='
forbid_line "${debian}/tensorplate-observability.service" '^ReadWritePaths=.*/run/tensorplate'

# Observability must NOT order itself after the agent.
forbid_line "${debian}/tensorplate-observability.service" '(After|Requires|Wants|BindsTo)=tensorplate-agent'

# Architecture parity. The supervision contract must be one source, not a
# per-architecture copy that drifts: a unit named for an architecture, or an
# `.install` line that selects a unit by architecture, means two hosts can be
# supervised differently while claiming the same contract.
arch_units="$(find "${repo_root}/packaging" -name '*.service' \
  \( -name '*amd64*' -o -name '*arm64*' -o -name '*x86*' -o -name '*jetson*' \) -print)"
if [ -n "${arch_units}" ]; then
  echo "FAIL: architecture-specific unit files exist; the supervision contract must be shared:" >&2
  printf '%s\n' "${arch_units}" >&2
  fail=1
fi
for install in "${debian}/tensorplate-agent.install" "${debian}/tensorplate-observability.install"; do
  if grep -qE '^\[[^]]*\].*\.service' "${install}"; then
    echo "FAIL: ${install} selects a unit file by architecture" >&2
    fail=1
  fi
done

# The two units must agree on the supervision contract itself. dh_installsystemd
# installs both with --no-start, and an operator reading one unit's restart
# policy must be able to assume the other's.
for directive in 'Restart=on-failure' 'RestartSec=5' 'StartLimitBurst=5' \
                 'StartLimitIntervalSec=60'; do
  for unit in "${debian}/tensorplate-agent.service" "${debian}/tensorplate-observability.service"; do
    require_line "${unit}" "^${directive}\$"
  done
done

# Logs must be reachable at the documented path on every architecture, which
# means systemd owns the directory rather than a maintainer script guessing.
for unit in "${debian}/tensorplate-agent.service" "${debian}/tensorplate-observability.service"; do
  require_line "${unit}" '^LogsDirectory=tensorplate$'
  require_line "${unit}" '^LogsDirectoryMode=0750$'
done

# No serving unit anywhere in the tree.
if find "${repo_root}/packaging" -name 'tensorplate-serving.service' -print -quit | grep -q .; then
  echo "FAIL: tensorplate-serving.service exists somewhere under packaging/" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_systemd_units: ok"
fi
exit "${fail}"
