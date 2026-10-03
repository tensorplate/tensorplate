#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# packaging: run the release workflow's speech runtime job for real, from stub
# wheels and at a candidate version, then take what it builds through the
# release manifest and the installer's package selection.
#
# Not part of run.sh: it builds packages with dpkg-buildpackage and fetches
# two files from PyPI.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
cd "$repo_root"

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
note() { printf '==> %s\n' "$*"; }
pass() { printf 'PASS: %s\n' "$*"; }

if [[ "${CI:-}" != "true" && "${TP_ALLOW_LOCAL_SPEECH_RUNTIME_PACKAGE_TEST:-}" != "1" ]]; then
  echo "verify_speech_runtime_release_job: skipped (set CI=true or TP_ALLOW_LOCAL_SPEECH_RUNTIME_PACKAGE_TEST=1)"
  exit 0
fi

for tool in dpkg-buildpackage dpkg-deb python3.12 git; do
  command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done
python3.12 -c 'import yaml' 2>/dev/null || die "python3.12 needs the yaml module (python3-yaml)"
[[ "$(dpkg --print-architecture)" == amd64 ]] || die "the family is Architecture: amd64; run on an amd64 host"

workflow=".github/workflows/release.yml"
driver="tools/release/tensorplate-release.sh"
job=build_speech_runtime
version="$(packaging/version.sh)"
candidate=7
deb_version="${version}~rc.${candidate}"
tag="v${version}-rc.${candidate}"
mapfile -t family < <(sed -n '/^readonly SPEECH_RUNTIME_PACKAGES=(/,/^)/p' "$driver" | sed '1d;$d' | awk '{print $1}')
((${#family[@]} > 0)) || die "could not read SPEECH_RUNTIME_PACKAGES from ${driver}"

td="$(mktemp -d)"
trap 'rm -rf -- "$td"' EXIT

# The steps stage a version into the changelog and write packages beside the
# checkout, so they run in a copy of the tree as it is, committed or not.
checkout="$td/work/checkout"
mkdir -p "$checkout" "$td/runner"
git ls-files -co --exclude-standard -z >"$td/files" || die "cannot list the files of this checkout"
xargs -0 cp -P --parents -t "$checkout" <"$td/files"

run_step() {
  local name="$1" body
  body="$(python3.12 - "$workflow" "$job" "$name" <<'PY'
import sys, yaml
steps = [s for s in yaml.safe_load(open(sys.argv[1]))["jobs"][sys.argv[2]]["steps"]
         if s.get("name") == sys.argv[3]]
if len(steps) != 1:
    sys.exit(f"{sys.argv[2]} has {len(steps)} steps named {sys.argv[3]!r}")
print(steps[0]["run"])
PY
)" || die "release.yml: no single step named '${name}' in ${job}"
  (cd "$checkout" &&
    RUNNER_TEMP="$td/runner" DEB_VERSION="$deb_version" \
      bash --noprofile --norc -eo pipefail -c "$body") >"$td/step.log" 2>&1 ||
    { tail -40 "$td/step.log" >&2; die "release job step failed: ${name}"; }
  pass "release job step: ${name}"
}

note "running the ${job} steps at ${deb_version}"
run_step "Stage the speech runtime wheelhouse"
run_step "Build speech runtime packages"
run_step "Assert speech runtime package closure"

built="$(cd "$checkout/dist/speech-runtime" && printf '%s\n' *.deb | sort)"
expected="$(printf "%s_${deb_version}-1_amd64.deb\n" "${family[@]}" | sort)"
[[ "$built" == "$expected" ]] ||
  { printf 'expected:\n%s\nbuilt:\n%s\n' "$expected" "$built" >&2; die "the job must collect exactly the family, at the candidate's version"; }
unpacked="$td/unpacked"
dpkg-deb -x "$checkout/dist/speech-runtime/${family[1]}_${deb_version}-1_amd64.deb" "$unpacked"
[[ -x "$unpacked/usr/lib/tensorplate/speech-runtime/bin/python" || -L "$unpacked/usr/lib/tensorplate/speech-runtime/bin/python" ]] ||
  die "${family[1]} does not ship the environment's interpreter link"
pass "the job builds the ${#family[@]} family packages at ${deb_version}-1"

# --- through the release manifest --------------------------------------------

note "staging the family beside a stand-in core set"
core="$td/core"
artifacts="$td/artifacts"
mkdir -p "$core" "$artifacts"
standin() {
  local package="$1" arch="$2" tree="$td/standin/${1}-${2}"
  mkdir -p "$tree/DEBIAN"
  printf 'Package: %s\nVersion: %s\nArchitecture: %s\nMaintainer: stand-in <stand-in@example.invalid>\nDescription: stand-in\n' \
    "$package" "${deb_version}-1" "$arch" >"$tree/DEBIAN/control"
  dpkg-deb --build --root-owner-group "$tree" "$core/${package}_${deb_version}-1_${arch}.deb" >/dev/null
}
for package in tensorplate-common tensorplate-backend-python-pytorch tensorplate-apt-source; do
  standin "$package" all
done
for package in tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli tensorplate; do
  standin "$package" arm64
  standin "$package" amd64
done

# Staged by the release build's own functions, as test/release/run.sh does.
functions="$(sed -n '/^release_asset_name() {$/,/^}$/p;/^stage_release_debs() {$/,/^}$/p' tools/release/build-release-artifacts.sh)"
[[ "$functions" == *"release_asset_name() {"*"stage_release_debs() {"* ]] ||
  die "build-release-artifacts.sh no longer defines release_asset_name and stage_release_debs"
(
  DEB_VERSION="$deb_version"
  eval "$functions"
  : "$DEB_VERSION"
  stage_release_debs "$artifacts" "$core"/*.deb "$checkout/dist/speech-runtime"/*.deb
) || die "release staging refused the built packages"
printf '#!/usr/bin/env bash\n' >"$artifacts/install.sh"
printf 'wheel\n' >"$artifacts/tensorplate_python-${version}rc${candidate}-py3-none-any.whl"
printf 'sdist\n' >"$artifacts/tensorplate_python-${version}rc${candidate}.tar.gz"

identity=(--version "$version" --tag "$tag" --artifacts-dir "$artifacts"
  --manifest "$artifacts/tensorplate-${tag}-artifacts.json" --checksums "$artifacts/SHA256SUMS")
if "$driver" manifest "${identity[@]}" >"$td/unasked.log" 2>&1; then
  die "manifest generation accepted the family without --with-speech-runtime"
fi
grep -q 'speech runtime packages are staged' "$td/unasked.log" ||
  { cat "$td/unasked.log" >&2; die "the unasked family was refused for another reason"; }
"$driver" manifest "${identity[@]}" --with-speech-runtime >"$td/manifest.log" 2>&1 ||
  { cat "$td/manifest.log" >&2; die "manifest generation refused the family the job built"; }
"$driver" verify --skip-tag-verify "${identity[@]}" --with-speech-runtime >"$td/verify.log" 2>&1 ||
  { cat "$td/verify.log" >&2; die "verification refused the family the job built"; }
[[ "$(find "$artifacts" -maxdepth 1 -name '*.deb' | wc -l)" == 21 ]] || die "expected 21 packages in the artifact set"
pass "manifest and verify carry the family at its control version"

# --- through the installer's selection ---------------------------------------

selected="$(
  eval "$(sed -n '/^readonly [A-Z_]*_PACKAGE=/p;/^readonly SPEECH_RUNTIME_/p;/^write_install_deb_list() {$/,/^}$/p' packaging/scripts/install.sh)"
  write_install_deb_list "$artifacts/tensorplate-${tag}-artifacts.json" 0 runtime amd64 1
)" || die "install.sh --with-speech-runtime selects nothing from the manifest"
for package in "${family[@]}"; do
  name="${package}_${deb_version//\~/.}-1_amd64.deb"
  grep -qxF "$name" <<<"$selected" || die "install.sh --with-speech-runtime does not select ${name}"
  [[ "$(dpkg-deb -f "$artifacts/$name" Version)" == "${deb_version}-1" ]] || die "${name} is not at ${deb_version}-1"
done
[[ "$(wc -l <<<"$selected")" == $((6 + ${#family[@]})) ]] ||
  { printf '%s\n' "$selected" >&2; die "install.sh --with-speech-runtime must select the core runtime, the backend package and the family"; }
pass "install.sh --with-speech-runtime selects the family from the manifest"

printf 'verify_speech_runtime_release_job: ok (%s, %d packages)\n' "$deb_version" "${#family[@]}"
