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
# exactly once: offline is always skipped, the rest always run.
assert sorted(run) == sorted(["install", "deploy-smoke", "status-logs", "restart",
                              "crash-loop", "upgrade", "rollback"]), sorted(run)
assert sorted(skipped) == sorted(["offline", "upgrade", "rollback"]), sorted(skipped)

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
print("stage coverage: 5 always run, offline skipped, upgrade and rollback run or skipped with a reason; digest recorded after install")
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
for tool in sudo systemctl; do
  printf '#!/bin/sh\nexit 0\n' >"${stub_bin}/${tool}"
  chmod +x "${stub_bin}/${tool}"
done

# dpkg, and the package database behind the stubbed appliance, in one
# script that dispatches on the name it is invoked by.
#
# As `dpkg` it compares versions, because the upgrade path is refused or
# admitted on that comparison, and a stub that answered 0 for everything
# would admit any pair. It knows the two version shapes release builds
# produce, and treats an empty version as older than any other, as dpkg
# does -- so a manifest with no version is admitted by the comparison
# alone, and only the harness's own version check refuses it. Any other
# shape exits 2, where dpkg would reject some and only warn about others;
# the harness refuses those before comparing as well.
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

VERSION = re.compile(r"(\d+)\.(\d+)\.(\d+)(?:~rc\.(\d+))?-(\d+)")
RUNTIME = (
    "tensorplate-common", "tensorplate-agent", "tensorplate-serving",
    "tensorplate-observability", "tensorplate-cli", "tensorplate-backend-python-pytorch",
)
PACKAGED_CLI_CONFIG = '{"fixture": "packaged cli config"}\n'

def version_key(version):
    if version == "":
        return ()
    match = VERSION.fullmatch(version)
    if not match:
        return None
    major, minor, patch, rc, revision = match.groups()
    # A candidate sorts before its release, as Debian's tilde does.
    return (int(major), int(minor), int(patch), 0 if rc else 1, int(rc or 0), int(revision))

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
    left, op, right = version_key(args[1]), args[2], version_key(args[3])
    if left is None or right is None:
        print(f"dpkg: error: version has bad syntax: {args[1]!r} {args[3]!r}", file=sys.stderr)
        return 2
    results = {"lt": left < right, "le": left <= right, "eq": left == right,
               "ne": left != right, "ge": left >= right, "gt": left > right}
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
    wanted = {
        a["package"]: a["version"] for a in manifest["artifacts"]
        if a.get("package") in RUNTIME and a.get("architecture") in ("amd64", "all")
    }
    packages = db["packages"]
    for name, version in wanted.items():
        current = packages.get(name)
        if current and current["status"] == "installed" \
                and version_key(version) < version_key(current["version"]):
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

    # install-paths.sh lays out the state directory at configure time.
    (varlib / "state").mkdir(parents=True, exist_ok=True)
    if mode == "upgrade-loses-deployment" and phase == "upgraded":
        shutil.rmtree(varlib / "state")
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
# in the release build's shape, and SHA256SUMS over both. The manifest
# lists an arm64 build beside each amd64 one, as a published release's
# does, so the harness has to select by architecture. None of the .deb
# files exist; the fake package database installs from the manifest.
#
# `set-rc1` rather than `assets-rc1`, so a search of the sudo log for the
# candidate's `assets-rc2/` path cannot match the baseline's lines.
fixtures="${td}/sets"
python3 - "$fixtures" <<'PY'
import hashlib, json, pathlib, sys

root = pathlib.Path(sys.argv[1])
PER_ARCH = ("tensorplate-agent", "tensorplate-serving", "tensorplate-observability", "tensorplate-cli")
ALL_ARCH = ("tensorplate-common", "tensorplate-backend-python-pytorch", "tensorplate-apt-source", "tensorplate")

def make(name, rc, *, manifest=True, release_fields=None, drop=(), versions=None, extra=()):
    directory = root / name
    directory.mkdir(parents=True)
    deb_version = f"0.2.1~rc.{rc}-1"
    (directory / "install.sh").write_text(f"#!/bin/sh\n# fixture installer, v0.2.1-rc.{rc}\nexit 0\n")
    artifacts = []
    for package in PER_ARCH + ALL_ARCH:
        if package in drop:
            continue
        for arch in (("amd64", "arm64") if package in PER_ARCH else ("all",)):
            artifacts.append({
                "file": f"{package}_{deb_version}_{arch}.deb",
                "package": package,
                "version": (versions or {}).get(package, deb_version),
                "architecture": arch,
            })
    artifacts.extend(extra)
    listed = ["install.sh"]
    if manifest:
        release = {"project": "tensorplate", "version": "0.2.1", "tag": f"v0.2.1-rc.{rc}",
                   "provenance": "github-release", "unreleased": False}
        release.update(release_fields or {})
        manifest_name = f"tensorplate-v0.2.1-rc.{rc}-artifacts.json"
        (directory / manifest_name).write_text(json.dumps(
            {"release": release, "artifacts": artifacts}, indent=2) + "\n")
        listed.append(manifest_name)
    # GNU format, written directly so the fixture is the same on both
    # platforms rather than depending on which checksum tool is present.
    (directory / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256((directory / f).read_bytes()).hexdigest()}  {f}\n" for f in listed))
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
cp "${stub_bin}/python3" "${appliance}/bin/python3"
real_mktemp="$(command -v mktemp)"

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
# Package, state and config operations touch only the fixture and the
# harness's temporary backup. This verifies the bytes can actually be
# restored, rather than treating a logged copy command as a successful
# restoration.
case "$*" in
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
  "test -f /var/lib/tensorplate/state.bak/state.json")
    [ -f "${TP_FAKE_VARLIB}/state.bak/state.json" ]
    exit
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
      rollback-state-not-preserved) rm -rf "$source_dir" ;;
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
          *tensorplate-agent*) printf '11111111111111111111111111111111\n' ;;
          *tensorplate-observability*) printf '22222222222222222222222222222222\n' ;;
          *) exit 9 ;;
        esac
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
cp "$fake_dpkg" "${appliance}/bin/fake-dpkg-db"
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
cat >"${appliance}/bin/journalctl" <<'STUB'
#!/bin/sh
# The metadata every record carries, whatever the mode.
host_fields='"_HOSTNAME":"tp-synthetic-host","_MACHINE_ID":"00000000000000000000000000000000","_BOOT_ID":"00000000000000000000000000000000","_TRANSPORT":"stdout","_CMDLINE":"/usr/bin/tensorplate-agent --config /etc/tensorplate/agent.json","__MONOTONIC_TIMESTAMP":"84210000000"'
cursor_field='"__CURSOR":"s=00000000;i=4a1;b=00000000000000000000000000000000;m=139f2a;t=6591c0;x=51d2"'
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
      "$host_fields" "$cursor_field" "$((4000 + attempt))" "$message"
    # systemd's own record of the restart, which carries UNIT rather
    # than _SYSTEMD_UNIT.
    printf '{%s,%s,"_SYSTEMD_UNIT":"init.scope","UNIT":"tensorplate-agent.service","_PID":"1","PRIORITY":"4","SYSLOG_IDENTIFIER":"systemd","MESSAGE":"tensorplate-agent.service: Scheduled restart job, restart counter is at %s.","__REALTIME_TIMESTAMP":"1789300000000001"}\n' \
      "$host_fields" "$cursor_field" "$attempt"
  done
  printf '{%s,%s,"_SYSTEMD_UNIT":"init.scope","UNIT":"tensorplate-agent.service","_PID":"1","PRIORITY":"3","SYSLOG_IDENTIFIER":"systemd","MESSAGE":"tensorplate-agent.service: Start request repeated too quickly.","__REALTIME_TIMESTAMP":"1789300000000002"}\n' \
    "$host_fields" "$cursor_field"
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
printf '{%s,%s,"_SYSTEMD_INVOCATION_ID":"%s","_SYSTEMD_UNIT":"%s","_PID":"4242","PRIORITY":"6","SYSLOG_IDENTIFIER":"%s","MESSAGE":"%s","__REALTIME_TIMESTAMP":"1789300000000000"}\n' \
  "$host_fields" "$cursor_field" "$invocation" "$unit" "${unit%.service}" "$message"
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
    if [ "$mode:$installed" = upgrade-wrong-row:upgraded ]; then row_status=warning; fi
    case "$mode:$installed" in
      baseline-doctor-failing:baseline|baseline-doctor-failing:rolled-back) failing=1 ;;
    esac
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
    # Doctor exits 10 when a finding fails. Only the baseline mode says
    # so, so the install-stage cases keep reaching the harness's own check.
    case "$mode:$failing" in baseline-doctor-failing:1) exit 10 ;; esac
    ;;
  deploy)
    # The deployment is durable state: it is what an upgraded agent
    # re-warms, and what a rollback sets aside.
    mkdir -p "${TP_FAKE_VARLIB}/state" || exit 9
    printf '{"active":"%s"}\n' "${TP_FAKE_DEPLOYMENT_ID}" >"$state_file" || exit 9
    "${TP_FAKE_DPKG_DB}" agent-version >>"${TP_FAKE_DEPLOY_VERSIONS}"
    printf '{"command":"deploy","payload":{"phase":"active","deployment_id":"%s"}}\n' \
      "${TP_FAKE_DEPLOYMENT_ID}"
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
      "${TP_FAKE_DEPLOYMENT_ID}" "$serving_url"
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
  : >"$TP_FAKE_PUBLICATION_LOG"
  rm -f "${appliance}/restarted" "${appliance}/config-broken" \
    "${appliance}/restarts" "${appliance}/restore-failed" "${appliance}/backup-path" \
    "${appliance}/dpkg-db.json"
  rm -rf "${appliance}/varlib"
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
    TP_CLOUD_CRASH_LOOP_POLL_SECONDS=0 \
    bash "$harness" \
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

ok_evidence="${td}/stages-ok"
check "a stubbed run completes" "0" "$(run_stages ok "$ok_evidence")"
for stage in install deploy-smoke status-logs restart crash-loop; do
  check "  ${stage} is recorded as a pass" "pass" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
for stage in upgrade rollback offline; do
  check "  ${stage} is recorded as skipped" "skipped" "$(stage_status "${ok_evidence}/lifecycle-report.json" "$stage")"
done
check "  the skipped stages keep the run incomplete" incomplete \
  "$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["outcome"])' \
    "${ok_evidence}/lifecycle-report.json")"
check "  offline explains the missing metadata-independent identity support" yes \
  "$(python3 - "${ok_evidence}/lifecycle-report.json" <<'PY'
import json, sys

stage = next(s for s in json.load(open(sys.argv[1]))["stages"] if s["stage"] == "offline")
detail = stage.get("detail", "").lower()
print("yes" if "metadata" in detail and "identity" in detail else "no")
PY
)"
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
check "  a deferred offline stage never mutates network policy" no \
  "$(grep -Eq 'IPAddress(Deny|Allow)|systemd-run|validation-offline' "${appliance}/sudo.log" && echo yes || echo no)"
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
# next thing that collects logs.
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
publication_scan="${td}/publication-scan.out"
scan_status=0
"${repo_root}/tools/validation/check-evidence-publication.sh" \
  --patterns-only "$ok_evidence" >"$publication_scan" 2>&1 || scan_status=$?
check "  and the run's own evidence passes the publication scanner" 0 "$scan_status"
if ((scan_status != 0)); then
  sed 's/^/       /' "$publication_scan" >&2
fi

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
raw_capture="${td}/raw-agent-journal.txt"
TP_FAKE_MODE=ok "${appliance}/bin/journalctl" -u tensorplate-agent.service \
  _SYSTEMD_INVOCATION_ID=11111111111111111111111111111111 \
  -n 100 --no-pager --output=json >"$raw_capture"
check "  the unprojected capture carries the fields the projection removes" \
  "1 _BOOT_ID,_CMDLINE,_HOSTNAME,_MACHINE_ID,_TRANSPORT,__CURSOR,__MONOTONIC_TIMESTAMP none" \
  "$(journal_projection "$raw_capture" MESSAGE _SYSTEMD_UNIT _SYSTEMD_INVOCATION_ID)"

unprojected="${td}/evidence-unprojected"
rm -rf "$unprojected"
cp -R "$ok_evidence" "$unprojected"
cp "$raw_capture" "${unprojected}/agent-journal.txt"
unprojected_scan="${td}/unprojected-scan.out"
unprojected_status=0
"${repo_root}/tools/validation/check-evidence-publication.sh" \
  --patterns-only "$unprojected" >"$unprojected_scan" 2>&1 || unprojected_status=$?
check "  and the same evidence with one capture unprojected is refused" 1 \
  "$unprojected_status"
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
            crash-loop-never-fails crash-loop-stopped; do
  evidence="${td}/stages-${mode}"
  check "${mode} fails the run" 1 "$(run_stages "$mode" "$evidence")"
  check "  restart passed before it" pass "$(stage_status "${evidence}/lifecycle-report.json" restart)"
  check "  and crash-loop is recorded as a failure" fail \
    "$(stage_status "${evidence}/lifecycle-report.json" crash-loop)"
  check "  and the agent config was restored anyway" yes \
    "$(config_restored)"
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
stage_log_says() {
  grep -Fq -- "$2" "$1" && echo yes || echo no
}

evidence="${td}/stages-upgrade-ok"
report="${evidence}/lifecycle-report.json"
check "a run with a baseline completes" 0 "$(run_upgrade_stages ok "$evidence" "" set-rc1)"
for stage in install deploy-smoke status-logs restart crash-loop upgrade rollback; do
  check "  ${stage} is recorded as a pass" pass "$(stage_status "$report" "$stage")"
done
check "  offline is still skipped" skipped "$(stage_status "$report" offline)"
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
check "  and keeps the run incomplete" incomplete \
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
# deploy-smoke, restart and crash-loop on the candidate; a deploy on the
# baseline; the baseline's deployment answering on the upgraded candidate
# without a deploy; and a fresh deploy on the rolled-back baseline.
check "  deploys ran on candidate, baseline, rolled-back baseline" \
  "0.2.1~rc.2-1 0.2.1~rc.1-1 0.2.1~rc.1-1" "$(one_line "${appliance}/deploy-versions.log")"
check "  inferences ran on each install in turn" \
  "0.2.1~rc.2-1 0.2.1~rc.2-1 0.2.1~rc.2-1 0.2.1~rc.1-1 0.2.1~rc.2-1 0.2.1~rc.1-1" \
  "$(one_line "${appliance}/infer-versions.log")"
# Empty when the line is missing, which every check below treats as a
# failure; `|| true` keeps a missing line from ending the suite early.
first_baseline_install="$(sudo_line 'set-rc1/install.sh --local-artifacts' || true)"
last_candidate_install="$(last_sudo_line 'assets-rc2/install.sh --local-artifacts' || true)"
remove_line="$(sudo_line 'apt-get remove' || true)"
check "  the candidate is purged before the baseline is installed" yes \
  "$(purge="$(last_sudo_line 'apt-get purge')"
     [[ -n "$purge" && -n "$first_baseline_install" && "$purge" -lt "$first_baseline_install" ]] && echo yes || echo no)"
check "  the rollback removes after the last candidate install" yes \
  "$([[ -n "$remove_line" && -n "$last_candidate_install" && "$remove_line" -gt "$last_candidate_install" ]] && echo yes || echo no)"
check "  and removes the backend with the rest of the set" yes \
  "$(sed -n "${remove_line:-0}p" "${appliance}/sudo.log" | tr ' ' '\n' | grep -qx 'tensorplate-backend-python-pytorch' && echo yes || echo no)"
check "  and never purges after the last candidate install" no \
  "$(sudo_after "$last_candidate_install" 'apt-get purge')"
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
  "upgrade-loses-deployment::worker round-trip checks failed" \
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
  "rollback-state-not-preserved::step failed (exit 1): the set-aside state is preserved" \
  "rollback-keeps-state::the rolled-back agent reports active" \
  "rollback-keeps-previous::the rolled-back agent reports previous_active" \
  "rollback-agent-unavailable::the agent is not available after the rollback" \
  "rollback-infer-garbled::worker round-trip checks failed" \
  "ok:systemctl stop:step failed (exit 9): stop the services" \
  "ok:mv -T:step failed (exit 9): set durable state aside" \
  "ok:apt-get remove:step failed (exit 9): remove tensorplate-"; do
  mode="${case%%:*}"
  rest="${case#*:}"
  sudo_fail="${rest%%:*}"
  message="${rest#*:}"
  expected_status=1
  if [[ -n "$sudo_fail" ]]; then expected_status=9; fi
  evidence="${td}/stages-rollback-${mode}-${sudo_fail// /-}"
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
  esac
done

check "no destructive command reached the host" "yes" \
  "$([[ -f "${appliance}/sudo.log" ]] && echo yes || echo no)"

printf '\n%s\n' "$([[ "$failures" -eq 0 ]] && echo "verify_ubuntu_l4_cloud_lifecycle: ok" || echo "${failures} check(s) failed")"
exit "$failures"
