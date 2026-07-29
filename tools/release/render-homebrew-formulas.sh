#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Render the checked-in Homebrew formula graph for one source archive.

set -Eeuo pipefail

usage() {
  cat <<'EOF'
Usage:
  render-homebrew-formulas.sh \
    --source-url URL \
    --sha256 HEX \
    --output-dir DIR \
    [--template-dir DIR]

Renders the complete TensorPlate Homebrew formula graph into DIR. Existing
managed formula files are replaced atomically; unrelated files are untouched.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

script_dir="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(CDPATH='' cd -- "${script_dir}/../.." && pwd)"

SOURCE_URL=""
SHA256=""
OUTPUT_DIR=""
TEMPLATE_DIR="${repo_root}/packaging/homebrew/Formula"
FORMULAS=(
  tensorplate-agent.rb
  tensorplate-serving.rb
  tensorplate-cli.rb
  tensorplate-observability.rb
  tensorplate-backend-python-pytorch.rb
  tensorplate.rb
)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source-url) SOURCE_URL="${2:-}"; shift 2 ;;
    --sha256) SHA256="${2:-}"; shift 2 ;;
    --output-dir) OUTPUT_DIR="${2:-}"; shift 2 ;;
    --template-dir) TEMPLATE_DIR="${2:-}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) die "unknown option '$1'" ;;
  esac
done

[[ "$SOURCE_URL" =~ ^https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/archive/refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[1-9][0-9]*)?\.tar\.gz$ ]] ||
  die "--source-url must be a TensorPlate-style GitHub tag archive URL"
[[ "$SHA256" =~ ^[0-9a-f]{64}$ ]] || die "--sha256 must be 64 lowercase hexadecimal characters"
[[ -n "$OUTPUT_DIR" ]] || die "--output-dir is required"
[[ -d "$TEMPLATE_DIR" ]] || die "template directory not found: $TEMPLATE_DIR"

shopt -s nullglob
templates=("${TEMPLATE_DIR}"/*.rb)
[[ "${#templates[@]}" -eq "${#FORMULAS[@]}" ]] ||
  die "expected ${#FORMULAS[@]} formula templates, found ${#templates[@]}"

mkdir -p "$OUTPUT_DIR"
for name in "${FORMULAS[@]}"; do
  template="${TEMPLATE_DIR}/${name}"
  destination="${OUTPUT_DIR}/${name}"
  [[ -f "$template" ]] || die "formula template not found: $template"
  [[ "$(grep -c '^  url "' "$template")" -eq 1 ]] ||
    die "$name must contain exactly one top-level source url"
  [[ "$(grep -c '^  sha256 "' "$template")" -eq 1 ]] ||
    die "$name must contain exactly one top-level sha256"

  rendered="$(mktemp "${OUTPUT_DIR}/.${name}.XXXXXX")"
  sed \
    -e "s|^  url \".*\"$|  url \"${SOURCE_URL}\"|" \
    -e "s|^  sha256 \".*\"$|  sha256 \"${SHA256}\"|" \
    "$template" >"$rendered"
  grep -qF "  url \"${SOURCE_URL}\"" "$rendered" ||
    die "url rewrite did not apply to $name"
  grep -qF "  sha256 \"${SHA256}\"" "$rendered" ||
    die "sha256 rewrite did not apply to $name"
  chmod 0644 "$rendered"
  mv -f "$rendered" "$destination"
  printf '%s\n' "$destination"
done
