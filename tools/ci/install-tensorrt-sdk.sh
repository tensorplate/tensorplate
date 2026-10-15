#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install the pinned SDK on a disposable Ubuntu 22.04 amd64 CI host.
# Usage: tools/ci/install-tensorrt-sdk.sh <apt-archive-directory>
set -Eeuo pipefail

cd "$(dirname "$0")/../.."
# shellcheck source=tools/ci/tensorrt-sdk-versions.sh
source tools/ci/tensorrt-sdk-versions.sh

[[ $# == 1 && $(dpkg --print-architecture) == amd64 ]] || {
  echo 'usage: install-tensorrt-sdk.sh <apt-archive-directory> (amd64 only)' >&2
  exit 2
}
archives=$(realpath -m "$1")
mkdir -p "$archives"
privileged=()
if [[ $(id -u) != 0 ]]; then privileged=(sudo); fi

keyring=$(mktemp --suffix=.deb)
trap 'rm -f "$keyring"' EXIT
curl --fail --silent --show-error --location --retry 3 --max-time 120 \
  "$NVIDIA_APT_URL/cuda-keyring_${NVIDIA_KEYRING_VERSION}_all.deb" -o "$keyring"
printf '%s  %s\n' "$NVIDIA_KEYRING_SHA256" "$keyring" | sha256sum --check
"${privileged[@]}" dpkg -i "$keyring"
tools/ci/apt-get.sh -o Acquire::Retries=3 -o Acquire::https::Timeout=30 update

# APT authenticates the repository and checks cached archives against its
# package hashes. Keep download and unpack timing separate on hits and misses.
start=$SECONDS
APT_BOUND_SECONDS=600 tools/ci/apt-get.sh \
  -o "Dir::Cache::archives=$archives" -o Acquire::Retries=3 \
  -o Acquire::https::Timeout=30 install -y --allow-downgrades --no-install-recommends \
  --download-only "${TENSORRT_SDK_PACKAGES[@]}"
download_seconds=$((SECONDS - start))
archive_bytes=$(find "$archives" -maxdepth 1 -name '*.deb' -printf '%s\n' | awk '{s += $1} END {printf "%.0f", s}')
start=$SECONDS
APT_BOUND_SECONDS=600 tools/ci/apt-get.sh \
  -o "Dir::Cache::archives=$archives" install -y --allow-downgrades --no-install-recommends \
  --no-download "${TENSORRT_SDK_PACKAGES[@]}"
install_seconds=$((SECONDS - start))

for pin in "${TENSORRT_SDK_PACKAGES[@]}"; do
  actual=$(dpkg-query -W -f='${Version}' "${pin%%=*}")
  [[ $actual == "${pin#*=}" ]] || { echo "version mismatch: $pin" >&2; exit 1; }
  printf '%s\n' "$pin"
done
printf 'CUDAToolkit_ROOT=%s\n' "$CUDAToolkit_ROOT" >>"${GITHUB_ENV:-/dev/null}"
{
  echo '### TensorRT SDK packages'
  printf 'Archive cache hit: %s; archive bytes: %s; apt download/check: %ss; install: %ss.\n' \
    "${SDK_CACHE_HIT:-false}" "$archive_bytes" "$download_seconds" "$install_seconds"
  echo 'Repository/keyring setup and Actions cache transfer time are separate steps.'
} | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
