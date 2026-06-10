#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: tensorplate-apt-source bootstrap package linter.
#
# Asserts the bootstrap package stays exactly what it claims to be: an
# architecture-independent, dependency-free package that installs only the
# archive keyring and the stable Deb822 source, and never touches runtime
# state or pins a runtime version in the repository URI.

set -eu

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
debian="${repo_root}/packaging/debian"
apt_dir="${repo_root}/packaging/apt"
control="${debian}/control"
sources="${apt_dir}/tensorplate.sources"
keyring_asc="${apt_dir}/tensorplate-archive-keyring.asc"
install_manifest="${debian}/tensorplate-apt-source.install"
postinst="${debian}/tensorplate-apt-source.postinst"

STABLE_URI="https://packages.tensorplate.com/apt"
KEYRING_PATH="/usr/share/keyrings/tensorplate-archive-keyring.gpg"

fail=0

stanza="$(awk '/^Package: tensorplate-apt-source$/{f=1} f && /^$/{exit} f{print}' "${control}")"

# Control stanza: arch all, foreign, no runtime dependencies.
if [ -z "${stanza}" ]; then
  echo "FAIL: tensorplate-apt-source missing Package: stanza in debian/control" >&2
  fail=1
else
  if ! printf '%s\n' "${stanza}" | grep -q '^Architecture: all$'; then
    echo "FAIL: tensorplate-apt-source must be Architecture: all" >&2
    fail=1
  fi
  if ! printf '%s\n' "${stanza}" | grep -q '^Multi-Arch: foreign$'; then
    echo "FAIL: tensorplate-apt-source must be Multi-Arch: foreign" >&2
    fail=1
  fi
  if printf '%s\n' "${stanza}" | grep -Eq '^(Pre-Depends|Recommends|Suggests):'; then
    echo "FAIL: tensorplate-apt-source must declare no Pre-Depends/Recommends/Suggests" >&2
    fail=1
  fi
  if printf '%s\n' "${stanza}" | sed -n '/^Description:/q;p' |
     grep -v '^Package:' | grep -q 'tensorplate-'; then
    echo "FAIL: tensorplate-apt-source must not depend on other tensorplate packages" >&2
    fail=1
  fi
fi

# The dearmor step in debian/rules needs gnupg at package build time.
if ! grep -q '^ gnupg,$' "${control}"; then
  echo "FAIL: debian/control Build-Depends must include gnupg for the keyring dearmor" >&2
  fail=1
fi

# Install manifest: exactly the keyring and the source file, nothing else.
if [ ! -f "${install_manifest}" ]; then
  echo "FAIL: missing ${install_manifest}" >&2
  fail=1
else
  payload_lines="$(grep -cv '^[[:space:]]*\(#\|$\)' "${install_manifest}")"
  if [ "${payload_lines}" -ne 2 ]; then
    echo "FAIL: tensorplate-apt-source.install must ship exactly 2 payloads, found ${payload_lines}" >&2
    fail=1
  fi
  if ! grep -q '^debian/generated/tensorplate-archive-keyring\.gpg[[:space:]]\{1,\}usr/share/keyrings/$' "${install_manifest}"; then
    echo "FAIL: tensorplate-apt-source.install must install the dearmored keyring to usr/share/keyrings/" >&2
    fail=1
  fi
  if ! grep -q '^packaging/apt/tensorplate\.sources[[:space:]]\{1,\}etc/apt/sources\.list\.d/$' "${install_manifest}"; then
    echo "FAIL: tensorplate-apt-source.install must install tensorplate.sources to etc/apt/sources.list.d/" >&2
    fail=1
  fi
fi

# Deb822 source: stable URI, signed-by the shipped keyring, no runtime
# version anywhere (the channel must outlive 0.1.2 / 0.1.3 / 0.2.0).
if [ ! -f "${sources}" ]; then
  echo "FAIL: missing ${sources}" >&2
  fail=1
else
  if ! grep -q '^Types: deb$' "${sources}"; then
    echo "FAIL: tensorplate.sources must declare Types: deb" >&2
    fail=1
  fi
  if ! grep -q "^URIs: ${STABLE_URI}\$" "${sources}"; then
    echo "FAIL: tensorplate.sources URIs must be exactly ${STABLE_URI}" >&2
    fail=1
  fi
  if ! grep -q '^Suites: jammy$' "${sources}"; then
    echo "FAIL: tensorplate.sources must declare Suites: jammy" >&2
    fail=1
  fi
  if ! grep -q '^Components: main$' "${sources}"; then
    echo "FAIL: tensorplate.sources must declare Components: main" >&2
    fail=1
  fi
  if ! grep -q "^Signed-By: ${KEYRING_PATH}\$" "${sources}"; then
    echo "FAIL: tensorplate.sources Signed-By must be ${KEYRING_PATH}" >&2
    fail=1
  fi
  if grep -Eq '[0-9]+\.[0-9]+\.[0-9]+' "${sources}"; then
    echo "FAIL: tensorplate.sources must not embed a runtime version" >&2
    fail=1
  fi
  if grep -q 'http://' "${sources}"; then
    echo "FAIL: tensorplate.sources must use https only" >&2
    fail=1
  fi
fi

# Keyring: committed as reviewable ASCII armor; a staging placeholder is
# only acceptable while the release builder refuses to publish it.
if [ ! -f "${keyring_asc}" ]; then
  echo "FAIL: missing ${keyring_asc}" >&2
  fail=1
else
  if ! grep -q '^-----BEGIN PGP PUBLIC KEY BLOCK-----$' "${keyring_asc}" ||
     ! grep -q '^-----END PGP PUBLIC KEY BLOCK-----$' "${keyring_asc}"; then
    echo "FAIL: ${keyring_asc} is not an armored OpenPGP public key block" >&2
    fail=1
  fi
  if grep -q 'PRIVATE KEY' "${keyring_asc}"; then
    echo "FAIL: ${keyring_asc} must never contain private key material" >&2
    fail=1
  fi
  if grep -q 'STAGING PLACEHOLDER' "${keyring_asc}" &&
     ! grep -q 'STAGING PLACEHOLDER' "${repo_root}/tools/release/build-release-artifacts.sh"; then
    echo "FAIL: staging placeholder keyring requires the release-build guard in build-release-artifacts.sh" >&2
    fail=1
  fi
fi

# debian/rules must dearmor the committed .asc into the installed payload,
# and the generated tree must be cleaned.
if ! grep -q -- '--dearmor' "${debian}/rules" ||
   ! grep -q 'debian/generated/tensorplate-archive-keyring\.gpg' "${debian}/rules" ||
   ! grep -q 'packaging/apt/tensorplate-archive-keyring\.asc' "${debian}/rules"; then
  echo "FAIL: debian/rules must dearmor packaging/apt/tensorplate-archive-keyring.asc into debian/generated/" >&2
  fail=1
fi
if ! grep -q '^debian/generated/$' "${debian}/clean"; then
  echo "FAIL: debian/clean must remove debian/generated/" >&2
  fail=1
fi

# Maintainer scripts: a fail-closed postinst and nothing else. The package
# must never run apt, install runtime packages, or mutate runtime state.
if [ ! -f "${postinst}" ]; then
  echo "FAIL: missing ${postinst}" >&2
  fail=1
else
  if grep -v '^[[:space:]]*#' "${postinst}" |
     grep -Eq 'apt(-get|-cache)?[[:space:]]+(update|install)'; then
    echo "FAIL: tensorplate-apt-source.postinst must not run apt update/install" >&2
    fail=1
  fi
  if grep -Eq 'create-users\.sh|install-paths\.sh' "${postinst}"; then
    echo "FAIL: tensorplate-apt-source.postinst must not touch TensorPlate runtime layout" >&2
    fail=1
  fi
  if ! grep -q "${KEYRING_PATH}" "${postinst}" ||
     ! grep -q '/etc/apt/sources.list.d/tensorplate.sources' "${postinst}"; then
    echo "FAIL: tensorplate-apt-source.postinst must fail closed on both installed paths" >&2
    fail=1
  fi
fi
for extra in preinst prerm postrm service; do
  if [ -e "${debian}/tensorplate-apt-source.${extra}" ]; then
    echo "FAIL: tensorplate-apt-source must not ship a ${extra}" >&2
    fail=1
  fi
done

# Release tooling must carry the package into assets, manifest, and
# SHA256SUMS; the GitHub-release installer must not auto-install it
# (that behavior decision belongs to the install/upgrade-flow
# validation, issue #43).
if ! grep -q 'tensorplate-apt-source' "${repo_root}/tools/release/build-release-artifacts.sh"; then
  echo "FAIL: build-release-artifacts.sh REQUIRED_PACKAGES must include tensorplate-apt-source" >&2
  fail=1
fi
if [ "$(grep -c 'tensorplate-apt-source' "${repo_root}/tools/release/tensorplate-release.sh")" -lt 3 ]; then
  echo "FAIL: tensorplate-release.sh must require tensorplate-apt-source in artifact, manifest, and verify lists" >&2
  fail=1
fi
if grep -q 'tensorplate-apt-source' "${repo_root}/packaging/scripts/install.sh"; then
  echo "FAIL: install.sh must not install tensorplate-apt-source implicitly (owned by the install/upgrade-flow validation, issue #43)" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_apt_source: ok"
fi
exit "${fail}"
