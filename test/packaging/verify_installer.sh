#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: release installer verifier.

set -eu

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
installer="${repo_root}/packaging/scripts/install.sh"

td="$(mktemp -d)"
cleanup() { rm -rf "${td}"; }
trap cleanup EXIT

cat >"${td}/os-release.supported" <<'EOF'
ID=ubuntu
VERSION_ID="22.04"
EOF

cat >"${td}/os-release.unsupported" <<'EOF'
ID=debian
VERSION_ID="12"
EOF

cat >"${td}/nv-tegra.supported" <<'EOF'
# R36 (release), REVISION: 4.0
EOF

cat >"${td}/nv-tegra.unsupported" <<'EOF'
# R35 (release), REVISION: 4.1
EOF

printf 'NVIDIA Jetson Orin Nano Developer Kit\0' >"${td}/model.supported"
printf 'NVIDIA Jetson Unknown Developer Kit\0' >"${td}/model.unknown"
mkdir -p "${td}/self-check"
cp "${installer}" "${td}/self-check/install.sh"
(cd "${td}/self-check" && sha256sum install.sh >SHA256SUMS)
: >"${td}/self-check/SHA256SUMS.cosign.bundle"

mkdir -p "${td}/bin"
cat >"${td}/bin/cosign-ok" <<'STUB'
#!/bin/sh
# cosign stub: accept a verify-blob invocation and report success.
[ "$1" = "verify-blob" ] || { echo "unexpected cosign subcommand: $1" >&2; exit 2; }
echo "Verified OK" >&2
exit 0
STUB
chmod +x "${td}/bin/cosign-ok"

bootstrap_asset=""
bootstrap_checksum_var=""
case "$(uname -m)" in
  x86_64|amd64)
    bootstrap_asset="cosign-linux-amd64"
    bootstrap_checksum_var="TP_INSTALL_COSIGN_LINUX_AMD64_SHA256"
    ;;
  aarch64|arm64)
    bootstrap_asset="cosign-linux-arm64"
    bootstrap_checksum_var="TP_INSTALL_COSIGN_LINUX_ARM64_SHA256"
    ;;
esac
if [ -n "${bootstrap_asset}" ]; then
  mkdir -p "${td}/cosign-release"
  cp "${td}/bin/cosign-ok" "${td}/cosign-release/${bootstrap_asset}"
  bootstrap_checksum="$(sha256sum "${td}/cosign-release/${bootstrap_asset}" | awk '{print $1}')"
fi

bash -n "${installer}"
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "${installer}"
else
  echo "verify_installer: shellcheck not found; skipping shellcheck"
fi

TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
TP_INSTALL_ARCH="aarch64" \
TP_INSTALL_DEB_ARCH="arm64" \
  bash "${installer}" --dry-run --yes >"${td}/supported.out" 2>"${td}/supported.err"
grep -q "Would download:" "${td}/supported.out"
grep -q "Install mode: runtime" "${td}/supported.out"

if TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.unsupported" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.unsupported" \
   TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
   TP_INSTALL_ARCH="aarch64" \
   TP_INSTALL_DEB_ARCH="arm64" \
     bash "${installer}" --dry-run --yes >"${td}/unsupported.out" 2>"${td}/unsupported.err"; then
  echo "FAIL: unsupported OS dry-run unexpectedly passed" >&2
  exit 1
fi
grep -q "unsupported OS" "${td}/unsupported.err"

TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.unsupported" \
TP_INSTALL_OS_RELEASE="${td}/os-release.unsupported" \
TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
TP_INSTALL_ARCH="aarch64" \
TP_INSTALL_DEB_ARCH="arm64" \
  bash "${installer}" --dry-run --force-os --yes >"${td}/force-os.out" 2>"${td}/force-os.err"
grep -q -- "--force-os was provided" "${td}/force-os.err"

TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
TP_INSTALL_DEVICE_MODEL="${td}/model.unknown" \
TP_INSTALL_ARCH="x86_64" \
TP_INSTALL_DEB_ARCH="amd64" \
  bash "${installer}" --dry-run --yes >"${td}/hardware-warn.out" 2>"${td}/hardware-warn.err"
grep -q "architecture x86_64 is not arm64/aarch64" "${td}/hardware-warn.err"
grep -q "unrecognized Jetson model" "${td}/hardware-warn.err"

if TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
   TP_INSTALL_DEVICE_MODEL="${td}/model.unknown" \
   TP_INSTALL_ARCH="x86_64" \
   TP_INSTALL_DEB_ARCH="amd64" \
     bash "${installer}" --dry-run --strict-hardware --yes >"${td}/strict.out" 2>"${td}/strict.err"; then
  echo "FAIL: strict hardware dry-run unexpectedly passed" >&2
  exit 1
fi
grep -q "strict-hardware" "${td}/strict.err"

# Self-check authenticates SHA256SUMS (cosign) before checking install.sh,
# then proceeds to the root requirement.
if TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
   TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
   TP_INSTALL_ARCH="aarch64" \
   TP_INSTALL_DEB_ARCH="arm64" \
   TP_INSTALL_COSIGN="${td}/bin/cosign-ok" \
     bash "${td}/self-check/install.sh" --yes >"${td}/self-check.out" 2>"${td}/self-check.err"; then
  echo "FAIL: self-check install unexpectedly passed without root" >&2
  exit 1
fi
grep -q "verifying SHA256SUMS signature with cosign" "${td}/self-check.out"
grep -q "SHA256SUMS signature verified" "${td}/self-check.out"
grep -q "verifying install.sh with SHA256SUMS" "${td}/self-check.out"
grep -q "install.sh: OK" "${td}/self-check.out"
grep -q "run as root" "${td}/self-check.err"

# When cosign is missing, the installer can bootstrap a pinned transient
# binary before verifying SHA256SUMS.
if [ -n "${bootstrap_asset}" ]; then
  if env \
     TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
     TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
     TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
     TP_INSTALL_ARCH="aarch64" \
     TP_INSTALL_DEB_ARCH="arm64" \
     TP_INSTALL_FORCE_COSIGN_BOOTSTRAP=1 \
     TP_INSTALL_UNSAFE_TEST_HOOKS=1 \
     TP_INSTALL_COSIGN_BOOTSTRAP_ASSET="${bootstrap_asset}" \
     TP_INSTALL_COSIGN_BASE_URL="file://${td}/cosign-release" \
     "${bootstrap_checksum_var}=${bootstrap_checksum}" \
       bash "${td}/self-check/install.sh" --yes >"${td}/bootstrap.out" 2>"${td}/bootstrap.err"; then
    echo "FAIL: bootstrap self-check install unexpectedly passed without root" >&2
    exit 1
  fi
  grep -q "downloading pinned ${bootstrap_asset}" "${td}/bootstrap.out"
  grep -q "${bootstrap_asset}: OK" "${td}/bootstrap.out"
  grep -q "SHA256SUMS signature verified" "${td}/bootstrap.out"
  grep -q "run as root" "${td}/bootstrap.err"
fi

# Signature verification fails closed when cosign is unavailable, bootstrap is
# disabled, and the operator has not explicitly opted out.
if TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
   TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
   TP_INSTALL_ARCH="aarch64" \
   TP_INSTALL_DEB_ARCH="arm64" \
   TP_INSTALL_COSIGN="${td}/bin/cosign-absent" \
   TP_INSTALL_COSIGN_BOOTSTRAP=0 \
     bash "${td}/self-check/install.sh" --yes >"${td}/nocosign.out" 2>"${td}/nocosign.err"; then
  echo "FAIL: install without cosign unexpectedly passed" >&2
  exit 1
fi
grep -q "configured cosign binary was not found" "${td}/nocosign.err"
if grep -q "run as root" "${td}/nocosign.err"; then
  echo "FAIL: install proceeded past the signature gate without cosign" >&2
  exit 1
fi

# --allow-unsigned proceeds without cosign and without a published signature.
TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
TP_INSTALL_ARCH="aarch64" \
TP_INSTALL_DEB_ARCH="arm64" \
TP_INSTALL_COSIGN="${td}/bin/cosign-absent" \
  bash "${td}/self-check/install.sh" --yes --allow-unsigned >"${td}/unsigned.out" 2>"${td}/unsigned.err" || true
grep -q "signature verification disabled" "${td}/unsigned.err"
grep -q "run as root" "${td}/unsigned.err"

TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.unsupported" \
TP_INSTALL_OS_RELEASE="${td}/os-release.unsupported" \
TP_INSTALL_DEVICE_MODEL="${td}/model.unknown" \
TP_INSTALL_ARCH="x86_64" \
TP_INSTALL_DEB_ARCH="amd64" \
  bash "${installer}" --dry-run --cli-only --yes >"${td}/cli-only.out" 2>"${td}/cli-only.err"
grep -q "CLI-only mode selected" "${td}/cli-only.out"
grep -q "Install mode: cli" "${td}/cli-only.out"
grep -q "tensorplate-common and tensorplate-cli package assets" "${td}/cli-only.out"

if bash "${installer}" --dry-run --cli-only --with-python-backend >"${td}/cli-conflict.out" 2>"${td}/cli-conflict.err"; then
  echo "FAIL: --cli-only with --with-python-backend unexpectedly passed" >&2
  exit 1
fi
grep -q "cannot be combined" "${td}/cli-conflict.err"

echo "verify_installer: ok"
