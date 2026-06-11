#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: TensorPlate-ready host check linter.
#
# Runs tools/validation/tensorplate-ready-check.sh against known-good and
# known-bad source/keyring fixtures and asserts the correct verdict, so
# the image/provisioning validation helper stays trustworthy.

set -eu

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
check="${repo_root}/tools/validation/tensorplate-ready-check.sh"
apt_dir="${repo_root}/packaging/apt"

fail=0
td="$(mktemp -d)"
trap 'rm -rf "${td}"' EXIT HUP INT TERM

if [ ! -x "${check}" ]; then
  echo "FAIL: ${check} missing or not executable" >&2
  exit 1
fi
if ! bash -n "${check}"; then
  echo "FAIL: ${check} has shell syntax errors" >&2
  exit 1
fi

# Good fixture: the real Deb822 source with Signed-By rewritten to a
# fixture keyring. Use a real dearmored keyring when gpg is available,
# otherwise representative binary (non-armored) bytes.
keyring="${td}/keyring.gpg"
if command -v gpg >/dev/null 2>&1; then
  gpg --batch --yes --dearmor --output "${keyring}" \
    "${apt_dir}/tensorplate-archive-keyring.asc" 2>/dev/null
else
  printf '\231\001\015binary-openpgp-fixture' > "${keyring}"
fi
sed "s|^Signed-By: .*|Signed-By: ${keyring}|" \
  "${apt_dir}/tensorplate.sources" > "${td}/good.sources"

run_check() {
  TP_READY_KEYRING="${1}" TP_READY_SOURCES="${2}" TP_READY_SKIP_DPKG=1 \
    "${check}" >/dev/null 2>&1
}

if ! run_check "${keyring}" "${td}/good.sources"; then
  echo "FAIL: ready-check must pass on a correctly provisioned host" >&2
  fail=1
fi

if run_check "${td}/missing.gpg" "${td}/good.sources"; then
  echo "FAIL: ready-check must fail when the keyring is missing" >&2
  fail=1
fi

# Armored keyring: APT cannot use it at the .gpg Signed-By path.
sed "s|^Signed-By: .*|Signed-By: ${apt_dir}/tensorplate-archive-keyring.asc|" \
  "${apt_dir}/tensorplate.sources" > "${td}/armored.sources"
if run_check "${apt_dir}/tensorplate-archive-keyring.asc" "${td}/armored.sources"; then
  echo "FAIL: ready-check must reject an ASCII-armored keyring" >&2
  fail=1
fi

# Version-pinned repository URI breaks future-version discovery.
sed -e "s|^Signed-By: .*|Signed-By: ${keyring}|" \
    -e "s|^URIs: .*|URIs: https://packages.tensorplate.com/apt/0.1.2|" \
  "${apt_dir}/tensorplate.sources" > "${td}/pinned.sources"
if run_check "${keyring}" "${td}/pinned.sources"; then
  echo "FAIL: ready-check must reject a version-pinned repository URI" >&2
  fail=1
fi

# Signed-By pointing somewhere other than the expected keyring.
sed "s|^Signed-By: .*|Signed-By: /usr/share/keyrings/other.gpg|" \
  "${apt_dir}/tensorplate.sources" > "${td}/wrongkey.sources"
if run_check "${keyring}" "${td}/wrongkey.sources"; then
  echo "FAIL: ready-check must reject a Signed-By mismatch" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_ready_check: ok"
fi
exit "${fail}"
