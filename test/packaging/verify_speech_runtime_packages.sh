#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# packaging: build the speech runtime family from stub wheels through the
# real builder and debian/rules, install it with dpkg, and assert the result.
# The stub lock keeps three real pins: setuptools and the two wheels the
# builder builds itself, so their pinned digests are reproduced on every run.
#
# Not part of run.sh: it writes .deb files to the repository parent, installs
# and purges packages, and fetches two files from PyPI.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
cd "$repo_root"

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
note() { printf '==> %s\n' "$*"; }
pass() { printf 'PASS: %s\n' "$*"; }

if [[ "${CI:-}" != "true" && "${TP_ALLOW_LOCAL_SPEECH_RUNTIME_PACKAGE_TEST:-}" != "1" ]]; then
  echo "verify_speech_runtime_packages: skipped (set CI=true or TP_ALLOW_LOCAL_SPEECH_RUNTIME_PACKAGE_TEST=1)"
  exit 0
fi

for tool in dpkg-buildpackage dpkg-deb dpkg-parsechangelog python3.12; do
  command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done
[[ "$(dpkg --print-architecture)" == amd64 ]] || die "the family is Architecture: amd64; run on an amd64 host"

family=tensorplate-speech-runtime
env_root=/usr/lib/tensorplate/speech-runtime
real_lock=packaging/speech-runtime/lock
builder=packaging/speech-runtime/build-environment.py
version="$(dpkg-parsechangelog -l packaging/debian/changelog -S Version)"
repo_parent="$(dirname "$repo_root")"

components=()
for path in "$real_lock"/*.txt; do
  name="$(basename "$path" .txt)"
  [[ "$name" == index || "$name" == sources || "$name" == undeclared-licenses ]] || components+=("$name")
done

td="$(mktemp -d)"
before="$(ls -1 "$repo_parent")"
as_root=()
[[ "$(id -u)" == 0 ]] || as_root=(sudo)
installed=0
cleanup() {
  local f
  if ((installed)); then
    "${as_root[@]}" dpkg --purge "${expected[@]}" tensorplate-serving tensorplate-backend-python-pytorch >/dev/null 2>&1 || true
  fi
  # Only what this run added to the repository parent.
  while IFS= read -r f; do
    [[ -n "$f" ]] && rm -f -- "${repo_parent}/${f}"
  done < <(comm -13 <(printf '%s\n' "$before") <(ls -1 "$repo_parent"))
  "${as_root[@]}" rm -rf -- "$td"
}
trap cleanup EXIT

# --- the stub lock and wheelhouse -----------------------------------------

make_stubs() {
  python3.12 test/packaging/speech_runtime_stub_wheelhouse.py "$td" "$repo_root/$real_lock"
}

note "writing the stub lock and wheelhouse"
make_stubs
python3.12 "$builder" fetch --lock-dir "$td/lock" --wheelhouse "$td/wheelhouse" \
  --work-dir "$td/fetch-work" >"$td/fetch.log" 2>&1 ||
  { tail -20 "$td/fetch.log" >&2; die "fetch of the setuptools wheel and the docopt archive failed"; }
python3.12 "$builder" fetch --lock-dir "$td/lock" --wheelhouse "$td/wheelhouse" \
  --work-dir "$td/fetch-work" >"$td/refetch.log" 2>&1 || die "a second fetch failed"
grep -q 'already holds every index pin' "$td/refetch.log" ||
  die "a second fetch must find the wheelhouse complete and download nothing"
pass "fetch is idempotent"

# --- what the builder refuses ----------------------------------------------

# Each case copies the stub lock, breaks one thing, and must fail for that reason.
refuses() {
  local what="$1" expected="$2" edit="$3" case_dir
  cases=$((cases + 1))
  case_dir="$td/case-${cases}"
  mkdir -p "$case_dir"
  cp -R "$td/lock" "$case_dir/lock"
  cp -R "$td/wheelhouse" "$case_dir/wheelhouse"
  (cd "$case_dir" && "$edit")
  if python3.12 "$builder" build --lock-dir "$case_dir/lock" --wheelhouse "$case_dir/wheelhouse" \
      --work-dir "$case_dir/work" --destdir "$case_dir/out" --source-root "$repo_root" \
      >"$case_dir/log" 2>&1; then
    die "the builder accepted: ${what}"
  fi
  grep -qF -- "$expected" "$case_dir/log" ||
    { tail -5 "$case_dir/log" >&2; die "${what}: refused, but not for \"${expected}\""; }
  [[ ! -e "$case_dir/work" ]] || die "${what}: the work directory was left behind"
  pass "refused: ${what}"
}
cases=0

changed_wheel() { printf x >>wheelhouse/tpstub_vad-1.0-py3-none-any.whl; }
changed_archive() { printf x >>wheelhouse/docopt-0.6.2.tar.gz; }
changed_loader_tree() {
  cp -R "$repo_root/packaging/speech-runtime/espeakng-loader" tree
  echo "# changed" >>tree/espeakng_loader/__init__.py
  sed -i "s|^\(espeakng-loader .*\) [^ ]*$|\1 ${PWD}/tree|" lock/sources.txt
}
two_hashes() { sed -i "1s|\$| --hash=sha256:$(printf '%064d' 0)|" lock/vad.txt; }
pinned_twice() { head -1 lock/vad.txt >>lock/ct2.txt; }
no_setuptools() { sed -i '/^setuptools==/d' lock/base.txt; }
failing_self_test() { echo 'raise SystemExit(1)' >lock/self-test.py; }
unexcepted_license() { : >lock/undeclared-licenses.txt; }
stale_exception() { echo "tpstub-vad stub reason" >>lock/undeclared-licenses.txt; }
unpinned_exception() { echo "tpstub-absent stub reason" >>lock/undeclared-licenses.txt; }

note "builder refusals"
refuses "a wheel whose bytes differ from its pin" "DO NOT MATCH THE HASHES" changed_wheel
refuses "a source archive that is not the locked one" "not the locked archive" changed_archive
refuses "a loader tree that no longer builds the pinned wheel" "not the locked digest" changed_loader_tree
refuses "a pin with two hashes" "not \`name==version --hash=sha256:<digest>\`" two_hashes
refuses "a distribution pinned in two components" "is pinned twice" pinned_twice
refuses "a lock without setuptools" "pins no setuptools" no_setuptools
refuses "a failing self-test" "self-test.py exited 1" failing_self_test
refuses "a distribution with no declared license" "tpstub-cuda declares no license" unexcepted_license
refuses "a license exception for a distribution that declares one" "tpstub-vad declares a license" stale_exception
refuses "a license exception for a distribution that is not pinned" "expected \`pinned-name reason\`" unpinned_exception

# pip writes a /bin/sh wrapper instead of a shebang once the interpreter
# path passes 127 bytes, as it does under a hosted runner's checkout.
note "a build directory too long for a shebang"
long="$td/$(printf 'l%.0s' {1..140})"
mkdir -p "$long"
python3.12 "$builder" build --lock-dir "$td/lock" --wheelhouse "$td/wheelhouse" \
  --work-dir "$long/work" --destdir "$td/long-out" --source-root "$repo_root" >"$td/long.log" 2>&1 ||
  { tail -5 "$td/long.log" >&2; die "the builder failed in a long build directory"; }
[[ "$(head -1 "$td/long-out/${family}-ct2${env_root}/bin/tpstub-tool")" == "#!${env_root}/bin/python" ]] ||
  die "a console script written in the wrapper form must start the environment's own interpreter"
pass "console scripts are rewritten in both forms"

# --- the package build ------------------------------------------------------

note "building the family under the speech-runtime profile"
build_log="$td/build.log"
if ! DEB_BUILD_PROFILES=pkg.tensorplate.speech-runtime \
     TP_SPEECH_RUNTIME_LOCK_DIR="$td/lock" TP_SPEECH_RUNTIME_WHEELHOUSE="$td/wheelhouse" \
     packaging/scripts/build-deb.sh -B >"$build_log" 2>&1; then
  tail -40 "$build_log" >&2
  die "dpkg-buildpackage -B failed under the speech-runtime profile"
fi

built=()
while IFS= read -r name; do
  [[ "$name" == *.deb ]] && built+=("${name%%_*}")
done < <(comm -13 <(printf '%s\n' "$before") <(ls -1 "$repo_parent"))

expected=("$family")
for component in "${components[@]}"; do expected+=("${family}-${component}"); done
if [[ "$(printf '%s\n' "${expected[@]}" | sort)" != "$(printf '%s\n' "${built[@]:-}" | sort)" ]]; then
  printf 'expected:\n%s\nbuilt:\n%s\n' "$(printf '%s\n' "${expected[@]}" | sort)" "$(printf '%s\n' "${built[@]:-}" | sort)" >&2
  die "the profile must build exactly the family: one package per lock file plus the metapackage"
fi
pass "built set is exactly the ${#expected[@]} family packages"

deb_of() { printf '%s/%s_%s_amd64.deb' "$repo_parent" "$1" "$version"; }
field() { dpkg-deb -f "$(deb_of "$1")" "$2"; }
depends_on() {
  field "$1" Depends | tr ',' '\n' | sed 's/^ *//' | grep -qxF -- "$2" ||
    die "$1 must depend on: $2 (has: $(field "$1" Depends))"
}

for package in "${expected[@]}"; do
  [[ "$(field "$package" Architecture)" == amd64 ]] || die "${package} must be Architecture: amd64"
  depends_on "$package" "tensorplate-serving (= ${version})"
done
depends_on "${family}-base" "python3.12 (<< 3.13)"
depends_on "${family}-kokoro" libespeak-ng1
depends_on "${family}-kokoro" espeak-ng-data
for component in "${components[@]}"; do
  depends_on "$family" "${family}-${component} (= ${version})"
  [[ "$component" == base ]] || depends_on "${family}-${component}" "${family}-base (= ${version})"
done
pass "control fields bind the family to one serving version"

payload="$(dpkg-deb -c "$(deb_of "$family")" | awk '$6 !~ /\/$/ {print $6}' | { grep -v '^\./usr/share/doc/' || true; })"
[[ -z "$payload" ]] || die "the metapackage must ship no payload; found: ${payload}"
pass "metapackage ships no payload"

# Triggers: one restart of the agent per dpkg run, handled by the base package.
control_member() { dpkg-deb --ctrl-tarfile "$(deb_of "$1")" | tar -xO "./$2" 2>/dev/null || true; }
[[ "$(control_member "${family}-base" triggers)" == "interest-noawait ${family}" ]] ||
  die "the base package must declare interest in the family trigger"
control_member "${family}-base" postinst | grep -q 'try-restart tensorplate-agent.service' ||
  die "the base package's postinst must restart a running agent when triggered"
for component in "${components[@]}"; do
  [[ "$component" == base ]] && continue
  [[ "$(control_member "${family}-${component}" triggers)" == "activate-noawait ${family}" ]] ||
    die "${family}-${component} must activate the family trigger"
  [[ -z "$(control_member "${family}-${component}" postinst)" ]] ||
    die "${family}-${component} must not carry its own postinst"
done
pass "trigger wiring"

# Every file in exactly one package, and each stub in its own.
listing="$td/listing"
for component in "${components[@]}"; do
  dpkg-deb -c "$(deb_of "${family}-${component}")" |
    awk -v c="$component" '$1 !~ /^d/ {print c, $6}' >>"$listing"
done
dupes="$(awk '{print $2}' "$listing" | sort | uniq -d)"
[[ -z "$dupes" ]] || die "files shipped by more than one package: ${dupes}"
site=".${env_root}/lib/python3.12/site-packages"
for component in "${components[@]}"; do
  for file in "tpstub_${component}/__init__.py" "tpstub_${component}/data.bin" \
              "tpstub_${component}/__pycache__/__init__.cpython-312.pyc" \
              "tpstub_${component}-1.0.dist-info/RECORD"; do
    grep -qxF "${component} ${site}/${file}" "$listing" ||
      die "${family}-${component} must ship ${site}/${file}"
  done
  grep -qxF "${component} ./usr/share/doc/${family}-${component}/third-party-licenses.json" "$listing" ||
    die "${family}-${component} must ship its license manifest uncompressed"
done
dpkg-deb -c "$(deb_of "${family}-base")" |
  grep -qE " \.${env_root}/bin/python3\.12 -> \.\./\.\./\.\./\.\./bin/python3\.12\$" ||
  die "the environment's interpreter must be a link to /usr/bin/python3.12"
for file in "${env_root}/pyvenv.cfg" "${env_root}/bin/python" \
            "${env_root}/lib/python3.12/site-packages/tensorplate_pytorch_backend/runner.py" \
            "${env_root}/lib/python3.12/site-packages/setuptools/__init__.py"; do
  grep -qxF "base .${file}" "$listing" || die "the base package must ship ${file}"
done
for file in docopt.py espeakng_loader/__init__.py; do
  grep -qxF "kokoro ${site}/${file}" "$listing" || die "the kokoro package must ship ${site}/${file}"
done
if grep -qE "site-packages/pip(/|-)" "$listing"; then
  die "the environment must not contain pip"
fi
pass "each distribution ships from the package its lock file names"

# The installed tree describes its installed location, never the build's.
root="$td/root"
mkdir -p "$root"
for component in "${components[@]}"; do
  dpkg-deb -x "$(deb_of "${family}-${component}")" "$root"
done
[[ "$(cat "${root}${env_root}/pyvenv.cfg")" == $'home = /usr/bin\ninclude-system-site-packages = false' ]] ||
  die "pyvenv.cfg must name only the distribution interpreter's directory"
[[ "$(head -1 "${root}${env_root}/bin/tpstub-tool")" == "#!${env_root}/bin/python" ]] ||
  die "console scripts must start the environment's own interpreter"
if grep -rlF -- "$repo_root" "$root" >"$td/leaks" 2>/dev/null; then
  die "files record the build directory: $(head -3 "$td/leaks" | tr '\n' ' ')"
fi
python3.12 - "${root}${env_root}/lib/python3.12/site-packages" "$env_root" <<'PY' || die "bytecode caches are not checked-hash caches of the installed paths"
import marshal, pathlib, sys
site, env_root = pathlib.Path(sys.argv[1]), sys.argv[2]
for module in ("tpstub_ct2", "tensorplate_pytorch_backend", "espeakng_loader"):
    data = (site / module / "__pycache__" / "__init__.cpython-312.pyc").read_bytes()
    flags = int.from_bytes(data[4:8], "little")
    recorded = marshal.loads(data[16:]).co_filename
    expected = f"{env_root}/lib/python3.12/site-packages/{module}/__init__.py"
    if flags != 0b11 or recorded != expected:
        sys.exit(f"{module}: flags {flags:#b}, source {recorded}")
PY
pass "installed tree records only installed paths; bytecode is checked-hash"

python3.12 - "$root" "$td/lock" "$family" "${components[@]}" <<'PY' || die "license manifests do not describe what ships"
import json, pathlib, re, sys
root, lock, family = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3]
for component in sys.argv[4:]:
    manifest = json.loads((root / "usr/share/doc" / f"{family}-{component}" / "third-party-licenses.json").read_text())
    pins = dict(re.match(r"(\S+)==\S+ --hash=sha256:(\S+)", l).groups() for l in (lock / f"{component}.txt").read_text().splitlines())
    listed = {d["name"].lower().replace("_", "-"): d for d in manifest["distributions"]}
    if set(listed) != set(pins):
        sys.exit(f"{component}: lists {sorted(listed)}, lock pins {sorted(pins)}")
    for name, entry in listed.items():
        if entry["sha256"] != pins[name]:
            sys.exit(f"{component}: {name} digest differs from its pin")
        for path in entry["license_files"]:
            if not (root / manifest["environment_root"].lstrip("/") / path).is_file():
                sys.exit(f"{component}: {name} lists a license file that is not shipped: {path}")
    stub = listed[f"tpstub-{component}"]
    if component == "cuda":
        if stub["undeclared_license_reason"] != "stub reason" or stub["license_files"]:
            sys.exit(f"{component}: accepted exception not recorded: {stub}")
    elif stub["license_expression"] != "MIT" or not stub["license_files"] or stub["undeclared_license_reason"]:
        sys.exit(f"{component}: stub license not recorded: {stub}")
PY
pass "license manifests list every shipped distribution with its pin"

# --- real dpkg ---------------------------------------------------------------

note "installing the built packages with dpkg"
[[ -z "$(dpkg-query -W -f='${Package} ' 'tensorplate*' 2>/dev/null)" ]] ||
  die "refusing to install over the tensorplate packages already on this host"
[[ -d /run/systemd/system ]] || die "needs a systemd host: the maintainer scripts skip the restart without one"

mkdir -p "$td/bin" "$td/standin/DEBIAN"
printf '#!/bin/sh\necho "$@" >>%s\n' "$td/restarts" >"$td/bin/deb-systemd-invoke"
chmod +x "$td/bin/deb-systemd-invoke"
for standin in "tensorplate-serving amd64" "tensorplate-backend-python-pytorch all"; do
  printf 'Package: %s\nVersion: %s\nArchitecture: %s\nMaintainer: stand-in <stand-in@example.invalid>\nDescription: stand-in\n' \
    "${standin% *}" "$version" "${standin#* }" >"$td/standin/DEBIAN/control"
  dpkg-deb --build --root-owner-group "$td/standin" "$td/${standin% *}.deb" >/dev/null
done

# One dpkg run with the recording deb-systemd-invoke first on PATH; prints
# how many times that run asked for the agent to be restarted.
restarts_from() {
  : >"$td/restarts"
  "${as_root[@]}" env PATH="$td/bin:/usr/sbin:/usr/bin:/sbin:/bin" dpkg "$@" >"$td/dpkg.log" 2>&1 ||
    { tail -15 "$td/dpkg.log" >&2; die "dpkg $1 failed"; }
  grep -cx 'try-restart tensorplate-agent.service' "$td/restarts" || true
}
debs=()
for package in "${expected[@]}"; do debs+=("$(deb_of "$package")"); done

installed=1
[[ "$(restarts_from -i "$td/tensorplate-serving.deb" "$td/tensorplate-backend-python-pytorch.deb")" == 0 ]] ||
  die "installing the stand-ins must not restart anything"
[[ "$(restarts_from -i "${debs[@]}")" == 1 ]] ||
  die "installing the family in one dpkg run must restart the agent exactly once ($(cat "$td/restarts"))"
[[ -z "$(dpkg --verify "${expected[@]}" 2>&1)" ]] || die "dpkg --verify reports changed or missing files"
"${env_root}/bin/python" -c 'import tensorplate_pytorch_backend, tpstub_kokoro, espeakng_loader' ||
  die "the installed environment does not import what it ships"
[[ -z "$(find "$env_root" -newer "$td/restarts" -type f)" ]] || die "running the environment wrote files into it"
[[ "$(restarts_from -r "$family" "${family}-ct2")" == 1 ]] ||
  die "removing a component must restart the agent exactly once"
[[ "$(restarts_from -i "$(deb_of "${family}-ct2")")" == 1 ]] ||
  die "adding a component must restart the agent exactly once"
remaining=()
for package in "${expected[@]}"; do [[ "$package" == "$family" ]] || remaining+=("$package"); done
[[ "$(restarts_from -r "${remaining[@]}")" == 1 ]] ||
  die "removing the whole family must restart the agent exactly once"
[[ "$(restarts_from --purge "${remaining[@]}")" == 0 ]] || die "purging removed packages must not restart the agent"
[[ ! -e "$env_root" ]] || die "purge left files under ${env_root}"
pass "dpkg: one agent restart per run, clean verify, nothing written at run time, clean purge"

printf 'verify_speech_runtime_packages: ok (version %s, %d refusals)\n' "$version" "$cases"
