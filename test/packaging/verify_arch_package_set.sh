#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# packaging: assert a binary-arch-only build produces exactly the runtime
# package set for the host architecture.
#
# This is the one thing no other check covers. Which packages
# `dpkg-buildpackage -B` emits is decided by debhelper from the Architecture
# fields in debian/control, and the release workflow's hosted job depends on
# that answer being exactly five arch-dependent packages with the three
# architecture-independent ones left to the primary job. Everything else in
# the suite reads control as text; this reads what dpkg actually builds.
#
# Binaries are stubbed, so this is a packaging-shape check and not a build.
# They are stubbed as compiled ELF objects rather than shell scripts, because
# debhelper only generates -dbgsym packages for real ELF files and the release
# job's copy globs have to exclude those.
#
# Not part of run.sh: it invokes dpkg-buildpackage and writes .deb files to the
# repository parent, which run.sh's callers (including a release build) must
# not have happen underneath them.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
cd "$repo_root"

die() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
note() { printf '==> %s\n' "$*"; }
pass() { printf 'PASS: %s\n' "$*"; }

# Mutates the repository parent, so refuse to run on a developer machine
# unless explicitly forced.
if [[ "${CI:-}" != "true" && "${TP_ALLOW_LOCAL_ARCH_PACKAGE_TEST:-}" != "1" ]]; then
  echo "verify_arch_package_set: skipped (set CI=true or TP_ALLOW_LOCAL_ARCH_PACKAGE_TEST=1)"
  exit 0
fi

for tool in dpkg-buildpackage dpkg-deb dpkg-parsechangelog cc; do
  command -v "$tool" >/dev/null 2>&1 || die "required command not found: $tool"
done

host_arch="$(dpkg --print-architecture)"
version="$(dpkg-parsechangelog -l packaging/debian/changelog -S Version)"
repo_parent="$(dirname "$repo_root")"

# Packages debhelper must build for the host architecture under -B.
expected_arch_packages=(
  tensorplate
  tensorplate-agent
  tensorplate-serving
  tensorplate-observability
  tensorplate-cli
)
# Architecture: all packages are shared and must NOT appear in a -B build.
expected_absent_packages=(
  tensorplate-common
  tensorplate-backend-python-pytorch
  tensorplate-apt-source
)

created=()
cleanup() {
  local f
  for f in "${created[@]:-}"; do
    [[ -n "$f" ]] && rm -f -- "$f"
  done
  rm -rf -- "${repo_root}/target/release/.arch-set-stub" 2>/dev/null || true
}
trap cleanup EXIT

note "staging stub ELF binaries"
stub_src="$(mktemp -d)/stub.c"
printf 'int main(void) { return 0; }\n' > "$stub_src"
for stub in target/release/tensorplate \
            target/release/tensorplate-agent \
            target/release/tensorplate-observability \
            build/release/tensorplate-serving; do
  mkdir -p "$(dirname "$stub")"
  # -g so debhelper has debug info to split into a -dbgsym package.
  cc -g -o "$stub" "$stub_src"
done

note "building binary-arch packages for ${host_arch}"
build_log="$(mktemp)"
if ! packaging/scripts/build-deb.sh -B >"$build_log" 2>&1; then
  tail -30 "$build_log" >&2
  die "dpkg-buildpackage -B failed for ${host_arch}"
fi

shopt -s nullglob
for deb in "${repo_parent}"/tensorplate*_"${version}"_*.deb; do
  created+=("$deb")
done
shopt -u nullglob

for pkg in "${expected_arch_packages[@]}"; do
  candidate="${repo_parent}/${pkg}_${version}_${host_arch}.deb"
  [[ -f "$candidate" ]] ||
    die "${pkg} was not built for ${host_arch}; a -B build must produce it"
  pass "${pkg} built for ${host_arch}"
done

for pkg in "${expected_absent_packages[@]}"; do
  shopt -s nullglob
  strays=("${repo_parent}/${pkg}_${version}_"*.deb)
  shopt -u nullglob
  ((${#strays[@]} == 0)) ||
    die "${pkg} is Architecture: all and must not be built by -B; found ${strays[*]}"
  pass "${pkg} correctly left to the primary build"
done

# The metapackage must stay an empty dependency bundle on this architecture.
# debhelper always adds changelog.Debian.gz and copyright under /usr/share/doc,
# so "empty" means no payload outside that. Field 6 is the path; $NF would be
# the link target on a symlink line.
meta="${repo_parent}/tensorplate_${version}_${host_arch}.deb"
payload="$(dpkg-deb -c "$meta" | awk '$6 !~ /\/$/ {print $6}' | grep -v '^\./usr/share/doc/' || true)"
[[ -z "$payload" ]] || die "the tensorplate metapackage must ship no payload; found: ${payload}"
pass "metapackage ships no payload on ${host_arch}"

# Package closure for the deploy-smoke path: the serving binary must come from
# the package, with no source-tree fallback.
serving="${repo_parent}/tensorplate-serving_${version}_${host_arch}.deb"
dpkg-deb -c "$serving" | grep -qE ' \./usr/lib/tensorplate/tensorplate-serving$' ||
  die "tensorplate-serving must ship /usr/lib/tensorplate/tensorplate-serving"
pass "serving binary ships from the package"

# The release job copies by explicit package name so auto-generated -dbgsym
# packages cannot enter the asset set. Prove the globs it uses exclude them,
# using whatever dbgsym packages this build actually produced.
shopt -s nullglob
dbgsyms=("${repo_parent}"/tensorplate*-dbgsym_"${version}"_*.deb)
shopt -u nullglob
if ((${#dbgsyms[@]} > 0)); then
  for pkg in "${expected_arch_packages[@]}"; do
    shopt -s nullglob
    matched=("${repo_parent}/${pkg}"_*_"${host_arch}".deb)
    shopt -u nullglob
    for hit in "${matched[@]}"; do
      case "$(basename -- "$hit")" in
        *-dbgsym_*) die "the ${pkg} copy glob matches a dbgsym package: $(basename -- "$hit")" ;;
      esac
    done
  done
  pass "release copy globs exclude ${#dbgsyms[@]} dbgsym package(s)"
else
  note "no dbgsym packages produced; copy-glob exclusion not exercised"
fi

printf 'verify_arch_package_set: ok (%s, version %s)\n' "$host_arch" "$version"
