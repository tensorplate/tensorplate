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

runtime_version="$("${repo_root}/packaging/version.sh")"

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

# The declared runtime range must bracket the version this tree builds.
# The descriptor is not in the release driver's rewritten-file list, so a
# version bump leaves it behind unless something fails loudly here.
def key(v):
    core = v.split('-')[0].split('~')[0]
    return tuple(int(p) if p.isdigit() else 0 for p in core.split('.')[:3])

rng = d.get('tensorplate_runtime_range')
if not isinstance(rng, dict):
    print('descriptor must declare tensorplate_runtime_range', file=sys.stderr)
    sys.exit(1)
lo, hi = rng.get('min'), rng.get('max_exclusive')
if not lo or not hi:
    print('tensorplate_runtime_range needs both min and max_exclusive', file=sys.stderr)
    sys.exit(1)
if key(lo) >= key(hi):
    print(f'tensorplate_runtime_range min {lo} must precede max_exclusive {hi}', file=sys.stderr)
    sys.exit(1)
runtime = '${runtime_version}'
if not (key(lo) <= key(runtime) < key(hi)):
    print(
        f'packaging/VERSION {runtime} is outside the declared backend runtime '
        f'range [{lo}, {hi}); bump max_exclusive in lockstep with the release line',
        file=sys.stderr,
    )
    sys.exit(1)
print('descriptor OK:', d['package_name'], d['package_version'], f'[{lo}, {hi})')
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
