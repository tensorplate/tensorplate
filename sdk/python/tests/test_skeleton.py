"""Import smoke and public-export checks for the tensorplate SDK skeleton."""

from __future__ import annotations

import re

import tensorplate

EXPECTED_EXPORTS = ("Detection", "ServingClient", "TensorPlateError", "VisionClient")


def test_package_imports() -> None:
    assert tensorplate.__name__ == "tensorplate"


def test_version_is_populated() -> None:
    assert isinstance(tensorplate.__version__, str)
    # PEP 440 release segment (plus optional dev/pre/post); assert only the
    # leading numeric triple so the check survives version bumps.
    assert re.match(r"^\d+\.\d+\.\d+", tensorplate.__version__)


def test_public_exports_are_present() -> None:
    for name in EXPECTED_EXPORTS:
        assert name in tensorplate.__all__
        assert hasattr(tensorplate, name)


def test_exported_classes_are_classes() -> None:
    assert isinstance(tensorplate.ServingClient, type)
    assert isinstance(tensorplate.VisionClient, type)
    assert isinstance(tensorplate.Detection, type)


def test_error_base_is_exception() -> None:
    assert issubclass(tensorplate.TensorPlateError, Exception)
