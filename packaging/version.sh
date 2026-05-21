#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# Emit the canonical TensorPlate version stamp.
#
# Single source of truth: packaging/VERSION. Maintainer scripts, tests,
# and CI consume this helper so the runtime crate version, the
# `debian/changelog` entry, and `tensorplate doctor` agree.

set -eu

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
file="${here}/VERSION"

if [ ! -r "${file}" ]; then
  echo "packaging/VERSION not found at ${file}" >&2
  exit 1
fi

# Trim whitespace and refuse empty content.
version="$(tr -d '[:space:]' <"${file}")"
if [ -z "${version}" ]; then
  echo "packaging/VERSION is empty" >&2
  exit 1
fi

printf '%s\n' "${version}"
