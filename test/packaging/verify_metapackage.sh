#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: tensorplate runtime metapackage linter.
#
# Asserts the `tensorplate` metapackage stays an empty, architecture-
# qualified dependency bundle: strict-versioned depends on the runtime set,
# no payload, no maintainer scripts, optional Python backend suggested but
# never pulled in, and no dependency on the apt bootstrap package.

set -eu

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
debian="${repo_root}/packaging/debian"
control="${debian}/control"

fail=0

stanza="$(awk '/^Package: tensorplate$/{f=1} f && /^$/{exit} f{print}' "${control}")"

if [ -z "${stanza}" ]; then
  echo "FAIL: tensorplate missing Package: stanza in debian/control" >&2
  fail=1
else
  # `any`, not `all`: the strict `= ${binary:Version}` relations below bind
  # per-architecture runtime binaries, so the metapackage must be built in
  # the same dpkg-buildpackage run as the binaries it pins. `all` would also
  # offer the package on architectures that have no runtime set, where it
  # would fail with unsatisfiable dependencies instead of simply not existing.
  if ! printf '%s\n' "${stanza}" | grep -q '^Architecture: any$'; then
    echo "FAIL: tensorplate metapackage must be Architecture: any (built per runtime architecture)" >&2
    fail=1
  fi
  if ! printf '%s\n' "${stanza}" | grep -q '^Section: metapackages$'; then
    echo "FAIL: tensorplate must declare Section: metapackages" >&2
    fail=1
  fi
  # shellcheck disable=SC2016 # Debian substvars are literal text here.
  for dep in \
    'tensorplate-common (= ${source:Version}),' \
    'tensorplate-agent (= ${binary:Version}),' \
    'tensorplate-serving (= ${binary:Version}),' \
    'tensorplate-observability (= ${binary:Version}),' \
    'tensorplate-cli (= ${binary:Version}),'; do
    if ! printf '%s\n' "${stanza}" | grep -qF " ${dep}"; then
      echo "FAIL: tensorplate must strictly depend on: ${dep%,}" >&2
      fail=1
    fi
  done
  if printf '%s\n' "${stanza}" | grep -q 'tensorplate-apt-source'; then
    echo "FAIL: tensorplate must not depend on the apt bootstrap package" >&2
    fail=1
  fi
  if printf '%s\n' "${stanza}" | sed -n '/^Suggests:/q;p' |
     grep -q 'tensorplate-backend-python-pytorch'; then
    echo "FAIL: the Python backend must stay optional (Suggests only, never Depends/Recommends)" >&2
    fail=1
  fi
  if printf '%s\n' "${stanza}" | grep -q '^Suggests:' &&
     ! printf '%s\n' "${stanza}" | grep -q ' tensorplate-backend-python-pytorch'; then
    echo "FAIL: tensorplate should suggest the optional Python backend" >&2
    fail=1
  fi
fi

# A metapackage ships nothing: no payload manifest, maintainer scripts,
# units, or conffiles.
for extra in install preinst postinst prerm postrm service conffiles; do
  if [ -e "${debian}/tensorplate.${extra}" ]; then
    echo "FAIL: tensorplate is a metapackage and must not ship tensorplate.${extra}" >&2
    fail=1
  fi
done

# Release tooling must require the metapackage in artifact, manifest,
# and verify lists.
if ! grep -Eq '^[[:space:]]+tensorplate$' "${repo_root}/tools/release/build-release-artifacts.sh"; then
  echo "FAIL: build-release-artifacts.sh REQUIRED_PACKAGES must include tensorplate" >&2
  fail=1
fi
if ! grep -Eq '^[[:space:]]+tensorplate$' "${repo_root}/tools/release/tensorplate-release.sh" ||
   [ "$(grep -c '"tensorplate",' "${repo_root}/tools/release/tensorplate-release.sh")" -lt 2 ]; then
  echo "FAIL: tensorplate-release.sh must require tensorplate in artifact, manifest, and verify lists" >&2
  fail=1
fi

# The GitHub-release installer keeps installing the explicit package set;
# pulling the metapackage through install.sh is a separate flow decision
# (issue #43).
if grep -q '"tensorplate",' "${repo_root}/packaging/scripts/install.sh"; then
  echo "FAIL: install.sh must not install the tensorplate metapackage implicitly (owned by issue #43)" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_metapackage: ok"
fi
exit "${fail}"
