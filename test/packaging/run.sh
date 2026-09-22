#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging verification suite orchestrator.
#
# Usage: run.sh [all|core|harness]
#
#   core     the packaging, installer and descriptor checks, and the
#            static checks over the lifecycle harnesses that drive no
#            appliance. Fast, and what the release artifact build runs.
#   harness  the lifecycle validation harness verifiers. They drive each
#            harness against a stubbed appliance through every failure
#            mode, which takes far longer than the core checks, so CI
#            runs them as their own job.
#   all      both groups, core first. The default.
#
# Every verify_*.sh here belongs to exactly one group or to the list of
# host-mutating verifiers this suite never runs. A verifier in none of
# them fails the suite, so a new one cannot be silently skipped.

set -eu

here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"

core="verify_layout.sh
verify_debian_metadata.sh
verify_apt_source.sh
verify_metapackage.sh
verify_ready_check.sh
verify_systemd_units.sh
verify_lifecycle_scripts.sh
verify_descriptor.sh
verify_installer.sh
verify_lifecycle_state_guard.sh"

harness="verify_macos_homebrew_lifecycle.sh
verify_macos_offline_runtime.sh
verify_linux_offline_runtime.sh
verify_ubuntu_l4_cloud_lifecycle.sh
verify_jetson_lifecycle.sh"

# Run elsewhere: they mutate the host they run on.
host_mutating="verify_arch_package_set.sh
verify_service_supervision.sh
verify_cpu_only_smoke.sh"

group="${1:-all}"
case "${group}" in
  all) selected="${core}
${harness}" ;;
  core) selected="${core}" ;;
  harness) selected="${harness}" ;;
  *)
    echo "usage: $(basename "$0") [all|core|harness]" >&2
    exit 2
    ;;
esac

known="${core}
${harness}
${host_mutating}"
unlisted=""
for path in "${here}"/verify_*.sh; do
  [ -e "${path}" ] || continue
  name="$(basename "${path}")"
  if ! printf '%s\n' "${known}" | grep -Fqx -- "${name}"; then
    unlisted="${unlisted} ${name}"
  fi
done
if [ -n "${unlisted}" ]; then
  echo "run.sh: verifier(s) in no group:${unlisted}" >&2
  echo "run.sh: add each to core, harness or host_mutating in $0" >&2
  exit 1
fi

ran=0
for name in ${selected}; do
  ran=$((ran + 1))
  echo "==> ${name}"
  "${here}/${name}"
done
echo "packaging suite (${group}): ${ran} verifiers green"
