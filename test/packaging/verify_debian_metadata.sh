#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: debhelper metadata linter.

set -eu

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
debian="${repo_root}/packaging/debian"

fail=0

# Expected binary packages.
PACKAGES="tensorplate-common tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli tensorplate-backend-python-pytorch tensorplate-apt-source tensorplate"

for pkg in ${PACKAGES}; do
  if ! grep -q "^Package: ${pkg}\$" "${debian}/control"; then
    echo "FAIL: ${pkg} missing Package: stanza in debian/control" >&2
    fail=1
    continue
  fi
  if [ "${pkg}" = "tensorplate" ]; then
    # The runtime metapackage deliberately ships no files; its empty
    # shape is asserted by verify_metapackage.sh.
    continue
  fi
  if [ ! -f "${debian}/${pkg}.install" ]; then
    echo "FAIL: ${pkg} missing ${pkg}.install" >&2
    fail=1
  fi
done

# The optional Python backend package must be importable and runnable
# once installed, not merely copy source files under /usr/lib.
for backend_payload in \
  "${repo_root}/packaging/python/tensorplate_pytorch_backend.pth" \
  "${repo_root}/packaging/bin/tensorplate-backend-python-pytorch"; do
  if [ ! -f "${backend_payload}" ]; then
    echo "FAIL: missing Python backend install payload ${backend_payload}" >&2
    fail=1
  fi
done
if ! grep -q 'usr/lib/python3/dist-packages' "${debian}/tensorplate-backend-python-pytorch.install"; then
  echo "FAIL: Python backend manifest must install its dist-packages .pth" >&2
  fail=1
fi
if ! grep -q 'usr/bin/' "${debian}/tensorplate-backend-python-pytorch.install"; then
  echo "FAIL: Python backend manifest must install its console entrypoint" >&2
  fail=1
fi
if ! grep -q '=> usr/share/tensorplate/backends/python_pytorch/backend.json' "${debian}/tensorplate-backend-python-pytorch.install"; then
  echo "FAIL: Python backend manifest must rename its descriptor to backend.json" >&2
  fail=1
fi

# Maintainer scripts must be executable.
for f in "${debian}"/*.preinst "${debian}"/*.postinst "${debian}"/*.prerm "${debian}"/*.postrm; do
  [ -e "${f}" ] || continue
  if [ ! -x "${f}" ]; then
    echo "FAIL: maintainer script not executable: ${f}" >&2
    fail=1
  fi
  # Strip the DEBHELPER token before syntax-checking with sh -n.
  if ! sed 's/#DEBHELPER#//' "${f}" | sh -n; then
    echo "FAIL: maintainer script has shell syntax error: ${f}" >&2
    fail=1
  fi
done
if ! grep -q 'deb-systemd-invoke stop tensorplate-agent.service' "${debian}/tensorplate-agent.prerm"; then
  echo "FAIL: tensorplate-agent.prerm must stop the running unit before remove/upgrade" >&2
  fail=1
fi
if ! grep -q 'deb-systemd-invoke stop tensorplate-observability.service' "${debian}/tensorplate-observability.prerm"; then
  echo "FAIL: tensorplate-observability.prerm must stop the running unit before remove/upgrade" >&2
  fail=1
fi
# The provisioning manifest is the trust root for `tensorplate bundle
# provision`, and the CLI reads it from the path the protocol crate names.
if ! grep -Eq '^packaging/provisioning/manifest\.json[[:space:]]+usr/share/tensorplate/provisioning/$' "${debian}/tensorplate-cli.install"; then
  echo "FAIL: tensorplate-cli must ship the provisioning manifest at /usr/share/tensorplate/provisioning/manifest.json" >&2
  fail=1
fi
for payload in bundles README.md; do
  if ! awk -v payload="packaging/provisioning/${payload}" '
    $1 == payload && $2 == "usr/share/tensorplate/provisioning/" { found = 1 }
    END { exit !found }
  ' "${debian}/tensorplate-cli.install"; then
    echo "FAIL: tensorplate-cli must ship provisioning ${payload}" >&2
    fail=1
  fi
done
cli_stanza="$(sed -n '/^Package: tensorplate-cli$/,/^Package: tensorplate-backend-python-pytorch$/p' "${debian}/control")"
for dependency in curl ca-certificates; do
  if ! printf '%s\n' "$cli_stanza" | grep -Eq "^ ${dependency},$"; then
    echo "FAIL: tensorplate-cli needs ${dependency} for verified HTTPS fetch" >&2
    fail=1
  fi
done
if ! grep -q 'rmdir /var/lib/tensorplate /etc/tensorplate' "${debian}/tensorplate-agent.postrm"; then
  echo "FAIL: tensorplate-agent.postrm purge must remove empty install roots" >&2
  fail=1
fi
# Purge removes durable state; remove keeps it. The identity directory is
# durable state kept apart from state/ so the documented rollback's
# set-aside does not move it, and purge must clear it with the rest.
purge_block="$(sed -n '/^    purge)$/,/^        ;;$/p' "${debian}/tensorplate-agent.postrm")"
for dir in /var/lib/tensorplate/state /var/lib/tensorplate/identity; do
  if ! printf '%s\n' "$purge_block" | grep -Eq "(^|[[:space:]])${dir}([[:space:]]|$)"; then
    echo "FAIL: tensorplate-agent.postrm purge must remove ${dir}" >&2
    fail=1
  fi
done

# Conffile assertions: configs under /etc are auto-managed by
# debhelper as conffiles. Do not duplicate those entries via explicit
# *.conffiles files.
for cfg_pkg in tensorplate-agent tensorplate-observability tensorplate-serving tensorplate-cli tensorplate-apt-source; do
  if [ -e "${debian}/${cfg_pkg}.conffiles" ]; then
    echo "FAIL: ${cfg_pkg} must not duplicate auto-generated conffile metadata" >&2
    fail=1
  fi
done

# systemd units: agent + observability must have one, serving must not.
for unit_pkg in tensorplate-agent tensorplate-observability; do
  if [ ! -f "${debian}/${unit_pkg}.service" ]; then
    echo "FAIL: ${unit_pkg} missing ${unit_pkg}.service" >&2
    fail=1
  fi
done
if [ -f "${debian}/tensorplate-serving.service" ]; then
  echo "FAIL: tensorplate-serving.service must not exist (agent supervises the serving worker, V01-E09)" >&2
  fail=1
fi

# debian/rules and source/format must exist.
for f in "${debian}/rules" "${debian}/source/format" "${debian}/changelog" "${debian}/copyright"; do
  if [ ! -f "${f}" ]; then
    echo "FAIL: missing ${f}" >&2
    fail=1
  fi
done
if ! grep -q 'debhelper-compat (= 13)' "${debian}/control"; then
  echo "FAIL: debian/control must declare debhelper-compat (= 13)" >&2
  fail=1
fi
if [ -e "${debian}/compat" ]; then
  echo "FAIL: debhelper compat must not be duplicated in debian/compat" >&2
  fail=1
fi
if [ ! -x "${debian}/rules" ]; then
  echo "FAIL: debian/rules must be executable" >&2
  fail=1
fi
if [ ! -x "${repo_root}/packaging/scripts/build-deb.sh" ]; then
  echo "FAIL: packaging/scripts/build-deb.sh must be executable" >&2
  fail=1
fi
if grep -q -- '--with systemd' "${debian}/rules"; then
  echo "FAIL: debian/rules must rely on debhelper 13's default dh_installsystemd sequence" >&2
  fail=1
fi
if ! grep -q '^override_dh_auto_configure:' "${debian}/rules"; then
  echo "FAIL: debian/rules must keep configure external to the package skeleton" >&2
  fail=1
fi
# The cli-only build profile builds just the workstation CLI without the
# runtime services or the metapackage. The release workflow no longer uses
# it — the hosted amd64 job builds the full runtime set — but it remains a
# supported build mode, so the opt-outs must stay declared. Runtime services
# and the metapackage opt out; the CLI and its arch-all companions must not.
if [ "$(grep -c '^Build-Profiles: <!pkg\.tensorplate\.cli-only !pkg\.tensorplate\.speech-runtime>$' "${debian}/control")" -ne 4 ]; then
  echo "FAIL: agent, serving, observability, and the metapackage must declare Build-Profiles: <!pkg.tensorplate.cli-only !pkg.tensorplate.speech-runtime>" >&2
  fail=1
fi
if awk '/^Package: tensorplate-cli$/{f=1} f && /^$/{exit} f{print}' "${debian}/control" | grep -q 'cli-only'; then
  echo "FAIL: tensorplate-cli must stay buildable under the cli-only profile" >&2
  fail=1
fi

# The speech runtime family: one package per lock file plus the metapackage,
# built only under its own profile. That build stages no core binary, so
# every arch-dependent core package must opt out of it.
family="tensorplate-speech-runtime"
lock_dir="${repo_root}/packaging/speech-runtime/lock"
stanza_of() { awk -v p="$1" '$0 == "Package: " p {f=1} f && /^$/{exit} f{print}' "${debian}/control"; }
expected_family="${family}"
for lock in "${lock_dir}"/*.txt; do
  component="$(basename "${lock}" .txt)"
  case "${component}" in index|sources|undeclared-licenses) continue ;; esac
  expected_family="${expected_family}
${family}-${component}"
done
declared_family="$(sed -n "s/^Package: \\(${family}.*\\)\$/\\1/p" "${debian}/control")"
if [ "$(printf '%s\n' "${expected_family}" | sort)" != "$(printf '%s\n' "${declared_family}" | sort)" ]; then
  echo "FAIL: debian/control must declare exactly one ${family}-<component> package per lock file, plus ${family}" >&2
  fail=1
fi
for pkg in ${declared_family}; do
  stanza="$(stanza_of "${pkg}")"
  # shellcheck disable=SC2016 # Debian substvars are literal text here.
  for line in 'Architecture: amd64' 'Build-Profiles: <pkg.tensorplate.speech-runtime>' \
              ' tensorplate-serving (= ${binary:Version}),'; do
    if ! printf '%s\n' "${stanza}" | grep -qxF -- "${line}"; then
      echo "FAIL: ${pkg} must declare: ${line}" >&2
      fail=1
    fi
  done
  # The environment builder stages the family. The one file it does not is
  # a runner profile's declaration, installed by that profile's package.
  case "${pkg}" in
    "${family}-ct2") declaration=faster_whisper ;;
    "${family}-kokoro") declaration=kokoro ;;
    *) declaration="" ;;
  esac
  if [ -z "${declaration}" ]; then
    if [ -e "${debian}/${pkg}.install" ]; then
      echo "FAIL: ${pkg} is staged by the environment builder and must not ship ${pkg}.install" >&2
      fail=1
    fi
  elif [ "$(cat "${debian}/${pkg}.install" 2>/dev/null)" != "packaging/backend-metadata/runner_profiles/${declaration}.json usr/share/tensorplate/backends/python_pytorch/runner_profiles.d/" ]; then
    echo "FAIL: ${pkg}.install must install the ${declaration} runner profile declaration and nothing else" >&2
    fail=1
  fi
  for script in preinst postinst prerm postrm; do
    [ -e "${debian}/${pkg}.${script}" ] || continue
    if sed 's/#.*//' "${debian}/${pkg}.${script}" | grep -Eq '(^|[^a-z-])(pip|python[0-9.]*|curl|wget|apt|apt-get)([^a-z-]|$)'; then
      echo "FAIL: ${pkg}.${script} must not run pip, Python or a network client" >&2
      fail=1
    fi
  done
done
if ! printf '%s\n' "$(stanza_of "${family}-base")" | grep -qxF ' python3.12 (<< 3.13),'; then
  echo "FAIL: ${family}-base must depend on the distribution's python3.12 (<< 3.13)" >&2
  fail=1
fi
for dependency in libespeak-ng1 espeak-ng-data; do
  if ! printf '%s\n' "$(stanza_of "${family}-kokoro")" | grep -qxF " ${dependency},"; then
    echo "FAIL: ${family}-kokoro must depend on the distribution's ${dependency}" >&2
    fail=1
  fi
done
if [ "$(cat "${debian}/${family}-base.triggers" 2>/dev/null)" != "interest-noawait ${family}" ]; then
  echo "FAIL: ${family}-base must hold the family trigger without awaiting it" >&2
  fail=1
fi
# The restart belongs to one arm of each script: every other dpkg action,
# `configure` included, would restart the agent once per package.
for arm in "postinst triggered" "postrm remove"; do
  script="${debian}/${family}-base.${arm% *}"
  inside="$(sed -n "/^    ${arm#* })\$/,/^        ;;\$/p" "${script}" 2>/dev/null | grep -c 'deb-systemd-invoke try-restart tensorplate-agent.service' || true)"
  anywhere="$(grep -c 'deb-systemd-invoke [a-z]' "${script}" 2>/dev/null || true)"
  if [ "${inside}" != 1 ] || [ "${anywhere}" != 1 ]; then
    echo "FAIL: ${family}-base.${arm% *} must try-restart the agent in its ${arm#* } arm and nowhere else" >&2
    fail=1
  fi
done
# Each runner profile's package set is the dependency closure of the package
# that declares it, and the packaged declaration names that set.
if ! python3 - "${debian}/control" "${repo_root}/packaging/backend-metadata/runner_profiles" "${family}" <<'PY'
import json, pathlib, re, sys

control, declarations, family = sys.argv[1:]
depends = {}
for stanza in open(control).read().split("\n\n"):
    name = re.search(r"^Package: (\S+)$", stanza, re.M)
    if name:
        block = re.search(r"^Depends:\n((?: .*\n?)*)", stanza, re.M)
        depends[name.group(1)] = re.findall(r"^ ([a-z0-9][a-z0-9.+-]*)", block.group(1) if block else "", re.M)

def closure(package):
    seen, todo = set(), [package]
    while todo:
        current = todo.pop()
        if current not in seen and current.startswith(family):
            seen.add(current)
            todo += depends[current]
    return seen

problems = []
if "tensorplate-backend-python-pytorch" not in depends[f"{family}-base"]:
    problems.append(f"{family}-base must depend on tensorplate-backend-python-pytorch, which ships the descriptor")
leaf = {"faster_whisper": f"{family}-ct2", "kokoro": f"{family}-kokoro"}
profiles = {}
for path in sorted(pathlib.Path(declarations).glob("*.json")):
    declared = json.load(open(path))
    profile = declared["runner_profile"]
    if declared["backend_name"] != "python_pytorch" or profile["id"] != path.stem:
        problems.append(f"{path.name} must declare the python_pytorch runner profile it is named for")
    profiles[profile["id"]] = set(profile["packages"])
if set(profiles) != set(leaf):
    problems.append(f"the packaged declarations' profiles are {sorted(profiles)}")
for profile, package in leaf.items():
    if closure(package) != profiles.get(profile):
        problems.append(f"{package} and its dependencies are {sorted(closure(package))}; "
                        f"the {profile} profile needs {sorted(profiles.get(profile, []))}")
missing = {p for p in depends if p.startswith(family + "-")} - set(depends[family])
if missing:
    problems.append(f"{family} must depend on every component; missing {sorted(missing)}")
for problem in problems:
    print(f"FAIL: {problem}", file=sys.stderr)
sys.exit(1 if problems else 0)
PY
then
  fail=1
fi
for pkg in ${declared_family}; do
  case "${pkg}" in "${family}"|"${family}-base") continue ;; esac
  if [ "$(cat "${debian}/${pkg}.triggers" 2>/dev/null)" != "activate-noawait ${family}" ]; then
    echo "FAIL: ${pkg} must activate the family trigger" >&2
    fail=1
  fi
done
if awk '
    /^Package: / { pkg = $2; any = 0; out = 0 }
    /^Architecture: any$/ { any = 1 }
    /^Build-Profiles: .*!pkg\.tensorplate\.speech-runtime/ { out = 1 }
    /^$/ { if (any && !out) print pkg; any = 0; out = 0 }
    END { if (any && !out) print pkg }
  ' "${debian}/control" | grep -q .; then
  echo "FAIL: every Architecture: any package must opt out of the pkg.tensorplate.speech-runtime profile" >&2
  fail=1
fi
if ! grep -q 'dh_builddeb -- -Zzstd -z19' "${debian}/rules"; then
  echo "FAIL: debian/rules must pin the family's package compression" >&2
  fail=1
fi
if ! grep -q 'dh_installsystemd --no-start -ptensorplate-agent' "${debian}/rules"; then
  echo "FAIL: debian/rules must install the agent unit into tensorplate-agent" >&2
  fail=1
fi
if ! grep -q 'dh_installsystemd --no-start -ptensorplate-observability' "${debian}/rules"; then
  echo "FAIL: debian/rules must install the observability unit into tensorplate-observability" >&2
  fail=1
fi
if ! grep -q 'dh_shlibdeps -ptensorplate-serving --dpkg-shlibdeps-params=--ignore-missing-info' "${debian}/rules"; then
  echo "FAIL: debian/rules must tolerate JetPack CUDA libraries without shlibs metadata for tensorplate-serving" >&2
  fail=1
fi

if [ "${fail}" -eq 0 ]; then
  echo "verify_debian_metadata: ok"
fi
exit "${fail}"
