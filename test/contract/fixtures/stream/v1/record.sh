#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Rewrites the hex column of every *.frames file beside this script from its
# text column, encoded by the protoc given, and writes that protoc's version
# line to versions.txt. With --check it rewrites nothing and fails, naming
# each frame whose committed hex column differs, and versions.txt when it
# names another protoc.
#
# The committed frames were recorded from the repository root with the protoc
# of the pinned vcpkg baseline (versions.txt), in a tree configured with
# TP_ENABLE_STREAMING_GRPC=ON:
#
#   test/contract/fixtures/stream/v1/record.sh \
#     build/vcpkg_installed/x64-linux/tools/protobuf/protoc

set -Eeuo pipefail

check=0
if [[ "${1:-}" == --check ]]; then
  check=1
  shift
fi
if [[ $# -ne 1 || ! -x "$1" ]]; then
  printf 'usage: %s [--check] <protoc>\n' "$0" >&2
  exit 2
fi
protoc="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
schemas="$here/../../../../../protocol/proto"
tab=$'\t'
carriage_return=$'\r'
version="$("$protoc" --version)"

encode() {
  printf '%s\n' "$2" |
    "$protoc" --proto_path="$schemas" --proto_path="$here" --encode="$1" \
      tensorplate/stream/v1/session.proto extension.proto |
    od -An -v -tx1 | tr -d ' \n'
}

status=0
recorded=
trap 'rm -f "$recorded"' EXIT
for file in "$here"/*.frames; do
  recorded="$(mktemp)"
  line_number=0
  while IFS= read -r line || [[ -n "$line" ]]; do
    line_number=$((line_number + 1))
    if [[ "$line" == *"$carriage_return"* ]]; then
      printf '%s:%d: a carriage return; lines end with a line feed\n' "${file##*/}" "$line_number" >&2
      exit 2
    fi
    if [[ -z "$line" || "$line" == '#'* ]]; then
      printf '%s\n' "$line" >>"$recorded"
      continue
    fi
    # Split on single tabs: an unrecorded frame has an empty hex column.
    name="${line%%"$tab"*}"
    rest="${line#*"$tab"}"
    type="${rest%%"$tab"*}"
    rest="${rest#*"$tab"}"
    old_hex="${rest%%"$tab"*}"
    text="${rest#*"$tab"}"
    if [[ "$line" != *"$tab"*"$tab"*"$tab"* || "$text" == *"$tab"* ]]; then
      printf '%s:%d: expected four tab-separated columns\n' "${file##*/}" "$line_number" >&2
      exit 2
    fi
    if ! hex="$(encode "$type" "$text")"; then
      printf '%s: %s: protoc refused the text column\n' "${file##*/}" "$name" >&2
      exit 2
    fi
    if [[ "$hex" != "$old_hex" ]]; then
      status=1
      if [[ $check -eq 1 ]]; then
        printf '%s: %s: the hex column is not what protoc encodes\n' "${file##*/}" "$name" >&2
      fi
    fi
    printf '%s\t%s\t%s\t%s\n' "$name" "$type" "$hex" "$text" >>"$recorded"
  done <"$file"
  if [[ $check -eq 0 ]]; then
    cat "$recorded" >"$file"
  fi
  rm -f "$recorded"
done

if [[ $check -eq 0 ]]; then
  printf '%s\n' "$version" >"$here/versions.txt"
  exit 0
fi
if [[ ! -r "$here/versions.txt" || "$(<"$here/versions.txt")" != "$version" ]]; then
  printf 'versions.txt: does not name this protoc (%s)\n' "$version" >&2
  status=1
fi
exit "$status"
