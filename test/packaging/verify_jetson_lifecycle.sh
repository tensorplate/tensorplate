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
                    "crash-loop", "offline", "upgrade", "rollback"}, sorted(run)
# upgrade and rollback are run or skipped depending on whether a baseline
# set was supplied, so each appears once in each list. Nothing else is
# ever skipped: the harness covers all eight canonical stages, and a run
# with a baseline set is the only one that can report a pass.
assert set(skipped) == {"upgrade", "rollback"}, sorted(skipped)
assert set(run) & set(skipped) == {"upgrade", "rollback"}, sorted(set(run) & set(skipped))

# Every skip states a reason: an unexplained skip is indistinguishable
# from a stage nobody thought about. The conditional pair names the
# options that run them instead.
reasons = {}
for stage in skipped:
    match = re.search(
        r"lifecycle_skip\s+" + re.escape(stage) + r"\s*\\\n\s*\"([^\"]+)\"", body
    )
    assert match, f"{stage} is skipped without a quoted reason"
    reasons[stage] = match.group(1)
    assert len(match.group(1)) > 40, f"{stage}'s skip reason is too thin: {match.group(1)}"
for stage in ("upgrade", "rollback"):
    for option in ("--baseline-tag", "--baseline-assets-dir"):
        assert option in reasons[stage], \
            f"{stage}'s skip reason does not name {option}, which runs it"

# Order, and it is load-bearing in both directions. offline is about the
# candidate install the stages above exercise and needs the deployment
# they left serving, so it follows crash-loop; upgrade's clear_install
# purges that install and deletes /var/lib/tensorplate, so offline has to
# stay ahead of it. Rollback returns from what upgrade left, so it
# follows.
positions = {
    "crash-loop": re.search(r"^\s*lifecycle_stage\s+crash-loop\s", body, re.M),
    "offline": re.search(r"^\s*lifecycle_stage\s+offline\s", body, re.M),
    "upgrade": re.search(r"^\s*lifecycle_stage\s+upgrade\s", body, re.M),
    "rollback": re.search(r"^\s*lifecycle_stage\s+rollback\s", body, re.M),
}
assert all(positions.values()), positions
assert (positions["crash-loop"].start() < positions["offline"].start()
        < positions["upgrade"].start() < positions["rollback"].start()), \
    "the stages must run in the order crash-loop, offline, upgrade, rollback"

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
print("stage coverage: all eight run with a baseline, offline between crash-loop and "
      "upgrade, digest recorded after install")
PY

# --- the offline stage's own rules, read from the harness text.
#
# Several of them cannot be driven from a stub run because they are about
# what the harness may never do, and one absence looks like any other.
python3 - "$harness" "${repo_root}/tools/validation/linux_offline_runtime.py" <<'PY'
import re, sys

harness_path, module_path = sys.argv[1:]
body = open(harness_path, encoding="utf-8").read()
module = open(module_path, encoding="utf-8").read()

# The mechanism is shared with the Ubuntu cloud harness, not copied: the
# drop-in text, the transient unit's properties and the classification
# come from the module, so both harnesses run the same rule.
assert "linux_offline_runtime.py" in body, \
    "the harness does not use the shared offline-runtime module"
# Naming the properties to read them back is fine; giving one a value is
# a second copy of the rule that can drift from the module's.
spelled = re.search(r"IPAddress(?:Allow|Deny)=(?:any|localhost|[0-9A-Fa-f][0-9A-Fa-f:.]*)", body)
assert not spelled, (
    "the harness spells the address policy itself instead of taking it from the module: "
    + (spelled.group(0) if spelled else ""))

# Runtime drop-ins only. A path under /etc would outlive the run and the
# device's next reboot.
assert "/run/systemd/system" in module, "the module does not write runtime unit files"
for forbidden in re.findall(r"/etc/systemd/system\S*", body):
    raise AssertionError(f"the harness names a persistent unit path: {forbidden}")

def function(name):
    match = re.search(r"^" + name + r"\(\) \{\n.*?^\}\n", body, re.M | re.S)
    assert match, f"{name} is missing"
    return match.group(0)

def logical_lines(text):
    r"""One logical line at a time: backslash continuations joined, whole
    comment lines dropped. Matched over raw text, `\s` spans newlines, so
    a nearby comment could otherwise satisfy an assertion about a call."""
    joined = re.sub(r"\\\n\s*", " ", text)
    return [line.strip() for line in joined.splitlines()
            if line.strip() and not line.strip().startswith("#")]

stage = "".join(function(name) for name in
                ("stage_offline", "stage_offline_in", "offline_cli_under_denial"))
stage_lines = logical_lines(stage)

# No stage function spells the CLI itself. On this row every CLI call is
# pinned to a private configuration, and the denied transient unit and
# that pin both live in run_denied_cli: a call written here would have
# neither, and an operator profile could redirect it.
for line in stage_lines:
    assert not re.search(r"(?<![\w.-])tensorplate\s", line), \
        f"an offline-stage CLI call is not run through run_denied_cli: {line!r}"

# Exactly the calls the module files probes for, each once and in its
# order: the certificate reads offline-cli-probe-<call>.json back for
# every name in CLI_CALLS, so a stage that made fewer could not produce
# it and one that made more would file a probe nothing reads.
module_calls = re.findall(
    r'"([a-z-]+)"',
    re.search(r"^CLI_CALLS = \((.*?)\)", module, re.M | re.S).group(1))
assert module_calls, "the module declares no CLI calls"
calls = []
for line in stage_lines:
    found = (re.search(r'run_denied_cli_out\s+"\S+"\s+([a-z-]+)', line)
             or re.search(r'run_denied_cli\s+([a-z-]+)', line))
    if found:
        calls.append(found.group(1))
assert calls == module_calls, \
    f"the offline stage makes {calls}, not the module's {module_calls}"

# The wrapper those calls go through: the denied unit, the module's
# run-denied probe of it, the private config, and this row's metadata
# answer.
wrapper = " ".join(logical_lines(function("run_denied_cli")))
assert "run_denied python3" in wrapper and "run-denied --call" in wrapper, \
    "run_denied_cli no longer runs the module's run-denied in a denied transient unit"
assert '-- tensorplate --config "$CLI_CONFIG"' in wrapper, \
    "the CLI call made under the denial is not pinned to this run's private config"

# This row has no metadata service, so every probe and control is taken
# with the metadata operations omitted and every classification REQUIRES
# them absent. Without the second half a metadata operation that quietly
# vanished from a document would read as one that passed.
offline = "".join(function(name) for name in (
    "run_denied_cli", "offline_unit_probe", "offline_unit_classify",
    "offline_cli_under_denial", "stage_offline_in"))
# Every way the module is asked to send anything: the two transient-unit
# subcommands, the pair run inside a service's control group -- whose
# name the harness builds from `kind` -- and the CLI wrapper's.
takes_a_probe = re.compile(
    r'"\$OFFLINE_HELPER"\s+(?:"\$\{kind\}-unit"|probe|control|run-denied)(?![\w-])')
probes = classifications = 0
for line in logical_lines(offline):
    if takes_a_probe.search(line):
        probes += 1
        assert "--metadata-address none" in line, \
            f"an offline probe is taken with a metadata service this row has none of: {line!r}"
    if re.search(r"offline_helper\s+classify(?![\w-])", line):
        classifications += 1
        assert "--metadata-operation absent" in line, \
            f"an offline classification does not require the metadata operations absent: {line!r}"
assert probes == 4, \
    f"expected the control, the denied probe, the in-service pair and the CLI wrapper, found {probes}"
assert classifications == 2, \
    f"expected the in-service and the transient classification, found {classifications}"

# And nothing the harness RUNS restates the cloud row's identity
# mechanism. A Jetson has no metadata service and writes no boot-bound
# machine-type record, so an assertion about one would certify a fallback
# path that never runs on this device. Read from the code alone: the
# comments that say this row has none of it are not what would assert it.
code = [line for line in body.splitlines() if not line.lstrip().startswith("#")]
for forbidden in ("169.254.169.254", "machine-type.json", "recorded_gce_metadata"):
    offenders = [line.strip() for line in code if forbidden in line]
    assert not offenders, \
        f"the harness reaches for the Compute Engine row's identity mechanism: {offenders}"
identity = " ".join(logical_lines(function("stage_offline_in")))
assert "--expect-source none" in identity, \
    "the offline stage does not require the agent to establish no machine type"
doctor = " ".join(logical_lines(function("offline_cli_under_denial")))
assert "--forbid-host-os-phrase 'GCE metadata'" in doctor, \
    "the offline stage does not forbid a GCE machine type in doctor's host_os"
assert "--host-os-phrase 'L4T '" in doctor, \
    "the offline stage does not require doctor's host_os to keep naming the L4T release"

# The control runs before the denial is applied. Reversed, a refusal
# under the denial would be attributable to nothing.
control = stage.index("control probe")
deny = stage.index("offline_deny ||")
assert control < deny, "the offline stage denies the network before running its control"
# And each service's own control, taken inside its control group, before
# the denial too; its probe after the readback that proved the services
# were replaced under it.
for call in ("offline_unit_probe control", "offline_unit_probe probe",
             "offline_check_policy denied"):
    assert call in stage, f"the offline stage no longer calls {call}"
unit_control = stage.index("offline_unit_probe control")
unit_probe = stage.index("offline_unit_probe probe")
readback = stage.index("offline_check_policy denied")
assert unit_control < deny, \
    "the offline stage denies the network before taking the in-service controls"
assert readback < unit_probe, \
    "the offline stage probes the services before reading back that they were replaced"

# The control's allowed loopback port comes from a status taken after the
# last thing that respawned the worker. status.json is the status-logs
# capture, three stages and two respawns earlier; crash-loop is the stage
# immediately before this one and files its recovery status.
port_source = re.search(r'serving-port\s+--status "\$\{EVIDENCE_DIR\}/(\S+?)"',
                        re.sub(r"\\\n\s*", " ", stage))
assert port_source, "the offline stage does not take its control port from a status capture"
assert port_source.group(1) == "status-after-crash-loop.json", (
    f"the offline control's loopback port comes from {port_source.group(1)}, "
    "which predates the restart and crash-loop stages' worker respawns")

# The agent journal is captured before the stage's own CLI calls. The
# capture is a bounded tail of the invocation's records and the identity
# line is written once at start, so a deploy, two status calls, an
# inference and a probe in between can push it out of the window.
capture = stage.index("capture_current_journal")
cli_calls = stage.index('offline_cli_under_denial "$work" || return')
assert capture < cli_calls, (
    "the offline stage captures the agent journal after its CLI calls, so the "
    "start-up identity line can fall outside the tail it reads")

# The denial is put back from the exit and signal handler as well as from
# the stage, and before the crash-loop restore: a device left denied
# cannot reach anything the operator would recover it with.
handler = function("finish_with_cleanup")
assert "cleanup_offline_denial" in handler, \
    "an interrupted run does not remove the network denial it installed"
assert handler.index("cleanup_offline_denial") < handler.index("cleanup_crash_loop"), \
    "the exit handler restores the agent config before it restores the network policy"
print("offline stage: shared module, runtime drop-ins only, every CLI call denied and "
      "pinned, control first, metadata absent on this row")
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
    # Named as the release publishes it: GitHub has no `~`, so the release
    # build stages a 0.2.1~rc.2-1 package as 0.2.1.rc.2-1. Its control
    # Version keeps the tilde.
    name = f"{package}_{deb_version.replace('~', '.')}_{arch}.deb"
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
    artifacts.append({"file": f"tensorplate-cli_{version.replace('~', '.')}_arm64.deb",
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
# The interpreter itself, not whatever wrapper is first on PATH: a
# version-manager shim re-execs through a directory it prepends to PATH,
# which would shadow the stub below and let the harness reach the real
# interpreter for every call after the first.
real_python="$(python3 -c 'import sys; print(sys.executable)')"

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

# python3, with the offline module's probing subcommands routed to a
# fixture. Every other Python invocation -- the harness's own heredocs,
# the bundle generator's, and every other subcommand of the offline
# module, including the drop-in lint, the policy readback, the
# classification and the certificate -- still runs the real interpreter
# on the real file.
#
# The interpreter is baked in rather than read from the environment: this
# stub is on PATH for the bundle generator and for preflight as well, and
# one of those running without the variable set would break every Python
# call in the suite.
{
  printf '#!/bin/sh\n'
  printf 'real_python=%s\n' "$real_python"
  cat <<'STUB'
case "${1:-}" in
  */linux_offline_runtime.py)
    case "${2:-}" in
      probe|control|probe-unit|control-unit|run-denied)
        TP_OFFLINE_MODULE_DIR=$(dirname "$1")
        export TP_OFFLINE_MODULE_DIR
        shift
        exec "$real_python" "$TP_FAKE_OFFLINE_PROBE_STUB" "$@"
        ;;
    esac
    ;;
esac
exec "$real_python" "$@"
STUB
} >"${stub_bin}/python3"

# The probes and their controls, with the socket calls replaced by the
# filter systemd would have installed, answering the way a Linux kernel
# does: a datagram is refused with EPERM, and a TCP connect whose SYN the
# filter dropped reports nothing and times out (net/ipv4/tcp_output.c,
# tcp_connect, returns only -ECONNREFUSED from a transmit).
#
# For a transient unit the allow list comes from the properties the
# harness actually passed to systemd-run, not from a list kept here, so a
# harness that stopped passing them -- or passed the `localhost`
# shorthand -- is caught by the probe's own outcomes rather than by a grep
# over the sudo log. For a probe inside a service's control group it
# comes from the drop-in that service's running instance was started
# under, which the sudo stub snapshots at each start.
export TP_FAKE_OFFLINE_PROBE_STUB="${td}/offline-probe.py"
cat >"$TP_FAKE_OFFLINE_PROBE_STUB" <<'PY'
import errno, ipaddress, os, socket, sys

sys.path.insert(0, os.environ["TP_OFFLINE_MODULE_DIR"])
import linux_offline_runtime as m

MODE = os.environ.get("TP_FAKE_MODE", "ok")
IN_UNIT = sys.argv[1] in ("probe-unit", "control-unit")
# The probe a CLI call's own transient unit takes before the call, which
# then execs the stub CLI for real.
IN_CLI = sys.argv[1] == "run-denied"


def option(name):
    arguments = sys.argv[2:]
    return arguments[arguments.index(name) + 1]


# This row has no metadata service. A probe taken as though it had one
# would make its control require an answer nothing on this device can
# give, and the stage would fail as a network fault rather than as the
# mistake it is -- so the fixture refuses it by name.
if "--metadata-address" not in sys.argv[2:] or option("--metadata-address") != "none":
    raise SystemExit(
        "fixture: this row has no metadata service; every probe must pass "
        "--metadata-address none")


def expand(tokens):
    networks = []
    for token in tokens:
        if token == "localhost":
            # systemd's shorthand, expanded the way systemd expands it.
            networks += [ipaddress.ip_network("127.0.0.0/8"), ipaddress.ip_network("::1/128")]
        else:
            networks.append(ipaddress.ip_network(token))
    return networks


if IN_UNIT:
    UNIT = option("--unit")
    running = os.path.join(os.environ["TP_OFFLINE_UNIT_ROOT"], "generation", UNIT + ".running")
    DENIED = os.path.exists(running)
    tokens = []
    if DENIED:
        with open(running, encoding="utf-8") as handle:
            tokens = [line.split("=", 1)[1].strip() for line in handle
                      if line.startswith("IPAddressAllow=")]
    # systemd's best-effort attach failing for one service: configured,
    # restarted, and sending freely.
    if MODE == "offline-service-filter-not-attached" and UNIT == "tensorplate-observability":
        DENIED = False
    ALLOW = expand(tokens)
else:
    UNIT = ""
    DENIED = os.environ.get("TP_FAKE_DENIED") == "1"
    ALLOW = expand(os.environ.get("TP_FAKE_ALLOW", "").split())
    # systemd's best-effort attach failing for one CLI call's unit only:
    # created with the denial's properties -- the stub CLI would say it
    # ran denied -- and sending freely.
    if IN_CLI and MODE == "offline-cli-filter-not-attached-" + option("--call"):
        DENIED = False


def blocked(host):
    address = ipaddress.ip_address(host)
    return DENIED and not any(address in net for net in ALLOW) \
        and MODE != "offline-probe-leaks"


def refused_control():
    # A host firewall refusing a control, which makes the probe's own
    # refusal attributable to nothing: the transient control, or only
    # the control taken inside one service.
    if DENIED:
        return False
    return MODE == "offline-control-refused" or (
        MODE == "offline-unit-control-refused" and UNIT == "tensorplate-agent")


joined = []
dropped = []


def udp_send(host, port, family=socket.AF_INET):
    # SystemExit, not an exception the probe would record as an outcome.
    if IN_UNIT and not (joined and dropped):
        raise SystemExit("fixture: a service's probe sent before joining its group and dropping root")
    if blocked(host) or refused_control():
        raise OSError(errno.EPERM, "Operation not permitted")


def tcp_connect(host, port, family=socket.AF_INET):
    if IN_UNIT or IN_CLI:
        raise SystemExit("fixture: a datagram-only probe opened a TCP connection")
    if blocked(host):
        raise socket.timeout("timed out")
    if refused_control():
        raise OSError(errno.EPERM, "Operation not permitted")


def unix_connect(path):
    # AF_UNIX is not IP: the address filter never sees it, and the agent
    # socket has to keep working under the denial.
    if MODE == "offline-agent-socket-unreachable":
        raise OSError(errno.ECONNREFUSED, "Connection refused")


def join_control_group(control_group, path):
    # The module has already refused any group that is not this unit's,
    # under the fixture's cgroup root; what is checked here is that the
    # group is the one systemctl reported for it, and that nothing is
    # probed before the join.
    expected = "/system.slice/{}.service".format(UNIT)
    if control_group != expected or path != os.path.join(
            os.environ["TP_OFFLINE_CGROUP_ROOT"], expected.lstrip("/")):
        raise m.CheckFailed("fixture: joined {} at {}".format(control_group, path))
    joined.append(control_group)


def drop_privileges(uid, gid, ops=None):
    if not joined:
        raise m.CheckFailed("fixture: privileges dropped before joining a control group")
    if (uid, gid) != (os.getuid(), os.getgid()):
        raise m.CheckFailed("fixture: the probe would drop to {}:{}, not the operator".format(
            uid, gid))
    dropped.append((uid, gid))


m.udp_send = udp_send
m.tcp_connect = tcp_connect
m.unix_connect = unix_connect
m.join_control_group = join_control_group
m.drop_privileges = drop_privileges
m.child_udp_send = lambda: m.attempt(lambda: udp_send("192.0.2.1", 9))
# The transient control's child exiting non-zero: it sent nothing, so it
# cannot be the baseline the child's refusal is classified against.
if MODE == "offline-control-child-fails" and not DENIED and not IN_UNIT:
    m.child_udp_send = lambda: "child_exit_3"


# A probe that crashes under the denial, with a message that quotes the
# module's own path -- the checkout's, which is what an unguarded
# traceback would have put into the stage log.
def crash(*args, **kwargs):
    raise RuntimeError("the probe crashed in " + os.path.abspath(m.__file__))


if MODE == "offline-probe-crashes" and DENIED:
    m.run_probe = crash
    m.run_unit_probe = crash
if MODE == "offline-cli-probe-crashes" and DENIED and IN_CLI:
    m.run_unit_probe = crash
sys.exit(m.main())
PY

# Present so preflight's `command -v` finds it, and loud if it is ever
# invoked directly: a transient unit that denies the network has to be
# started with privilege, and one started without it would deny nothing.
cat >"${stub_bin}/systemd-run" <<'STUB'
#!/bin/sh
printf 'systemd-run must be run under sudo\n' >&2
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
#
# An optional `@denied` or `@restored` suffix narrows the injection to
# one side of the offline stage, for the commands that stage takes and an
# earlier one takes too, spelled exactly the same way: `systemctl restart
# tensorplate-agent tensorplate-observability` is the restart stage's
# command as well, and `systemctl daemon-reload` and `journalctl` are
# taken before the denial too. A substring alone would always fail the
# earlier stage and never reach this one. `denied` is while the agent's
# runtime drop-in is installed, `restored` is after this run removed one
# it had installed.
if [ -n "${TP_FAKE_SUDO_FAIL:-}" ]; then
  sudo_fail_when=""
  sudo_fail_match="${TP_FAKE_SUDO_FAIL}"
  case "${TP_FAKE_SUDO_FAIL}" in
    *@denied) sudo_fail_when=denied; sudo_fail_match="${TP_FAKE_SUDO_FAIL%@denied}" ;;
    *@restored) sudo_fail_when=restored; sudo_fail_match="${TP_FAKE_SUDO_FAIL%@restored}" ;;
  esac
  agent_conf="${TP_OFFLINE_UNIT_ROOT:-/nowhere}/run/systemd/system/tensorplate-agent.service.d/10-tensorplate-validation-offline.conf"
  case "$*" in
    *"${sudo_fail_match}"*)
      case "$sudo_fail_when" in
        "") exit 9 ;;
        denied) [ -f "$agent_conf" ] && exit 9 ;;
        restored)
          [ -f "${TP_OFFLINE_UNIT_ROOT:-/nowhere}/denial-was-installed" ] &&
            [ ! -f "$agent_conf" ] && exit 9 ;;
      esac
      ;;
  esac
fi
# That this run installed a denial drop-in at all, which is what tells
# the restore-side restart apart from every restart taken before the
# stage. Recorded after the injection, so a failed install is not one.
if [ -n "${TP_OFFLINE_UNIT_ROOT:-}" ]; then
  case "$*" in
    "install -D -m 0644 "*10-tensorplate-validation-offline.conf)
      : >"${TP_OFFLINE_UNIT_ROOT}/denial-was-installed" ;;
  esac
fi
# A sudo policy or PAM environment that hands every privileged command a
# verification opt-out the harness's own environment did not carry.
if [ "${TP_FAKE_MODE:-ok}" = sudo-passes-allow-unsigned ]; then
  TP_INSTALL_ALLOW_UNSIGNED=1
  export TP_INSTALL_ALLOW_UNSIGNED
fi
# The offline stage's transient units. Everything before `--` is
# systemd-run's; what follows is the command, which is run here so the
# stage's CLI calls and its probe actually execute. TP_FAKE_DENIED says
# whether the denial properties were passed, so a harness that stopped
# passing them is caught by the probe coming back unrefused rather than
# by a grep over this log.
if [ "$1" = systemd-run ]; then
  denied=0
  allow=""
  user=""
  groups=""
  flags=""
  shift
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --) shift; break ;;
      --property=IPAddressDeny=*) denied=1; shift ;;
      --property=IPAddressAllow=*) allow="${allow}${1#--property=IPAddressAllow=} "; shift ;;
      --property=User=*) user="${1#--property=User=}"; shift ;;
      --property=SupplementaryGroups=*) groups="${1#--property=SupplementaryGroups=}"; shift ;;
      --pipe|--wait|--collect) flags="${flags}${1} "; shift ;;
      *) shift ;;
    esac
  done
  [ "$#" -gt 0 ] || exit 9
  # Who each transient unit runs as, and how its status comes back.
  # Recorded rather than ignored: a unit started without these runs the
  # CLI as root, with root's HOME and without the operator's membership
  # of the agent's control group, which is a different call from the one
  # this run says the operator makes -- and one started without --wait
  # would report systemd-run's exit status instead of the command's.
  printf '%s|%s|%s\n' "$user" "$groups" "$flags" >>"${TP_FAKE_TRANSIENT_LOG:-/dev/null}"
  case "${TP_FAKE_MODE:-ok}" in
    offline-transient-not-denied) denied=0; allow="" ;;
  esac
  # Interrupted with the device denied: the signal handlers have to take
  # the same cleanup path a failed stage does.
  if [ "$denied" = 1 ]; then
    case "${TP_FAKE_MODE:-ok}" in
      offline-signal-int) kill -INT "$PPID" ;;
      offline-signal-term) kill -TERM "$PPID" ;;
      offline-signal-hup) kill -HUP "$PPID" ;;
    esac
  fi
  TP_FAKE_DENIED="$denied"
  TP_FAKE_ALLOW="$allow"
  export TP_FAKE_DENIED TP_FAKE_ALLOW
  exec "$@"
fi
# The probe run inside a service's control group, which needs root. The
# stubbed python3 routes it to the fixture probe, which never touches a
# real control group; nothing else run under sudo is executed.
case "$1:$2:${3:-}" in
  python3:*/linux_offline_runtime.py:probe-unit|python3:*/linux_offline_runtime.py:control-unit)
    exec "$@"
    ;;
esac
# systemd's loaded configuration and its start generations, which are not
# the same thing and are the reason the harness compares invocation ids.
#
# `systemctl show` answers with what the last `daemon-reload` read, so a
# drop-in that was installed and reloaded but never restarted under reads
# back exactly like an enforced one. Only a start replaces the running
# instance and gives it a new invocation id. Modelling both here is what
# lets the deny-side restart, the restore-side restart and the reload
# each fail a named check when they are removed.
offline_unit_conf() {
  printf '%s/run/systemd/system/%s.service.d/10-tensorplate-validation-offline.conf' \
    "${TP_OFFLINE_UNIT_ROOT}" "$1"
}
case "$*" in
  *"systemctl daemon-reload"*)
    mkdir -p "${TP_OFFLINE_UNIT_ROOT}/generation" || exit 9
    for unit in tensorplate-agent tensorplate-observability; do
      conf="$(offline_unit_conf "$unit")"
      loaded="${TP_OFFLINE_UNIT_ROOT}/generation/${unit}.loaded"
      if [ -f "$conf" ]; then
        cp "$conf" "$loaded" || exit 9
      else
        rm -f "$loaded" || exit 9
      fi
    done
    ;;
esac
case "$*" in
  *"systemctl restart "*|*"systemctl start "*)
    mkdir -p "${TP_OFFLINE_UNIT_ROOT}/generation" || exit 9
    for word in "$@"; do
      case "$word" in
        tensorplate-agent|tensorplate-observability) ;;
        *) continue ;;
      esac
      conf="$(offline_unit_conf "$word")"
      record="${TP_OFFLINE_UNIT_ROOT}/generation/${word}"
      generation=$(sed -n 1p "$record" 2>/dev/null)
      state=$(sed -n 2p "$record" 2>/dev/null)
      [ -n "$generation" ] || generation=0
      skip=0
      case "${TP_FAKE_MODE:-ok}" in
        # A restart systemd reported and that did not replace the running
        # instance: the loaded policy still reads back as denied, and the
        # services are still the ones that started without it.
        offline-deny-restart-ignored) [ -f "$conf" ] && skip=1 ;;
        # The mirror of it on the way out: the drop-in is gone and the
        # loaded policy is empty, but the denied instances are still the
        # ones running.
        offline-restore-restart-ignored)
          [ -f "$conf" ] || { [ "$state" = denied ] && skip=1; } ;;
      esac
      if [ "$skip" -eq 0 ]; then
        generation=$((generation + 1))
        if [ -f "$conf" ]; then state=denied; else state=open; fi
        printf '%s\n%s\n' "$generation" "$state" >"$record" || exit 9
        # What this start was actually made under: the configuration the
        # last daemon-reload loaded, which is what systemd attaches.
        if [ -f "${record}.loaded" ]; then
          cp "${record}.loaded" "${record}.running" || exit 9
        else
          rm -f "${record}.running" || exit 9
        fi
      fi
    done
    ;;
esac
# A drop-in an earlier run left behind, planted where the harness's own
# reset cannot clear it first: on the install stage's state clearing,
# which runs once in a run with no baseline, so the leftover is never
# planted again after the offline stage's cleanup removed it. Not on the
# purge -- a device whose only tensorplate* record is the apt channel's
# bootstrap has nothing to purge, and clear_install calls apt-get at all
# only when the listing names something. One mode per unit, and one that
# plants a dangling symlink rather than a file, because the refusal is a
# step per unit and `install -D` would write through a symlink to
# wherever it points.
case "$*" in
  "rm -rf /etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate")
    leftover_unit=""
    leftover_link=0
    case "${TP_FAKE_MODE:-ok}" in
      offline-leftover-drop-in) leftover_unit=tensorplate-agent ;;
      offline-leftover-dangling-symlink) leftover_unit=tensorplate-agent; leftover_link=1 ;;
      offline-leftover-drop-in-observability) leftover_unit=tensorplate-observability ;;
    esac
    if [ -n "$leftover_unit" ]; then
      leftover="$(offline_unit_conf "$leftover_unit")"
      mkdir -p "$(dirname "$leftover")" || exit 9
      if [ "$leftover_link" -eq 1 ]; then
        ln -s "${TP_OFFLINE_UNIT_ROOT}/nowhere" "$leftover" || exit 9
      else
        : >"$leftover" || exit 9
      fi
    fi
    ;;
esac
case "$*" in
  "install -D -m 0644 "*)
    src="$5"
    dst="$6"
    case "$dst" in
      "${TP_OFFLINE_UNIT_ROOT}/run/systemd/system/"*) ;;
      *) echo "sudo stub: refusing to install outside the fixture unit root: $dst" >&2; exit 9 ;;
    esac
    mkdir -p "$(dirname "$dst")" || exit 9
    cp "$src" "$dst" || exit 9
    case "${TP_FAKE_MODE:-ok}" in
      # A drop-in that reaches the device spelling the shorthand the
      # owner ruling refuses: it expands to 127.0.0.0/8, which admits the
      # systemd-resolved stub and the DNS namespace behind it.
      offline-localhost-shorthand)
        sed -e 's|^IPAddressAllow=127.0.0.1/32$|IPAddressAllow=localhost|' \
            -e '/^IPAddressAllow=::1\/128$/d' "$src" >"$dst" || exit 9
        ;;
      # A directive systemd would act on that the address-policy readback
      # cannot see: the effective IPAddressDeny and IPAddressAllow are
      # unchanged, so only the on-disk lint can refuse this.
      offline-drop-in-extra-directive)
        printf 'ExecStartPre=/bin/true\n' >>"$dst" || exit 9
        ;;
      # Written under /etc as well, which no exit path removes.
      offline-persistent-drop-in)
        persistent="${TP_OFFLINE_UNIT_ROOT}/etc/systemd/system/${dst##*/run/systemd/system/}"
        mkdir -p "$(dirname "$persistent")" || exit 9
        cp "$src" "$persistent" || exit 9
        ;;
    esac
    exit 0
    ;;
  "rm -f "*"/10-tensorplate-validation-offline.conf")
    case "${TP_FAKE_MODE:-ok}" in
      offline-drop-in-not-removed) exit 0 ;;
    esac
    rm -f "$3"
    exit
    ;;
  "rmdir "*".service.d")
    rmdir "$2" 2>/dev/null
    exit 0
    ;;
esac
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
  # The privileged listing of a directory under /var/lib/tensorplate: the
  # entry set the manifest is built from, including the dotfiles `ls -A`
  # shows. A directory that is not there is reported the way ls reports
  # it, by name, so a set-aside copy that was removed outright fails the
  # read rather than reading as empty.
  "ls -A /var/lib/tensorplate/"*)
    [ "$#" -eq 3 ] || exit 9
    target="${TP_FAKE_VARLIB}/${3#/var/lib/tensorplate/}"
    if [ ! -d "$target" ]; then
      printf "ls: cannot access '%s': No such file or directory\n" "$3" >&2
      exit 1
    fi
    ls -A "$target"
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
# A file in that directory that belongs to neither the agent nor the
# harness's own idea of it: packaging/conf/observability.json points the
# observability unit's snapshot sink at
# /var/lib/tensorplate/state/observability-snapshot.json, so a running
# device keeps it beside the agent's two. The harness knows no name here
# -- it holds whatever the directory carries to its digests -- and this
# is what proves it.
printf '{"fixture":"observability snapshot"}\n' \
  >"${TP_FAKE_VARLIB}/state/observability-snapshot.json"
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
# An upgrade that leaves the state directory populated but without the
# agent's deployment state in it. The manifest the rollback takes would
# still have entries, and two directories holding only the rest would
# compare equal all the way to a pass.
if [ "$mode" = rollback-state-file-missing ] && [ "$phase" = upgraded ]; then
  rm -f "${TP_FAKE_VARLIB}/state/state.json"
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
#
# The same three shapes against the agent's recovery copy and against the
# observability snapshot, plus one deletion and one addition each: the
# destruction the rollback has to be held to is destruction ANYWHERE in
# the directory it set aside, not in the one name the harness happens to
# know. A device whose state.json survives and whose state.json.bak was
# emptied has lost exactly the copy that makes a corrupt primary
# recoverable.
if [ "$phase" = rolled-back ]; then
  case "$mode" in
    rollback-empties-backup) : >"${TP_FAKE_VARLIB}/state.bak/state.json" ;;
    rollback-truncates-backup) printf '{"fixture":"dur' >"${TP_FAKE_VARLIB}/state.bak/state.json" ;;
    rollback-rewrites-backup)
      printf '{"fixture":"durable_state"}\n' >"${TP_FAKE_VARLIB}/state.bak/state.json"
      ;;
    rollback-empties-agent-bak) : >"${TP_FAKE_VARLIB}/state.bak/state.json.bak" ;;
    rollback-truncates-agent-bak)
      printf '{"fixture":"durable state","reco' >"${TP_FAKE_VARLIB}/state.bak/state.json.bak"
      ;;
    rollback-rewrites-agent-bak)
      printf '{"fixture":"durable_state","recovery":true}\n' \
        >"${TP_FAKE_VARLIB}/state.bak/state.json.bak"
      ;;
    rollback-deletes-agent-bak) rm -f "${TP_FAKE_VARLIB}/state.bak/state.json.bak" ;;
    rollback-empties-snapshot) : >"${TP_FAKE_VARLIB}/state.bak/observability-snapshot.json" ;;
    rollback-deletes-snapshot)
      rm -f "${TP_FAKE_VARLIB}/state.bak/observability-snapshot.json"
      ;;
    rollback-adds-state-file)
      printf '{"fixture":"not what was set aside"}\n' \
        >"${TP_FAKE_VARLIB}/state.bak/state.json.new"
      ;;
    # Every file gone while the directory stays: the set-aside copy reads
    # as a directory that is there and holds nothing, which must not be
    # what "unchanged" means.
    rollback-empties-state-dir) rm -f "${TP_FAKE_VARLIB}/state.bak/"* ;;
    # The set-aside state destroyed by an install that then fails the way
    # install.sh does, after every package is installed: the stage never
    # reaches its preservation check, and the stranded-device report is
    # the only thing that says what is behind the pathname it hands the
    # operator.
    rollback-destroys-backup-then-fails)
      : >"${TP_FAKE_VARLIB}/state.bak/state.json"
      echo "error: TensorPlate services did not become ready within 30s" >&2
      exit 1
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

# This session's identity, in the four spellings the harness asks for.
# The numeric ids are this process's own: the offline stage hands them to
# the in-service probes, which drop to them, and the fixture probe
# refuses any pair that is not the operator's.
{
  printf '#!/bin/sh\n'
  printf 'real_uid=%s\nreal_gid=%s\n' "$(id -u)" "$(id -g)"
  cat <<'STUB'
case "$1" in
  -nG|-Gn)
    if [ "${TP_FAKE_GROUP:-member}" = member ]; then
      printf 'operator adm sudo tensorplate\n'
    else
      printf 'operator adm sudo\n'
    fi
    ;;
  -un) printf 'operator\n' ;;
  -u) printf '%s\n' "$real_uid" ;;
  -g) printf '%s\n' "$real_gid" ;;
  *) exit 9 ;;
esac
STUB
} >"${stub_bin}/id"

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
      *IPAddressDeny*)
        # The effective address policy, the way systemd reports it: the
        # allow list is whatever the unit's drop-in asks for, expanded --
        # so `localhost` reads back as 127.0.0.0/8, which is the whole
        # point of refusing the shorthand. Matched before ActiveState,
        # which the harness asks for in the same call.
        unit=""
        for word in "$@"; do
          case "$word" in tensorplate-*) unit="$word" ;; esac
        done
        # What the last daemon-reload read, not the file on disk and not
        # what any running instance was started under: that is what
        # `systemctl show` answers with, and the whole reason the harness
        # also compares the invocation id.
        conf="${TP_OFFLINE_UNIT_ROOT}/generation/${unit}.loaded"
        generation=$(sed -n 1p "${TP_OFFLINE_UNIT_ROOT}/generation/${unit}" 2>/dev/null)
        case "${TP_FAKE_MODE:-ok}" in
          offline-unit-dead)
            # A unit systemd does not have. Every property comes back
            # empty, which a readback that only looks for unexpected
            # entries would read as a denied unit.
            printf 'LoadState=not-found\nActiveState=inactive\nInvocationID=\nIPAddressDeny=\nIPAddressAllow=\n'
            exit 0
            ;;
        esac
        if [ -z "$generation" ]; then
          # A unit systemd has but has never started: there is no
          # instance and so no invocation id to report for one.
          printf 'LoadState=loaded\nActiveState=inactive\nInvocationID=\nIPAddressDeny=\nIPAddressAllow=\n'
          exit 0
        fi
        printf 'LoadState=loaded\nActiveState=active\n'
        case "$unit" in
          tensorplate-agent) prefix=1111111111111111111111111111 ;;
          *) prefix=2222222222222222222222222222 ;;
        esac
        printf 'InvocationID=%s%04x\n' "$prefix" "$generation"
        if [ -f "$conf" ] && [ "${TP_FAKE_MODE:-ok}" != offline-denial-inert ]; then
          # systemd prints both lists from a hash set, in an order that
          # changes with each PID 1 start. One unit reads back in the
          # drop-in's order and the other in reverse, so a readback that
          # compared the order could never pass here.
          if [ "$unit" = tensorplate-agent ]; then
            printf 'IPAddressDeny=0.0.0.0/0 ::/0\n'
            order='p'
          else
            printf 'IPAddressDeny=::/0 0.0.0.0/0\n'
            order='1!G;h;$p'
          fi
          printf 'IPAddressAllow='
          sed -n 's/^IPAddressAllow=//p' "$conf" \
            | sed -e 's/localhost/127.0.0.0\/8 ::1\/128/' | tr ' ' '\n' \
            | sed -n "$order" | tr '\n' ' ' | sed -e 's/ *$//'
          printf '\n'
        else
          printf 'IPAddressDeny=\nIPAddressAllow=\n'
        fi
        exit 0
        ;;
      *ControlGroup*)
        # The group a running unit is in, and nothing for one that is not.
        unit=""
        for word in "$@"; do
          case "$word" in tensorplate-*) unit="$word" ;; esac
        done
        if [ -s "${TP_OFFLINE_UNIT_ROOT}/generation/${unit}" ]; then
          printf '/system.slice/%s.service\n' "$unit"
        else
          printf '\n'
        fi
        ;;
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
        # The id carries the unit in its prefix and that unit's start
        # generation in its last four digits, so a restart reports a
        # different invocation than the one before it. That is the whole
        # reason the offline readback can tell a policy that reached a
        # running instance from one that only reached the loaded
        # configuration.
        case "$*" in
          *tensorplate-agent*) unit=tensorplate-agent; prefix=1111111111111111111111111111 ;;
          *tensorplate-observability*)
            unit=tensorplate-observability; prefix=2222222222222222222222222222 ;;
          *) exit 9 ;;
        esac
        generation=$(sed -n 1p "${TP_OFFLINE_UNIT_ROOT}/generation/${unit}" 2>/dev/null)
        state=$(sed -n 2p "${TP_OFFLINE_UNIT_ROOT}/generation/${unit}" 2>/dev/null)
        # A unit that is not running has no current invocation.
        if [ "${TP_FAKE_MODE:-ok}" = invocation-empty ] && [ "$unit" = tensorplate-agent ]; then
          generation=""
        fi
        # The agent's id unreadable at exactly one point: while its
        # instance is still the denied one and its drop-in is already
        # gone, which is the cleanup's capture before the restart.
        if [ "${TP_FAKE_MODE:-ok}:$unit:$state" = offline-cleanup-invocation-unreadable:tensorplate-agent:denied ] \
           && [ ! -e "${TP_OFFLINE_UNIT_ROOT}/run/systemd/system/tensorplate-agent.service.d/10-tensorplate-validation-offline.conf" ]; then
          exit 1
        fi
        if [ -z "$generation" ]; then
          printf '\n'
        else
          printf '%s%04x\n' "$prefix" "$generation"
        fi
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

# The metadata journald attaches to every record on a real device, so the
# harness's projection has something to take out. A projection written as
# a denylist of the host fields it knew about would pass a stub that
# emitted only those and then write evidence the scanner refuses on the
# first real Jetson; for the same reason every service record carries a
# field no list could name in advance, whose name is new on every run of
# this file.
TP_FAKE_JOURNAL_FIELD="TP_FIXTURE_$(python3 -c 'import secrets; print(secrets.token_hex(4).upper())')"
export TP_FAKE_JOURNAL_FIELD
cat >"${stub_bin}/journalctl" <<'STUB'
#!/bin/sh
zero=00000000000000000000000000000000
cursor="s=00000000;i=4a1;b=${zero};m=139f2a;t=6591c0;x=51d2"
# What every record carries, whoever wrote it.
host_fields="\"_HOSTNAME\":\"tp-synthetic-host\",\"_MACHINE_ID\":\"${zero}\",\"_BOOT_ID\":\"${zero}\",\"__CURSOR\":\"${cursor}\",\"__MONOTONIC_TIMESTAMP\":\"84210000000\",\"__SEQNUM\":\"1185\",\"__SEQNUM_ID\":\"${zero}\""
# What journald's stream transport adds to a line a service printed.
service_fields() {
  printf '"_TRANSPORT":"stdout","_STREAM_ID":"%s","_UID":"998","_GID":"998","_COMM":"%s","_EXE":"/usr/bin/%s","_CMDLINE":"/usr/bin/%s --config /etc/tensorplate/%s.json","_CAP_EFFECTIVE":"0","_SYSTEMD_CGROUP":"/system.slice/%s.service","_SYSTEMD_SLICE":"system.slice","SYSLOG_FACILITY":"3","%s":"fixture"' \
    "$zero" "$1" "$1" "$1" "${1#tensorplate-}" "$1" "${TP_FAKE_JOURNAL_FIELD:?}"
}
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
    printf '{%s,%s,"_SYSTEMD_UNIT":"%s","_PID":"%s","PRIORITY":"3","SYSLOG_IDENTIFIER":"%s","MESSAGE":"%s","__REALTIME_TIMESTAMP":"1789300000000000"}\n' \
      "$host_fields" "$(service_fields "${unit%.service}")" "$unit" \
      "$((4000 + written))" "${unit%.service}" "$message"
    written=$((written + 1))
  done
  exit 0
fi
# The invocation id carries the unit in its prefix and that unit's start
# generation in its last four digits, so a restart asks for a different
# invocation than the one before it.
case "$invocation" in
  1111111111111111111111111111????) unit=tensorplate-agent.service ;;
  2222222222222222222222222222????) unit=tensorplate-observability.service ;;
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
printf '{%s,%s,"_SYSTEMD_INVOCATION_ID":"%s","_SYSTEMD_UNIT":"%s","_PID":"4001","PRIORITY":"6","SYSLOG_IDENTIFIER":"%s","MESSAGE":"%s","__REALTIME_TIMESTAMP":"1789300000000000"}\n' \
  "$host_fields" "$(service_fields "${unit%.service}")" "$invocation" "$unit" \
  "${unit%.service}" "$message"

# The agent's platform identity line, emitted only by a start that
# happened under the denial drop-in. On this row the agent establishes no
# machine type at all -- there is no metadata service to establish one
# from -- and writes no record, which is what it says online as well.
#
# Keyed on the start generation, not on the drop-in file existing. A
# stage that installed the drop-in and never restarted the agent is a
# stage whose agent never started under it, so it gets no identity line.
agent_state="$(sed -n 2p "${TP_OFFLINE_UNIT_ROOT}/generation/tensorplate-agent" 2>/dev/null)"
if [ "$unit" = tensorplate-agent.service ] && [ "$agent_state" = denied ]; then
  identity='machine_type=none source=none record=not_applicable'
  repeat=1
  case "${TP_FAKE_MODE:-ok}" in
    # A machine type the denial let through, which on this row could only
    # have come from a metadata service it is not supposed to have.
    offline-identity-live-metadata)
      identity='machine_type=g2-standard-8 source=gce_metadata record=written' ;;
    # A record written while denied.
    offline-identity-records)
      identity='machine_type=none source=none record=written' ;;
    # Two lines from one invocation: the agent restarted, and the earlier
    # line might have been the one taken before the denial.
    offline-identity-twice) repeat=2 ;;
  esac
  while [ "$repeat" -gt 0 ]; do
    printf '{%s,%s,"_SYSTEMD_INVOCATION_ID":"%s","_SYSTEMD_UNIT":"%s","_PID":"4001","PRIORITY":"6","SYSLOG_IDENTIFIER":"%s","MESSAGE":"platform identity: %s","__REALTIME_TIMESTAMP":"1789300000000001"}\n' \
      "$host_fields" "$(service_fields "${unit%.service}")" "$invocation" "$unit" \
      "${unit%.service}" "$identity"
    repeat=$((repeat - 1))
  done
fi
# What `journalctl --show-cursor` prints after the records: a line that
# is not a record, after records that are. The projection refuses it
# where it reads the capture, so the stage's own parser never sees it.
if [ "${TP_FAKE_MODE:-ok}" = journal-trailing-line ] &&
   [ "$unit" = tensorplate-agent.service ]; then
  printf '%s\n' "-- cursor: ${cursor}"
fi
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
units = pathlib.Path(os.environ["TP_OFFLINE_UNIT_ROOT"], "run", "systemd", "system")
dropins = len(list(units.glob("*.service.d/10-tensorplate-validation-offline.conf")))
with open(os.environ["TP_FAKE_CLI_CALLS"], "a", encoding="utf-8") as log:
    log.write(json.dumps({"command": command, "config": str(path),
                          "socket": profile["socket_path"],
                          "denied": os.environ.get("TP_FAKE_DENIED", "unset"),
                          "dropins": dropins}) + "\n")
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
    # What this row's doctor renders into host_os, in the CLI's own order
    # (cli/src/commands/doctor/mod.rs): the OS a row names and its
    # version, the image identity in parentheses, the machine type after
    # it where there is one, then the exact facts. No machine type here,
    # because there is no metadata service on this device to name one.
    #
    # The image identity is the BSP line and the Ubuntu base, lowercase
    # -- platform/src/detect.rs builds it from row_granularity(), which
    # is r<major>.x and deliberately not the revision -- and the exact
    # L4T release is exact(), which is lowercase too. The exact version
    # is the nvidia-jetpack package's, which carries the build suffix the
    # row version drops. The recorded fixture for this row carries the
    # same identity, and the check below reads this line against it.
    host_os="JetPack 6.2 (L4T r36.x (Ubuntu 22.04 base)) [exact: version 6.2-b77, L4T r36.4.3]"
    case "$mode" in
      # A machine type doctor could only have got from a metadata
      # service, in each of the two spellings the CLI renders. Under the
      # denial either one means the offline stage is certifying a row
      # that resolved from an answer rather than from local facts.
      offline-doctor-live-metadata)
        if [ "${TP_FAKE_DENIED:-0}" = 1 ]; then
          host_os="JetPack 6.2 (L4T r36.x (Ubuntu 22.04 base)) on g2-standard-8 (from GCE metadata) [exact: version 6.2-b77, L4T r36.4.3]"
        fi
        ;;
      offline-doctor-recorded-metadata)
        if [ "${TP_FAKE_DENIED:-0}" = 1 ]; then
          host_os="JetPack 6.2 (L4T r36.x (Ubuntu 22.04 base)) on g2-standard-8 (recorded from GCE metadata by tensorplate-agent; metadata service unreachable; same kernel boot; CPU count, MemTotal and NVIDIA devices unchanged) [exact: version 6.2-b77, L4T r36.4.3]"
        fi
        ;;
      # The L4T release gone from host_os, from the image identity and
      # from the exact facts alike: doctor could not read the local file
      # this row resolves out of, so it names neither.
      offline-doctor-no-l4t)
        if [ "${TP_FAKE_DENIED:-0}" = 1 ]; then
          host_os="JetPack 6.2 [exact: version 6.2-b77]"
        fi
        ;;
      offline-doctor-failing)
        if [ "${TP_FAKE_DENIED:-0}" = 1 ]; then failing=1; fi
        ;;
      offline-doctor-row-warning)
        if [ "${TP_FAKE_DENIED:-0}" = 1 ]; then row_status=warning; fi
        ;;
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
 {"id":"host_os","status":"ok","message":"${host_os}"},
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
    # Both files the agent persists: agent/src/state.rs writes state.json
    # and refreshes the same-directory state.json.bak it falls back to
    # when the primary fails to decode, on every mutation. A fixture with
    # one file where a device has two would model a state directory the
    # rollback cannot be held to.
    printf '{"fixture":"durable state"}\n' >"${TP_FAKE_VARLIB}/state/state.json"
    printf '{"fixture":"durable state","recovery":true}\n' \
      >"${TP_FAKE_VARLIB}/state/state.json.bak"
    deploy_phase=active
    deployed="$deployment_id"
    [ "$mode" = deploy-not-active ] && deploy_phase=rolled_back
    [ "$mode" = deploy-other-id ] && deployed=a-different-deployment
    # The same two, but only for the deploy the offline stage makes, so
    # every stage before it passes.
    if [ "${TP_FAKE_DENIED:-0}" = 1 ]; then
      [ "$mode" = offline-deploy-not-active ] && deploy_phase=rolled_back
      [ "$mode" = offline-deploy-other-id ] && deployed=a-different-deployment
    fi
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
    # Status answered from inside a denied transient unit: the
    # deployment the agent re-warmed, and the loopback serving URL the
    # denial has to have left reachable.
    if [ "${TP_FAKE_DENIED:-0}" = 1 ]; then
      [ "$mode" = offline-status-other-deployment ] && active_id=a-different-deployment
      [ "$mode" = offline-status-not-loopback ] && serving_url="\"http://169.254.169.254:${TP_FAKE_SERVING_PORT}/infer\""
      # The read AFTER the denied deploy, and only that one: the agent
      # answers with something other than the deployment the deploy
      # reply said it had made. Keyed on the deploy having landed -- the
      # active id it wrote ends in the offline suffix -- so the read
      # before it is untouched and the stage gets as far as the check
      # that reads this one. The two modes above are applied to every
      # denied read and are caught by the first `status checks`, which
      # is why neither of them reaches this step.
      if [ "$mode" = offline-status-after-deploy-other-deployment ]; then
        case "$active_id" in
          *-offline) active_id=a-different-deployment ;;
        esac
      fi
    fi
    # The ninth read is the rollback's precondition: the candidate
    # serving what the baseline deployed. A device that is not in that
    # state must be refused there, before anything is stopped. The
    # offline stage reads status twice, before and after its own deploy,
    # so the reads before it are deploy-smoke, status-logs, the restart
    # recovery and the crash-loop recovery, and after it the upgrade's
    # two round trips.
    if [ "$reads" -eq 9 ] && [ "$mode" = rollback-other-active ]; then
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
    # The engine answering wrongly only from inside the denied unit, so
    # deploy-smoke, restart and crash-loop all pass first.
    [ "$mode" = offline-infer-garbled ] && [ "${TP_FAKE_DENIED:-0}" = 1 ] && garble=1
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
    TP_OFFLINE_UNIT_ROOT="${preflight_units:-${td}/preflight-scratch/units}" \
    TP_OFFLINE_CGROUP_ROOT="${td}/preflight-scratch/cgroup" \
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
    "$BASH" "${preflight_harness:-$harness}" \
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
# Overwrite the listed package, not a new file beside it: an unlisted file
# changes nothing sha256sum -c reads.
tampered_deb="${tampered}/tensorplate-agent_${candidate_version/\~/.}_arm64.deb"
[[ -f "$tampered_deb" ]] || { printf 'FAIL: fixture has no %s to tamper with\n' "$tampered_deb" >&2; exit 1; }
printf 'Package: tensorplate-agent\nVersion: 6.6.6-1\n' >"$tampered_deb"
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
# systemd-run is here because preflight requires it: the offline stage
# runs every CLI call in a transient unit that denies the network, so a
# device without it cannot produce that stage at all.
required_commands=(sudo systemctl journalctl python3 sha256sum dpkg-query dpkg-deb dpkg
                   systemd-run)
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

# --- the offline stage's preconditions, checked before anything runs.
#
# The offline mechanism is a file beside the harness; a copy of the
# harness taken without it cannot deny anything, and must say so rather
# than fail six stages in.
lone_harness="${td}/lone/tools/validation/jetson-lifecycle.sh"
mkdir -p "$(dirname "$lone_harness")"
cp "$harness" "$lone_harness"
preflight_harness="$lone_harness"
check "a harness without the offline module beside it is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-no-module" 0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and names the file it looked for" yes \
  "$(said "missing $(dirname "$lone_harness")/linux_offline_runtime.py")"
cp "${repo_root}/tools/validation/linux_offline_runtime.py" "$(dirname "$lone_harness")/"
preflight_harness=""

# A denial drop-in an earlier run left behind would deny both services
# through install and every stage before offline, all of which have to
# run online. Refused in preflight, per unit, with nothing changed --
# including the leftover, which is not this run's to remove.
for leftover_unit in tensorplate-agent tensorplate-observability; do
  leftover_units="${td}/preflight-units-${leftover_unit}"
  leftover_dir="${leftover_units}/run/systemd/system/${leftover_unit}.service.d"
  mkdir -p "$leftover_dir"
  printf '[Service]\n' >"${leftover_dir}/10-tensorplate-validation-offline.conf"
  preflight_units="$leftover_units"
  check "a device carrying a ${leftover_unit} denial drop-in is refused" "1" \
    "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-leftover-${leftover_unit}" \
        0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
  check "  and says nothing was changed" yes "$(said 'nothing was changed')"
  check "  and names the file and how to remove it" yes \
    "$(said "remove it with: sudo rm -f ${leftover_dir}/10-tensorplate-validation-offline.conf && sudo systemctl daemon-reload && sudo systemctl restart ${leftover_unit}")"
  check "  and leaves it where it is" yes \
    "$([[ -f "${leftover_dir}/10-tensorplate-validation-offline.conf" ]] && echo yes || echo no)"
  preflight_units=""
done
# A dangling symlink is still a file at that path, and `install -D` would
# write through it to wherever it points, off the runtime unit tree the
# stage confines itself to.
leftover_units="${td}/preflight-units-symlink"
leftover_dir="${leftover_units}/run/systemd/system/tensorplate-agent.service.d"
mkdir -p "$leftover_dir"
ln -s "${leftover_units}/nowhere" "${leftover_dir}/10-tensorplate-validation-offline.conf"
preflight_units="$leftover_units"
check "a dangling denial drop-in symlink is refused too" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-leftover-symlink" \
      0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}")"
check "  and leaves it where it is" yes \
  "$([[ -L "${leftover_dir}/10-tensorplate-validation-offline.conf" ]] && echo yes || echo no)"
preflight_units=""

# Every id this run derives from --deployment-id, each of which the CLI
# validates as one filesystem path segment: the offline stage's
# <id>-offline and, in a run with a baseline set, the upgrade and
# rollback stages' <id>-baseline and <id>-rollback. Refused here rather
# than stages in, where the deploy would fail as a CLI validation error
# and read as a failure of the stage that made it.
#
# The boundaries are one byte apart, so a guard that bounded only the
# shortest derivation would pass the middle case. Read against
# protocol/rust/src/agent_control.rs: MAX_DEPLOYMENT_ID_BYTES is 128.
long_id="$(printf 'a%.0s' $(seq 1 121))"
check "a deployment id whose offline form passes the 128-byte limit is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-long-id" 0.2.1 v0.2.1-rc.2 "$assets" \
      "${confirm[@]}" --deployment-id "$long_id")"
check "  and names the id the offline stage would have deployed" yes \
  "$(said "deploys as ${long_id}-offline")"
# 120 bytes: the offline form is exactly 128 and the upgrade and rollback
# forms are 129. A guard that only bounded the offline form would admit
# this and fail seven stages later, inside stage_upgrade, after the
# candidate has been purged.
baseline_id="${long_id%a}"
check "a deployment id whose baseline form passes the limit is refused too" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-baseline-id" 0.2.1 v0.2.1-rc.2 "$assets" \
      "${confirm[@]}" --deployment-id "$baseline_id")"
check "  and names the id the upgrade stage would have deployed" yes \
  "$(said "deploys as ${baseline_id}-baseline")"
# Refused with no baseline set named, as with one: one id is accepted or
# refused by one rule however the run is invoked, and a --preflight-only
# run answers for the run that follows it.
check "  with a baseline set named as well" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-baseline-id-set" 0.2.1 v0.2.1-rc.2 \
      "$assets" "${confirm[@]}" --deployment-id "$baseline_id" \
      --baseline-tag v0.1.5 --baseline-assets-dir "$baseline_assets")"
check "  while one byte shorter passes" "0" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-long-id-ok" 0.2.1 v0.2.1-rc.2 "$assets" \
      "${confirm[@]}" --deployment-id "${baseline_id%a}")"
check "a deployment id outside the allowed charset is refused" "1" \
  "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-bad-id" 0.2.1 v0.2.1-rc.2 "$assets" \
      "${confirm[@]}" --deployment-id 'smoke/../../etc')"
check "  and names the charset" yes \
  "$(said 'must be ASCII letters, digits, dot, dash or underscore')"
# `.` and `..` pass the charset and are reserved path segments the CLI
# rejects, so the charset alone is not the rule.
for reserved in . ..; do
  check "a deployment id of ${reserved} is refused although the charset admits it" "1" \
    "$(preflight aarch64 "$jammy" "$r36" "${td}/evidence-reserved-id-${#reserved}" \
        0.2.1 v0.2.1-rc.2 "$assets" "${confirm[@]}" --deployment-id "$reserved")"
  check "  and says the CLI reserves it" yes \
    "$(said 'must not be . or .., which the CLI reserves as path segments')"
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
  : >"${appliance}/transient.log"
  : >"${appliance}/health-requests.log"
  rm -rf "${appliance}/staged-bundle" "${appliance}/scratch" "${appliance}/varlib" \
    "${appliance}/units"
  mkdir -p "${appliance}/scratch" "${appliance}/units/generation"
  # The device starts with both services running and no address policy:
  # one start generation each, and no drop-in loaded under it.
  printf '1\nopen\n' >"${appliance}/units/generation/tensorplate-agent"
  printf '1\nopen\n' >"${appliance}/units/generation/tensorplate-observability"
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
    TP_FAKE_TRANSIENT_LOG="${appliance}/transient.log" \
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
    TP_OFFLINE_UNIT_ROOT="${appliance}/units" \
    TP_OFFLINE_CGROUP_ROOT="${appliance}/cgroup" \
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
    python3 -c "$default_signals" "$BASH" "${harness_under_test:-$harness}" \
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

# The runtime drop-in the offline stage installs for a unit, in the
# fixture's unit tree, and how many of them a run left behind. -L as well
# as -e: a dangling symlink at that path is a file the run left there,
# and -e alone reports it as gone.
offline_drop_in() {
  printf '%s' "${appliance}/units/run/systemd/system/${1}.service.d/10-tensorplate-validation-offline.conf"
}
offline_drop_ins_left() {
  local unit path count=0
  for unit in tensorplate-agent tensorplate-observability; do
    path="$(offline_drop_in "$unit")"
    [[ -e "$path" || -L "$path" ]] && count=$((count + 1))
  done
  printf '%s' "$count"
}

# Where the denial reached, read from what the stubs saw rather than from
# the harness text.
#
#   window          the stage installed a drop-in and removed one after it
#   outside         offline-only privileged commands outside that window
#   denied          CLI calls made inside a denied transient unit
#   leaked          CLI calls made on a device carrying a drop-in but not
#                   denied themselves
#   transient       CLI calls made in an undenied transient unit
#   installs        install.sh runs
#   installs_denied install.sh runs inside the window
offline_scope() {
  python3 - "${appliance}/sudo.log" "${appliance}/cli-calls.jsonl" <<'PY'
import json, re, sys

sudo = [line.rstrip("\n") for line in open(sys.argv[1], encoding="utf-8")]
calls = [json.loads(line) for line in open(sys.argv[2], encoding="utf-8")]
offline_only = re.compile(r"^systemd-run |--property=IPAddress"
                          r"|/10-tensorplate-validation-offline\.conf(?: |$)"
                          r"|/linux_offline_runtime\.py ")
marks = [i for i, line in enumerate(sudo) if offline_only.search(line)]
installed = [i for i, line in enumerate(sudo) if re.match(r"^install -D -m 0644 ", line)]
removed = [i for i, line in enumerate(sudo)
           if re.fullmatch(r"rm -f \S*/10-tensorplate-validation-offline\.conf", line)]
window = bool(installed and removed and max(removed) > max(installed))
first, last = (min(marks), max(marks)) if marks else (0, -1)
outside = [line for i, line in enumerate(sudo)
           if offline_only.search(line) and not first <= i <= last]
installs = [i for i, line in enumerate(sudo)
            if re.search(r"/install\.sh --local-artifacts ", line)]
print("window={} outside={} denied={} leaked={} transient={} installs={} "
      "installs_denied={}".format(
          "yes" if window else "no", len(outside),
          sum(1 for call in calls if call["denied"] == "1"),
          sum(1 for call in calls if call["denied"] != "1" and call["dropins"]),
          sum(1 for call in calls if call["denied"] == "0"),
          len(installs),
          sum(1 for i in installs if first <= i <= last)))
PY
}

ok_evidence="${td}/stages-ok"
check "a stubbed run completes" "0" "$(run_stages ok "$ok_evidence" "")"
check "  every CLI command ignores inherited agent and inference endpoints" yes \
  "$(python3 - "${appliance}/cli-calls.jsonl" <<'PY'
import json, sys

calls = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
# install, deploy-smoke, status-logs, restart, crash-loop, then the
# offline stage's five, which are the module's CLI_CALLS in its order.
expected = ["doctor", "deploy", "infer", "status", "status", "logs",
            "infer", "status", "infer", "status",
            "status", "doctor", "deploy", "status", "infer"]
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
for stage in install deploy-smoke status-logs restart crash-loop offline; do
  check "  ${stage} is recorded as a pass" "pass" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
for stage in upgrade rollback; do
  check "  ${stage} is recorded as skipped" "skipped" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
check "  the skipped stages keep the run incomplete" incomplete \
  "$(report_field "${ok_evidence}/lifecycle-report.json" outcome)"
check "  every skip says what would run it" yes \
  "$(python3 - "${ok_evidence}/lifecycle-report.json" <<'PY'
import json, sys

stages = {s["stage"]: s for s in json.load(open(sys.argv[1]))["stages"]}
# The two a baseline would have run name the options that run them.
wanted = {"upgrade": ["--baseline-tag", "--baseline-assets-dir"],
          "rollback": ["--baseline-tag", "--baseline-assets-dir"]}
print("yes" if all(
    stages[name]["status"] == "skipped"
    and all(marker in stages[name].get("detail", "") for marker in markers)
    for name, markers in wanted.items()
) else "no")
PY
)"
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
check "  the deploys name the smoke id and the offline one, both from the staged bundle" \
  "${deployment_id} ${appliance}/staged-bundle|${deployment_id}-offline ${appliance}/staged-bundle" \
  "$(tr '\n' '|' <"${appliance}/deploy.log" | sed 's/|$//')"
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
# The invocation id carries the unit in its prefix and that unit's start
# generation in its last four digits, so a capture taken after a restart
# asks for a different id than one taken before it.
for invocation in 1111111111111111111111111111 2222222222222222222222222222; do
  check "  journal capture selects a current invocation of ${invocation}" yes \
    "$(grep -F "journalctl" "${appliance}/sudo.log" \
       | grep -Eq "_SYSTEMD_INVOCATION_ID=${invocation}[0-9a-f]{4}( |$)" && echo yes || echo no)"
done
# And the offline stage's capture is a LATER generation of the agent than
# the status-logs one: it reads the instance that started under the
# denial, which is the only one whose identity line is this stage's.
check "  and the offline capture reads a later agent instance than status-logs did" yes \
  "$(python3 - "${appliance}/sudo.log" <<'PY'
import re, sys

ids = re.findall(r"_SYSTEMD_INVOCATION_ID=(1111111111111111111111111111[0-9a-f]{4})",
                 open(sys.argv[1], encoding="utf-8").read())
print("yes" if len(ids) >= 2 and ids[-1] > ids[0] else f"no: {ids}")
PY
)"
# --- the offline stage, read off the run above.
#
# These were negative checks while the stage was deferred -- that the
# harness never touched network policy. They are the same claims read the
# other way now: the drop-in IS applied, under /run and nowhere else, and
# IS removed on the way out.
check "  the offline stage denied both services through runtime drop-ins" "yes yes" \
  "$(printf '%s %s' \
     "$(grep -F 'install -D -m 0644' "${appliance}/sudo.log" | grep -q '/run/systemd/system/tensorplate-agent.service.d/' && echo yes || echo no)" \
     "$(grep -F 'install -D -m 0644' "${appliance}/sudo.log" | grep -q '/run/systemd/system/tensorplate-observability.service.d/' && echo yes || echo no)")"
check "  and never wrote a persistent unit file" no \
  "$(grep -q '/etc/systemd/system' "${appliance}/sudo.log" && echo yes || echo no)"
check "  and removed every drop-in it installed" 0 "$(offline_drop_ins_left)"
check "  and filed the removal it read back from systemd" "2 False" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))
print(r["drop_ins_removed"], r["denied"])' "${ok_evidence}/offline-restored.json")"
check "  the allowed prefixes are the two host addresses, not the localhost shorthand" \
  "127.0.0.1/32 ::1/128" \
  "$(python3 -c 'import json,sys
print(" ".join(json.load(open(sys.argv[1]))["mechanism"]["allow"]))' \
    "${ok_evidence}/offline-runtime.json")"
# status twice -- once for the deployment the agent re-warmed while
# denied, once for the one this stage deployed -- plus doctor, deploy and
# infer. Every one of them inside its own denied transient unit, and
# every one pinned to this run's private CLI config.
check "  every CLI call the stage made ran in a denied transient unit" 5 \
  "$(grep -c 'systemd-run .*--property=IPAddressDeny=any.*-- tensorplate --config ' "${appliance}/sudo.log")"
check "  and none of them ran in an undenied one" 0 \
  "$(grep 'systemd-run .*-- tensorplate ' "${appliance}/sudo.log" \
     | grep -vc 'IPAddressDeny=any' || true)"
check "  each call ran behind a probe of its own unit" \
  "status doctor deploy status-after-deploy infer" \
  "$(sed -n 's/^systemd-run .*--property=IPAddressDeny=any.* -- python3 .*linux_offline_runtime\.py run-denied --call \([a-z-]*\) .* -- tensorplate .*/\1/p' \
       "${appliance}/sudo.log" | tr '\n' ' ' | sed 's/ $//')"
check "  and the certificate classifies each unit's probe against the transient control" \
  "deploy:5 doctor:5 infer:5 status:5 status-after-deploy:5" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))["cli_units_probed_before_each_call"]
print(" ".join("%s:%d" % (call, len(entry["classification"]["operations_refused_under_the_denial"]))
               for call, entry in sorted(r.items()) if entry["probe"]["call"] == call))' \
    "${ok_evidence}/offline-runtime.json")"
check "  the control ran in a transient unit with no denial" 1 \
  "$(grep -c 'systemd-run .*-- python3 .*linux_offline_runtime.py control ' "${appliance}/sudo.log")"
check "  and the control carried no address policy" 0 \
  "$(grep 'linux_offline_runtime.py control ' "${appliance}/sudo.log" | grep -c 'IPAddressDeny' || true)"
check "  the probe ran denied" 1 \
  "$(grep -c 'systemd-run .*--property=IPAddressDeny=any.*-- python3 .*linux_offline_runtime.py probe ' "${appliance}/sudo.log")"
# This row has no metadata service. Every probe and control says so, and
# the module then requires the two metadata operations to be ABSENT from
# both documents rather than letting an operation that quietly vanished
# read as one that passed.
check "  no probe was taken as though this row had a metadata service" "11 0" \
  "$(printf '%s %s' \
     "$(grep -cE 'linux_offline_runtime\.py (probe|control|probe-unit|control-unit|run-denied) ' "${appliance}/sudo.log")" \
     "$(grep -E 'linux_offline_runtime\.py (probe|control|probe-unit|control-unit|run-denied) ' "${appliance}/sudo.log" \
        | grep -vc -- '--metadata-address none' || true)")"
check "  and the certificate records the metadata operations as absent, and carries none" \
  "absent 0" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))
operations = set(r["control"]["denied"]) | set(r["probe"]["denied"])
print(r["classification"]["metadata_operation"],
      len([name for name in operations if "metadata" in name]))' \
    "${ok_evidence}/offline-runtime.json")"
check "  the resolver stub is cut off under the denial, as the shorthand would not have been" \
  "timeout EPERM" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))["denied"]
print(d["tcp_resolver_stub"], d["udp_resolver_stub"])' "${ok_evidence}/offline-probe.json")"
# Both services, probed from inside their own control groups: each
# control completed before the denial, each probe refused under it.
for unit in tensorplate-agent tensorplate-observability; do
  check "  ${unit} was probed inside its own control group, against its own control" \
    "ok EPERM unit 5" \
    "$(python3 -c 'import json,sys
d=sys.argv[1]; u=sys.argv[2]
control=json.load(open(f"{d}/offline-unit-control-{u}.service.json"))["denied"]
probe=json.load(open(f"{d}/offline-unit-probe-{u}.service.json"))["denied"]
c=json.load(open(f"{d}/offline-unit-classification-{u}.service.json"))
print(" ".join(sorted(set(control.values()))), " ".join(sorted(set(probe.values()))),
      c["scope"], len(c["operations_refused_under_the_denial"]))' "$ok_evidence" "$unit")"
  check "  and as root only to join it" yes \
    "$(grep -Eq "^python3 .*linux_offline_runtime\.py probe-unit --unit ${unit} --control-group /system\.slice/${unit}\.service --uid $(id -u) --gid $(id -g) " \
         "${appliance}/sudo.log" && echo yes || echo no)"
done
# The transient units are the operator's own: every CLI call made under
# the denial, its probe and the control probe run as this account, with
# its groups, so what the denial is applied to is the call this operator
# makes online. Without these properties systemd-run runs the unit as
# root -- a different call, with root's HOME and without the membership
# of the agent's control group the run checked in preflight -- and the
# stage would certify it as the operator's.
#
# --pipe --wait --collect is read back for the same reason: without
# --wait the unit's exit status is systemd-run's rather than the CLI's,
# so a failed call would pass.
#
# Seven units: the control probe, the five CLI calls and the denied
# probe. Every one of them, rather than any one, so a property dropped
# from one path is not covered by another. The operator is the stubbed
# session's, not the runner's -- `id` is a stub here, and the harness
# asks it the same questions it would ask on a device.
check "  every transient unit ran as the operator, and reported the command's own status" \
  "7 $("${stub_bin}/id" -un)|$("${stub_bin}/id" -Gn)|--pipe --wait --collect " \
  "$(python3 -c 'import sys
lines = [line.rstrip("\n") for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
# One distinct line, or every distinct one, so a single unit started
# differently from the rest is named rather than averaged away.
print(len(lines), " and ".join(sorted(set(lines))))' "${appliance}/transient.log")"
check "  where the certificate carries both" \
  "tensorplate-agent.service tensorplate-observability.service" \
  "$(python3 -c 'import json,sys
print(" ".join(sorted(json.load(open(sys.argv[1]))["units_probed_in_their_own_control_group"])))' \
    "${ok_evidence}/offline-runtime.json")"
check "  the agent socket and the serving port stay reachable under the denial" "ok ok" \
  "$(python3 -c 'import json,sys
a=json.load(open(sys.argv[1]))["allowed"]
print(a["unix_agent_socket"], a["tcp_loopback_serving_port"])' "${ok_evidence}/offline-probe.json")"
# This row establishes no machine type: there is no metadata service to
# establish one from, and nothing is recorded. Under the denial the agent
# has to say exactly what it says online.
check "  the agent established no machine type, and recorded none" "none not_applicable" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))
print(r["machine_type_source"], r["record"])' "${ok_evidence}/offline-identity.json")"
check "  a fresh deployment was made while denied" "${deployment_id}-offline" \
  "$(python3 -c 'import json,sys
print(json.load(open(sys.argv[1]))["payload"]["deployment_id"])' \
    "${ok_evidence}/offline-deploy.json")"
check "  and the TensorRT engine returned its input unchanged under the denial" pass \
  "$(report_field "${ok_evidence}/offline-infer-check.json" infer)"
check "  the certificate's verdicts are the ones its own documents carry" \
  "True pass pass pass pass pass" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))
cli=r["cli_under_denial"]
print(r["enforced"], cli["status"], cli["doctor"], cli["deploy"],
      cli["status_after_deploy"], cli["infer"])' \
    "${ok_evidence}/offline-runtime.json")"
check "  and it records both services replaced under the denial and again without it" \
  "tensorplate-agent.service tensorplate-observability.service | tensorplate-agent.service tensorplate-observability.service" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))
print(" ".join(r["units_restarted_under_the_denial"]), "|",
      " ".join(r["restore"]["units_restarted_without_the_denial"]))' \
    "${ok_evidence}/offline-runtime.json")"
check "  and names the operations the denial refused, and the connects it silenced" "5 1" \
  "$(python3 -c 'import json,sys
c=json.load(open(sys.argv[1]))["classification"]
print(len(c["operations_refused_under_the_denial"]), len(c["operations_silenced_under_the_denial"]))' \
    "${ok_evidence}/offline-runtime.json")"
check "  and the denied readback compared prefixes the host printed in either order" \
  "127.0.0.1/32 ::1/128" \
  "$(python3 -c 'import json,sys
print(" ".join(json.load(open(sys.argv[1]))["allowed_prefixes"]))' \
    "${ok_evidence}/offline-denial.json")"
# Which is only a claim about the readback if the stub really prints the
# observability unit's lists in the other order: asked directly, for a
# unit started under the rendered drop-in.
show_order="${td}/show-order"
mkdir -p "${show_order}/generation"
printf '1\ndenied\n' >"${show_order}/generation/tensorplate-observability"
python3 "${repo_root}/tools/validation/linux_offline_runtime.py" drop-in-text \
  >"${show_order}/generation/tensorplate-observability.loaded"
check "  (the observability unit printed both lists in reverse)" \
  "IPAddressDeny=::/0 0.0.0.0/0|IPAddressAllow=::1/128 127.0.0.1/32" \
  "$(env TP_OFFLINE_UNIT_ROOT="$show_order" "${stub_bin}/systemctl" show \
       -p LoadState -p ActiveState -p InvocationID -p IPAddressDeny -p IPAddressAllow \
       -- tensorplate-observability | grep '^IPAddress' | tr '\n' '|' | sed 's/|$//')"
# Read from what the stubs saw: the offline stage made exactly five CLI
# calls, every one denied; no call before or after it was denied or made
# in a transient unit at all; the installer ran with no denial in place;
# and no offline-only privileged command ran outside the stage.
check "  the denial reached exactly the offline stage" \
  "window=yes outside=0 denied=5 leaked=0 transient=0 installs=1 installs_denied=0" \
  "$(offline_scope)"
# The claim this check's name makes, run against the file rather than
# asserted of it: the scanner is what admits published evidence, and an
# existence test would pass for a document naming an internal address.
offline_certificate_scan="${td}/offline-certificate-scan.out"
offline_certificate_status=0
"${repo_root}/tools/validation/check-evidence-publication.sh" --patterns-only \
  "${ok_evidence}/offline-runtime.json" >"$offline_certificate_scan" 2>&1 \
  || offline_certificate_status=$?
check "  and the offline evidence names no host address the scanner refuses" 0 \
  "$offline_certificate_status"
if ((offline_certificate_status != 0)); then
  sed 's/^/       /' "$offline_certificate_scan" >&2
fi

# --- the journal captures are projected where they are captured.
#
# The stub journalctl emits the metadata systemd attaches to every real
# record. What reaches the evidence directory must carry only the fields
# the stage assertions read -- the set the publication scanner accepts --
# and must still carry them, or the projection has taken out what the
# assertions depend on. The record counts are the stub's own, so a
# projection that dropped records would fail here too.
#
# Prints `<records> <keys outside the allowlist> <required keys missing
# from some record>`.
journal_projection() {
  local file="$1"
  shift
  python3 - "$file" "$@" <<'PY'
import json, sys

ALLOWED = {"MESSAGE", "PRIORITY", "SYSLOG_IDENTIFIER", "UNIT", "_PID",
           "_SYSTEMD_UNIT", "_SYSTEMD_INVOCATION_ID", "__REALTIME_TIMESTAMP"}

path = sys.argv[1]
required = set(sys.argv[2:])
extra, missing, records = set(), set(), 0
for line in open(path, encoding="utf-8"):
    if not line.strip():
        continue
    entry = json.loads(line)
    records += 1
    extra |= set(entry) - ALLOWED
    missing |= required - set(entry)
print(records, ",".join(sorted(extra)) or "none", ",".join(sorted(missing)) or "none")
PY
}
for journal in agent observability; do
  check "  the ${journal} journal is projected to the service's own fields" "1 none none" \
    "$(journal_projection "${ok_evidence}/${journal}-journal.txt" \
       MESSAGE _SYSTEMD_UNIT _SYSTEMD_INVOCATION_ID)"
done
# The agent instance that ran under the denial: its start record and the
# platform identity line the offline stage reads out of it.
check "  the offline agent journal is projected too, identity line and all" "2 none none" \
  "$(journal_projection "${ok_evidence}/offline-agent-journal.txt" \
     MESSAGE _SYSTEMD_UNIT _SYSTEMD_INVOCATION_ID)"
# The crash-loop capture is taken by timestamp rather than by invocation,
# so only MESSAGE and _SYSTEMD_UNIT are required of every record here.
check "  the crash-loop journal is projected to the service's own fields" "5 none none" \
  "$(journal_projection "${ok_evidence}/crash-loop-journal.txt" MESSAGE _SYSTEMD_UNIT)"
# A projection that ran but left the raw capture behind would publish
# nothing, and would still leave host metadata on the machine for the
# next thing that collects logs. The scratch space is shared by every run
# in this file, so the count is cumulative: the first run that leaves a
# capture behind fails its own check, and every later one.
scratch_holding_host_metadata() {
  local count=0 file
  while IFS= read -r file; do
    if grep -qF '_HOSTNAME' "$file"; then
      count=$((count + 1))
    fi
  done < <(find "${appliance}/scratch" -type f)
  printf '%s' "$count"
}
check "  and no raw capture is left in the harness's scratch space" 0 \
  "$(scratch_holding_host_metadata)"

# --- and the scanner refuses a capture that was not projected.
#
# The four checks above are read against this file's own idea of the
# allowed set. What decides publication is the scanner, so drive it over
# the same files: the projected captures pass, and the stub journalctl's
# own unprojected output for one of them does not. Without the second
# half the first would certify a harness that had stopped projecting only
# if the scanner also stopped objecting.
projected_scan="${td}/journal-projected-scan.out"
projected_status=0
"${repo_root}/tools/validation/check-evidence-publication.sh" --patterns-only \
  "${ok_evidence}/agent-journal.txt" "${ok_evidence}/observability-journal.txt" \
  "${ok_evidence}/offline-agent-journal.txt" "${ok_evidence}/crash-loop-journal.txt" \
  >"$projected_scan" 2>&1 || projected_status=$?
check "  the projected captures pass the publication scanner" 0 "$projected_status"
if ((projected_status != 0)); then
  sed 's/^/       /' "$projected_scan" >&2
fi
raw_capture="${td}/raw-agent-journal.txt"
# Its own unit root, so what the stub prints does not depend on which
# run left the appliance's generation files as they are: one agent
# instance that started with nothing denied, which is one record.
raw_units="${td}/raw-journal-units"
mkdir -p "${raw_units}/generation"
printf '1\nopen\n' >"${raw_units}/generation/tensorplate-agent"
TP_FAKE_MODE=ok TP_OFFLINE_UNIT_ROOT="$raw_units" "${stub_bin}/journalctl" \
  -u tensorplate-agent _SYSTEMD_INVOCATION_ID=11111111111111111111111111110001 \
  -n 100 --no-pager --output=json >"$raw_capture"
# Every field the stub emits and the projection removes, named here so a
# stub trimmed back to the handful of host fields a denylist would name
# fails rather than quietly admitting one. The field named per run sorts
# between SYSLOG_FACILITY and the underscored names.
check "  the unprojected capture carries the fields the projection removes" \
  "1 SYSLOG_FACILITY,${TP_FAKE_JOURNAL_FIELD},_BOOT_ID,_CAP_EFFECTIVE,_CMDLINE,_COMM,_EXE,_GID,_HOSTNAME,_MACHINE_ID,_STREAM_ID,_SYSTEMD_CGROUP,_SYSTEMD_SLICE,_TRANSPORT,_UID,__CURSOR,__MONOTONIC_TIMESTAMP,__SEQNUM,__SEQNUM_ID none" \
  "$(journal_projection "$raw_capture" MESSAGE _SYSTEMD_UNIT _SYSTEMD_INVOCATION_ID)"
raw_scan="${td}/journal-raw-scan.out"
raw_status=0
"${repo_root}/tools/validation/check-evidence-publication.sh" --patterns-only \
  "$raw_capture" >"$raw_scan" 2>&1 || raw_status=$?
check "  and the scanner refuses it" 1 "$raw_status"
check "  for its journal fields, and for nothing else" "journal-field" \
  "$(python3 - "$raw_scan" <<'PY'
import re, sys

classes = set()
for line in open(sys.argv[1], encoding="utf-8"):
    match = re.match(r"^(\S+):\d+: (.+) \(\d+ chars\)$", line.rstrip("\n"))
    if match:
        classes.add(match.group(2))
print(",".join(sorted(classes)) or "none")
PY
)"
# The three lists that have to agree: what the harness keeps, what the
# scanner admits, and what this file checks against. A rename or an
# emptied collection reads as no field set at all, which is drift this
# has to report rather than silently compare nothing.
check "  the harness, the scanner and this file agree on the allowed fields" yes \
  "$(python3 - "$harness" "${repo_root}/tools/validation/check-evidence-publication.sh" "$0" <<'PY'
import re, sys

def field_set(path, name):
    """The double-quoted strings in the collection `name` is assigned."""
    body = open(path, encoding="utf-8").read()
    start = body.find("\n" + name + " = ")
    if start < 0:
        return None
    depth = 0
    for index in range(start, len(body)):
        character = body[index]
        if character in "({[":
            depth += 1
        elif character in ")}]":
            depth -= 1
            if depth == 0:
                return frozenset(re.findall(r'"([^"]*)"', body[start:index])) or None
    return None

harness, scanner, verifier = sys.argv[1:]
sets = {
    "harness": field_set(harness, "KEEP"),
    "scanner": field_set(scanner, "JOURNAL_KEYS"),
    "verifier": field_set(verifier, "ALLOWED"),
}
unread = sorted(where for where, fields in sets.items() if fields is None)
if unread:
    print("no field set read from: " + ", ".join(unread))
elif len(set(sets.values())) != 1:
    print(" ".join(f"{where} {sorted(fields)}" for where, fields in sets.items()))
else:
    print("yes")
PY
)"

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
                 "journal-no-entries|1|tensorplate-agent: journal line 1 is not a JSON record" \
                 "journal-not-object|1|tensorplate-agent: journal line 1 is not a JSON record" \
                 "journal-trailing-line|1|tensorplate-agent: journal line 2 is not a JSON record" \
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
  # Whatever the verdict on a capture, its raw copy is gone: a harness
  # that skipped the removal on a failing capture leaves host metadata
  # in the scratch space for the next thing that collects logs.
  check "  and no raw capture is left in the harness's scratch space" 0 \
    "$(scratch_holding_host_metadata)"
  if [[ "$mode" == journal-trailing-line ]]; then
    # The record before the refused line was filed, and filed projected:
    # a refusal does not copy the raw capture through.
    check "  and what was filed before the refusal is projected" "1 none none" \
      "$(journal_projection "${evidence}/agent-journal.txt" \
         MESSAGE _SYSTEMD_UNIT _SYSTEMD_INVOCATION_ID)"
  fi
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

# Whether a check filed its verdict document. `_emit` writes one only
# after that check passed, so a document that is there is a check that
# passed and one that is not is a check that never did.
filed() {
  [[ -s "$1" ]] && printf 'yes' || printf 'no'
}

# The doctor fixture renders this row's host_os the way the CLI does, so
# a later tightening of the harness's --host-os-phrase can be read off
# it rather than guessed at. The image identity is not transcribed here:
# it is the one the repository records for this exact row, and platform
# matching is exact string equality, so a fixture that spelled it
# differently would make a tightened phrase pass here and fail on every
# device.
check "the doctor fixture's host_os carries this row's recorded image identity" yes \
  "$(python3 - "$0" "${repo_root}/test/platform/host_identity/jetson-orin-nano-8gb-jp62.json" <<'@H@'
import json, re, sys

verifier, recorded = sys.argv[1:]
expect = json.load(open(recorded, encoding="utf-8"))["expect"]
# What the CLI writes before the exact facts: "<os_name> <os_version>
# (<image_identity>)", and no machine type, because this row has none.
head = "{} {} ({})".format(expect["os_name"], expect["os_version"],
                           expect["image_identity"])
body = open(verifier, encoding="utf-8").read()
match = re.search(r'^    host_os="([^"]*)"$', body, re.M)
if not match:
    print("no: the fixture no longer assigns host_os on its own line")
elif not match.group(1).startswith(head):
    print("no: {!r} does not start with {!r}".format(match.group(1), head))
elif expect["machine_type"] is not None:
    print("no: the recorded row now expects a machine type")
else:
    print("yes")
@H@
)"

# --- offline regressions.
#
# Every way the offline stage could certify a device it did not deny, an
# identity it did not establish, an appliance that stopped working, or a
# device it did not put back. Each mode must fail the stage AND leave the
# network policy as it found it: a run that ends with the appliance
# denied cannot reach anything, including whatever the operator would use
# to recover it.
#
# `crash-loop passed before it` on every one of them: the mode has to be
# reaching the offline stage rather than breaking an earlier one, which
# is what would make a stage that asserts nothing look covered.
for mode in offline-denial-inert offline-localhost-shorthand offline-drop-in-extra-directive \
            offline-unit-dead offline-persistent-drop-in \
            offline-deny-restart-ignored offline-restore-restart-ignored \
            offline-drop-in-not-removed offline-cleanup-invocation-unreadable \
            offline-leftover-drop-in offline-leftover-drop-in-observability \
            offline-leftover-dangling-symlink \
            offline-probe-leaks offline-probe-crashes offline-cli-probe-crashes \
            offline-transient-not-denied \
            offline-service-filter-not-attached offline-cli-filter-not-attached-deploy \
            offline-control-refused offline-unit-control-refused offline-control-child-fails \
            offline-agent-socket-unreachable \
            offline-status-other-deployment offline-status-not-loopback \
            offline-status-after-deploy-other-deployment \
            offline-doctor-live-metadata offline-doctor-recorded-metadata \
            offline-doctor-no-l4t offline-doctor-failing offline-doctor-row-warning \
            offline-identity-live-metadata offline-identity-records offline-identity-twice \
            offline-deploy-not-active offline-deploy-other-id offline-infer-garbled \
            offline-signal-int offline-signal-term offline-signal-hup; do
  evidence="${td}/stages-${mode}"
  # The run exits with the failed stage's status: a crashing probe's is
  # the module's for a failure it did not anticipate, and a transient
  # unit with no filter is refused by the first CLI call's own probe.
  expected_status=1
  case "$mode" in
    offline-probe-crashes|offline-cli-probe-crashes) expected_status=70 ;;
    offline-transient-not-denied|offline-cli-filter-not-attached-deploy) expected_status=71 ;;
    offline-signal-int) expected_status=130 ;;
    offline-signal-term) expected_status=143 ;;
    offline-signal-hup) expected_status=129 ;;
  esac
  # What each mode leaves behind. A leftover from an earlier run is not
  # this run's to remove, and the stage refuses before it installs
  # anything; a removal the device ignored is the one shape where the
  # readback, not the removal, is what fails.
  expected_leftovers=0
  case "$mode" in
    offline-leftover-drop-in|offline-leftover-drop-in-observability|offline-leftover-dangling-symlink)
      expected_leftovers=1 ;;
    offline-drop-in-not-removed) expected_leftovers=2 ;;
  esac
  check "${mode} fails the run" "$expected_status" "$(run_stages "$mode" "$evidence" "")"
  check "  crash-loop passed before it" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and offline is recorded as a failure, not a pass" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" offline)"
  check "  and the network policy is put back anyway" "$expected_leftovers" \
    "$(offline_drop_ins_left)"
  # The stage stopped where the mode broke it, and said so: the step that
  # failed, and the check inside it that did.
  step_failed=""
  failure=""
  case "$mode" in
    offline-leftover-drop-in|offline-leftover-dangling-symlink)
      step_failed="no denial drop-in is already installed for tensorplate-agent"
      failure="a validation denial drop-in for tensorplate-agent is already installed" ;;
    offline-leftover-drop-in-observability)
      step_failed="no denial drop-in is already installed for tensorplate-observability"
      failure="a validation denial drop-in for tensorplate-observability is already installed" ;;
    offline-control-refused)
      step_failed="control probe"
      failure="control_not_refused:udp_test_net_v4" ;;
    offline-control-child-fails)
      step_failed="control probe"
      failure="control_completed:udp_test_net_v4_from_child" ;;
    offline-agent-socket-unreachable)
      step_failed="control probe"
      failure="control_allowed:unix_agent_socket" ;;
    offline-unit-control-refused)
      step_failed="control inside the tensorplate-agent control group"
      failure="control_not_refused:udp_test_net_v4" ;;
    offline-localhost-shorthand)
      step_failed="the installed tensorplate-agent drop-in is the rendered runtime one"
      failure="drop_in_uses_the_localhost_shorthand" ;;
    offline-drop-in-extra-directive)
      step_failed="the installed tensorplate-agent drop-in is the rendered runtime one"
      failure="drop_in_unexpected_directive:ExecStartPre" ;;
    offline-denial-inert)
      step_failed="both units read back as denied"
      failure="tensorplate-agent.service:denies_every_address" ;;
    offline-unit-dead)
      step_failed="both units read back as denied"
      failure="tensorplate-agent.service:unit_loaded" ;;
    offline-cleanup-invocation-unreadable)
      failure="no current invocation ID for tensorplate-agent" ;;
    offline-signal-int|offline-signal-term|offline-signal-hup) ;;
    offline-persistent-drop-in)
      step_failed="both units read back as denied"
      failure="tensorplate-agent.service:no_persistent_drop_in" ;;
    offline-deny-restart-ignored)
      step_failed="both units read back as denied"
      failure="tensorplate-agent.service:restarted_under_the_denial" ;;
    offline-restore-restart-ignored)
      step_failed="both units read back as none"
      failure="tensorplate-agent.service:restarted_without_the_denial" ;;
    offline-drop-in-not-removed)
      step_failed="both units read back as none"
      failure="drop_in_removed:tensorplate-agent.service.d" ;;
    offline-service-filter-not-attached)
      step_failed="the denial is enforced inside the tensorplate-observability control group"
      failure="refused:udp_test_net_v4" ;;
    offline-probe-leaks)
      step_failed="the denial is enforced inside the tensorplate-agent control group"
      failure="refused:udp_test_net_v4" ;;
    offline-identity-live-metadata)
      step_failed="the agent established no machine type, and recorded none"
      failure="machine_type_source_is_the_expected_one" ;;
    offline-identity-records)
      step_failed="the agent established no machine type, and recorded none"
      failure="record_not_rewritten_while_denied" ;;
    offline-identity-twice)
      step_failed="the agent established no machine type, and recorded none"
      failure="identity_logged_once" ;;
    offline-status-other-deployment)
      step_failed="status checks"
      failure="active_deployment" ;;
    offline-status-not-loopback)
      step_failed="status checks"
      failure="serving_url_on_the_allowed_loopback_address" ;;
    # The deploy reply is the CLI's account of its own request; this is
    # the agent's account of what it then had active. Only the check
    # after the second denied status read can catch the difference.
    offline-status-after-deploy-other-deployment)
      step_failed="the fresh deployment is active"
      failure="active_deployment" ;;
    offline-doctor-live-metadata|offline-doctor-recorded-metadata)
      step_failed="doctor resolves jetson-orin-nano-8gb-jp62 from this device's own facts"
      failure="host_os_names_the_forbidden_phrase" ;;
    offline-doctor-no-l4t)
      step_failed="doctor resolves jetson-orin-nano-8gb-jp62 from this device's own facts"
      failure="host_os_names_the_expected_phrase" ;;
    offline-doctor-failing)
      step_failed="doctor resolves jetson-orin-nano-8gb-jp62 from this device's own facts"
      failure="doctor_no_failing_findings" ;;
    offline-doctor-row-warning)
      step_failed="doctor resolves jetson-orin-nano-8gb-jp62 from this device's own facts"
      failure="platform_row_ok" ;;
    offline-deploy-not-active)
      step_failed="deploy checks"
      failure="deployment_phase_active" ;;
    offline-deploy-other-id)
      step_failed="deploy checks"
      failure="deployment_id" ;;
    offline-infer-garbled)
      step_failed="the engine returned its input unchanged under denial"
      failure="value mismatch at " ;;
    offline-probe-crashes)
      step_failed="probe inside the tensorplate-agent control group"
      failure="error: probe-unit failed unexpectedly: RuntimeError" ;;
    # The same crash one layer out: inside the probe a CLI call's own
    # transient unit takes of itself, where the module has to exit rather
    # than exec the call it could not clear. A different path from the
    # one above, which stops in a service's control group, three steps
    # earlier and before any CLI call is attempted.
    offline-cli-probe-crashes)
      step_failed="status under denial"
      failure="error: run-denied failed unexpectedly: RuntimeError" ;;
    offline-transient-not-denied)
      step_failed="status under denial"
      failure="the status unit does not enforce the denial, so tensorplate was not run" ;;
    offline-cli-filter-not-attached-deploy)
      step_failed="fresh deploy under denial"
      failure="the deploy unit does not enforce the denial, so tensorplate was not run" ;;
  esac
  if [[ -n "$step_failed" ]]; then
    check "  and the stage stopped at ${step_failed}" yes \
      "$(grep -Fq "step failed (exit ${expected_status}): ${step_failed}" \
           "${evidence}/offline.log" && echo yes || echo no)"
  fi
  if [[ "$mode" == offline-cli-probe-crashes ]]; then
    # The call is exec'd by the probe's own process, after the probe
    # classified: a probe that raised never got there, so the CLI stub
    # recorded nothing under the denial.
    check "  and no CLI call was made under the denial" 0 \
      "$(python3 -c 'import json,sys
print(sum(1 for line in open(sys.argv[1], encoding="utf-8")
          if json.loads(line)["denied"] == "1"))' "${appliance}/cli-calls.jsonl")"
  fi
  if [[ "$mode" == offline-status-after-deploy-other-deployment ]]; then
    # Reached only because everything before it passed: the first denied
    # status read, doctor, the deploy and the deploy's own check. Without
    # this the case would be another way of failing `status checks` and
    # would say nothing about the step it exists for.
    check "  having passed the first status read and the deploy checks" "yes yes" \
      "$(filed "${evidence}/offline-status-check.json") $(filed "${evidence}/offline-deploy-check.json")"
    # And the certificate cannot be filed without this step's verdict:
    # the stage stopped before it, so no offline-runtime.json exists.
    check "  and no offline certificate was filed" no \
      "$([[ -e "${evidence}/offline-runtime.json" ]] && echo yes || echo no)"
  fi
  if [[ -n "$failure" ]]; then
    check "  for the reason the case provokes" yes \
      "$(logged "${evidence}/offline.log" "$failure")"
  fi
  # A control that cannot be a baseline is refused as it is taken, before
  # anything is denied or restarted: a device that cannot provide one
  # fails the stage unchanged.
  case "$mode" in
    offline-control-refused|offline-control-child-fails|offline-agent-socket-unreachable|offline-unit-control-refused)
      check "  before any drop-in is installed or any service restarted for it" "0 0" \
        "$(printf '%s %s' \
           "$(grep -c '^install -D -m 0644 ' "${appliance}/sudo.log" || true)" \
           "$(sed -n '/python3 .*linux_offline_runtime\.py control /,$p' "${appliance}/sudo.log" \
              | grep -c 'systemctl restart' || true)")"
      ;;
  esac
  # An interrupted stage still has to put the network policy back, from
  # the signal handler rather than from the stage, and the restoration is
  # read back from systemd there too.
  case "$mode" in
    offline-signal-int|offline-signal-term|offline-signal-hup)
      check "  and the restore was read back from systemd, not assumed" "False 2" \
        "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))
print(r["denied"], r["drop_ins_removed"])' "${evidence}/offline-restored.json" 2>/dev/null \
          || echo absent)"
      ;;
  esac
  # A call that never ran because its own unit did not enforce the denial
  # is a call that never ran at all.
  case "$mode" in
    offline-transient-not-denied)
      check "  and no CLI call ran while the services were denied" 0 \
        "$(python3 -c 'import json,sys
calls=[json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
print(sum(1 for call in calls if call["dropins"]))' "${appliance}/cli-calls.jsonl")"
      ;;
    offline-cli-filter-not-attached-deploy)
      check "  and the calls before it were made, the deploy and everything after it not" \
        "status doctor" \
        "$(python3 -c 'import json,sys
calls=[json.loads(line) for line in open(sys.argv[1], encoding="utf-8")]
print(" ".join(call["command"] for call in calls if call["denied"] == "1"))' \
          "${appliance}/cli-calls.jsonl")"
      ;;
  esac
done

# --- every privileged step the offline stage takes, failed on its own.
#
# The same rule the other stages are held to: errexit is suspended inside
# a stage body, so an unguarded `sudo ...` there runs on and the stage
# returns 0. Each command below is failed by itself, and the stage has to
# record a failure, name the step, and put the network policy back.
#
# The first seven are the stage body's, where the stage stops at the
# first failure. The last three are the cleanup's, which is deliberately
# best-effort -- a cleanup that stopped at its first failure would leave
# the rest of the device denied -- so what is required of those is that
# the failure is named and the stage fails, not that nothing followed.
#
# `systemctl restart`, `systemctl daemon-reload` and `journalctl` are
# taken by earlier stages too, spelled the same way, so those four are
# narrowed to the side of this stage they belong to: a substring alone
# would always fail the earlier stage and never reach this one. Every
# other command named here appears only in this stage.
agent_drop_in="$(offline_drop_in tensorplate-agent)"
for injected_case in "install -D -m 0644=install the tensorplate-agent denial drop-in=0" \
                     "systemctl daemon-reload=reload systemd=0" \
                     "systemd-run=control probe=0" \
                     "journalctl@denied=capture the tensorplate-agent journal=0" \
                     "control-unit=control inside the tensorplate-agent control group=0" \
                     "probe-unit=probe inside the tensorplate-agent control group=0" \
                     "systemctl restart@denied=restart both services under denial=0" \
                     "systemctl daemon-reload@restored=reload systemd=0" \
                     "systemctl restart@restored=restart both services without denial=0" \
                     "rm -f ${agent_drop_in}=remove the tensorplate-agent drop-in=1"; do
  injected="${injected_case%%=*}"
  rest="${injected_case#*=}"
  step_name="${rest%=*}"
  expected_leftovers="${rest##*=}"
  evidence="${td}/stages-offline-sudo-fails-${step_name// /-}"
  check "a failing '${step_name}' is recorded as a failed offline stage" fail \
    "$(run_stages ok "$evidence" "$injected" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" offline)"
  check "  crash-loop passed before it" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and the failed step is the one named" yes \
    "$(logged "${evidence}/offline.log" "step failed (exit 9): ${step_name}")"
  check "  and the network policy is put back anyway" "$expected_leftovers" \
    "$(offline_drop_ins_left)"
  check "  and no offline certificate was filed" no \
    "$([[ -e "${evidence}/offline-runtime.json" ]] && echo yes || echo no)"
done
# The one case that cannot put the policy back: the removal itself is
# what fails, so the stage says how to remove it by hand rather than
# leaving the operator to work the path out.
check "a drop-in the device would not remove is named in the recovery instruction" yes \
  "$(logged "${td}/stages-offline-sudo-fails-remove-the-tensorplate-agent-drop-in/offline.log" \
     "the offline network denial may still be in place; remove it with: sudo rm -f")"

# A probe that crashes, from a checkout under a home directory -- the
# shape of CI's own /home/runner checkout, where an uncaught traceback
# would quote the module's path into offline.log and the publication scan
# would refuse the run's evidence. The module names the subcommand and
# the exception type and nothing else, so the evidence stays publishable.
# The harness, the runner and the module are copied there together,
# because the harness finds the module beside itself.
home_checkout="${td}/home/tp-reviewer/checkout"
mkdir -p "${home_checkout}/tools/validation"
for file in jetson-lifecycle.sh lifecycle-stages.sh linux_offline_runtime.py \
            create_trt_identity_bundle.sh verify_trt_identity_response.py \
            trt_identity_engine.cpp; do
  cp "${repo_root}/tools/validation/${file}" "${home_checkout}/tools/validation/${file}"
done
home_evidence="${td}/stages-home-offline-probe-crashes"
harness_under_test="${home_checkout}/tools/validation/jetson-lifecycle.sh"
check "a crashing probe from a checkout under a home directory fails the run" 70 \
  "$(run_stages offline-probe-crashes "$home_evidence" "")"
harness_under_test=""
check "  and offline is recorded as a failure" fail \
  "$(stage_status "${home_evidence}/lifecycle-report.json" offline)"
check "  and the network policy is put back anyway" 0 "$(offline_drop_ins_left)"
check "  and the stage log names the subcommand and the exception type, and no path" "yes yes no" \
  "$(printf '%s %s %s' \
     "$(grep -Fxq 'error: probe-unit failed unexpectedly: RuntimeError' \
          "${home_evidence}/offline.log" && echo yes || echo no)" \
     "$(grep -Fxq 'step failed (exit 70): probe inside the tensorplate-agent control group' \
          "${home_evidence}/offline.log" && echo yes || echo no)" \
     "$(grep -Fq "$home_checkout" "${home_evidence}/offline.log" && echo yes || echo no)")"

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
# Every file the fixture device carries in its durable state directory,
# with its digest: what the rollback's preservation check has to hold the
# saved copy to, file by file. The agent writes the first two on a deploy
# and the observability unit's snapshot sink writes the third, so the
# directory the rollback sets aside has three owners and the harness
# knows none of their names.
state_fixture='{"fixture":"durable state"}'
agent_bak_fixture='{"fixture":"durable state","recovery":true}'
snapshot_fixture='{"fixture":"observability snapshot"}'
sha256_of() {
  printf '%s\n' "$1" | python3 -c 'import hashlib, sys
print(hashlib.sha256(sys.stdin.buffer.read()).hexdigest())'
}
state_sha256="$(sha256_of "$state_fixture")"
agent_bak_sha256="$(sha256_of "$agent_bak_fixture")"
snapshot_sha256="$(sha256_of "$snapshot_fixture")"

baseline_evidence="${td}/stages-baseline"
check "a run with a baseline completes" "0" "$(run_baseline_stages ok "$baseline_evidence" "")"
for stage in install deploy-smoke status-logs restart crash-loop offline upgrade rollback; do
  check "  ${stage} is recorded as a pass" "pass" \
    "$(stage_status "${baseline_evidence}/lifecycle-report.json" "$stage")"
done
# Nothing is skipped, so this is the run shape that can report a pass at
# all: the release gate reads the outcome, and an incomplete row is
# refused whatever its stages say.
check "  nothing is skipped" 0 \
  "$(python3 -c 'import json,sys
report=json.load(open(sys.argv[1]))
print(sum(1 for s in report["stages"] if s["status"] == "skipped"))' \
    "${baseline_evidence}/lifecycle-report.json")"
check "  and all eight canonical stages make the run a pass" pass \
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
check "  and every file that was set aside survived the rollback with its bytes" \
  "${state_fixture}|${agent_bak_fixture}|${snapshot_fixture}" \
  "$(cd "${appliance}/varlib/state.bak" 2>/dev/null &&
     printf '%s|%s|%s' "$(cat state.json 2>/dev/null || echo missing)" \
       "$(cat state.json.bak 2>/dev/null || echo missing)" \
       "$(cat observability-snapshot.json 2>/dev/null || echo missing)")"
# The digests the preservation check compares against are only worth
# anything if they were taken from the stopped agent's own copies, before
# the move left no original to compare with. Consecutive reads of the
# same directory collapse to one token, so this says what happened in
# what order without pinning how many files the directory holds.
check "  listed and digested with the services stopped and before the move" \
  "systemctl-stop ls-state sha256sum-state mv" \
  "$(third="$(install_line 3)"
     tail -n "+$((third + 1))" "${appliance}/sudo.log" |
       sed -n -e 's/^systemctl stop tensorplate-agent tensorplate-observability$/systemctl-stop/p' \
              -e 's#^ls -A /var/lib/tensorplate/state$#ls-state#p' \
              -e 's#^sha256sum /var/lib/tensorplate/state/.*$#sha256sum-state#p' \
              -e 's#^mv -T /var/lib/tensorplate/state /var/lib/tensorplate/state.bak$#mv#p' |
       uniq | tr '\n' ' ' | sed 's/ $//')"
# Every file, not just the one the harness would have known to look for.
check "  and the saved copy is read back after the baseline install, file by file" \
  "yes yes yes yes" \
  "$(fourth="$(install_line 4)"
     echo "$(sudo_after "$fourth" 'ls -A /var/lib/tensorplate/state.bak')" \
       "$(sudo_after "$fourth" 'sha256sum /var/lib/tensorplate/state.bak/state.json')" \
       "$(sudo_after "$fourth" 'sha256sum /var/lib/tensorplate/state.bak/state.json.bak')" \
       "$(sudo_after "$fourth" 'sha256sum /var/lib/tensorplate/state.bak/observability-snapshot.json')")"
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
  "jetson-lifecycle-smoke jetson-lifecycle-smoke-offline jetson-lifecycle-smoke-baseline jetson-lifecycle-smoke-rollback" \
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
        "jetson-lifecycle-smoke jetson-lifecycle-smoke-offline jetson-lifecycle-smoke-baseline" \
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
                 "rollback-state-not-preserved|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-empties-backup|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-truncates-backup|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-rewrites-backup|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-empties-agent-bak|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-truncates-agent-bak|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-rewrites-agent-bak|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-deletes-agent-bak|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-empties-snapshot|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-deletes-snapshot|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-adds-state-file|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-empties-state-dir|1|no|step failed (exit 1): the set-aside state is preserved, file by file" \
                 "rollback-state-file-missing|1|yes|step failed (exit 1): digest the durable state before setting it aside" \
                 "rollback-digest-not-hex|1|yes|step failed (exit 1): digest the durable state before setting it aside" \
                 "rollback-destroys-backup-then-fails|1|yes|step failed (exit 1): install.sh" \
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
        "$(err_says "$evidence" '/etc/tensorplate conffiles are kept.') $(err_says "$evidence" 'durable state is at /var/lib/tensorplate/state.bak and still matches the digests taken before the move.')"
      ;;
    rollback-digest-not-hex)
      # A leading field that is not sha256 hex is refused where it is
      # read, not carried forward: two files digested through such a
      # sha256sum would otherwise compare equal and the stage would
      # credit the rollback with preserving state it never read.
      # The file named is the first entry of the state directory in the
      # order the manifest reads it, C-sorted, not whichever name the
      # harness happens to care about.
      check "  and names the file it could not digest" yes \
        "$(logged "${evidence}/rollback.log" \
           'could not compute a sha256 of /var/lib/tensorplate/state/observability-snapshot.json')"
      check "  with nothing set aside or removed" "no no" \
        "$(third="$(install_line 3)"
           echo "$(sudo_after "$third" 'mv -T')" "$(sudo_after "$third" 'apt-get remove')")"
      ;;
    rollback-state-not-preserved)
      # The set-aside directory is not there at all: the check names what
      # it could not read and stops there. Reporting it as CHANGED would
      # be a different claim -- that something was read and compared --
      # and would be made on an empty digest, so the read has to fail the
      # check rather than fall through to the comparison.
      check "  and names the directory it could not read" yes \
        "$(logged "${evidence}/rollback.log" \
           "ls: cannot access '/var/lib/tensorplate/state.bak': No such file or directory")"
      check "  and does not report it as changed" no \
        "$(logged "${evidence}/rollback.log" 'the rollback did not preserve')"
      # Nor as the empty directory the case below is about. A listing
      # whose failure went unchecked would read as a directory holding
      # nothing, which is a different fact with a different recovery:
      # there is a set-aside copy to inspect in one and none in the other.
      check "  nor as a directory that is there and holds nothing" no \
        "$(logged "${evidence}/rollback.log" \
           'holds no files; there is no durable state here to preserve')"
      ;;
    rollback-empties-backup|rollback-truncates-backup|rollback-rewrites-backup|\
    rollback-empties-agent-bak|rollback-truncates-agent-bak|rollback-rewrites-agent-bak|\
    rollback-empties-snapshot)
      # Which file each mode destroyed, and the digest it had when the
      # services were stopped. The pathname is still a regular file in
      # every one of them, so `test -f` on it would have passed them all
      # -- including the three that leave the agent's own state.json
      # untouched and destroy the copy it recovers FROM.
      case "$mode" in
        rollback-empties-backup|rollback-truncates-backup|rollback-rewrites-backup)
          destroyed=state.json
          was="$state_sha256"
          ;;
        rollback-empties-snapshot)
          destroyed=observability-snapshot.json
          was="$snapshot_sha256"
          ;;
        *)
          destroyed=state.json.bak
          was="$agent_bak_sha256"
          ;;
      esac
      check "  and ${destroyed} is still a regular file" yes \
        "$([[ -f "${appliance}/varlib/state.bak/${destroyed}" ]] && echo yes || echo no)"
      check "  and the failure names the file whose contents did not survive" yes \
        "$(logged "${evidence}/rollback.log" \
           "the rollback did not preserve /var/lib/tensorplate/state.bak/${destroyed}: sha256 was")"
      check "  and reports the digest that file had when the services were stopped" yes \
        "$(logged "${evidence}/rollback.log" "sha256 was ${was} when the services were stopped")"
      # The checks after it intentionally do not load the set-aside state,
      # so they cannot stand in for this one: the stage has to stop here.
      check "  and the stage stopped before reading the agent back" no \
        "$(logged "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-deletes-agent-bak|rollback-deletes-snapshot)
      # Gone rather than damaged. A per-file digest of the names the
      # harness knows would still have to notice this; a check that
      # digested only state.json could not.
      if [[ "$mode" == rollback-deletes-agent-bak ]]; then gone=state.json.bak; else gone=observability-snapshot.json; fi
      check "  and the failure names the file the set-aside copy no longer holds" yes \
        "$(logged "${evidence}/rollback.log" \
           "the rollback did not preserve /var/lib/tensorplate/state.bak/${gone}: it was in the durable state when the services were stopped and the set-aside copy does not hold it")"
      check "  and the stage stopped before reading the agent back" no \
        "$(logged "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-adds-state-file)
      # Every file that was set aside is still there, byte for byte, and
      # the install put one more beside them. "Unchanged" has to mean the
      # directory, or an install that seeded the older agent with state
      # of its own would read as preservation.
      check "  and every file that was set aside is intact" "yes yes yes" \
        "$(cd "${appliance}/varlib/state.bak" &&
           echo "$([[ "$(cat state.json)" == "$state_fixture" ]] && echo yes || echo no)" \
             "$([[ "$(cat state.json.bak)" == "$agent_bak_fixture" ]] && echo yes || echo no)" \
             "$([[ "$(cat observability-snapshot.json)" == "$snapshot_fixture" ]] && echo yes || echo no)")"
      check "  and the failure names the file that was added" yes \
        "$(logged "${evidence}/rollback.log" \
           'the rollback did not preserve /var/lib/tensorplate/state.bak: it holds state.json.new, which the durable state did not when the services were stopped')"
      check "  and the stage stopped before reading the agent back" no \
        "$(logged "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-empties-state-dir)
      # A directory that is there and holds nothing is not a preserved
      # one. Saying so about the directory is the honest report: naming
      # whichever file the comparison happened to reach first would
      # describe one loss out of three.
      check "  and the set-aside directory is still there" yes \
        "$([[ -d "${appliance}/varlib/state.bak" ]] && echo yes || echo no)"
      check "  and the failure names the directory, not one file in it" yes \
        "$(logged "${evidence}/rollback.log" \
           '/var/lib/tensorplate/state.bak holds no files; there is no durable state here to preserve')"
      # The other half of the pair above: a directory that is there and
      # holds nothing is not a directory that could not be read.
      check "  rather than as a directory it could not read" no \
        "$(logged "${evidence}/rollback.log" 'ls: cannot access')"
      check "  and the stage stopped before reading the agent back" no \
        "$(logged "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-state-file-missing)
      # Refused where the manifest is taken, before the move: a state
      # directory without the agent's deployment state gives a manifest
      # that both sides can satisfy while preserving nothing this stage
      # is about.
      check "  and says which file the durable state is missing" yes \
        "$(logged "${evidence}/rollback.log" \
           '/var/lib/tensorplate/state holds no state.json: there is no deployment state for the rollback to preserve')"
      check "  with nothing set aside or removed" "no no" \
        "$(third="$(install_line 3)"
           echo "$(sudo_after "$third" 'mv -T')" "$(sudo_after "$third" 'apt-get remove')")"
      ;;
    rollback-destroys-backup-then-fails)
      # The install destroyed the saved state and then failed the way
      # install.sh does, after installing every package. The stage never
      # reaches its preservation check, so the stranded-device report --
      # the one document the operator acts on -- is what has to say that
      # the pathname it hands them is not what was set aside.
      check "  and the saved state.json is a zero-byte file" "yes 0" \
        "$([[ -f "${appliance}/varlib/state.bak/state.json" ]] && echo yes || echo no) \
$(wc -c <"${appliance}/varlib/state.bak/state.json" | tr -d ' ')"
      check "  and the preservation check never ran" no \
        "$(logged "${evidence}/rollback.log" 'the set-aside state is preserved, file by file')"
      check "  and the report says the saved state no longer matches" yes \
        "$(err_says "$evidence" \
           'durable state is at /var/lib/tensorplate/state.bak but NO LONGER matches the digests taken before the move; treat it as damaged.')"
      # Neither the bare pathname the report used to print, nor the line
      # it prints when the saved copy did survive.
      check "  rather than sending the operator to it unqualified" "no no" \
        "$(err_says "$evidence" 'durable state is at /var/lib/tensorplate/state.bak.') \
$(err_says "$evidence" 'durable state is at /var/lib/tensorplate/state.bak and still matches the digests taken before the move.')"
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
        "$(err_says "$evidence" '/etc/tensorplate conffiles are kept.') $(err_says "$evidence" 'durable state is at /var/lib/tensorplate/state.bak and still matches the digests taken before the move.')"
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

# The stranded report's read of the set-aside state is best-effort and
# runs from the exit path, so it has to be able to fail -- and a read that
# failed must not read as either answer. Same destructive install as the
# case above, with the privileged listing of the set-aside copy made to
# fail: the only reader of that directory in this mode is the report.
evidence="${td}/stages-rollback-stranded-state-unreadable"
check "a stranded report that cannot read the set-aside state still files" 1 \
  "$(run_baseline_stages rollback-destroys-backup-then-fails "$evidence" \
     'ls -A /var/lib/tensorplate/state.bak')"
check "  and is filed and printed whole" yes "$(stranded_filed "$evidence")"
check "  and says the set-aside state could not be read back" yes \
  "$(err_says "$evidence" \
     'durable state is at /var/lib/tensorplate/state.bak, but it could not be read back; whether it still matches the digests taken before the move is NOT known.')"
check "  rather than claiming it matches, or that it is damaged" "no no" \
  "$(err_says "$evidence" 'still matches the digests taken before the move.') \
$(err_says "$evidence" 'NO LONGER matches the digests taken before the move')"

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
      "$(err_says "$evidence" 'durable state is at /var/lib/tensorplate/state.bak and still matches the digests taken before the move.')"
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
