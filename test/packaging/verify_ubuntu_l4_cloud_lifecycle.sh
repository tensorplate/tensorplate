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
assert set(run) == {"install", "deploy-smoke", "status-logs", "restart"}, sorted(run)
assert set(skipped) == {"upgrade", "rollback", "crash-loop", "offline"}, sorted(skipped)

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
print("stage coverage: 4 run, 4 skipped with reasons, digest recorded after install")
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
for tool in sudo systemctl dpkg; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf '#!/bin/sh\nexit 0\n' >"${stub_bin}/${tool}"
    chmod +x "${stub_bin}/${tool}"
  fi
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
: >"${assets}/SHA256SUMS"
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

# sudo records and succeeds without executing: the harness purges
# packages and removes system directories, and none of that may happen
# to the machine running this suite.
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
exit 0
STUB
cat >"${appliance}/bin/systemctl" <<'STUB'
#!/bin/sh
case "$1" in
  show)
    case "$*" in
      *ActiveState*) printf 'active\n' ;;
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
cat >"${appliance}/bin/journalctl" <<'STUB'
#!/bin/sh
printf 'stub journal line for %s\n' "$*"
STUB
cat >"${appliance}/bin/tensorplate" <<'STUB'
#!/bin/sh
# A stubbed appliance. TP_FAKE_MODE selects which way it misbehaves.
mode="${TP_FAKE_MODE:-ok}"
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
    printf '{"command":"status","payload":{"severity":"ready","agent":{"agent_state":"ready","active":{"deployment_id":"%s","backend":"python_pytorch","serving_url":"http://127.0.0.1:%s/infer"}}}}\n' \
      "${TP_FAKE_DEPLOYMENT_ID}" "${TP_FAKE_SERVING_PORT}"
    ;;
  infer)
    name=echo_probe
    payload=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["inputs"][0]["payload_b64"])' "$input")
    if [ "$mode" = "infer-garbled" ]; then payload="AAAA"; fi
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
        body = json.dumps({
            "state": "ready",
            "active_model_id": (directory / "deployment-id").read_text().strip(),
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
  env PATH="${appliance}/bin:${PATH}" \
    TP_FAKE_SUDO_FAIL="$sudo_fail" \
    TP_CLOUD_ARCH=x86_64 \
    TP_CLOUD_OS_RELEASE="${td}/os-release.noble" \
    TP_CLOUD_NVIDIA_VERSION="${td}/nvidia-version" \
    TP_CLOUD_PYTHON="${td}/python-with-torch" \
    TP_CLOUD_AGENT_SOCKET="${appliance}/run/agent.sock" \
    TP_CLOUD_LOG_DIR="${appliance}/log" \
    TP_CLOUD_BUNDLE_STAGING="${appliance}/staged-bundle" \
    TP_FAKE_MODE="$mode" \
    TP_FAKE_SUDO_LOG="${appliance}/sudo.log" \
    TP_FAKE_PID_FILE="${appliance}/pid" \
    TP_FAKE_DEPLOYMENT_ID="$deployment_id" \
    TP_FAKE_SERVING_PORT="$serving_port" \
    bash "$harness" \
      --assets-dir "$assets" \
      --bundle-dir "$bundle" \
      --evidence-dir "$evidence" \
      --tested-version 0.2.1 \
      --confirm RESET-TENSORPLATE >"${evidence}.out" 2>"${evidence}.err"
  local status=$?
  set -e
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
for stage in install deploy-smoke status-logs restart; do
  check "  ${stage} is recorded as a pass" "pass" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
for stage in upgrade rollback crash-loop offline; do
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

check "no destructive command reached the host" "yes" \
  "$([[ -f "${appliance}/sudo.log" ]] && echo yes || echo no)"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "verify_ubuntu_l4_cloud_lifecycle: ok" || echo "${failures} check(s) failed")"
exit "$failures"
