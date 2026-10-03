#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Records what dpkg-query prints for a package in each dpkg state, asked the
# way the backend descriptor reader asks. The files beside this script are
# its output on Ubuntu 24.04; the package names are stand-ins built here.
#
# Installs and removes packages, so run it as root in a throwaway container:
#
#   docker run --rm --network none -v "$PWD:/rec" ubuntu:24.04 /rec/record.sh

set -Eeuo pipefail

out=/rec
work="$(mktemp -d)"
cd "$work"
rm -f "$out"/*.stdout "$out"/*.stderr "$out"/exit-codes.txt "$out"/versions.txt

# shellcheck disable=SC2016
FORMAT='${Package}\t${db:Status-Status}\n'
NAMES=(tprec-base tprec-leaf tprec-awaiting tprec-installed tprec-unpacked
  tprec-half-configured tprec-half-installed tprec-config-files
  tprec-removed tprec-purged tprec-never-existed)

record() {
  local label="$1" rc=0
  dpkg-query -W -f="$FORMAT" -- "${NAMES[@]}" >"$out/$label.stdout" 2>"$out/$label.stderr" || rc=$?
  printf '%s %s\n' "$label" "$rc" >>"$out/exit-codes.txt"
}

script() {
  mkdir -p "$1/DEBIAN"
  printf '#!/bin/sh\n%s\n' "$3" >"$1/DEBIAN/$2"
  chmod 0755 "$1/DEBIAN/$2"
}

build() {
  local name="$1"
  shift
  mkdir -p "$name/DEBIAN"
  {
    printf 'Package: %s\nVersion: 1.0\nArchitecture: all\n' "$name"
    printf 'Maintainer: stand-in <stand-in@example.invalid>\nDescription: stand-in\n'
    printf '%s\n' "$@"
  } | sed '/^$/d' >"$name/DEBIAN/control"
  dpkg-deb --build --root-owner-group "$name" "$name.deb" >/dev/null
}

# The base package is triggered the way the speech runtime's is, and asks
# from inside its own trigger processing, where a restarted agent would.
mkdir -p tprec-base/DEBIAN
printf 'interest-noawait tprec-family\ninterest tprec-awaited\n' >tprec-base/DEBIAN/triggers
script tprec-base postinst "label=\"\$(cat $work/label 2>/dev/null || true)\"
if [ \"\$1\" = triggered ] && [ -n \"\$label\" ]; then
  rc=0
  dpkg-query -W -f='$FORMAT' -- tprec-base tprec-leaf >\"$out/\$label.stdout\" 2>\"$out/\$label.stderr\" || rc=\$?
  printf '%s %s\n' \"\$label\" \"\$rc\" >>\"$out/exit-codes.txt\"
  rm -f $work/label
fi"
build tprec-base
mkdir -p tprec-leaf/DEBIAN tprec-awaiting/DEBIAN
printf 'activate-noawait tprec-family\n' >tprec-leaf/DEBIAN/triggers
build tprec-leaf 'Depends: tprec-base'
printf 'activate tprec-awaited\n' >tprec-awaiting/DEBIAN/triggers
build tprec-awaiting 'Depends: tprec-base'
build tprec-installed
build tprec-unpacked
script tprec-half-configured postinst 'exit 1'
build tprec-half-configured
# shellcheck disable=SC2016
script tprec-half-installed postrm 'if [ "$1" = remove ]; then exit 1; fi'
build tprec-half-installed
mkdir -p tprec-config-files/etc/tprec tprec-config-files/DEBIAN
echo setting >tprec-config-files/etc/tprec/conf
printf '/etc/tprec/conf\n' >tprec-config-files/DEBIAN/conffiles
build tprec-config-files
build tprec-removed
build tprec-purged

quiet() { "$@" >/dev/null 2>&1; }

record nothing-installed

echo inside-trigger >label
quiet dpkg -i tprec-base.deb tprec-leaf.deb
quiet dpkg -i tprec-installed.deb tprec-config-files.deb tprec-removed.deb tprec-purged.deb \
  tprec-half-installed.deb
quiet dpkg --unpack tprec-unpacked.deb
quiet dpkg -i tprec-half-configured.deb || true
quiet dpkg -r tprec-config-files tprec-removed
quiet dpkg --purge tprec-purged
quiet dpkg -r tprec-half-installed || true
record each-state

echo inside-trigger-during-purge >label
quiet dpkg --purge tprec-leaf

# apt may unpack in one dpkg run and configure in another.
quiet dpkg --purge tprec-base
quiet dpkg --unpack tprec-base.deb tprec-leaf.deb
record after-unpack-run
quiet dpkg --configure tprec-base tprec-leaf

quiet dpkg --no-triggers -i tprec-awaiting.deb
record triggers-deferred
quiet dpkg --triggers-only --pending

{
  # shellcheck disable=SC1091
  . /etc/os-release
  printf '%s\n' "$PRETTY_NAME"
  dpkg-query --version | head -1
} >"$out/versions.txt"
chmod a+r "$out"/*
rm -rf "$work"
