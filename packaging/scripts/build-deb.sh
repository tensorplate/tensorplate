#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# Build TensorPlate Debian binary packages from the repository root.
#
# The packaging metadata intentionally lives under packaging/debian/ so
# the repo root stays organized, but dpkg-buildpackage expects a root
# debian/ directory. This helper creates a temporary symlink when needed
# and removes only the symlink it created.

set -eu

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
cd "${repo_root}"

created_symlink=0
if [ ! -e debian ]; then
  ln -s packaging/debian debian
  created_symlink=1
fi

cleanup() {
  if [ "${created_symlink}" -eq 1 ] && [ -L debian ]; then
    rm debian
  fi
}
trap cleanup EXIT HUP INT TERM

# Default to a full binary build unless the caller already selects a
# build type (e.g. -A builds only the architecture-independent packages,
# which needs no staged upstream binaries).
build_type_given=0
for arg in "$@"; do
  case "${arg}" in
    -b|-B|-A|-F|-S|-g|-G|--build|--build=*) build_type_given=1 ;;
  esac
done

if [ "${build_type_given}" -eq 1 ]; then
  dpkg-buildpackage -us -uc "$@"
else
  dpkg-buildpackage -us -uc -b "$@"
fi
