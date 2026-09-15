#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Bind a local baseline's checksums to a publicly available GitHub release.

A workflow signature establishes build provenance, not publication: signed
Actions artifacts exist before the workflow creates a release, including when
that later step fails or creates a draft. This check uses no authentication,
so a maintainer's access to drafts cannot make one a published baseline.
The release installer still verifies the checksum signature independently.
"""

import argparse
import datetime
import hashlib
import http.client
import json
import pathlib
import re
import sys
import urllib.error
import urllib.parse
import urllib.request


REPOSITORY = "tensorplate/tensorplate"
RELEASE_TAG = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-rc\.[1-9][0-9]*)?")
REQUEST_TIMEOUT_SECONDS = 20
MAX_METADATA_BYTES = 2 * 1024 * 1024
MAX_CHECKSUM_BYTES = 1024 * 1024


class PublicationError(Exception):
    """The baseline could not be established as a published release."""


def fetch_bytes(url, limit, label, accept):
    request = urllib.request.Request(url, headers={
        "Accept": accept,
        "User-Agent": "tensorplate-baseline-publication",
        "X-GitHub-Api-Version": "2022-11-28",
    })
    try:
        with urllib.request.urlopen(request, timeout=REQUEST_TIMEOUT_SECONDS) as response:
            # Release assets redirect to GitHub's storage. Keep HTTPS across
            # that redirect without trusting a download URL from metadata.
            if urllib.parse.urlsplit(response.geturl()).scheme != "https":
                raise PublicationError(f"{label} redirected away from HTTPS")
            body = response.read(limit + 1)
    except urllib.error.HTTPError as error:
        raise PublicationError(f"cannot fetch {label} (HTTP {error.code})") from error
    except (urllib.error.URLError, OSError, http.client.HTTPException) as error:
        raise PublicationError(f"cannot fetch {label}: {type(error).__name__}") from error
    if len(body) > limit:
        raise PublicationError(f"{label} exceeds the {limit}-byte limit")
    return body


def published_digest(assets_dir, release_tag):
    if not RELEASE_TAG.fullmatch(release_tag):
        raise PublicationError("release tag must be vX.Y.Z or vX.Y.Z-rc.N")
    tag = urllib.parse.quote(release_tag, safe="")
    api_url = f"https://api.github.com/repos/{REPOSITORY}/releases/tags/{tag}"
    asset_url = f"https://github.com/{REPOSITORY}/releases/download/{tag}/SHA256SUMS"
    try:
        with (pathlib.Path(assets_dir) / "SHA256SUMS").open("rb") as handle:
            local = handle.read(MAX_CHECKSUM_BYTES + 1)
    except OSError as error:
        raise PublicationError("cannot read the baseline SHA256SUMS") from error
    if not local or len(local) > MAX_CHECKSUM_BYTES:
        raise PublicationError("baseline SHA256SUMS is empty or exceeds the size limit")

    raw = fetch_bytes(api_url, MAX_METADATA_BYTES, "public release metadata",
                      "application/vnd.github+json")
    try:
        release = json.loads(raw)
    except (ValueError, UnicodeError) as error:
        raise PublicationError("public release metadata is not valid JSON") from error
    if not isinstance(release, dict) or release.get("tag_name") != release_tag:
        raise PublicationError("public release metadata does not match the baseline tag")
    if release.get("draft") is not False:
        raise PublicationError("the baseline release is a draft or has no publication status")
    published_at = release.get("published_at")
    try:
        if not isinstance(published_at, str):
            raise ValueError("missing publication time")
        timestamp = datetime.datetime.fromisoformat(published_at.replace("Z", "+00:00"))
        if timestamp.utcoffset() is None:
            raise ValueError("publication time has no timezone")
    except ValueError as error:
        raise PublicationError("the baseline release has no valid publication time") from error

    assets = release.get("assets")
    if not isinstance(assets, list):
        raise PublicationError("public release metadata has no asset list")
    checksums = [asset for asset in assets
                 if isinstance(asset, dict) and asset.get("name") == "SHA256SUMS"]
    if (len(checksums) != 1 or checksums[0].get("state") != "uploaded"
            or checksums[0].get("browser_download_url") != asset_url):
        raise PublicationError("the published baseline has no unique uploaded SHA256SUMS asset")

    public = fetch_bytes(asset_url, MAX_CHECKSUM_BYTES, "published SHA256SUMS",
                         "application/octet-stream")
    if public != local:
        raise PublicationError("baseline SHA256SUMS differs from the published release asset")
    return hashlib.sha256(public).hexdigest()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets-dir", required=True)
    parser.add_argument("--release-tag", required=True)
    args = parser.parse_args(argv)
    try:
        digest = published_digest(args.assets_dir, args.release_tag)
    except PublicationError as error:
        print(f"baseline publication: {error}", file=sys.stderr)
        return 1
    print(digest)
    return 0


if __name__ == "__main__":
    sys.exit(main())
