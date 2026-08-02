#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
harness="${repo_root}/tools/validation/macos-homebrew-lifecycle.sh"

[[ -x "$harness" ]] || {
  printf 'FAIL: macOS Homebrew lifecycle harness is not executable\n' >&2
  exit 1
}

bash -n "$harness"
"$harness" --help >/dev/null

if grep -Fq 'if "$@"' "$harness"; then
  printf 'FAIL: lifecycle stages must not mask intermediate command failures\n' >&2
  exit 1
fi

if grep -Fq 'state/lifecycle-marker"' "$harness"; then
  printf 'FAIL: lifecycle rollback marker must not use a persistent fixed path\n' >&2
  exit 1
fi
grep -Fq 'state/lifecycle-marker.XXXXXX' "$harness"
grep -Fq 'backend_profile") != "mps_fixture"' "$harness"
grep -Fq 'mps_tensor_operation_required_for_load' "$harness"
grep -Fq 'sanitized-transcript.json' "$harness"

macos_install_doc="${repo_root}/docs/install/macos-cli.md"
uninstall_block="$(
  awk '/^brew uninstall tensorplate/ {capture=1} capture {print} /^brew untap/ {exit}' \
    "$macos_install_doc"
)"
for formula_name in \
  tensorplate \
  tensorplate-agent \
  tensorplate-backend-python-pytorch \
  tensorplate-cli \
  tensorplate-observability \
  tensorplate-serving; do
  grep -Fq "$formula_name" <<<"$uninstall_block"
done
grep -Fq 'Apple M1 Pro with 16 GB unified memory' "$macos_install_doc"

printf '%s\n' '{"formulae":[{"name":"tensorplate","versions":{"stable":"0.2.1-rc.1"}}]}' |
  python3 -c 'import json,sys; f=json.load(sys.stdin)["formulae"][0]; print(f["name"], f["versions"]["stable"])' |
  grep -Fxq 'tensorplate 0.2.1-rc.1'

if "$harness" \
  --candidate-formula-dir /missing \
  --baseline-formula /missing \
  --bundle-dir /missing \
  --evidence-dir /missing 2>/dev/null; then
  printf 'FAIL: lifecycle harness ran without its mutation opt-in\n' >&2
  exit 1
fi

if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "$harness"
else
  printf 'verify_macos_homebrew_lifecycle: shellcheck not found; skipping shellcheck\n'
fi

printf 'verify_macos_homebrew_lifecycle: ok\n'
