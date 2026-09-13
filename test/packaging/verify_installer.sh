#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: release installer verifier.

set -eu

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
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

# The x86_64 runtime platform. Each architecture is held to exactly one
# OS, so 22.04 is the supported spelling on a Jetson and an unsupported
# one on x86_64, and 24.04 is the reverse.
cat >"${td}/os-release.noble" <<'EOF'
ID=ubuntu
VERSION_ID="24.04"
EOF

printf 'NVRM version: NVIDIA UNIX x86_64 Kernel Module  560.35.03\n' >"${td}/nvidia-version"

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

# Probe fixtures must not inherit a developer GPU's nvidia-smi. Keep the
# real tools needed by dry-run on an isolated PATH, then add the NVIDIA
# fixture only for cases that explicitly need it.
probe_tools="${td}/probe-tools"
probe_nvidia="${td}/probe-nvidia"
mkdir -p "$probe_tools" "$probe_nvidia"
for tool in awk bash curl python3 sha256sum; do
  ln -s "$(command -v "$tool")" "${probe_tools}/${tool}"
done
cat >"${probe_nvidia}/nvidia-smi" <<'STUB'
#!/bin/sh
printf '%s\n' "$*" >>"${TP_TEST_NVIDIA_LOG}"
[ "$#" -eq 2 ] && [ "$1" = "--query-gpu=driver_version" ] &&
  [ "$2" = "--format=csv,noheader" ] || exit 2
if [ "${TP_TEST_NVIDIA_EXIT}" -eq 0 ]; then
  printf '560.35.03\n'
fi
exit "${TP_TEST_NVIDIA_EXIT}"
STUB
chmod +x "${probe_nvidia}/nvidia-smi"

run_x86_probe() {
  probe_path="$1" probe_driver="$2" probe_exit="$3"
  shift 3
  PATH="$probe_path" \
  TP_INSTALL_NV_TEGRA_RELEASE="${td}/absent-nv-tegra" \
  TP_INSTALL_OS_RELEASE="${td}/os-release.noble" \
  TP_INSTALL_NVIDIA_VERSION="$probe_driver" \
  TP_INSTALL_ARCH="x86_64" \
  TP_INSTALL_DEB_ARCH="amd64" \
  TP_TEST_NVIDIA_LOG="${td}/nvidia-query.log" \
  TP_TEST_NVIDIA_EXIT="$probe_exit" \
    bash "${installer}" --dry-run --yes "$@"
}

assert_nvidia_query() {
  if [ "$(cat "${td}/nvidia-query.log")" != "--query-gpu=driver_version --format=csv,noheader" ]; then
    echo "FAIL: NVIDIA fallback must query the driver exactly once" >&2
    exit 1
  fi
}

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

mkdir -p "${td}/local-artifacts"
cat >"${td}/local-artifacts/tensorplate-snapshot-develop-deadbeef1234-artifacts.json" <<'EOF'
{
  "schema": "https://tensorplate.com/schemas/release-artifact-manifest-v1.json",
  "release": {
    "project": "tensorplate",
    "version": "0.1.0~dev.20260604.deadbeef1234",
    "tag": "snapshot-develop-deadbeef1234",
    "provenance": "local-source-snapshot",
    "unreleased": true
  },
  "artifacts": []
}
EOF
: >"${td}/local-artifacts/SHA256SUMS"
TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
TP_INSTALL_ARCH="aarch64" \
TP_INSTALL_DEB_ARCH="arm64" \
  bash "${installer}" --dry-run --yes --local-artifacts "${td}/local-artifacts" --allow-unsigned >"${td}/local-artifacts.out" 2>"${td}/local-artifacts.err"
grep -q "dry-run selected snapshot-develop-deadbeef1234" "${td}/local-artifacts.out"
grep -q "Would install from local artifacts: ${td}/local-artifacts" "${td}/local-artifacts.out"
grep -q "Signature check: DISABLED" "${td}/local-artifacts.out"

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
TP_INSTALL_ARCH="aarch64" \
TP_INSTALL_DEB_ARCH="arm64" \
  bash "${installer}" --dry-run --yes >"${td}/hardware-warn.out" 2>"${td}/hardware-warn.err"
grep -q "unrecognized Jetson model" "${td}/hardware-warn.err"

if TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
   TP_INSTALL_DEVICE_MODEL="${td}/model.unknown" \
   TP_INSTALL_ARCH="aarch64" \
   TP_INSTALL_DEB_ARCH="arm64" \
     bash "${installer}" --dry-run --strict-hardware --yes >"${td}/strict.out" 2>"${td}/strict.err"; then
  echo "FAIL: strict hardware dry-run unexpectedly passed" >&2
  exit 1
fi
grep -q "strict-hardware" "${td}/strict.err"

# The x86_64 runtime platform: Ubuntu 24.04, with no Jetson metadata on
# the host. Demanding L4T release files of an x86_64 host is what made
# this platform uninstallable.
TP_INSTALL_NV_TEGRA_RELEASE="${td}/absent-nv-tegra" \
TP_INSTALL_OS_RELEASE="${td}/os-release.noble" \
TP_INSTALL_NVIDIA_VERSION="${td}/nvidia-version" \
TP_INSTALL_ARCH="x86_64" \
TP_INSTALL_DEB_ARCH="amd64" \
  bash "${installer}" --dry-run --yes >"${td}/x86.out" 2>"${td}/x86.err"
grep -q "Ubuntu 24.04 on x86_64 detected" "${td}/x86.out"
grep -q "Would download:" "${td}/x86.out"
grep -q "Install mode: runtime" "${td}/x86.out"
if grep -qi "jetson\|L4T" "${td}/x86.err"; then
  echo "FAIL: x86_64 host was held to Jetson expectations" >&2
  exit 1
fi

# Each architecture is held to its own OS: the Jetson spelling of Ubuntu
# is not accepted on x86_64, and 24.04 does not satisfy the Jetson
# platform just because the widening happened.
if TP_INSTALL_NV_TEGRA_RELEASE="${td}/absent-nv-tegra" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.supported" \
   TP_INSTALL_NVIDIA_VERSION="${td}/nvidia-version" \
   TP_INSTALL_ARCH="x86_64" \
   TP_INSTALL_DEB_ARCH="amd64" \
     bash "${installer}" --dry-run --yes >"${td}/x86-jammy.out" 2>"${td}/x86-jammy.err"; then
  echo "FAIL: x86_64 dry-run on 22.04 unexpectedly passed" >&2
  exit 1
fi
grep -q "expected ubuntu 24.04" "${td}/x86-jammy.err"

if TP_INSTALL_NV_TEGRA_RELEASE="${td}/absent-nv-tegra" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.noble" \
   TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
   TP_INSTALL_ARCH="aarch64" \
   TP_INSTALL_DEB_ARCH="arm64" \
     bash "${installer}" --dry-run --yes >"${td}/arm-noble.out" 2>"${td}/arm-noble.err"; then
  echo "FAIL: aarch64 dry-run without L4T metadata unexpectedly passed" >&2
  exit 1
fi
grep -q "expected NVIDIA Jetson L4T release metadata" "${td}/arm-noble.err"

# Isolate the Jetson OS check from its separate L4T check: valid R36
# metadata must not make Ubuntu 24.04 acceptable on arm64.
if TP_INSTALL_NV_TEGRA_RELEASE="${td}/nv-tegra.supported" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.noble" \
   TP_INSTALL_DEVICE_MODEL="${td}/model.supported" \
   TP_INSTALL_ARCH="aarch64" \
   TP_INSTALL_DEB_ARCH="arm64" \
     bash "${installer}" --dry-run --yes >"${td}/arm-noble-r36.out" 2>"${td}/arm-noble-r36.err"; then
  echo "FAIL: aarch64 dry-run on 24.04 unexpectedly passed with valid L4T" >&2
  exit 1
fi
grep -q "expected ubuntu 22.04" "${td}/arm-noble-r36.err"

# The GPU check on x86_64 is advisory, like the Jetson model check, and
# fatal only under --strict-hardware. An absent tool and a tool whose
# query fails are distinct inputs; neither is a usable fallback driver.
: >"${td}/nvidia-query.log"
run_x86_probe "$probe_tools" "${td}/absent-nvidia-version" 9 \
  >"${td}/x86-nogpu.out" 2>"${td}/x86-nogpu.err"
grep -q "no NVIDIA driver found" "${td}/x86-nogpu.err"

if run_x86_probe "$probe_tools" "${td}/absent-nvidia-version" 9 --strict-hardware \
  >"${td}/x86-strict.out" 2>"${td}/x86-strict.err"; then
  echo "FAIL: x86_64 strict hardware dry-run without a driver unexpectedly passed" >&2
  exit 1
fi
grep -q "strict-hardware" "${td}/x86-strict.err"
[ ! -s "${td}/nvidia-query.log" ]

for strict in advisory strict; do
  set --
  if [ "$strict" = strict ]; then
    set -- --strict-hardware
  fi
  : >"${td}/nvidia-query.log"
  if run_x86_probe "${probe_nvidia}:${probe_tools}" "${td}/absent-nvidia-version" 9 "$@" \
    >"${td}/x86-broken-${strict}.out" 2>"${td}/x86-broken-${strict}.err"; then
    if [ "$strict" = strict ]; then
      echo "FAIL: strict hardware accepted a failed NVIDIA driver query" >&2
      exit 1
    fi
  elif [ "$strict" = advisory ]; then
    echo "FAIL: advisory hardware refused a failed NVIDIA driver query" >&2
    exit 1
  fi
  grep -q "no NVIDIA driver found" "${td}/x86-broken-${strict}.err"
  if [ "$strict" = strict ]; then
    grep -q "strict-hardware" "${td}/x86-broken-${strict}.err"
  fi
  assert_nvidia_query

  : >"${td}/nvidia-query.log"
  run_x86_probe "${probe_nvidia}:${probe_tools}" "${td}/absent-nvidia-version" 0 "$@" \
    >"${td}/x86-query-${strict}.out" 2>"${td}/x86-query-${strict}.err"
  grep -q "hardware validation passed" "${td}/x86-query-${strict}.out"
  if grep -q "no NVIDIA driver found" "${td}/x86-query-${strict}.err"; then
    echo "FAIL: successful NVIDIA query was reported as missing a driver" >&2
    exit 1
  fi
  assert_nvidia_query
done

# A readable kernel driver file remains sufficient. The fallback is not
# invoked, even if a present nvidia-smi would fail its query.
: >"${td}/nvidia-query.log"
run_x86_probe "${probe_nvidia}:${probe_tools}" "${td}/nvidia-version" 9 --strict-hardware \
  >"${td}/x86-driver-file.out" 2>"${td}/x86-driver-file.err"
grep -q "hardware validation passed" "${td}/x86-driver-file.out"
[ ! -s "${td}/nvidia-query.log" ]

# An architecture that is neither runtime platform is refused by name,
# rather than falling through one platform's checks.
if TP_INSTALL_NV_TEGRA_RELEASE="${td}/absent-nv-tegra" \
   TP_INSTALL_OS_RELEASE="${td}/os-release.noble" \
   TP_INSTALL_ARCH="riscv64" \
   TP_INSTALL_DEB_ARCH="riscv64" \
     bash "${installer}" --dry-run --yes >"${td}/riscv.out" 2>"${td}/riscv.err"; then
  echo "FAIL: unsupported architecture dry-run unexpectedly passed" >&2
  exit 1
fi
grep -q "not a supported runtime architecture" "${td}/riscv.err"

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
