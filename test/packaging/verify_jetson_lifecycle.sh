#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# packaging: Jetson lifecycle harness verifier.
#
# The harness runs natively on a Jetson, so CI can never execute a real
# run of it. What CI can execute is the part that decides whether a run
# may start at all, and every stage body against a stubbed appliance:
# lifecycle_stage calls a stage from a tested context, which suspends
# errexit inside it, so a stage whose assertions never fire certifies a
# broken device as a validated one, and that is invisible to reading.
#
# The harness is invoked through "$BASH", so running this file under
# macOS /bin/bash drives the harness under bash 3.2 as well.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
harness="${repo_root}/tools/validation/jetson-lifecycle.sh"
builder="${repo_root}/tools/validation/create_trt_identity_bundle.sh"
schema="${repo_root}/config/schemas/lifecycle_report.json"

[[ -x "$harness" ]] || { printf 'FAIL: harness is not executable\n' >&2; exit 1; }

"$BASH" -n "$harness"
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "$harness"
else
  printf 'verify_jetson_lifecycle: shellcheck not found; skipping shellcheck\n'
fi
"$BASH" "$harness" --help >/dev/null

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
                    "crash-loop"}, sorted(run)
assert set(skipped) == {"upgrade", "rollback", "offline"}, sorted(skipped)

# Every skip states a reason, and the reason names the work that closes
# it. An unexplained skip is indistinguishable from a stage nobody
# thought about.
for stage in skipped:
    match = re.search(
        r"lifecycle_skip\s+" + re.escape(stage) + r"\s*\\\n\s*\"([^\"]+)\"", body
    )
    assert match, f"{stage} is skipped without a quoted reason"
    assert len(match.group(1)) > 40, f"{stage}'s skip reason is too thin: {match.group(1)}"
    assert "follow-up" in match.group(1), f"{stage}'s skip reason names no follow-up work"

# The digest must be recorded after the install stage passed, so it
# attests an install that happened. Matched as a call rather than as a
# mention, so a comment naming the function cannot satisfy this.
install_call = re.search(r"^\s*lifecycle_stage\s+install\s", body, re.M)
digest_call = re.search(r"^\s*lifecycle_artifact_digest\s", body, re.M)
assert install_call, "the harness does not run an install stage"
assert digest_call, "the harness never records an artifact digest"
assert digest_call.start() > install_call.start(), \
    "the artifact digest is recorded before the install stage"

# lifecycle_begin clears the digest sidecar so a retry cannot inherit a
# previous attempt's digest; a harness that wrote it first would delete
# its own.
begin_call = re.search(r"^\s*lifecycle_begin\s", body, re.M)
assert begin_call, "the harness never calls lifecycle_begin"
assert "artifact-digest.txt" not in body[: begin_call.start()], \
    "the harness writes the digest sidecar before lifecycle_begin, which clears it"
print("stage coverage: 5 run, 3 skipped with follow-up reasons, digest recorded after install")
PY

# --- the bundle must be staged somewhere the sandboxed agent can see.
#
# Read from the shipped unit rather than from a list kept here, so
# tightening the unit fails this check instead of a device run.
python3 - "$harness" "${repo_root}/packaging/debian/tensorplate-agent.service" <<'PY'
import re, sys

harness_path, unit_path = sys.argv[1:]
body = open(harness_path, encoding="utf-8").read()
unit = open(unit_path, encoding="utf-8").read()

match = re.search(r'^BUNDLE_STAGING_DIR="\$\{TP_JETSON_BUNDLE_STAGING:-([^}]+)\}"', body, re.M)
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

# --- the honesty claim.
if ! grep -Fq 'does NOT' "$harness"; then
  printf 'FAIL: the harness must say what a pass does NOT establish\n' >&2
  exit 1
fi

# --- fixtures.
cat >"${td}/os-release.jammy" <<'EOF'
ID=ubuntu
VERSION_ID="22.04"
PRETTY_NAME="Ubuntu 22.04.5 LTS"
EOF
cat >"${td}/os-release.noble" <<'EOF'
ID=ubuntu
VERSION_ID="24.04"
PRETTY_NAME="Ubuntu 24.04.1 LTS"
EOF
# Synthetic GCID and date; the shape of the first line L4T writes.
printf '# R36 (release), REVISION: 4.3, GCID: 00000000, BOARD: generic, EABI: aarch64, DATE: synthetic\n' \
  >"${td}/nv-r36"
printf '# R35 (release), REVISION: 6.0, GCID: 00000000, BOARD: generic, EABI: aarch64, DATE: synthetic\n' \
  >"${td}/nv-r35"
mkdir -p "${td}/cuda/include"
: >"${td}/cuda/include/cuda_runtime_api.h"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# A candidate set in the shape `jetson-clean-room.sh download` leaves:
# install.sh, one manifest, SHA256SUMS and its cosign bundle, and debs.
# Each fixture .deb is a control-style text file the dpkg-deb stub reads.
make_assets() {
  local dir="$1" manifest_tag="$2" version="$3" variant="${4:-}"
  mkdir -p "$dir"
  printf '#!/bin/sh\nexit 0\n' >"${dir}/install.sh"
  printf '{}\n' >"${dir}/SHA256SUMS.cosign.bundle"
  python3 - "$dir" "$manifest_tag" "$version" "$variant" <<'PY'
import json, pathlib, sys

directory, tag, version, variant = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
artifacts = []
for package, arch, deb_version in (
    ("tensorplate-common", "all", version),
    ("tensorplate-agent", "arm64", version),
    # Another architecture's build of the same package, at a version no
    # arm64 install could report: selecting it would fail the run.
    ("tensorplate-agent", "amd64", "9.9.9-1"),
    ("tensorplate-serving", "arm64", version),
    ("tensorplate-observability", "arm64", version),
    ("tensorplate-cli", "arm64", version),
    ("tensorplate-backend-python-pytorch", "all", version),
    ("tensorplate-apt-source", "all", version),
):
    name = f"{package}_{deb_version}_{arch}.deb"
    (directory / name).write_text(
        f"Package: {package}\nVersion: {deb_version}\nArchitecture: {arch}\n", encoding="utf-8"
    )
    artifacts.append({"file": name, "package": package, "architecture": arch})
if variant == "duplicate-cli":
    # A manifest naming the CLI twice for this architecture, which leaves
    # no single package to compare the installed version against.
    artifacts.append({"file": f"tensorplate-cli_{version}_arm64.deb",
                      "package": "tensorplate-cli", "architecture": "arm64"})
manifest ={"release": {"tag": tag, "version": version}, "artifacts": artifacts}
(directory / "tensorplate-v0.2.1-rc.2-artifacts.json").write_text(
    json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
)
PY
  # A real checksum file over non-empty files: GNU coreutils rejects a
  # checksum file with no properly formatted lines.
  if command -v sha256sum >/dev/null 2>&1; then
    ( cd "$dir" && sha256sum ./*.deb ./*.json install.sh SHA256SUMS.cosign.bundle | sed 's# \./# #' >SHA256SUMS )
  else
    ( cd "$dir" && shasum -a 256 ./*.deb ./*.json install.sh SHA256SUMS.cosign.bundle | sed 's# \./# #' >SHA256SUMS )
  fi
}

candidate_version='0.2.1~rc.2-1'
assets="${td}/assets"
make_assets "$assets" v0.2.1-rc.2 "$candidate_version"
candidate_digest="$(sha256_of "${assets}/SHA256SUMS")"

# --- stubs, placed unconditionally.
#
# Not only where the real tool is absent: on a Linux runner the real
# systemctl and dpkg-query exist, and a run that let them through would
# answer about the runner rather than about the harness.
stub_bin="${td}/bin"
mkdir -p "$stub_bin" "${td}/run" "${td}/log" "${td}/scratch"
real_mktemp="$(command -v mktemp)"
real_sha256sum="$(command -v sha256sum || true)"

# An explicit template keeps every scratch directory -- the harness's and
# the bundle generator's -- inside the fixture, including backups
# deliberately retained after a failed cleanup.
cat >"${stub_bin}/mktemp" <<'STUB'
#!/bin/sh
if [ "$#" -eq 1 ] && [ "$1" = -d ]; then
  exec "${TP_FAKE_MKTEMP}" -d "${TMPDIR}/tmp.XXXXXXXXXX"
fi
if [ "$#" -eq 2 ] && [ "$1" = -d ]; then
  case "$2" in "${TMPDIR}/"*) exec "${TP_FAKE_MKTEMP}" -d "$2" ;; esac
fi
exit 9
STUB

# sudo records without executing privileged commands. The only commands
# it carries out act on fixture paths: the staged bundle, the agent config
# fixture and its backup, and markers the other stubs read.
cat >"${stub_bin}/sudo" <<'STUB'
#!/bin/sh
printf '%s\n' "$*" >>"${TP_FAKE_SUDO_LOG}"
# Injectable failure, so a privileged step that the harness forgot to
# check can be caught here rather than on a device.
if [ -n "${TP_FAKE_SUDO_FAIL:-}" ]; then
  case "$*" in
    *"${TP_FAKE_SUDO_FAIL}"*) exit 9 ;;
  esac
fi
case "$*" in
  *"apt-get purge"*) : >"${TP_FAKE_PURGE_MARKER}" ;;
  *"systemctl restart"*)
    : >"${TP_FAKE_RESTART_MARKER}"
    if [ "${TP_FAKE_MODE:-ok}" = restart-socket-missing ]; then
      rm -f "${TP_FAKE_SOCKET}"
    fi
    ;;
  *"systemctl start"*) : >"${TP_FAKE_START_MARKER}" ;;
  "bash "*"/install.sh --local-artifacts "*)
    : >"${TP_FAKE_INSTALLED_MARKER}"
    if [ "${TP_FAKE_MODE:-ok}" = install-socket-missing ]; then
      rm -f "${TP_FAKE_SOCKET}"
    fi
    ;;
  "rm -rf ${TP_FAKE_STAGING}") rm -rf "${TP_FAKE_STAGING}" ;;
  "mkdir -p "*)
    [ "$3" = "$(dirname "${TP_FAKE_STAGING}")" ] || exit 9
    mkdir -p "$3"
    ;;
  "cp -R "*" ${TP_FAKE_STAGING}") cp -R "$3" "${TP_FAKE_STAGING}" ;;
  "chmod -R a+rX ${TP_FAKE_STAGING}") chmod -R a+rX "${TP_FAKE_STAGING}" ;;
  "cp -p /etc/tensorplate/agent.json "*)
    case "$4" in "${TMPDIR}/"*/agent.json) ;; *) exit 9 ;; esac
    cp "${TP_FAKE_AGENT_CONFIG}" "$4" || exit
    printf '%s\n' "$4" >"${TP_FAKE_BACKUP_PATH}"
    ;;
  *"invalid json"*)
    printf '{ invalid json\n' >"${TP_FAKE_AGENT_CONFIG}"
    : >"${TP_FAKE_CONFIG_BROKEN}"
    case "${TP_FAKE_MODE:-ok}" in
      crash-loop-signal-int) kill -INT "$PPID" ;;
      crash-loop-signal-term|crash-loop-signal-twice) kill -TERM "$PPID" ;;
      crash-loop-signal-hup) kill -HUP "$PPID" ;;
    esac
    ;;
  "cp -p "*" /etc/tensorplate/agent.json")
    case "$3" in "${TMPDIR}/"*/agent.json) ;; *) exit 9 ;; esac
    case "${TP_FAKE_MODE:-ok}" in
      # A second signal while the first one's cleanup is restoring.
      crash-loop-signal-twice) kill -TERM "$PPID" ;;
      crash-loop-restore-fails-once)
        if [ ! -f "${TP_FAKE_RESTORE_FAILED}" ]; then
          : >"${TP_FAKE_RESTORE_FAILED}"
          exit 9
        fi
        ;;
      crash-loop-restore-always-fails) exit 9 ;;
    esac
    cp "$3" "${TP_FAKE_AGENT_CONFIG}" || exit
    rm -f "${TP_FAKE_CONFIG_BROKEN}"
    ;;
esac
if [ "$1" = journalctl ]; then
  shift
  exec "${TP_FAKE_JOURNALCTL}" "$@"
fi
exit 0
STUB

# dpkg's package database, in the two shapes the harness queries it: a
# `name status` listing of tensorplate*, and one package's status and
# version. The apt channel's bootstrap package is always installed, so
# every run shows whether the harness leaves it alone, and dpkg always
# remembers a package that was once available but is not installed,
# which the purge must not name.
cat >"${stub_bin}/dpkg-query" <<'STUB'
#!/bin/sh
mode="${TP_FAKE_MODE:-ok}"
case "$*" in
  *'binary:Package'*)
    printf 'tensorplate-apt-source installed\n'
    printf 'tensorplate-backend-python-pytorch not-installed\n'
    case "$mode" in
      installed-runtime)
        [ -f "${TP_FAKE_PURGE_MARKER}" ] && exit 0
        for pkg in tensorplate-agent tensorplate-serving tensorplate-observability \
                   tensorplate-cli tensorplate-common; do
          printf '%s installed\n' "$pkg"
        done
        ;;
      purge-leaves-packages) printf 'tensorplate-common config-files\n' ;;
    esac
    exit 0
    ;;
esac
pkg=""
for arg in "$@"; do pkg="$arg"; done
if [ ! -f "${TP_FAKE_INSTALLED_MARKER}" ] || [ "$mode:$pkg" = "package-missing:tensorplate-serving" ]; then
  printf 'dpkg-query: no packages found matching %s\n' "$pkg" >&2
  exit 1
fi
version="${TP_FAKE_CANDIDATE_VERSION}"
[ "$mode:$pkg" = "stale-version:tensorplate-agent" ] && version='0.2.1~rc.1-1'
printf 'installed %s' "$version"
STUB

cat >"${stub_bin}/dpkg-deb" <<'STUB'
#!/bin/sh
[ "$1" = -f ] && [ "$3" = Version ] || exit 9
case "${TP_FAKE_MODE:-ok}:$2" in
  deb-unreadable:*/tensorplate-cli_*)
    printf 'dpkg-deb: error: archive has premature member\n' >&2
    exit 2
    ;;
esac
sed -n 's/^Version: //p' "$2"
STUB

cat >"${stub_bin}/dpkg" <<'STUB'
#!/bin/sh
exit 0
STUB

# The group database and this session's groups. The tensorplate group
# exists and the session is in it, unless a case says the device never
# had TensorPlate installed or the operator has not joined the group.
cat >"${stub_bin}/getent" <<'STUB'
#!/bin/sh
[ "$1" = group ] && [ "$2" = tensorplate ] || exit 2
[ "${TP_FAKE_GROUP:-member}" = absent ] && exit 2
printf 'tensorplate:x:998:\n'
STUB

cat >"${stub_bin}/id" <<'STUB'
#!/bin/sh
[ "$1" = -nG ] || exit 9
if [ "${TP_FAKE_GROUP:-member}" = member ]; then
  printf 'operator adm sudo tensorplate\n'
else
  printf 'operator adm sudo\n'
fi
STUB

cat >"${stub_bin}/nvpmodel" <<'STUB'
#!/bin/sh
printf 'NV Power Mode: MAXN_SUPER\n2\n'
STUB

# The compiler the bundle generator calls. It leaves a "builder" that
# writes fixture engine bytes, so the real generator -- manifest, digest
# and sample request -- runs unchanged.
cat >"${stub_bin}/c++" <<'STUB'
#!/bin/sh
printf '%s\n' "$*" >>"${TP_FAKE_CXX_LOG}"
if [ "${TP_FAKE_MODE:-ok}" = builder-fails ]; then
  printf 'fatal error: NvInfer.h: No such file or directory\n' >&2
  exit 1
fi
out=""
previous=""
for arg in "$@"; do
  [ "$previous" = -o ] && out="$arg"
  previous="$arg"
done
[ -n "$out" ] || exit 9
printf '#!/bin/sh\nprintf "fixture identity engine\\n" >"$1"\n' >"$out"
chmod +x "$out"
STUB

cat >"${stub_bin}/systemctl" <<'STUB'
#!/bin/sh
case "$1" in
  --version)
    printf 'systemd 249 (249.11-0ubuntu3.12)\n+PAM +AUDIT\n'
    ;;
  show)
    broken=0
    [ -f "${TP_FAKE_CONFIG_BROKEN}" ] && broken=1
    # systemctl failing to answer about the looping unit.
    if [ "$broken" -eq 1 ]; then
      case "${TP_FAKE_MODE:-ok}:$*" in
        state-show-fails:*ActiveState*|restarts-show-fails:*NRestarts*|result-show-fails:*Result*) exit 1 ;;
      esac
    fi
    case "$*" in
      *ActiveState*)
        # A looping unit reads as failed between attempts, which is why
        # the harness must not settle on that state alone. One stopped by
        # something else reads as inactive, which is not a crash loop.
        state=active
        if [ "$broken" -eq 1 ]; then
          case "${TP_FAKE_MODE:-ok}" in
            crash-loop-never-fails) state=active ;;
            crash-loop-activating) state=activating ;;
            crash-loop-stopped) state=inactive ;;
            *) state=failed ;;
          esac
        else
          # A unit that never comes up: after install, after the restart
          # stage, or after crash-loop restores the config and starts it.
          case "${TP_FAKE_MODE:-ok}:$*" in
            install-agent-inactive:*tensorplate-agent*|install-observability-inactive:*tensorplate-observability*)
              state=inactive
              ;;
            restart-agent-inactive:*tensorplate-agent*)
              if [ -f "${TP_FAKE_RESTART_MARKER}" ]; then state=inactive; fi
              ;;
            crash-loop-agent-not-ready:*tensorplate-agent*)
              if [ -f "${TP_FAKE_START_MARKER}" ]; then state=inactive; fi
              ;;
          esac
        fi
        printf '%s\n' "$state"
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
      *InvocationID*)
        case "${TP_FAKE_MODE:-ok}:$*" in
          # A unit that is not running has no current invocation.
          invocation-empty:*tensorplate-agent*) printf '\n' ;;
          *tensorplate-agent*) printf '11111111111111111111111111111111\n' ;;
          *tensorplate-observability*) printf '22222222222222222222222222222222\n' ;;
          *) exit 9 ;;
        esac
        ;;
      *MainPID*)
        # A restart must change the pid, so hand back a new one each call,
        # except for the unit a mode says kept its process, or the one
        # read a mode says systemctl could not answer.
        reads=$(cat "${TP_FAKE_MAINPID_CALLS}" 2>/dev/null || echo 0)
        reads=$((reads + 1))
        printf '%s\n' "$reads" >"${TP_FAKE_MAINPID_CALLS}"
        if [ "${TP_FAKE_MODE:-ok}" = "mainpid-show-fails-${reads}" ]; then
          exit 1
        fi
        case "${TP_FAKE_MODE:-ok}:$*" in
          restart-agent-pid-unchanged:*tensorplate-agent*|restart-observability-pid-unchanged:*tensorplate-observability*)
            printf '100\n'
            exit 0
            ;;
        esac
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

cat >"${stub_bin}/journalctl" <<'STUB'
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
  unit=tensorplate-agent.service
  records=5
  case "${TP_FAKE_MODE:-ok}" in
    crash-loop-other-error) message='state store error: permission denied' ;;
    crash-loop-other-unit) unit=tensorplate-observability.service ;;
    crash-loop-one-config-error) records=1 ;;
  esac
  written=0
  while [ "$written" -lt "$records" ]; do
    printf '{"_SYSTEMD_UNIT":"%s","MESSAGE":"%s"}\n' "$unit" "$message"
    written=$((written + 1))
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
  journal-not-object:tensorplate-agent.service) printf '%s\n' '"fixture service started"'; exit 0 ;;
  journal-empty-observability:tensorplate-observability.service) exit 0 ;;
  journal-stale-invocation:*) invocation=ffffffffffffffffffffffffffffffff ;;
  journal-wrong-unit:*) unit=another.service ;;
esac
message='fixture service started'
[ "${TP_FAKE_MODE:-ok}" = journal-empty-message ] && message=''
printf '{"_SYSTEMD_INVOCATION_ID":"%s","_SYSTEMD_UNIT":"%s","MESSAGE":"%s","__REALTIME_TIMESTAMP":"1789300000000000"}\n' \
  "$invocation" "$unit" "$message"
STUB

cat >"${stub_bin}/tensorplate" <<'STUB'
#!/bin/sh
# A stubbed appliance. TP_FAKE_MODE selects which way it misbehaves.
mode="${TP_FAKE_MODE:-ok}"
phase=initial
[ -f "${TP_FAKE_RESTART_MARKER}" ] && phase=restarted
command="$1"
shift
out=""
input=""
bundle=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-file) out="$2"; shift 2 ;;
    --input) input="$2"; shift 2 ;;
    --deployment-id|--output|--component|--tail) shift 2 ;;
    *) bundle="$1"; shift ;;
  esac
done
case "$command" in
  doctor)
    # Row identity comes from the messages, so a mode can resolve another
    # row without changing any status. A failing finding makes the real
    # CLI exit 10 after writing its report, and the stub does the same.
    failing=0
    row_status=ok
    row=jetson-orin-nano-8gb-jp62
    profile_row=jetson-orin-nano-8gb-jp62
    warned=""
    case "$mode" in
      doctor-failing) failing=1 ;;
      wrong-row) row_status=warning ;;
      other-row) row=jetson-orin-nx-16gb-jp62 ;;
      profile-other-row) profile_row=jetson-orin-nx-16gb-jp62 ;;
      doctor-warns-*) warned="${mode#doctor-warns-}" ;;
    esac
    status_of() {
      if [ "$1" = "$warned" ]; then printf warning; else printf ok; fi
    }
    serving_status="$(status_of serving_binary_installed)"
    if [ "$failing" -ne 0 ]; then
      serving_status=fail
    fi
    cat <<JSON
{"command":"doctor","payload":{"failing":${failing},"findings":[
 {"id":"platform_row","status":"${row_status}","message":"resolved ${row}"},
 {"id":"platform_profile","status":"ok","message":"host matches 1 candidate support row(s): ${profile_row}"},
 {"id":"host_os","status":"ok","message":"JetPack 6.2 (L4T r36.4.3)"},
 {"id":"accelerator_facts","status":"ok","message":"integrated accelerator: Orin"},
 {"id":"tensorrt_runtime","status":"ok","message":"libnvinfer present"},
 {"id":"cuda_runtime","status":"ok","message":"libcudart present"},
 {"id":"platform_registry","status":"$(status_of platform_registry)","message":"ok"},
 {"id":"agent_reachable","status":"$(status_of agent_reachable)","message":"ok"},
 {"id":"agent_socket","status":"$(status_of agent_socket)","message":"ok"},
 {"id":"serving_binary_installed","status":"${serving_status}","message":"ok"},
 {"id":"python_pytorch_backend","status":"missing","message":"no Python/PyTorch backend descriptor"},
 {"id":"path_layout","status":"$(status_of path_layout)","message":"ok"},
 {"id":"config_files","status":"$(status_of config_files)","message":"ok"}]}}
JSON
    if [ "$failing" -ne 0 ]; then
      exit 10
    fi
    if [ "$mode" = doctor-exits-nonzero ]; then
      exit 1
    fi
    ;;
  deploy)
    printf '%s\n' "$bundle" >>"${TP_FAKE_DEPLOY_LOG}"
    if [ "$mode" = deploy-fails ]; then
      printf 'error: the agent rejected the deployment\n' >&2
      exit 3
    fi
    deploy_phase=active
    deployed="${TP_FAKE_DEPLOYMENT_ID}"
    [ "$mode" = deploy-not-active ] && deploy_phase=rolled_back
    [ "$mode" = deploy-other-id ] && deployed=a-different-deployment
    printf '{"command":"deploy","payload":{"phase":"%s","deployment_id":"%s"}}\n' \
      "$deploy_phase" "$deployed"
    ;;
  status)
    # Status is read once by deploy-smoke, once by status-logs, and once
    # each by the restart and crash-loop recoveries, so a mode can target
    # the status-logs read alone by its position.
    reads=$(cat "${TP_FAKE_STATUS_CALLS}" 2>/dev/null || echo 0)
    reads=$((reads + 1))
    printf '%s\n' "$reads" >"${TP_FAKE_STATUS_CALLS}"
    command_name=status
    severity=ready
    agent_state=ready
    active_id="${TP_FAKE_DEPLOYMENT_ID}"
    backend=tensorrt
    supervision=""
    serving_url="\"http://127.0.0.1:${TP_FAKE_SERVING_PORT}/infer\""
    case "$mode" in
      status-fails)
        printf 'error: agent unreachable\n' >&2
        exit 4
        ;;
      status-degraded) severity=degraded ;;
      agent-not-ready) agent_state=recovering ;;
      status-other-deployment) active_id=a-different-deployment ;;
      status-wrong-backend) backend=python_pytorch ;;
      supervision-ready) supervision=',"supervision":{"serving_state":"ready","crash_loop":false}' ;;
      supervision-failed) supervision=',"supervision":{"serving_state":"failed","crash_loop":false}' ;;
      supervision-crash-loop) supervision=',"supervision":{"serving_state":"ready","crash_loop":true}' ;;
      url-not-http) serving_url="\"https://127.0.0.1:${TP_FAKE_SERVING_PORT}/infer\"" ;;
      url-not-loopback) serving_url="\"http://localhost:${TP_FAKE_SERVING_PORT}/infer\"" ;;
      url-wrong-path) serving_url="\"http://127.0.0.1:${TP_FAKE_SERVING_PORT}/predict\"" ;;
    esac
    [ "$mode:$phase" = restart-no-worker:restarted ] && serving_url=null
    if [ "$reads" -eq 2 ]; then
      case "$mode" in
        statuslogs-status-fails)
          printf 'error: agent unreachable\n' >&2
          exit 4
          ;;
        statuslogs-degraded) severity=degraded ;;
        statuslogs-lost-deployment) active_id=a-different-deployment ;;
        statuslogs-wrong-command) command_name=doctor ;;
      esac
    fi
    printf '{"command":"%s","payload":{"severity":"%s","agent":{"agent_state":"%s","active":{"deployment_id":"%s","backend":"%s","serving_url":%s}%s}}}\n' \
      "$command_name" "$severity" "$agent_state" "$active_id" "$backend" "$serving_url" "$supervision"
    ;;
  infer)
    printf '%s %s\n' "$phase" "$input" >>"${TP_FAKE_INFER_LOG}"
    # An agent that cannot load its config serves nothing, so a recovery
    # checked before the config is restored cannot pass.
    if [ -f "${TP_FAKE_CONFIG_BROKEN}" ]; then
      printf 'error: agent unavailable\n' >&2
      exit 4
    fi
    if [ "$mode" = infer-fails ]; then
      printf 'error: inference failed\n' >&2
      exit 11
    fi
    garble=0
    [ "$mode" = infer-garbled ] && garble=1
    [ "$mode:$phase" = restart-infer-garbled:restarted ] && garble=1
    # Only once crash-loop has backed up the config, so every stage
    # before it passes and only the recovery answers wrongly.
    [ "$mode" = crashloop-recovery-garbled ] && [ -f "${TP_FAKE_BACKUP_PATH}" ] && garble=1
    python3 - "$input" "$out" "$garble" <<'PY' || exit 1
import base64, json, struct, sys

request = json.load(open(sys.argv[1], encoding="utf-8"))
payload = request["inputs"][0]["payload_b64"]
if sys.argv[3] == "1":
    # The right size and shape, the wrong values.
    payload = base64.b64encode(struct.pack("<48f", *([0.0] * 48))).decode("ascii")
response = {
    "status": "success",
    "outputs": [{
        "name": "features",
        "tensor": {"dtype": "float32", "layout": "row_major", "shape": [1, 3, 4, 4],
                   "byte_offset": 0, "byte_size": 192},
        "payload_b64": payload,
    }],
}
json.dump(response, open(sys.argv[2], "w", encoding="utf-8"))
print(json.dumps({"command": "infer", "payload": {"result": response}}))
PY
    ;;
  logs)
    # The real CLI fails here: nothing writes the configured file log on
    # a packaged Linux install. The stub reproduces that.
    printf 'no log_source.path configured\n' >&2
    exit 2
    ;;
esac
STUB
chmod +x "${stub_bin}/"*

# A bundle built by the real generator through the stubbed compiler, for
# the runs that pass --bundle-dir and for the malformed variants.
env PATH="${stub_bin}:${PATH}" TMPDIR="${td}/scratch" TP_FAKE_MKTEMP="$real_mktemp" \
  TP_FAKE_CXX_LOG="${td}/cxx-prebuild.log" CUDA_HOME="${td}/cuda" \
  sh "$builder" "${td}/bundle-good" >/dev/null
for variant in wrong-backend wrong-kind bad-digest two-models escapes-root bad-sample no-sample no-manifest; do
  cp -R "${td}/bundle-good" "${td}/bundle-${variant}"
done
python3 - "$td" <<'PY'
import json, pathlib, sys

root = pathlib.Path(sys.argv[1])
for variant, edit in (
    ("wrong-backend", lambda m: m.update(backend_hint="python_pytorch")),
    ("wrong-kind", lambda m: m["artifacts"][0].update(kind="onnx_model")),
    ("two-models", lambda m: m["artifacts"].append(dict(m["artifacts"][0]))),
    # The same engine bytes and digest, reached from outside the bundle.
    ("escapes-root", lambda m: m["artifacts"][0].update(path="../bundle-good/model.engine")),
):
    path = root / f"bundle-{variant}" / "manifest.json"
    manifest = json.loads(path.read_text(encoding="utf-8"))
    assert manifest["artifacts"][0]["role"] == "model", manifest
    edit(manifest)
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
with (root / "bundle-bad-digest" / "model.engine").open("a", encoding="utf-8") as engine:
    engine.write("tampered\n")
(root / "bundle-bad-sample" / "sample_infer.json").write_text("not a request\n", encoding="utf-8")
(root / "bundle-no-sample" / "sample_infer.json").unlink()
(root / "bundle-no-manifest" / "manifest.json").unlink()
PY

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

# --- eligibility, executed.
#
# Runs preflight with every seam pointed at a fixture and returns the
# harness's exit status. --preflight-only stops before the first
# privileged command; the sudo log shows it did.
#
# A case may set preflight_group (member, not-member or absent),
# preflight_staging (the bundle staging seam; empty means the harness
# default) or preflight_path (directories put ahead of the stubs) for
# the one call.
preflight() {
  local arch="$1" os_release="$2" nv="$3" evidence="$4" version="$5" tag="$6" assets_dir="$7"
  shift 7
  set +e
  rm -rf "${td}/preflight-scratch"
  mkdir -p "${td}/preflight-scratch"
  : >"${td}/preflight-sudo.log"
  : >"${td}/preflight-cxx.log"
  env PATH="${preflight_path:-}${stub_bin}:${PATH}" \
    TP_FAKE_GROUP="${preflight_group:-member}" \
    TP_JETSON_BUNDLE_STAGING="${preflight_staging:-}" \
    TP_FAKE_REAL_SHA256SUM="$real_sha256sum" \
    TMPDIR="${td}/preflight-scratch" \
    TP_FAKE_MKTEMP="$real_mktemp" \
    TP_FAKE_SUDO_LOG="${td}/preflight-sudo.log" \
    TP_FAKE_CXX_LOG="${td}/preflight-cxx.log" \
    CUDA_HOME="${td}/cuda" \
    TP_JETSON_ARCH="$arch" \
    TP_JETSON_OS_RELEASE="$os_release" \
    TP_JETSON_NV_TEGRA_RELEASE="$nv" \
    "$BASH" "$harness" \
      --candidate-tag "$tag" \
      --candidate-assets-dir "$assets_dir" \
      --evidence-dir "$evidence" \
      --tested-version "$version" \
      --preflight-only \
      "$@" >"${td}/preflight.out" 2>"${td}/preflight.err"
  local status=$?
  set -e
  printf '%s' "$status"
}
said() {
  grep -Fq -- "$1" "${td}/preflight.err" && echo yes || echo no
}

jammy="${td}/os-release.jammy"
r36="${td}/nv-r36"
confirm=(--confirm RESET-TENSORPLATE)

check "an eligible host passes preflight" "0" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-ok" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and builds the bundle with the device's compiler" yes \
  "$([[ -s "${td}/preflight-cxx.log" ]] && echo yes || echo no)"
check "  and writes no evidence" "no" \
  "$([[ -e "${td}/evidence-ok" ]] && echo yes || echo no)"
check "  and leaves no scratch bundle behind" "" \
  "$(find "${td}/preflight-scratch" -mindepth 1 -print -quit)"
check "  and runs nothing privileged" "" "$(cat "${td}/preflight-sudo.log")"

check "a pre-built --bundle-dir passes preflight" "0" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-prebuilt" 0.2.1 v0.2.1-rc.2 "$assets" \
     --bundle-dir "${td}/bundle-good" "${confirm[@]}")"
check "  and compiles nothing" "" "$(cat "${td}/preflight-cxx.log")"

for lacking in no-sample no-manifest; do
  check "a --bundle-dir with ${lacking} is refused" "1" \
    "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-${lacking}" 0.2.1 v0.2.1-rc.2 "$assets" \
       --bundle-dir "${td}/bundle-${lacking}" "${confirm[@]}")"
  check "  and names what the bundle lacks" yes "$(said 'must contain manifest.json and sample_infer.json')"
done

# The run deletes these before it reads or writes what they hold, after
# the device has already been purged. The clean-room smoke's own bundle
# location is under /var/lib/tensorplate, and each path is refused for
# where it is, whether or not it exists on this machine.
for deleted in /etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate; do
  check "a --bundle-dir under ${deleted} is refused" "1" \
    "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-bundle-deleted" 0.2.1 v0.2.1-rc.2 "$assets" \
       --bundle-dir "${deleted}/validation/tensorplate-trt-identity-bundle" "${confirm[@]}")"
  check "  and says the run deletes it" yes "$(said "is under ${deleted}, which this run deletes")"
done
check "a --bundle-dir that is the default staging copy is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-bundle-staged" 0.2.1 v0.2.1-rc.2 "$assets" \
     --bundle-dir /opt/tensorplate-validation/trt-identity "${confirm[@]}")"
check "  and says the run deletes it" yes \
  "$(said 'is under /opt/tensorplate-validation/trt-identity, which this run deletes')"
check "an assets directory that is the staging directory is refused" "1" \
  "$(preflight_staging="$assets" \
     preflight aarch64 "$jammy" "$r36" "${td}/evidence-assets-staged" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the option" yes "$(said "--candidate-assets-dir ${assets} is under")"
check "an evidence directory under the staging directory is refused" "1" \
  "$(preflight_staging="${td}/staging" \
     preflight aarch64 "$jammy" "$r36" "${td}/staging/evidence" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the option" yes "$(said "--evidence-dir ${td}/staging/evidence is under")"

check "a session outside the tensorplate group is refused" "1" \
  "$(preflight_group=not-member \
     preflight aarch64 "$jammy" "$r36" "${td}/evidence-nogroup" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and says how to join it" yes "$(said 'not in the tensorplate group; run sudo usermod -aG tensorplate')"
check "a device that never had the group passes preflight" "0" \
  "$(preflight_group=absent \
     preflight aarch64 "$jammy" "$r36" "${td}/evidence-groupless" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"

check "a run without the confirmation token is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-noconfirm" 0.2.1 v0.2.1-rc.2 "$assets")"
check "  and says what it would have purged" yes "$(said 'purges TensorPlate packages and state')"

check "an x86_64 host is refused" "1" \
  "$(preflight x86_64 "$jammy" "$r36" "${td}/evidence-arch" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the architecture it validates" yes "$(said 'validates aarch64 Jetson rows')"

check "Ubuntu 24.04 is refused on this row" "1" \
  "$(preflight aarch64 "${td}/os-release.noble" "$r36" "${td}/evidence-noble" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names what it expected" yes "$(said 'expected Ubuntu 22.04')"

check "an L4T R35 host is refused" "1" \
  "$(preflight aarch64 "$jammy" "${td}/nv-r35" "${td}/evidence-r35" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and says it is not R36" yes "$(said 'is not L4T R36.x')"

check "a host without L4T release metadata is refused" "1" \
  "$(preflight aarch64 "$jammy" "${td}/absent-nv" "${td}/evidence-nonv" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the file it looked for" yes "$(said 'expected NVIDIA Jetson L4T release metadata')"

# The report's version must be the release it authorizes, never the
# candidate spelling the artifacts were built as.
check "a candidate version spelling is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-rc" 0.2.1-rc.2 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and says a bare version is required" yes "$(said 'must be a bare X.Y.Z release version')"
check "  and so is the tilde spelling" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-tilde" '0.2.1~rc.2' v0.2.1-rc.2 "$assets" "${confirm[@]}")"

check "a candidate tag for another release is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-othertag" 0.2.1 v0.2.2-rc.1 "$assets" "${confirm[@]}")"
check "  and says the tag is not a build of the tested version" yes "$(said 'is not a build of 0.2.1')"

check "a candidate tag that is not a release tag is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-badtag" 0.2.1 0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the accepted spellings" yes "$(said 'must be a release tag')"

# A signed set for a different tag is not the set for this one.
other_assets="${td}/assets-rc1"
make_assets "$other_assets" v0.2.1-rc.1 '0.2.1~rc.1-1'
check "assets whose manifest names another tag are refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-wrongset" 0.2.1 v0.2.1-rc.2 "$other_assets" "${confirm[@]}")"
check "  and name the tag the manifest carries" yes "$(said "names release tag 'v0.2.1-rc.1', not 'v0.2.1-rc.2'")"

two_manifests="${td}/assets-two-manifests"
cp -R "$assets" "$two_manifests"
cp "${two_manifests}/tensorplate-v0.2.1-rc.2-artifacts.json" "${two_manifests}/tensorplate-v0.2.1-rc.1-artifacts.json"
check "assets with two manifests are refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-twomanifests" 0.2.1 v0.2.1-rc.2 "$two_manifests" "${confirm[@]}")"
check "  and say exactly one is expected" yes "$(said 'expected exactly one tensorplate-*-artifacts.json')"

tampered="${td}/assets-tampered"
cp -R "$assets" "$tampered"
printf 'Package: tensorplate-agent\nVersion: 6.6.6-1\n' >"${tampered}/tensorplate-agent_${candidate_version}_arm64.deb"
check "assets that fail their checksums are refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-tampered" 0.2.1 v0.2.1-rc.2 "$tampered" "${confirm[@]}")"
check "  and say the set failed verification" yes "$(said 'failed verification')"

mkdir -p "${td}/evidence-dirty"
: >"${td}/evidence-dirty/lifecycle-report.json"
check "a non-empty evidence directory is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-dirty" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the directory rule" yes "$(said 'must be new or empty')"

no_installer="${td}/assets-no-installer"
cp -R "$assets" "$no_installer"
rm "${no_installer}/install.sh"
check "an assets directory with no installer is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-noinstaller" 0.2.1 v0.2.1-rc.2 "$no_installer" "${confirm[@]}")"
check "  and names the missing installer" yes "$(said "missing ${no_installer}/install.sh")"

no_checksums="${td}/assets-no-checksums"
cp -R "$assets" "$no_checksums"
rm "${no_checksums}/SHA256SUMS"
check "an assets directory with no SHA256SUMS is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-nochecksums" 0.2.1 v0.2.1-rc.2 "$no_checksums" "${confirm[@]}")"
check "  and names the missing checksum file" yes "$(said "missing ${no_checksums}/SHA256SUMS")"

check "an assets directory that does not exist is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-noassets" 0.2.1 v0.2.1-rc.2 "${td}/absent-assets" "${confirm[@]}")"
check "  and says it must be a directory" yes "$(said '--candidate-assets-dir must name a directory')"

check "a run without an evidence directory is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and says it is required" yes "$(said '--evidence-dir is required')"

check "a run without a tested version is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-noversion" "" v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and says it is required" yes "$(said '--tested-version is required')"

check "an unreadable os-release is refused" "1" \
  "$(preflight aarch64 "${td}/absent-os-release" "$r36" "${td}/evidence-noos" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the file" yes "$(said "cannot read ${td}/absent-os-release")"

printf 'ID=debian\nVERSION_ID="22.04"\n' >"${td}/os-release.debian"
check "another distribution at the same version is refused" "1" \
  "$(preflight aarch64 "${td}/os-release.debian" "$r36" "${td}/evidence-debian" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names what it found" yes "$(said 'host reports ID=debian VERSION_ID=22.04')"

# Every command preflight requires is refused by name when it is absent,
# before any host fact is read. The PATH holds only the other required
# commands and dirname, which locating the repository needs.
required_commands=(sudo systemctl journalctl python3 sha256sum dpkg-query dpkg-deb)
for missing in "${required_commands[@]}"; do
  restricted="${td}/path-without-${missing}"
  mkdir -p "$restricted"
  for tool in "${required_commands[@]}" dirname; do
    if [[ "$tool" != "$missing" ]]; then
      ln -s "$(PATH="${stub_bin}:${PATH}" command -v "$tool")" "${restricted}/${tool}"
    fi
  done
  set +e
  env PATH="$restricted" TP_JETSON_ARCH=aarch64 TP_JETSON_OS_RELEASE="$jammy" TP_JETSON_NV_TEGRA_RELEASE="$r36" \
    "$BASH" "$harness" --candidate-tag v0.2.1-rc.2 --candidate-assets-dir "$assets" \
      --evidence-dir "${td}/evidence-without-${missing}" --tested-version 0.2.1 \
      --preflight-only "${confirm[@]}" >"${td}/preflight.out" 2>"${td}/preflight.err"
  missing_status=$?
  set -e
  check "a host without ${missing} is refused" 1 "$missing_status"
  check "  and names it" yes "$(said "missing required command: ${missing}")"
done

# A digest that is not sha256 hex would be refused only when it is
# recorded, after the install stage has purged the device.
mkdir -p "${td}/bad-digest-bin"
cat >"${td}/bad-digest-bin/sha256sum" <<'STUB'
#!/bin/sh
if [ "$#" -eq 1 ] && [ "$1" = SHA256SUMS ]; then
  printf 'sha256:not-hex  SHA256SUMS\n'
  exit 0
fi
exec "${TP_FAKE_REAL_SHA256SUM}" "$@"
STUB
chmod +x "${td}/bad-digest-bin/sha256sum"
check "a checksum file whose digest cannot be computed is refused" "1" \
  "$(preflight_path="${td}/bad-digest-bin:" \
     preflight aarch64 "$jammy" "$r36" "${td}/evidence-baddigest" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and says so before anything is installed" yes "$(said 'could not compute a digest')"

# --- the stages, executed against a stubbed appliance.
appliance="${td}/appliance"
mkdir -p "${appliance}/run" "${appliance}/log" "${appliance}/scratch"

# A serving /health endpoint, which the round-trip checker fetches.
#
# Both of its streams are redirected away from this script's: a
# background process holding the suite's stdout or stderr keeps a pipe
# open after the suite exits, and `run.sh | tail` would hang forever
# waiting for an EOF that never comes.
python3 - "${appliance}" >"${appliance}/health.port" 2>"${appliance}/health.err" <<'PY' &
import http.server, json, os, pathlib, socket, sys

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
        if (restarted and mode == "restart-wrong-health") or mode == "health-wrong-deployment":
            deployment = "a-different-deployment"
        body = json.dumps({"state": state, "active_model_id": deployment}).encode()
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
(directory / "health.pid").write_text(str(os.getpid()))
server.serve_forever()
PY
health_pid=$!
deployment_id="jetson-lifecycle-smoke"
printf '%s\n' "$deployment_id" >"${appliance}/deployment-id"
for _ in $(seq 1 50); do
  [[ -s "${appliance}/health.port" ]] && break
  sleep 0.1
done
serving_port="$(head -n1 "${appliance}/health.port")"
# shellcheck disable=SC2329 # Invoked through the EXIT trap below.
cleanup() {
  # Kill the server before removing its directory, and never let cleanup
  # itself fail the suite.
  kill "$health_pid" 2>/dev/null || true
  wait "$health_pid" 2>/dev/null || true
  rm -rf "$td"
}
trap cleanup EXIT

# The harness waits for a control socket, which only a real agent
# creates. The stub appliance provides one.
python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' \
  "${appliance}/run/agent.sock"

# Signals ignored when a shell starts cannot be trapped by it, and a
# background job of a non-interactive shell starts with SIGINT ignored.
# The harness is started with the default dispositions restored, so the
# signal cases exercise its traps however this suite was launched.
default_signals='import os, signal, sys
for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
    signal.signal(number, signal.SIG_DFL)
os.execv(sys.argv[1], sys.argv[1:])'

run_stages() {
  local mode="$1" evidence="$2" sudo_fail="${3:-}"
  shift 3
  set +e
  : >"${appliance}/sudo.log"
  : >"${appliance}/infer.log"
  : >"${appliance}/deploy.log"
  : >"${appliance}/cxx.log"
  : >"${appliance}/health-requests.log"
  rm -rf "${appliance}/staged-bundle" "${appliance}/scratch"
  mkdir -p "${appliance}/scratch"
  rm -f "${appliance}/restarted" "${appliance}/config-broken" "${appliance}/installed" \
    "${appliance}/restarts" "${appliance}/restore-failed" "${appliance}/backup-path" \
    "${appliance}/pid" "${appliance}/started" "${appliance}/status-reads" "${appliance}/mainpid-reads"
  # A case may have removed the control socket.
  if [[ ! -S "${appliance}/run/agent.sock" ]]; then
    python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' \
      "${appliance}/run/agent.sock"
  fi
  local log_dir="${appliance}/log"
  if [[ "$mode" == log-dir-missing ]]; then
    log_dir="${appliance}/absent-log"
  fi
  printf '{"fixture":"original agent config"}\n' >"${appliance}/agent-config"
  printf '%s\n' "$mode" >"${appliance}/mode"
  env PATH="${stub_bin}:${PATH}" \
    TP_FAKE_GROUP=member \
    TP_FAKE_SOCKET="${appliance}/run/agent.sock" \
    TP_FAKE_START_MARKER="${appliance}/started" \
    TP_FAKE_STATUS_CALLS="${appliance}/status-reads" \
    TP_FAKE_MAINPID_CALLS="${appliance}/mainpid-reads" \
    TP_JETSON_READY_TIMEOUT_SECONDS=1 \
    TMPDIR="${appliance}/scratch" \
    TP_FAKE_MKTEMP="$real_mktemp" \
    TP_FAKE_SUDO_FAIL="$sudo_fail" \
    TP_FAKE_PURGE_MARKER="${evidence}.purged" \
    TP_FAKE_INSTALLED_MARKER="${appliance}/installed" \
    TP_FAKE_CANDIDATE_VERSION="$candidate_version" \
    TP_FAKE_STAGING="${appliance}/staged-bundle" \
    TP_FAKE_CXX_LOG="${appliance}/cxx.log" \
    CUDA_HOME="${td}/cuda" \
    TP_JETSON_ARCH=aarch64 \
    TP_JETSON_OS_RELEASE="$jammy" \
    TP_JETSON_NV_TEGRA_RELEASE="$r36" \
    TP_JETSON_AGENT_SOCKET="${appliance}/run/agent.sock" \
    TP_JETSON_LOG_DIR="$log_dir" \
    TP_JETSON_BUNDLE_STAGING="${appliance}/staged-bundle" \
    TP_JETSON_CRASH_LOOP_POLL_SECONDS=0 \
    TP_FAKE_MODE="$mode" \
    TP_FAKE_SUDO_LOG="${appliance}/sudo.log" \
    TP_FAKE_JOURNALCTL="${stub_bin}/journalctl" \
    TP_FAKE_RESTART_MARKER="${appliance}/restarted" \
    TP_FAKE_INFER_LOG="${appliance}/infer.log" \
    TP_FAKE_DEPLOY_LOG="${appliance}/deploy.log" \
    TP_FAKE_PID_FILE="${appliance}/pid" \
    TP_FAKE_DEPLOYMENT_ID="$deployment_id" \
    TP_FAKE_SERVING_PORT="$serving_port" \
    TP_FAKE_CONFIG_BROKEN="${appliance}/config-broken" \
    TP_FAKE_AGENT_CONFIG="${appliance}/agent-config" \
    TP_FAKE_BACKUP_PATH="${appliance}/backup-path" \
    TP_FAKE_RESTORE_FAILED="${appliance}/restore-failed" \
    TP_FAKE_RESTARTS_FILE="${appliance}/restarts" \
    python3 -c "$default_signals" "$BASH" "$harness" \
      --candidate-tag v0.2.1-rc.2 \
      --candidate-assets-dir "$assets" \
      --evidence-dir "$evidence" \
      --tested-version 0.2.1 \
      --confirm RESET-TENSORPLATE \
      "$@" >"${evidence}.out" 2>"${evidence}.err"
  local status=$?
  set -e
  # A probe that fails without saying why costs a CI round trip to
  # diagnose, and the evidence directory is deleted with the temp dir.
  if [[ ! -f "${evidence}/lifecycle-report.json" && "$mode" != builder-fails ]]; then
    printf '  -- no report written; last lines of the harness:\n' >&2
    tail -n 12 "${evidence}.err" 2>/dev/null | sed 's/^/     /' >&2 || true
  fi
  printf '%s' "$status"
}

stage_status() {
  python3 -c 'import json,sys
report=json.load(open(sys.argv[1]))
print(next((s["status"] for s in report["stages"] if s["stage"]==sys.argv[2]), "absent"))' \
    "$1" "$2" 2>/dev/null || echo "no-report"
}
report_field() {
  python3 -c 'import json,sys
value=json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value=value[key]
print(value)' "$@" 2>/dev/null || echo "absent"
}
sudo_line() {
  grep -nF -- "$1" "${appliance}/sudo.log" | head -n1 | cut -d: -f1
}
# Whether a stage log carries the failure a case exists to provoke. A
# stage that fails for some other reason must not stand in for it.
logged() {
  grep -Fq -- "$2" "$1" && echo yes || echo no
}

ok_evidence="${td}/stages-ok"
check "a stubbed run completes" "0" "$(run_stages ok "$ok_evidence" "")"
for stage in install deploy-smoke status-logs restart crash-loop; do
  check "  ${stage} is recorded as a pass" "pass" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
for stage in upgrade rollback offline; do
  check "  ${stage} is recorded as skipped" "skipped" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
check "  the skipped stages keep the run incomplete" incomplete \
  "$(report_field "${ok_evidence}/lifecycle-report.json" outcome)"
check "  every skip names its follow-up work" yes \
  "$(python3 - "${ok_evidence}/lifecycle-report.json" <<'PY'
import json, sys

stages = {s["stage"]: s for s in json.load(open(sys.argv[1]))["stages"]}
wanted = {"offline": "127.0.0.1/32", "upgrade": "v0.1.5", "rollback": "v0.1.5"}
print("yes" if all(
    stages[name]["status"] == "skipped"
    and "follow-up" in stages[name].get("detail", "")
    and marker in stages[name].get("detail", "")
    for name, marker in wanted.items()
) else "no")
PY
)"
check "  the skipped offline stage never mutates network policy" no \
  "$(grep -Eq 'IPAddress(Deny|Allow)|systemd-run|validation-offline' "${appliance}/sudo.log" && echo yes || echo no)"
check "  the harness names itself in the report" jetson-lifecycle \
  "$(report_field "${ok_evidence}/lifecycle-report.json" harness)"
check "  the row under test is the Jetson row" jetson-orin-nano-8gb-jp62 \
  "$(report_field "${ok_evidence}/lifecycle-report.json" row_id)"
check "  the artifact digest is the candidate's SHA256SUMS" "$candidate_digest" \
  "$(report_field "${ok_evidence}/lifecycle-report.json" subject artifact_digest)"
check "  and the sidecar names what was hashed" "${candidate_digest}  SHA256SUMS" \
  "$(cat "${ok_evidence}/artifact-digest.txt")"
check "  the checksum verification is filed" yes \
  "$(grep -Fq 'install.sh: OK' "${ok_evidence}/checksums.txt" && echo yes || echo no)"
check "  host facts are filed" yes \
  "$(grep -Fq 'l4t: # R36' "${ok_evidence}/host-facts.txt" \
     && grep -Fq 'systemd: systemd 249' "${ok_evidence}/host-facts.txt" \
     && grep -Fq 'power mode: NV Power Mode' "${ok_evidence}/host-facts.txt" && echo yes || echo no)"
check "  the bundle was built on the device" yes \
  "$([[ -s "${appliance}/cxx.log" ]] && echo yes || echo no)"
check "  and its scratch copy was removed on exit" "" \
  "$(find "${appliance}/scratch" -name manifest.json -print -quit)"
check "  install.sh verifies the signature and installs no Python backend" yes \
  "$(grep -Fxq "bash ${assets}/install.sh --local-artifacts ${assets} --yes" "${appliance}/sudo.log" && echo yes || echo no)"
check "  the installed versions are filed per package" 5 \
  "$(grep -c "~rc.2-1 " "${ok_evidence}/packages.txt" || true)"
check "  and never the other architecture's build" no \
  "$(grep -Fq amd64 "${ok_evidence}/packages.txt" && echo yes || echo no)"
check "  the deploy names the staged bundle" "${appliance}/staged-bundle" \
  "$(cat "${appliance}/deploy.log")"
check "  the recorded result is the TensorRT identity round trip" "tensorrt tensorrt_identity" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]));print(r["backend"], r["inference_round_trip"])' "${ok_evidence}/deploy-result.json")"
# The no-compute claim, asserted as a recorded value rather than as a
# string that appears somewhere in the file.
check "  and makes no compute claim" none \
  "$(report_field "${ok_evidence}/deploy-result.json" compute_claim)"
check "  and records the supervision state it actually saw" not_configured \
  "$(report_field "${ok_evidence}/deploy-result.json" supervision_state)"
check "  and the log command outcome is filed" "2" \
  "$(cat "${ok_evidence}/logs-command.exit")"
for phase in initial restarted; do
  check "  ${phase} worker answers the staged identity request" yes \
    "$(grep -Fxq "$phase ${appliance}/staged-bundle/sample_infer.json" "${appliance}/infer.log" && echo yes || echo no)"
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

# A device that cannot build the bundle is refused while its install is
# still intact: no report, and nothing privileged ran.
builder_evidence="${td}/stages-builder-fails"
check "a device that cannot build the bundle is refused" "1" \
  "$(run_stages builder-fails "$builder_evidence" "")"
check "  before anything is purged" "" "$(cat "${appliance}/sudo.log")"
check "  and files no report" no \
  "$([[ -e "${builder_evidence}/lifecycle-report.json" ]] && echo yes || echo no)"
check "  and says what the device lacks" yes \
  "$(grep -Fq 'could not build the TensorRT identity bundle' "${builder_evidence}.err" && echo yes || echo no)"

prebuilt_evidence="${td}/stages-prebuilt"
check "a run with a pre-built bundle completes" "0" \
  "$(run_stages ok "$prebuilt_evidence" "" --bundle-dir "${td}/bundle-good")"
check "  and compiles nothing on the device" "" "$(cat "${appliance}/cxx.log")"

# --- install.
#
# Found on a real cloud host: the purge must name only packages dpkg
# knows, and nothing may remain before the state directories go.
rerun_evidence="${td}/stages-rerun"
check "a re-run over an existing install completes" "0" \
  "$(run_stages installed-runtime "$rerun_evidence" "")"
purge_line="$(grep -F 'apt-get purge' "${appliance}/sudo.log" || true)"
check "  and purges the runtime packages that were installed" yes \
  "$(printf '%s\n' "$purge_line" | grep -qF 'tensorplate-agent' && echo yes || echo no)"
check "  and leaves the apt channel's bootstrap package installed" no \
  "$(printf '%s\n' "$purge_line" | tr ' ' '\n' | grep -qx 'tensorplate-apt-source' && echo yes || echo no)"
check "  and never names a package dpkg reports as not installed" no \
  "$(printf '%s\n' "$purge_line" | tr ' ' '\n' | grep -qx 'tensorplate-backend-python-pytorch' && echo yes || echo no)"
check "  and never names the metapackage install.sh does not install" no \
  "$(printf '%s\n' "$purge_line" | tr ' ' '\n' | grep -qx 'tensorplate' && echo yes || echo no)"
check "  purge, clear, install run in that order" yes \
  "$(purge="$(sudo_line 'apt-get purge')"; clear="$(sudo_line 'rm -rf /etc/tensorplate')"
     installer="$(sudo_line '/install.sh --local-artifacts')"
     [[ -n "$purge" && -n "$clear" && -n "$installer" && "$purge" -lt "$clear" && "$clear" -lt "$installer" ]] \
       && echo yes || echo no)"

leftover_evidence="${td}/stages-purge-leaves"
check "packages surviving the purge fail install before state is cleared" fail \
  "$(run_stages purge-leaves-packages "$leftover_evidence" "" >/dev/null; \
     stage_status "${leftover_evidence}/lifecycle-report.json" install)"
check "  and the state directories were never removed" no \
  "$(grep -qF 'rm -rf /etc/tensorplate' "${appliance}/sudo.log" && echo yes || echo no)"
check "  and the survivor is named" yes \
  "$(grep -Fq 'remain after the purge: tensorplate-common config-files' "${leftover_evidence}/install.log" && echo yes || echo no)"

evidence="${td}/stages-purge-fails"
check "a failing purge is recorded as a failed install" fail \
  "$(run_stages installed-runtime "$evidence" "apt-get purge" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" install)"
check "  and the failed step is the one named" yes "$(logged "${evidence}/install.log" 'step failed (exit 9): purge')"
check "  and the stage stopped there rather than at the leftover check" no \
  "$(logged "${evidence}/install.log" 'remain after the purge')"

install_cases=()
for id in platform_registry agent_reachable agent_socket serving_binary_installed path_layout config_files; do
  install_cases+=("doctor-warns-${id}|${id} is warning")
done
for mode_case in "doctor-failing|doctor reports 1 failing finding(s): serving_binary_installed" \
                 "doctor-exits-nonzero|step failed (exit 1): doctor" \
                 "wrong-row|platform_row is warning" \
                 "other-row|platform_row did not name jetson-orin-nano-8gb-jp62" \
                 "profile-other-row|jetson-orin-nano-8gb-jp62 is not among the host's candidate rows" \
                 "${install_cases[@]}" \
                 "stale-version|tensorplate-agent: expected 'installed ${candidate_version}', dpkg reports 'installed 0.2.1~rc.1-1'" \
                 "package-missing|tensorplate-serving: expected 'installed ${candidate_version}', dpkg reports 'not installed'" \
                 "deb-unreadable|tensorplate-cli: could not read the Version of tensorplate-cli_" \
                 "install-agent-inactive|step failed (exit 1): services ready" \
                 "install-observability-inactive|step failed (exit 1): services ready" \
                 "install-socket-missing|step failed (exit 1): services ready"; do
  mode="${mode_case%%|*}"
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_stages "$mode" "$evidence" "" --bundle-dir "${td}/bundle-good")"
  check "  and install is recorded as a failure, not a pass" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" install)"
  check "  and the run does not certify itself" fail \
    "$(report_field "${evidence}/lifecycle-report.json" outcome)"
  check "  and no digest is attested" absent \
    "$(report_field "${evidence}/lifecycle-report.json" subject artifact_digest)"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/install.log" "${mode_case#*|}")"
done
# The real CLI exits 10 after writing a report with a failing finding.
# The report is still read, so the log names the finding and the exit.
check "a failing doctor names its exit status as well as its finding" yes \
  "$(logged "${td}/stages-doctor-failing/install.log" 'step failed (exit 10): doctor')"
for mode in install-agent-inactive install-observability-inactive install-socket-missing; do
  check "${mode} stops install before doctor" no \
    "$([[ -e "${td}/stages-${mode}/doctor.json" ]] && echo yes || echo no)"
done

duplicate_assets="${td}/assets-duplicate-cli"
make_assets "$duplicate_assets" v0.2.1-rc.2 "$candidate_version" duplicate-cli
evidence="${td}/stages-duplicate-cli"
check "a manifest naming one package twice fails install" fail \
  "$(run_stages ok "$evidence" "" --bundle-dir "${td}/bundle-good" \
       --candidate-assets-dir "$duplicate_assets" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" install)"
check "  for the reason the case provokes" yes \
  "$(logged "${evidence}/install.log" 'tensorplate-cli: the manifest selects 2 .deb files, not one')"

# A privileged step that fails must fail its stage. Without this, an
# unguarded `sudo ...` inside a stage body is invisible: errexit is
# suspended there, so the stage runs on and returns 0.
# Each case also names the step that failed, so a later check that
# happens to fail for another reason cannot stand in for the missing one.
for injected_case in "install.sh=install.sh" "rm -rf /etc/tensorplate=clear installed state" \
                     "systemctl enable --now tensorplate-agent=enable tensorplate-agent" \
                     "systemctl enable --now tensorplate-observability=enable tensorplate-observability"; do
  injected="${injected_case%%=*}"
  step_name="${injected_case#*=}"
  evidence="${td}/stages-sudo-fails-${injected//[^a-z]/-}"
  check "a failing '${injected}' is recorded as a failed install" fail \
    "$(run_stages ok "$evidence" "$injected" --bundle-dir "${td}/bundle-good" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" install)"
  check "  and the failed step is the one named" yes \
    "$(grep -Fq "step failed (exit 9): ${step_name}" "${evidence}/install.log" && echo yes || echo no)"
  # The step's message is printed before the stage decides whether to
  # stop, so only what ran afterwards shows the stage actually stopped.
  check "  and nothing privileged ran after it" yes \
    "$(tail -n 1 "${appliance}/sudo.log" | grep -Fq -- "$injected" && echo yes || echo no)"
done

# --- deploy-smoke.
for variant_case in "wrong-backend|must declare backend_hint=tensorrt" \
                    "wrong-kind|must be a tensorrt_engine" \
                    "bad-digest|digest does not match its manifest" \
                    "two-models|must declare exactly one model artifact" \
                    "escapes-root|escapes the bundle root" \
                    "bad-sample|Expecting value"; do
  variant="${variant_case%%|*}"
  evidence="${td}/stages-bundle-${variant}"
  check "a ${variant} bundle fails deploy-smoke" fail \
    "$(run_stages ok "$evidence" "" --bundle-dir "${td}/bundle-${variant}" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" deploy-smoke)"
  check "  and is never deployed" "" "$(cat "${appliance}/deploy.log")"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/deploy-smoke.log" "${variant_case#*|}")"
done
for mode_case in "infer-garbled|1|value mismatch at 1" \
                 "infer-fails|11|step failed (exit 11): infer" \
                 "deploy-fails|3|step failed (exit 3): deploy" \
                 "deploy-not-active|1|checks failed: deployment_phase" \
                 "deploy-other-id|1|checks failed: deployment_id" \
                 "status-fails|4|step failed (exit 4): status" \
                 "status-degraded|1|checks failed: status_severity" \
                 "agent-not-ready|1|checks failed: agent_state" \
                 "status-other-deployment|1|checks failed: active_deployment" \
                 "status-wrong-backend|1|checks failed: active_backend" \
                 "url-not-http|1|checks failed: active_serving_url" \
                 "url-not-loopback|1|checks failed: active_serving_url" \
                 "url-wrong-path|1|checks failed: active_serving_url" \
                 "health-wrong-deployment|1|checks failed: serving_health_deployment" \
                 "supervision-failed|1|checks failed: supervision_healthy_when_configured" \
                 "supervision-crash-loop|1|checks failed: supervision_healthy_when_configured"; do
  mode="${mode_case%%|*}"
  rest="${mode_case#*|}"
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" "${rest%%|*}" \
    "$(run_stages "$mode" "$evidence" "" --bundle-dir "${td}/bundle-good")"
  check "  install passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" install)"
  check "  and deploy-smoke is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" deploy-smoke)"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/deploy-smoke.log" "${rest#*|}")"
  if [[ "$mode" == deploy-fails ]]; then
    check "  and no inference is issued against it" "" "$(cat "${appliance}/infer.log")"
  fi
done
# Each command's failure stops the round trip where it happened, rather
# than leaving a later check to fail on what the command never wrote.
check "a failed inference is never handed to the identity verifier" no \
  "$(logged "${td}/stages-infer-fails/deploy-smoke.log" 'the engine returned its input unchanged')"
check "a failed status is never parsed" no \
  "$(logged "${td}/stages-status-fails/deploy-smoke.log" 'Traceback')"

evidence="${td}/stages-supervision-ready"
check "a healthy supervised worker passes" 0 \
  "$(run_stages supervision-ready "$evidence" "" --bundle-dir "${td}/bundle-good")"
check "  and records the supervision state it saw" ready \
  "$(report_field "${evidence}/deploy-result.json" supervision_state)"

for injected_case in "rm -rf ${appliance}/staged-bundle=stage the bundle" \
                     "mkdir -p=create the staging parent" \
                     "cp -R=copy the bundle" "chmod -R a+rX=make the bundle readable"; do
  injected="${injected_case%%=*}"
  step_name="${injected_case#*=}"
  evidence="${td}/stages-sudo-fails-${step_name// /-}"
  check "a failing '${step_name}' while staging is recorded as a failed deploy-smoke" fail \
    "$(run_stages ok "$evidence" "$injected" --bundle-dir "${td}/bundle-good" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" deploy-smoke)"
  check "  and the failed step is the one named" yes \
    "$(grep -Fq "step failed (exit 9): ${step_name}" "${evidence}/deploy-smoke.log" && echo yes || echo no)"
  # The step's message is printed before the stage decides whether to
  # stop, so only what ran afterwards shows the stage actually stopped.
  check "  and nothing privileged ran after it" yes \
    "$(tail -n 1 "${appliance}/sudo.log" | grep -Fq -- "$injected" && echo yes || echo no)"
done

# --- status-logs.
for mode_case in "journal-command-fails|9|step failed (exit 9): capture the tensorplate-agent journal" \
                 "journal-empty-agent|1|tensorplate-agent.service: no journal records from the current service invocation" \
                 "journal-no-entries|1|tensorplate-agent.service: journal output is not a JSON record" \
                 "journal-not-object|1|tensorplate-agent.service: journal record is not from the current service invocation" \
                 "journal-empty-observability|1|tensorplate-observability.service: no journal records from the current service invocation" \
                 "journal-stale-invocation|1|tensorplate-agent.service: journal record is not from the current service invocation" \
                 "journal-wrong-unit|1|tensorplate-agent.service: journal record is not from the current service invocation" \
                 "journal-empty-message|1|tensorplate-agent.service: journal record has no text message" \
                 "invocation-empty|1|no current invocation ID for tensorplate-agent" \
                 "statuslogs-status-fails|4|step failed (exit 4): status" \
                 "statuslogs-wrong-command|1|AssertionError: {'command': 'doctor'" \
                 "statuslogs-degraded|1|AssertionError: degraded" \
                 "statuslogs-lost-deployment|1|status no longer reports jetson-lifecycle-smoke as active" \
                 "log-dir-missing|1|log directory missing at ${appliance}/absent-log"; do
  mode="${mode_case%%|*}"
  rest="${mode_case#*|}"
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" "${rest%%|*}" \
    "$(run_stages "$mode" "$evidence" "" --bundle-dir "${td}/bundle-good")"
  check "  deployment passed before the status-logs failure" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" deploy-smoke)"
  check "  and status-logs is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" status-logs)"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/status-logs.log" "${rest#*|}")"
done
check "a failed status read stops status-logs before anything else is recorded" no \
  "$([[ -e "${td}/stages-statuslogs-status-fails/logs-command.exit" ]] && echo yes || echo no)"

# --- restart.
for mode_case in "restart-no-worker|checks failed: active_serving_url" \
                 "restart-unhealthy-health|checks failed: serving_health_state" \
                 "restart-wrong-health|checks failed: serving_health_deployment" \
                 "restart-infer-garbled|value mismatch at 1" \
                 "restart-agent-pid-unchanged|agent MainPID did not change" \
                 "restart-observability-pid-unchanged|observability MainPID did not change" \
                 "restart-agent-inactive|step failed (exit 1): services ready again" \
                 "restart-socket-missing|step failed (exit 1): services ready again"; do
  mode="${mode_case%%|*}"
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_stages "$mode" "$evidence" "" --bundle-dir "${td}/bundle-good")"
  check "  status-logs passed before the restart regression" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" status-logs)"
  check "  the restart failure is recorded against restart" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" restart)"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/restart.log" "${mode_case#*|}")"
done
# systemctl failing to report a pid, before and after the restart. A
# value nobody read cannot show the process changed.
for read in 1 2 3 4; do
  evidence="${td}/stages-mainpid-show-fails-${read}"
  check "a MainPID read ${read} that fails is recorded as a failed restart" fail \
    "$(run_stages "mainpid-show-fails-${read}" "$evidence" "" --bundle-dir "${td}/bundle-good" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" restart)"
  check "  and the recovered worker is never credited" no \
    "$(grep -q '^restarted ' "${appliance}/infer.log" && echo yes || echo no)"
done
evidence="${td}/stages-restart-fails"
check "a restart that fails is recorded as a failed restart" fail \
  "$(run_stages ok "$evidence" "systemctl restart" --bundle-dir "${td}/bundle-good" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" restart)"
check "  for the reason the case provokes" yes "$(logged "${evidence}/restart.log" 'step failed (exit 9): restart both units')"

# --- crash-loop recovery.
#
# This stage breaks the appliance on purpose, so check restoration as
# well as the stage verdict, including interruption and restore failure.
restore_line='/agent.json /etc/tensorplate/agent.json'
config_restored() {
  [[ ! -e "${appliance}/config-broken" && \
     "$(cat "${appliance}/agent-config")" == '{"fixture":"original agent config"}' ]] && echo yes || echo no
}
backup_retained() {
  local backup
  backup="$(cat "${appliance}/backup-path" 2>/dev/null)" || { echo no; return 0; }
  [[ -f "$backup" && "$(cat "$backup")" == '{"fixture":"original agent config"}' ]] && echo yes || echo no
}
run_stages ok "${td}/stages-ok-again" "" --bundle-dir "${td}/bundle-good" >/dev/null
check "the ok run breaks the agent config, then restores it" yes \
  "$(broke="$(sudo_line 'invalid json')"; restored="$(sudo_line "$restore_line")"
     [[ -n "$broke" && -n "$restored" && "$broke" -lt "$restored" ]] && echo yes || echo no)"
check "  and files what systemd did with the loop" "failed 4 start-limit-hit" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]));print(r["active_state"],r["restarts"],r["result"])' \
    "${td}/stages-ok-again/crash-loop-result.json")"
check "  and the recovered worker answered" tensorrt_identity \
  "$(report_field "${td}/stages-ok-again/crash-loop-recovery.json" inference_round_trip)"
check "  and the original config bytes were restored" yes "$(config_restored)"
check "  and the backup was removed once the agent recovered" no "$(backup_retained)"
for mode_case in "crash-loop-keeps-restarting|the agent never settled" \
                 "crash-loop-not-retried|checks failed: restarted_before_giving_up" \
                 "crash-loop-other-error|checks failed: agent_rejected_the_config" \
                 "crash-loop-other-unit|checks failed: agent_rejected_the_config" \
                 "crash-loop-one-config-error|checks failed: agent_rejected_the_config" \
                 "crash-loop-never-fails|the agent never settled" \
                 "crash-loop-activating|the agent never settled" \
                 "crash-loop-stopped|checks failed: unit_failed" \
                 "crashloop-recovery-garbled|value mismatch at 1"; do
  mode="${mode_case%%|*}"
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_stages "$mode" "$evidence" "" --bundle-dir "${td}/bundle-good")"
  check "  restart passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" restart)"
  check "  and crash-loop is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and the agent config was restored anyway" yes "$(config_restored)"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/crash-loop.log" "${mode_case#*|}")"
done

# A property systemctl cannot report is not a value to settle or judge
# on: an empty state or restart count would otherwise read as settled.
for mode in state-show-fails restarts-show-fails result-show-fails; do
  evidence="${td}/stages-${mode}"
  check "${mode} fails crash-loop" fail \
    "$(run_stages "$mode" "$evidence" "" --bundle-dir "${td}/bundle-good" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  before any journal is taken as evidence" no \
    "$([[ -e "${evidence}/crash-loop-journal.txt" ]] && echo yes || echo no)"
  check "  and the config is still restored" yes "$(config_restored)"
done

evidence="${td}/stages-crash-loop-journal-fails"
check "a crash-loop journal capture that fails is recorded as a failed crash-loop" fail \
  "$(run_stages ok "$evidence" "journalctl -u tensorplate-agent --since" --bundle-dir "${td}/bundle-good" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  for the reason the case provokes" yes \
  "$(logged "${evidence}/crash-loop.log" 'step failed (exit 9): capture the crash-loop journal')"
check "  and the empty capture is never judged" no "$(logged "${evidence}/crash-loop.log" 'crash-loop checks failed')"
check "  and the config is still restored" yes "$(config_restored)"

# Without a backup there is nothing to restore from, so the config must
# never be broken.
evidence="${td}/stages-backup-fails"
check "a config backup that fails is recorded as a failed crash-loop" fail \
  "$(run_stages ok "$evidence" "cp -p /etc/tensorplate/agent.json" --bundle-dir "${td}/bundle-good" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  for the reason the case provokes" yes \
  "$(logged "${evidence}/crash-loop.log" 'step failed (exit 9): back up the agent config')"
check "  and the config is never corrupted" no \
  "$(grep -Fq 'invalid json' "${appliance}/sudo.log" && echo yes || echo no)"
check "  and the config bytes are intact" yes "$(config_restored)"

evidence="${td}/stages-corrupt-fails"
check "a config corruption that fails is recorded as a failed crash-loop" fail \
  "$(run_stages ok "$evidence" "invalid json" --bundle-dir "${td}/bundle-good" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  for the reason the case provokes" yes \
  "$(logged "${evidence}/crash-loop.log" 'step failed (exit 9): corrupt the agent config')"
check "  and the loop is never observed" no "$(logged "${evidence}/crash-loop.log" 'the agent never settled')"
check "  and the config is still restored" yes "$(config_restored)"

# Restoring the bytes is not recovery: a failed state that is not
# cleared, an agent that is not started, or one that never comes back
# fails the stage, and the backup is kept for the operator.
for recovery_case in "ok|9|systemctl reset-failed|clear the agent's failed state" \
                     "ok|9|systemctl start tensorplate-agent|start the agent" \
                     "crash-loop-agent-not-ready|1||services ready again"; do
  mode="${recovery_case%%|*}"
  rest="${recovery_case#*|}"
  expected_status="${rest%%|*}"
  rest="${rest#*|}"
  injected="${rest%%|*}"
  step_name="${rest#*|}"
  evidence="${td}/stages-recovery-${step_name//[^a-z]/-}"
  check "an agent whose '${step_name}' fails refuses the run" "$expected_status" \
    "$(run_stages "$mode" "$evidence" "$injected" --bundle-dir "${td}/bundle-good")"
  check "  and crash-loop is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  for the reason the case provokes" yes \
    "$(logged "${evidence}/crash-loop.log" "step failed (exit ${expected_status}): ${step_name}")"
  check "  and the config bytes were restored" yes "$(config_restored)"
  check "  and the backup is kept for manual recovery" yes "$(backup_retained)"
done

# "twice" sends a second TERM while the first one's cleanup is restoring
# the config, which must not cut the restoration short.
for signal_case in int:130 term:143 hup:129 twice:143; do
  signal="${signal_case%:*}"
  expected_status="${signal_case#*:}"
  evidence="${td}/stages-crash-loop-signal-${signal}"
  check "${signal} during config corruption preserves the signal exit status" "$expected_status" \
    "$(run_stages "crash-loop-signal-${signal}" "$evidence" "")"
  check "  the interrupted crash-loop stage is recorded as failed" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  the signal cleanup restores the original config bytes" yes "$(config_restored)"
  check "  and starts the restored agent" yes \
    "$(grep -Fxq 'systemctl start tensorplate-agent' "${appliance}/sudo.log" && echo yes || echo no)"
  check "  and removes the scratch bundle" "" \
    "$(find "${appliance}/scratch" -name manifest.json -print -quit)"
done

evidence="${td}/stages-crash-loop-restore-fails-once"
check "a failed config restore is retried on exit without hiding its failure" 9 \
  "$(run_stages crash-loop-restore-fails-once "$evidence" "" --bundle-dir "${td}/bundle-good")"
check "  the failed restore still fails the crash-loop stage" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  the exit retry restores the original config bytes" yes "$(config_restored)"

evidence="${td}/stages-crash-loop-restore-always-fails"
check "a persistent config restore failure refuses the run" 9 \
  "$(run_stages crash-loop-restore-always-fails "$evidence" "" --bundle-dir "${td}/bundle-good")"
check "  and preserves the backup for manual recovery" yes "$(backup_retained)"
check "  the report does not certify the failed recovery" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "verify_jetson_lifecycle: ok" || echo "${failures} check(s) failed")"
exit "$failures"
