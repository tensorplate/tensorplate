#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Compare the asset names a GitHub Release serves with the names its SHA256SUMS lists.

GitHub stores an uploaded asset under a name of its own choosing: it served
v0.2.1-rc.1's packages with `.` where their names had `~`, so that
release's signed SHA256SUMS and manifest named files the release did not
contain, `sha256sum -c` over a download of it failed, and its installer
404ed on the first package. The release build stages every package under
the name GitHub is known to serve, but that is a model of GitHub's
behaviour. This reads back what the release actually serves once the
upload is done, and fails on any difference, whatever the cause.

--release-json is the output of `gh release view TAG --json assets`. The
expected names are every name SHA256SUMS lists plus each --also name, for
the assets uploaded beside it that it cannot list: SHA256SUMS itself and
its signature bundle. The release must serve exactly those, each fully
uploaded.
"""

import argparse
import json
import sys
from pathlib import Path


def listed_names(checksums):
    names = []
    for number, line in enumerate(checksums.read_text().splitlines(), 1):
        if not line.strip():
            continue
        fields = line.split(None, 1)
        if len(fields) != 2:
            raise SystemExit(f"{checksums}:{number}: not a `DIGEST  NAME` line: {line!r}")
        name = fields[1].strip()
        # GNU sha256sum marks a binary-mode entry with a leading `*`.
        names.append(name[1:] if name.startswith("*") else name)
    return names


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--checksums", required=True, type=Path)
    parser.add_argument("--release-json", required=True, type=Path)
    parser.add_argument("--also", action="append", default=[],
                        help="an uploaded asset SHA256SUMS does not list")
    args = parser.parse_args(argv)

    expected = listed_names(args.checksums) + args.also
    release = json.loads(args.release_json.read_text())
    assets = release.get("assets") if isinstance(release, dict) else None
    if not isinstance(assets, list):
        raise SystemExit(f"{args.release_json} holds no asset list")
    served = [asset.get("name") for asset in assets if isinstance(asset, dict)]

    problems = []
    for label, names in (("listed", expected), ("served", served)):
        duplicates = sorted({str(name) for name in names if names.count(name) > 1})
        if duplicates:
            problems.append(f"{label} more than once: {', '.join(duplicates)}")
    missing = sorted(set(expected) - set(served))
    if missing:
        problems.append("listed but not served: " + ", ".join(missing))
    unlisted = sorted(set(map(str, served)) - set(expected))
    if unlisted:
        problems.append("served but not listed: " + ", ".join(unlisted))
    incomplete = sorted(
        f"{asset.get('name')} ({asset.get('state')!r})"
        for asset in assets
        if isinstance(asset, dict) and asset.get("state") != "uploaded"
    )
    if incomplete:
        problems.append("not fully uploaded: " + ", ".join(incomplete))
    if problems:
        for problem in problems:
            print(f"release asset names: {problem}", file=sys.stderr)
        return 1
    print(f"release serves exactly the {len(expected)} assets SHA256SUMS names, "
          "under the names it lists them by")
    return 0


if __name__ == "__main__":
    sys.exit(main())
