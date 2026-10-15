#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Installs the toolchain rust-toolchain.toml pins, safely to repeat. A runner
# can hold a half-installed copy of the pinned toolchain, and installing over
# it has stopped at "detected conflict: 'bin/cargo-clippy'"; the copy is
# removed between attempts so the next one starts clean. The pin is read from
# the file, never repeated here.
#
# Usage: tools/ci/install-pinned-toolchain.sh
# Environment: TOOLCHAIN_INSTALL_ATTEMPTS (default 3), TOOLCHAIN_RETRY_DELAY
# (seconds, scaled by the attempt number; default 5).

set -Eeuo pipefail

cd "$(dirname -- "$0")/../.."

attempts="${TOOLCHAIN_INSTALL_ATTEMPTS:-3}"
delay="${TOOLCHAIN_RETRY_DELAY:-5}"
if [[ ! "${attempts}" =~ ^[1-9][0-9]*$ || ! "${delay}" =~ ^[0-9]+$ ]]; then
  printf '::error::TOOLCHAIN_INSTALL_ATTEMPTS must be a positive integer and TOOLCHAIN_RETRY_DELAY a number of seconds\n'
  exit 1
fi
rustup_home="${RUSTUP_HOME:-${HOME:+${HOME}/.rustup}}"
if [[ -z "${rustup_home}" ]]; then
  printf '::error::neither RUSTUP_HOME nor HOME is set\n'
  exit 1
fi
pin="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml)"
# The sweep below globs on the pin, so a moving channel name is refused.
if [[ ! "${pin}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf '::error::rust-toolchain.toml channel is not an exact version\n'
  exit 1
fi

for ((n = 1; n <= attempts; n++)); do
  # With no argument rustup installs what rust-toolchain.toml names, with its
  # profile and components; the version checks fail the attempt if it did not.
  if rustup toolchain install && rustc --version && cargo --version \
    && cargo fmt --version && cargo clippy --version; then
    exit 0
  fi
  printf '::warning::toolchain %s: install attempt %d of %d failed\n' "${pin}" "${n}" "${attempts}"
  if ((n < attempts)); then
    rustup toolchain uninstall "${pin}" || true
    # A partial copy rustup no longer lists is still a directory on disk.
    rm -rf -- "${rustup_home}/toolchains/${pin}"-* \
      || printf '::warning::could not remove the toolchain directory under %s\n' "${rustup_home}"
    sleep "$((delay * n))"
  fi
done

printf '::error::toolchain %s could not be installed in %d attempts\n' "${pin}" "${attempts}"
exit 1
