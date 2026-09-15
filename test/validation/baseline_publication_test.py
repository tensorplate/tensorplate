#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check baseline publication with synthetic HTTP responses, never the network."""

import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import sys
import tempfile
import unittest
import urllib.error
from unittest import mock

sys.dont_write_bytecode = True
HELPER = pathlib.Path(__file__).resolve().parents[2] / "tools/validation/check-baseline-publication.py"
SPEC = importlib.util.spec_from_file_location("baseline_publication", HELPER)
publication = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publication)

TAG = "v0.2.1-rc.1"
API_URL = "https://api.github.com/repos/tensorplate/tensorplate/releases/tags/" + TAG
ASSET_URL = "https://github.com/tensorplate/tensorplate/releases/download/" + TAG + "/SHA256SUMS"
CHECKSUMS = ("0" * 64 + "  install.sh\n").encode()


class Response(io.BytesIO):
    def __init__(self, body, url):
        super().__init__(body)
        self.url = url

    def geturl(self):
        return self.url


class BaselinePublicationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="tp-baseline-publication-")
        self.addCleanup(self.directory.cleanup)
        self.assets = pathlib.Path(self.directory.name)
        (self.assets / "SHA256SUMS").write_bytes(CHECKSUMS)
        self.metadata = {
            "tag_name": TAG,
            "draft": False,
            "prerelease": True,
            "published_at": "2026-09-01T00:00:00Z",
            "assets": [{"name": "SHA256SUMS", "state": "uploaded",
                        "browser_download_url": ASSET_URL}],
        }

    def invoke(self, *, metadata=None, checksums=CHECKSUMS, error_at=None,
               error=None, redirect=None, tag=TAG):
        metadata = self.metadata if metadata is None else metadata
        metadata_bytes = metadata if isinstance(metadata, bytes) else json.dumps(metadata).encode()
        requests = []

        def urlopen(request, timeout):
            # A token in the process environment must never make a private
            # or draft release visible to this public-release check.
            self.assertNotIn("authorization", {key.lower() for key, _ in request.header_items()})
            self.assertGreater(timeout, 0)
            self.assertLessEqual(timeout, 20)
            requests.append(request.full_url)
            self.assertIn(request.full_url, (API_URL, ASSET_URL))
            if request.full_url == error_at:
                raise error
            body = metadata_bytes if request.full_url == API_URL else checksums
            url = redirect if request.full_url == ASSET_URL and redirect else request.full_url
            return Response(body, url)

        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch("urllib.request.urlopen", side_effect=urlopen), \
                mock.patch.dict(os.environ, {"GH_TOKEN": "synthetic-token", "GITHUB_TOKEN": "synthetic-token"}), \
                contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = publication.main(["--assets-dir", str(self.assets), "--release-tag", tag])
        return status, stdout.getvalue(), stderr.getvalue(), requests

    def assert_refused(self, result):
        status, stdout, stderr, _ = result
        self.assertEqual(status, 1, result)
        self.assertEqual(stdout, "", result)
        self.assertIn("baseline publication:", stderr, result)

    def test_published_matching_candidate_returns_public_digest(self):
        status, stdout, stderr, requests = self.invoke()
        self.assertEqual(status, 0, stderr)
        self.assertEqual(stdout, hashlib.sha256(CHECKSUMS).hexdigest() + "\n")
        self.assertEqual(requests, [API_URL, ASSET_URL])

    def test_https_asset_storage_redirect_is_allowed(self):
        result = self.invoke(redirect="https://release-assets.githubusercontent.com/synthetic/SHA256SUMS")
        self.assertEqual(result[0], 0, result)

    def test_draft_or_unpublished_release_is_refused(self):
        for key, value in (("draft", True), ("draft", None), ("published_at", None),
                           ("published_at", ""), ("published_at", "not a date")):
            with self.subTest(key=key, value=value):
                metadata = copy.deepcopy(self.metadata)
                metadata[key] = value
                result = self.invoke(metadata=metadata)
                self.assert_refused(result)
                self.assertEqual(result[3], [API_URL])

    def test_release_tag_must_match_exactly(self):
        metadata = copy.deepcopy(self.metadata)
        metadata["tag_name"] = "v0.2.1-rc.2"
        self.assert_refused(self.invoke(metadata=metadata))

    def test_release_missing_from_public_api_is_refused(self):
        result = self.invoke(error_at=API_URL,
                             error=urllib.error.HTTPError(API_URL, 404, "Not Found", None, None))
        self.assert_refused(result)
        self.assertEqual(result[3], [API_URL])

    def test_checksum_asset_must_be_unique_uploaded_and_canonical(self):
        for change in ("missing", "duplicate", "uploading", "foreign-url"):
            with self.subTest(change=change):
                metadata = copy.deepcopy(self.metadata)
                if change == "missing":
                    metadata["assets"] = []
                elif change == "duplicate":
                    metadata["assets"] *= 2
                elif change == "uploading":
                    metadata["assets"][0]["state"] = "new"
                else:
                    metadata["assets"][0]["browser_download_url"] = "https://example.invalid/SHA256SUMS"
                result = self.invoke(metadata=metadata)
                self.assert_refused(result)
                self.assertEqual(result[3], [API_URL])

    def test_local_checksum_bytes_must_match_public_asset(self):
        self.assert_refused(self.invoke(checksums=CHECKSUMS + b"\n"))

    def test_network_failure_on_either_request_is_refused(self):
        for url in (API_URL, ASSET_URL):
            with self.subTest(url=url):
                self.assert_refused(self.invoke(error_at=url,
                                               error=urllib.error.URLError("synthetic network failure")))

    def test_plain_http_asset_redirect_is_refused(self):
        self.assert_refused(self.invoke(redirect="http://example.invalid/SHA256SUMS"))

    def test_malformed_metadata_is_refused(self):
        for metadata in (b"{", b"[]", b"null"):
            with self.subTest(metadata=metadata):
                self.assert_refused(self.invoke(metadata=metadata))

    def test_responses_are_bounded(self):
        self.assert_refused(self.invoke(metadata=b" " * (2 * 1024 * 1024 + 1)))
        self.assert_refused(self.invoke(checksums=b" " * (1024 * 1024 + 1)))

    def test_missing_local_checksums_is_refused(self):
        (self.assets / "SHA256SUMS").unlink()
        self.assert_refused(self.invoke())

    def test_invalid_tag_is_refused_before_any_request(self):
        for tag in ("", "../other", "v0.2.1-rc.1?draft=true"):
            with self.subTest(tag=tag):
                result = self.invoke(tag=tag)
                self.assert_refused(result)
                self.assertEqual(result[3], [])


if __name__ == "__main__":
    unittest.main()
