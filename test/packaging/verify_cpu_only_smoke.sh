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
# The x86_64 serving worker is configured from the profile the release
# workflow's amd64 job reads: compiler, DWARF version and backends.
# shellcheck source=tools/release/amd64-build-profile.sh disable=SC1091
. tools/release/amd64-build-profile.sh
CC="$TP_AMD64_CC" CXX="$TP_AMD64_CXX" cmake -S . -B build/release -G Ninja \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DTP_BUILD_TESTS=OFF -DTP_BUILD_EXAMPLES=OFF -DTP_ENABLE_SANITIZERS=OFF \
  "${TP_AMD64_CMAKE_ARGS[@]}" >/dev/null || die "C++ configure failed"
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

# The row declares a python_pytorch backend path over apt, and the backend
# package deliberately does not depend on PyTorch — the operator provides it.
# Doctor probes the backend runtime unconditionally on Linux, so without this
# the probe reports a critical failure and doctor exits non-zero. Installing it
# turns that into positive evidence that the row's declared path works.
note "2b. provide PyTorch for the declared backend path"
/usr/bin/python3 -m pip install --quiet --upgrade pip >/dev/null 2>&1 || true
# The CPU index matters: plain `pip install torch` on Linux resolves to the
# CUDA build and pulls gigabytes of NVIDIA wheels onto an accelerator-less host.
/usr/bin/python3 -m pip install --quiet \
  --index-url https://download.pytorch.org/whl/cpu 'torch>=2.1' ||
  die "installing the CPU PyTorch wheel failed"
/usr/bin/python3 -c 'import torch; print("torch", torch.__version__)' > "${evidence}/torch.txt" ||
  die "torch is not importable by the descriptor's interpreter"
pass "PyTorch available to /usr/bin/python3: $(cat "${evidence}/torch.txt")"

note "3. package closure and the per-architecture agent config"
[[ -x "$TP_SERVING_BINARY_PATH" ]] || die "serving binary missing at ${TP_SERVING_BINARY_PATH}"
[[ -r "$TP_PYTHON_PYTORCH_BACKEND_DESCRIPTOR" ]] ||
  die "backend descriptor missing at ${TP_PYTHON_PYTORCH_BACKEND_DESCRIPTOR}"
[[ -x /usr/bin/tensorplate-backend-python-pytorch ]] ||
  die "backend entrypoint missing at /usr/bin/tensorplate-backend-python-pytorch"
[[ -d "${TP_PLATFORM_REGISTRY_DIR}/rows" ]] ||
  die "platform registry rows missing under ${TP_PLATFORM_REGISTRY_DIR}"
# The agent config is per-architecture; prove the x86_64 variant is the
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
python3 - "${evidence}/doctor.json" "$EXPECTED_ROW" "$TP_AGENT_SOCKET_PATH" "$TP_CLI_CONFIG_PATH" <<'PY'
import json, sys
path, expected_row, packaged_socket, cli_config = sys.argv[1:]
envelope = json.load(open(path))
payload = envelope["payload"]
by_id = {f["id"]: f for f in payload["findings"]}

assert payload["failing"] == 0, f"doctor reports {payload['failing']} failing finding(s)"

# Live detection, not a fixture: this host must resolve to the row under test.
profile = by_id["platform_profile"]
assert profile["status"] == "ok", f"platform_profile is {profile['status']}: {profile['message']}"
assert expected_row in profile["message"], \
    f"platform_profile did not name {expected_row}: {profile['message']}"

for required_ok in ("platform_registry", "agent_reachable", "agent_socket",
                    "serving_binary_installed", "python_pytorch_backend",
                    "python_pytorch_runtime", "path_layout", "config_files"):
    f = by_id[required_ok]
    assert f["status"] == "ok", f"{required_ok} is {f['status']}: {f['message']}"

# The whole of issue #203, on the only host that can answer it: with neither
# --config nor $TENSORPLATE_CLI_CONFIG set, did the CLI read the conffile the
# package installed? Doctor takes the socket from the resolved profile, and the
# packaged and built-in values differ -- /run/... against /var/run/... -- so
# this finding's message is what tells a fixed CLI from the broken one. The
# `/var/run -> /run` symlink means BOTH paths reach the live socket and both
# report `ok`, which is exactly why the status alone proves nothing. The
# message is the evidence line quoted in #203.
socket = by_id["agent_socket"]
assert packaged_socket in socket["message"], (
    f"doctor reports socket from the built-in defaults, not {cli_config}: {socket['message']}"
)
# `assert "/run/..." in message` would also pass on "/var/run/...", so rule the
# built-in default out by name rather than relying on the substring.
assert "/var/run/tensorplate" not in socket["message"], (
    f"doctor reports the built-in default socket; {cli_config} was not read: {socket['message']}"
)

# A packaged config that was found and not used is reported in the envelope,
# never silently. Running as root, it must have been readable, so there is
# nothing to report.
assert not envelope.get("warnings"), \
    f"doctor warned about the packaged config while running as root: {envelope['warnings']}"

# The row carries no model-performance claim, so nothing doctor prints may
# describe this host as Production.
blob = json.dumps(payload).lower()
assert "production" not in blob, "doctor output makes a Production claim on a Preview row"
print(f"doctor: {len(payload['findings'])} findings, 0 failing, row {expected_row}")
print(f"doctor: agent socket from {cli_config}: {socket['message']}")
PY
pass "doctor green; ${EXPECTED_ROW} resolved by live detection; packaged cli.json in effect"

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
# `tensorplate logs` reads NDJSON files, and this install writes none: both
# services log to the journal, so the packaged cli config names no
# log_source.path. The documented answer is exit 6 (`unavailable`) naming
# the journalctl command to run instead -- never an empty successful read,
# and never the exit 2 this returned while the CLI ignored the packaged
# config. A host whose operator did configure a source reads it and exits
# 0. Any other status is a regression, so the status is checked, not
# swallowed.
logs_status=0
tensorplate logs --component agent --tail 20 > "${evidence}/agent.log" 2>&1 ||
  logs_status=$?
case "$logs_status" in
  0)
    # A site that set log_source.path once a component writes one. Exit 0
    # with nothing printed is the silent case this check exists to rule
    # out: the agent is a separate process from the only NDJSON writer, so
    # `--component agent` can be empty forever. Require either entries or
    # the note that says why there are none.
    if grep -q 'entries=0' "${evidence}/agent.log"; then
      grep -q 'read 0 entries' "${evidence}/agent.log" ||
        die "tensorplate logs read 0 entries and said nothing about why"
      pass "log path present; an empty read named its source and the reason"
    else
      pass "log path present; a configured NDJSON source returned entries"
    fi
    ;;
  6)
    grep -q 'journalctl -u tensorplate-agent' "${evidence}/agent.log" ||
      die "tensorplate logs exited 6 without naming the journal to read instead"
    pass "log path present; logs exited 6 and named the journal"
    ;;
  *) die "tensorplate logs exited ${logs_status}; expected 0 or 6" ;;
esac

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
