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
assert set(run) | set(skipped) == set(canonical), (
    f"harness covers {sorted(set(run) | set(skipped))}, the schema names {sorted(canonical)}"
)
assert len(set(run)) == len(run), f"a stage is run twice: {run}"
assert len(set(skipped)) == len(skipped), f"a stage is skipped twice: {skipped}"
assert set(run) == {"install", "deploy-smoke", "status-logs", "restart",
                    "crash-loop", "upgrade", "rollback"}, sorted(run)
# upgrade and rollback are run or skipped depending on whether a baseline
# set was supplied, so each appears once in each list. offline is the one
# stage this harness cannot run at all.
assert set(skipped) == {"upgrade", "rollback", "offline"}, sorted(skipped)
assert set(run) & set(skipped) == {"upgrade", "rollback"}, sorted(set(run) & set(skipped))

# Every skip states a reason: an unexplained skip is indistinguishable
# from a stage nobody thought about. offline names the work that closes
# it; the conditional pair names the options that run them instead.
reasons = {}
for stage in skipped:
    match = re.search(
        r"lifecycle_skip\s+" + re.escape(stage) + r"\s*\\\n\s*\"([^\"]+)\"", body
    )
    assert match, f"{stage} is skipped without a quoted reason"
    reasons[stage] = match.group(1)
    assert len(match.group(1)) > 40, f"{stage}'s skip reason is too thin: {match.group(1)}"
assert "follow-up" in reasons["offline"], "offline's skip reason names no follow-up work"
for stage in ("upgrade", "rollback"):
    for option in ("--baseline-tag", "--baseline-assets-dir"):
        assert option in reasons[stage], \
            f"{stage}'s skip reason does not name {option}, which runs it"

# Order. offline is about the candidate install the stages above
# exercise, and upgrade replaces that install with the baseline, so
# offline has to stay ahead of it. Rollback returns from what upgrade
# left, so it follows.
positions = {
    "offline": re.search(r"^\s*lifecycle_skip\s+offline\s", body, re.M),
    "upgrade": re.search(r"^\s*lifecycle_stage\s+upgrade\s", body, re.M),
    "rollback": re.search(r"^\s*lifecycle_stage\s+rollback\s", body, re.M),
}
assert all(positions.values()), positions
assert positions["offline"].start() < positions["upgrade"].start() < positions["rollback"].start(), \
    "the stages must run in the order offline, upgrade, rollback"

# Neither set is ever installed without its signature verified, so the
# installer's opt-out must not appear in anything the harness runs. The
# comments that say so are not what would install one.
code = [line for line in body.splitlines() if not line.lstrip().startswith("#")]
offenders = [line.strip() for line in code if "--allow-unsigned" in line]
assert not offenders, \
    f"the harness can pass --allow-unsigned; both sets are always signature-verified: {offenders}"

# The digest must be recorded after the install stage passed, so it
# attests an install that happened. Matched as a call rather than as a
# mention, so a comment naming the function cannot satisfy this.
install_call = re.search(r"^\s*lifecycle_stage\s+install\s", body, re.M)
digest_call = re.search(r"^\s*lifecycle_artifact_digest\s", body, re.M)
assert install_call, "the harness does not run an install stage"
assert digest_call, "the harness never records an artifact digest"
assert digest_call.start() > install_call.start(), \
    "the artifact digest is recorded before the install stage"
# One digest, and it is the candidate's. The baseline's is filed beside
# the stage logs instead, because the report attests one artifact set.
assert len(re.findall(r"^\s*lifecycle_artifact_digest\s", body, re.M)) == 1, \
    "the harness records more than one artifact digest"
assert re.search(r"^\s*lifecycle_artifact_digest\s+\"\$ARTIFACT_DIGEST\"", body, re.M), \
    "the recorded artifact digest is not the candidate's"

# lifecycle_begin clears the digest sidecar so a retry cannot inherit a
# previous attempt's digest; a harness that wrote it first would delete
# its own.
begin_call = re.search(r"^\s*lifecycle_begin\s", body, re.M)
assert begin_call, "the harness never calls lifecycle_begin"
assert "artifact-digest.txt" not in body[: begin_call.start()], \
    "the harness writes the digest sidecar before lifecycle_begin, which clears it"
print("stage coverage: 7 run with a baseline, offline always skipped, digest recorded after install")
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
# What tensorplate-release.sh records for a set built from a tag and
# published as a GitHub release, which is what a baseline must be.
release = {"tag": tag, "version": version,
           "unreleased": False, "provenance": "github-release"}
if variant == "snapshot":
    release["unreleased"] = True
if variant == "local-provenance":
    release["provenance"] = "local-source-snapshot"
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
    control_version = deb_version
    # A .deb whose control Version is not the one the manifest and the
    # file name carry. The release driver parses the manifest's version
    # out of the file name and never reads the package, so only a harness
    # that reads the control field sees this -- and the control field is
    # what apt orders on.
    if variant == "deb-version-newer" and package == "tensorplate-agent" and arch == "arm64":
        control_version = "9.9.9-1"
    control = f"Package: {package}\nVersion: {control_version}\nArchitecture: {arch}\n"
    # A file dpkg-deb cannot read as a package at all, under the name and
    # manifest entry of a good one.
    if variant == "deb-corrupt" and package == "tensorplate-serving" and arch == "arm64":
        control = "not a Debian archive\n"
    (directory / name).write_text(control, encoding="utf-8")
    entry = {"file": name, "package": package, "architecture": arch,
             # The release manifest records each package's Debian version
             # beside its file; the upgrade path is compared on the
             # control Version inside the .deb, not on this.
             "version": deb_version}
    # A manifest that names the file but not the version it carries. The
    # comparison alone would admit it, so only the harness's own shape
    # check refuses it.
    if variant == "no-package-version" and package == "tensorplate-agent" and arch == "arm64":
        del entry["version"]
    artifacts.append(entry)
if variant == "duplicate-cli":
    # A manifest naming the CLI twice for this architecture, which leaves
    # no single package to compare the installed version against.
    artifacts.append({"file": f"tensorplate-cli_{version}_arm64.deb",
                      "package": "tensorplate-cli", "architecture": "arm64",
                      "version": version})
manifest = {"release": release, "artifacts": artifacts}
(directory / f"tensorplate-{tag}-artifacts.json").write_text(
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
# The candidate set every run installs from, and the one the stubs are
# told about. Every case but candidate-set-changed uses the shared set;
# that one points both at a throwaway copy it is free to change under
# the harness.
candidate_assets="$assets"

# The published predecessor set the upgrade moves from and the rollback
# returns to: the last published arm64 runtime release.
baseline_version='0.1.5-1'
baseline="${td}/baseline"
make_assets "$baseline" v0.1.5 "$baseline_version"
baseline_digest="$(sha256_of "${baseline}/SHA256SUMS")"
# As with the candidate: every case but baseline-set-changed uses the
# shared set.
baseline_assets="$baseline"

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
# fixture and its backup, the package database, and markers the other
# stubs read. The one thing it runs as the harness wrote it is the
# environment scrub the harness puts in front of an installer, and that
# runs in front of the fixture installer rather than a release's.
cat >"${stub_bin}/sudo" <<'STUB'
#!/bin/sh
printf '%s\n' "$*" >>"${TP_FAKE_SUDO_LOG}"
# Recorded before any failure is injected: a removal that was attempted,
# whatever became of it.
case "$*" in
  *"apt-get remove"*) : >"${TP_FAKE_REMOVE_ATTEMPTED}" ;;
esac
# Injectable failure, so a privileged step that the harness forgot to
# check can be caught here rather than on a device.
if [ -n "${TP_FAKE_SUDO_FAIL:-}" ]; then
  case "$*" in
    *"${TP_FAKE_SUDO_FAIL}"*) exit 9 ;;
  esac
fi
# A sudo policy or PAM environment that hands every privileged command a
# verification opt-out the harness's own environment did not carry.
if [ "${TP_FAKE_MODE:-ok}" = sudo-passes-allow-unsigned ]; then
  TP_INSTALL_ALLOW_UNSIGNED=1
  export TP_INSTALL_ALLOW_UNSIGNED
fi
# The operator's conffile edit, applied to the fixture copy and nowhere
# else.
if [ "$1" = bash ] && [ "$2" = -c ] && [ "$3" = 'printf "\n" >>"$1"' ]; then
  [ "$5" = "${TP_FAKE_OPERATOR_CONFIG}" ] || exit 9
  printf '\n' >>"$5"
  exit
fi
# The package database keeps one record per package, holding its dpkg
# status. A purged package has no record.
set_status() {
  printf '%s\n' "$2" >"${TP_FAKE_PKG_DB}/$1"
}
case "$*" in
  # The upgrade's and the rollback's installs: the harness's own scrub,
  # run as written, in front of the fixture installer.
  "bash -c "*" tensorplate-install "*"/install.sh --local-artifacts "*)
    [ "$4" = tensorplate-install ] && [ "$5" = "$7/install.sh" ] &&
      [ "$6" = --local-artifacts ] && [ "$8" = --yes ] && [ "$#" -eq 8 ] || exit 9
    exec bash -c "$3" "$4" "${TP_FAKE_INSTALLER}" "$6" "$7" "$8"
    ;;
  # The install stage's own call.
  "bash "*"/install.sh --local-artifacts "*)
    [ "$2" = "$4/install.sh" ] && [ "$3" = --local-artifacts ] && [ "$#" -eq 5 ] || exit 9
    exec bash "${TP_FAKE_INSTALLER}" "$3" "$4" "$5"
    ;;
  *"apt-get purge"*)
    : >"${TP_FAKE_PURGE_MARKER}"
    # DEBIAN_FRONTEND=noninteractive apt-get purge -y <packages>
    shift 4
    for pkg in "$@"; do
      rm -f "${TP_FAKE_PKG_DB}/${pkg}"
    done
    rm -f "${TP_FAKE_INSTALLED_VERSION}" "${TP_FAKE_PHASE}"
    ;;
  # `remove`, not `purge`, and only what it names: a package that ships a
  # conffile keeps it and is left config-files, and tensorplate-common,
  # which ships none, is dropped to not-installed. The installed version
  # is kept, so the installer can refuse a downgrade over what the
  # removal left. A mode can emulate a removal that took a package's
  # conffiles with it, left a package behind, or took the apt channel's
  # bootstrap package too.
  *"apt-get remove"*)
    shift 4
    printf 'removed\n' >"${TP_FAKE_PHASE}"
    for pkg in "$@"; do
      state=config-files
      [ "$pkg" = tensorplate-common ] && state=not-installed
      case "${TP_FAKE_MODE:-ok}:$pkg" in
        rollback-leaves-package:tensorplate-cli|rollback-leaves-common:tensorplate-common) state=installed ;;
        rollback-common-half-configured:tensorplate-common) state=half-configured ;;
        rollback-purges-observability:tensorplate-observability) state=not-installed ;;
        rollback-purges-conffiles:tensorplate-agent) state="" ;;
      esac
      if [ -n "$state" ]; then
        set_status "$pkg" "$state"
      else
        rm -f "${TP_FAKE_PKG_DB}/${pkg}"
      fi
    done
    if [ "${TP_FAKE_MODE:-ok}" = rollback-removes-apt-source ] &&
       [ -f "${TP_FAKE_PKG_DB}/tensorplate-apt-source" ]; then
      set_status tensorplate-apt-source config-files
    fi
    ;;
  "rm -rf /etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate")
    rm -rf "${TP_FAKE_VARLIB}"
    rm -f "${TP_FAKE_OPERATOR_CONFIG}" "${TP_FAKE_ACTIVE_ID}"
    ;;
  "test ! -e /var/lib/tensorplate/state.bak")
    [ ! -e "${TP_FAKE_VARLIB}/state.bak" ]
    exit
    ;;
  # The privileged digest of a file under /var/lib/tensorplate, taken
  # from the fixture's copy of it and printed in sha256sum's own format,
  # so the harness reads a fixture exactly as it reads a device. A file
  # that is not there is reported the way sha256sum reports it, by name.
  "sha256sum /var/lib/tensorplate/"*)
    [ "$#" -eq 2 ] || exit 9
    target="${TP_FAKE_VARLIB}/${2#/var/lib/tensorplate/}"
    if [ ! -f "$target" ]; then
      printf 'sha256sum: %s: No such file or directory\n' "$2" >&2
      exit 1
    fi
    digest="$(python3 -c 'import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$target")" || exit 9
    # A sha256sum that prints BSD's format instead of GNU's: every file
    # then reads as the same leading field, so a digest taken from it
    # would make any two files compare equal.
    if [ "${TP_FAKE_MODE:-ok}" = rollback-digest-not-hex ]; then
      printf 'SHA256 (%s) = %s\n' "$2" "$digest"
      exit 0
    fi
    printf '%s  %s\n' "$digest" "$2"
    exit 0
    ;;
  "mv -T /var/lib/tensorplate/state /var/lib/tensorplate/state.bak")
    # GNU mv -T: never moves into the target, replaces only an empty one.
    [ -d "${TP_FAKE_VARLIB}/state" ] || exit 1
    if [ -e "${TP_FAKE_VARLIB}/state.bak" ]; then
      rmdir "${TP_FAKE_VARLIB}/state.bak" 2>/dev/null || {
        echo "mv: cannot overwrite '/var/lib/tensorplate/state.bak': Directory not empty" >&2
        exit 1
      }
    fi
    case "${TP_FAKE_MODE:-ok}" in
      # State the older agent can still read, and state that is gone
      # rather than set aside.
      rollback-keeps-state) cp -R "${TP_FAKE_VARLIB}/state" "${TP_FAKE_VARLIB}/state.bak" ;;
      rollback-state-not-preserved) rm -rf "${TP_FAKE_VARLIB}/state" ;;
      *)
        mv "${TP_FAKE_VARLIB}/state" "${TP_FAKE_VARLIB}/state.bak"
        rm -f "${TP_FAKE_ACTIVE_ID}"
        ;;
    esac
    exit
    ;;
  *"systemctl restart"*)
    : >"${TP_FAKE_RESTART_MARKER}"
    if [ "${TP_FAKE_MODE:-ok}" = restart-socket-missing ]; then
      rm -f "${TP_FAKE_SOCKET}"
    fi
    ;;
  *"systemctl start"*) : >"${TP_FAKE_START_MARKER}" ;;
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

# The release installer, as far as the harness can see it: invoked as
# `install.sh --local-artifacts DIR --yes`, it reports the signature
# check, installs the set's runtime packages over whatever dpkg holds,
# and brings the services up. It lives outside the stub directory, so
# nothing on PATH reaches it except through sudo.
fake_installer="${td}/fake-install.sh"
cat >"$fake_installer" <<'STUB'
#!/bin/sh
[ "$#" -eq 3 ] && [ "$1" = --local-artifacts ] && [ "$3" = --yes ] || exit 9
dir="$2"
mode="${TP_FAKE_MODE:-ok}"
runtime="tensorplate-common tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli"
# Which set is being installed, and what that makes of the run so far: a
# baseline over a removal is a rollback, a candidate over a baseline is
# an upgrade.
previous="$(cat "${TP_FAKE_PHASE}" 2>/dev/null || echo none)"
if [ "$dir" = "${TP_FAKE_BASELINE_ASSETS:-}" ]; then
  installed_version="${TP_FAKE_BASELINE_VERSION}"
  if [ "$previous" = removed ]; then phase=rolled-back; else phase=baseline; fi
else
  installed_version="${TP_FAKE_CANDIDATE_VERSION}"
  if [ "$previous" = baseline ]; then phase=upgraded; else phase=candidate; fi
fi
printf '==> TensorPlate installer (fixture) for %s\n' "$dir"
# A case that needs the harness's terminal gone before the install fails
# waits here until the reader of that terminal has closed it.
if [ -n "${TP_FAKE_STDERR_CLOSED:-}" ]; then
  waited=0
  while [ ! -f "${TP_FAKE_STDERR_CLOSED}" ] && [ "$waited" -lt 100 ]; do
    sleep 0.1
    waited=$((waited + 1))
  done
fi
# An installer that refuses the device it is handed, before anything is
# installed.
if [ "$mode" = "install-fails-${phase}" ]; then
  echo "E: fixture installer refused this host" >&2
  exit 1
fi
# What both releases' install.sh do with TP_INSTALL_ALLOW_UNSIGNED in
# their environment: skip the signature check, warn, and carry on. A mode
# models any other way an install can succeed without verifying.
if [ "${TP_INSTALL_ALLOW_UNSIGNED:-0}" = 1 ]; then
  echo "warning: signature verification disabled (--allow-unsigned); SHA256SUMS authenticity is NOT verified" >&2
elif [ "$mode" != "installer-unverified-${phase}" ]; then
  echo "==> SHA256SUMS signature verified: signed by tensorplate/tensorplate release workflow"
fi
# apt-get -y without --allow-downgrades: a runtime package the removal
# left present at the candidate's version makes the baseline install a
# downgrade, and nothing is installed.
if [ "$installed_version" = "${TP_FAKE_BASELINE_VERSION}" ] &&
   [ "$(cat "${TP_FAKE_INSTALLED_VERSION}" 2>/dev/null || true)" = "${TP_FAKE_CANDIDATE_VERSION}" ]; then
  for pkg in $runtime; do
    case "$(cat "${TP_FAKE_PKG_DB}/${pkg}" 2>/dev/null || echo absent)" in
      absent|not-installed|config-files) ;;
      *)
        echo "E: Packages were downgraded and -y was used without --allow-downgrades." >&2
        exit 100
        ;;
    esac
  done
fi
# A set's checksum file changing between the digest preflight recorded
# and a later install of that set: the candidate's while the baseline is
# installed over it, the baseline's while the candidate is. Only a
# per-case copy of either set is ever handed to these modes.
if [ "$mode" = candidate-set-changed ] && [ "$phase" = baseline ]; then
  printf '\n' >>"${TP_FAKE_CANDIDATE_ASSETS}/SHA256SUMS"
fi
if [ "$mode" = baseline-set-changed ] && [ "$phase" = upgraded ]; then
  printf '\n' >>"${TP_FAKE_BASELINE_ASSETS}/SHA256SUMS"
fi
printf '%s\n' "$phase" >"${TP_FAKE_PHASE}"
printf '%s\n' "$installed_version" >"${TP_FAKE_INSTALLED_VERSION}"
for pkg in $runtime; do
  printf 'installed\n' >"${TP_FAKE_PKG_DB}/${pkg}"
done
# install-paths.sh lays out the state directory at configure time; the
# agent writes state.json into it when something is deployed.
mkdir -p "${TP_FAKE_VARLIB}/state"
# A conffile is written only where none exists, as --force-confold keeps
# an operator's copy. The reset modes model a package that replaces it
# anyway, in one direction or the other.
if [ ! -f "${TP_FAKE_OPERATOR_CONFIG}" ] ||
   { [ "$mode" = upgrade-resets-conffile ] && [ "$phase" = upgraded ]; } ||
   { [ "$mode" = rollback-resets-conffile ] && [ "$phase" = rolled-back ]; }; then
  printf '{"fixture":"packaged cli config"}\n' >"${TP_FAKE_OPERATOR_CONFIG}"
fi
if [ "$mode" = install-socket-missing ]; then
  rm -f "${TP_FAKE_SOCKET}"
fi
# An upgrade whose agent comes up without the deployment the baseline
# recorded, which is the whole point of the stage.
if [ "$mode" = upgrade-loses-deployment ] && [ "$phase" = upgraded ]; then
  rm -f "${TP_FAKE_ACTIVE_ID}"
fi
# A run has deleted /var/lib/tensorplate by now, so this plants the
# directory where only the rollback's own refusal can catch it.
if [ "$mode" = rollback-state-aside-exists ] && [ "$phase" = upgraded ]; then
  mkdir -p "${TP_FAKE_VARLIB}/state.bak"
  printf '{"fixture":"an earlier rollback"}\n' >"${TP_FAKE_VARLIB}/state.bak/state.json"
fi
# The set-aside state destroyed by the install that follows the removal,
# while its pathname stays a regular file: emptied, truncated, and
# replaced with different bytes at exactly the same length, which no
# check on the file's existence or size could tell from the original.
if [ "$phase" = rolled-back ]; then
  case "$mode" in
    rollback-empties-backup) : >"${TP_FAKE_VARLIB}/state.bak/state.json" ;;
    rollback-truncates-backup) printf '{"fixture":"dur' >"${TP_FAKE_VARLIB}/state.bak/state.json" ;;
    rollback-rewrites-backup)
      printf '{"fixture":"durable_state"}\n' >"${TP_FAKE_VARLIB}/state.bak/state.json"
      ;;
  esac
fi
# What install.sh does when the services do not come up or doctor reports
# a critical finding: fail AFTER every package is installed.
if [ "$mode" = "install-late-fails-${phase}" ]; then
  echo "error: TensorPlate services did not become ready within 30s" >&2
  exit 1
fi
echo "==> TensorPlate install complete"
STUB

# dpkg's package database, in the two shapes the harness queries it: a
# `name status` listing of tensorplate*, and one package's status and
# version. Each package's status is its record in the fixture database,
# which the installer, the purge and the removal move. The device starts
# with the apt channel's bootstrap package installed, unless a case says
# it never had it, and with a package dpkg remembers but never installed,
# which the purge must not name.
cat >"${stub_bin}/dpkg-query" <<'STUB'
#!/bin/sh
mode="${TP_FAKE_MODE:-ok}"
case "$*" in
  *'binary:Package'*)
    reads=$(cat "${TP_FAKE_PACKAGE_LIST_CALLS}" 2>/dev/null || echo 0)
    reads=$((reads + 1))
    printf '%s\n' "$reads" >"${TP_FAKE_PACKAGE_LIST_CALLS}"
    case "$mode:$reads" in
      # The install stage reads the listing twice, before and after its
      # purge, and the upgrade's clearing twice more.
      dpkg-query-fails-before:1|dpkg-query-fails-after:2|\
      upgrade-dpkg-query-fails-before:3|upgrade-dpkg-query-fails-after:4)
        printf 'dpkg-query: error: cannot read package database\n' >&2
        exit 2
        ;;
      dpkg-query-partial-before:1|dpkg-query-partial-after:2)
        printf 'tensorplate-agent installed\n'
        printf 'dpkg-query: no packages found matching tensorplate*\n' >&2
        exit 1
        ;;
      dpkg-query-unexpected-error:1)
        printf 'dpkg-query: unexpected query failure\n' >&2
        exit 1
        ;;
      dpkg-query-malformed:*)
        printf 'tensorplate-agent\n'
        exit 0
        ;;
      dpkg-no-match:*)
        printf 'dpkg-query: no packages found matching tensorplate*\n' >&2
        exit 1
        ;;
    esac
    # A database that stops answering once the rollback has set durable
    # state aside, or once it has attempted the removal.
    if { [ "$mode" = rollback-listing-fails ] && [ -e "${TP_FAKE_VARLIB}/state.bak" ]; } ||
       { [ "$mode" = listing-fails-after-remove ] && [ -f "${TP_FAKE_REMOVE_ATTEMPTED}" ]; }; then
      printf 'dpkg-query: error: cannot read package database\n' >&2
      exit 2
    fi
    for pkg in tensorplate-apt-source tensorplate-backend-python-pytorch \
               tensorplate-agent tensorplate-serving tensorplate-observability \
               tensorplate-cli tensorplate-common; do
      state=""
      [ -f "${TP_FAKE_PKG_DB}/${pkg}" ] && state="$(cat "${TP_FAKE_PKG_DB}/${pkg}")"
      # A re-run over a device that already carries the runtime.
      case "$mode:$pkg" in
        *:tensorplate-apt-source|*:tensorplate-backend-python-pytorch) ;;
        installed-runtime:*|dpkg-query-fails-after:*|dpkg-query-partial-after:*)
          [ -f "${TP_FAKE_PURGE_MARKER}" ] || state=installed
          ;;
      esac
      [ -z "$state" ] || printf '%s %s\n' "$pkg" "$state"
    done
    # A purge that leaves a record behind: the install stage's, or the
    # upgrade's -- the first purge a fresh fixture device sees.
    if [ "$mode" = purge-leaves-packages ] ||
       { [ "$mode" = upgrade-purge-leaves-packages ] && [ -f "${TP_FAKE_PURGE_MARKER}" ]; }; then
      printf 'tensorplate-common config-files\n'
    fi
    exit 0
    ;;
esac
pkg=""
for arg in "$@"; do pkg="$arg"; done
state=""
[ -f "${TP_FAKE_PKG_DB}/${pkg}" ] && state="$(cat "${TP_FAKE_PKG_DB}/${pkg}")"
version=""
[ -f "${TP_FAKE_INSTALLED_VERSION}" ] && version="$(cat "${TP_FAKE_INSTALLED_VERSION}")"
if [ -z "$state" ] || [ -z "$version" ] || [ "$mode:$pkg" = "package-missing:tensorplate-serving" ]; then
  printf 'dpkg-query: no packages found matching %s\n' "$pkg" >&2
  exit 1
fi
[ "$mode:$pkg" = "stale-version:tensorplate-agent" ] && version='0.2.1~rc.1-1'
# One package left behind at the other set's version, in each direction.
phase="$(cat "${TP_FAKE_PHASE}" 2>/dev/null || echo none)"
case "$mode:$phase:$pkg" in
  upgrade-keeps-baseline-version:upgraded:tensorplate-serving) version="${TP_FAKE_BASELINE_VERSION}" ;;
  rollback-keeps-candidate-version:rolled-back:tensorplate-cli) version="${TP_FAKE_CANDIDATE_VERSION}" ;;
esac
printf '%s %s' "$state" "$version"
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
# A file that is not a package, whatever its name says.
if ! grep -q '^Package: ' "$2"; then
  printf 'dpkg-deb: error: %s is not a Debian format archive\n' "$2" >&2
  exit 2
fi
sed -n 's/^Version: //p' "$2"
STUB

# dpkg, which the harness uses only to compare the two sets' package
# versions. It has to compare them for real: the upgrade path is admitted
# or refused on that answer, and the `exit 0` stub this replaced admitted
# every pair, so no ordering case could have failed.
#
# It knows the two version shapes release builds produce and treats an
# empty version as older than any other, as dpkg does -- so a manifest
# with no version is admitted by the comparison alone, and only the
# harness's own shape check refuses it. Any other shape exits 2, where
# dpkg would reject some and only warn about others; the harness refuses
# those before comparing as well.
cat >"${stub_bin}/dpkg" <<'STUB'
#!/usr/bin/env python3
import re, sys

VERSION = re.compile(r"(\d+)\.(\d+)\.(\d+)(?:~rc\.(\d+))?-(\d+)")

def version_key(version):
    if version == "":
        return ()
    match = VERSION.fullmatch(version)
    if not match:
        return None
    major, minor, patch, rc, revision = match.groups()
    # A candidate sorts before its release, as Debian's tilde does.
    return (int(major), int(minor), int(patch), 0 if rc else 1, int(rc or 0), int(revision))

args = sys.argv[1:]
if not args or args[0] != "--compare-versions":
    sys.exit(0)
if len(args) != 4:
    sys.exit(2)
left, op, right = version_key(args[1]), args[2], version_key(args[3])
if left is None or right is None:
    print(f"dpkg: error: version has bad syntax: {args[1]!r} {args[3]!r}", file=sys.stderr)
    sys.exit(2)
results = {"lt": left < right, "le": left <= right, "eq": left == right,
           "ne": left != right, "ge": left >= right, "gt": left > right}
if op not in results:
    sys.exit(2)
sys.exit(0 if results[op] else 1)
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
          # A unit an installer did not bring back: after the upgrade's
          # baseline install, after the candidate install over it, or
          # after the rollback's baseline install.
          phase="$(cat "${TP_FAKE_PHASE}" 2>/dev/null || echo none)"
          case "${TP_FAKE_MODE:-ok}:$phase:$*" in
            baseline-agent-inactive:baseline:*tensorplate-agent*|\
            upgrade-observability-inactive:upgraded:*tensorplate-observability*|\
            rollback-agent-inactive:rolled-back:*tensorplate-agent*)
              state=inactive
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
        # A service the upgrade did not actually replace: the same pid
        # both sides of the installer run.
        phase="$(cat "${TP_FAKE_PHASE}" 2>/dev/null || echo none)"
        case "${TP_FAKE_MODE:-ok}:$phase:$*" in
          upgrade-agent-pid-unchanged:baseline:*tensorplate-agent*|\
          upgrade-agent-pid-unchanged:upgraded:*tensorplate-agent*|\
          upgrade-observability-pid-unchanged:baseline:*tensorplate-observability*|\
          upgrade-observability-pid-unchanged:upgraded:*tensorplate-observability*)
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
# Inspect the effective explicit configuration before returning any fake
# appliance response. Merely accepting --config would hide a pin that
# still selects another agent or overrides inference with a serving URL.
if [ "${1:-}" != --config ] || [ -z "${2:-}" ]; then
  printf 'fixture CLI: an explicit pinned config is required\n' >&2
  exit 9
fi
config="$2"
shift 2
python3 - "$config" "${1:-}" <<'PY' || exit 9
import json, os, pathlib, sys

path, command = pathlib.Path(sys.argv[1]), sys.argv[2]
config = json.loads(path.read_text(encoding="utf-8"))
assert config.get("schema_version") == "0.1", "unexpected CLI config schema"
profile = config["profiles"][config["default_profile"]]
assert profile.get("mode") == "local", "CLI profile does not target the local appliance"
assert profile.get("socket_path") == os.environ["TP_FAKE_SOCKET"], "CLI socket is not the validated appliance"
assert not profile.get("agent_url"), "CLI profile overrides the agent endpoint"
assert not profile.get("serving_url"), "CLI profile overrides the discovered worker"
assert path.resolve() != pathlib.Path(os.environ["TENSORPLATE_CLI_CONFIG"]).resolve(), "CLI retained inherited config"
with open(os.environ["TP_FAKE_CLI_CALLS"], "a", encoding="utf-8") as log:
    log.write(json.dumps({"command": command, "config": str(path), "socket": profile["socket_path"]}) + "\n")
PY
mode="${TP_FAKE_MODE:-ok}"
phase=initial
[ -f "${TP_FAKE_RESTART_MARKER}" ] && phase=restarted
command="$1"
shift
out=""
input=""
bundle=""
deployment_id=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-file) out="$2"; shift 2 ;;
    --input) input="$2"; shift 2 ;;
    --deployment-id) deployment_id="$2"; shift 2 ;;
    --output|--component|--tail) shift 2 ;;
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
      # A candidate that is only unhealthy once it has been upgraded onto
      # the baseline, so the install stage passes and the upgrade fails.
      upgrade-doctor-failing)
        [ "$(cat "${TP_FAKE_PHASE}" 2>/dev/null || echo none)" = upgraded ] && failing=1
        ;;
      # A baseline whose doctor fails, in both phases the harness runs it
      # on. The candidate's own doctor stays green, so what this shows is
      # whether the baseline's is recorded or asserted.
      baseline-doctor-failing)
        case "$(cat "${TP_FAKE_PHASE}" 2>/dev/null || echo none)" in
          baseline|rolled-back) failing=1 ;;
        esac
        ;;
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
    printf '%s %s\n' "$deployment_id" "$bundle" >>"${TP_FAKE_DEPLOY_LOG}"
    if [ "$mode" = deploy-fails ]; then
      printf 'error: the agent rejected the deployment\n' >&2
      exit 3
    fi
    # A deployment the agent accepted is the one status reports and the
    # one it writes to durable state, so a later stage reading it back is
    # reading what an earlier install actually deployed.
    printf '%s\n' "$deployment_id" >"${TP_FAKE_ACTIVE_ID}"
    mkdir -p "${TP_FAKE_VARLIB}/state"
    printf '{"fixture":"durable state"}\n' >"${TP_FAKE_VARLIB}/state/state.json"
    deploy_phase=active
    deployed="$deployment_id"
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
    # What the agent has, rather than what the fixture wishes it had: an
    # agent whose state was set aside reports no deployment at all.
    active_id=""
    [ -f "${TP_FAKE_ACTIVE_ID}" ] && active_id="$(cat "${TP_FAKE_ACTIVE_ID}")"
    previous_active=null
    available=true
    backend=tensorrt
    supervision=""
    serving_url="\"http://127.0.0.1:${TP_FAKE_SERVING_PORT}/infer\""
    case "$mode" in
      rollback-agent-unavailable)
        [ "$(cat "${TP_FAKE_PHASE}" 2>/dev/null || echo none)" = rolled-back ] && available=false
        ;;
      rollback-previous-active)
        [ "$(cat "${TP_FAKE_PHASE}" 2>/dev/null || echo none)" = rolled-back ] &&
          previous_active='{"deployment_id":"an-earlier-deployment"}'
        ;;
    esac
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
    # The seventh read is the rollback's precondition: the candidate
    # serving what the baseline deployed. A device that is not in that
    # state must be refused there, before anything is stopped.
    if [ "$reads" -eq 7 ] && [ "$mode" = rollback-other-active ]; then
      active_id=a-different-deployment
    fi
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
    if [ -z "$active_id" ]; then
      active=null
    else
      active="{\"deployment_id\":\"${active_id}\",\"backend\":\"${backend}\",\"serving_url\":${serving_url}}"
    fi
    if [ "$available" = false ]; then
      printf '{"command":"%s","payload":{"severity":"%s","agent":{"available":false}}}\n' \
        "$command_name" "$severity"
      exit 0
    fi
    printf '{"command":"%s","payload":{"severity":"%s","agent":{"available":true,"agent_state":"%s","active":%s,"previous_active":%s%s}}}\n' \
      "$command_name" "$severity" "$agent_state" "$active" "$previous_active" "$supervision"
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
    python3 - "$input" "$out" "$garble" "$mode" "$phase" <<'PY' || exit 1
import base64, json, os, pathlib, struct, sys

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
endpoint = "http://127.0.0.1:" + os.environ["TP_FAKE_SERVING_PORT"] + "/infer"
source = "agent-discovered"
mode, phase = sys.argv[4:]
if mode == "infer-configured-endpoint":
    source = "profile"
if mode == "infer-wrong-endpoint" \
        or (mode == "restart-infer-wrong-endpoint" and phase == "restarted") \
        or (mode == "crashloop-infer-wrong-endpoint" and pathlib.Path(os.environ["TP_FAKE_BACKUP_PATH"]).exists()):
    endpoint = "http://127.0.0.1:1/infer"
print(json.dumps({"command": "infer", "payload": {
    "endpoint": endpoint, "endpoint_source": source, "result": response,
}}))
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
required_commands=(sudo systemctl journalctl python3 sha256sum dpkg-query dpkg-deb dpkg)
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
# TP_FAKE_BAD_DIGEST_DIR confines it to one set's directory.
cat >"${td}/bad-digest-bin/sha256sum" <<'STUB'
#!/bin/sh
if [ "$#" -eq 1 ] && [ "$1" = SHA256SUMS ] &&
   { [ -z "${TP_FAKE_BAD_DIGEST_DIR:-}" ] ||
     [ "$(pwd -P)" = "$(cd "${TP_FAKE_BAD_DIGEST_DIR}" && pwd -P)" ]; }; then
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

# The installers' own environment knobs switch verification off or point
# it elsewhere, so a run that carries any is refused, by name.
check "a run whose environment sets TP_INSTALL_ALLOW_UNSIGNED is refused" "1" \
  "$(TP_INSTALL_ALLOW_UNSIGNED=1 \
     preflight aarch64 "$jammy" "$r36" "${td}/evidence-allow-unsigned" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the variable" yes "$(said 'unset TP_INSTALL_ALLOW_UNSIGNED first')"
check "  before anything privileged ran" "" "$(cat "${td}/preflight-sudo.log")"
check "a run carrying any other installer knob is refused, each named" "1" \
  "$(TP_INSTALL_REPO=someone/else TP_INSTALL_COSIGN=/bin/true \
     preflight aarch64 "$jammy" "$r36" "${td}/evidence-installer-knobs" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  naming both" "yes yes" \
  "$(said 'TP_INSTALL_REPO') $(said 'TP_INSTALL_COSIGN')"

# --- the stages, executed against a stubbed appliance.
appliance="${td}/appliance"
mkdir -p "${appliance}/run" "${appliance}/log" "${appliance}/scratch"
# Every run inherits an unrelated appliance and serving endpoint. The
# fixture CLI checks that the harness explicitly overrides both before
# it supplies any healthy response; a bare call cannot silently pass.
cat >"${appliance}/inherited-cli.json" <<'JSON'
{"schema_version":"0.1","default_profile":"unrelated","profiles":{"unrelated":{"mode":"url","agent_url":"127.0.0.1:1","serving_url":"http://127.0.0.1:2/infer"}}}
JSON

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
        # The worker serves whatever the agent last deployed, so the
        # health endpoint answers about that and not about a fixture
        # constant: after an upgrade or a rollback it is a different id.
        active = directory / "active-id"
        deployment = active.read_text().strip() if active.exists() else ""
        phase_file = directory / "phase"
        installed = phase_file.read_text().strip() if phase_file.exists() else ""
        if (restarted and mode == "restart-wrong-health") or mode == "health-wrong-deployment" \
                or (mode == "baseline-health-wrong" and installed == "baseline"):
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
  : >"${appliance}/cli-calls.jsonl"
  : >"${appliance}/health-requests.log"
  rm -rf "${appliance}/staged-bundle" "${appliance}/scratch" "${appliance}/varlib"
  mkdir -p "${appliance}/scratch"
  rm -f "${appliance}/restarted" "${appliance}/config-broken" "${appliance}/installed-version" \
    "${appliance}/restarts" "${appliance}/restore-failed" "${appliance}/backup-path" \
    "${appliance}/pid" "${appliance}/started" "${appliance}/status-reads" "${appliance}/mainpid-reads" \
    "${appliance}/package-list-reads" "${appliance}/phase" "${appliance}/remove-attempted" \
    "${appliance}/active-id" "${appliance}/cli.json"
  # The device's package database before the run: the apt channel's
  # bootstrap package installed, unless the case is a device set up the
  # way the runbook sets one up, by install.sh alone, which never installs
  # it; and a package dpkg knows but never installed.
  rm -rf "${appliance}/packages"
  mkdir -p "${appliance}/packages"
  if [[ "$mode" != no-apt-source ]]; then
    printf 'installed\n' >"${appliance}/packages/tensorplate-apt-source"
  fi
  printf 'not-installed\n' >"${appliance}/packages/tensorplate-backend-python-pytorch"
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
    TENSORPLATE_CLI_CONFIG="${appliance}/inherited-cli.json" \
    TP_FAKE_CLI_CALLS="${appliance}/cli-calls.jsonl" \
    TP_FAKE_PACKAGE_LIST_CALLS="${appliance}/package-list-reads" \
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
    TP_FAKE_INSTALLED_VERSION="${appliance}/installed-version" \
    TP_FAKE_PKG_DB="${appliance}/packages" \
    TP_FAKE_INSTALLER="$fake_installer" \
    TP_FAKE_REMOVE_ATTEMPTED="${appliance}/remove-attempted" \
    TP_FAKE_STDERR_CLOSED="${stages_stderr_closed:-}" \
    TP_FAKE_PHASE="${appliance}/phase" \
    TP_FAKE_VARLIB="${appliance}/varlib" \
    TP_FAKE_ACTIVE_ID="${appliance}/active-id" \
    TP_FAKE_OPERATOR_CONFIG="${appliance}/cli.json" \
    TP_JETSON_OPERATOR_CONFIG="${appliance}/cli.json" \
    TP_FAKE_BASELINE_ASSETS="$baseline_assets" \
    TP_FAKE_CANDIDATE_ASSETS="$candidate_assets" \
    TP_FAKE_BASELINE_VERSION="$baseline_version" \
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
    TP_FAKE_SERVING_PORT="$serving_port" \
    TP_FAKE_CONFIG_BROKEN="${appliance}/config-broken" \
    TP_FAKE_AGENT_CONFIG="${appliance}/agent-config" \
    TP_FAKE_BACKUP_PATH="${appliance}/backup-path" \
    TP_FAKE_RESTORE_FAILED="${appliance}/restore-failed" \
    TP_FAKE_RESTARTS_FILE="${appliance}/restarts" \
    python3 -c "$default_signals" "$BASH" "$harness" \
      --candidate-tag v0.2.1-rc.2 \
      --candidate-assets-dir "$candidate_assets" \
      --evidence-dir "$evidence" \
      --tested-version 0.2.1 \
      --confirm RESET-TENSORPLATE \
      "$@" >"${evidence}.out" 2>"${stages_stderr:-${evidence}.err}"
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
check "  every CLI command ignores inherited agent and inference endpoints" yes \
  "$(python3 - "${appliance}/cli-calls.jsonl" <<'PY'
import json, sys

calls = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
expected = ["doctor", "deploy", "infer", "status", "status", "logs",
            "infer", "status", "infer", "status"]
print("yes" if [call["command"] for call in calls] == expected else str(calls))
PY
)"
check "  the private CLI config is removed after the run" yes \
  "$(python3 - "${appliance}/cli-calls.jsonl" <<'PY'
import json, pathlib, sys

paths = {json.loads(line)["config"] for line in open(sys.argv[1], encoding="utf-8")}
print("yes" if paths and all(not pathlib.Path(path).exists() for path in paths) else "no")
PY
)"
for stage in install deploy-smoke status-logs restart crash-loop; do
  check "  ${stage} is recorded as a pass" "pass" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
for stage in upgrade rollback offline; do
  check "  ${stage} is recorded as skipped" "skipped" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
check "  the skipped stages keep the run incomplete" incomplete \
  "$(report_field "${ok_evidence}/lifecycle-report.json" outcome)"
check "  every skip says what would run it" yes \
  "$(python3 - "${ok_evidence}/lifecycle-report.json" <<'PY'
import json, sys

stages = {s["stage"]: s for s in json.load(open(sys.argv[1]))["stages"]}
# offline names the work that closes it; the two that a baseline would
# have run name the options that run them.
wanted = {"offline": ["follow-up", "127.0.0.1/32"],
          "upgrade": ["--baseline-tag", "--baseline-assets-dir"],
          "rollback": ["--baseline-tag", "--baseline-assets-dir"]}
print("yes" if all(
    stages[name]["status"] == "skipped"
    and all(marker in stages[name].get("detail", "") for marker in markers)
    for name, markers in wanted.items()
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
check "  the deploy names the smoke id and the staged bundle" \
  "${deployment_id} ${appliance}/staged-bundle" "$(cat "${appliance}/deploy.log")"
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

# A failed database read is not an empty database, either before the
# purge or while checking it afterward. The no-match control is the one
# nonzero query result that means there are no packages to remove.
evidence="${td}/stages-dpkg-no-match"
check "a documented dpkg no-match result permits a clean install" 0 \
  "$(run_stages dpkg-no-match "$evidence" "" --bundle-dir "${td}/bundle-good")"
check "  and certifies the install stage" pass "$(stage_status "${evidence}/lifecycle-report.json" install)"
check "  without invoking an empty purge" no \
  "$(grep -Fq 'apt-get purge' "${appliance}/sudo.log" && echo yes || echo no)"
for query_case in "dpkg-query-fails-before|2|cannot read package database|no" \
                  "dpkg-query-fails-after|2|cannot read package database|yes" \
                  "dpkg-query-partial-before|1|no packages found matching tensorplate*|no" \
                  "dpkg-query-partial-after|1|no packages found matching tensorplate*|yes" \
                  "dpkg-query-unexpected-error|1|unexpected query failure|no" \
                  "dpkg-query-malformed|1|invalid dpkg-query package inventory record|no"; do
  mode="${query_case%%|*}"
  rest="${query_case#*|}"
  expected_status="${rest%%|*}"
  rest="${rest#*|}"
  diagnostic="${rest%%|*}"
  purged="${rest#*|}"
  evidence="${td}/stages-${mode}"
  check "${mode} preserves the package-query failure" "$expected_status" \
    "$(run_stages "$mode" "$evidence" "" --bundle-dir "${td}/bundle-good")"
  check "  and records a failed install" fail "$(stage_status "${evidence}/lifecycle-report.json" install)"
  check "  and retains the query diagnostic" yes "$(logged "${evidence}/install.log" "$diagnostic")"
  check "  and reaches the intended side of the purge" "$purged" \
    "$(grep -Fq 'apt-get purge' "${appliance}/sudo.log" && echo yes || echo no)"
  check "  and never removes the installed state" no \
    "$(grep -Fq 'rm -rf /etc/tensorplate' "${appliance}/sudo.log" && echo yes || echo no)"
  check "  and never invokes the installer" no \
    "$(grep -Fq '/install.sh --local-artifacts' "${appliance}/sudo.log" && echo yes || echo no)"
  check "  and attests no artifact digest" absent \
    "$(report_field "${evidence}/lifecycle-report.json" subject artifact_digest)"
done

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
                 "infer-configured-endpoint|1|checks failed: inference_endpoint_source" \
                 "infer-wrong-endpoint|1|checks failed: inference_endpoint" \
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
                 "restart-infer-wrong-endpoint|checks failed: inference_endpoint" \
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
                 "crashloop-recovery-garbled|value mismatch at 1" \
                 "crashloop-infer-wrong-endpoint|checks failed: inference_endpoint"; do
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

# --- upgrade and rollback.
#
# Two more installs and a removal, so the fixture dpkg database, the
# conffile and the durable state all move with them: the assertions are
# about what the device carries after each step, not about which commands
# were logged.
run_baseline_stages() {
  local mode="$1" evidence="$2" sudo_fail="${3:-}"
  shift 3
  run_stages "$mode" "$evidence" "$sudo_fail" --bundle-dir "${td}/bundle-good" \
    --baseline-tag v0.1.5 --baseline-assets-dir "$baseline_assets" "$@"
}
# The assets directory of each install.sh call, in order, and the sudo
# log line number of the Nth of them.
install_order() {
  sed -n 's#.*/install\.sh --local-artifacts \([^ ]*\) --yes$#\1#p' "${appliance}/sudo.log" | tr '\n' ' '
}
install_line() {
  grep -nF '/install.sh --local-artifacts ' "${appliance}/sudo.log" | sed -n "${1}p" | cut -d: -f1
}
# Whether the sudo log carries TEXT after line N, or `missing` when there
# is no line N to count from.
sudo_after() {
  if [[ -z "$1" ]]; then
    echo missing
    return 0
  fi
  tail -n "+$(($1 + 1))" "${appliance}/sudo.log" | grep -qF -- "$2" && echo yes || echo no
}
# The packaged conffile plus the operator's appended newline: the
# content unchanged, and one more line than the package ships.
operator_config_edited() {
  if [[ ! -f "${appliance}/cli.json" ]]; then
    echo missing
  elif [[ "$(cat "${appliance}/cli.json")" == '{"fixture":"packaged cli config"}' &&
          "$(wc -l <"${appliance}/cli.json")" -eq 2 ]]; then
    echo yes
  else
    echo no
  fi
}
# Whether a run's stderr carries TEXT.
err_says() {
  grep -Fq -- "$2" "${1}.err" && echo yes || echo no
}
# Whether a run filed a stranded-device report, and printed every line of
# it: `yes`, `no`, or `unprinted` when the file says more than stderr did.
stranded_filed() {
  local report="${1}/stranded-device.txt"
  if [[ ! -s "$report" ]]; then
    echo no
  elif [[ "$(grep -cFxf "$report" "${1}.err" || true)" -eq "$(wc -l <"$report")" ]]; then
    echo yes
  else
    echo unprinted
  fi
}
signature_line='==> SHA256SUMS signature verified: signed by tensorplate/tensorplate release workflow'
cleared_dirs='/etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate'
candidate_present='tensorplate-agent installed, tensorplate-serving installed, tensorplate-observability installed, tensorplate-cli installed, tensorplate-common installed'
# The durable state the fixture agent writes on a deploy, and its digest:
# what the rollback's preservation check has to hold the saved copy to.
state_fixture='{"fixture":"durable state"}'
state_sha256="$(printf '%s\n' "$state_fixture" |
  python3 -c 'import hashlib, sys
print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())')"

baseline_evidence="${td}/stages-baseline"
check "a run with a baseline completes" "0" "$(run_baseline_stages ok "$baseline_evidence" "")"
for stage in install deploy-smoke status-logs restart crash-loop upgrade rollback; do
  check "  ${stage} is recorded as a pass" "pass" \
    "$(stage_status "${baseline_evidence}/lifecycle-report.json" "$stage")"
done
check "  offline is the only skipped stage" skipped \
  "$(stage_status "${baseline_evidence}/lifecycle-report.json" offline)"
check "  and keeps the run incomplete" incomplete \
  "$(report_field "${baseline_evidence}/lifecycle-report.json" outcome)"
check "  the report still attests the candidate, not the baseline" "$candidate_digest" \
  "$(report_field "${baseline_evidence}/lifecycle-report.json" subject artifact_digest)"
check "  and the baseline digest is filed on its own" "${baseline_digest}  SHA256SUMS" \
  "$(cat "${baseline_evidence}/baseline-digest.txt")"
check "  the baseline checksum verification is filed" yes \
  "$(grep -Fq 'install.sh: OK' "${baseline_evidence}/baseline-checksums.txt" && echo yes || echo no)"
check "  the upgrade path names both tags and neither is unsigned" \
  "v0.1.5 False v0.2.1-rc.2 False" \
  "$(python3 -c 'import json,sys
p=json.load(open(sys.argv[1]))
print(p["from"]["release_tag"], p["from"]["allow_unsigned"], p["to"]["release_tag"], p["to"]["allow_unsigned"])' \
    "${baseline_evidence}/upgrade-path.json")"
check "  and the digests it names are each set's own" "${baseline_digest} ${candidate_digest}" \
  "$(python3 -c 'import json,sys
p=json.load(open(sys.argv[1]))
print(p["from"]["sha256sums_sha256"], p["to"]["sha256sums_sha256"])' \
    "${baseline_evidence}/upgrade-path.json")"
# The path filed is the one preflight compared, so the evidence says why
# it was admitted rather than restating the options the operator typed.
check "  and it records the package versions preflight compared" \
  "${baseline_version} ${candidate_version}" \
  "$(python3 -c 'import json,sys
p=json.load(open(sys.argv[1]))
found=[]
for side in ("from", "to"):
    versions=sorted(set(p[side]["packages"].values()))
    assert len(versions) == 1, p[side]["packages"]
    found.append(versions[0])
print(" ".join(found))' \
    "${baseline_evidence}/upgrade-path.json")"
check "  for every runtime package this row installs" \
  "tensorplate-agent tensorplate-cli tensorplate-common tensorplate-observability tensorplate-serving" \
  "$(python3 -c 'import json,sys
p=json.load(open(sys.argv[1]))
assert p["from"]["packages"].keys() == p["to"]["packages"].keys(), p
print(" ".join(sorted(p["from"]["packages"])))' \
    "${baseline_evidence}/upgrade-path.json")"

# candidate, then baseline, then candidate over it, then baseline again.
check "  the run installs candidate, baseline, candidate, baseline" \
  "${assets} ${baseline} ${assets} ${baseline} " "$(install_order)"
check "  the install stage runs the candidate's installer as it always has" yes \
  "$(grep -Fxq "bash ${assets}/install.sh --local-artifacts ${assets} --yes" "${appliance}/sudo.log" && echo yes || echo no)"
check "  and the other three run behind the installer environment scrub" 3 \
  "$(grep -c '^bash -c .* tensorplate-install .*/install\.sh --local-artifacts .* --yes$' "${appliance}/sudo.log" || true)"
for installed in install-baseline install-upgrade install-rollback; do
  check "  ${installed}.txt carries the installer's verified signature" yes \
    "$(grep -Fxq "$signature_line" "${baseline_evidence}/${installed}.txt" && echo yes || echo no)"
done
check "  the upgrade purges the candidate before installing the baseline" yes \
  "$(purge="$(sudo_line 'apt-get purge')"; first="$(install_line 1)"; second="$(install_line 2)"
     [[ -n "$purge" && -n "$first" && -n "$second" && "$first" -lt "$purge" && "$purge" -lt "$second" ]] \
       && echo yes || echo no)"
check "  and the rollback removes rather than purges" 1 \
  "$(grep -c 'apt-get purge' "${appliance}/sudo.log" || true)"
check "  the removal falls between the upgrade and the last install" yes \
  "$(remove="$(sudo_line 'apt-get remove')"; third="$(install_line 3)"; fourth="$(install_line 4)"
     [[ -n "$remove" && -n "$third" && -n "$fourth" && "$third" -lt "$remove" && "$remove" -lt "$fourth" ]] \
       && echo yes || echo no)"
# The controls for the precondition cases below: after the upgrade's
# install, the rollback stops both services, sets state aside and removes.
check "  the rollback stops, sets aside and removes after the upgrade" "yes yes yes" \
  "$(third="$(install_line 3)"
     echo "$(sudo_after "$third" 'systemctl stop tensorplate-agent tensorplate-observability')" \
       "$(sudo_after "$third" 'mv -T /var/lib/tensorplate/state /var/lib/tensorplate/state.bak')" \
       "$(sudo_after "$third" 'apt-get remove')")"
remove_line="$(grep -F 'apt-get remove' "${appliance}/sudo.log" || true)"
check "  the removal names every runtime package and nothing else" \
  "tensorplate-agent tensorplate-cli tensorplate-common tensorplate-observability tensorplate-serving" \
  "$(printf '%s\n' "$remove_line" | tr ' ' '\n' | grep '^tensorplate' | sort | tr '\n' ' ' | sed 's/ $//')"
check "  the state is set aside under the documented name" yes \
  "$(grep -Fxq 'mv -T /var/lib/tensorplate/state /var/lib/tensorplate/state.bak' "${appliance}/sudo.log" \
     && echo yes || echo no)"
check "  and the set-aside state survived the rollback with its bytes" \
  "$state_fixture" \
  "$(cat "${appliance}/varlib/state.bak/state.json" 2>/dev/null || echo missing)"
# The digest the preservation check compares against is only worth
# anything if it was taken from the stopped agent's own copy, before the
# move left no original to compare with.
check "  digested with the services stopped and before the move" \
  "systemctl-stop sha256sum mv" \
  "$(third="$(install_line 3)"
     tail -n "+$((third + 1))" "${appliance}/sudo.log" |
       sed -n -e 's/^systemctl stop tensorplate-agent tensorplate-observability$/systemctl-stop/p' \
              -e 's#^sha256sum /var/lib/tensorplate/state/state.json$#sha256sum#p' \
              -e 's#^mv -T /var/lib/tensorplate/state /var/lib/tensorplate/state.bak$#mv#p' |
       tr '\n' ' ' | sed 's/ $//')"
check "  and the saved copy is read back after the baseline install" yes \
  "$(fourth="$(install_line 4)"
     sudo_after "$fourth" 'sha256sum /var/lib/tensorplate/state.bak/state.json')"
check "  the operator's conffile edit survived both directions" yes "$(operator_config_edited)"
check "  the baseline versions are filed per package" 5 \
  "$(grep -c "${baseline_version} " "${baseline_evidence}/packages-baseline.txt" || true)"
check "  the candidate versions are filed after the upgrade" 5 \
  "$(grep -c "${candidate_version} " "${baseline_evidence}/packages-after-upgrade.txt" || true)"
check "  the baseline versions are filed after the rollback" 5 \
  "$(grep -c "${baseline_version} " "${baseline_evidence}/packages-after-rollback.txt" || true)"
# Both listings are the unfiltered ones, so what the device carried either
# side of the removal is evidence rather than an inference from the words
# the harness passed to apt-get.
check "  the listing before the removal is filed whole" \
  "tensorplate-apt-source installed|tensorplate-backend-python-pytorch not-installed|tensorplate-agent installed|tensorplate-serving installed|tensorplate-observability installed|tensorplate-cli installed|tensorplate-common installed" \
  "$(tr '\n' '|' <"${baseline_evidence}/packages-before-remove.txt" | sed 's/|$//')"
check "  every conffile-owning package kept its conffiles" 4 \
  "$(grep -c ' config-files$' "${baseline_evidence}/packages-after-remove.txt" || true)"
check "  and tensorplate-common, which has none, is no longer installed" \
  "tensorplate-common not-installed" \
  "$(grep -F 'tensorplate-common ' "${baseline_evidence}/packages-after-remove.txt" || true)"
check "  and the listing records what the removal left alone" \
  "tensorplate-apt-source installed" \
  "$(grep -F 'tensorplate-apt-source ' "${baseline_evidence}/packages-after-remove.txt" || true)"
check "  including the package dpkg never had installed" \
  "tensorplate-backend-python-pytorch not-installed" \
  "$(grep -F 'tensorplate-backend-python-pytorch ' "${baseline_evidence}/packages-after-remove.txt" || true)"
check "  and the rollback says the bootstrap package is as it was" yes \
  "$(logged "${baseline_evidence}/rollback.log" 'and tensorplate-apt-source is still installed')"
check "  the baseline doctor is filed rather than asserted" "0 0" \
  "$(cat "${baseline_evidence}/doctor-baseline.exit" "${baseline_evidence}/doctor-after-rollback.exit" | tr '\n' ' ' | sed 's/ $//')"
check "  each install served a deployment under its own id" \
  "jetson-lifecycle-smoke jetson-lifecycle-smoke-baseline jetson-lifecycle-smoke-rollback" \
  "$(awk '{print $1}' "${appliance}/deploy.log" | tr '\n' ' ' | sed 's/ $//')"
check "  the baseline served its own deployment before the upgrade" jetson-lifecycle-smoke-baseline \
  "$(report_field "${baseline_evidence}/upgrade-baseline-result.json" deployment_id)"
check "  the candidate re-warmed the baseline's deployment without a deploy of its own" \
  jetson-lifecycle-smoke-baseline \
  "$(report_field "${baseline_evidence}/upgrade-result.json" deployment_id)"
check "  and answered its identity request" tensorrt_identity \
  "$(report_field "${baseline_evidence}/upgrade-result.json" inference_round_trip)"
check "  the rolled-back agent reported no deployment before the redeploy" None \
  "$(python3 -c 'import json,sys
print(json.load(open(sys.argv[1]))["payload"]["agent"]["active"])' \
    "${baseline_evidence}/status-after-rollback.json")"
check "  and the fresh deployment answered on the baseline" jetson-lifecycle-smoke-rollback \
  "$(report_field "${baseline_evidence}/rollback-result.json" deployment_id)"
check "  no stranded-device report was filed" no "$(stranded_filed "$baseline_evidence")"
check "  or printed" no "$(err_says "$baseline_evidence" 'did not finish')"
check "  and the report is schema-valid" "yes" \
  "$(python3 - "$schema" "${baseline_evidence}/lifecycle-report.json" <<'PY'
import json, sys
try:
    import jsonschema
except ImportError:
    print("yes")
    sys.exit(0)
errors = list(jsonschema.Draft7Validator(json.load(open(sys.argv[1]))).iter_errors(
    json.load(open(sys.argv[2]))))
print("yes" if not errors else f"no: {errors[0].message}")
PY
)"

# The runbook's own device: install.sh never installs the apt channel's
# bootstrap package, so a device set up by it has none, and the rollback
# must neither require one nor add one.
evidence="${td}/stages-no-apt-source"
check "a device without the apt channel's bootstrap package completes the rollback" 0 \
  "$(run_baseline_stages no-apt-source "$evidence" "")"
check "  and rollback is recorded as a pass" pass \
  "$(stage_status "${evidence}/lifecycle-report.json" rollback)"
check "  neither listing names the bootstrap package" no \
  "$(cat "${evidence}/packages-before-remove.txt" "${evidence}/packages-after-remove.txt" \
     | grep -qF tensorplate-apt-source && echo yes || echo no)"
check "  and the rollback says it is still absent" yes \
  "$(logged "${evidence}/rollback.log" 'and tensorplate-apt-source is still absent')"

# Doctor on the baseline is recorded, not asserted. Without a case that
# makes it fail, turning record_baseline_doctor's `|| status=$?` into a
# refusal would leave every check above green.
doctor_evidence="${td}/stages-baseline-doctor-failing"
check "a baseline whose doctor fails does not fail the run" "0" \
  "$(run_baseline_stages baseline-doctor-failing "$doctor_evidence" "")"
check "  upgrade still passes" pass \
  "$(stage_status "${doctor_evidence}/lifecycle-report.json" upgrade)"
check "  rollback still passes" pass \
  "$(stage_status "${doctor_evidence}/lifecycle-report.json" rollback)"
check "  and both baseline doctors are filed with their exit status" "10 10" \
  "$(cat "${doctor_evidence}/doctor-baseline.exit" "${doctor_evidence}/doctor-after-rollback.exit" \
     | tr '\n' ' ' | sed 's/ $//')"
check "  and the failing finding is in the filed report" fail \
  "$(python3 -c 'import json,sys
by_id={f["id"]: f for f in json.load(open(sys.argv[1]))["payload"]["findings"]}
print(by_id["serving_binary_installed"]["status"])' \
    "${doctor_evidence}/doctor-baseline.json")"
# The candidate's own doctor is still asserted; upgrade-doctor-failing
# below is what shows a failing one there fails the stage.

# A sudo policy or PAM environment that hands the installer
# TP_INSTALL_ALLOW_UNSIGNED although the harness's own environment is
# clean. The upgrade's and the rollback's installs drop it and verify.
evidence="${td}/stages-sudo-passes-allow-unsigned"
check "an opt-out sudo hands the installers does not reach the upgrade's or the rollback's" 0 \
  "$(run_baseline_stages sudo-passes-allow-unsigned "$evidence" "")"
for installed in install-baseline install-upgrade install-rollback; do
  check "  ${installed}.txt carries a verified signature and no disabled check" "yes no" \
    "$(grep -Fxq "$signature_line" "${evidence}/${installed}.txt" && echo yes || echo no) $(grep -Fq 'signature verification disabled' "${evidence}/${installed}.txt" && echo yes || echo no)"
done
# The control: the fixture's sudo really did hand it over. The install
# stage keeps its own call, which is not scrubbed, and saw it.
check "  while the install stage's own, unscrubbed call did see it" yes \
  "$(logged "${evidence}/install.log" 'signature verification disabled')"

# An installer run that succeeds without saying it verified the
# signature, in each of the three installs the scrub fronts.
for unverified_case in "installer-unverified-baseline|upgrade|${baseline}" \
                       "installer-unverified-upgraded|upgrade|${assets}" \
                       "installer-unverified-rolled-back|rollback|${baseline}"; do
  mode="${unverified_case%%|*}"
  rest="${unverified_case#*|}"
  stage="${rest%%|*}"
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_baseline_stages "$mode" "$evidence" "")"
  check "  and ${stage} is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" "$stage")"
  check "  for the reason the case provokes" yes \
    "$(logged "${evidence}/${stage}.log" "${rest#*|}/install.sh succeeded without reporting a verified SHA256SUMS signature")"
done

# --- the baseline's own eligibility.
baseline_preflight() {
  local evidence="$1"
  shift
  preflight aarch64 "$jammy" "$r36" "$evidence" 0.2.1 v0.2.1-rc.2 "$assets" \
    --bundle-dir "${td}/bundle-good" "${confirm[@]}" "$@"
}
check "a baseline older than the candidate passes preflight" "0" \
  "$(baseline_preflight "${td}/evidence-baseline-ok" --baseline-tag v0.1.5 --baseline-assets-dir "$baseline")"
check "--baseline-assets-dir without --baseline-tag is refused" "1" \
  "$(baseline_preflight "${td}/evidence-baseline-no-tag" --baseline-assets-dir "$baseline")"
check "  and names the missing --baseline-tag" yes "$(said 'needs --baseline-tag')"
check "--baseline-tag without --baseline-assets-dir is refused" "1" \
  "$(baseline_preflight "${td}/evidence-baseline-no-dir" --baseline-tag v0.1.5)"
check "  and names the missing --baseline-assets-dir" yes "$(said 'needs --baseline-assets-dir')"
check "a baseline directory that does not exist is refused" "1" \
  "$(baseline_preflight "${td}/evidence-baseline-absent" --baseline-tag v0.1.5 \
     --baseline-assets-dir "${td}/absent-baseline")"
check "  and says the directory is missing" yes \
  "$(said '--baseline-assets-dir must name a directory')"
make_assets "${td}/baseline-manifest-v0.1.4" v0.1.4 "$baseline_version"
check "a baseline whose manifest names another tag is refused" "1" \
  "$(baseline_preflight "${td}/evidence-baseline-other-tag" --baseline-tag v0.1.5 \
     --baseline-assets-dir "${td}/baseline-manifest-v0.1.4")"
check "  and names the tag the manifest carries" yes "$(said "names release tag 'v0.1.4', not 'v0.1.5'")"
check "a baseline under a directory this run deletes is refused" "1" \
  "$(baseline_preflight "${td}/evidence-baseline-deleted" --baseline-tag v0.1.5 \
     --baseline-assets-dir /var/lib/tensorplate/baseline)"
check "  and says the run deletes the baseline directory" yes \
  "$(said '--baseline-assets-dir /var/lib/tensorplate/baseline is under')"

# This row's baseline is v0.1.5 and nothing else: the report names only
# the candidate, so a gate could not tell an earlier candidate of the same
# release, or another predecessor, from the path this row validates.
# Each set below is a well-formed one for the tag it is named with.
for other_tag in v0.1.4 v0.2.1-rc.1 v0.2.1-rc.2 v0.2.1 v0.3.0 0.1.5; do
  make_assets "${td}/baseline-tag-${other_tag}" "$other_tag" '0.1.4-1'
  check "a baseline tagged ${other_tag} is refused" "1" \
    "$(baseline_preflight "${td}/evidence-tag-${other_tag}" --baseline-tag "$other_tag" \
       --baseline-assets-dir "${td}/baseline-tag-${other_tag}")"
  check "  and names this row's baseline" yes \
    "$(said "--baseline-tag must be v0.1.5, this row's published predecessor; got ${other_tag}")"
done

# Strictly older than the candidate, with a candidate sorting below the
# release it leads to: a candidate that is not newer than v0.1.5 is
# refused against it by tag, whatever its packages say.
for candidate_case in "v0.1.5|0.1.5|0.1.5-1" "v0.1.5-rc.1|0.1.5|0.1.5~rc.1-1" "v0.1.4|0.1.4|0.1.4-1"; do
  candidate_tag="${candidate_case%%|*}"
  rest="${candidate_case#*|}"
  make_assets "${td}/candidate-${candidate_tag}" "$candidate_tag" "${rest#*|}"
  check "a ${candidate_tag} candidate is refused against the v0.1.5 baseline" "1" \
    "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-candidate-${candidate_tag}" "${rest%%|*}" \
       "$candidate_tag" "${td}/candidate-${candidate_tag}" --bundle-dir "${td}/bundle-good" \
       "${confirm[@]}" --baseline-tag v0.1.5 --baseline-assets-dir "$baseline")"
  check "  and says the baseline must be strictly older" yes \
    "$(said "the baseline v0.1.5 must be strictly older than the candidate ${candidate_tag}")"
done

# An older tag does not make an installable upgrade path. What apt orders
# is the Debian version each .deb carries: a baseline whose packages are
# not older makes the upgrade stage's candidate install a downgrade
# apt-get -y refuses, on a device the run has already rebuilt twice.
make_assets "${td}/baseline-newer-debs" v0.1.5 9.9.9-1
check "a baseline whose tag is older but whose packages are not is refused" "1" \
  "$(baseline_preflight "${td}/evidence-newer-debs" --baseline-tag v0.1.5 \
     --baseline-assets-dir "${td}/baseline-newer-debs")"
check "  and names the package and both versions" yes \
  "$(said "tensorplate-common: the baseline's 9.9.9-1 is not older than the candidate's ${candidate_version}")"
check "  and says the two sets form no upgrade path" yes \
  "$(said 'the baseline and candidate sets do not form an upgrade path')"
check "  before anything privileged ran" "" "$(cat "${td}/preflight-sudo.log")"

# What apt orders on is the control Version inside the .deb. The release
# driver parses the manifest's `version` out of the file name and never
# reads the package, so a set whose two disagree is admitted by anything
# that compares the manifest -- and refused by apt-get on a device the
# run has already rebuilt twice.
make_assets "${td}/baseline-deb-newer" v0.1.5 "$baseline_version" deb-version-newer
check "a baseline whose .deb carries a newer version than its manifest is refused" "1" \
  "$(baseline_preflight "${td}/evidence-deb-newer" --baseline-tag v0.1.5 \
     --baseline-assets-dir "${td}/baseline-deb-newer")"
check "  and compares the versions the packages carry" yes \
  "$(said "tensorplate-agent: the baseline's 9.9.9-1 is not older than the candidate's ${candidate_version}")"
check "  although its manifest declares an older one" "0.1.5-1" \
  "$(python3 -c 'import json,pathlib,sys
m=json.loads(pathlib.Path(sys.argv[1]).read_text())
print(next(a["version"] for a in m["artifacts"]
           if a["package"] == "tensorplate-agent" and a["architecture"] == "arm64"))' \
    "${td}/baseline-deb-newer/tensorplate-v0.1.5-artifacts.json")"
check "  before anything privileged ran" "" "$(cat "${td}/preflight-sudo.log")"

# A .deb that dpkg-deb cannot read carries no version to compare, and an
# empty one is older than any other to dpkg --compare-versions.
make_assets "${td}/baseline-deb-corrupt" v0.1.5 "$baseline_version" deb-corrupt
check "a baseline with a .deb dpkg-deb cannot read is refused" "1" \
  "$(baseline_preflight "${td}/evidence-deb-corrupt" --baseline-tag v0.1.5 \
     --baseline-assets-dir "${td}/baseline-deb-corrupt")"
check "  and names the file and what dpkg-deb did" yes \
  "$(said "the baseline set's tensorplate-serving_${baseline_version}_arm64.deb does not carry a readable Debian Version in its control file: dpkg-deb exited 2")"
check "  before anything privileged ran" "" "$(cat "${td}/preflight-sudo.log")"

# dpkg --compare-versions reads an empty version as older than any other,
# so a manifest that names no version for a package would be admitted by
# the comparison alone.
make_assets "${td}/baseline-no-version" v0.1.5 "$baseline_version" no-package-version
check "a baseline manifest with no version for a package is refused" "1" \
  "$(baseline_preflight "${td}/evidence-no-version" --baseline-tag v0.1.5 \
     --baseline-assets-dir "${td}/baseline-no-version")"
check "  and names the package and what it found" yes \
  "$(said 'the baseline set lists tensorplate-agent at version None, which is not a Debian version')"

# Strictly older, so the pinned tag over the candidate's own package
# versions -- a set retagged rather than rebuilt -- is refused too.
make_assets "${td}/baseline-same-debs" v0.1.5 "$candidate_version"
check "a baseline carrying the candidate's own package versions is refused" "1" \
  "$(baseline_preflight "${td}/evidence-same-debs" --baseline-tag v0.1.5 \
     --baseline-assets-dir "${td}/baseline-same-debs")"
check "  and says the versions are not older" yes \
  "$(said "the baseline's ${candidate_version} is not older than the candidate's ${candidate_version}")"

# A baseline is the published predecessor, not a set somebody built.
for published_case in "snapshot|unreleased=True" "local-provenance|provenance='local-source-snapshot'"; do
  variant="${published_case%%|*}"
  make_assets "${td}/baseline-${variant}" v0.1.5 "$baseline_version" "$variant"
  check "a ${variant} baseline is refused" "1" \
    "$(baseline_preflight "${td}/evidence-baseline-${variant}" --baseline-tag v0.1.5 \
       --baseline-assets-dir "${td}/baseline-${variant}")"
  check "  and says its manifest records a local snapshot" yes "$(said "${published_case#*|}")"
done

cp -R "$baseline" "${td}/baseline-tampered"
printf 'tampered\n' >>"${td}/baseline-tampered/install.sh"
check "a baseline that fails its checksums is refused" "1" \
  "$(baseline_preflight "${td}/evidence-baseline-checksums" --baseline-tag v0.1.5 \
     --baseline-assets-dir "${td}/baseline-tampered")"
check "  and says the set failed verification" yes \
  "$(said 'the baseline artifact set failed verification')"
check "  before anything privileged ran" "" "$(cat "${td}/preflight-sudo.log")"

# The digest check applies to the baseline's checksum file on its own:
# the candidate's hashes normally here.
check "a baseline checksum file whose digest cannot be computed is refused" "1" \
  "$(TP_FAKE_BAD_DIGEST_DIR="$baseline" preflight_path="${td}/bad-digest-bin:" \
     baseline_preflight "${td}/evidence-baseline-bad-digest" --baseline-tag v0.1.5 \
       --baseline-assets-dir "$baseline")"
check "  and names the baseline's checksum file" yes \
  "$(said "could not compute a digest for ${baseline}/SHA256SUMS")"
check "  before anything privileged ran" "" "$(cat "${td}/preflight-sudo.log")"

# --- upgrade regressions.
#
# mode|exit|whether the run ends inside the upgrade's window|reason
for mode_case in "upgrade-keeps-baseline-version|1|no|installed package versions do not match the candidate set" \
                 "upgrade-agent-pid-unchanged|1|no|agent MainPID 100 did not change across the upgrade" \
                 "upgrade-observability-pid-unchanged|1|no|observability MainPID 100 did not change across the upgrade" \
                 "upgrade-resets-conffile|1|no|the upgrade did not keep the operator-edited" \
                 "upgrade-doctor-failing|1|no|doctor reports 1 failing finding(s)" \
                 "upgrade-loses-deployment|1|no|checks failed: active_deployment" \
                 "baseline-agent-inactive|1|no|step failed (exit 1): baseline services ready" \
                 "upgrade-observability-inactive|1|no|step failed (exit 1): services ready after the upgrade" \
                 "baseline-health-wrong|1|no|step failed (exit 1): baseline worker round trip" \
                 "candidate-set-changed|1|no|SHA256SUMS changed after it was verified" \
                 "install-fails-upgraded|1|no|step failed (exit 1): install.sh" \
                 "install-fails-baseline|1|yes|step failed (exit 1): install.sh" \
                 "install-late-fails-baseline|1|yes|step failed (exit 1): install.sh" \
                 "upgrade-purge-leaves-packages|1|yes|TensorPlate packages remain after the purge: tensorplate-common config-files" \
                 "upgrade-dpkg-query-fails-before|2|yes|cannot read package database" \
                 "upgrade-dpkg-query-fails-after|2|yes|cannot read package database"; do
  mode="${mode_case%%|*}"
  rest="${mode_case#*|}"
  expected_status="${rest%%|*}"
  rest="${rest#*|}"
  stranded="${rest%%|*}"
  evidence="${td}/stages-${mode}"
  # A throwaway copy of the candidate set, so the one case that changes
  # SHA256SUMS under the harness cannot reach any other run.
  if [[ "$mode" == candidate-set-changed ]]; then
    rm -rf "${td}/assets-mutable"
    cp -R "$assets" "${td}/assets-mutable"
    candidate_assets="${td}/assets-mutable"
  fi
  check "${mode} fails the run" "$expected_status" "$(run_baseline_stages "$mode" "$evidence" "")"
  check "  crash-loop passed before it" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and upgrade is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" upgrade)"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/upgrade.log" "${rest#*|}")"
  check "  and the rollback never runs on a device the upgrade left broken" absent \
    "$(stage_status "${evidence}/lifecycle-report.json" rollback)"
  check "  and nothing was removed" no \
    "$(grep -Fq 'apt-get remove' "${appliance}/sudo.log" && echo yes || echo no)"
  # Only a run that ends between clearing the candidate and a completed
  # baseline install is reported as stranded; one that ends after it is a
  # device carrying a set, and saying otherwise sends the operator to
  # recover what is not broken.
  check "  and a stranded-device report is filed only inside the window: ${stranded}" "$stranded" \
    "$(stranded_filed "$evidence")"
  check "  and printed only then" "$stranded" "$(err_says "$evidence" 'the upgrade did not finish')"
  case "$mode" in
    upgrade-loses-deployment)
      check "  and no deploy of the harness's own hid the loss" \
        "jetson-lifecycle-smoke jetson-lifecycle-smoke-baseline" \
        "$(awk '{print $1}' "${appliance}/deploy.log" | tr '\n' ' ' | sed 's/ $//')"
      ;;
    baseline-health-wrong)
      check "  on the baseline's own round trip" yes \
        "$(logged "${evidence}/upgrade.log" 'checks failed: serving_health_deployment')"
      check "  and the candidate was never installed over it" 2 \
        "$(grep -cF '/install.sh --local-artifacts ' "${appliance}/sudo.log" || true)"
      ;;
    candidate-set-changed)
      # A set whose checksum file no longer hashes to the digest preflight
      # recorded must not be installed under the identity that
      # verification gave it, so the last install is still the baseline's.
      check "  and the changed candidate was never installed over the baseline" "$baseline" \
        "$(install_order | awk '{print $NF}')"
      candidate_assets="$assets"
      ;;
    install-fails-baseline)
      # An installer whose failure is not checked leaves the stage running
      # against a device that has nothing installed, where a later check
      # fails for a reason that is not the one that happened.
      check "  and the stage stopped rather than checking versions" no \
        "$([[ -e "${evidence}/packages-baseline.txt" ]] && echo yes || echo no)"
      # The upgrade's own window: clear_install purged the packages AND
      # deleted the state directories, and the baseline installer that was
      # to replace them refused before installing anything.
      check "  and the operator is told the device is bare" yes \
        "$(err_says "$evidence" 'this device has NO TensorPlate installed')"
      check "  and that this window deleted the durable state" yes \
        "$(err_says "$evidence" "the clearing step deleted ${cleared_dirs} with the packages, durable state included.")"
      check "  and that the installer ran, and where its output is" yes \
        "$(err_says "$evidence" "the v0.1.5 installer was started and its install was not accepted; its output is in ${evidence}/install-baseline.txt")"
      check "  and how to install the baseline by hand" yes \
        "$(err_says "$evidence" "  sudo bash ${baseline}/install.sh --local-artifacts ${baseline} --yes")"
      check "  with no downgrade warning, since nothing is installed" no \
        "$(err_says "$evidence" 'downgrade')"
      ;;
    install-late-fails-baseline)
      # install.sh fails after installing every package when the services
      # do not come up or doctor reports a critical finding: the device
      # carries the baseline, and the report reads that rather than
      # repeating what the clearing step found.
      check "  and the report names what the installer left" yes \
        "$(err_says "$evidence" 'dpkg lists these TensorPlate packages as present: tensorplate-agent installed')"
      check "  rather than claiming the device is bare" no \
        "$(err_says "$evidence" 'NO TensorPlate installed')"
      check "  and says the installer ran" yes \
        "$(err_says "$evidence" 'the v0.1.5 installer was started')"
      check "  without calling a reinstall of the baseline a downgrade" no \
        "$(err_says "$evidence" 'downgrade')"
      ;;
    upgrade-purge-leaves-packages)
      check "  and the state directories were not deleted after the purge" no \
        "$(sudo_after "$(sudo_line 'apt-get purge')" 'rm -rf /etc/tensorplate')"
      check "  and the report says so" yes \
        "$(err_says "$evidence" "the clearing step stopped before deleting ${cleared_dirs}.")"
      check "  and that the baseline installer never ran" yes \
        "$(err_says "$evidence" 'the v0.1.5 installer was not run.')"
      ;;
    upgrade-dpkg-query-fails-before)
      check "  and nothing was purged" 0 "$(grep -c 'apt-get purge' "${appliance}/sudo.log" || true)"
      check "  and the report names the candidate still installed" yes \
        "$(err_says "$evidence" "dpkg lists these TensorPlate packages as present: ${candidate_present}.")"
      check "  and that installing the baseline over it is a downgrade" yes \
        "$(err_says "$evidence" 'installing v0.1.5 is a downgrade apt-get refuses')"
      check "  and that the state directories are still there" yes \
        "$(err_says "$evidence" "the clearing step stopped before deleting ${cleared_dirs}.")"
      ;;
    upgrade-dpkg-query-fails-after)
      check "  and the state directories were not deleted after the purge" no \
        "$(sudo_after "$(sudo_line 'apt-get purge')" 'rm -rf /etc/tensorplate')"
      check "  and the report reads the purged device as bare" yes \
        "$(err_says "$evidence" 'this device has NO TensorPlate installed')"
      check "  with its state directories not yet deleted" yes \
        "$(err_says "$evidence" "the clearing step stopped before deleting ${cleared_dirs}.")"
      ;;
  esac
done
for injected_case in "install.sh --local-artifacts ${baseline}=install.sh" \
                     ">>=operator edit" \
                     "apt-get purge=purge tensorplate-"; do
  injected="${injected_case%%=*}"
  step_name="${injected_case#*=}"
  evidence="${td}/stages-upgrade-sudo-${step_name//[^a-z]/-}"
  check "a failing '${step_name}' during the upgrade is recorded as a failed upgrade" fail \
    "$(run_baseline_stages ok "$evidence" "$injected" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" upgrade)"
  check "  and the failed step is the one named" yes \
    "$(logged "${evidence}/upgrade.log" "step failed (exit 9): ${step_name}")"
done
# The purge failing leaves the candidate installed and its state in place.
evidence="${td}/stages-upgrade-sudo-purge-tensorplate-"
check "a purge that fails during the upgrade stops there rather than at the leftover check" no \
  "$(logged "${evidence}/upgrade.log" 'remain after the purge')"
check "  and is reported as what it left" yes \
  "$(err_says "$evidence" "dpkg lists these TensorPlate packages as present: ${candidate_present}.")"
check "  not as a bare device" no "$(err_says "$evidence" 'NO TensorPlate installed')"
check "  with its state directories not deleted" yes \
  "$(err_says "$evidence" "the clearing step stopped before deleting ${cleared_dirs}.")"
check "  and the baseline installer never run" yes \
  "$(err_says "$evidence" 'the v0.1.5 installer was not run.')"
check "  and filed" yes "$(stranded_filed "$evidence")"

# --- rollback regressions.
#
# mode|exit|whether the run ends inside the rollback's windows|reason
for mode_case in "rollback-other-active|1|no|the rollback must start from jetson-lifecycle-smoke-baseline" \
                 "rollback-state-aside-exists|1|no|step failed (exit 1): refuse to replace an existing /var/lib/tensorplate/state.bak" \
                 "rollback-keeps-candidate-version|1|no|installed package versions do not match the v0.1.5 set" \
                 "rollback-resets-conffile|1|no|the rollback did not keep the operator-edited" \
                 "rollback-state-not-preserved|1|no|step failed (exit 1): the set-aside state is preserved" \
                 "rollback-empties-backup|1|no|step failed (exit 1): the set-aside state is preserved" \
                 "rollback-truncates-backup|1|no|step failed (exit 1): the set-aside state is preserved" \
                 "rollback-rewrites-backup|1|no|step failed (exit 1): the set-aside state is preserved" \
                 "rollback-digest-not-hex|1|yes|step failed (exit 1): digest the durable state before setting it aside" \
                 "rollback-keeps-state|1|no|it loaded state that was set aside" \
                 "rollback-agent-unavailable|1|no|the agent is not available after the rollback" \
                 "rollback-previous-active|1|no|reports previous_active" \
                 "rollback-agent-inactive|1|no|step failed (exit 1): services ready after the rollback" \
                 "rollback-leaves-package|1|yes|the removal did not leave only conffiles: tensorplate-cli is still installed" \
                 "rollback-leaves-common|1|yes|the removal did not leave only conffiles: tensorplate-common is still installed" \
                 "rollback-common-half-configured|1|yes|the removal did not leave only conffiles: tensorplate-common is still half-configured" \
                 "rollback-purges-conffiles|1|yes|tensorplate-agent is absent, not config-files" \
                 "rollback-purges-observability|1|yes|tensorplate-observability is not-installed, not config-files" \
                 "rollback-removes-apt-source|1|yes|tensorplate-apt-source was installed before the removal and is config-files after it" \
                 "baseline-set-changed|1|yes|SHA256SUMS changed after it was verified" \
                 "install-fails-rolled-back|1|yes|step failed (exit 1): install.sh" \
                 "install-late-fails-rolled-back|1|yes|step failed (exit 1): install.sh" \
                 "rollback-listing-fails|2|yes|cannot read package database" \
                 "listing-fails-after-remove|2|yes|cannot read package database"; do
  mode="${mode_case%%|*}"
  rest="${mode_case#*|}"
  expected_status="${rest%%|*}"
  rest="${rest#*|}"
  stranded="${rest%%|*}"
  evidence="${td}/stages-${mode}"
  if [[ "$mode" == baseline-set-changed ]]; then
    rm -rf "${td}/baseline-mutable"
    cp -R "$baseline" "${td}/baseline-mutable"
    baseline_assets="${td}/baseline-mutable"
  fi
  check "${mode} fails the run" "$expected_status" "$(run_baseline_stages "$mode" "$evidence" "")"
  check "  upgrade passed before it" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" upgrade)"
  check "  and rollback is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" rollback)"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/rollback.log" "${rest#*|}")"
  check "  and a stranded-device report is filed only inside the windows: ${stranded}" "$stranded" \
    "$(stranded_filed "$evidence")"
  check "  and printed only then" "$stranded" "$(err_says "$evidence" 'the rollback did not finish')"
  case "$mode" in
    rollback-other-active|rollback-state-aside-exists)
      # A precondition is only a precondition if it is checked before the
      # device is touched.
      check "  and nothing was stopped, set aside or removed after the upgrade" "no no no" \
        "$(third="$(install_line 3)"
           echo "$(sudo_after "$third" 'systemctl stop tensorplate-agent tensorplate-observability')" \
             "$(sudo_after "$third" 'mv -T')" "$(sudo_after "$third" 'apt-get remove')")"
      ;;
    rollback-leaves-package|rollback-leaves-common|rollback-common-half-configured)
      case "$mode" in
        rollback-leaves-package) left='tensorplate-cli installed' ;;
        rollback-leaves-common) left='tensorplate-common installed' ;;
        *) left='tensorplate-common half-configured' ;;
      esac
      check "  and the last install was still the candidate's" "$assets" \
        "$(install_order | awk '{print $NF}')"
      # The report is about what dpkg says, not about what the harness
      # asked apt-get to do. Telling an operator the device is bare while
      # a newer package is still there sends them to a command apt refuses.
      check "  and the report names the package left behind" yes \
        "$(err_says "$evidence" "dpkg lists these TensorPlate packages as present: ${left}.")"
      check "  rather than claiming the device is bare" no \
        "$(err_says "$evidence" 'NO TensorPlate installed')"
      check "  and says the baseline installer was not run" yes \
        "$(err_says "$evidence" 'the v0.1.5 installer was not run.')"
      check "  and warns that installing over it is a downgrade" yes \
        "$(err_says "$evidence" 'installing v0.1.5 is a downgrade apt-get refuses')"
      check "  and that the conffiles and state are where the procedure put them" "yes yes" \
        "$(err_says "$evidence" '/etc/tensorplate conffiles are kept.') $(err_says "$evidence" 'durable state is at /var/lib/tensorplate/state.bak.')"
      ;;
    rollback-digest-not-hex)
      # A leading field that is not sha256 hex is refused where it is
      # read, not carried forward: two files digested through such a
      # sha256sum would otherwise compare equal and the stage would
      # credit the rollback with preserving state it never read.
      check "  and names the file it could not digest" yes \
        "$(logged "${evidence}/rollback.log" \
           'could not compute a sha256 of /var/lib/tensorplate/state/state.json')"
      check "  with nothing set aside or removed" "no no" \
        "$(third="$(install_line 3)"
           echo "$(sudo_after "$third" 'mv -T')" "$(sudo_after "$third" 'apt-get remove')")"
      ;;
    rollback-state-not-preserved)
      # Nothing is there to read: the check names the file it could not
      # digest rather than reporting it as changed.
      check "  and names the file it could not read" yes \
        "$(logged "${evidence}/rollback.log" \
           'sha256sum: /var/lib/tensorplate/state.bak/state.json: No such file or directory')"
      ;;
    rollback-empties-backup|rollback-truncates-backup|rollback-rewrites-backup)
      # The pathname is still a regular file in all three, so `test -f`
      # on it would have passed every one of them.
      check "  and the backup is still a regular file" yes \
        "$([[ -f "${appliance}/varlib/state.bak/state.json" ]] && echo yes || echo no)"
      check "  and the failure names the file whose contents did not survive" yes \
        "$(logged "${evidence}/rollback.log" \
           'the rollback did not preserve /var/lib/tensorplate/state.bak/state.json')"
      check "  and reports the digest the stopped agent's copy had" yes \
        "$(logged "${evidence}/rollback.log" "sha256 was ${state_sha256} when the services were stopped")"
      # The checks after it intentionally do not load the backup, so they
      # cannot stand in for this one: the stage has to stop here.
      check "  and the stage stopped before reading the agent back" no \
        "$(logged "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-purges-conffiles|rollback-purges-observability)
      if [[ "$mode" == rollback-purges-conffiles ]]; then lost=tensorplate-agent; else lost=tensorplate-observability; fi
      check "  and the report names the conffiles that were lost" yes \
        "$(err_says "$evidence" "the removal did NOT keep the /etc/tensorplate conffiles of: ${lost}.")"
      check "  rather than claiming they are kept" no \
        "$(err_says "$evidence" 'conffiles are kept')"
      ;;
    baseline-set-changed)
      check "  and the changed baseline was never installed" "$assets" \
        "$(install_order | awk '{print $NF}')"
      check "  and the report says the installer was not run" yes \
        "$(err_says "$evidence" 'the v0.1.5 installer was not run.')"
      baseline_assets="$baseline"
      ;;
    install-fails-rolled-back)
      # The device is bare: the removal completed and the baseline did not
      # install. The harness says so and reinstalls nothing by itself.
      check "  and reports the bare device" yes \
        "$(err_says "$evidence" 'this device has NO TensorPlate installed')"
      check "  and the kept conffiles and the set-aside state" "yes yes" \
        "$(err_says "$evidence" '/etc/tensorplate conffiles are kept.') $(err_says "$evidence" 'durable state is at /var/lib/tensorplate/state.bak.')"
      check "  and that the installer ran, and where its output is" yes \
        "$(err_says "$evidence" "the v0.1.5 installer was started and its install was not accepted; its output is in ${evidence}/install-rollback.txt")"
      check "  and hands the operator the command rather than attempting it" "4 yes" \
        "$(grep -cF '/install.sh --local-artifacts ' "${appliance}/sudo.log" || true) $(err_says "$evidence" "  sudo bash ${baseline}/install.sh --local-artifacts ${baseline} --yes")"
      # Re-running the harness is offered as a recovery, and its install
      # stage deletes the state.bak the lines above point the operator at.
      check "  and says what re-running this harness would cost" yes \
        "$(err_says "$evidence" "its install stage deletes ${cleared_dirs} first, including any /var/lib/tensorplate/state.bak")"
      ;;
    install-late-fails-rolled-back)
      check "  and the report names what the installer left" yes \
        "$(err_says "$evidence" 'dpkg lists these TensorPlate packages as present: tensorplate-agent installed')"
      check "  rather than claiming the device is bare" no \
        "$(err_says "$evidence" 'NO TensorPlate installed')"
      check "  and says the installer ran" yes "$(err_says "$evidence" 'the v0.1.5 installer was started')"
      check "  without calling a reinstall of the baseline a downgrade" no \
        "$(err_says "$evidence" 'downgrade')"
      ;;
    rollback-listing-fails)
      # Stopped and set aside, then the database stopped answering: the
      # candidate is still installed as far as anything knows.
      check "  and nothing was removed" no \
        "$(grep -Fq 'apt-get remove' "${appliance}/sudo.log" && echo yes || echo no)"
      check "  and the report says where the rollback stopped" yes \
        "$(err_says "$evidence" "the rollback did not finish: it had started stopping the candidate's services and had not removed any package.")"
      check "  and that the listing could not be read, with the query to run" "yes yes" \
        "$(err_says "$evidence" 'what this device carries could NOT be read') $(err_says "$evidence" "dpkg-query -W -f='\${binary:Package} \${db:Status-Status}\\n' 'tensorplate*'")"
      check "  and how to return to the candidate, state first" "yes yes" \
        "$(err_says "$evidence" '  sudo mv -T /var/lib/tensorplate/state.bak /var/lib/tensorplate/state') $(err_says "$evidence" '  sudo systemctl start tensorplate-agent tensorplate-observability')"
      ;;
    listing-fails-after-remove)
      check "  and the report says the listing could not be read" yes \
        "$(err_says "$evidence" 'what this device carries could NOT be read')"
      check "  nor whether the conffiles were kept" yes \
        "$(err_says "$evidence" 'whether the removal kept the /etc/tensorplate conffiles was NOT read.')"
      check "  and installs nothing until the listing says it may" yes \
        "$(err_says "$evidence" 'once that listing shows no newer TensorPlate package installed, install v0.1.5 by hand')"
      ;;
  esac
done

for injected_case in "systemctl stop tensorplate-agent tensorplate-observability=stop the services" \
                     "sha256sum /var/lib/tensorplate/state/state.json=digest the durable state before setting it aside" \
                     "mv -T=set durable state aside" \
                     "apt-get remove=remove tensorplate-"; do
  injected="${injected_case%%=*}"
  step_name="${injected_case#*=}"
  evidence="${td}/stages-rollback-sudo-${step_name// /-}"
  check "a failing '${step_name}' during the rollback is recorded as a failed rollback" fail \
    "$(run_baseline_stages ok "$evidence" "$injected" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" rollback)"
  check "  and the failed step is the one named" yes \
    "$(logged "${evidence}/rollback.log" "step failed (exit 9): ${step_name}")"
  check "  and the candidate is reported still installed" yes \
    "$(err_says "$evidence" "dpkg lists these TensorPlate packages as present: ${candidate_present}.")"
  check "  and filed" yes "$(stranded_filed "$evidence")"
  if [[ "$step_name" == "remove tensorplate-" ]]; then
    # The removal failed, so nothing read what it left of the conffiles;
    # the state was already set aside.
    check "  and the report says where the rollback stopped" yes \
      "$(err_says "$evidence" 'the rollback did not finish: it had started removing the candidate')"
    check "  and that the conffiles were not read" yes \
      "$(err_says "$evidence" 'whether the removal kept the /etc/tensorplate conffiles was NOT read.')"
    check "  and that the baseline installer never ran" yes \
      "$(err_says "$evidence" 'the v0.1.5 installer was not run.')"
    check "  and that installing the baseline over the candidate is a downgrade" yes \
      "$(err_says "$evidence" 'installing v0.1.5 is a downgrade apt-get refuses')"
    check "  and where the state is" yes \
      "$(err_says "$evidence" 'durable state is at /var/lib/tensorplate/state.bak.')"
  else
    # Stopped, and perhaps not set aside: the way back is to start the
    # candidate again, with no state to move back.
    check "  and the report says the rollback removed nothing" yes \
      "$(err_says "$evidence" "the rollback did not finish: it had started stopping the candidate's services and had not removed any package.")"
    check "  and that the state was not moved" yes \
      "$(err_says "$evidence" 'durable state is still at /var/lib/tensorplate/state.')"
    check "  and how to start the candidate again, with no state to move back" "yes no" \
      "$(err_says "$evidence" '  sudo systemctl start tensorplate-agent tensorplate-observability') $(err_says "$evidence" '  sudo mv -T')"
  fi
done

# A terminal that goes away while the device is stranded -- an SSH
# session dropping -- fails every write to the harness's stderr, and the
# harness's EXIT handler runs under errexit. The lifecycle report must
# still be written, the private CLI config still removed, and the
# stranded-device report must still reach the evidence. The fixture
# installer fails only once the reader has closed the terminal.
evidence="${td}/stages-stderr-lost"
fifo="${td}/stderr-lost.fifo"
closed="${td}/stderr-lost.closed"
rm -f "$fifo" "$closed"
mkfifo "$fifo"
python3 - "$fifo" "${td}/stderr-lost.seen" "$closed" <<'PY' &
import sys

fifo, seen, closed = sys.argv[1:]
with open(fifo, encoding="utf-8", errors="replace") as stream, \
        open(seen, "w", encoding="utf-8") as out:
    for line in stream:
        out.write(line)
        if line.startswith("== stage rollback"):
            break
open(closed, "w").close()
PY
reader=$!
check "a terminal lost inside the rollback's window still fails the run" 1 \
  "$(stages_stderr="$fifo" stages_stderr_closed="$closed" \
     run_baseline_stages install-fails-rolled-back "$evidence" "")"
wait "$reader" || true
check "  the terminal was gone before the report" no \
  "$(grep -Fq 'did not finish' "${td}/stderr-lost.seen" && echo yes || echo no)"
check "  and the rollback is still recorded as a failure" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" rollback)"
check "  the stranded-device report is still filed" yes \
  "$(grep -Fq 'this device has NO TensorPlate installed' "${evidence}/stranded-device.txt" 2>/dev/null \
     && echo yes || echo no)"
check "  and the private CLI config is still removed" "" \
  "$(find "${appliance}/scratch" -name cli.json -print -quit)"

# PYTHONOPTIMIZE strips every Python assert, and several of the harness's
# checks are asserts: wrong-row is caught only by one, since doctor exits
# 0 there, and so is a degraded status in status-logs. The harness drops
# the variable, so each of these still fails; the rollback's own checks
# are not asserts at all.
for optimized_case in "wrong-row|install|platform_row is warning" \
                      "statuslogs-degraded|status-logs|AssertionError: degraded" \
                      "rollback-keeps-state|rollback|it loaded state that was set aside"; do
  mode="${optimized_case%%|*}"
  rest="${optimized_case#*|}"
  stage="${rest%%|*}"
  evidence="${td}/stages-optimized-${mode}"
  check "${mode} still fails with PYTHONOPTIMIZE set" 1 \
    "$(PYTHONOPTIMIZE=1 run_baseline_stages "$mode" "$evidence" "")"
  check "  and ${stage} is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" "$stage")"
  check "  for the reason the case provokes" yes "$(logged "${evidence}/${stage}.log" "${rest#*|}")"
done

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "verify_jetson_lifecycle: ok" || echo "${failures} check(s) failed")"
exit "$failures"
