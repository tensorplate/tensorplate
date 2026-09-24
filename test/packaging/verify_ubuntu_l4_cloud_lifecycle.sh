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
python3 "${repo_root}/test/validation/baseline_publication_test.py"

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
assert set(run + skipped) == set(canonical), (
    f"harness covers {sorted(set(run + skipped))}, the schema names {sorted(canonical)}"
)
# Upgrade and rollback run when a baseline is supplied and are skipped
# otherwise, so each is named once each way. Every other stage is named
# exactly once, and always runs: nothing is skipped unconditionally.
assert sorted(run) == sorted(["install", "deploy-smoke", "status-logs", "restart",
                              "crash-loop", "offline", "upgrade", "rollback"]), sorted(run)
assert sorted(skipped) == sorted(["upgrade", "rollback"]), sorted(skipped)

# Offline must precede upgrade. Upgrade's clean baseline install deletes
# /var/lib/tensorplate, taking the boot-bound machine-type record with
# it, and the baseline release never wrote one -- so an offline stage
# after it would fail for want of evidence the ordering destroyed.
offline_call = re.search(r"^\s*lifecycle_stage\s+offline\s", body, re.M)
upgrade_call = re.search(r"^\s*lifecycle_stage\s+upgrade\s", body, re.M)
assert offline_call and upgrade_call, "the harness does not run both offline and upgrade"
assert offline_call.start() < upgrade_call.start(), \
    "the offline stage runs after upgrade, which has already deleted the machine-type record"

# Every skip states a reason. An unexplained skip is indistinguishable
# from a stage nobody thought about.
for stage in skipped:
    match = re.search(
        r"lifecycle_skip\s+" + re.escape(stage) + r"\s*\\\n\s*\"([^\"]+)\"", body
    )
    assert match, f"{stage} is skipped without a quoted reason"
    assert len(match.group(1)) > 40, f"{stage}'s skip reason is too thin: {match.group(1)}"
    # The operator reading a skipped upgrade or rollback needs to know
    # what would have run it.
    if stage in ("upgrade", "rollback"):
        assert "--baseline-assets-dir" in match.group(1), \
            f"{stage}'s skip reason does not name --baseline-assets-dir: {match.group(1)}"

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
print("stage coverage: 6 always run, upgrade and rollback run or skipped with a reason; "
      "offline before upgrade; digest recorded after install")
PY

# --- the offline stage's own rules, read from the harness text.
#
# Three of them cannot be driven from a stub run because they are about
# what the harness may never do, and one absence looks like any other.
python3 - "$harness" "${repo_root}/tools/validation/linux_offline_runtime.py" <<'PY'
import re, sys

harness_path, module_path = sys.argv[1:]
body = open(harness_path, encoding="utf-8").read()
module = open(module_path, encoding="utf-8").read()

# The mechanism is shared, not copied: the drop-in text, the transient
# unit's properties and the classification come from the module, so the
# Jetson harness adopts the same rule by calling the same file.
assert "linux_offline_runtime.py" in body, \
    "the harness does not use the shared offline-runtime module"
# Naming the properties to read them back is fine; giving one a value is
# a second copy of the rule that can drift from the module's.
spelled = re.search(r"IPAddress(?:Allow|Deny)=(?:any|localhost|[0-9A-Fa-f][0-9A-Fa-f:.]*)", body)
assert not spelled, (
    "the harness spells the address policy itself instead of taking it from the module: "
    + (spelled.group(0) if spelled else ""))

# Runtime drop-ins only. A path under /etc would outlive the run and the
# host's next reboot.
assert "/run/systemd/system" in module, "the module does not write runtime unit files"
for forbidden in re.findall(r"/etc/systemd/system\S*", body):
    raise AssertionError(f"the harness names a persistent unit path: {forbidden}")

# Every CLI call the offline stage makes goes through run_denied_cli: a
# denied transient unit that probes itself before the call. Read from the
# stage's own functions rather than from the whole file, which
# legitimately calls the CLI online in other stages.
stage = "".join(
    match.group(0) for match in re.finditer(
        r"^(?:stage_offline|stage_offline_in|offline_cli_under_denial)\(\) \{\n.*?^\}\n",
        body, re.M | re.S))
assert stage, "the offline stage functions are missing"
# One LOGICAL line at a time -- backslash continuations joined, whole-line
# comments dropped -- and the CLI's own prefix on that line. Matched over
# the raw text, `\s` spans newlines, so each "call" ran from the end of
# the previous match and a nearby comment mentioning run_denied satisfied
# the assertion for a call that had none.
logical = re.sub(r"\\\n\s*", " ", stage)
calls = 0
for line in logical.splitlines():
    line = line.strip()
    if line.startswith("#"):
        continue
    found = re.search(r"(?<![\w.-])tensorplate\s", line)
    if not found:
        continue
    calls += 1
    prefix = line[: found.start()]
    assert "run_denied_cli" in prefix, \
        f"an offline-stage CLI call is not run in a self-probing denied unit: {line!r}"
assert calls >= 4, \
    "the offline stage no longer runs status, doctor, a deploy and an inference"

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
port_source = re.search(r'serving-port\s+--status "\$\{EVIDENCE_DIR\}/(\S+?)"', logical)
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
print("offline stage: shared module, runtime drop-ins only, every CLI call denied, control first")
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
# The publication helper is tested above with mocked HTTP. All harness
# probes route that helper alone to a local response fixture; every other
# Python invocation still uses the real interpreter. There is no product
# environment switch that can bypass the public-release check.
export TP_FAKE_REAL_PYTHON
TP_FAKE_REAL_PYTHON="$(command -v python3)"
export TP_FAKE_PUBLICATION_HELPER="${repo_root}/tools/validation/check-baseline-publication.py"
export TP_FAKE_PUBLICATION_STUB="${td}/publication.py"
export TP_FAKE_PUBLICATION_LOG="${td}/publication.log"
cat >"${stub_bin}/python3" <<'STUB'
#!/bin/sh
case "${1:-}" in
  "$TP_FAKE_PUBLICATION_HELPER")
    shift
    exec "$TP_FAKE_REAL_PYTHON" "$TP_FAKE_PUBLICATION_STUB" "$@"
    ;;
  */check-baseline-publication.py)
    echo 'unexpected publication helper path in fixture' >&2
    exit 9
    ;;
esac
exec "$TP_FAKE_REAL_PYTHON" "$@"
STUB
chmod +x "${stub_bin}/python3"
cat >"$TP_FAKE_PUBLICATION_STUB" <<'PY'
import argparse, hashlib, json, os, pathlib, sys

parser = argparse.ArgumentParser()
parser.add_argument("--assets-dir", required=True)
parser.add_argument("--release-tag", required=True)
args = parser.parse_args()
assets = pathlib.Path(args.assets_dir)
manifest, = assets.glob("tensorplate-*-artifacts.json")
assert args.release_tag == json.loads(manifest.read_text())["release"]["tag"]
with open(os.environ["TP_FAKE_PUBLICATION_LOG"], "a") as log:
    log.write(args.release_tag + " " + str(assets) + "\n")
mode = os.environ.get("TP_FAKE_MODE", "ok")
if mode in ("baseline-publication-draft", "baseline-publication-network-failure",
            "baseline-publication-checksum-mismatch"):
    print("baseline publication: synthetic refusal: " + mode, file=sys.stderr)
    sys.exit(1)
checksums = assets / "SHA256SUMS"
digest = hashlib.sha256(checksums.read_bytes()).hexdigest()
if mode == "baseline-publication-set-changed":
    # The public check succeeded, but the local set changes before the
    # harness records its baseline digest. The digest it returned must
    # remain binding rather than being replaced by a fresh local hash.
    with checksums.open("a") as output:
        output.write("\n")
print(digest)
PY
# Unconditionally, not only where the real tool is absent. On a Linux
# runner the real systemctl and dpkg exist, and a probe that let them
# through would query the actual host for services it never installed --
# answering about the runner rather than about the harness.
# systemd-run is here because preflight requires it: the offline stage
# runs every CLI call inside a transient unit, and a host without it
# cannot produce that stage. The appliance run intercepts the real
# invocation through its own sudo stub, never through this one.
for tool in sudo systemctl systemd-run; do
  printf '#!/bin/sh\nexit 0\n' >"${stub_bin}/${tool}"
  chmod +x "${stub_bin}/${tool}"
done

# dpkg, and the package database behind the stubbed appliance, in one
# script that dispatches on the name it is invoked by.
#
# As `dpkg` it compares versions, because the upgrade path is refused or
# admitted on that comparison, and a stub that answered 0 for everything
# would admit any pair. It orders versions with dpkg's own algorithm, so
# each sorts here where it sorts on a host: 0.2.1.rc.1-1 above 0.2.1-1,
# not refused as a shape the stub does not know, which would fail a
# harness that compared it for the stub's reason rather than the order's.
# It treats an empty version as older than any other, as dpkg does -- so
# a manifest with no version is admitted by the comparison alone, and
# only the harness's own version check refuses it -- and exits 2 on the
# versions dpkg rejects outright.
#
# As `fake-dpkg-db` it is the appliance's package database: install.sh
# installs a set's runtime packages from its manifest, apt-get purges and
# removes them, and dpkg-query lists them. It refuses to downgrade an
# installed package the way apt-get -y does without --allow-downgrades,
# which is what makes a rollback without a removal fail here as it does
# on a host.
fake_dpkg="${td}/fake-dpkg"
cat >"$fake_dpkg" <<'PY'
#!/usr/bin/env python3
import json, os, pathlib, re, shutil, sys

RUNTIME = (
    "tensorplate-common", "tensorplate-agent", "tensorplate-serving",
    "tensorplate-observability", "tensorplate-cli", "tensorplate-backend-python-pytorch",
)
PACKAGED_CLI_CONFIG = '{"fixture": "packaged cli config"}\n'
# What the sudo stub's agent starts write too, byte for byte.
FAKE_MACHINE_TYPE_RECORD = '{"schema_version":2,"machine_type":"g2-standard-8"}\n'
FAKE_INSTANCE_BINDING = '{"schema_version":1,"fixture":"instance binding"}\n'

# Debian version ordering as dpkg implements it (lib/dpkg/version.c and
# parsehelp.c), so the harness is ordered here the way a host orders it:
# `~` sorts before everything, even the end of the string, which is what
# puts 0.2.1~rc.1-1 below 0.2.1-1 and 0.2.1.rc.1-1 above it. A version
# dpkg rejects outright is an error (dpkg exits 2); one it only warns
# about is compared anyway, as dpkg compares it.
def parse_version(version):
    """(epoch, upstream, revision), or an error string where dpkg errors."""
    version = version.strip(" \t")
    if version == "":
        return "version string is empty"
    if any(c in " \t" for c in version):
        return "version string has embedded spaces"
    epoch = 0
    if ":" in version:
        head, _, version = version.partition(":")
        if head == "":
            return "epoch in version is empty"
        if not head.isdigit():
            return "epoch in version is not number"
        if version == "":
            return "nothing after colon in version number"
        epoch = int(head)
    upstream, hyphen, revision = version.rpartition("-")
    if not hyphen:
        upstream, revision = version, ""
    elif revision == "":
        return "revision number is empty"
    if upstream == "":
        return "version number is empty"
    return (epoch, upstream, revision)

def isdigit(c):
    return "0" <= c <= "9"

def isalpha(c):
    return c.isascii() and c.isalpha()

def order(c):
    if isdigit(c):
        return 0
    if isalpha(c):
        return ord(c)
    if c == "~":
        return -1
    return ord(c) + 256 if c else 0

def verrevcmp(a, b):
    i = j = 0
    while i < len(a) or j < len(b):
        first_diff = 0
        while (i < len(a) and not isdigit(a[i])) or (j < len(b) and not isdigit(b[j])):
            ac = order(a[i] if i < len(a) else "")
            bc = order(b[j] if j < len(b) else "")
            if ac != bc:
                return ac - bc
            i += 1
            j += 1
        while i < len(a) and a[i] == "0":
            i += 1
        while j < len(b) and b[j] == "0":
            j += 1
        while i < len(a) and isdigit(a[i]) and j < len(b) and isdigit(b[j]):
            if not first_diff:
                first_diff = ord(a[i]) - ord(b[j])
            i += 1
            j += 1
        if i < len(a) and isdigit(a[i]):
            return 1
        if j < len(b) and isdigit(b[j]):
            return -1
        if first_diff:
            return first_diff
    return 0

def compare_versions(left, right):
    """dpkg --compare-versions' ordering: <0, 0 or >0."""
    if left[0] != right[0]:
        return left[0] - right[0]
    return verrevcmp(left[1], right[1]) or verrevcmp(left[2], right[2])

def dpkg_version(version):
    # dpkg --compare-versions reads an empty argument as a blank version,
    # older than any other, rather than as an error.
    return (0, "", "") if version == "" else parse_version(version)

def dpkg(args):
    if args and args[0] == "-l":
        # `dpkg -l` prints each package's description, and the packaging
        # descriptions quote planning identifiers, which have no place in
        # published evidence. Nothing in the harness calls this today;
        # it is here so that a harness that went back to `dpkg -l` is
        # caught by the publication scan rather than passing quietly. The
        # identifier is built rather than written out: one belongs only
        # in the changelog.
        planning = "V%03d-E%02d-F%02d" % (21, 5, 1)
        print("||/ Name              Version   Architecture Description")
        print("+++-=================-=========-============-=====================")
        print(f"ii  tensorplate-agent 0.2.1-1   amd64        agent ({planning})")
        return 0
    if not args or args[0] != "--compare-versions":
        return 0
    if len(args) != 4:
        return 2
    left, op, right = dpkg_version(args[1]), args[2], dpkg_version(args[3])
    for text, parsed in ((args[1], left), (args[3], right)):
        if isinstance(parsed, str):
            print(f"dpkg: error: version '{text}' has bad syntax: {parsed}", file=sys.stderr)
            return 2
    cmp = compare_versions(left, right)
    results = {"lt": cmp < 0, "le": cmp <= 0, "eq": cmp == 0,
               "ne": cmp != 0, "ge": cmp >= 0, "gt": cmp > 0}
    if op not in results:
        return 2
    return 0 if results[op] else 1

mode = os.environ.get("TP_FAKE_MODE", "ok")

def db_path():
    return pathlib.Path(os.environ["TP_FAKE_DB"])

def load():
    path = db_path()
    if path.exists():
        return json.loads(path.read_text())
    return {"packages": {}, "phase": "none"}

def save(db):
    db_path().write_text(json.dumps(db, indent=2, sort_keys=True))

def installed_agent(db):
    agent = db["packages"].get("tensorplate-agent")
    return agent["version"] if agent and agent["status"] == "installed" else "none"

def install(db, directory):
    manifests = sorted(pathlib.Path(directory).glob("tensorplate-*-artifacts.json"))
    if not manifests:
        # The legacy fixture set has no manifest, and installs nothing.
        return 0
    manifest = json.loads(manifests[0].read_text())
    tag = manifest["release"]["tag"]
    # install.sh's selection, and apt's version: the one the package
    # carries, never the manifest's.
    wanted = {}
    for a in manifest["artifacts"]:
        if a.get("package") in RUNTIME and a.get("architecture") in ("amd64", "all") \
                and str(a.get("file", "")).endswith(".deb"):
            control = re.search(r"^Version: (.+)$",
                                (pathlib.Path(directory) / a["file"]).read_text(), re.M)
            if not control:
                print(f"E: {a['file']} is not a Debian package", file=sys.stderr)
                return 100
            wanted[a["package"]] = control.group(1)
    packages = db["packages"]
    for name, version in wanted.items():
        current = packages.get(name)
        if current and current["status"] == "installed" \
                and compare_versions(parse_version(version),
                                     parse_version(current["version"])) < 0:
            print("E: Packages were downgraded and -y was used without --allow-downgrades.",
                  file=sys.stderr)
            return 100
    agent = packages.get("tensorplate-agent")
    how = "fresh" if agent is None else {"installed": "over", "config-files": "after-remove"}.get(agent["status"], "other")
    phase = {
        ("fresh", "v0.2.1-rc.2"): "candidate",
        ("fresh", "v0.2.1-rc.1"): "baseline",
        ("over", "v0.2.1-rc.2"): "upgraded",
        ("after-remove", "v0.2.1-rc.1"): "rolled-back",
    }.get((how, tag), "other")
    varlib = pathlib.Path(os.environ["TP_FAKE_VARLIB"])
    cli_config = pathlib.Path(os.environ["TP_FAKE_CLI_CONFIG"])

    if mode == "baseline-install-noop" and phase == "baseline":
        return 0
    if mode == "candidate-set-changed" and phase == "baseline":
        with open(os.path.join(os.environ["TP_FAKE_CANDIDATE_DIR"], "SHA256SUMS"), "a") as sums:
            sums.write("\n")
    if mode == "upgrade-signal-term" and phase == "upgraded":
        # The sudo stub signals the harness, which is its parent.
        return 200

    for name, version in wanted.items():
        if mode == "upgrade-leaves-baseline-package" and phase == "upgraded" \
                and name == "tensorplate-serving":
            continue
        # At the right version but never configured, as a package whose
        # postinst failed is left.
        if mode == "upgrade-leaves-unpacked" and phase == "upgraded" \
                and name == "tensorplate-serving":
            packages[name] = {"status": "unpacked", "version": version}
            continue
        if mode == "rollback-leaves-candidate-package" and phase == "rolled-back" \
                and name == "tensorplate-cli":
            packages[name]["status"] = "installed"
            continue
        packages[name] = {"status": "installed", "version": version}
    if mode == "apt-source-installed":
        packages["tensorplate-apt-source"] = {"status": "installed", "version": "0.1.2-1"}
    if mode == "upgrade-leaves-unlisted-package" and phase == "upgraded":
        packages["tensorplate-unlisted"] = {"status": "installed", "version": "0.2.1~rc.1-1"}

    # install-paths.sh lays out the state directory at configure time,
    # and install.sh brings the services up, so the observability unit's
    # snapshot is there from the install on. Its path is fixed by
    # packaging/conf/observability.json, and it is durable state that
    # lives beside the agent's own: the rollback has to carry it across
    # too, which is why the preservation check is about the directory.
    (varlib / "state").mkdir(parents=True, exist_ok=True)
    (varlib / "state" / "observability-snapshot.json").write_text(
        '{"fixture": "observability snapshot", "schema_version": "0.1"}\n')
    if mode == "upgrade-loses-deployment" and phase == "upgraded":
        shutil.rmtree(varlib / "state")
        (varlib / "state").mkdir()

    # The agent install.sh brings up is online, so it records the machine
    # type: every release this harness moves between does, in the same
    # bytes on the same boot, exactly as the sudo stub's agent starts do.
    # Only the candidate's installer lays out the identity directory, and
    # only its agent binds the record to the instance there; the baseline
    # never touches it. The modes below break each of those in turn.
    record = varlib / "state" / "machine-type.json"
    if mode != "offline-no-machine-type-record":
        record.write_text(FAKE_MACHINE_TYPE_RECORD)
    if (mode == "upgrade-rewrites-record" and phase == "upgraded") \
            or (mode == "rollback-baseline-rewrites-record" and phase == "rolled-back"):
        record.write_text('{"schema_version":3,"machine_type":"g2-standard-8"}\n')
    binding = varlib / "identity" / "instance-binding.json"
    if phase in ("candidate", "upgraded"):
        binding.parent.mkdir(parents=True, exist_ok=True)
        if not (mode == "upgrade-no-binding" and phase == "upgraded"):
            binding.write_text(FAKE_INSTANCE_BINDING)
    if mode == "rollback-touches-binding" and phase == "rolled-back":
        binding.write_text('{"fixture": "rewritten by the baseline"}\n')
    # A real run has deleted /var/lib/tensorplate by now, so this plants
    # the directory where only the rollback's own refusal can catch it.
    if mode == "rollback-state-aside-exists" and phase == "upgraded":
        (varlib / "state.bak").mkdir()
        (varlib / "state.bak" / "state.json").write_text('{"fixture": "an earlier rollback"}\n')

    # A conffile is written only where none exists, as --force-confold
    # keeps an operator's copy; the reset modes model a package that
    # replaces it anyway.
    if not cli_config.exists() \
            or (mode == "upgrade-resets-conffile" and phase == "upgraded") \
            or (mode == "rollback-resets-conffile" and phase == "rolled-back"):
        cli_config.write_text(PACKAGED_CLI_CONFIG)
    # The set-aside state destroyed by the install that follows the
    # removal, while its pathname stays a regular file: emptied,
    # truncated, and replaced with different bytes, none of which a check
    # on the file's existence could tell from the original.
    #
    # The same shapes against the agent's recovery copy and against the
    # observability snapshot, plus one deletion and one addition: the
    # destruction the rollback has to be held to is destruction ANYWHERE
    # in the directory it set aside, not in the one name the harness
    # happens to know. A host whose state.json survives and whose
    # state.json.bak was emptied has lost exactly the copy that makes a
    # corrupt primary recoverable.
    aside = varlib / "state.bak"
    if phase == "rolled-back":
        # Gone outright by the time it is read back. Destroyed here rather
        # than in the move, because the rollback restores the machine-type
        # record out of the set-aside copy first; a move that lost it would
        # stop the stage at that restore instead of at the read-back this
        # mode is about.
        if mode == "rollback-state-not-preserved":
            shutil.rmtree(aside)
        elif mode == "rollback-empties-backup":
            (aside / "state.json").write_text("")
        # A genuine prefix of what the agent wrote, and -- below --
        # different bytes at exactly the same length: neither a size nor
        # a mode tells them from the original, only a digest does.
        elif mode == "rollback-truncates-backup":
            (aside / "state.json").write_text('{"active":"cloud-lifecy')
        elif mode == "rollback-rewrites-backup":
            (aside / "state.json").write_text('{"active":"cloud-lifecycle-other"}\n')
        elif mode == "rollback-empties-agent-bak":
            (aside / "state.json.bak").write_text("")
        elif mode == "rollback-rewrites-agent-bak":
            (aside / "state.json.bak").write_text('{"active":"cloud-lifecycle-other"}\n')
        elif mode == "rollback-deletes-agent-bak":
            (aside / "state.json.bak").unlink(missing_ok=True)
        elif mode == "rollback-empties-snapshot":
            (aside / "observability-snapshot.json").write_text("")
        elif mode == "rollback-deletes-snapshot":
            (aside / "observability-snapshot.json").unlink(missing_ok=True)
        # Emptied, exactly as rollback-empties-backup does to
        # state.json. The failure has to name "two words.json", not the
        # first word of it: a manifest line is "<name> <sha256>" and a
        # reader that split it on the FIRST space would report a file
        # called "two" whose digest began with the rest of the name.
        elif mode == "rollback-empties-spaced-file":
            (aside / "two words.json").write_text("")
        elif mode == "rollback-adds-state-file":
            (aside / "state.json.new").write_text('{"fixture": "not what was set aside"}\n')
        # Added, as rollback-adds-state-file does, under a name that
        # holds a space. The comparison that reports an added file reads
        # the set-aside manifest, not the one taken before the move, so
        # it splits its lines on its own: a reader that cut at the FIRST
        # space would report a file called "extra" that nobody wrote.
        elif mode == "rollback-adds-spaced-file":
            (aside / "extra file.json").write_text('{"fixture": "not what was set aside"}\n')
        # Every file gone while the directory stays: the set-aside copy
        # reads as a directory that is there and holds nothing, which
        # must not be what "unchanged" means.
        elif mode == "rollback-empties-state-dir":
            for entry in aside.iterdir():
                entry.unlink()
    db["phase"] = phase
    save(db)
    # The installer's later steps -- readiness, doctor -- can fail after
    # apt has already installed the set, which leaves nothing else amiss.
    if mode == "upgrade-install-fails" and phase == "upgraded":
        print("E: fixture installer failed after installing the packages", file=sys.stderr)
        return 1
    return 0

def forget(db, names, keep_conffiles):
    cli_config = pathlib.Path(os.environ["TP_FAKE_CLI_CONFIG"])
    for name in names:
        if name not in db["packages"]:
            continue
        if keep_conffiles:
            db["packages"][name]["status"] = "config-files"
        else:
            del db["packages"][name]
            if name == "tensorplate-cli" and cli_config.exists():
                cli_config.unlink()
    save(db)
    return 0

def database(args):
    db = load()
    command = args[0]
    if command == "query":
        if not db["packages"]:
            return 1
        for name, package in sorted(db["packages"].items()):
            print(f"{name} {package['status']} {package['version']}")
        return 0
    if command == "phase":
        print(db["phase"])
        return 0
    if command == "agent-version":
        print(installed_agent(db))
        return 0
    if command == "install":
        return install(db, args[1])
    if command == "purge":
        return forget(db, args[1:], keep_conffiles=False)
    if command == "remove":
        names = args[1:]
        if mode == "rollback-remove-leaves-backend":
            names = [n for n in names if n != "tensorplate-backend-python-pytorch"]
        return forget(db, names, keep_conffiles=mode != "rollback-remove-purges")
    return 9

if os.path.basename(sys.argv[0]) == "dpkg":
    sys.exit(dpkg(sys.argv[1:]))
sys.exit(database(sys.argv[1:]))
PY
cp "$fake_dpkg" "${stub_bin}/dpkg"
chmod +x "${stub_bin}/dpkg"

# dpkg-deb, which the harness uses to read the Version each package
# carries: what apt orders on and dpkg-query reports once it is installed.
# Each fixture .deb is a control-style text file.
cat >"${stub_bin}/dpkg-deb" <<'STUB'
#!/bin/sh
[ "$1" = -f ] && [ "$3" = Version ] || exit 9
# A file that is not a package, whatever its name says.
if ! grep -q '^Package: ' "$2" 2>/dev/null; then
  printf 'dpkg-deb: error: %s is not a Debian format archive\n' "$2" >&2
  exit 2
fi
sed -n 's/^Version: //p' "$2"
STUB
chmod +x "${stub_bin}/dpkg-deb"

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

# Release-shaped sets for upgrade and rollback: the installer, a manifest
# in the release build's shape, the packages, and SHA256SUMS over all of
# them. The manifest lists an arm64 build beside each amd64 one, as a
# published release's does, so the harness has to select by architecture.
# Each .deb is a control-style text file the dpkg-deb stub reads, named as
# the release publishes it: GitHub has no `~`, so a candidate's
# 0.2.1~rc.N-1 package is published as 0.2.1.rc.N-1. The fake package
# database installs each at the version it carries, as apt does.
#
# `set-rc1` rather than `assets-rc1`, so a search of the sudo log for the
# candidate's `assets-rc2/` path cannot match the baseline's lines.
fixtures="${td}/sets"
python3 - "$fixtures" <<'PY'
import hashlib, json, pathlib, sys

root = pathlib.Path(sys.argv[1])
PER_ARCH = ("tensorplate-agent", "tensorplate-serving", "tensorplate-observability", "tensorplate-cli")
ALL_ARCH = ("tensorplate-common", "tensorplate-backend-python-pytorch", "tensorplate-apt-source", "tensorplate")

def make(name, rc, *, manifest=True, release_fields=None, drop=(), versions=None,
         controls=None, corrupt=(), extra=(), listed_as_built=False):
    """A release set; rc=None makes the final release.

    listed_as_built makes it v0.2.1-rc.1 as GitHub serves it: the manifest
    and SHA256SUMS list each package under the `~` name dpkg gave it, and
    the file is present only under the `.` name GitHub served.
    """
    directory = root / name
    directory.mkdir(parents=True)
    tag = "v0.2.1" if rc is None else f"v0.2.1-rc.{rc}"
    deb_version = "0.2.1-1" if rc is None else f"0.2.1~rc.{rc}-1"
    published_version = deb_version.replace("~", ".")
    (directory / "install.sh").write_text(f"#!/bin/sh\n# fixture installer, {tag}\nexit 0\n")
    artifacts = []
    listed = ["install.sh"]
    for package in PER_ARCH + ALL_ARCH:
        if package in drop:
            continue
        for arch in (("amd64", "arm64") if package in PER_ARCH else ("all",)):
            file = f"{package}_{published_version}_{arch}.deb"
            # `versions` changes what the manifest declares, `controls` what
            # the package itself carries, and `corrupt` leaves a file that
            # is not a package at all under a good one's name.
            control = (f"Package: {package}\n"
                       f"Version: {(controls or {}).get(package, deb_version)}\n"
                       f"Architecture: {arch}\n")
            if package in corrupt:
                control = "not a Debian archive\n"
            (directory / file).write_text(control)
            if listed_as_built:
                (directory / file).rename(directory / f"{package}_{deb_version}_{arch}.deb")
                file = f"{package}_{deb_version}_{arch}.deb"
            listed.append(file)
            artifacts.append({
                "file": file,
                "package": package,
                "version": (versions or {}).get(package, deb_version),
                "architecture": arch,
            })
    artifacts.extend(extra)
    if manifest:
        release = {"project": "tensorplate", "version": "0.2.1", "tag": tag,
                   "provenance": "github-release", "unreleased": False}
        release.update(release_fields or {})
        manifest_name = f"tensorplate-{tag}-artifacts.json"
        (directory / manifest_name).write_text(json.dumps(
            {"release": release, "artifacts": artifacts}, indent=2) + "\n")
        listed.append(manifest_name)
    # GNU format, written directly so the fixture is the same on both
    # platforms rather than depending on which checksum tool is present.
    (directory / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256((directory / f).read_bytes()).hexdigest()}  {f}\n" for f in listed))
    if listed_as_built:
        for f in listed:
            if "~" in f:
                (directory / f).rename(directory / f.replace("~", "."))
    return directory

make("assets-rc2", 2)
make("set-rc1", 1)
make("set-rc1-nomanifest", 1, manifest=False)
make("set-rc1-snapshot", 1,
     release_fields={"provenance": "local-source-snapshot", "unreleased": True})
# Each half of the published-release refusal on its own, so neither can
# be dropped behind the other.
make("set-rc1-unreleased", 1, release_fields={"unreleased": True})
make("set-rc1-local-provenance", 1, release_fields={"provenance": "local-source-snapshot"})
make("set-rc1-missing-backend", 1, drop=("tensorplate-backend-python-pytorch",))
# A release also publishes wheels; one naming a runtime package for amd64
# is not a second .deb of it.
make("set-rc1-wheel-named-agent", 1, extra=({
    "file": "tensorplate_agent-0.2.1rc1-py3-none-any.whl",
    "package": "tensorplate-agent",
    "version": "0.2.1~rc.1-1",
    "architecture": "amd64",
},))
# dpkg --compare-versions reads an empty version as older than any other,
# so only the harness's own check refuses this set.
make("set-rc1-empty-version", 1, versions={"tensorplate-backend-python-pytorch": ""})
# The package carries a newer version than its manifest declares. Only a
# harness that reads the package sees it, and apt orders on the package.
make("set-rc1-deb-newer", 1, controls={"tensorplate-agent": "0.2.1~rc.3-1"})
make("set-rc1-deb-corrupt", 1, corrupt=("tensorplate-serving",))
# The final release, and a candidate baseline whose manifest records each
# version as its file name spells it, which is what reading versions off
# the published names gives. The packages still say 0.2.1~rc.1-1, which
# sorts below 0.2.1-1; the manifest's 0.2.1.rc.1-1 sorts above it, so a
# harness ordering on the manifest refuses this upgrade and one ordering
# on the packages admits it.
make("assets-final", None)
make("set-rc1-published-versions", 1,
     versions={p: "0.2.1.rc.1-1" for p in PER_ARCH + ALL_ARCH})
# v0.2.1-rc.1 as a download of it from GitHub held it.
make("set-rc1-as-served", 1, listed_as_built=True)
(make("set-rc1-noinstaller", 1) / "install.sh").unlink()
(make("set-rc1-nosums", 1) / "SHA256SUMS").unlink()
tampered = make("set-rc1-tampered", 1)
with open(tampered / "install.sh", "a") as installer:
    installer.write("# changed after SHA256SUMS was written\n")
PY

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
# stops before the first privileged command. The unit tree preflight
# looks for leftover denial drop-ins in is a fixture too, so a runner's
# own /run never decides a check. PREFLIGHT_PATH and PREFLIGHT_HARNESS
# replace the search path and the harness for the cases about those.
preflight_units="${td}/preflight-units"
mkdir -p "${preflight_units}/run/systemd/system"
PREFLIGHT_UNIT_ROOT="$preflight_units"
PREFLIGHT_PATH=""
PREFLIGHT_HARNESS=""
preflight() {
  local arch="$1" os_release="$2" nvidia="$3" python_bin="$4" evidence="$5" version="$6"
  shift 6
  set +e
  env PATH="${PREFLIGHT_PATH:-${stub_bin}:${PATH}}" \
    TP_CLOUD_ARCH="$arch" \
    TP_CLOUD_OS_RELEASE="$os_release" \
    TP_CLOUD_NVIDIA_VERSION="$nvidia" \
    TP_CLOUD_PYTHON="$python_bin" \
    TP_OFFLINE_UNIT_ROOT="$PREFLIGHT_UNIT_ROOT" \
    bash "${PREFLIGHT_HARNESS:-$harness}" \
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

# The offline stage deploys as <id>-offline, which the CLI validates as
# one filesystem path segment. Refused in preflight rather than five
# stages in, where the deploy would fail on a CLI validation error and
# read as a failure of the denial.
long_id="$(python3 -c 'print("d" * 121)')"
check "a deployment id whose offline form passes the 128-byte limit is refused" "1" \
  "$(preflight "${ok_args[@]}" "${td}/evidence-longid" 0.2.1 \
     --confirm RESET-TENSORPLATE --deployment-id "$long_id")"
check "  and names the id the offline stage would have deployed" yes \
  "$(grep -Fq "deploys as ${long_id}-offline" "${td}/preflight.err" && echo yes || echo no)"
check "  while the longest one that fits is accepted" "0" \
  "$(preflight "${ok_args[@]}" "${td}/evidence-fitting-id" 0.2.1 \
     --confirm RESET-TENSORPLATE --deployment-id "${long_id%d}")"
check "a deployment id outside the CLI's charset is refused" "1" \
  "$(preflight "${ok_args[@]}" "${td}/evidence-badid" 0.2.1 \
     --confirm RESET-TENSORPLATE --deployment-id 'cloud lifecycle/smoke')"
check "  and names the charset" yes \
  "$(grep -Fq "letters, digits, dot, dash or underscore" "${td}/preflight.err" \
     && echo yes || echo no)"

# The offline stage runs every CLI call in a transient unit, so a host
# without systemd-run is refused before anything is installed. The search
# path here holds what preflight runs and nothing else: on a Linux runner
# the real systemd-run sits in /usr/bin beside everything preflight
# needs, so prepending a directory could never take it away.
no_systemd_run="${td}/path-without-systemd-run"
mkdir -p "$no_systemd_run"
for tool in sudo systemctl python3 dpkg dpkg-deb; do
  ln -s "${stub_bin}/${tool}" "${no_systemd_run}/${tool}"
done
for tool in bash env dirname basename find awk cat sed grep head tr mkdir rm \
            uname id sha256sum; do
  if tool_path="$(command -v "$tool")"; then
    ln -s "$tool_path" "${no_systemd_run}/${tool}"
  fi
done
check "the minimal search path passes preflight with systemd-run added" "0" \
  "$(ln -s "${stub_bin}/systemd-run" "${no_systemd_run}/systemd-run"
     PREFLIGHT_PATH="$no_systemd_run" preflight "${ok_args[@]}" \
       "${td}/evidence-minimal-path" 0.2.1 --confirm RESET-TENSORPLATE
     rm -f "${no_systemd_run}/systemd-run")"
check "a host without systemd-run is refused" "1" \
  "$(PREFLIGHT_PATH="$no_systemd_run" preflight "${ok_args[@]}" \
     "${td}/evidence-no-systemd-run" 0.2.1 --confirm RESET-TENSORPLATE)"
check "  and names it" yes \
  "$(grep -Fq "missing required command: systemd-run" "${td}/preflight.err" \
     && echo yes || echo no)"

# The offline mechanism is a file beside the harness; a copy of the
# harness without it is refused rather than failing five stages in.
lone_harness="${td}/lone-harness/tools/validation/ubuntu-l4-cloud-lifecycle.sh"
mkdir -p "$(dirname "$lone_harness")"
cp "$harness" "$lone_harness"
check "a harness without the offline module beside it is refused" "1" \
  "$(PREFLIGHT_HARNESS="$lone_harness" preflight "${ok_args[@]}" \
     "${td}/evidence-lone-harness" 0.2.1 --confirm RESET-TENSORPLATE)"
check "  and names the missing file" yes \
  "$(grep -Fq "missing $(dirname "$lone_harness")/linux_offline_runtime.py" \
       "${td}/preflight.err" && echo yes || echo no)"
cp "${repo_root}/tools/validation/linux_offline_runtime.py" "$(dirname "$lone_harness")/"
check "  while the same copy with the module beside it passes" "0" \
  "$(PREFLIGHT_HARNESS="$lone_harness" preflight "${ok_args[@]}" \
     "${td}/evidence-lone-harness" 0.2.1 --confirm RESET-TENSORPLATE)"

# A denial drop-in an earlier run left behind would deny both services
# through install and every stage before offline. Refused in preflight,
# for either unit and for a dangling symlink, naming the path and how to
# remove it -- and with nothing changed.
for leftover_case in tensorplate-agent:file tensorplate-observability:file \
                     tensorplate-agent:symlink; do
  leftover_unit="${leftover_case%:*}"
  leftover_units="${td}/preflight-leftover-${leftover_unit}-${leftover_case#*:}"
  leftover_dir="${leftover_units}/run/systemd/system/${leftover_unit}.service.d"
  mkdir -p "$leftover_dir"
  if [[ "${leftover_case#*:}" == symlink ]]; then
    ln -s "${leftover_units}/nowhere" "${leftover_dir}/10-tensorplate-validation-offline.conf"
  else
    printf '[Service]\n' >"${leftover_dir}/10-tensorplate-validation-offline.conf"
  fi
  check "a leftover ${leftover_case#*:} denial for ${leftover_unit} is refused in preflight" "1" \
    "$(PREFLIGHT_UNIT_ROOT="$leftover_units" preflight "${ok_args[@]}" \
       "${td}/evidence-leftover" 0.2.1 --confirm RESET-TENSORPLATE)"
  check "  and says how to remove it" yes \
    "$(grep -Fq "remove it with: sudo rm -f ${leftover_dir}/10-tensorplate-validation-offline.conf && sudo systemctl daemon-reload && sudo systemctl restart ${leftover_unit}" \
         "${td}/preflight.err" && echo yes || echo no)"
  check "  and leaves it where it was" yes \
    "$([[ -L "${leftover_dir}/10-tensorplate-validation-offline.conf" \
          || -f "${leftover_dir}/10-tensorplate-validation-offline.conf" ]] && echo yes || echo no)"
done

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

# --- the upgrade path, refused or admitted before anything is installed.
preflight_upgrade() {
  local candidate="$1" baseline="$2" evidence="$3"
  set +e
  env PATH="${stub_bin}:${PATH}" \
    TP_CLOUD_ARCH=x86_64 \
    TP_CLOUD_OS_RELEASE="${td}/os-release.noble" \
    TP_CLOUD_NVIDIA_VERSION="${td}/nvidia-version" \
    TP_CLOUD_PYTHON="${td}/python-with-torch" \
    bash "$harness" \
      --assets-dir "$candidate" \
      --baseline-assets-dir "$baseline" \
      --bundle-dir "$bundle" \
      --evidence-dir "$evidence" \
      --tested-version 0.2.1 \
      --preflight-only --confirm RESET-TENSORPLATE \
      >"${td}/preflight.out" 2>"${td}/preflight.err"
  local status=$?
  set -e
  printf '%s' "$status"
}
preflight_said() {
  grep -Fq -- "$1" "${td}/preflight.err" && echo yes || echo no
}

check "a release baseline older than the candidate passes preflight" "0" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1" "${td}/evidence-upgrade-ok")"
check "  and preflight writes nothing" "no" \
  "$([[ -e "${td}/evidence-upgrade-ok" ]] && echo yes || echo no)"

check "an empty --baseline-assets-dir is refused, not read as no baseline" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "" "${td}/evidence-upgrade-empty")"
check "  and says the option needs a directory" yes \
  "$(preflight_said '--baseline-assets-dir must name a directory')"

check "a baseline with no artifact manifest is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-nomanifest" "${td}/evidence-upgrade-nomanifest")"
check "  and names the missing manifest" yes \
  "$(preflight_said 'the baseline set needs exactly one tensorplate-*-artifacts.json')"

check "a candidate with no artifact manifest is refused when a baseline is given" "1" \
  "$(preflight_upgrade "$assets" "${fixtures}/set-rc1" "${td}/evidence-upgrade-candidate-nomanifest")"
check "  and names the candidate's missing manifest" yes \
  "$(preflight_said 'the candidate set needs exactly one tensorplate-*-artifacts.json')"

check "a baseline without the backend package is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-missing-backend" "${td}/evidence-upgrade-nobackend")"
check "  and names the package it lacks" yes \
  "$(preflight_said 'exactly one tensorplate-backend-python-pytorch package for amd64')"

check "a snapshot baseline is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-snapshot" "${td}/evidence-upgrade-snapshot")"
check "  and says it is not a published release" yes \
  "$(preflight_said 'the baseline set is not a published release')"

check "a baseline labelled github-release but unreleased is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-unreleased" "${td}/evidence-upgrade-unreleased")"
check "  and names what its manifest records" yes \
  "$(preflight_said "unreleased=True provenance='github-release'")"

check "a released baseline from a local source snapshot is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-local-provenance" "${td}/evidence-upgrade-local-provenance")"
check "  and names what its manifest records" yes \
  "$(preflight_said "unreleased=False provenance='local-source-snapshot'")"

check "a baseline with no installer is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-noinstaller" "${td}/evidence-upgrade-noinstaller")"
check "  and names the missing installer" yes \
  "$(preflight_said "missing ${fixtures}/set-rc1-noinstaller/install.sh")"

check "a baseline with no SHA256SUMS is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-nosums" "${td}/evidence-upgrade-nosums")"
check "  and names the missing checksum file" yes \
  "$(preflight_said "missing ${fixtures}/set-rc1-nosums/SHA256SUMS")"

check "a wheel naming a runtime package is not counted as its .deb" "0" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-wheel-named-agent" "${td}/evidence-upgrade-wheel")"

check "a baseline package with an empty version is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-empty-version" "${td}/evidence-upgrade-empty-version")"
check "  and names the package and its version" yes \
  "$(preflight_said "the baseline set lists tensorplate-backend-python-pytorch at version '', which is not a Debian version")"

check "the candidate passed as its own baseline is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/assets-rc2" "${td}/evidence-upgrade-same")"
check "  because no package would be upgraded" yes \
  "$(preflight_said "the baseline's 0.2.1~rc.2-1 is not older than the candidate's 0.2.1~rc.2-1")"

check "a baseline newer than the candidate is refused" "1" \
  "$(preflight_upgrade "${fixtures}/set-rc1" "${fixtures}/assets-rc2" "${td}/evidence-upgrade-swapped")"
check "  because it would be a downgrade" yes \
  "$(preflight_said "the baseline's 0.2.1~rc.2-1 is not older than the candidate's 0.2.1~rc.1-1")"

# What apt orders on is the control Version inside each .deb. Earlier
# releases took the manifest's from the file name without reading the
# package, and the file name spells a candidate the way GitHub publishes
# it, so neither can stand in for the package.
check "a baseline whose .deb carries a newer version than its manifest is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-deb-newer" "${td}/evidence-upgrade-deb-newer")"
check "  and compares the version the package carries" yes \
  "$(preflight_said "tensorplate-agent: the baseline's 0.2.1~rc.3-1 is not older than the candidate's 0.2.1~rc.2-1")"
# The fake dpkg orders as dpkg does, so this passes because the harness
# compares 0.2.1~rc.1-1 with 0.2.1-1, and would fail if it compared the
# manifest's 0.2.1.rc.1-1, which sorts above the release.
check "a candidate baseline whose manifest spells its versions as its file names do upgrades to the release" "0" \
  "$(preflight_upgrade "${fixtures}/assets-final" "${fixtures}/set-rc1-published-versions" "${td}/evidence-upgrade-published-versions")"
check "the release is not a baseline for its own candidate" "1" \
  "$(preflight_upgrade "${fixtures}/set-rc1" "${fixtures}/assets-final" "${td}/evidence-upgrade-final-first")"
check "  because 0.2.1-1 sorts above 0.2.1~rc.1-1" yes \
  "$(preflight_said "tensorplate-common: the baseline's 0.2.1-1 is not older than the candidate's 0.2.1~rc.1-1")"
check "a baseline with a .deb dpkg-deb cannot read is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-deb-corrupt" "${td}/evidence-upgrade-deb-corrupt")"
check "  and names the file and what dpkg-deb did" yes \
  "$(preflight_said "the baseline set's tensorplate-serving_0.2.1.rc.1-1_amd64.deb does not carry a readable Debian Version in its control file: dpkg-deb exited 2 and reported '': dpkg-deb: error: ${fixtures}/set-rc1-deb-corrupt/tensorplate-serving_0.2.1.rc.1-1_amd64.deb is not a Debian format archive")"

# A download of v0.2.1-rc.1: its manifest lists each package under a `~`
# name the download does not hold. Refused as a file that is not there,
# not as a package dpkg-deb cannot read.
check "a baseline whose manifest lists packages it does not hold is refused" "1" \
  "$(preflight_upgrade "${fixtures}/assets-rc2" "${fixtures}/set-rc1-as-served" "${td}/evidence-upgrade-as-served")"
check "  and names the listed file that is not there" yes \
  "$(preflight_said "the baseline set's manifest lists tensorplate-common_0.2.1~rc.1-1_all.deb, which is not in ${fixtures}/set-rc1-as-served")"

# --- the stages, executed against a stubbed appliance.
#
# The five running stages issue real commands against a real install, so
# CI cannot run them for their own sake. What CI must be able to see is
# whether their assertions FIRE: lifecycle_stage calls a stage function
# from a tested context, which suspends errexit inside it, so a stage
# written the obvious way runs past its own failures and returns the
# status of its last command. That defect certifies a broken host as a
# validated one, and it is invisible to any amount of reading.
appliance="${td}/appliance"
mkdir -p "${appliance}/bin" "${appliance}/run" "${appliance}/log" "${appliance}/scratch"
real_mktemp="$(command -v mktemp)"

# python3, with the two helpers that must not do the real thing routed to
# fixtures: the publication check, and the offline probe. Every other
# Python invocation -- including every other subcommand of the offline
# module -- still runs the real interpreter on the real file.
cat >"${appliance}/bin/python3" <<'STUB'
#!/bin/sh
case "${1:-}" in
  "$TP_FAKE_PUBLICATION_HELPER")
    shift
    exec "$TP_FAKE_REAL_PYTHON" "$TP_FAKE_PUBLICATION_STUB" "$@"
    ;;
  */check-baseline-publication.py)
    echo 'unexpected publication helper path in fixture' >&2
    exit 9
    ;;
  */linux_offline_runtime.py)
    case "${2:-}" in
      probe|control|probe-unit|control-unit|run-denied)
        shift
        exec "$TP_FAKE_REAL_PYTHON" "$TP_FAKE_OFFLINE_PROBE_STUB" "$@"
        ;;
    esac
    ;;
esac
exec "$TP_FAKE_REAL_PYTHON" "$@"
STUB
chmod +x "${appliance}/bin/python3"

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

sys.path.insert(0, os.path.join(os.environ["TP_REPO"], "tools", "validation"))
import linux_offline_runtime as m

MODE = os.environ.get("TP_FAKE_MODE", "ok")
IN_UNIT = sys.argv[1] in ("probe-unit", "control-unit")
# The probe a CLI call's own transient unit takes before the call, which
# then execs the stub CLI for real.
IN_CLI = sys.argv[1] == "run-denied"


def option(name):
    arguments = sys.argv[2:]
    return arguments[arguments.index(name) + 1]


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
    if not DENIED and host == m.METADATA_ADDRESS \
            and MODE == "offline-control-metadata-unreachable":
        raise OSError(errno.EHOSTUNREACH, "No route to host")


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
# traceback would have put into the stage log. One mode crashes whichever
# denied probe runs first, which is a service's; the other only the probe
# in the denied transient unit.
def crash(*args, **kwargs):
    raise RuntimeError("the probe crashed in " + os.path.abspath(m.__file__))


if MODE == "offline-probe-crashes" and DENIED:
    m.run_probe = crash
    m.run_unit_probe = crash
if MODE == "offline-transient-probe-crashes" and DENIED and not IN_UNIT:
    m.run_probe = crash
if MODE == "offline-cli-probe-crashes" and DENIED and IN_CLI:
    m.run_unit_probe = crash
sys.exit(m.main())
PY

# An explicit template keeps every harness scratch directory inside the
# fixture, including backups deliberately retained after failed cleanup.
cat >"${appliance}/bin/mktemp" <<'STUB'
#!/bin/sh
[ "$#" -eq 1 ] && [ "$1" = -d ] || exit 9
exec "${TP_FAKE_MKTEMP}" -d "${TMPDIR}/tmp.XXXXXXXXXX"
STUB

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
# The operator's conffile edit, applied to the fixture copy and nowhere
# else.
if [ "$1" = bash ] && [ "$2" = -c ] && [ "$3" = 'printf "\n" >>"$1"' ]; then
  [ "$5" = "${TP_FAKE_CLI_CONFIG}" ] || exit 9
  printf '\n' >>"$5"
  exit
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
  shift
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --) shift; break ;;
      --property=IPAddressDeny=*) denied=1; shift ;;
      --property=IPAddressAllow=*) allow="${allow}${1#--property=IPAddressAllow=} "; shift ;;
      *) shift ;;
    esac
  done
  [ "$#" -gt 0 ] || exit 9
  case "${TP_FAKE_MODE:-ok}" in
    offline-transient-not-denied) denied=0; allow="" ;;
  esac
  # Interrupted with the host denied: the signal handlers have to take
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
# appliance's python3 routes it to the fixture probe, which never touches
# a real control group; nothing else run under sudo is executed.
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
  *"systemctl enable --now "*|*"systemctl restart "*|*"systemctl start "*)
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
# Whether the appliance was online whenever an installer ran: no denial
# drop-in on disk and no running instance started under one. Install and
# upgrade have to run the way an operator runs them.
case "$*" in
  "bash "*"/install.sh --local-artifacts "*)
    dropins=0
    for file in "${TP_OFFLINE_UNIT_ROOT}"/run/systemd/system/*.service.d/10-tensorplate-validation-offline.conf; do
      if [ -e "$file" ] || [ -L "$file" ]; then dropins=$((dropins + 1)); fi
    done
    denied_units=0
    for file in "${TP_OFFLINE_UNIT_ROOT}"/generation/*.running; do
      if [ -e "$file" ]; then denied_units=$((denied_units + 1)); fi
    done
    printf 'install dropins=%s denied_units=%s\n' "$dropins" "$denied_units" \
      >>"${TP_FAKE_ONLINE_LOG}"
    ;;
esac
# The agent writes its boot-bound machine-type record on every start where
# the metadata service answered. An online start does; a start under the
# denial drop-in does not, which is what makes the offline stage's
# identity come from the record rather than from a fresh answer.
case "$*" in
  *"systemctl enable --now tensorplate-agent"*|*"systemctl restart"*|*"systemctl start tensorplate-agent"*)
    if [ ! -f "${TP_OFFLINE_UNIT_ROOT}/run/systemd/system/tensorplate-agent.service.d/10-tensorplate-validation-offline.conf" ] \
       && [ "${TP_FAKE_MODE:-ok}" != offline-no-machine-type-record ]; then
      mkdir -p "${TP_FAKE_VARLIB}/state" || exit 9
      printf '{"schema_version":2,"machine_type":"g2-standard-8"}\n' \
        >"${TP_FAKE_VARLIB}/state/machine-type.json" || exit 9
    fi
    ;;
esac
# An agent that cleared its deployment state as it stopped, which leaves
# the durable state directory populated and without the one file the
# rollback stage is about. The manifest taken after this stop still has
# entries, and two directories holding only the rest would compare equal
# all the way to a pass. Hooked on the stop rather than on an install
# because in this fixture state.json IS the active-deployment record: an
# install that removed it would fail the upgrade's own round trip and the
# rollback would never run. The clear_install stops that precede it wipe
# /var/lib/tensorplate straight after, so only the rollback's stop shows.
case "$*" in
  *"systemctl stop tensorplate-agent"*)
    # The BSD-format sha256sum starts with the rollback's stop, so the
    # upgrade's own read of the machine-type record still passes and the
    # first read this mode breaks is the manifest's. A stop from
    # clear_install is followed by the wipe of /var/lib/tensorplate, which
    # takes the marker with it; only the rollback's stop leaves one.
    if [ "${TP_FAKE_MODE:-ok}" = rollback-digest-not-hex ] && [ -d "${TP_FAKE_VARLIB}" ]; then
      : >"${TP_FAKE_VARLIB}/.fixture-services-stopped"
    fi
    if [ "${TP_FAKE_MODE:-ok}" = rollback-state-file-missing ]; then
      rm -f "${TP_FAKE_VARLIB}/state/state.json"
    fi
    # A durable-state file whose name holds a space, present in the
    # directory at the one moment that decides the check: when the
    # manifest is taken. Planted on the rollback's stop for the same
    # reason as the mode above -- the earlier stages' clear_install stops
    # wipe /var/lib/tensorplate straight after, so only the rollback's
    # stop shows. The Production rows hold no such name; this is about
    # which file the operator is told to go and look at when one appears.
    if [ "${TP_FAKE_MODE:-ok}" = rollback-empties-spaced-file ]; then
      printf '{"fixture": "a name with a space in it"}\n' \
        >"${TP_FAKE_VARLIB}/state/two words.json"
    fi
    ;;
esac
# A drop-in an earlier run left behind, planted where the harness's own
# reset cannot clear it first.
#
# Only on the install stage's one-time enable, never on a restart:
# planting it again after the offline stage's cleanup would fail that
# cleanup whatever the stage's own refusal did, and the leftover check
# would pass for a reason that has nothing to do with it.
#
# One mode per unit, and one that plants a dangling symlink rather than a
# file: the refusal is a `step` per unit, and a mode that only ever
# planted one leftover would let the other half be deleted with nothing
# failing.
case "$*" in
  *"systemctl enable --now tensorplate-agent"*)
    case "${TP_FAKE_MODE:-ok}" in
      offline-leftover-drop-in)
        leftover="${TP_OFFLINE_UNIT_ROOT}/run/systemd/system/tensorplate-agent.service.d"
        mkdir -p "$leftover" || exit 9
        : >"${leftover}/10-tensorplate-validation-offline.conf" || exit 9
        ;;
      # A dangling symlink is still a file at that path, and `install -D`
      # would write through it to wherever it points.
      offline-leftover-dangling-symlink)
        leftover="${TP_OFFLINE_UNIT_ROOT}/run/systemd/system/tensorplate-agent.service.d"
        mkdir -p "$leftover" || exit 9
        ln -s "${TP_OFFLINE_UNIT_ROOT}/nowhere" \
          "${leftover}/10-tensorplate-validation-offline.conf" || exit 9
        ;;
    esac
    ;;
esac
case "$*" in
  *"systemctl enable --now tensorplate-observability"*)
    if [ "${TP_FAKE_MODE:-ok}" = offline-leftover-drop-in-observability ]; then
      leftover="${TP_OFFLINE_UNIT_ROOT}/run/systemd/system/tensorplate-observability.service.d"
      mkdir -p "$leftover" || exit 9
      : >"${leftover}/10-tensorplate-validation-offline.conf" || exit 9
    fi
    ;;
esac
# Package, state and config operations touch only the fixture and the
# harness's temporary backup. This verifies the bytes can actually be
# restored, rather than treating a logged copy command as a successful
# restoration.
case "$*" in
  "test -f "*"/machine-type.json")
    [ -f "${TP_FAKE_VARLIB}/state/machine-type.json" ]
    exit
    ;;
  "test -f /var/lib/tensorplate/identity/instance-binding.json")
    [ -f "${TP_FAKE_VARLIB}/identity/instance-binding.json" ]
    exit
    ;;
  # The rollback's restore, done for real in the fixture: the state
  # directory recreated, and the record copied back from the set-aside
  # state, so the digest the harness takes next reads what was copied.
  "install -d -o tensorplate -g tensorplate -m 0750 /var/lib/tensorplate/state")
    mkdir -p "${TP_FAKE_VARLIB}/state"
    exit
    ;;
  "cp -p /var/lib/tensorplate/state.bak/machine-type.json /var/lib/tensorplate/state/machine-type.json")
    # A copy that lands different bytes under the right name.
    if [ "${TP_FAKE_MODE:-ok}" = rollback-restore-garbles-record ]; then
      printf '{"schema_version":2}\n' >"${TP_FAKE_VARLIB}/state/machine-type.json"
      exit
    fi
    cp -p "${TP_FAKE_VARLIB}/state.bak/machine-type.json" "${TP_FAKE_VARLIB}/state/machine-type.json"
    exit
    ;;
  # The deploy-smoke bundle staging, done for real in the fixture's
  # staging directory and nowhere else: what cp prints about a bundle file
  # it cannot read goes into the published stage log. Any source is
  # copied, so a harness that named the bundle by its own path would have
  # that path printed here too.
  # The value itself, not "$3": a staging path with a space in it would
  # still match the joined arguments, and "$3" would be only its first part.
  "rm -rf ${TP_CLOUD_BUNDLE_STAGING:-/nonexistent/unset-staging}")
    rm -rf "$TP_CLOUD_BUNDLE_STAGING"
    exit
    ;;
  # Between the bundle check and the copy, the bundle directory can stop
  # being one the operator can enter; a case locks it here to say so.
  "mkdir -p ${TP_CLOUD_BUNDLE_STAGING%/*}")
    if [ -n "${TP_FAKE_LOCK_BUNDLE:-}" ]; then
      chmod 000 "${TP_FAKE_LOCK_BUNDLE}" || exit 9
    fi
    exit 0
    ;;
  "cp -R "*" ${TP_CLOUD_BUNDLE_STAGING:-/nonexistent/unset-staging}")
    [ "$#" -eq 4 ] || exit 9
    exec cp -R "$3" "$4"
    ;;
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
      # A drop-in that reaches the host spelling the shorthand the owner
      # ruling refuses.
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
  "bash "*"/install.sh --local-artifacts "*)
    status=0
    "${TP_FAKE_DPKG_DB}" install "$4" || status=$?
    if [ "$status" -eq 200 ]; then
      kill -TERM "$PPID"
      exit 0
    fi
    exit "$status"
    ;;
  *"apt-get purge"*)
    : >"${TP_FAKE_PURGE_MARKER}"
    shift 4
    exec "${TP_FAKE_DPKG_DB}" purge "$@"
    ;;
  *"apt-get remove"*)
    shift 4
    exec "${TP_FAKE_DPKG_DB}" remove "$@"
    ;;
  "rm -rf /etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate")
    rm -rf "${TP_FAKE_VARLIB}" "${TP_FAKE_CLI_CONFIG}"
    exit
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
  # so the harness reads a fixture exactly as it reads a host. A file
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
    if [ "${TP_FAKE_MODE:-ok}" = rollback-digest-not-hex ] \
       && [ -f "${TP_FAKE_VARLIB}/.fixture-services-stopped" ]; then
      printf 'SHA256 (%s) = %s\n' "$2" "$digest"
      exit 0
    fi
    printf '%s  %s\n' "$digest" "$2"
    exit 0
    ;;
  "mv -T /var/lib/tensorplate/state /var/lib/tensorplate/state.bak")
    # GNU mv -T: never moves into the target, replaces only an empty one.
    source_dir="${TP_FAKE_VARLIB}/state"
    target_dir="${TP_FAKE_VARLIB}/state.bak"
    [ -d "$source_dir" ] || exit 1
    if [ -e "$target_dir" ]; then
      rmdir "$target_dir" 2>/dev/null || { echo "mv: cannot overwrite '$target_dir': Directory not empty" >&2; exit 1; }
    fi
    case "${TP_FAKE_MODE:-ok}" in
      rollback-keeps-state) cp -R "$source_dir" "$target_dir" ;;
      *) mv "$source_dir" "$target_dir" ;;
    esac
    exit
    ;;
  *"systemctl restart"*) : >"${TP_FAKE_RESTART_MARKER}" ;;
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
      crash-loop-signal-term) kill -TERM "$PPID" ;;
      crash-loop-signal-hup) kill -HUP "$PPID" ;;
    esac
    ;;
  "cp -p "*" /etc/tensorplate/agent.json")
    case "$3" in "${TMPDIR}/"*/agent.json) ;; *) exit 9 ;; esac
    case "${TP_FAKE_MODE:-ok}" in
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
  case "${TP_FAKE_MODE:-ok}:$*" in
    journal-signal-term:*_SYSTEMD_INVOCATION_ID=*|crash-loop-journal-signal-term:*--since*)
      # The raw capture is written, and then the harness is signalled
      # before it can project or remove it. The capture runs under a
      # `bash -c` wrapper, so the harness is found among the ancestors
      # rather than assumed to be the parent.
      "${TP_FAKE_JOURNALCTL}" "$@" || exit
      pid=$PPID
      while [ -n "$pid" ] && [ "$pid" -gt 1 ]; do
        case "$(ps -ww -o args= -p "$pid" 2>/dev/null)" in
          *"${TP_FAKE_HARNESS}"*) kill -TERM "$pid"; exit 0 ;;
        esac
        pid="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')"
      done
      echo 'fixture: no harness process among the ancestors' >&2
      exit 9
      ;;
  esac
  exec "${TP_FAKE_JOURNALCTL}" "$@"
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
      printf '%s installed 0.2.0-1\n' "$pkg"
    done
    ;;
  purge-leaves-packages)
    printf 'tensorplate-common config-files 0.2.0-1\n'
    ;;
  *) exec "${TP_FAKE_DPKG_DB}" query ;;
esac
STUB
cat >"${appliance}/bin/systemctl" <<'STUB'
#!/bin/sh
case "$1" in
  show)
    broken=0
    [ -f "${TP_FAKE_CONFIG_BROKEN}" ] && broken=1
    case "$*" in
      *IPAddressDeny*)
        # The effective address policy, the way systemd reports it: the
        # allow list is whatever the unit's drop-in asks for, expanded --
        # so `localhost` reads back as 127.0.0.0/8, which is the whole
        # point of refusing the shorthand.
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
      *InvocationID*)
        case "$*" in
          *tensorplate-agent*) unit=tensorplate-agent; prefix=1111111111111111111111111111 ;;
          *tensorplate-observability*)
            unit=tensorplate-observability; prefix=2222222222222222222222222222 ;;
          *) exit 9 ;;
        esac
        generation=$(sed -n 1p "${TP_OFFLINE_UNIT_ROOT}/generation/${unit}" 2>/dev/null)
        state=$(sed -n 2p "${TP_OFFLINE_UNIT_ROOT}/generation/${unit}" 2>/dev/null)
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
      *MainPID*)
        # A service the upgrade never restarted keeps the pid it had.
        case "${TP_FAKE_MODE:-ok}:$*" in
          upgrade-agent-not-restarted:*tensorplate-agent*|upgrade-observability-not-restarted:*tensorplate-observability*)
            case "$("${TP_FAKE_DPKG_DB}" phase)" in
              baseline|upgraded) printf '4242\n'; exit 0 ;;
            esac
            ;;
        esac
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
cp "$fake_dpkg" "${appliance}/bin/dpkg"
cp "${stub_bin}/dpkg-deb" "${appliance}/bin/dpkg-deb"
cp "$fake_dpkg" "${appliance}/bin/fake-dpkg-db"
# Present so preflight's `command -v` finds it, and loud if it is ever
# invoked directly: a transient unit that denies the network has to be
# started with privilege, and one started without it would deny nothing.
cat >"${appliance}/bin/systemd-run" <<'STUB'
#!/bin/sh
echo 'systemd-run was invoked without sudo; the transient unit would not be denied' >&2
exit 9
STUB
# The real checksum tool, except that the baseline's digest can be made
# unreadable, so the harness's refusal of a digest it could not compute
# is exercised rather than assumed.
real_sha256sum="$(command -v sha256sum || true)"
cat >"${appliance}/bin/sha256sum" <<'STUB'
#!/bin/sh
if [ "${TP_FAKE_MODE:-ok}" = baseline-digest-unreadable ] && [ "$*" = SHA256SUMS ]; then
  case "$(pwd)" in */set-rc1) exit 0 ;; esac
fi
[ -n "${TP_FAKE_SHA256SUM}" ] || exit 127
exec "${TP_FAKE_SHA256SUM}" "$@"
STUB
# journalctl, emitting what systemd actually attaches to an entry.
#
# The host fields are the point: _HOSTNAME, _MACHINE_ID, _BOOT_ID,
# __CURSOR, _CMDLINE and the rest describe the machine, and the harness
# must project them out before the capture reaches the evidence
# directory. A stub that emitted only the fields the assertions read
# could not tell a projection from its absence. What the publication
# scanner refuses here is the field name, so the values are the
# synthetic ones from docs/validation/evidence/v0.2.1/README.md rather
# than anything that could be read as a machine's own.
#
# The field set is the one journald attaches to a service's stdout and
# to systemd's own records, not a sample of it: a projection that
# dropped the host fields it knew about, rather than keeping only the
# service's own, would pass a stub that emitted only those and then
# write evidence the scanner refuses on the first real host. For the
# same reason every service record also carries a field no list could
# name in advance, because its name is new on every run of this file.
TP_FAKE_JOURNAL_FIELD="TP_FIXTURE_$(python3 -c 'import secrets; print(secrets.token_hex(4).upper())')"
export TP_FAKE_JOURNAL_FIELD
cat >"${appliance}/bin/journalctl" <<'STUB'
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
# What systemd adds to its own records about a unit.
manager_fields="\"_TRANSPORT\":\"journal\",\"_UID\":\"0\",\"_GID\":\"0\",\"_COMM\":\"systemd\",\"_EXE\":\"/usr/lib/systemd/systemd\",\"_CMDLINE\":\"/sbin/init\",\"_CAP_EFFECTIVE\":\"1ffffffffff\",\"_SYSTEMD_CGROUP\":\"/init.scope\",\"_SYSTEMD_SLICE\":\"-.slice\",\"SYSLOG_FACILITY\":\"3\",\"CODE_FILE\":\"src/core/unit.c\",\"CODE_LINE\":\"2210\",\"CODE_FUNC\":\"unit_log_failure\",\"MESSAGE_ID\":\"${zero}\",\"INVOCATION_ID\":\"${zero}\",\"_SOURCE_REALTIME_TIMESTAMP\":\"1789300000000001\""
agent_fields="$(service_fields tensorplate-agent)" || exit
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
  for attempt in 1 2 3 4 5; do
    printf '{%s,%s,"_SYSTEMD_UNIT":"tensorplate-agent.service","_SYSTEMD_INVOCATION_ID":"11111111111111111111111111111111","_PID":"%s","PRIORITY":"3","SYSLOG_IDENTIFIER":"tensorplate-agent","MESSAGE":"%s","__REALTIME_TIMESTAMP":"1789300000000000"}\n' \
      "$host_fields" "$agent_fields" "$((4000 + attempt))" "$message"
    # systemd's own record of the restart, which carries UNIT rather
    # than _SYSTEMD_UNIT.
    printf '{%s,%s,"_SYSTEMD_UNIT":"init.scope","UNIT":"tensorplate-agent.service","_PID":"1","PRIORITY":"4","SYSLOG_IDENTIFIER":"systemd","MESSAGE":"tensorplate-agent.service: Scheduled restart job, restart counter is at %s.","__REALTIME_TIMESTAMP":"1789300000000001"}\n' \
      "$host_fields" "$manager_fields" "$attempt"
  done
  printf '{%s,%s,"_SYSTEMD_UNIT":"init.scope","UNIT":"tensorplate-agent.service","_PID":"1","PRIORITY":"3","SYSLOG_IDENTIFIER":"systemd","MESSAGE":"tensorplate-agent.service: Start request repeated too quickly.","__REALTIME_TIMESTAMP":"1789300000000002"}\n' \
    "$host_fields" "$manager_fields"
  # What `journalctl --show-cursor` prints after the records: a line that
  # is not a record, after records that are.
  if [ "${TP_FAKE_MODE:-ok}" = crash-loop-trailing-line ]; then
    printf '%s\n' "-- cursor: ${cursor}"
  fi
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
  journal-empty-observability:tensorplate-observability.service) exit 0 ;;
  journal-stale-invocation:*) invocation=ffffffffffffffffffffffffffffffff ;;
  journal-wrong-unit:*) unit=another.service ;;
esac
message='fixture service started'
[ "${TP_FAKE_MODE:-ok}" = journal-empty-message ] && message=''
fields="$(service_fields "${unit%.service}")" || exit
printf '{%s,%s,"_SYSTEMD_INVOCATION_ID":"%s","_SYSTEMD_UNIT":"%s","_PID":"4242","PRIORITY":"6","SYSLOG_IDENTIFIER":"%s","MESSAGE":"%s","__REALTIME_TIMESTAMP":"1789300000000000"}\n' \
  "$host_fields" "$fields" "$invocation" "$unit" "${unit%.service}" "$message"

# The agent's platform identity line, emitted only by a start that
# happened under the denial drop-in: such a start finds the metadata
# service unreachable and resolves its shape from the boot-bound record.
#
# Keyed on the start generation, not on the drop-in file existing. A
# stage that installed the drop-in and never restarted the agent is a
# stage whose agent never queried metadata under it, so it gets no
# identity line at all.
agent_state="$(sed -n 2p "${TP_OFFLINE_UNIT_ROOT}/generation/tensorplate-agent" 2>/dev/null)"
if [ "$unit" = tensorplate-agent.service ] && [ "$agent_state" = denied ]; then
  identity='machine_type=g2-standard-8 source=recorded_gce_metadata record=not_applicable'
  repeat=1
  case "${TP_FAKE_MODE:-ok}" in
    offline-identity-live-metadata)
      identity='machine_type=g2-standard-8 source=gce_metadata record=written' ;;
    offline-identity-rewrites-record)
      identity='machine_type=g2-standard-8 source=recorded_gce_metadata record=written' ;;
    offline-identity-undetected)
      identity='machine_type=none source=none record=not_applicable' ;;
    offline-identity-twice) repeat=2 ;;
  esac
  while [ "$repeat" -gt 0 ]; do
    printf '{%s,%s,"_SYSTEMD_INVOCATION_ID":"%s","_SYSTEMD_UNIT":"%s","_PID":"4242","PRIORITY":"6","SYSLOG_IDENTIFIER":"%s","MESSAGE":"platform identity: %s","__REALTIME_TIMESTAMP":"1789300000000001"}\n' \
      "$host_fields" "$fields" "$invocation" "$unit" "${unit%.service}" "$identity"
    repeat=$((repeat - 1))
  done
fi
# What `journalctl --show-cursor` prints after the records.
if [ "${TP_FAKE_MODE:-ok}:$unit" = journal-trailing-line:tensorplate-agent.service ]; then
  printf '%s\n' "-- cursor: ${cursor}"
fi
STUB
cat >"${appliance}/bin/tensorplate" <<'STUB'
#!/bin/sh
# A stubbed appliance. TP_FAKE_MODE selects which way it misbehaves.
mode="${TP_FAKE_MODE:-ok}"
phase=initial
[ -f "${TP_FAKE_RESTART_MARKER}" ] && phase=restarted
# Which install the appliance is on: candidate, baseline, upgraded or
# rolled-back, from the package database.
installed="$("${TP_FAKE_DPKG_DB}" phase)"
state_file="${TP_FAKE_VARLIB}/state/state.json"
command="$1"
shift
# Every call, with whether it ran in a denied transient unit and how many
# denial drop-ins were on the host at the time. Read against each other,
# these say which calls the offline stage made and whether each was
# denied -- whatever helper function the harness made it through.
dropins=0
for file in "${TP_OFFLINE_UNIT_ROOT}"/run/systemd/system/*.service.d/10-tensorplate-validation-offline.conf; do
  if [ -e "$file" ] || [ -L "$file" ]; then dropins=$((dropins + 1)); fi
done
printf '%s denied=%s dropins=%s\n' "$command" "${TP_FAKE_DENIED:-unset}" "$dropins" \
  >>"${TP_FAKE_CLI_LOG}"
out=""
deployment="${TP_FAKE_DEPLOYMENT_ID}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output-file) out="$2"; shift 2 ;;
    --input) input="$2"; shift 2 ;;
    --deployment-id) deployment="$2"; shift 2 ;;
    *) shift ;;
  esac
done
# The deployment the appliance is actually serving, which the offline
# stage changes by deploying a fresh one while denied.
active="${TP_FAKE_DEPLOYMENT_ID}"
[ -f "$state_file" ] && active="$(sed 's/.*"active":"\([^"]*\)".*/\1/' "$state_file")"
case "$command" in
  doctor)
    failing=0
    row_status=ok
    if [ "$mode" = "doctor-failing" ]; then failing=1; fi
    if [ "$mode" = "wrong-row" ]; then row_status=warning; fi
    if [ "$mode:$installed" = upgrade-wrong-row:upgraded ]; then row_status=warning; fi
    case "$mode:$installed" in
      baseline-doctor-failing:baseline|baseline-doctor-failing:rolled-back) failing=1 ;;
    esac
    # Where host_os says the machine type came from. Denied, the agent
    # cannot reach the metadata service, so a live answer here would mean
    # the denial let the query through.
    machine_type_source=" (from GCE metadata)"
    if [ "${TP_FAKE_DENIED:-0}" = 1 ]; then
      machine_type_source=" (recorded from GCE metadata by tensorplate-agent; metadata service unreachable; same kernel boot; CPU count, MemTotal and NVIDIA devices unchanged)"
    fi
    case "$mode" in
      offline-doctor-live-metadata) machine_type_source=" (from GCE metadata)" ;;
      offline-doctor-failing) [ "${TP_FAKE_DENIED:-0}" = 1 ] && failing=1 ;;
      offline-doctor-row-warning) [ "${TP_FAKE_DENIED:-0}" = 1 ] && row_status=warning ;;
    esac
    # The online stages' doctor -- any call not made in a transient unit --
    # answering from the record or from nothing: a metadata service that
    # did not answer a host meant to be online.
    if [ "${TP_FAKE_DENIED:-unset}" = unset ]; then
      case "$mode:$installed" in
        install-doctor-recorded:*|upgrade-doctor-recorded:upgraded)
          machine_type_source=" (recorded from GCE metadata by tensorplate-agent; metadata service unreachable; same kernel boot; CPU count, MemTotal and NVIDIA devices unchanged)"
          ;;
        install-doctor-no-source:*) machine_type_source="" ;;
      esac
    fi
    cat <<JSON
{"command":"doctor","payload":{"failing":${failing},"findings":[
 {"id":"host_os","status":"ok","message":"ubuntu 24.04 on g2-standard-8${machine_type_source}"},
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
    # Doctor exits 10 when a finding fails. Only the baseline mode says
    # so, so the install-stage cases keep reaching the harness's own check.
    case "$mode:$failing" in baseline-doctor-failing:1) exit 10 ;; esac
    ;;
  deploy)
    # The deployment is durable state: it is what an upgraded agent
    # re-warms, and what a rollback sets aside.
    mkdir -p "${TP_FAKE_VARLIB}/state" || exit 9
    printf '{"active":"%s"}\n' "$deployment" >"$state_file" || exit 9
    # The recovery copy the agent keeps beside the primary and falls back
    # to when the primary fails to decode (agent/src/state.rs). It is
    # durable state the rollback has to preserve, and a check on
    # state.json alone would never see it destroyed.
    printf '{"active":"%s"}\n' "$deployment" >"${state_file}.bak" || exit 9
    "${TP_FAKE_DPKG_DB}" agent-version >>"${TP_FAKE_DEPLOY_VERSIONS}"
    printf '{"command":"deploy","payload":{"phase":"active","deployment_id":"%s"}}\n' \
      "$deployment"
    ;;
  status)
    if [ "$mode:$installed" = rollback-agent-unavailable:rolled-back ]; then
      printf '{"command":"status","payload":{"severity":"blocked","agent":{"available":false}}}\n'
      exit 0
    fi
    # An agent with no durable deployment reports none -- unless it kept
    # the previous one from state it should not have loaded.
    if [ ! -f "$state_file" ]; then
      previous=null
      if [ "$mode:$installed" = rollback-keeps-previous:rolled-back ]; then
        previous="{\"deployment_id\":\"${TP_FAKE_DEPLOYMENT_ID}\",\"backend\":\"python_pytorch\"}"
      fi
      printf '{"command":"status","payload":{"severity":"ready","agent":{"available":true,"agent_state":"ready","active":null,"previous_active":%s}}}\n' \
        "$previous"
      exit 0
    fi
    serving_url="\"http://127.0.0.1:${TP_FAKE_SERVING_PORT}/infer\""
    if [ "$mode:$phase" = restart-no-worker:restarted ]; then serving_url=null; fi
    printf '{"command":"status","payload":{"severity":"ready","agent":{"available":true,"agent_state":"ready","active":{"deployment_id":"%s","backend":"python_pytorch","serving_url":%s},"previous_active":null}}}\n' \
      "$active" "$serving_url"
    ;;
  infer)
    printf '%s\n' "$phase" >>"${TP_FAKE_INFER_LOG}"
    "${TP_FAKE_DPKG_DB}" agent-version >>"${TP_FAKE_INFER_VERSIONS}"
    name=echo_probe
    payload=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["inputs"][0]["payload_b64"])' "$input")
    if [ "$mode" = "infer-garbled" ]; then payload="AAAA"; fi
    if [ "$mode:$phase" = restart-infer-garbled:restarted ]; then payload="AAAA"; fi
    if [ "$mode:$installed" = rollback-infer-garbled:rolled-back ]; then payload="AAAA"; fi
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

packaged_cli_config='{"fixture": "packaged cli config"}'

# The checkout a stubbed run executes the harness and the offline module
# from: this one, except where a case is about the checkout's own path.
checkout_root="$repo_root"
checkout_harness="$harness"
# A bundle directory the sudo stub locks just before the copy, or none.
locked_bundle=""

# Runs the harness against the stubbed appliance, from a clean fixture
# state: empty package database, no durable state, the packaged cli
# config. Arguments after the assets directory go to the harness.
run_harness() {
  local mode="$1" evidence="$2" sudo_fail="$3" assets_dir="$4"
  shift 4
  set +e
  : >"${appliance}/sudo.log"
  : >"${appliance}/infer.log"
  : >"${appliance}/health-requests.log"
  : >"${appliance}/deploy-versions.log"
  : >"${appliance}/infer-versions.log"
  : >"${appliance}/cli.log"
  : >"${appliance}/online.log"
  : >"$TP_FAKE_PUBLICATION_LOG"
  rm -f "${appliance}/restarted" "${appliance}/config-broken" \
    "${appliance}/restarts" "${appliance}/restore-failed" "${appliance}/backup-path" \
    "${appliance}/dpkg-db.json"
  rm -rf "${appliance}/varlib" "${appliance}/units"
  # The fixture's /run and /etc, so a run starts with no unit drop-in and
  # a leftover from a previous run cannot certify this one.
  mkdir -p "${appliance}/units/run/systemd/system" "${appliance}/units/etc/systemd/system"
  # Except where the case is exactly that: a drop-in an earlier run left
  # behind, there before this run starts.
  if [[ "$mode" == offline-leftover-before-run ]]; then
    mkdir -p "${appliance}/units/run/systemd/system/tensorplate-observability.service.d"
    printf '[Service]\n' \
      >"${appliance}/units/run/systemd/system/tensorplate-observability.service.d/10-tensorplate-validation-offline.conf"
  fi
  printf '{"fixture":"original agent config"}\n' >"${appliance}/agent-config"
  printf '%s\n' "$packaged_cli_config" >"${appliance}/cli.json"
  printf '%s\n' "$mode" >"${appliance}/mode"
  env PATH="${appliance}/bin:${PATH}" \
    TP_FAKE_DPKG_DB="${appliance}/bin/fake-dpkg-db" \
    TP_FAKE_DB="${appliance}/dpkg-db.json" \
    TP_FAKE_VARLIB="${appliance}/varlib" \
    TP_FAKE_CLI_CONFIG="${appliance}/cli.json" \
    TP_CLOUD_OPERATOR_CONFIG="${appliance}/cli.json" \
    TP_FAKE_CANDIDATE_DIR="$assets_dir" \
    TP_FAKE_DEPLOY_VERSIONS="${appliance}/deploy-versions.log" \
    TP_FAKE_INFER_VERSIONS="${appliance}/infer-versions.log" \
    TMPDIR="${appliance}/scratch" \
    TP_FAKE_MKTEMP="$real_mktemp" \
    TP_FAKE_SHA256SUM="$real_sha256sum" \
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
    TP_FAKE_HARNESS="$checkout_harness" \
    TP_FAKE_RESTART_MARKER="${appliance}/restarted" \
    TP_FAKE_INFER_LOG="${appliance}/infer.log" \
    TP_FAKE_PID_FILE="${appliance}/pid" \
    TP_FAKE_DEPLOYMENT_ID="$deployment_id" \
    TP_FAKE_SERVING_PORT="$serving_port" \
    TP_FAKE_CONFIG_BROKEN="${appliance}/config-broken" \
    TP_FAKE_AGENT_CONFIG="${appliance}/agent-config" \
    TP_FAKE_BACKUP_PATH="${appliance}/backup-path" \
    TP_FAKE_RESTORE_FAILED="${appliance}/restore-failed" \
    TP_FAKE_RESTARTS_FILE="${appliance}/restarts" \
    TP_OFFLINE_UNIT_ROOT="${appliance}/units" \
    TP_OFFLINE_CGROUP_ROOT="${appliance}/cgroup" \
    TP_FAKE_CLI_LOG="${appliance}/cli.log" \
    TP_FAKE_ONLINE_LOG="${appliance}/online.log" \
    TP_REPO="$checkout_root" \
    TP_FAKE_LOCK_BUNDLE="$locked_bundle" \
    TP_CLOUD_CRASH_LOOP_POLL_SECONDS=0 \
    bash "$checkout_harness" \
      --assets-dir "$assets_dir" \
      --bundle-dir "$bundle" \
      --evidence-dir "$evidence" \
      --tested-version 0.2.1 \
      --confirm RESET-TENSORPLATE "$@" >"${evidence}.out" 2>"${evidence}.err"
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

run_stages() {
  run_harness "$1" "$2" "${3:-}" "$assets"
}

# A run with a baseline, from per-run copies of the release-shaped sets,
# so a mode that changes a set cannot leak into a later run. The copies
# are at <evidence>.sets.
run_upgrade_stages() {
  local mode="$1" evidence="$2" sudo_fail="$3" baseline="$4"
  shift 4
  local sets="${evidence}.sets"
  mkdir -p "$sets"
  cp -R "${fixtures}/assets-rc2" "${fixtures}/${baseline}" "${sets}/"
  run_harness "$mode" "$evidence" "$sudo_fail" "${sets}/assets-rc2" \
    --baseline-assets-dir "${sets}/${baseline}" "$@"
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
stage_log_says() {
  grep -Fq -- "$2" "$1" && echo yes || echo no
}

# The publication scanner, run the way the runbook gives it to the
# operator but without the private literal file CI cannot have.
#
# Prints the scanner's exit status; its output is kept at the second
# argument.
publication_scan() {
  local output="$1" status=0
  shift
  "${repo_root}/tools/validation/check-evidence-publication.sh" \
    --patterns-only "$@" >"$output" 2>&1 || status=$?
  printf '%s' "$status"
}
# Checks that the scanner admits every directory given, and shows what
# it found when it does not: the evidence is gone with the temp dir by
# the time anyone reads a CI log. A finding names a file relative to its
# directory, so with several directories the refused ones are named too.
check_publishable() {
  local what="$1" output="$2" status dir
  shift 2
  status="$(publication_scan "$output" "$@")"
  check "$what" 0 "$status"
  if [[ "$status" != 0 ]]; then
    sed 's/^/       /' "$output"
    if (($# > 1)); then
      for dir in "$@"; do
        if [[ "$(publication_scan "${output}.one" "$dir")" != 0 ]]; then
          printf '       in %s\n' "${dir##*/}"
        fi
      done
    fi
  fi
}

# Where the network denial reached, read from what the stubs saw rather
# than from the harness text: the offline stage's window in sudo.log --
# from its record check to its last drop-in removal -- and every CLI call
# and every installer run with the denial state at that moment.
#
#   window     the offline stage's window exists
#   outside    offline-only privileged commands outside that window
#   denied     CLI calls made in a denied transient unit
#   leaked     CLI calls made while drop-ins were installed, not denied
#   transient  CLI calls made in any transient unit with no drop-in
#              installed: the online stages never use one
#   installs   installer runs, and how many ran with a denial in place
offline_scope() {
  python3 - "${appliance}/sudo.log" "${appliance}/cli.log" "${appliance}/online.log" <<'PY'
import re, sys

sudo, cli, online = (open(path, encoding="utf-8").read().splitlines() for path in sys.argv[1:])
starts = [i for i, line in enumerate(sudo)
          if re.fullmatch(r"test -f \S*/machine-type\.json", line)]
ends = [i for i, line in enumerate(sudo)
        if re.fullmatch(r"rm -f \S*/10-tensorplate-validation-offline\.conf", line)]
window = len(starts) == 1 and bool(ends) and starts[0] < ends[-1]
# Commands and arguments, not substrings: a checkout or a temporary
# directory may be called anything.
offline_only = re.compile(r"^systemd-run |--property=IPAddress"
                          r"|/10-tensorplate-validation-offline\.conf(?: |$)"
                          r"|/linux_offline_runtime\.py ")
outside = [line for i, line in enumerate(sudo) if offline_only.search(line)
           and not (window and starts[0] <= i <= ends[-1])]
calls = []
for line in cli:
    command, denied, dropins = line.split()
    calls.append((command, denied.split("=", 1)[1], int(dropins.split("=", 1)[1])))
denied = [c for c in calls if c[1] == "1" and c[2] > 0]
leaked = [c for c in calls if c[2] > 0 and c[1] != "1"]
transient = [c for c in calls if c[2] == 0 and c[1] != "unset"]
under_denial = [line for line in online if line != "install dropins=0 denied_units=0"]
for name, items in (("outside", outside), ("leaked", leaked), ("transient", transient),
                    ("under_denial", under_denial)):
    for item in items:
        print(f"       {name}: {item}", file=sys.stderr)
print(f"window={'yes' if window else 'no'} outside={len(outside)} denied={len(denied)} "
      f"leaked={len(leaked)} transient={len(transient)} "
      f"installs={len(online)} installs_denied={len(under_denial)}")
PY
}

ok_evidence="${td}/stages-ok"
check "a stubbed run completes" "0" "$(run_stages ok "$ok_evidence")"
# Copied from inside the bundle, so cp only ever names relative paths,
# and still staged as the bundle's own tree rather than nested in it.
check "  the bundle is staged as a copy of its own tree" "yes yes" \
  "$(printf '%s %s' \
     "$(grep -Fxq "cp -R . ${appliance}/staged-bundle" "${appliance}/sudo.log" \
          && echo yes || echo no)" \
     "$(diff -r "$bundle" "${appliance}/staged-bundle" >/dev/null 2>&1 \
          && echo yes || echo no)")"
for stage in install deploy-smoke status-logs restart crash-loop offline; do
  check "  ${stage} is recorded as a pass" "pass" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
for stage in upgrade rollback; do
  check "  ${stage} is recorded as skipped" "skipped" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
check "  the skipped stages keep the run incomplete" incomplete \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' \
    "${ok_evidence}/lifecycle-report.json")"
for stage in upgrade rollback; do
  check "  ${stage} is skipped for want of --baseline-assets-dir" yes \
    "$(python3 - "${ok_evidence}/lifecycle-report.json" "$stage" <<'PY'
import json, sys

stage = next(s for s in json.load(open(sys.argv[1]))["stages"] if s["stage"] == sys.argv[2])
print("yes" if "--baseline-assets-dir" in stage.get("detail", "") else "no")
PY
)"
done
check "  without a baseline the candidate is installed once and nothing is removed" "1 0" \
  "$(printf '%s %s' "$(grep -c '/install.sh --local-artifacts' "${appliance}/sudo.log")" \
     "$(grep -c 'apt-get remove' "${appliance}/sudo.log")")"
# --- the offline stage, executed against the stubbed appliance.
#
# These were negative checks while the stage was deferred -- that the
# harness never touched network policy. They are the same claims read the
# other way now: the drop-in IS applied, under /run and nowhere else, and
# IS removed on the way out.
offline_drop_in() {
  printf '%s' "${appliance}/units/run/systemd/system/${1}.service.d/10-tensorplate-validation-offline.conf"
}
check "  the offline stage denies both services through runtime drop-ins" "yes yes" \
  "$(printf '%s %s' \
     "$(grep -Fq "install -D -m 0644" "${appliance}/sudo.log" && echo yes || echo no)" \
     "$(grep -F 'install -D -m 0644' "${appliance}/sudo.log" | grep -cq '/run/systemd/system/tensorplate-agent.service.d/' && echo yes || echo no)")"
check "  and denies the observability service too" yes \
  "$(grep -F 'install -D -m 0644' "${appliance}/sudo.log" | grep -cq '/run/systemd/system/tensorplate-observability.service.d/' && echo yes || echo no)"
check "  and never writes a persistent unit file" no \
  "$(grep -q '/etc/systemd/system' "${appliance}/sudo.log" && echo yes || echo no)"
check "  and removes every drop-in it installed" "no no" \
  "$(printf '%s %s' \
     "$([[ -e "$(offline_drop_in tensorplate-agent)" ]] && echo yes || echo no)" \
     "$([[ -e "$(offline_drop_in tensorplate-observability)" ]] && echo yes || echo no)")"
check "  and files the removal it read back from systemd" "2 False" \
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
# infer. Every one of them inside its own denied transient unit.
check "  every CLI call ran in a denied transient unit" "5" \
  "$(grep -c 'systemd-run .*--property=IPAddressDeny=any.*-- tensorplate ' "${appliance}/sudo.log")"
check "  and none of them ran in an undenied one" "0" \
  "$(grep 'systemd-run .*-- tensorplate ' "${appliance}/sudo.log" \
     | grep -vc 'IPAddressDeny=any' || true)"
for command in status doctor deploy infer; do
  check "  ${command} ran denied" yes \
    "$(grep 'systemd-run .*--property=IPAddressDeny=any' "${appliance}/sudo.log" \
       | grep -q -- "-- tensorplate ${command} " && echo yes || echo no)"
done
# Each through the module's run-denied, which probes that call's own unit
# and only then execs the call, in the stage's order.
check "  each call ran behind a probe of its own unit" \
  "status doctor deploy status-after-deploy infer" \
  "$(sed -n 's/^systemd-run .*--property=IPAddressDeny=any.* -- python3 .*linux_offline_runtime\.py run-denied --call \([a-z-]*\) .* -- tensorplate .*/\1/p' \
       "${appliance}/sudo.log" | tr '\n' ' ' | sed 's/ $//')"
check "  and the certificate classifies each unit's probe against the transient control" \
  "deploy:6 doctor:6 infer:6 status:6 status-after-deploy:6" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))["cli_units_probed_before_each_call"]
print(" ".join("%s:%d" % (call, len(entry["classification"]["operations_refused_under_the_denial"]))
               for call, entry in sorted(r.items()) if entry["probe"]["call"] == call))' \
    "${ok_evidence}/offline-runtime.json")"
check "  the control ran in a transient unit with no denial" "1" \
  "$(grep -c 'systemd-run .*-- python3 .*linux_offline_runtime.py control' "${appliance}/sudo.log")"
check "  and the control carried no address policy" "0" \
  "$(grep 'linux_offline_runtime.py control' "${appliance}/sudo.log" | grep -c 'IPAddressDeny')"
check "  the probe ran denied" "1" \
  "$(grep -c 'systemd-run .*--property=IPAddressDeny=any.*-- python3 .*linux_offline_runtime.py probe' "${appliance}/sudo.log")"
# What a kernel answers: the metadata service's connect silenced, its
# address refused outright as a datagram.
check "  the metadata service answered the control and went silent under the denial" \
  "ok ok timeout EPERM" \
  "$(python3 -c 'import json,sys
control=json.load(open(sys.argv[1]))["denied"]
probe=json.load(open(sys.argv[2]))["denied"]
print(control["tcp_gce_metadata"], control["udp_gce_metadata"],
      probe["tcp_gce_metadata"], probe["udp_gce_metadata"])' \
    "${ok_evidence}/offline-control.json" "${ok_evidence}/offline-probe.json")"
check "  the resolver stub is cut off under the denial, as the shorthand would not have been" \
  "timeout EPERM" \
  "$(python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))["denied"]
print(d["tcp_resolver_stub"], d["udp_resolver_stub"])' "${ok_evidence}/offline-probe.json")"
# Both services, probed from inside their own control groups: each
# control completed before the denial, each probe refused under it.
for unit in tensorplate-agent tensorplate-observability; do
  check "  ${unit} was probed inside its own control group, against its own control" \
    "ok EPERM unit 6" \
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
check "  where the certificate carries both" \
  "tensorplate-agent.service tensorplate-observability.service" \
  "$(python3 -c 'import json,sys
print(" ".join(sorted(json.load(open(sys.argv[1]))["units_probed_in_their_own_control_group"])))' \
    "${ok_evidence}/offline-runtime.json")"
# Read from what the stubs saw: the offline stage made exactly five CLI
# calls, every one denied; no call before or after it was denied or made
# in a transient unit at all; the installer ran with no denial in place;
# and no offline-only privileged command ran outside the stage.
check "  the denial reached exactly the offline stage" \
  "window=yes outside=0 denied=5 leaked=0 transient=0 installs=1 installs_denied=0" \
  "$(offline_scope)"
check "  the agent socket and the serving port stay reachable under the denial" "ok ok" \
  "$(python3 -c 'import json,sys
a=json.load(open(sys.argv[1]))["allowed"]
print(a["unix_agent_socket"], a["tcp_loopback_serving_port"])' "${ok_evidence}/offline-probe.json")"
check "  the identity came from the boot-bound record" "recorded_gce_metadata not_applicable" \
  "$(python3 -c 'import json,sys
r=json.load(open(sys.argv[1]))
print(r["machine_type_source"], r["record"])' "${ok_evidence}/offline-identity.json")"
check "  a fresh deployment was made while denied" "cloud-lifecycle-smoke-offline" \
  "$(python3 -c 'import json,sys
print(json.load(open(sys.argv[1]))["payload"]["deployment_id"])' \
    "${ok_evidence}/offline-deploy.json")"
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
check "  and names the operations the denial refused, and the connects it silenced" "6 2" \
  "$(python3 -c 'import json,sys
c=json.load(open(sys.argv[1]))["classification"]
print(len(c["operations_refused_under_the_denial"]), len(c["operations_silenced_under_the_denial"]))' \
    "${ok_evidence}/offline-runtime.json")"
check "  and the restored readback compared prefixes the host printed in either order" \
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
  "$(env TP_OFFLINE_UNIT_ROOT="$show_order" "${appliance}/bin/systemctl" show \
       -p LoadState -p ActiveState -p InvocationID -p IPAddressDeny -p IPAddressAllow \
       -- tensorplate-observability | grep '^IPAddress' | tr '\n' '|' | sed 's/|$//')"
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

# Every way the offline stage could certify a host it did not deny, an
# identity it did not establish, or a host it did not put back. Each mode
# must fail the stage AND leave the drop-ins removed: a run that ends
# with the appliance denied cannot reach anything, including whatever the
# operator would use to recover it.
offline_drop_ins_left() {
  local unit path count=0
  for unit in tensorplate-agent tensorplate-observability; do
    path="$(offline_drop_in "$unit")"
    # -L too: a dangling symlink at that path is a file the run left
    # behind, and -e alone reports it as gone.
    [[ -e "$path" || -L "$path" ]] && count=$((count + 1))
  done
  printf '%s' "$count"
}
for mode in offline-no-machine-type-record offline-denial-inert offline-localhost-shorthand \
            offline-unit-dead offline-probe-leaks offline-probe-crashes \
            offline-control-refused offline-control-metadata-unreachable \
            offline-control-child-fails \
            offline-transient-not-denied offline-agent-socket-unreachable \
            offline-doctor-live-metadata offline-doctor-failing offline-doctor-row-warning \
            offline-identity-live-metadata offline-identity-rewrites-record \
            offline-identity-undetected offline-identity-twice \
            offline-persistent-drop-in offline-drop-in-extra-directive \
            offline-deny-restart-ignored offline-restore-restart-ignored \
            offline-service-filter-not-attached offline-unit-control-refused; do
  evidence="${td}/stages-${mode}"
  # The run exits with the failed stage's status: a crashing probe's is
  # the module's for a failure it did not anticipate, and a transient
  # unit with no filter is refused by the first CLI call's own probe.
  expected_status=1
  case "$mode" in
    offline-probe-crashes) expected_status=70 ;;
    offline-transient-not-denied) expected_status=71 ;;
  esac
  check "${mode} fails the run" "$expected_status" "$(run_stages "$mode" "$evidence")"
  check "  crash-loop passed before it" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and offline is recorded as a failure, not a pass" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" offline)"
  check "  and the network policy is put back anyway" 0 "$(offline_drop_ins_left)"
  if [[ "$mode" == offline-transient-not-denied ]]; then
    check "  and no CLI call ran while the services were denied" 0 \
      "$(grep -c ' dropins=[1-9]' "${appliance}/cli.log" || true)"
  fi
  # A control that cannot be a baseline is refused as it is taken, by the
  # step that took it and for the operation that did not complete, and
  # before anything is denied or restarted.
  control_step=""
  case "$mode" in
    offline-control-refused)
      control_step="control probe"
      control_failure="control_not_refused:udp_test_net_v4" ;;
    offline-control-metadata-unreachable)
      control_step="control probe"
      control_failure="control_metadata_service_reachable:tcp_gce_metadata" ;;
    offline-control-child-fails)
      control_step="control probe"
      control_failure="control_completed:udp_test_net_v4_from_child" ;;
    offline-unit-control-refused)
      control_step="control inside the tensorplate-agent control group"
      control_failure="control_not_refused:udp_test_net_v4" ;;
  esac
  if [[ -n "$control_step" ]]; then
    check "  and the control is refused as it is taken, naming what it did not do" "yes yes" \
      "$(printf '%s %s' \
         "$(grep -Fxq "step failed (exit 1): ${control_step}" \
              "${evidence}/offline.log" && echo yes || echo no)" \
         "$(grep -F 'control checks failed: ' "${evidence}/offline.log" \
              | grep -Fq "$control_failure" && echo yes || echo no)")"
    check "  before any drop-in is installed or any service restarted for it" "0 0" \
      "$(grep -c '^install -D -m 0644 ' "${appliance}/sudo.log" || true) $(sed -n \
           '/python3 .*linux_offline_runtime\.py control/,$p' "${appliance}/sudo.log" \
           | grep -c 'systemctl restart' || true)"
  fi
done

# A probe that crashes, from a checkout under a home directory -- the
# shape of CI's own /home/runner checkout, where an uncaught traceback
# quoted the module's path into offline.log and the publication scan
# refused the run's evidence. The module names the subcommand and the
# exception type and nothing else, so the evidence stays publishable.
# The harness, the runner and the module are copied there together,
# because the harness finds the module beside itself.
home_checkout="${td}/home/tp-reviewer/checkout"
mkdir -p "${home_checkout}/tools/validation"
for file in ubuntu-l4-cloud-lifecycle.sh lifecycle-stages.sh linux_offline_runtime.py; do
  cp "${repo_root}/tools/validation/${file}" "${home_checkout}/tools/validation/${file}"
done
checkout_root="$home_checkout"
checkout_harness="${home_checkout}/tools/validation/ubuntu-l4-cloud-lifecycle.sh"
for crash_case in "offline-probe-crashes|probe-unit|probe inside the tensorplate-agent control group|0" \
                  "offline-cli-probe-crashes|run-denied|status under denial|0" \
                  "offline-transient-probe-crashes|probe|denied probe|5"; do
  IFS='|' read -r crash_mode crash_subcommand crash_step crash_calls <<<"$crash_case"
  evidence="${td}/stages-home-${crash_mode}"
  check "${crash_mode}, from a checkout under a home directory, fails the run" 70 \
    "$(run_stages "$crash_mode" "$evidence")"
  check "  and offline is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" offline)"
  check "  and the network policy is put back anyway" 0 "$(offline_drop_ins_left)"
  check "  and the stage log names the subcommand and the exception type" "yes yes" \
    "$(printf '%s %s' \
       "$(grep -Fxq "error: ${crash_subcommand} failed unexpectedly: RuntimeError" \
            "${evidence}/offline.log" && echo yes || echo no)" \
       "$(grep -Fxq "step failed (exit 70): ${crash_step}" \
            "${evidence}/offline.log" && echo yes || echo no)")"
  check "  and the CLI calls made under the denial are the ones before the crash" \
    "$crash_calls" "$(grep -c ' denied=1 ' "${appliance}/cli.log" || true)"
  check "  and no file of the run quotes a traceback, the exception or the checkout" 0 \
    "$(grep -rlF -e Traceback -e 'the probe crashed' -e "$home_checkout" "$evidence" \
       | wc -l | tr -d ' ')"
  check_publishable "  and the run's evidence passes the publication scanner" \
    "${td}/home-${crash_mode}-scan.out" "$evidence"
done
checkout_root="$repo_root"
checkout_harness="$harness"

# The drop-in removal itself failing must fail the run rather than being
# reported as a restored host.
evidence="${td}/stages-offline-drop-in-not-removed"
check "a drop-in that cannot be removed refuses the run" 1 \
  "$(run_stages offline-drop-in-not-removed "$evidence")"
check "  and offline is recorded as a failure" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" offline)"
check "  and the run does not certify itself" fail \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' \
     "${evidence}/lifecycle-report.json")"

# A service whose filter never attached fails on that unit, and only
# there: the transient probe and the other service pass. (A service's
# control that was refused fails before the denial, above.)
check "offline-service-filter-not-attached is refused for tensorplate-observability by its own classification" \
  "yes no" \
  "$(printf '%s %s' \
     "$(grep -Fq "step failed (exit 1): the denial is enforced inside the tensorplate-observability control group" \
          "${td}/stages-offline-service-filter-not-attached/offline.log" && echo yes || echo no)" \
     "$(grep -Fq "the denial is enforced inside the tensorplate-agent control group" \
          "${td}/stages-offline-service-filter-not-attached/offline.log" && echo yes || echo no)")"

# One CLI call's unit whose filter never attached, while every other
# unit's did. The call is never made, the calls before it were, and the
# stage fails on that unit's own probe -- not on anything the call would
# have done. Doctor is here because its exit status is captured rather
# than stepped on.
for cli_case in "doctor|status|doctor resolves ubuntu2404-x86-l4-g2s8 from the recorded machine type|1" \
                "deploy|status doctor|fresh deploy under denial|71" \
                "infer|status doctor deploy status|infer under denial|71"; do
  IFS='|' read -r cli_call cli_before cli_step cli_status <<<"$cli_case"
  evidence="${td}/stages-offline-cli-filter-not-attached-${cli_call}"
  check "offline-cli-filter-not-attached-${cli_call} fails the run" "$cli_status" \
    "$(run_stages "offline-cli-filter-not-attached-${cli_call}" "$evidence")"
  check "  crash-loop passed before it" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and offline is recorded as a failure, not a pass" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" offline)"
  check "  and the network policy is put back anyway" 0 "$(offline_drop_ins_left)"
  check "  and the ${cli_call} unit is refused before ${cli_call} runs" \
    "yes yes" \
    "$(printf '%s %s' \
       "$(grep -Fq "error: the ${cli_call} unit does not enforce the denial, so tensorplate was not run: refused:" \
            "${evidence}/offline.log" && echo yes || echo no)" \
       "$(grep -Fxq "step failed (exit ${cli_status}): ${cli_step}" \
            "${evidence}/offline.log" && echo yes || echo no)")"
  check "  and only the calls before it were made, each denied" "$cli_before" \
    "$(sed -n 's/^\([a-z]*\) denied=1 .*/\1/p' "${appliance}/cli.log" | tr '\n' ' ' | sed 's/ $//')"
  check "  and its unit's own probe is filed, showing nothing refused" ok \
    "$(python3 -c 'import json,sys
p=json.load(open(sys.argv[1]))
print(" ".join(sorted(set(p["denied"].values()))) if p["call"] == sys.argv[2] else "wrong call")' \
      "${evidence}/offline-cli-probe-${cli_call}.json" "$cli_call")"
  check "  and no certificate is filed" no \
    "$([[ -e "${evidence}/offline-runtime.json" ]] && echo yes || echo no)"
done

# One unit's removal failing does not stop the other's, nor the reload,
# the restart and the readback that follow -- in the stage, or in the
# exit handler's retry.
agent_drop_in="$(offline_drop_in tensorplate-agent)"
observability_drop_in="$(offline_drop_in tensorplate-observability)"
evidence="${td}/stages-offline-agent-removal-fails"
check "a drop-in removal that fails for one unit fails the run" 9 \
  "$(run_stages ok "$evidence" "rm -f ${agent_drop_in}")"
check "  and offline is recorded as a failure" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" offline)"
check "  and the other unit's drop-in is removed all the same" "yes no" \
  "$(printf '%s %s' \
     "$([[ -e "$agent_drop_in" ]] && echo yes || echo no)" \
     "$([[ -e "$observability_drop_in" ]] && echo yes || echo no)")"
check "  and systemd was reloaded after the removals, in the stage and on exit" 3 \
  "$(grep -c '^systemctl daemon-reload$' "${appliance}/sudo.log")"
check "  and the operator is told what is left and how to remove it" yes \
  "$(grep -Fq "remove it with: sudo rm -f ${agent_drop_in} ${observability_drop_in} && sudo systemctl daemon-reload && sudo systemctl restart tensorplate-agent tensorplate-observability" \
       "${evidence}/offline.log" && echo yes || echo no)"

evidence="${td}/stages-offline-cleanup-invocation-unreadable"
check "an invocation the cleanup cannot read fails the run" 1 \
  "$(run_stages offline-cleanup-invocation-unreadable "$evidence")"
check "  and offline is recorded as a failure" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" offline)"
# Both drop-ins go, systemd is reloaded in the stage and again by the exit
# handler's retry, and the retry -- which can read the agent's invocation
# by then -- files a readback that compared both.
check "  but both drop-ins are removed, and the exit handler retries the rest" "0 3" \
  "$(printf '%s %s' "$(offline_drop_ins_left)" \
     "$(grep -c '^systemctl daemon-reload$' "${appliance}/sudo.log")")"
check "  and the retry's readback compared both services' instances" \
  "tensorplate-agent.service tensorplate-observability.service" \
  "$(python3 -c 'import json,sys
print(" ".join(json.load(open(sys.argv[1]))["units_restarted"]))' \
    "${evidence}/offline-restored.json")"

# A drop-in an earlier run left behind is refused before the run begins:
# nothing is installed with the services denied.
evidence="${td}/stages-offline-leftover-before-run"
check "a leftover drop-in from an earlier run refuses the run" 1 \
  "$(run_stages offline-leftover-before-run "$evidence" 2>/dev/null)"
check "  before anything privileged runs, installer included" 0 \
  "$(wc -l <"${appliance}/sudo.log" | tr -d ' ')"
check "  and before a report is written" no \
  "$([[ -e "${evidence}/lifecycle-report.json" ]] && echo yes || echo no)"
check "  and names the leftover" yes \
  "$(grep -Fq "is already installed at ${observability_drop_in}" "${evidence}.err" \
     && echo yes || echo no)"

# A privileged step of the offline stage that fails must fail the stage.
# Written the obvious way -- errexit is suspended inside a stage body --
# an unguarded `sudo ...` here would be invisible.
for failing in "install -D" "systemctl daemon-reload" "systemd-run"; do
  evidence="${td}/stages-offline-fails-${failing// /-}"
  check "an offline step that fails (${failing}) is recorded as a failed offline" fail \
    "$(run_stages ok "$evidence" "$failing" >/dev/null; \
       stage_status "${evidence}/lifecycle-report.json" offline)"
  check "  and the drop-ins are still removed" 0 "$(offline_drop_ins_left)"
done

# A drop-in left by an earlier run means this run never applied the
# denial it would certify, so it is refused before anything is installed.
#
# One case per unit, because the refusal is one `step` per unit: a suite
# that only ever planted the agent's leftover would let the observability
# half be deleted with nothing failing. The dangling symlink is the third
# case: `-e` alone is false for one, and `install -D` would then write
# through it to wherever it points.
for leftover_mode in offline-leftover-drop-in offline-leftover-drop-in-observability \
                     offline-leftover-dangling-symlink; do
  leftover_evidence="${td}/stages-${leftover_mode}"
  check "${leftover_mode} refuses the offline stage" fail \
    "$(run_stages "$leftover_mode" "$leftover_evidence" >/dev/null; \
       stage_status "${leftover_evidence}/lifecycle-report.json" offline)"
  check "  and the leftover is not removed by a run that refused it" 1 \
    "$(offline_drop_ins_left)"
done

# Signals during the stage take the same cleanup path as a failure.
for signal_case in int:130 term:143 hup:129; do
  signal="${signal_case%:*}"
  expected_status="${signal_case#*:}"
  evidence="${td}/stages-offline-signal-${signal}"
  check "${signal} while the network is denied preserves the signal exit status" \
    "$expected_status" "$(run_stages "offline-signal-${signal}" "$evidence")"
  check "  the interrupted offline stage is recorded as failed" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" offline)"
  check "  and the signal cleanup removes every drop-in" 0 "$(offline_drop_ins_left)"
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
# Each unit's invocation id carries that unit in its prefix and its start
# generation in the last four digits, so a capture names one start rather
# than the unit.
for prefix in 1111111111111111111111111111 2222222222222222222222222222; do
  check "  journal capture selects a current invocation of ${prefix}" yes \
    "$(grep -F "journalctl" "${appliance}/sudo.log" \
       | grep -Eq "_SYSTEMD_INVOCATION_ID=${prefix}[0-9a-f]{4}" && echo yes || echo no)"
done
# And selects the start that was current each time, not one id for the
# whole run: the agent is restarted between the status-logs capture and
# the offline one, so the two name different starts.
check "  and each capture names the start that was current then" yes \
  "$(grep -F "journalctl" "${appliance}/sudo.log" \
     | sed -n 's/.*_SYSTEMD_INVOCATION_ID=1111111111111111111111111111\([0-9a-f]\{4\}\).*/\1/p' \
     | python3 -c 'import sys
starts = [int(value, 16) for value in sys.stdin.read().split()]
print("yes" if len(starts) >= 2 and starts[-1] > starts[0] else f"no: {starts}")')"

# --- the kept journal fields are one decision recorded in three files.
#
# The harness projects to a field set, this verifier asserts that set,
# and the publication scanner admits it. Any two of the three agreeing
# proves nothing about the third: a set widened in the harness and in
# this file together would still produce evidence the scanner refuses,
# and a set widened in the harness alone would be asserted by a stale
# expectation here. Compare all three before using any of them.
check "  the harness, this verifier and the scanner keep the same fields" yes \
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
# A renamed or emptied collection reads as no field set at all, which is
# a drift this has to report rather than silently compare nothing.
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

# --- the journal captures are projected where they are captured.
#
# The stub journalctl emits the metadata systemd attaches to every real
# record. What reaches the evidence directory must carry only the fields
# the stage assertions read -- the set the publication scanner accepts --
# and must still carry them, or the projection has taken out what the
# assertions depend on. The expected record counts are the stub's own, so
# a projection that dropped records would fail here too.
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
# systemd's own restart records name the unit in UNIT and have no
# invocation id, so only MESSAGE and _SYSTEMD_UNIT are required of every
# record here.
check "  the crash-loop journal is projected to the service's own fields" "11 none none" \
  "$(journal_projection "${ok_evidence}/crash-loop-journal.txt" MESSAGE _SYSTEMD_UNIT)"
# A projection that ran but left the raw capture behind would publish
# nothing, and would still leave host metadata on the machine for the
# next thing that collects logs. The scratch space is shared by every
# run in this file, so the count is cumulative: the first run that
# leaves a capture behind fails its own check, and every later one.
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

# --- the producer's output passes the publication scanner.
#
# Projecting the captures is worth doing only if what the harness
# produces is publishable, and that is a property of the evidence
# directory rather than of the journal files alone. This is the command
# the runbook gives the operator, without the private literal file CI
# cannot have: it closes the chain from the harness that writes evidence
# to the scanner that admits it. A stub run's paths are mktemp paths, so
# a runner whose TMPDIR sat under a home directory would report
# home-path findings here.
check_publishable "  and the run's own evidence passes the publication scanner" \
  "${td}/publication-scan.out" "$ok_evidence"

# --- and the scanner refuses a capture that was not projected.
#
# A passing scan says the evidence is publishable. On its own it says
# nothing about the projection: it would pass just as well if the
# scanner did not object to host metadata in the first place, and then
# the check above would certify a harness that had stopped projecting.
# So drive the other side with the same scanner over the same evidence
# directory, one journal file replaced by the stub journalctl's own
# unprojected output.
#
# The stub's host values are the synthetic ones the README names, so
# what the scanner has to refuse here is the field name alone -- exactly
# what the projection takes out, and the only difference between the two
# directories.
#
# Both unprojected captures are checked for the whole field set the stub
# emits, so a stub trimmed back to the handful of host fields a denylist
# would name fails here rather than quietly admitting one. The field
# named per run sorts between SYSLOG_FACILITY and the underscored names.
raw_capture="${td}/raw-agent-journal.txt"
TP_FAKE_MODE=ok "${appliance}/bin/journalctl" -u tensorplate-agent.service \
  _SYSTEMD_INVOCATION_ID=11111111111111111111111111111111 \
  -n 100 --no-pager --output=json >"$raw_capture"
service_extra="SYSLOG_FACILITY,${TP_FAKE_JOURNAL_FIELD},_BOOT_ID,_CAP_EFFECTIVE,_CMDLINE,_COMM,_EXE,_GID,_HOSTNAME,_MACHINE_ID,_STREAM_ID,_SYSTEMD_CGROUP,_SYSTEMD_SLICE,_TRANSPORT,_UID,__CURSOR,__MONOTONIC_TIMESTAMP,__SEQNUM,__SEQNUM_ID"
check "  the unprojected capture carries the fields the projection removes" \
  "1 ${service_extra} none" \
  "$(journal_projection "$raw_capture" MESSAGE _SYSTEMD_UNIT _SYSTEMD_INVOCATION_ID)"
raw_crash_loop="${td}/raw-crash-loop-journal.txt"
TP_FAKE_MODE=ok "${appliance}/bin/journalctl" -u tensorplate-agent --since @0 \
  --no-pager --output=json >"$raw_crash_loop"
check "  and so does the unprojected crash-loop capture, systemd's records included" \
  "11 CODE_FILE,CODE_FUNC,CODE_LINE,INVOCATION_ID,MESSAGE_ID,SYSLOG_FACILITY,${TP_FAKE_JOURNAL_FIELD},_BOOT_ID,_CAP_EFFECTIVE,_CMDLINE,_COMM,_EXE,_GID,_HOSTNAME,_MACHINE_ID,_SOURCE_REALTIME_TIMESTAMP,_STREAM_ID,_SYSTEMD_CGROUP,_SYSTEMD_SLICE,_TRANSPORT,_UID,__CURSOR,__MONOTONIC_TIMESTAMP,__SEQNUM,__SEQNUM_ID none" \
  "$(journal_projection "$raw_crash_loop" MESSAGE _SYSTEMD_UNIT)"

unprojected="${td}/evidence-unprojected"
rm -rf "$unprojected"
cp -R "$ok_evidence" "$unprojected"
cp "$raw_capture" "${unprojected}/agent-journal.txt"
unprojected_scan="${td}/unprojected-scan.out"
check "  and the same evidence with one capture unprojected is refused" 1 \
  "$(publication_scan "$unprojected_scan" "$unprojected")"
# Refused for that file and for its journal fields, not for something
# the copy happened to disturb.
check "  named by file and class, and by nothing else" "agent-journal.txt journal-field" \
  "$(python3 - "$unprojected_scan" <<'PY'
import re, sys

refs, classes = set(), set()
for line in open(sys.argv[1], encoding="utf-8"):
    match = re.match(r"^(\S+):\d+: (.+) \(\d+ chars\)$", line.rstrip("\n"))
    if match:
        refs.add(match.group(1))
        classes.add(match.group(2))
print(",".join(sorted(refs)) or "none", ",".join(sorted(classes)) or "none")
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

# Install runs online, so doctor has to have read the machine type live.
# A shape from the record, or none at all, fails the stage.
for mode in install-doctor-recorded install-doctor-no-source; do
  mode_evidence="${td}/stages-${mode}"
  check "${mode} fails the install stage" "fail" \
    "$(run_stages "$mode" "$mode_evidence" >/dev/null; stage_status "${mode_evidence}/lifecycle-report.json" install)"
  check "  and says doctor did not detect the host live" yes \
    "$(grep -Fq 'host_os does not show live detection from GCE metadata' \
         "${mode_evidence}/install.log" && echo yes || echo no)"
done

# A bundle that cannot be read, from under a home directory. The
# deploy-smoke bundle is the checkout's by default, and an OSError's
# message, cp's and cd's all quote the path they were given: the stage
# log names a file by its place in the bundle instead. The bundle check
# reads the manifest and the model; the copy reads every file, so a file
# the check never opens is the copy's to report, by a relative path. The
# copy enters the bundle to do that, and a bundle it cannot enter by then
# is refused without its path and without copying anything else.
#
# Each case: what to break, the file, a line the stage log must carry
# exactly, and an extended regex another of its lines must match -- cp's
# wording differs between GNU and BSD.
home_bundle="${td}/home/tp-reviewer/bundle"
for bundle_case in \
    "model-missing|x86-smoke.json|deploy-smoke bundle file \"x86-smoke.json\" cannot be read: No such file or directory|^deploy-smoke bundle file .*No such file or directory$" \
    "manifest-unreadable|manifest.json|deploy-smoke bundle file \"manifest.json\" cannot be read: Permission denied|^deploy-smoke bundle file .*Permission denied$" \
    "extra-unreadable|extra.bin|step failed (exit 1): copy the bundle|^cp: .*\./extra\.bin.*Permission denied$" \
    "dir-unenterable||step failed (exit 1): copy the bundle|^cannot enter the deploy-smoke bundle directory$"; do
  IFS='|' read -r bundle_mode bundle_file bundle_line bundle_pattern <<<"$bundle_case"
  rm -rf "$home_bundle"
  mkdir -p "$(dirname "$home_bundle")"
  cp -R "$bundle" "$home_bundle"
  case "$bundle_mode" in
    model-missing)
      rm -f "${home_bundle}/${bundle_file}"
      ;;
    dir-unenterable)
      locked_bundle="$home_bundle"
      ;;
    *)
      touch "${home_bundle}/${bundle_file}"
      chmod 000 "${home_bundle}/${bundle_file}"
      # Root reads a mode-000 file, and this case would prove nothing.
      check "a ${bundle_mode} bundle's ${bundle_file} is unreadable to this suite" no \
        "$([[ -r "${home_bundle}/${bundle_file}" ]] && echo yes || echo no)"
      ;;
  esac
  evidence="${td}/stages-home-bundle-${bundle_mode}"
  check "a ${bundle_mode} bundle fails the run" 1 \
    "$(run_harness ok "$evidence" "" "$assets" --bundle-dir "$home_bundle")"
  locked_bundle=""
  chmod 755 "$home_bundle"
  chmod -R u+rw "$home_bundle"
  check "  after install passed, as a failed deploy-smoke" "pass fail" \
    "$(printf '%s %s' "$(stage_status "${evidence}/lifecycle-report.json" install)" \
       "$(stage_status "${evidence}/lifecycle-report.json" deploy-smoke)")"
  check "  saying what could not be read, and why" "yes yes" \
    "$(printf '%s %s' \
       "$(grep -Fxq "$bundle_line" "${evidence}/deploy-smoke.log" && echo yes || echo no)" \
       "$(grep -Eq "$bundle_pattern" "${evidence}/deploy-smoke.log" && echo yes || echo no)")"
  check "  and no file of the run quotes a traceback or the bundle's path" 0 \
    "$(grep -rlF -e Traceback -e "$home_bundle" "$evidence" | wc -l | tr -d ' ')"
  check_publishable "  and the run's evidence passes the publication scanner" \
    "${td}/home-bundle-${bundle_mode}-scan.out" "$evidence"
  if [[ "$bundle_mode" == dir-unenterable ]]; then
    check "  and nothing was copied from anywhere else" "0 no" \
      "$(grep -c '^cp -R ' "${appliance}/sudo.log" || true) $(
         [[ -e "${appliance}/staged-bundle" ]] && echo yes || echo no)"
  fi
done

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
  # status-logs captured the journals, then the run failed. A harness
  # that projected its evidence at the end of a successful run instead of
  # at each capture would leave this one raw, which is the case the
  # projection exists for: an aborted run's evidence is still filed.
  check "  the journal captured before the failure is projected too" "1 none none" \
    "$(journal_projection "${evidence}/agent-journal.txt" \
       MESSAGE _SYSTEMD_UNIT _SYSTEMD_INVOCATION_ID)"
done

for mode in journal-command-fails journal-empty-agent journal-no-entries journal-empty-observability \
            journal-stale-invocation journal-wrong-unit journal-empty-message journal-trailing-line; do
  evidence="${td}/stages-${mode}"
  expected_status=1
  if [[ "$mode" == journal-command-fails ]]; then expected_status=9; fi
  check "${mode} fails the run" "$expected_status" "$(run_stages "$mode" "$evidence")"
  check "  deployment passed before the journal failure" pass \
    "$(stage_status "${evidence}/lifecycle-report.json" deploy-smoke)"
  check "  invalid journal evidence fails status-logs" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" status-logs)"
  # Whatever the verdict on the capture, its raw copy is gone. Most of
  # these captures carry host fields, so a harness that skipped the
  # removal on a failing capture fails here.
  check "  and no raw capture is left in the harness's scratch space" 0 \
    "$(scratch_holding_host_metadata)"
  # A line that is not a record is refused by the projection, and the
  # projection is the only thing that reads the raw capture: the stage's
  # own parser sees only what was projected. So each refusal is checked
  # by its message. On its own, `-- No entries --` would also fail the
  # parser's empty-capture check; a line after a valid record would not.
  case "$mode" in
    journal-no-entries)
      check "  because the projection refused the diagnostic line" yes \
        "$(stage_log_says "${evidence}/status-logs.log" \
           "tensorplate-agent: journal line 1 is not a JSON record")"
      ;;
    journal-trailing-line)
      check "  because the projection refused the line after the record" yes \
        "$(stage_log_says "${evidence}/status-logs.log" \
           "tensorplate-agent: journal line 2 is not a JSON record")"
      # The record before the refused line was filed, and filed
      # projected: a refusal does not copy the raw capture through.
      check "  and what was filed before the refusal is projected" "1 none none" \
        "$(journal_projection "${evidence}/agent-journal.txt" \
           MESSAGE _SYSTEMD_UNIT _SYSTEMD_INVOCATION_ID)"
      ;;
  esac
done

# A signal after journalctl has written the raw capture, and before the
# harness has projected or removed it. The EXIT handler removes it.
evidence="${td}/stages-journal-signal-term"
check "a signal during a journal capture preserves the signal exit status" 143 \
  "$(run_stages journal-signal-term "$evidence")"
check "  the interrupted status-logs stage is recorded as failed" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" status-logs)"
check "  and the raw capture is removed on the way out" 0 \
  "$(scratch_holding_host_metadata)"

# --- crash-loop recovery.
#
# This stage breaks the appliance on purpose, so check restoration as
# well as the stage verdict, including interruption and restore failure.
sudo_line() {
  grep -nF -- "$1" "${appliance}/sudo.log" | head -n1 | cut -d: -f1
}
restore_line='/agent.json /etc/tensorplate/agent.json'
config_restored() {
  [[ ! -e "${appliance}/config-broken" && \
     "$(cat "${appliance}/agent-config")" == '{"fixture":"original agent config"}' ]] && echo yes || echo no
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
check "  and the original config bytes were restored" yes "$(config_restored)"
for mode in crash-loop-keeps-restarting crash-loop-not-retried crash-loop-other-error \
            crash-loop-never-fails crash-loop-stopped crash-loop-trailing-line; do
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_stages "$mode" "$evidence")"
  check "  restart passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" restart)"
  check "  and crash-loop is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and the agent config was restored anyway" yes \
    "$(config_restored)"
  check "  and no raw capture is left in the harness's scratch space" 0 \
    "$(scratch_holding_host_metadata)"
  # Five config errors were captured, which is all the stage's own
  # checks ask for; only the projection's refusal of the line after them
  # fails this run.
  if [[ "$mode" == crash-loop-trailing-line ]]; then
    check "  because the projection refused the line after the records" yes \
      "$(stage_log_says "${evidence}/crash-loop.log" \
         "crash-loop: journal line 12 is not a JSON record")"
    check "  and what was filed before the refusal is projected" "11 none none" \
      "$(journal_projection "${evidence}/crash-loop-journal.txt" MESSAGE _SYSTEMD_UNIT)"
  fi
done

evidence="${td}/stages-corrupt-fails"
check "a config corruption that fails is recorded as a failed crash-loop" fail \
  "$(run_stages ok "$evidence" "invalid json" >/dev/null; \
     stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  and the config is still restored" yes \
  "$(config_restored)"

for signal_case in int:130 term:143 hup:129; do
  signal="${signal_case%:*}"
  expected_status="${signal_case#*:}"
  evidence="${td}/stages-crash-loop-signal-${signal}"
  check "${signal} during config corruption preserves the signal exit status" "$expected_status" \
    "$(run_stages "crash-loop-signal-${signal}" "$evidence")"
  check "  the interrupted crash-loop stage is recorded as failed" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  the signal cleanup restores the original config bytes" yes "$(config_restored)"
  check "  and starts the restored agent" yes \
    "$(grep -Fxq 'systemctl start tensorplate-agent' "${appliance}/sudo.log" && echo yes || echo no)"
done

# As during status-logs, but with the agent config broken: the EXIT
# handler both restores it and removes the raw crash-loop capture.
evidence="${td}/stages-crash-loop-journal-signal-term"
check "a signal during the crash-loop capture preserves the signal exit status" 143 \
  "$(run_stages crash-loop-journal-signal-term "$evidence")"
check "  the interrupted crash-loop stage is recorded as failed" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  the signal cleanup restores the original config bytes" yes "$(config_restored)"
check "  and the raw capture is removed on the way out" 0 \
  "$(scratch_holding_host_metadata)"

evidence="${td}/stages-crash-loop-restore-fails-once"
check "a failed config restore is retried on exit without hiding its failure" 9 \
  "$(run_stages crash-loop-restore-fails-once "$evidence")"
check "  the failed restore still fails the crash-loop stage" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  the exit retry restores the original config bytes" yes "$(config_restored)"

evidence="${td}/stages-crash-loop-restore-always-fails"
check "a persistent config restore failure refuses the run" 9 \
  "$(run_stages crash-loop-restore-always-fails "$evidence")"
check "  and preserves the backup for manual recovery" yes \
  "$(backup="$(cat "${appliance}/backup-path")"
     [[ -f "$backup" && "$(cat "$backup")" == '{"fixture":"original agent config"}' ]] && echo yes || echo no)"
check "  the report does not certify the failed recovery" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"

# --- upgrade and rollback.
#
# Against a fake package database that installs from each set's manifest
# and refuses a downgrade the way apt-get -y does, so a rollback that
# does not remove the newer set first fails here as it would on a host.
last_sudo_line() {
  grep -nF -- "$1" "${appliance}/sudo.log" | tail -n1 | cut -d: -f1
}
# Whether any sudo.log line after line $1 contains $2. A missing anchor
# line answers `missing`, so a check can never pass for want of one.
sudo_after() {
  if [[ -z "$1" ]]; then
    echo missing
    return 0
  fi
  tail -n "+$(($1 + 1))" "${appliance}/sudo.log" | grep -qF -- "$2" && echo yes || echo no
}
sha256_of() {
  python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}
one_line() {
  tr '\n' ' ' <"$1" | sed 's/ $//'
}

evidence="${td}/stages-upgrade-ok"
report="${evidence}/lifecycle-report.json"
check "a run with a baseline completes" 0 "$(run_upgrade_stages ok "$evidence" "" set-rc1)"
for stage in install deploy-smoke status-logs restart crash-loop upgrade rollback; do
  check "  ${stage} is recorded as a pass" pass "$(stage_status "$report" "$stage")"
done
check "  offline still passes with a baseline supplied" pass "$(stage_status "$report" offline)"
# Install and upgrade stay online, rollback too: every installer run with
# no denial in place, no CLI call outside the offline stage denied or made
# in a transient unit, and no offline-only privileged command after the
# stage -- the old never-mutates guard, kept for everything the offline
# stage is not.
check "  and the denial reached exactly the offline stage, not install, upgrade or rollback" \
  "window=yes outside=0 denied=5 leaked=0 transient=0 installs=4 installs_denied=0" \
  "$(offline_scope)"
# The install stage's listing is the dpkg-query shape -- `<name> <status>
# <version>` and nothing else -- which is the reason it is that query:
# `dpkg -l` adds each package's description, and the packaging
# descriptions quote planning identifiers, which belong in the changelog
# and not in published evidence. Asserted on a run that installs a
# release-shaped set, because the minimal fixture set installs no
# packages at all. The fake dpkg's `-l` output carries such an
# identifier, so a harness that went back to it also fails the
# publication scan above.
check "  the install listed the installed packages, without descriptions" yes \
  "$(python3 - "${evidence}/packages.txt" <<'PY'
import sys

names, problem = [], ""
for line in open(sys.argv[1], encoding="utf-8"):
    if not line.strip():
        continue
    fields = line.split()
    if len(fields) != 3:
        problem = f"{len(fields)} fields, not 3: {line.rstrip()}"
        break
    names.append(fields[0])
if problem:
    print(problem)
elif "tensorplate-agent" not in names or len(names) < 2:
    print(f"listed {names}")
else:
    print("yes")
PY
)"
# The upgrade and rollback files -- both sets' listings, their logs, the
# upgrade path, doctor on the baseline -- exist only on a run with a
# baseline, so the scan of the run without one says nothing about them.
check_publishable "  and the run's own evidence passes the publication scanner" \
  "${td}/upgrade-publication-scan.out" "$evidence"
# With a baseline every canonical stage runs, so nothing is skipped and
# the run is no longer incomplete. This is the only configuration in
# which the harness can produce a complete report.
check "  and all eight canonical stages ran" 8 \
  "$(python3 -c 'import json,sys
stages=json.load(open(sys.argv[1]))["stages"]
print(sum(1 for s in stages if s["status"] == "pass"))' "$report")"
check "  so the run is complete" pass \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' "$report")"
check "  and the report is schema-valid" "yes" \
  "$(python3 - "$schema" "$report" <<'PY'
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
candidate_digest="$(sha256_of "${evidence}.sets/assets-rc2/SHA256SUMS")"
baseline_digest="$(sha256_of "${evidence}.sets/set-rc1/SHA256SUMS")"
check "  the report's artifact digest is the candidate's" "$candidate_digest" \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["subject"].get("artifact_digest"))' "$report")"
check "  artifact-digest.txt names the candidate's SHA256SUMS alone" "${candidate_digest}  SHA256SUMS" \
  "$(cat "${evidence}/artifact-digest.txt")"
check "  the baseline's digest is filed in its own file" "${baseline_digest}  SHA256SUMS" \
  "$(cat "${evidence}/baseline-digest.txt" 2>/dev/null)"
check "  and appears nowhere in the report" no \
  "$(grep -Fq "$baseline_digest" "$report" && echo yes || echo no)"
check "  upgrade-path.json records both sides, the baseline signed" \
  "v0.2.1-rc.1 ${baseline_digest} False 0.2.1~rc.1-1 -> v0.2.1-rc.2 ${candidate_digest} False 0.2.1~rc.2-1" \
  "$(python3 - "${evidence}/upgrade-path.json" <<'PY'
import json, sys
path = json.load(open(sys.argv[1]))
def side(s):
    return f"{s['release_tag']} {s['sha256sums_sha256']} {s['allow_unsigned']} {s['packages']['tensorplate-backend-python-pytorch']}"
print(f"{side(path['from'])} -> {side(path['to'])}")
PY
)"
# Which agent version answered each deploy and each inference, in order:
# deploy-smoke, restart and crash-loop on the candidate; the offline
# stage's own fresh deploy and inference, still on the candidate and made
# with the network denied; a deploy on the baseline; the baseline's
# deployment answering on the upgraded candidate without a deploy; and a
# fresh deploy on the rolled-back baseline.
check "  deploys ran on candidate, candidate under denial, baseline, rolled-back baseline" \
  "0.2.1~rc.2-1 0.2.1~rc.2-1 0.2.1~rc.1-1 0.2.1~rc.1-1" \
  "$(one_line "${appliance}/deploy-versions.log")"
check "  inferences ran on each install in turn" \
  "0.2.1~rc.2-1 0.2.1~rc.2-1 0.2.1~rc.2-1 0.2.1~rc.2-1 0.2.1~rc.1-1 0.2.1~rc.2-1 0.2.1~rc.1-1" \
  "$(one_line "${appliance}/infer-versions.log")"
# Empty when the line is missing, which every check below treats as a
# failure; `|| true` keeps a missing line from ending the suite early.
first_baseline_install="$(sudo_line 'set-rc1/install.sh --local-artifacts' || true)"
last_baseline_install="$(last_sudo_line 'set-rc1/install.sh --local-artifacts' || true)"
last_candidate_install="$(last_sudo_line 'assets-rc2/install.sh --local-artifacts' || true)"
remove_line="$(sudo_line 'apt-get remove' || true)"
# What the upgrade leaves in the durable state directory for the rollback
# to carry across: the agent's primary state and the recovery copy it
# keeps beside it, both naming the deployment the baseline made, and the
# observability unit's snapshot the install lays down.
aside_state_fixture="{\"active\":\"${deployment_id}\"}"
aside_agent_bak_fixture="$aside_state_fixture"
aside_snapshot_fixture='{"fixture": "observability snapshot", "schema_version": "0.1"}'
# And the machine-type record the candidate's agent wrote, which the
# rollback also copies back out.
aside_record_fixture='{"schema_version":2,"machine_type":"g2-standard-8"}'
check "  the candidate is purged before the baseline is installed" yes \
  "$(purge="$(last_sudo_line 'apt-get purge')"
     [[ -n "$purge" && -n "$first_baseline_install" && "$purge" -lt "$first_baseline_install" ]] && echo yes || echo no)"
check "  the rollback removes after the last candidate install" yes \
  "$([[ -n "$remove_line" && -n "$last_candidate_install" && "$remove_line" -gt "$last_candidate_install" ]] && echo yes || echo no)"
check "  and removes the backend with the rest of the set" yes \
  "$(sed -n "${remove_line:-0}p" "${appliance}/sudo.log" | tr ' ' '\n' | grep -qx 'tensorplate-backend-python-pytorch' && echo yes || echo no)"
check "  and never purges after the last candidate install" no \
  "$(sudo_after "$last_candidate_install" 'apt-get purge')"
# The documented rollback puts the machine-type record back after setting
# state aside and before the baseline's installer starts its agent.
restore_record_line="$(sudo_line 'cp -p /var/lib/tensorplate/state.bak/machine-type.json' || true)"
check "  the rollback restores the machine-type record after the set-aside and before the baseline install" yes \
  "$(mv_line="$(sudo_line 'mv -T /var/lib/tensorplate/state ' || true)"
     [[ -n "$restore_record_line" && -n "$mv_line" && -n "$last_baseline_install" &&
        "$restore_record_line" -gt "$mv_line" && "$restore_record_line" -lt "$last_baseline_install" ]] &&
       echo yes || echo no)"
check "  and the restored record is the one set aside" yes \
  "$(cmp -s "${appliance}/varlib/state/machine-type.json" "${appliance}/varlib/state.bak/machine-type.json" &&
     echo yes || echo no)"
check "  and the candidate's instance binding outlives the rollback" yes \
  "$(cmp -s "${appliance}/varlib/identity/instance-binding.json" <(printf '{"schema_version":1,"fixture":"instance binding"}\n') &&
     echo yes || echo no)"
# Every privileged systemctl call once the baseline is installed, as a
# whole: only the rollback's stop may be there. Matching verbs instead
# would let restart, try-restart, `--now enable` or a systemctl inside
# `bash -c` bring the services back for an installer that no longer does.
check "  the harness's only systemctl call once the baseline is in play is the rollback's stop" \
  "systemctl stop tensorplate-agent tensorplate-observability" \
  "$(if [[ -z "$first_baseline_install" ]]; then echo missing; else
       tail -n "+$((first_baseline_install + 1))" "${appliance}/sudo.log" |
         { grep -F systemctl || true; } | tr '\n' '|' | sed 's/|$//'; fi)"
check "  no crash-loop restore runs once packages are being purged" no \
  "$(sudo_after "$(sudo_line 'apt-get purge')" "$restore_line")"
check "  the removal left every package holding its conffiles" \
  "config-files config-files config-files config-files config-files config-files" \
  "$(awk '{print $2}' "${evidence}/packages-after-remove.txt" | tr '\n' ' ' | sed 's/ $//')"
check "  the rolled-back agent answers with nothing active or previous" "True None None" \
  "$(python3 -c 'import json,sys
a=json.load(open(sys.argv[1]))["payload"]["agent"];print(a.get("available"),a.get("active","absent"),a.get("previous_active","absent"))' \
    "${evidence}/status-after-rollback.json")"
check "  the candidate's state is set aside, not deleted" yes \
  "$([[ -f "${appliance}/varlib/state.bak/state.json" ]] && echo yes || echo no)"
# More than one file, which is the reason the check is about the
# directory: the agent's primary state, the recovery copy it falls back
# to, and the observability unit's snapshot all live there, and a check
# on one pathname would leave the other two unguarded.
check "  the set-aside state is the whole directory, not one file" \
  "machine-type.json observability-snapshot.json state.json state.json.bak" \
  "$(python3 -c 'import os, sys; print(" ".join(sorted(os.listdir(sys.argv[1]))))' \
    "${appliance}/varlib/state.bak")"
check "  and every file that was set aside survived with its bytes" \
  "${aside_state_fixture}|${aside_agent_bak_fixture}|${aside_snapshot_fixture}|${aside_record_fixture}" \
  "$(cd "${appliance}/varlib/state.bak" 2>/dev/null &&
     printf '%s|%s|%s|%s' "$(cat state.json 2>/dev/null || echo missing)" \
       "$(cat state.json.bak 2>/dev/null || echo missing)" \
       "$(cat observability-snapshot.json 2>/dev/null || echo missing)" \
       "$(cat machine-type.json 2>/dev/null || echo missing)")"
# The digests the preservation check compares against are only worth
# anything if they were taken from the stopped agent's own copies, before
# the move left no original to compare with. Consecutive reads of the
# same directory collapse to one token, so this says what happened in
# what order without pinning how many files the directory holds. The
# machine-type record is read on its own at each end: after the upgrade,
# to show it survived, and after the move, to show the restored copy is
# the one set aside.
check "  listed and digested with the services stopped and before the move" \
  "sha256sum-state systemctl-stop ls-state sha256sum-state mv sha256sum-state" \
  "$(if [[ -z "$last_candidate_install" ]]; then echo missing; else
       tail -n "+$((last_candidate_install + 1))" "${appliance}/sudo.log" |
         sed -n -e 's/^systemctl stop tensorplate-agent tensorplate-observability$/systemctl-stop/p' \
                -e 's#^ls -A /var/lib/tensorplate/state$#ls-state#p' \
                -e 's#^sha256sum /var/lib/tensorplate/state/.*$#sha256sum-state#p' \
                -e 's#^mv -T /var/lib/tensorplate/state /var/lib/tensorplate/state.bak$#mv#p' |
         uniq | tr '\n' ' ' | sed 's/ $//'; fi)"
# Every file, not just the one the harness would have known to look for.
check "  and the saved copy is read back after the baseline install, file by file" \
  "yes yes yes yes" \
  "$(printf '%s %s %s %s' \
     "$(sudo_after "$last_baseline_install" 'ls -A /var/lib/tensorplate/state.bak')" \
     "$(sudo_after "$last_baseline_install" 'sha256sum /var/lib/tensorplate/state.bak/state.json')" \
     "$(sudo_after "$last_baseline_install" 'sha256sum /var/lib/tensorplate/state.bak/state.json.bak')" \
     "$(sudo_after "$last_baseline_install" 'sha256sum /var/lib/tensorplate/state.bak/observability-snapshot.json')")"
# And never by existence: `test -f` on a pathname is what this stage used
# to credit the rollback with, and it passes on a file truncated to
# nothing.
check "  and never by existence alone" no \
  "$(grep -qF 'test -f /var/lib/tensorplate/state.bak' "${appliance}/sudo.log" && echo yes || echo no)"
check "  the operator's cli.json edit survived both directions" yes \
  "$(python3 -c 'import sys
print("yes" if open(sys.argv[1],"rb").read() == (sys.argv[2] + "\n\n").encode() else "no")' \
    "${appliance}/cli.json" "$packaged_cli_config")"
check "  doctor on the baseline is filed" "0 0" \
  "$(cat "${evidence}/doctor-baseline.exit") $(cat "${evidence}/doctor-after-rollback.exit")"
check "  publication checked the baseline's exact tag and artifact directory" \
  "v0.2.1-rc.1 ${evidence}.sets/set-rc1" "$(cat "$TP_FAKE_PUBLICATION_LOG")"

# The baseline's doctor is evidence, not a gate: a defect the candidate
# fixed must not fail the candidate's run.
evidence="${td}/stages-baseline-doctor-failing"
check "a baseline whose doctor reports a failure still completes" 0 \
  "$(run_upgrade_stages baseline-doctor-failing "$evidence" "" set-rc1)"
check "  and its doctor exit status is filed" "10 10" \
  "$(cat "${evidence}/doctor-baseline.exit") $(cat "${evidence}/doctor-after-rollback.exit")"

# tensorplate-apt-source configures a channel and is no part of either
# runtime set: the rollback leaves it installed and the set checks ignore it.
evidence="${td}/stages-apt-source-installed"
check "a host with the APT source package completes upgrade and rollback" 0 \
  "$(run_upgrade_stages apt-source-installed "$evidence" "" set-rc1)"
check "  and the rollback does not remove it" no \
  "$(grep -F 'apt-get remove' "${appliance}/sudo.log" | tr ' ' '\n' | grep -qx 'tensorplate-apt-source' && echo yes || echo no)"
check "  though it was installed throughout" yes \
  "$(grep -Fq 'tensorplate-apt-source installed' "${evidence}/packages-after-rollback.txt" && echo yes || echo no)"

evidence="${td}/stages-upgrade-unsigned"
check "a run with a baseline and --allow-unsigned completes" 0 \
  "$(run_upgrade_stages ok "$evidence" "" set-rc1 --allow-unsigned)"
check "  the baseline is installed twice, never unsigned" "2 0" \
  "$(grep -cF 'set-rc1/install.sh' "${appliance}/sudo.log") $(grep -F 'set-rc1/install.sh' "${appliance}/sudo.log" | grep -cF -- '--allow-unsigned')"
check "  the candidate is installed twice, unsigned each time" "2 2" \
  "$(grep -cF 'assets-rc2/install.sh' "${appliance}/sudo.log") $(grep -F 'assets-rc2/install.sh' "${appliance}/sudo.log" | grep -cF -- '--allow-unsigned')"
check "  and upgrade-path.json says which side was unsigned" "False True" \
  "$(python3 -c 'import json,sys;p=json.load(open(sys.argv[1]));print(p["from"]["allow_unsigned"],p["to"]["allow_unsigned"])' \
    "${evidence}/upgrade-path.json")"

for mode in baseline-publication-draft baseline-publication-network-failure \
            baseline-publication-checksum-mismatch; do
  for unsigned in no yes; do
    unsigned_flag=""
    if [[ "$unsigned" == yes ]]; then unsigned_flag=--allow-unsigned; fi
    evidence="${td}/stages-${mode}-${unsigned}"
    check "${mode} refuses the run (unsigned candidate: ${unsigned})" 1 \
      "$(run_upgrade_stages "$mode" "$evidence" "" set-rc1 ${unsigned_flag:+"$unsigned_flag"} 2>/dev/null)"
    check "  before any privileged command" 0 \
      "$(wc -l <"${appliance}/sudo.log" | tr -d ' ')"
    check "  before a lifecycle report is written" no \
      "$([[ -e "${evidence}/lifecycle-report.json" ]] && echo yes || echo no)"
    check "  and preserves the publication refusal diagnostic" yes \
      "$(grep -Fq "baseline publication: synthetic refusal: ${mode}" "${evidence}.err" && echo yes || echo no)"
  done
done

evidence="${td}/stages-baseline-publication-set-changed"
check "a baseline changed after its publication check refuses the run" 1 \
  "$(run_upgrade_stages baseline-publication-set-changed "$evidence" "" set-rc1 2>/dev/null)"
check "  before any privileged command" 0 \
  "$(wc -l <"${appliance}/sudo.log" | tr -d ' ')"
check "  and identifies the changed public-release digest" yes \
  "$(grep -Fq 'baseline SHA256SUMS changed after its public release was verified' \
    "${evidence}.err" && echo yes || echo no)"

evidence="${td}/stages-upgrade-tampered"
check "a baseline that fails its checksums refuses the run" 1 \
  "$(run_upgrade_stages ok "$evidence" "" set-rc1-tampered 2>/dev/null)"
check "  before a report is written" no \
  "$([[ -e "${evidence}/lifecycle-report.json" ]] && echo yes || echo no)"
check "  and before anything privileged runs" 0 \
  "$(wc -l <"${appliance}/sudo.log" | tr -d ' ')"

evidence="${td}/stages-baseline-digest-unreadable"
check "a baseline whose digest cannot be computed refuses the run" 1 \
  "$(run_upgrade_stages baseline-digest-unreadable "$evidence" "" set-rc1 2>/dev/null)"
check "  and says so" yes \
  "$(grep -Fq 'could not compute a digest for' "${evidence}.err" && echo yes || echo no)"
check "  before a report is written" no \
  "$([[ -e "${evidence}/lifecycle-report.json" ]] && echo yes || echo no)"
check "  and before anything privileged runs" 0 \
  "$(wc -l <"${appliance}/sudo.log" | tr -d ' ')"

for case in \
  "baseline-install-noop::tensorplate-common is not-installed -, expected installed 0.2.1~rc.1-1" \
  "candidate-set-changed::SHA256SUMS changed after it was verified" \
  "upgrade-install-fails::step failed (exit 1): install.sh" \
  "upgrade-leaves-baseline-package::tensorplate-serving is installed 0.2.1~rc.1-1, expected installed 0.2.1~rc.2-1" \
  "upgrade-leaves-unpacked::tensorplate-serving is unpacked 0.2.1~rc.2-1, expected installed 0.2.1~rc.2-1" \
  "upgrade-leaves-unlisted-package::tensorplate-unlisted 0.2.1~rc.1-1 is installed but is not in v0.2.1-rc.2" \
  "upgrade-agent-not-restarted::agent MainPID 4242 did not change across the upgrade" \
  "upgrade-observability-not-restarted::observability MainPID 4242 did not change across the upgrade" \
  "upgrade-resets-conffile::the upgrade did not keep the operator-edited" \
  "upgrade-wrong-row::platform_row is warning" \
  "upgrade-doctor-recorded::host_os does not show live detection from GCE metadata" \
  "upgrade-loses-deployment::worker round-trip checks failed" \
  "upgrade-rewrites-record::the machine-type record /var/lib/tensorplate/state/machine-type.json changed across the upgrade: sha256 was" \
  "upgrade-no-binding::step failed (exit 1): the candidate bound the record to this instance" \
  "ok:set-rc1/install.sh:step failed (exit 9): install.sh" \
  "ok:>>:step failed (exit 9): operator edit"; do
  mode="${case%%:*}"
  rest="${case#*:}"
  sudo_fail="${rest%%:*}"
  message="${rest#*:}"
  expected_status=1
  if [[ -n "$sudo_fail" ]]; then expected_status=9; fi
  evidence="${td}/stages-${mode}-${sudo_fail//[!a-z0-9]/-}"
  check "${mode}${sudo_fail:+ with a failing ${sudo_fail}} fails the run" "$expected_status" \
    "$(run_upgrade_stages "$mode" "$evidence" "$sudo_fail" set-rc1)"
  check "  crash-loop passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  upgrade is recorded as a failure" fail "$(stage_status "${evidence}/lifecycle-report.json" upgrade)"
  check "  and rollback never ran" absent "$(stage_status "${evidence}/lifecycle-report.json" rollback)"
  check "  and the upgrade log says why" yes "$(stage_log_says "${evidence}/upgrade.log" "$message")"
  if [[ "$mode" == candidate-set-changed ]]; then
    check "  and the changed candidate was never installed over the baseline" no \
      "$(sudo_after "$(sudo_line 'apt-get purge')" 'assets-rc2/install.sh')"
  fi
done

evidence="${td}/stages-upgrade-signal-term"
check "TERM during the upgrade preserves the signal exit status" 143 \
  "$(run_upgrade_stages upgrade-signal-term "$evidence" "" set-rc1)"
check "  crash-loop passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
check "  the interrupted upgrade is recorded as failed" fail \
  "$(stage_status "${evidence}/lifecycle-report.json" upgrade)"
check "  and rollback never ran" absent "$(stage_status "${evidence}/lifecycle-report.json" rollback)"
check "  the agent config still holds its original bytes" yes "$(config_restored)"
check "  and no crash-loop restore ran over the purged install" no \
  "$(sudo_after "$(sudo_line 'apt-get purge')" "$restore_line")"

for case in \
  "rollback-state-aside-exists::step failed (exit 1): refuse to replace an existing /var/lib/tensorplate/state.bak" \
  "rollback-remove-leaves-backend::tensorplate-backend-python-pytorch is still installed" \
  "rollback-remove-purges::tensorplate-agent is absent, not config-files" \
  "rollback-leaves-candidate-package::tensorplate-cli is installed 0.2.1~rc.2-1, expected installed 0.2.1~rc.1-1" \
  "rollback-resets-conffile::the rollback did not keep the operator-edited" \
  "rollback-state-not-preserved::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-empties-backup::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-truncates-backup::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-rewrites-backup::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-empties-agent-bak::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-rewrites-agent-bak::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-deletes-agent-bak::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-empties-snapshot::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-deletes-snapshot::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-adds-state-file::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-adds-spaced-file::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-empties-state-dir::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-empties-spaced-file::step failed (exit 1): the set-aside state is preserved, file by file" \
  "rollback-state-file-missing::step failed (exit 1): digest the durable state before setting it aside" \
  "rollback-digest-not-hex::step failed (exit 1): digest the durable state before setting it aside" \
  "rollback-keeps-state::the rolled-back agent reports active" \
  "rollback-keeps-previous::the rolled-back agent reports previous_active" \
  "rollback-agent-unavailable::the agent is not available after the rollback" \
  "rollback-infer-garbled::worker round-trip checks failed" \
  "rollback-restore-garbles-record::the machine-type record /var/lib/tensorplate/state/machine-type.json changed in the restore: sha256 was" \
  "rollback-baseline-rewrites-record::the machine-type record /var/lib/tensorplate/state/machine-type.json changed when the baseline started: sha256 was" \
  "rollback-touches-binding::the rollback did not keep the instance binding /var/lib/tensorplate/identity/instance-binding.json: sha256 was" \
  "ok:cp -p /var/lib/tensorplate/state.bak/machine-type.json:step failed (exit 9): restore the machine-type record for the baseline" \
  "ok:systemctl stop:step failed (exit 9): stop the services" \
  "ok:sha256sum /var/lib/tensorplate/state/state.json:step failed (exit 9): digest the durable state before setting it aside" \
  "ok:mv -T:step failed (exit 9): set durable state aside" \
  "ok:apt-get remove:step failed (exit 9): remove tensorplate-"; do
  mode="${case%%:*}"
  rest="${case#*:}"
  sudo_fail="${rest%%:*}"
  message="${rest#*:}"
  expected_status=1
  if [[ -n "$sudo_fail" ]]; then expected_status=9; fi
  evidence="${td}/stages-rollback-${mode}-${sudo_fail//[!a-z0-9]/-}"
  check "${mode}${sudo_fail:+ with a failing ${sudo_fail}} fails the run" "$expected_status" \
    "$(run_upgrade_stages "$mode" "$evidence" "$sudo_fail" set-rc1)"
  check "  upgrade passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" upgrade)"
  check "  rollback is recorded as a failure" fail "$(stage_status "${evidence}/lifecycle-report.json" rollback)"
  check "  and the rollback log says why" yes "$(stage_log_says "${evidence}/rollback.log" "$message")"
  case "$mode" in
    rollback-state-aside-exists)
      check "  and the earlier set-aside state is untouched" yes \
        "$([[ "$(cat "${appliance}/varlib/state.bak/state.json")" == '{"fixture": "an earlier rollback"}' ]] && echo yes || echo no)"
      check "  because nothing was stopped, moved or removed" "no no no" \
        "$(after="$(last_sudo_line 'assets-rc2/install.sh')"
           printf '%s %s %s' "$(sudo_after "$after" 'systemctl stop')" \
             "$(sudo_after "$after" 'mv -T')" "$(sudo_after "$after" 'apt-get remove')")"
      ;;
    rollback-remove-leaves-backend)
      check "  and the baseline was never installed over the leftover" no \
        "$(sudo_after "$(sudo_line 'apt-get remove')" 'set-rc1/install.sh')"
      ;;
    rollback-state-not-preserved)
      # The set-aside directory is not there at all: the check names what
      # it could not read and stops there. Reporting it as CHANGED would
      # be a different claim -- that something was read and compared --
      # and would be made on an empty digest, so the read has to fail the
      # check rather than fall through to the comparison.
      check "  and names the directory it could not read" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           "ls: cannot access '/var/lib/tensorplate/state.bak': No such file or directory")"
      check "  and does not report it as changed" no \
        "$(stage_log_says "${evidence}/rollback.log" 'the rollback did not preserve')"
      # Nor as the empty directory the case below is about. A listing
      # whose failure went unchecked would read as a directory holding
      # nothing, which is a different fact with a different recovery:
      # there is a set-aside copy to inspect in one and none in the other.
      check "  nor as a directory that is there and holds nothing" no \
        "$(stage_log_says "${evidence}/rollback.log" \
           'holds no files; there is no durable state here to preserve')"
      ;;
    rollback-empties-backup|rollback-truncates-backup|rollback-rewrites-backup|\
    rollback-empties-agent-bak|rollback-rewrites-agent-bak|rollback-empties-snapshot)
      # Which file each mode destroyed. The pathname is still a regular
      # file in every one of them, so `test -f` on it would have passed
      # them all -- including the two that leave the agent's own
      # state.json untouched and destroy the copy it recovers FROM.
      case "$mode" in
        rollback-empties-snapshot) destroyed=observability-snapshot.json ;;
        rollback-empties-agent-bak|rollback-rewrites-agent-bak) destroyed=state.json.bak ;;
        *) destroyed=state.json ;;
      esac
      check "  and ${destroyed} is still a regular file" yes \
        "$([[ -f "${appliance}/varlib/state.bak/${destroyed}" ]] && echo yes || echo no)"
      check "  and the failure names the file whose contents did not survive" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           "the rollback did not preserve /var/lib/tensorplate/state.bak/${destroyed}: sha256 was")"
      # The checks after it intentionally do not load the set-aside state,
      # so they cannot stand in for this one: the stage has to stop here.
      check "  and the stage stopped before reading the agent back" no \
        "$(stage_log_says "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-deletes-agent-bak|rollback-deletes-snapshot)
      # Gone rather than damaged. A check that digested only state.json
      # could not notice either of these.
      if [[ "$mode" == rollback-deletes-agent-bak ]]; then
        gone=state.json.bak
      else
        gone=observability-snapshot.json
      fi
      check "  and the failure names the file the set-aside copy no longer holds" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           "the rollback did not preserve /var/lib/tensorplate/state.bak/${gone}: it was in the durable state when the services were stopped and the set-aside copy does not hold it")"
      check "  and the stage stopped before reading the agent back" no \
        "$(stage_log_says "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-adds-state-file)
      # Every file that was set aside is still there, byte for byte, and
      # the install put one more beside them. "Unchanged" has to mean the
      # directory, or an install that seeded the older agent with state
      # of its own would read as preservation.
      check "  and every file that was set aside is intact" "yes yes yes" \
        "$(cd "${appliance}/varlib/state.bak" &&
           printf '%s %s %s' \
             "$([[ "$(cat state.json)" == "$aside_state_fixture" ]] && echo yes || echo no)" \
             "$([[ "$(cat state.json.bak)" == "$aside_agent_bak_fixture" ]] && echo yes || echo no)" \
             "$([[ "$(cat observability-snapshot.json)" == "$aside_snapshot_fixture" ]] && echo yes || echo no)")"
      check "  and the failure names the file that was added" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           'the rollback did not preserve /var/lib/tensorplate/state.bak: it holds state.json.new, which the durable state did not when the services were stopped')"
      check "  and the stage stopped before reading the agent back" no \
        "$(stage_log_says "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-adds-spaced-file)
      # The added-file half of the comparison, held to the same rule as
      # the changed-file half is by rollback-empties-spaced-file: the name
      # is everything before a manifest line's LAST space. It is a
      # separate loop over a separate manifest, and the two cases are
      # what keep each of them honest -- neither reaches the other's.
      check "  and the failure names the whole added name, spaces and all" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           'the rollback did not preserve /var/lib/tensorplate/state.bak: it holds extra file.json, which the durable state did not when the services were stopped')"
      check "  and not the first word of it" no \
        "$(stage_log_says "${evidence}/rollback.log" \
           'the rollback did not preserve /var/lib/tensorplate/state.bak: it holds extra,')"
      check "  and the stage stopped before reading the agent back" no \
        "$(stage_log_says "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-empties-state-dir)
      # A directory that is there and holds nothing is not a preserved
      # one. Saying so about the directory is the honest report: naming
      # whichever file the comparison happened to reach first would
      # describe one loss out of three.
      check "  and the set-aside directory is still there" yes \
        "$([[ -d "${appliance}/varlib/state.bak" ]] && echo yes || echo no)"
      check "  and the failure names the directory, not one file in it" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           '/var/lib/tensorplate/state.bak holds no files; there is no durable state here to preserve')"
      # The other half of the pair above: a directory that is there and
      # holds nothing is not a directory that could not be read.
      check "  rather than as a directory it could not read" no \
        "$(stage_log_says "${evidence}/rollback.log" 'ls: cannot access')"
      # Nor as one file out of three. This is the half a lost refusal
      # breaks: admit the empty directory as a manifest and the
      # comparison below it runs, reaching the first C-sorted name the
      # copy no longer holds and reporting THAT as what the rollback did
      # not preserve. The stage would still fail, which is why the
      # positive check above moves on its own -- but the operator would
      # be sent to recover one file when all of them are gone, and those
      # are different recoveries. The claim is about the directory, so
      # no individual file may be named here at all.
      check "  and names no individual file as the thing that was lost" no \
        "$(stage_log_says "${evidence}/rollback.log" 'the rollback did not preserve /var/lib/tensorplate/state.bak/')"
      check "  and the stage stopped before reading the agent back" no \
        "$(stage_log_says "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-empties-spaced-file)
      # A manifest line is "<name> <sha256>", and the name is everything
      # before the LAST space -- privileged_sha256 refuses any digest
      # that is not 64 hex characters, so the final field is the digest
      # and nothing else can be. Split on the FIRST space instead and a
      # name holding one reads as a shorter name whose digest begins with
      # the rest of the name, which names a file that does not exist and
      # prints two things that are not digests. The verdict is right
      # either way; the diagnosis the operator acts on is not.
      check "  and the failure names the whole name, spaces and all" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           "the rollback did not preserve /var/lib/tensorplate/state.bak/two words.json: sha256 was")"
      check "  and not the first word of it" no \
        "$(stage_log_says "${evidence}/rollback.log" \
           'the rollback did not preserve /var/lib/tensorplate/state.bak/two:')"
      # Both digests in the message are digests. A first-space split puts
      # the rest of the name in front of one and leaves the other as a
      # bare fragment of it.
      check "  and reports two sha256 digests, not fragments of the name" yes \
        "$(grep -Eq "sha256 was [0-9a-f]{64} when the services were stopped, now [0-9a-f]{64}$" \
           "${evidence}/rollback.log" && echo yes || echo no)"
      check "  and the stage stopped before reading the agent back" no \
        "$(stage_log_says "${evidence}/rollback.log" 'the rolled-back agent answers')"
      ;;
    rollback-state-file-missing)
      # Refused where the manifest is taken, before the move: a state
      # directory without the agent's deployment state gives a manifest
      # that both sides can satisfy while preserving nothing this stage
      # is about.
      check "  and says which file the durable state is missing" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           '/var/lib/tensorplate/state holds no state.json: there is no deployment state for the rollback to preserve')"
      check "  with nothing set aside or removed" "no no" \
        "$(after="$(last_sudo_line 'assets-rc2/install.sh')"
           printf '%s %s' "$(sudo_after "$after" 'mv -T')" "$(sudo_after "$after" 'apt-get remove')")"
      ;;
    rollback-digest-not-hex)
      # A leading field that is not sha256 hex is refused where it is
      # read, not carried forward: two files digested through such a
      # sha256sum would otherwise compare equal and the stage would
      # credit the rollback with preserving state it never read. The file
      # named is the first entry of the state directory in the order the
      # manifest reads it, C-sorted, not whichever name the harness
      # happens to care about: the machine-type record, which sorts first.
      check "  and names the file it could not digest" yes \
        "$(stage_log_says "${evidence}/rollback.log" \
           'could not compute a sha256 of /var/lib/tensorplate/state/machine-type.json')"
      check "  with nothing set aside or removed" "no no" \
        "$(after="$(last_sudo_line 'assets-rc2/install.sh')"
           printf '%s %s' "$(sudo_after "$after" 'mv -T')" "$(sudo_after "$after" 'apt-get remove')")"
      ;;
  esac
done

check "no destructive command reached the host" "yes" \
  "$([[ -f "${appliance}/sudo.log" ]] && echo yes || echo no)"

# --- across every stubbed run.
#
# A failed run's evidence is filed too, and its report copies the tail of
# the failing stage's log. Every run above that wrote a report, passing
# or not, must have written evidence the scanner admits, and none may
# have left a raw capture behind.
check "no stubbed run left a raw capture in the harness's scratch space" 0 \
  "$(scratch_holding_host_metadata)"
# Every run's evidence directory is named stages-*. A pattern that
# matched nothing would reach the scanner as a literal path, which it
# refuses without a verdict.
run_evidence=()
for run_report in "${td}"/stages-*/lifecycle-report.json; do
  run_evidence+=("$(dirname "$run_report")")
done
check_publishable "every stubbed run's evidence passes the publication scanner" \
  "${td}/all-runs-publication-scan.out" "${run_evidence[@]}"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "verify_ubuntu_l4_cloud_lifecycle: ok" || echo "${failures} check(s) failed")"
exit "$failures"
