#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: debhelper metadata linter.

set -eu

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
debian="${repo_root}/packaging/debian"

fail=0

# Expected binary packages.
PACKAGES="tensorplate-common tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli tensorplate-backend-python-pytorch tensorplate-apt-source tensorplate"

for pkg in ${PACKAGES}; do
  if ! grep -q "^Package: ${pkg}\$" "${debian}/control"; then
    echo "FAIL: ${pkg} missing Package: stanza in debian/control" >&2
    fail=1
    continue
  fi
  if [ "${pkg}" = "tensorplate" ]; then
    # The runtime metapackage deliberately ships no files; its empty
    # shape is asserted by verify_metapackage.sh.
    continue
  fi
  if [ ! -f "${debian}/${pkg}.install" ]; then
    echo "FAIL: ${pkg} missing ${pkg}.install" >&2
    fail=1
  fi
done

# The optional Python backend package must be importable and runnable
# once installed, not merely copy source files under /usr/lib.
for backend_payload in \
  "${repo_root}/packaging/python/tensorplate_pytorch_backend.pth" \
  "${repo_root}/packaging/bin/tensorplate-backend-python-pytorch"; do
  if [ ! -f "${backend_payload}" ]; then
    echo "FAIL: missing Python backend install payload ${backend_payload}" >&2
    fail=1
  fi
done
if ! grep -q 'usr/lib/python3/dist-packages' "${debian}/tensorplate-backend-python-pytorch.install"; then
  echo "FAIL: Python backend manifest must install its dist-packages .pth" >&2
  fail=1
fi
if ! grep -q 'usr/bin/' "${debian}/tensorplate-backend-python-pytorch.install"; then
  echo "FAIL: Python backend manifest must install its console entrypoint" >&2
  fail=1
fi
if ! grep -q '=> usr/share/tensorplate/backends/python_pytorch/backend.json' "${debian}/tensorplate-backend-python-pytorch.install"; then
  echo "FAIL: Python backend manifest must rename its descriptor to backend.json" >&2
  fail=1
fi

# Maintainer scripts must be executable.
for f in "${debian}"/*.preinst "${debian}"/*.postinst "${debian}"/*.prerm "${debian}"/*.postrm; do
  [ -e "${f}" ] || continue
  if [ ! -x "${f}" ]; then
    echo "FAIL: maintainer script not executable: ${f}" >&2
    fail=1
  fi
  # Strip the DEBHELPER token before syntax-checking with sh -n.
  if ! sed 's/#DEBHELPER#//' "${f}" | sh -n; then
    echo "FAIL: maintainer script has shell syntax error: ${f}" >&2
    fail=1
  fi
done
if ! grep -q 'deb-systemd-invoke stop tensorplate-agent.service' "${debian}/tensorplate-agent.prerm"; then
  echo "FAIL: tensorplate-agent.prerm must stop the running unit before remove/upgrade" >&2
  fail=1
fi
if ! grep -q 'deb-systemd-invoke stop tensorplate-observability.service' "${debian}/tensorplate-observability.prerm"; then
  echo "FAIL: tensorplate-observability.prerm must stop the running unit before remove/upgrade" >&2
  fail=1
fi
if ! grep -q 'rmdir /var/lib/tensorplate /etc/tensorplate' "${debian}/tensorplate-agent.postrm"; then
  echo "FAIL: tensorplate-agent.postrm purge must remove empty install roots" >&2
  fail=1
fi

# Conffile assertions: configs under /etc are auto-managed by
# debhelper as conffiles. Do not duplicate those entries via explicit
# *.conffiles files.
for cfg_pkg in tensorplate-agent tensorplate-observability tensorplate-serving tensorplate-cli tensorplate-apt-source; do
  if [ -e "${debian}/${cfg_pkg}.conffiles" ]; then
    echo "FAIL: ${cfg_pkg} must not duplicate auto-generated conffile metadata" >&2
    fail=1
  fi
done

# systemd units: agent + observability must have one, serving must not.
for unit_pkg in tensorplate-agent tensorplate-observability; do
  if [ ! -f "${debian}/${unit_pkg}.service" ]; then
    echo "FAIL: ${unit_pkg} missing ${unit_pkg}.service" >&2
    fail=1
  fi
done
if [ -f "${debian}/tensorplate-serving.service" ]; then
  echo "FAIL: tensorplate-serving.service must not exist (agent supervises the serving worker, V01-E09)" >&2
  fail=1
fi

# debian/rules and source/format must exist.
for f in "${debian}/rules" "${debian}/source/format" "${debian}/changelog" "${debian}/copyright"; do
  if [ ! -f "${f}" ]; then
    echo "FAIL: missing ${f}" >&2
    fail=1
  fi
done
if ! grep -q 'debhelper-compat (= 13)' "${debian}/control"; then
  echo "FAIL: debian/control must declare debhelper-compat (= 13)" >&2
  fail=1
fi
if [ -e "${debian}/compat" ]; then
  echo "FAIL: debhelper compat must not be duplicated in debian/compat" >&2
  fail=1
fi
if [ ! -x "${debian}/rules" ]; then
  echo "FAIL: debian/rules must be executable" >&2
  fail=1
fi
if [ ! -x "${repo_root}/packaging/scripts/build-deb.sh" ]; then
  echo "FAIL: packaging/scripts/build-deb.sh must be executable" >&2
  fail=1
fi
if grep -q -- '--with systemd' "${debian}/rules"; then
  echo "FAIL: debian/rules must rely on debhelper 13's default dh_installsystemd sequence" >&2
  fail=1
fi
if ! grep -q '^override_dh_auto_configure:' "${debian}/rules"; then
  echo "FAIL: debian/rules must keep configure external to the package skeleton" >&2
  fail=1
fi
# The cli-only build profile builds just the workstation CLI without the
# runtime services or the metapackage. The release workflow no longer uses
# it — the hosted amd64 job builds the full runtime set — but it remains a
# supported build mode, so the opt-outs must stay declared. Runtime services
# and the metapackage opt out; the CLI and its arch-all companions must not.
if [ "$(grep -c '^Build-Profiles: <!pkg\.tensorplate\.cli-only>$' "${debian}/control")" -ne 4 ]; then
  echo "FAIL: agent, serving, observability, and the metapackage must declare Build-Profiles: <!pkg.tensorplate.cli-only>" >&2
  fail=1
fi
if awk '/^Package: tensorplate-cli$/{f=1} f && /^$/{exit} f{print}' "${debian}/control" | grep -q 'Build-Profiles'; then
  echo "FAIL: tensorplate-cli must stay buildable under the cli-only profile" >&2
  fail=1
fi
if ! grep -q 'dh_installsystemd --no-start -ptensorplate-agent' "${debian}/rules"; then
  echo "FAIL: debian/rules must install the agent unit into tensorplate-agent" >&2
  fail=1
fi
if ! grep -q 'dh_installsystemd --no-start -ptensorplate-observability' "${debian}/rules"; then
  echo "FAIL: debian/rules must install the observability unit into tensorplate-observability" >&2
  fail=1
fi
if ! grep -q 'dh_shlibdeps -ptensorplate-serving --dpkg-shlibdeps-params=--ignore-missing-info' "${debian}/rules"; then
  echo "FAIL: debian/rules must tolerate JetPack CUDA libraries without shlibs metadata for tensorplate-serving" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_debian_metadata: ok"
fi
exit "${fail}"
