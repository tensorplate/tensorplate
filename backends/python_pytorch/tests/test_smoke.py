"""V01-E01-F01 smoke tests for ``tensorplate-pytorch-backend``.

Verifies the package imports cleanly, the version constants are
populated, and the cross-language version surface agrees with the C++
runtime and Rust protocol crate. Real backend coverage lands in V01-E05.
"""

from __future__ import annotations

import re

import tensorplate_pytorch_backend as tp_pt


def test_skeleton_marker_is_stable() -> None:
    assert tp_pt.SKELETON_MARKER == "tensorplate-pytorch-backend-skeleton"


def test_version_is_populated() -> None:
    assert tp_pt.__version__
    # PEP 440 release segment + optional dev/pre/post; we just check the
    # leading numeric triple to keep the assertion stable across bumps.
    assert re.match(r"^\d+\.\d+\.\d+", tp_pt.__version__)


def test_protocol_version_format() -> None:
    assert re.fullmatch(r"\d+\.\d+", tp_pt.PROTOCOL_VERSION)


def test_bundle_format_version_format() -> None:
    assert re.fullmatch(r"\d+\.\d+", tp_pt.BUNDLE_FORMAT_VERSION)


def test_protocol_and_bundle_constants_are_independent() -> None:
    # The two surfaces evolve on independent cadences per
    # docs/architecture/versioning.md; v0.1.0 happens to ship both at
    # 0.1, but they must stay separately addressable.
    assert tp_pt.PROTOCOL_VERSION
    assert tp_pt.BUNDLE_FORMAT_VERSION
