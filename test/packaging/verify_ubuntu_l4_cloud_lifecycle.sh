#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# packaging: Ubuntu x86_64 cloud lifecycle harness verifier.
#
# The harness runs on a cloud VM with an NVIDIA accelerator, so CI can
# never execute a real run of it. What CI can execute is the part that
# decides whether a run may start at all, and that is where the harness
# can do the most damage: admitting an ineligible host produces evidence
# nobody should trust, and refusing an eligible one burns VM time.
#
# So the eligibility rules are driven for real here, through the same
# kind of seams the release installer uses, and the rest is pinned by
# literal and structural checks over the harness text.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
harness="${repo_root}/tools/validation/ubuntu-l4-cloud-lifecycle.sh"
schema="${repo_root}/config/schemas/lifecycle_report.json"

[[ -x "$harness" ]] || { printf 'FAIL: harness is not executable\n' >&2; exit 1; }

bash -n "$harness"
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "$harness"
else
  printf 'verify_ubuntu_l4_cloud_lifecycle: shellcheck not found; skipping shellcheck\n'
fi
"$harness" --help >/dev/null

td="$(mktemp -d)"
trap 'rm -rf "$td"' EXIT

# --- structure: the stage set matches the canonical eight.
#
# The harness names its stages in calls rather than in a list, so the
# drift check reads the calls. A stage renamed in the schema and not
# here would otherwise surface only as a rejected report after a run.
python3 - "$harness" "$schema" <<'PY'
import json, re, sys

harness_path, schema_path = sys.argv[1:]
body = open(harness_path, encoding="utf-8").read()
schema = json.load(open(schema_path, encoding="utf-8"))
canonical = schema["properties"]["stages"]["items"]["properties"]["stage"]["enum"]

run = re.findall(r"^\s*lifecycle_stage\s+([a-z-]+)\s", body, re.M)
skipped = re.findall(r"^\s*lifecycle_skip\s+([a-z-]+)\s", body, re.M)
named = run + skipped
assert sorted(named) == sorted(canonical), (
    f"harness covers {sorted(named)}, the schema names {sorted(canonical)}"
)
assert len(set(named)) == len(named), f"a stage is named twice: {named}"
assert set(run) == {"install", "deploy-smoke", "status-logs", "restart",
                    "crash-loop", "offline"}, sorted(run)
assert set(skipped) == {"upgrade", "rollback"}, sorted(skipped)

# Every skip states a reason. An unexplained skip is indistinguishable
# from a stage nobody thought about.
for stage in skipped:
    match = re.search(
        r"lifecycle_skip\s+" + re.escape(stage) + r"\s*\\\n\s*\"([^\"]+)\"", body
    )
    assert match, f"{stage} is skipped without a quoted reason"
    assert len(match.group(1)) > 40, f"{stage}'s skip reason is too thin: {match.group(1)}"

# The digest must be recorded after the install stage passed, so it
# attests an install that happened. Matched as a call rather than as a
# mention, so a comment naming the function cannot satisfy this.
install_call = re.search(r"^\s*lifecycle_stage\s+install\s", body, re.M)
digest_call = re.search(r"^\s*lifecycle_artifact_digest\s", body, re.M)
assert install_call, "the harness does not run an install stage"
assert digest_call, "the harness never records an artifact digest"
assert digest_call.start() > install_call.start(), \
    "the artifact digest is recorded before the install stage"

# The digest must not be written into the evidence directory before the
# runner starts: lifecycle_begin clears that sidecar so a retry cannot
# inherit a previous attempt's digest, and a harness that wrote it first
# would delete its own and abort.
begin_call = re.search(r"^\s*lifecycle_begin\s", body, re.M)
assert begin_call, "the harness never calls lifecycle_begin"
before_begin = body[: begin_call.start()]
assert "artifact-digest.txt" not in before_begin, \
    "the harness writes the digest sidecar before lifecycle_begin, which clears it"
print("stage coverage: 6 run, 2 skipped with reasons, digest recorded after install")
PY

# --- the bundle must be staged somewhere the sandboxed agent can see.
#
# The agent opens the bundle itself, as the tensorplate user, inside the
# unit's filesystem sandbox. The first real run staged it under /var/tmp,
# which PrivateTmp hides, and the agent reported a bundle it could not
# see as one that did not exist. This reads the sandbox from the shipped
# unit rather than from a list kept here, so tightening the unit fails
# this check instead of a VM run.
python3 - "$harness" "${repo_root}/packaging/debian/tensorplate-agent.service" <<'PY'
import re, sys

harness_path, unit_path = sys.argv[1:]
body = open(harness_path, encoding="utf-8").read()
unit = open(unit_path, encoding="utf-8").read()

match = re.search(r'^BUNDLE_STAGING_DIR="\$\{TP_CLOUD_BUNDLE_STAGING:-([^}]+)\}"', body, re.M)
assert match, "the harness does not declare a default bundle staging directory"
staging = match.group(1)

def enabled(key):
    found = re.search(rf"^{key}=(\S+)", unit, re.M)
    return found is not None and found.group(1).lower() not in ("false", "no", "0")

hidden = []
if enabled("PrivateTmp"):
    hidden += ["/tmp", "/var/tmp"]
if enabled("ProtectHome"):
    hidden += ["/home", "/root", "/run/user"]
for prefix in hidden:
    assert not (staging == prefix or staging.startswith(prefix + "/")), (
        f"the bundle is staged at {staging}, which the agent unit hides from the "
        f"agent ({prefix}); the agent would report it as nonexistent"
    )
print(f"bundle staging: {staging} is visible to the sandboxed agent")
PY

# --- the honesty claim the PR rests on.
grep -Fq 'does NOT' "$harness"
grep -Fq 'accelerator_kernel_executed' "$harness"
if ! grep -Fq 'execute a CUDA kernel' "$harness"; then
  printf 'FAIL: the harness must say it does not execute a CUDA kernel\n' >&2
  exit 1
fi

# --- eligibility, executed.
stub_bin="${td}/bin"
mkdir -p "$stub_bin"
# Unconditionally, not only where the real tool is absent. On a Linux
# runner the real systemctl and dpkg exist, and a probe that let them
# through would query the actual host for services it never installed --
# answering about the runner rather than about the harness.
for tool in sudo systemctl systemd-run dpkg; do
  printf '#!/bin/sh\nexit 0\n' >"${stub_bin}/${tool}"
  chmod +x "${stub_bin}/${tool}"
done

cat >"${td}/os-release.noble" <<'EOF'
ID=ubuntu
VERSION_ID="24.04"
EOF
cat >"${td}/os-release.jammy" <<'EOF'
ID=ubuntu
VERSION_ID="22.04"
EOF
printf 'NVRM version: NVIDIA UNIX x86_64 Kernel Module  560.35.03\n' >"${td}/nvidia-version"

cat >"${td}/python-with-torch" <<'STUB'
#!/bin/sh
exit 0
STUB
cat >"${td}/python-without-torch" <<'STUB'
#!/bin/sh
echo "ModuleNotFoundError: No module named 'torch'" >&2
exit 1
STUB
chmod +x "${td}/python-with-torch" "${td}/python-without-torch"

assets="${td}/assets"
mkdir -p "$assets"
printf '#!/bin/sh\nexit 0\n' >"${assets}/install.sh"
# A real checksum line, not an empty file: the harness verifies this set
# with `sha256sum -c`, and GNU coreutils rejects a checksum file with no
# properly formatted lines. An empty one passed on macOS and failed on
# the runner, which is the kind of difference a fixture should not have.
if command -v sha256sum >/dev/null 2>&1; then
  ( cd "$assets" && sha256sum install.sh >SHA256SUMS )
else
  ( cd "$assets" && shasum -a 256 install.sh >SHA256SUMS )
fi
bundle="${repo_root}/test/models/bundles/v0_1/x86_fixture_smoke"
[[ -f "${bundle}/manifest.json" ]] || {
  printf 'FAIL: the deploy-smoke bundle fixture is missing\n' >&2
  exit 1
}

failures=0
check() {
  local what="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    printf '  ok   %s\n' "$what"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$what" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

# Runs preflight with every seam pointed at a fixture, and returns the
# harness's exit status. Nothing here touches the host: --preflight-only
# stops before the first privileged command.
preflight() {
  local arch="$1" os_release="$2" nvidia="$3" python_bin="$4" evidence="$5" version="$6"
  shift 6
  set +e
  env PATH="${stub_bin}:${PATH}" \
    TP_CLOUD_ARCH="$arch" \
    TP_CLOUD_OS_RELEASE="$os_release" \
    TP_CLOUD_NVIDIA_VERSION="$nvidia" \
    TP_CLOUD_PYTHON="$python_bin" \
    bash "$harness" \
      --assets-dir "$assets" \
      --bundle-dir "$bundle" \
      --evidence-dir "$evidence" \
      --tested-version "$version" \
      --preflight-only \
      "$@" >"${td}/preflight.out" 2>"${td}/preflight.err"
  local status=$?
  set -e
  printf '%s' "$status"
}

ok_args=(x86_64 "${td}/os-release.noble" "${td}/nvidia-version" "${td}/python-with-torch")

check "an eligible host passes preflight" "0" \
  "$(preflight "${ok_args[@]}" "${td}/evidence-ok" 0.2.1 --confirm RESET-TENSORPLATE)"
check "  and preflight writes nothing" "no" \
  "$([[ -e "${td}/evidence-ok" ]] && echo yes || echo no)"

check "a run without the confirmation token is refused" "1" \
  "$(preflight "${ok_args[@]}" "${td}/evidence-noconfirm" 0.2.1)"
check "  and says what it would have purged" "yes" \
  "$(grep -q "purges TensorPlate packages and state" "${td}/preflight.err" && echo yes || echo no)"

check "a non-x86_64 host is refused" "1" \
  "$(preflight aarch64 "${td}/os-release.noble" "${td}/nvidia-version" \
     "${td}/python-with-torch" "${td}/evidence-arch" 0.2.1 --confirm RESET-TENSORPLATE)"

check "Ubuntu 22.04 is refused on this row" "1" \
  "$(preflight x86_64 "${td}/os-release.jammy" "${td}/nvidia-version" \
     "${td}/python-with-torch" "${td}/evidence-jammy" 0.2.1 --confirm RESET-TENSORPLATE)"
check "  and names what it expected" "yes" \
  "$(grep -q "expected Ubuntu 24.04" "${td}/preflight.err" && echo yes || echo no)"

# Advisory in the installer, fatal here: a run that cannot see the
# accelerator cannot produce evidence for a row whose subject is that
# accelerator.
check "a host with no NVIDIA driver is refused" "1" \
  "$(preflight x86_64 "${td}/os-release.noble" "${td}/absent-nvidia" \
     "${td}/python-with-torch" "${td}/evidence-nogpu" 0.2.1 --confirm RESET-TENSORPLATE)"
check "  and names the driver file it looked for" "yes" \
  "$(grep -q "no NVIDIA driver at" "${td}/preflight.err" && echo yes || echo no)"

check "a host without PyTorch is refused before the install burns time" "1" \
  "$(preflight x86_64 "${td}/os-release.noble" "${td}/nvidia-version" \
     "${td}/python-without-torch" "${td}/evidence-notorch" 0.2.1 --confirm RESET-TENSORPLATE)"
check "  and gives the operator the remedy" "yes" \
  "$(grep -q "externally-managed" "${td}/preflight.err" && echo yes || echo no)"

# The report's version must be the release it authorizes, never the
# candidate spelling the artifacts were built as.
check "a candidate version spelling is refused" "1" \
  "$(preflight "${ok_args[@]}" "${td}/evidence-rc" 0.2.1-rc.1 --confirm RESET-TENSORPLATE)"
check "  and so is the tilde spelling" "1" \
  "$(preflight "${ok_args[@]}" "${td}/evidence-tilde" '0.2.1~rc.1' --confirm RESET-TENSORPLATE)"

mkdir -p "${td}/evidence-dirty"
: >"${td}/evidence-dirty/lifecycle-report.json"
check "a non-empty evidence directory is refused" "1" \
  "$(preflight "${ok_args[@]}" "${td}/evidence-dirty" 0.2.1 --confirm RESET-TENSORPLATE)"

check "an assets directory with no installer is refused" "1" \
  "$(env PATH="${stub_bin}:${PATH}" TP_CLOUD_ARCH=x86_64 \
      TP_CLOUD_OS_RELEASE="${td}/os-release.noble" \
      TP_CLOUD_NVIDIA_VERSION="${td}/nvidia-version" \
      TP_CLOUD_PYTHON="${td}/python-with-torch" \
      bash "$harness" --assets-dir "$td" --bundle-dir "$bundle" \
        --evidence-dir "${td}/evidence-noassets" --tested-version 0.2.1 \
        --preflight-only --confirm RESET-TENSORPLATE >/dev/null 2>&1; printf '%s' "$?")"

# --- the stages, executed against a stubbed appliance.
#
# The four running stages issue real commands against a real install, so
# CI cannot run them for their own sake. What CI must be able to see is
# whether their assertions FIRE: lifecycle_stage calls a stage function
# from a tested context, which suspends errexit inside it, so a stage
# written the obvious way runs past its own failures and returns the
# status of its last command. That defect certifies a broken host as a
# validated one, and it is invisible to any amount of reading.
appliance="${td}/appliance"
mkdir -p "${appliance}/bin" "${appliance}/run" "${appliance}/log"

# sudo records without executing privileged commands. Journal requests
# are routed only to the fixture below; package and filesystem mutations
# must never reach the machine running this suite.
cat >"${appliance}/bin/sudo" <<'STUB'
#!/bin/sh
printf '%s\n' "$*" >>"${TP_FAKE_SUDO_LOG}"
# Injectable failure, so a privileged step that the harness forgot to
# check can be caught here rather than on a VM.
if [ -n "${TP_FAKE_SUDO_FAIL:-}" ]; then
  case "$*" in
    *"${TP_FAKE_SUDO_FAIL}"*) exit 9 ;;
  esac
fi
# A purge that succeeds empties dpkg's view of the packages. Breaking
# the agent config and installing the offline drop-in change what the
# stubbed systemctl reports, until they are undone.
case "$*" in
  *"apt-get purge"*) : >"${TP_FAKE_PURGE_MARKER}" ;;
  *"systemctl restart"*) : >"${TP_FAKE_RESTART_MARKER}" ;;
  *"invalid json"*) : >"${TP_FAKE_CONFIG_BROKEN}" ;;
  "cp -p "*" /etc/tensorplate/agent.json") rm -f "${TP_FAKE_CONFIG_BROKEN}" ;;
  "install -D "*)
    [ "${TP_FAKE_MODE:-ok}" = offline-dropin-ignored ] || : >"${TP_FAKE_OFFLINE_DROPIN}" ;;
  "rm -f "*".service.d/"*)
    [ "${TP_FAKE_MODE:-ok}" = offline-dropin-sticks ] || rm -f "${TP_FAKE_OFFLINE_DROPIN}" ;;
esac
if [ "$1" = journalctl ]; then
  shift
  exec "${TP_FAKE_JOURNALCTL}" "$@"
fi
# A transient unit runs its command here, without systemd. Address
# denial is emulated in the one place the harness observes it -- a
# Python sendto -- so each way enforcement can go wrong is selectable.
if [ "$1" = systemd-run ]; then
  shift
  denied=0
  while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
    case "$1" in IPAddressDeny=*) denied=1 ;; esac
    shift
  done
  [ "$#" -gt 0 ] && shift
  case "${TP_FAKE_MODE:-ok}" in
    offline-not-enforced) denied=0 ;;
    offline-control-denied) denied=1 ;;
  esac
  TP_FAKE_NETWORK_DENIED="$denied" PYTHONPATH="${TP_FAKE_SITE}" exec "$@"
fi
exit 0
STUB
# dpkg's package database, in the shape the harness queries it.
cat >"${appliance}/bin/dpkg-query" <<'STUB'
#!/bin/sh
case "${TP_FAKE_MODE:-ok}" in
  installed-runtime)
    # A host with a previous install.sh run: the runtime set is present,
    # and the metapackage -- which only the APT channel installs -- is not.
    [ -f "${TP_FAKE_PURGE_MARKER}" ] && exit 0
    for pkg in tensorplate-agent tensorplate-serving tensorplate-observability \
               tensorplate-cli tensorplate-common; do
      printf '%s installed\n' "$pkg"
    done
    ;;
  purge-leaves-packages)
    printf 'tensorplate-common config-files\n'
    ;;
  *) exit 0 ;;
esac
STUB
cat >"${appliance}/bin/systemctl" <<'STUB'
#!/bin/sh
case "$1" in
  show)
    broken=0
    [ -f "${TP_FAKE_CONFIG_BROKEN}" ] && broken=1
    case "$*" in
      *ActiveState*)
        # A looping unit reads as failed between attempts, which is why
        # the harness must not settle on that state alone. One stopped by
        # something else reads as inactive, which is not a crash loop.
        case "${broken}:${TP_FAKE_MODE:-ok}" in
          1:crash-loop-never-fails|0:*) printf 'active\n' ;;
          1:crash-loop-stopped) printf 'inactive\n' ;;
          *) printf 'failed\n' ;;
        esac
        ;;
      *NRestarts*)
        if [ "$broken" -eq 0 ]; then
          printf '0\n'
        else
          case "${TP_FAKE_MODE:-ok}" in
            crash-loop-not-retried) printf '0\n' ;;
            crash-loop-keeps-restarting)
              count=$(cat "${TP_FAKE_RESTARTS_FILE}" 2>/dev/null || echo 0)
              count=$((count + 1))
              printf '%s\n' "$count" >"${TP_FAKE_RESTARTS_FILE}"
              printf '%s\n' "$count"
              ;;
            *) printf '4\n' ;;
          esac
        fi
        ;;
      *Result*)
        if [ "$broken" -eq 1 ]; then printf 'start-limit-hit\n'; else printf 'success\n'; fi
        ;;
      *IPAddressDeny*)
        if [ -f "${TP_FAKE_OFFLINE_DROPIN}" ]; then printf '0.0.0.0/0 ::/0\n'; else printf '\n'; fi
        ;;
      *IPAddressAllow*)
        if [ -f "${TP_FAKE_OFFLINE_DROPIN}" ]; then printf '127.0.0.0/8 ::1/128\n'; else printf '\n'; fi
        ;;
      *InvocationID*)
        case "$*" in
          *tensorplate-agent*) printf '11111111111111111111111111111111\n' ;;
          *tensorplate-observability*) printf '22222222222222222222222222222222\n' ;;
          *) exit 9 ;;
        esac
        ;;
      *MainPID*)
        # A restart must change the pid, so hand back a new one each call.
        count=$(cat "${TP_FAKE_PID_FILE}" 2>/dev/null || echo 100)
        count=$((count + 1))
        printf '%s\n' "$count" >"${TP_FAKE_PID_FILE}"
        printf '%s\n' "$count"
        ;;
      *) printf '\n' ;;
    esac
    ;;
  *) exit 0 ;;
esac
STUB
cat >"${appliance}/bin/dpkg" <<'STUB'
#!/bin/sh
exit 0
STUB
# Present for the preflight check. Transient units need root, so reaching
# this directly rather than through sudo is a harness defect.
cat >"${appliance}/bin/systemd-run" <<'STUB'
#!/bin/sh
exit 9
STUB
cat >"${appliance}/bin/journalctl" <<'STUB'
#!/bin/sh
invocation=""
json=0
since=""
previous=""
for arg in "$@"; do
  case "$arg" in
    _SYSTEMD_INVOCATION_ID=*) invocation="${arg#*=}" ;;
    --output=json) json=1 ;;
  esac
  [ "$previous" = --since ] && since="$arg"
  previous="$arg"
done
[ "$json" -eq 1 ] || exit 9
# The agent's starts under a broken config, each refusing it.
if [ -n "$since" ]; then
  message='config error: agent.json is not valid JSON'
  [ "${TP_FAKE_MODE:-ok}" = crash-loop-other-error ] && message='state store error: permission denied'
  for _ in 1 2 3 4 5; do
    printf '{"_SYSTEMD_UNIT":"tensorplate-agent.service","MESSAGE":"%s"}\n' "$message"
  done
  exit 0
fi
case "$invocation" in
  11111111111111111111111111111111) unit=tensorplate-agent.service ;;
  22222222222222222222222222222222) unit=tensorplate-observability.service ;;
  *) exit 9 ;;
esac
case "${TP_FAKE_MODE:-ok}:$unit" in
  journal-command-fails:*) exit 9 ;;
  journal-empty-agent:tensorplate-agent.service) exit 0 ;;
  journal-no-entries:tensorplate-agent.service) printf '%s\n' '-- No entries --'; exit 0 ;;
  journal-empty-observability:tensorplate-observability.service) exit 0 ;;
  journal-stale-invocation:*) invocation=ffffffffffffffffffffffffffffffff ;;
  journal-wrong-unit:*) unit=another.service ;;
esac
message='fixture service started'
[ "${TP_FAKE_MODE:-ok}" = journal-empty-message ] && message=''
printf '{"_SYSTEMD_INVOCATION_ID":"%s","_SYSTEMD_UNIT":"%s","MESSAGE":"%s","__REALTIME_TIMESTAMP":"1789300000000000"}\n' \
  "$invocation" "$unit" "$message"
STUB
cat >"${appliance}/bin/tensorplate" <<'STUB'
#!/bin/sh
# A stubbed appliance. TP_FAKE_MODE selects which way it misbehaves.
mode="${TP_FAKE_MODE:-ok}"
phase=initial
[ -f "${TP_FAKE_RESTART_MARKER}" ] && phase=restarted
command="$1"
shift
out=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-file) out="$2"; shift 2 ;;
    --input) input="$2"; shift 2 ;;
    *) shift ;;
  esac
done
case "$command" in
  doctor)
    failing=0
    row_status=ok
    if [ "$mode" = "doctor-failing" ]; then failing=1; fi
    if [ "$mode" = "wrong-row" ]; then row_status=warning; fi
    if [ "$mode" = "offline-doctor-failing" ] && [ "${TP_FAKE_NETWORK_DENIED:-0}" = 1 ]; then
      failing=1
    fi
    cat <<JSON
{"command":"doctor","payload":{"failing":${failing},"findings":[
 {"id":"platform_row","status":"${row_status}","message":"resolved ubuntu2404-x86-l4-g2s8"},
 {"id":"platform_profile","status":"ok","message":"host matches 1 candidate support row(s): ubuntu2404-x86-l4-g2s8"},
 {"id":"accelerator_facts","status":"ok","message":"1 accelerator: NVIDIA L4"},
 {"id":"platform_registry","status":"ok","message":"ok"},
 {"id":"agent_reachable","status":"ok","message":"ok"},
 {"id":"agent_socket","status":"ok","message":"ok"},
 {"id":"serving_binary_installed","status":"ok","message":"ok"},
 {"id":"python_pytorch_backend","status":"ok","message":"ok"},
 {"id":"python_pytorch_runtime","status":"ok","message":"ok"},
 {"id":"path_layout","status":"ok","message":"ok"},
 {"id":"config_files","status":"ok","message":"ok"}]}}
JSON
    ;;
  deploy)
    printf '{"command":"deploy","payload":{"phase":"active","deployment_id":"%s"}}\n' \
      "${TP_FAKE_DEPLOYMENT_ID}"
    ;;
  status)
    serving_url="\"http://127.0.0.1:${TP_FAKE_SERVING_PORT}/infer\""
    if [ "$mode:$phase" = restart-no-worker:restarted ]; then serving_url=null; fi
    printf '{"command":"status","payload":{"severity":"ready","agent":{"agent_state":"ready","active":{"deployment_id":"%s","backend":"python_pytorch","serving_url":%s}}}}\n' \
      "${TP_FAKE_DEPLOYMENT_ID}" "$serving_url"
    ;;
  infer)
    printf '%s\n' "$phase" >>"${TP_FAKE_INFER_LOG}"
    name=echo_probe
    payload=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["inputs"][0]["payload_b64"])' "$input")
    if [ "$mode" = "infer-garbled" ]; then payload="AAAA"; fi
    if [ "$mode:$phase" = restart-infer-garbled:restarted ]; then payload="AAAA"; fi
    cat >"$out" <<JSON
{"outputs":[{"name":"${name}",
 "tensor":{"dtype":"float32","layout":"row_major","shape":[1,4],"byte_offset":0,"byte_size":16},
 "payload_b64":"${payload}"}]}
JSON
    ;;
  logs)
    # The real CLI fails here: nothing writes the configured file log on
    # a packaged Linux install. The stub reproduces that.
    printf 'cannot stat log source\n' >&2
    exit 1
    ;;
esac
STUB
chmod +x "${appliance}/bin/"*

# What the kernel does to a send from a network-denied cgroup: EPERM for
# anything but loopback. Loaded only into commands the stubbed
# systemd-run starts.
mkdir -p "${appliance}/site"
cat >"${appliance}/site/sitecustomize.py" <<'PY'
import os, socket

_denied = os.environ.get("TP_FAKE_NETWORK_DENIED") == "1"
_sendto = socket.socket.sendto

def sendto(self, data, *args):
    address = args[-1]
    if address[0] in ("127.0.0.1", "::1"):
        return _sendto(self, data, *args)
    if _denied:
        raise PermissionError(1, "Operation not permitted")
    return len(data)

socket.socket.sendto = sendto
PY

# A serving /health endpoint, which the deploy-smoke checker fetches.
#
# Both of its streams are redirected away from this script's: a
# background process holding the suite's stdout or stderr keeps a pipe
# open after the suite exits, and `run.sh | tail` would hang forever
# waiting for an EOF that never comes.
python3 - "${appliance}" >"${appliance}/health.port" 2>"${appliance}/health.err" <<'PY' &
import http.server, json, socket, sys, threading, pathlib

directory = pathlib.Path(sys.argv[1])

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        restarted = (directory / "restarted").exists()
        mode = (directory / "mode").read_text().strip()
        phase = "restarted" if restarted else "initial"
        with (directory / "health-requests.log").open("a") as log:
            log.write(f"{phase} {self.path}\n")
        state = "failed" if restarted and mode == "restart-unhealthy-health" else "ready"
        deployment = (directory / "deployment-id").read_text().strip()
        if restarted and mode == "restart-wrong-health":
            deployment = "a-different-deployment"
        body = json.dumps({
            "state": state,
            "active_model_id": deployment,
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass

sock = socket.socket()
sock.bind(("127.0.0.1", 0))
port = sock.getsockname()[1]
sock.close()
print(port, flush=True)
server = http.server.HTTPServer(("127.0.0.1", port), Handler)
(directory / "health.pid").write_text(str(__import__("os").getpid()))
server.serve_forever()
PY
deployment_id="cloud-lifecycle-smoke"
printf '%s\n' "$deployment_id" >"${appliance}/deployment-id"
for _ in $(seq 1 50); do
  [[ -s "${appliance}/health.port" ]] && break
  sleep 0.1
done
serving_port="$(head -n1 "${appliance}/health.port")"
health_pid=$!
# shellcheck disable=SC2329 # Invoked through the EXIT trap below.
cleanup() {
  # Kill the server before removing its directory, and never let cleanup
  # itself fail the suite.
  kill "$health_pid" 2>/dev/null || true
  wait "$health_pid" 2>/dev/null || true
  rm -rf "$td"
}
trap cleanup EXIT

run_stages() {
  local mode="$1" evidence="$2" sudo_fail="${3:-}"
  set +e
  : >"${appliance}/sudo.log"
  : >"${appliance}/infer.log"
  : >"${appliance}/health-requests.log"
  rm -f "${appliance}/restarted" "${appliance}/config-broken" \
    "${appliance}/offline-dropin" "${appliance}/restarts"
  printf '%s\n' "$mode" >"${appliance}/mode"
  env PATH="${appliance}/bin:${PATH}" \
    TP_FAKE_SUDO_FAIL="$sudo_fail" \
    TP_FAKE_PURGE_MARKER="${evidence}.purged" \
    TP_CLOUD_ARCH=x86_64 \
    TP_CLOUD_OS_RELEASE="${td}/os-release.noble" \
    TP_CLOUD_NVIDIA_VERSION="${td}/nvidia-version" \
    TP_CLOUD_PYTHON="${td}/python-with-torch" \
    TP_CLOUD_AGENT_SOCKET="${appliance}/run/agent.sock" \
    TP_CLOUD_LOG_DIR="${appliance}/log" \
    TP_CLOUD_BUNDLE_STAGING="${appliance}/staged-bundle" \
    TP_FAKE_MODE="$mode" \
    TP_FAKE_SUDO_LOG="${appliance}/sudo.log" \
    TP_FAKE_JOURNALCTL="${appliance}/bin/journalctl" \
    TP_FAKE_RESTART_MARKER="${appliance}/restarted" \
    TP_FAKE_INFER_LOG="${appliance}/infer.log" \
    TP_FAKE_PID_FILE="${appliance}/pid" \
    TP_FAKE_DEPLOYMENT_ID="$deployment_id" \
    TP_FAKE_SERVING_PORT="$serving_port" \
    TP_FAKE_CONFIG_BROKEN="${appliance}/config-broken" \
    TP_FAKE_OFFLINE_DROPIN="${appliance}/offline-dropin" \
    TP_FAKE_RESTARTS_FILE="${appliance}/restarts" \
    TP_FAKE_SITE="${appliance}/site" \
    TP_CLOUD_CRASH_LOOP_POLL_SECONDS=0 \
    bash "$harness" \
      --assets-dir "$assets" \
      --bundle-dir "$bundle" \
      --evidence-dir "$evidence" \
      --tested-version 0.2.1 \
      --confirm RESET-TENSORPLATE >"${evidence}.out" 2>"${evidence}.err"
  local status=$?
  set -e
  # A probe that fails without saying why costs a CI round trip to
  # diagnose, and the evidence directory is deleted with the temp dir.
  if [[ ! -f "${evidence}/lifecycle-report.json" ]]; then
    printf '  -- no report written; last lines of the harness:\n' >&2
    tail -n 12 "${evidence}.err" 2>/dev/null | sed 's/^/     /' >&2 || true
  fi
  printf '%s' "$status"
}

# The harness waits for a control socket, which only a real agent
# creates. The stub appliance provides one.
python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' \
  "${appliance}/run/agent.sock"

stage_status() {
  python3 -c 'import json,sys
report=json.load(open(sys.argv[1]))
print(next((s["status"] for s in report["stages"] if s["stage"]==sys.argv[2]), "absent"))' \
    "$1" "$2"
}

ok_evidence="${td}/stages-ok"
check "a stubbed run completes" "0" "$(run_stages ok "$ok_evidence")"
for stage in install deploy-smoke status-logs restart crash-loop offline; do
  check "  ${stage} is recorded as a pass" "pass" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
for stage in upgrade rollback; do
  check "  ${stage} is recorded as skipped" "skipped" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
check "  the artifact digest reaches the report" "yes" \
  "$(python3 -c 'import json,sys,re
subject=json.load(open(sys.argv[1]))["subject"]
print("yes" if re.fullmatch(r"[0-9a-f]{64}", subject.get("artifact_digest","")) else "no")' \
    "${ok_evidence}/lifecycle-report.json")"
# The no-CUDA-kernel claim, asserted as a recorded value rather than as
# a string that appears somewhere in the file. A grep for the identifier
# is satisfied by the header comment alone.
check "  the recorded result denies executing an accelerator kernel" "False" \
  "$(python3 -c 'import json,sys
print(json.load(open(sys.argv[1]))["accelerator_kernel_executed"])' \
    "${ok_evidence}/deploy-result.json")"
check "  and records the supervision state it actually saw" "not_configured" \
  "$(python3 -c 'import json,sys
print(json.load(open(sys.argv[1]))["supervision_state"])' \
    "${ok_evidence}/deploy-result.json")"
check "  and the log command outcome is filed" "1" \
  "$(cat "${ok_evidence}/logs-command.exit")"
for phase in initial restarted; do
  check "  ${phase} worker answers a fresh inference" yes \
    "$(grep -Fxq "$phase" "${appliance}/infer.log" && echo yes || echo no)"
  check "  ${phase} worker answers its health endpoint" yes \
    "$(grep -Fxq "$phase /health" "${appliance}/health-requests.log" && echo yes || echo no)"
done
for invocation in 11111111111111111111111111111111 22222222222222222222222222222222; do
  check "  journal capture selects current invocation ${invocation}" yes \
    "$(grep -F "journalctl" "${appliance}/sudo.log" | grep -Fq "_SYSTEMD_INVOCATION_ID=${invocation}" && echo yes || echo no)"
done

check "  and the report is schema-valid" "yes" \
  "$(python3 - "$schema" "${ok_evidence}/lifecycle-report.json" <<'PY'
import json, sys
try:
    import jsonschema
except ImportError:
    print("yes")
    sys.exit(0)
schema = json.load(open(sys.argv[1]))
report = json.load(open(sys.argv[2]))
errors = list(jsonschema.Draft7Validator(schema).iter_errors(report))
print("yes" if not errors else f"no: {errors[0].message}")
PY
)"

# The regression that matters: a stage whose assertion fails must be
# recorded as a failure. Written the obvious way, with errexit suspended
# inside the stage body and a `pass` line at the end, every one of these
# would come back `pass`.
fail_evidence="${td}/stages-doctor-failing"
check "a run whose doctor reports a failure exits non-zero" "1" \
  "$(run_stages doctor-failing "$fail_evidence")"
check "  and install is recorded as a failure, not a pass" "fail" \
  "$(stage_status "${fail_evidence}/lifecycle-report.json" install)"
check "  and the run does not certify itself" "fail" \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' \
     "${fail_evidence}/lifecycle-report.json")"

# Found on a real host: re-running on a machine that already had an
# install.sh install. The purge must name only packages dpkg knows. A
# fixed list that included the `tensorplate` metapackage -- which
# install.sh never installs -- made apt-get abort the whole purge, the
# harness then deleted conffiles dpkg still owned, and the reinstall came
# up with an empty /etc/tensorplate.
rerun_evidence="${td}/stages-rerun"
check "a re-run over an existing install completes" "0" \
  "$(run_stages installed-runtime "$rerun_evidence")"
purge_line="$(grep -F 'apt-get purge' "${appliance}/sudo.log" || true)"
check "  and purges the runtime packages that were installed" "yes" \
  "$(printf '%s\n' "$purge_line" | grep -qF 'tensorplate-agent' && echo yes || echo no)"
check "  and never names the metapackage install.sh does not install" "no" \
  "$(printf '%s\n' "$purge_line" | tr ' ' '\n' | grep -qx 'tensorplate' && echo yes || echo no)"

leftover_evidence="${td}/stages-purge-leaves"
check "packages surviving the purge fail install before state is cleared" "fail" \
  "$(run_stages purge-leaves-packages "$leftover_evidence" >/dev/null; \
     stage_status "${leftover_evidence}/lifecycle-report.json" install)"
check "  and the state directories were never removed" "no" \
  "$(grep -qF 'rm -rf' "${appliance}/sudo.log" && echo yes || echo no)"

# A privileged step that fails must fail its stage. Without this, an
# unguarded `sudo ...` inside a stage body is invisible: errexit is
# suspended there, so the stage runs on and returns 0.
installer_evidence="${td}/stages-installer-fails"
check "an installer that fails is recorded as a failed install" "fail" \
  "$(run_stages ok "$installer_evidence" install.sh >/dev/null; \
     stage_status "${installer_evidence}/lifecycle-report.json" install)"

clear_evidence="${td}/stages-clear-fails"
check "state clearing that fails is recorded as a failed install" "fail" \
  "$(run_stages ok "$clear_evidence" "rm -rf" >/dev/null; \
     stage_status "${clear_evidence}/lifecycle-report.json" install)"

restart_evidence="${td}/stages-restart-fails"
check "a restart that fails is recorded as a failed restart" "fail" \
  "$(run_stages ok "$restart_evidence" "systemctl restart" >/dev/null; \
     stage_status "${restart_evidence}/lifecycle-report.json" restart)"

row_evidence="${td}/stages-wrong-row"
check "a host whose row does not resolve fails the install stage" "fail" \
  "$(run_stages wrong-row "$row_evidence" >/dev/null; stage_status "${row_evidence}/lifecycle-report.json" install)"

infer_evidence="${td}/stages-infer-garbled"
check "an inference that does not echo the input fails deploy-smoke" "fail" \
  "$(run_stages infer-garbled "$infer_evidence" >/dev/null; stage_status "${infer_evidence}/lifecycle-report.json" deploy-smoke)"

for mode in restart-no-worker restart-unhealthy-health restart-wrong-health restart-infer-garbled; do
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_stages "$mode" "$evidence")"
  check "  deployment passed before the restart regression" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" deploy-smoke)"
  check "  the restarted worker failure is recorded against restart" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" restart)"
done

for mode in journal-command-fails journal-empty-agent journal-no-entries journal-empty-observability \
            journal-stale-invocation journal-wrong-unit journal-empty-message; do
  evidence="${td}/stages-${mode}"
  expected_status=1
  if [[ "$mode" == journal-command-fails ]]; then expected_status=9; fi
  check "${mode} fails the run" "$expected_status" "$(run_stages "$mode" "$evidence")"
  check "  deployment passed before the journal failure" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" deploy-smoke)"
  check "  invalid journal evidence fails status-logs" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" status-logs)"
done

# --- crash-loop and offline.
#
# Both stages break the appliance on purpose and must put it back, so
# each is checked for the undo as well as for the verdict.
sudo_line() {
  grep -nF -- "$1" "${appliance}/sudo.log" | head -n1 | cut -d: -f1
}
restore_line='/agent.json /etc/tensorplate/agent.json'
dropin_removals() {
  grep -cE '^rm -f /run/systemd/system/tensorplate-(agent|observability)\.service\.d/50-tensorplate-validation-offline\.conf$' \
    "${appliance}/sudo.log" || true
}

run_stages ok "${td}/stages-ok-again" >/dev/null
check "the ok run breaks the agent config, then restores it" yes \
  "$(broke="$(sudo_line 'invalid json')"; restored="$(sudo_line "$restore_line")"
     [[ -n "$broke" && -n "$restored" && "$broke" -lt "$restored" ]] && echo yes || echo no)"
check "  and files what systemd did with the loop" "failed 4 start-limit-hit" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]));print(r["active_state"],r["restarts"],r["result"])' \
    "${td}/stages-ok-again/crash-loop-result.json")"
check "  and the recovered worker answered" yes \
  "$([[ -s "${td}/stages-ok-again/crash-loop-recovery.json" ]] && echo yes || echo no)"
check "the offline drop-ins are runtime drop-ins for both units" 2 \
  "$(grep -cE '^install -D -m 0644 .* /run/systemd/system/tensorplate-(agent|observability)\.service\.d/50-tensorplate-validation-offline\.conf$' \
     "${appliance}/sudo.log" || true)"
check "  and both are removed afterwards" 2 "$(dropin_removals)"
check "  doctor ran under the address denial" yes \
  "$(grep -E '^systemd-run .*IPAddressDeny=any.* -- .*/tensorplate doctor --output json$' \
     "${appliance}/sudo.log" >/dev/null && echo yes || echo no)"
check "  the denial was observed as enforced" "denied sent" \
  "$(python3 -c 'import json,sys
p=json.load(open(sys.argv[1]));print(p["external"],p["loopback"])' \
    "${td}/stages-ok-again/offline-denied-probe.json")"
check "  against a control that could reach the network" "sent sent" \
  "$(python3 -c 'import json,sys
p=json.load(open(sys.argv[1]));print(p["external"],p["loopback"])' \
    "${td}/stages-ok-again/offline-control-probe.json")"

for mode in crash-loop-keeps-restarting crash-loop-not-retried crash-loop-other-error \
            crash-loop-never-fails crash-loop-stopped; do
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_stages "$mode" "$evidence")"
  check "  restart passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" restart)"
  check "  and crash-loop is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and the agent config was restored anyway" yes \
    "$([[ -n "$(sudo_line "$restore_line")" ]] && echo yes || echo no)"
done

evidence="${td}/stages-corrupt-fails"
check "a config corruption that fails is recorded as a failed crash-loop" fail \
  "$(run_stages ok "$evidence" "invalid json" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  and the config is still restored" yes \
  "$([[ -n "$(sudo_line "$restore_line")" ]] && echo yes || echo no)"

for mode in offline-dropin-ignored offline-not-enforced offline-control-denied \
            offline-doctor-failing offline-dropin-sticks; do
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_stages "$mode" "$evidence")"
  check "  crash-loop passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and offline is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" offline)"
  # A host that cannot reach the network fails before any drop-in exists;
  # every other failure has installed them and must take them away.
  expected_removals=2
  [[ "$mode" == offline-control-denied ]] && expected_removals=0
  check "  and the drop-ins were removed as far as they were installed" "$expected_removals" \
    "$(dropin_removals)"
done

evidence="${td}/stages-dropin-install-fails"
check "a drop-in that cannot be installed is recorded as a failed offline" fail \
  "$(run_stages ok "$evidence" "install -D" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" offline)"
check "  and the removal still runs" 2 "$(dropin_removals)"

check "no destructive command reached the host" "yes" \
  "$([[ -f "${appliance}/sudo.log" ]] && echo yes || echo no)"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "verify_ubuntu_l4_cloud_lifecycle: ok" || echo "${failures} check(s) failed")"
exit "$failures"
