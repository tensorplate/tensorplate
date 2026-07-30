#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Ubuntu x86_64 CPU-only Preview smoke: install, CLI, packaging, and control
# plane on a host with no accelerator.
#
# This is the only check that installs REAL binaries and runs the REAL CLI. The
# other packaging checks stub the runtime so they can rehearse packaging shape
# cheaply; that cannot tell you whether the installed appliance actually comes
# up. Here the agent and observability services start, `tensorplate doctor` runs
# against them, and the row this host matches is resolved by live detection
# rather than from a recorded fixture.
#
# The row is Preview and carries no model-performance claim, so nothing here
# deploys or infers. It asserts the four things the row does claim: install,
# CLI, packaging, and control plane.
#
# THIS SCRIPT MUTATES THE HOST (builds and installs system packages, creates a
# system user, starts services). Run it only on a disposable host: the CI runner
# or a container. It refuses unless CI=true or TP_CPU_SMOKE_ALLOW=1.

set -Eeuo pipefail

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
note() { printf '==> %s\n' "$*"; }
pass() { printf 'PASS: %s\n' "$*"; }

[[ "${CI:-}" == "true" || "${TP_CPU_SMOKE_ALLOW:-0}" == "1" ]] ||
  die "this smoke installs system packages; run on a disposable host with TP_CPU_SMOKE_ALLOW=1"
[[ "$(id -u)" -eq 0 ]] || die "run as root (dpkg and systemd operations)"

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git repository"
cd "$repo_root"
. packaging/scripts/path-constants.sh

# The row under test names an exact OS and architecture. Running this anywhere
# else would produce a green result that says nothing about the row, so pin the
# environment rather than adapting to it.
EXPECTED_ROW="${TP_CPU_SMOKE_ROW:-ubuntu2204-x86-cpu}"
EXPECTED_OS_VERSION="${TP_CPU_SMOKE_OS_VERSION:-22.04}"
host_arch="$(dpkg --print-architecture)"
os_id="$(. /etc/os-release && printf '%s' "$ID")"
os_version="$(. /etc/os-release && printf '%s' "$VERSION_ID")"
[[ "$host_arch" == "amd64" ]] ||
  die "this smoke validates an x86_64 row; host reports ${host_arch}"
[[ "$os_id" == "ubuntu" && "$os_version" == "$EXPECTED_OS_VERSION" ]] ||
  die "expected Ubuntu ${EXPECTED_OS_VERSION}; host reports ${os_id} ${os_version}"
# An accelerator would make this a different row.
if [[ -e /dev/nvidia0 ]] || command -v nvidia-smi >/dev/null 2>&1; then
  die "an NVIDIA accelerator is present; this is the CPU-only row's smoke"
fi

evidence="${TP_CPU_SMOKE_EVIDENCE:-${repo_root}/dist/smoke/${EXPECTED_ROW}}"
mkdir -p "$evidence"

note "0. host: ${os_id} ${os_version} ${host_arch}, row ${EXPECTED_ROW}"
{
  printf 'row: %s\n' "$EXPECTED_ROW"
  printf 'os: %s %s\n' "$os_id" "$os_version"
  printf 'arch: %s\n' "$host_arch"
  printf 'kernel: %s\n' "$(uname -r)"
  printf 'cpu: %s\n' "$(grep -m1 '^model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//' || echo unknown)"
} > "${evidence}/host-facts.txt"

note "1. build the real runtime binaries"
cargo build --release \
  --bin tensorplate-agent \
  --bin tensorplate-observability \
  --bin tensorplate
# TensorRT stays off: this host has no accelerator and the x86_64 serving
# package is built without the adapter (see the release workflow).
# -gdwarf-4: clang emits DWARF 5, whose .debug_addr section jammy's dwz (0.14)
# cannot read, and dh_dwz turns that into a hard dpkg-buildpackage failure.
cmake -S . -B build/release -G Ninja \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DCMAKE_CXX_FLAGS=-gdwarf-4 \
  -DTP_BUILD_TESTS=OFF -DTP_BUILD_EXAMPLES=OFF -DTP_ENABLE_SANITIZERS=OFF \
  -DTP_ENABLE_TENSORRT=OFF -DTP_REQUIRE_TENSORRT_SDK=OFF \
  -DTP_ENABLE_LIBTORCH=OFF -DTP_ENABLE_PYTHON_PYTORCH_SIDECAR=ON >/dev/null
cmake --build build/release --target tp_serving_worker --parallel >/dev/null
if [[ -x build/release/serving_worker/tensorplate-serving ]]; then
  install -m 0755 build/release/serving_worker/tensorplate-serving build/release/tensorplate-serving
fi
[[ -x build/release/tensorplate-serving ]] || die "serving worker was not staged"
pass "real binaries built"

note "2. build and install the package set"
work="$(mktemp -d)"
cleanup() { rm -rf "$work"; }
trap cleanup EXIT
packaging/scripts/build-deb.sh >"${work}/build.log" 2>&1 ||
  { tail -20 "${work}/build.log" >&2; die "package build failed"; }
version="$(dpkg-parsechangelog -l packaging/debian/changelog -S Version)"
repo_parent="$(dirname "$repo_root")"
# tensorplate-common carries the layout helpers every other package
# Pre-Depends on, so it must be configured before the rest.
dpkg -i "${repo_parent}/tensorplate-common_${version}_all.deb" >>"${work}/install.log" 2>&1 ||
  { tail -20 "${work}/install.log" >&2; die "installing tensorplate-common failed"; }
dpkg -i \
  "${repo_parent}"/tensorplate-agent_"${version}"_"${host_arch}".deb \
  "${repo_parent}"/tensorplate-serving_"${version}"_"${host_arch}".deb \
  "${repo_parent}"/tensorplate-observability_"${version}"_"${host_arch}".deb \
  "${repo_parent}"/tensorplate-cli_"${version}"_"${host_arch}".deb \
  "${repo_parent}/tensorplate-backend-python-pytorch_${version}_all.deb" \
  >>"${work}/install.log" 2>&1 ||
  { tail -30 "${work}/install.log" >&2; die "installing the runtime set failed"; }
dpkg -l 'tensorplate*' > "${evidence}/packages.txt"
pass "package set installed at ${version}"

note "3. package closure and the per-architecture agent config"
[[ -x "$TP_SERVING_BINARY_PATH" ]] || die "serving binary missing at ${TP_SERVING_BINARY_PATH}"
[[ -r "$TP_PYTHON_PYTORCH_BACKEND_DESCRIPTOR" ]] ||
  die "backend descriptor missing at ${TP_PYTHON_PYTORCH_BACKEND_DESCRIPTOR}"
[[ -x /usr/bin/tensorplate-backend-python-pytorch ]] ||
  die "backend entrypoint missing at /usr/bin/tensorplate-backend-python-pytorch"
[[ -d "${TP_PLATFORM_REGISTRY_DIR}/rows" ]] ||
  die "platform registry rows missing under ${TP_PLATFORM_REGISTRY_DIR}"
# PR-10 made the agent config per-architecture; prove the x86_64 variant is the
# one a real install lands, not just the one the build produced.
grep -q '"device_family": "x86_64"' "$TP_AGENT_CONFIG_PATH" ||
  die "${TP_AGENT_CONFIG_PATH} is not the x86_64 variant"
if grep -q '"tensorrt"' "$TP_AGENT_CONFIG_PATH"; then
  die "${TP_AGENT_CONFIG_PATH} advertises tensorrt; this build has no TensorRT adapter"
fi
pass "package closure holds and the agent config is the x86_64 variant"

note "4. the installed registry reports this row as Preview"
support_level="$(python3 - "${TP_PLATFORM_REGISTRY_DIR}/rows/${EXPECTED_ROW}.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))["support_level"])
PY
)"
[[ "$support_level" == "Preview" ]] ||
  die "${EXPECTED_ROW} reports support_level=${support_level}; this row must stay Preview"
pass "${EXPECTED_ROW} is Preview in the installed registry"

note "5. start the control plane"
systemctl enable --now tensorplate-agent >/dev/null 2>&1 ||
  { systemctl status tensorplate-agent --no-pager >&2 || true; die "agent failed to start"; }
systemctl enable --now tensorplate-observability >/dev/null 2>&1 ||
  { systemctl status tensorplate-observability --no-pager >&2 || true; die "observability failed to start"; }
for _ in $(seq 1 30); do
  [[ -S "$TP_AGENT_SOCKET_PATH" ]] && break
  sleep 1
done
[[ -S "$TP_AGENT_SOCKET_PATH" ]] || {
  journalctl -u tensorplate-agent --no-pager -n 40 >&2 || true
  die "agent control socket did not appear at ${TP_AGENT_SOCKET_PATH}"
}
systemctl status tensorplate-agent --no-pager > "${evidence}/agent-status.txt" 2>&1 || true
systemctl status tensorplate-observability --no-pager > "${evidence}/observability-status.txt" 2>&1 || true
pass "agent and observability active; control socket present"

note "6. CLI smoke against the running control plane"
tensorplate version --output json > "${evidence}/version.json" ||
  die "tensorplate version failed"
# Doctor must be GREEN on a clean package-only install with the services up.
# Absent CUDA/TensorRT on this row are `missing`/info findings, not failures,
# so a non-zero exit here means something real.
# `$?` after `if ! cmd` is the negation's status, not the command's, so the
# exit code has to be captured before it is tested.
doctor_code=0
tensorplate doctor --output json > "${evidence}/doctor.json" 2>"${evidence}/doctor.err" ||
  doctor_code=$?
if ((doctor_code != 0)); then
  python3 - "${evidence}/doctor.json" >&2 <<'PY' || cat "${evidence}/doctor.err" >&2
import json, sys
for f in json.load(open(sys.argv[1]))["payload"]["findings"]:
    if f["status"] == "fail":
        print("  FAILING:", f["id"], "-", f["message"])
PY
  die "tensorplate doctor exited ${doctor_code} on a clean CPU-only install"
fi
python3 - "${evidence}/doctor.json" "$EXPECTED_ROW" <<'PY'
import json, sys
path, expected_row = sys.argv[1:]
payload = json.load(open(path))["payload"]
by_id = {f["id"]: f for f in payload["findings"]}

assert payload["failing"] == 0, f"doctor reports {payload['failing']} failing finding(s)"

# Live detection, not a fixture: this host must resolve to the row under test.
profile = by_id["platform_profile"]
assert profile["status"] == "ok", f"platform_profile is {profile['status']}: {profile['message']}"
assert expected_row in profile["message"], \
    f"platform_profile did not name {expected_row}: {profile['message']}"

for required_ok in ("platform_registry", "agent_reachable", "agent_socket",
                    "serving_binary_installed", "python_pytorch_backend",
                    "path_layout", "config_files"):
    f = by_id[required_ok]
    assert f["status"] == "ok", f"{required_ok} is {f['status']}: {f['message']}"

# The row carries no model-performance claim, so nothing doctor prints may
# describe this host as Production.
blob = json.dumps(payload).lower()
assert "production" not in blob, "doctor output makes a Production claim on a Preview row"
print(f"doctor: {len(payload['findings'])} findings, 0 failing, row {expected_row}")
PY
pass "doctor green; ${EXPECTED_ROW} resolved by live detection; no Production claim"

note "7. control-plane query"
tensorplate status --output json > "${evidence}/status.json" 2>"${evidence}/status.err" ||
  { cat "${evidence}/status.err" >&2; die "tensorplate status failed against the running agent"; }
python3 - "${evidence}/status.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("command") == "status", d
assert "payload" in d, d
print("status: control plane answered")
PY
pass "control plane answered a status query"

note "8. logs are reachable at the documented path"
[[ -d "$TP_LOG_DIR" ]] || die "log directory missing at ${TP_LOG_DIR}"
tensorplate logs --component agent --tail 20 > "${evidence}/agent.log" 2>&1 || true
pass "log path present"

{
  printf 'result: pass\n'
  printf 'version: %s\n' "$version"
  printf 'doctor findings: %s\n' "$(python3 - "${evidence}/doctor.json" <<'PY'
import json, sys
print(len(json.load(open(sys.argv[1]))["payload"]["findings"]))
PY
)"
} >> "${evidence}/host-facts.txt"

printf 'verify_cpu_only_smoke: ok (%s on %s %s %s); evidence in %s\n' \
  "$EXPECTED_ROW" "$os_id" "$os_version" "$host_arch" "$evidence"
