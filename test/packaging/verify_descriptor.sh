#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
#
# packaging: backend descriptor sanity check.

set -eu

repo_root="$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
descriptor="${repo_root}/packaging/backend-metadata/python_pytorch.json"

if [ ! -r "${descriptor}" ]; then
  echo "FAIL: descriptor missing at ${descriptor}" >&2
  exit 1
fi

# Validate JSON syntax with python3 (always present in our CI).
if command -v python3 >/dev/null 2>&1; then
  python3 -c "
import json, sys
with open('${descriptor}') as f:
    d = json.load(f)
required = ['schema_version', 'backend_name', 'package_name', 'package_version']
for k in required:
    if k not in d:
        print(f'missing required field: {k}', file=sys.stderr)
        sys.exit(1)
if d['backend_name'] != 'python_pytorch':
    print('descriptor backend_name must be python_pytorch', file=sys.stderr)
    sys.exit(1)
py = d.get('python', {})
if py.get('interpreter') and not py['interpreter'].startswith('/'):
    print('python.interpreter must be absolute', file=sys.stderr)
    sys.exit(1)
print('descriptor OK:', d['package_name'], d['package_version'])
"
else
  # Fallback: just check the shape with grep.
  for f in schema_version backend_name package_name package_version; do
    if ! grep -q "\"${f}\"" "${descriptor}"; then
      echo "FAIL: descriptor missing field ${f}" >&2
      exit 1
    fi
  done
  echo "verify_descriptor: ok (grep fallback; python3 not installed)"
fi
