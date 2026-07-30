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

stub_paths=(
  target/release/tensorplate
  target/release/tensorplate-agent
  target/release/tensorplate-observability
  build/release/tensorplate-serving
)
created=()
saved_dir="$(mktemp -d)"
cleanup() {
  local f stub
  for f in "${created[@]:-}"; do
    [[ -n "$f" ]] && rm -f -- "$f"
  done
  # Put back whatever the stubs displaced. Leaving a 4-instruction stub named
  # tensorplate-agent in a developer's target/release would be discovered much
  # later and blamed on something else.
  for stub in "${stub_paths[@]}"; do
    if [[ -f "${saved_dir}/$(basename "$stub")" ]]; then
      mv -f "${saved_dir}/$(basename "$stub")" "$stub"
    else
      rm -f -- "$stub"
    fi
  done
  rm -rf -- "$saved_dir"
}
trap cleanup EXIT

note "staging stub ELF binaries"
stub_src="$(mktemp -d)/stub.c"
printf 'int main(void) { return 0; }\n' > "$stub_src"
for stub in "${stub_paths[@]}"; do
  mkdir -p "$(dirname "$stub")"
  [[ -f "$stub" ]] && mv -f "$stub" "${saved_dir}/$(basename "$stub")"
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
# Ubuntu writes automatic debug packages as .ddeb, Debian as .deb; collect
# both so neither is left behind in the repository parent.
for deb in "${repo_parent}"/tensorplate*_"${version}"_*.deb \
           "${repo_parent}"/tensorplate*_"${version}"_*.ddeb; do
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

# The built set must be EXACTLY the expected one. Without this, adding a
# sixth Architecture: any package to debian/control would build here and be
# silently absent from the release: the workflow's copy loop names only the
# five it knows, and every other check asserts presence rather than
# exhaustiveness. This is the only place that sees the real built set, so it
# is the only place that can notice.
shopt -s nullglob
built=()
for deb in "${repo_parent}"/*_"${version}"_"${host_arch}".deb; do
  name="$(basename -- "$deb")"
  case "$name" in
    *-dbgsym_*) continue ;;
  esac
  built+=("${name%%_*}")
done
shopt -u nullglob
expected_sorted="$(printf '%s\n' "${expected_arch_packages[@]}" | sort)"
built_sorted="$(printf '%s\n' "${built[@]:-}" | sort)"
if [[ "$expected_sorted" != "$built_sorted" ]]; then
  printf 'expected:\n%s\nbuilt:\n%s\n' "$expected_sorted" "$built_sorted" >&2
  die "the ${host_arch} build set does not match the declared runtime set; a new arch-dependent package must be added to SECONDARY_ARCH_PACKAGES in both release scripts and to the copy loop in release.yml"
fi
pass "built set is exactly the declared ${host_arch} runtime set"

# The metapackage must stay an empty dependency bundle on this architecture.
# debhelper always adds changelog.Debian.gz and copyright under /usr/share/doc,
# so "empty" means no payload outside that. Field 6 is the path; $NF would be
# the link target on a symlink line.
meta="${repo_parent}/tensorplate_${version}_${host_arch}.deb"
meta_contents="$(dpkg-deb -c "$meta")"
payload="$(printf '%s\n' "$meta_contents" | awk '$6 !~ /\/$/ {print $6}' | { grep -v '^\./usr/share/doc/' || true; })"
[[ -z "$payload" ]] || die "the tensorplate metapackage must ship no payload; found: ${payload}"
pass "metapackage ships no payload on ${host_arch}"

# Package closure for the deploy-smoke path: the serving binary must come from
# the package, with no source-tree fallback.
serving="${repo_parent}/tensorplate-serving_${version}_${host_arch}.deb"
dpkg-deb -c "$serving" | grep -qE ' \./usr/lib/tensorplate/tensorplate-serving$' ||
  die "tensorplate-serving must ship /usr/lib/tensorplate/tensorplate-serving"
pass "serving binary ships from the package"

# The agent config is selected per architecture by a dh-exec filter, which is
# only exercised by a real build. Read it back out of the archive: a filter
# that silently matched the wrong line would ship a config declaring a device
# family this host is not and a backend this build does not contain.
agent_deb="${repo_parent}/tensorplate-agent_${version}_${host_arch}.deb"
agent_conf="$(dpkg-deb --fsys-tarfile "$agent_deb" | tar -xO ./etc/tensorplate/agent.json)"
[[ -n "$agent_conf" ]] || die "tensorplate-agent must ship /etc/tensorplate/agent.json"
case "$host_arch" in
  amd64)
    printf '%s' "$agent_conf" | grep -q '"device_family": "x86_64"' ||
      die "the ${host_arch} agent config must declare device_family x86_64"
    if printf '%s' "$agent_conf" | grep -q '"tensorrt"'; then
      die "the ${host_arch} agent config must not advertise tensorrt; that build has no TensorRT adapter"
    fi
    pass "agent config is the ${host_arch} variant"
    ;;
  *)
    printf '%s' "$agent_conf" | grep -q '"device_family": "jetson-orin"' ||
      die "the ${host_arch} agent config must keep device_family jetson-orin"
    pass "agent config is the default variant on ${host_arch}"
    ;;
esac

# The release job copies by explicit package name so auto-generated -dbgsym
# packages cannot enter the asset set. Prove the globs it uses exclude them,
# using whatever dbgsym packages this build actually produced.
shopt -s nullglob
dbgsyms=("${repo_parent}"/tensorplate*-dbgsym_"${version}"_*.deb
         "${repo_parent}"/tensorplate*-dbgsym_"${version}"_*.ddeb)
shopt -u nullglob
if ((${#dbgsyms[@]} > 0)); then
  for pkg in "${expected_arch_packages[@]}"; do
    shopt -s nullglob
    matched=("${repo_parent}/${pkg}"_*_"${host_arch}".deb
             "${repo_parent}/${pkg}"_*_"${host_arch}".ddeb)
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
