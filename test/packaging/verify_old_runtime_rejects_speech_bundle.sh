#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Run on an Ubuntu 22.04 amd64 CPU host. Extracts, never installs, the
# released agent; all writable state and its private socket are temporary.
set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo 'old-runtime compatibility requires Linux amd64' >&2
  exit 1
fi
for tool in python3 dpkg-deb sha256sum; do
  command -v "$tool" >/dev/null || { echo "missing dependency: $tool" >&2; exit 1; }
done
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
asset=tensorplate-agent_0.2.1-1_amd64.deb
if [[ -n "${TP_RELEASE_ASSET_DIR:-}" ]]; then
  cp "${TP_RELEASE_ASSET_DIR}/${asset}" "${scratch}/${asset}"
else
  curl --fail --location --silent --show-error --connect-timeout 15 \
    --max-time 180 --retry 2 --retry-max-time 300 \
    "https://github.com/tensorplate/tensorplate/releases/download/v0.2.1/${asset}" \
    --output "${scratch}/${asset}"
fi
# Published v0.2.1 SHA256SUMS: also binds offline input to the real release.
(cd "$scratch" && printf '%s  %s\n' \
  b007d0a15c09a123165dc7041051385c39bb429a9f3095642045dc51b7713630 "$asset" \
  | sha256sum --check --status)
[[ "$(dpkg-deb -f "${scratch}/${asset}" Package)" == tensorplate-agent ]]
[[ "$(dpkg-deb -f "${scratch}/${asset}" Version)" == 0.2.1-1 ]]
[[ "$(dpkg-deb -f "${scratch}/${asset}" Architecture)" == amd64 ]]
dpkg-deb -x "${scratch}/${asset}" "${scratch}/package"

python3 - "$repo_root" "$scratch" <<'PY'
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time

repo, scratch = map(Path, sys.argv[1:])
agent = scratch / "package/usr/bin/tensorplate-agent"
version = subprocess.check_output([agent, "--version"], text=True, timeout=10)
if version.splitlines()[0] != "tensorplate-agent 0.2.1":
    raise RuntimeError("extracted binary does not identify as the released agent")

config = {
    "schema_version": "0.1",
    "socket_path": str(scratch / "agent.sock"),
    "state_dir": str(scratch / "state"),
    "staging_dir": str(scratch / "staging"),
    "available_backends": ["python_pytorch"],
    "backend_capabilities": {"python_pytorch": {
        "supported_precision": ["fp16", "fp32"],
        "supported_artifact_kinds": ["python_pytorch_entry"],
    }},
    "device_family": "x86_64",
    "worker": {"mode": "mock"},
}
# Deliberately omit runtime_version: the real binary supplies its own version.
# An empty descriptor directory gives the lower-floor control a distinct,
# deterministic refusal after compatibility, without importing any engines.
backends = scratch / "backends"
backends.mkdir()
env = dict(os.environ, TP_BACKEND_DESCRIPTOR_DIR=str(backends),
           TP_PLATFORM_REGISTRY_DIR=str(repo / "config/platform"))


def deploy(name, bundle):
    request = {"schema_version": "0.1", "op": "deploy", "deploy": {
        "deployment_id": name, "bundle_path": str(bundle),
    }}
    with socket.socket(socket.AF_UNIX) as control:
        control.settimeout(10)
        control.connect(config["socket_path"])
        control.sendall((json.dumps(request) + "\n").encode())
        with control.makefile("rb") as response:
            return json.loads(response.readline(1024 * 1024))


def require_refusal(response, *, message=None, context=None):
    error = response.get("error", {})
    if response.get("status") != "error" or error.get("code") != "unsupported":
        raise RuntimeError(f"expected unsupported refusal, got {response}")
    if message is not None and error.get("message") != message:
        raise RuntimeError(f"wrong refusal reason: {response}")
    if context is not None and error.get("context") != context:
        raise RuntimeError(f"wrong refusal context: {response}")
    if Path(config["staging_dir"]).exists():
        raise RuntimeError("refused bundle created a staging directory")
    state = json.loads((scratch / "state/state.json").read_text())
    if state.get("active") is not None:
        raise RuntimeError("refused bundle became active")
    quarantine = state.get("quarantined", [])
    if not quarantine or quarantine[-1].get("phase") != "received":
        raise RuntimeError("refusal occurred after the received phase")


with (scratch / "agent.log").open("w+") as log:
    process = subprocess.Popen([agent, "--config-json", json.dumps(config)],
                               env=env, stdout=log, stderr=subprocess.STDOUT)
    try:
        deadline = time.monotonic() + 15
        while not Path(config["socket_path"]).exists():
            if process.poll() is not None or time.monotonic() >= deadline:
                log.seek(0)
                raise RuntimeError(f"agent failed to start: {log.read()}")
            time.sleep(0.05)
        for name in ("speech_stt_streaming", "speech_tts_streaming"):
            bundle = repo / "test/models/bundles/v0_2" / name
            manifest = json.loads((bundle / "manifest.json").read_text())
            if (manifest["format_version"] != "0.2" or
                    manifest["runtime_compatibility"]["min_runtime_version"] != "0.3.0"):
                raise RuntimeError("speech fixture no longer exercises the intended floor")
            require_refusal(deploy(name, bundle), message=(
                "bundle declares unsupported runtime version range: "
                "bundle requires runtime >= 0.3.0; device runs 0.2.1"))
            control = scratch / name
            shutil.copytree(bundle, control)
            manifest["runtime_compatibility"]["min_runtime_version"] = "0.2.1"
            (control / "manifest.json").write_text(json.dumps(manifest))
            require_refusal(deploy(name + "-control", control),
                            context="missing_backend_package", message=(
                                "backend `python_pytorch` is unrunnable on this device: "
                                "backend descriptor not installed"))
            print(f"{name}: released 0.2.1 refuses runtime floor before staging; "
                  "lower-floor control reaches backend readiness")
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
PY
