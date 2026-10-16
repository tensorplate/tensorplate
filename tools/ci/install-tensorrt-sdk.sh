#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install the pinned SDK on a disposable Ubuntu 22.04 amd64 CI host.
# Usage: tools/ci/install-tensorrt-sdk.sh
set -Eeuo pipefail

cd "$(dirname "$0")/../.."
# shellcheck source=tools/ci/tensorrt-sdk-versions.sh
source tools/ci/tensorrt-sdk-versions.sh

[[ $# == 0 && $(dpkg --print-architecture) == amd64 ]] || {
  echo 'usage: install-tensorrt-sdk.sh (amd64 only)' >&2
  exit 2
}
privileged=()
if [[ $(id -u) != 0 ]]; then privileged=(sudo); fi

scratch=$(mktemp -d)
trap '"${privileged[@]}" rm -rf "$scratch"' EXIT
keyring=$scratch/keyring.deb
curl --fail --silent --show-error --location --retry 3 --max-time 120 \
  "$NVIDIA_APT_URL/cuda-keyring_${NVIDIA_KEYRING_VERSION}_all.deb" -o "$keyring" || {
  echo 'NVIDIA keyring download failed' >&2; exit 1;
}
printf '%s  %s\n' "$NVIDIA_KEYRING_SHA256" "$keyring" | sha256sum --check || {
  echo 'NVIDIA keyring checksum verification failed' >&2; exit 1;
}
"${privileged[@]}" dpkg -i "$keyring" || {
  echo 'NVIDIA keyring installation failed' >&2; exit 1;
}
tools/ci/apt-get.sh -o Acquire::Retries=3 -o Acquire::https::Timeout=30 update

# APT verifies the signed repository index, then hashes packages it downloads.
# Never reuse archives: apt can accept an existing file based on size alone.
# This fresh directory is removed on exit and is never saved in Actions cache.
mkdir "$scratch/archives"
start=$SECONDS
LC_ALL=C APT_BOUND_SECONDS=600 tools/ci/apt-get.sh \
  -o "Dir::Cache::archives=$scratch/archives" -o Acquire::Retries=3 \
  -o Acquire::https::Timeout=30 install -y --allow-downgrades --no-install-recommends \
  "${TENSORRT_SDK_PACKAGES[@]}" | tee "$scratch/install.log" || {
  echo 'TensorRT SDK package installation failed' >&2; exit 1;
}
install_seconds=$((SECONDS - start))

declare -A allowed=([cuda-keyring]=1)
for pin in "${TENSORRT_SDK_PACKAGES[@]}"; do
  allowed[${pin%%=*}]=1
  actual=$(dpkg-query -W -f='${Version}' "${pin%%=*}")
  [[ $actual == "${pin#*=}" ]] || { echo "version mismatch: $pin" >&2; exit 1; }
  printf '%s\n' "$pin"
done
# NVIDIA's packages identify their origin through the maintainer metadata;
# the optional Origin field is checked too. Ignore removed-package records.
dpkg-query -W -f='${Package}|${db:Status-Status}|${Maintainer}|${Origin}\n' \
  >"$scratch/packages" || { echo 'cannot inspect installed packages' >&2; exit 1; }
while IFS='|' read -r package status vendor; do
  if [[ $status == installed && ${vendor,,} == *nvidia* && ! -v allowed[$package] ]]; then
    echo "unlisted NVIDIA package: $package" >&2
    exit 1
  fi
done <"$scratch/packages"
printf 'CUDAToolkit_ROOT=%s\n' "$CUDAToolkit_ROOT" >>"${GITHUB_ENV:-/dev/null}"
{
  echo '### TensorRT SDK packages'
  echo 'SDK archive cache: disabled. APT download size and time:'
  grep '^Fetched ' "$scratch/install.log" || echo 'No packages needed downloading.'
  printf 'Download and installation: %ss (repository/keyring setup excluded).\n' "$install_seconds"
} | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
