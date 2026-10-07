"""The speech package's dependency guard, and the pins its extra installs from.

Passes with or without the ``speech`` extra installed. The CI job that
installs the pins sets ``TENSORPLATE_REQUIRE_SPEECH_EXTRAS=1``, which turns
their absence into a failure. The extra is read from the installed package
metadata, so reinstall after editing ``pyproject.toml``.
"""

from __future__ import annotations

import importlib
import os
import re
import subprocess
import sys
from collections.abc import Iterator, Sequence
from importlib import metadata
from importlib.abc import MetaPathFinder
from importlib.machinery import ModuleSpec
from pathlib import Path
from types import ModuleType

import pytest
from packaging.requirements import Requirement
from packaging.utils import canonicalize_name

import tensorplate
from tensorplate.errors import MissingDependencyError, TensorPlateError

_PINS = Path(__file__).resolve().parents[1] / "constraints" / "speech.txt"
_REQUIRE_EXTRA = os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1"
_PIN = re.compile(r"(?P<name>[A-Za-z0-9][A-Za-z0-9._-]*)==(?P<version>[0-9][0-9A-Za-z.]*)")
_HASH = re.compile(r"--hash=sha256:[0-9a-f]{64}")
_INSTALL_HINT = 'pip install "tensorplate-python[speech]"'


class _Absent(MetaPathFinder):
    """Reports one module as not installed, whatever the environment holds."""

    def __init__(self, name: str) -> None:
        self._name = name

    def find_spec(
        self, fullname: str, path: Sequence[str] | None, target: ModuleType | None = None
    ) -> ModuleSpec | None:
        if fullname == self._name:
            raise ModuleNotFoundError(f"No module named {fullname!r}", name=fullname)
        return None


def _declared(distribution: str) -> list[Requirement]:
    return [Requirement(text) for text in metadata.requires(distribution) or []]


def _extra() -> dict[str, Requirement]:
    return {
        canonicalize_name(req.name): req
        for req in _declared("tensorplate-python")
        if req.marker is not None and req.marker.evaluate({"extra": "speech"})
    }


def _closure(names: set[str]) -> set[str]:
    """The named installed distributions and everything they require without an extra."""
    pending = set(names)
    while pending:
        for req in _declared(pending.pop()):
            name = canonicalize_name(req.name)
            if name not in names and (req.marker is None or req.marker.evaluate({"extra": ""})):
                names.add(name)
                pending.add(name)
    return names


def _pins() -> dict[str, str]:
    pins: dict[str, str] = {}
    for line in _PINS.read_text(encoding="utf-8").replace("\\\n", " ").splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        spec, *hashes = line.split()
        pin = _PIN.fullmatch(spec)
        assert pin, f"not an exact pin: {spec!r}"
        assert hashes, f"{spec} has no hash"
        assert all(_HASH.fullmatch(item) for item in hashes), f"{spec}: not only SHA-256 hashes"
        name = canonicalize_name(pin["name"])
        assert name not in pins, f"{name} is pinned twice"
        pins[name] = pin["version"]
    return pins


def _installed(distribution: str) -> bool:
    try:
        metadata.version(distribution)
    except metadata.PackageNotFoundError:
        return False
    return True


def _import_speech() -> ModuleType:
    sys.modules.pop("tensorplate.speech", None)
    return importlib.import_module("tensorplate.speech")


@pytest.fixture
def stand_ins(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    """Stand-in dependency modules, so a case decides exactly which one is missing."""
    for name in ("grpc", "google", "google.protobuf"):
        module = ModuleType(name)
        module.__path__ = []
        monkeypatch.setitem(sys.modules, name, module)
    yield
    sys.modules.pop("tensorplate.speech", None)


def test_the_default_install_declares_no_dependency() -> None:
    unconditional = [
        str(req)
        for req in _declared("tensorplate-python")
        if req.marker is None or "extra" not in str(req.marker)
    ]
    assert unconditional == []


def test_the_error_is_exported_from_the_package() -> None:
    assert tensorplate.MissingDependencyError is MissingDependencyError
    assert "MissingDependencyError" in tensorplate.__all__


def test_the_extra_is_grpcio_and_protobuf_pinned_at_their_floors() -> None:
    pins = _pins()
    extra = _extra()
    assert sorted(extra) == ["grpcio", "protobuf"]
    for name, requirement in extra.items():
        floors = [spec.version for spec in requirement.specifier if spec.operator == ">="]
        assert floors == [pins[name]], f"{name}: floor {floors} is not the pin {pins[name]}"
        assert requirement.specifier.contains(pins[name])
        caps = [spec.version for spec in requirement.specifier if spec.operator == "<"]
        assert caps == [str(int(pins[name].split(".")[0]) + 1)], f"{name}: cap {caps}"


@pytest.mark.usefixtures("stand_ins")
def test_import_succeeds_when_both_dependencies_import() -> None:
    assert _import_speech().__name__ == "tensorplate.speech"


@pytest.mark.usefixtures("stand_ins")
def test_the_guard_checks_every_distribution_the_extra_declares() -> None:
    assert sorted(_import_speech()._REQUIRED.values()) == sorted(_extra())


@pytest.mark.usefixtures("stand_ins")
@pytest.mark.parametrize(
    ("absent", "module", "dependency"),
    [
        ("grpc", "grpc", "grpcio"),
        ("google.protobuf", "google.protobuf", "protobuf"),
        ("google", "google.protobuf", "protobuf"),
    ],
)
def test_import_without_a_dependency_raises_the_typed_error(
    monkeypatch: pytest.MonkeyPatch, absent: str, module: str, dependency: str
) -> None:
    for name in ("grpc", "google", "google.protobuf"):
        if f"{name}.".startswith(f"{absent}."):
            monkeypatch.delitem(sys.modules, name)
    monkeypatch.setattr(sys, "meta_path", [_Absent(absent), *sys.meta_path])

    with pytest.raises(MissingDependencyError) as caught:
        _import_speech()

    error = caught.value
    assert isinstance(error, TensorPlateError)
    assert isinstance(error, ImportError)
    assert (error.extra, error.dependency, error.name) == ("speech", dependency, module)
    assert dependency in str(error)
    assert _INSTALL_HINT in str(error)
    assert isinstance(error.__cause__, ModuleNotFoundError)


@pytest.mark.usefixtures("stand_ins")
@pytest.mark.parametrize(
    ("body", "name"),
    [
        ("import grpc_absent_sibling", "grpc_absent_sibling"),
        ("import grpc._absent_submodule", "grpc._absent_submodule"),
        ("raise ModuleNotFoundError('a prefix of the name', name='grp')", "grp"),
        ("raise ModuleNotFoundError('no module named')", None),
        ("raise ImportError('broken extension module', name='grpc')", "grpc"),
    ],
)
def test_a_dependency_that_fails_inside_its_own_import_is_not_called_missing(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, body: str, name: str | None
) -> None:
    (tmp_path / "grpc").mkdir()
    (tmp_path / "grpc" / "__init__.py").write_text(f"{body}\n", encoding="utf-8")
    monkeypatch.syspath_prepend(str(tmp_path))
    monkeypatch.delitem(sys.modules, "grpc")

    with pytest.raises(ImportError) as caught:
        _import_speech()

    assert not isinstance(caught.value, MissingDependencyError)
    assert caught.value.name == name


def test_the_real_import_agrees_with_what_is_installed() -> None:
    installed = all(_installed(name) for name in _extra())
    assert installed or not _REQUIRE_EXTRA, "this job must install the speech pins"

    result = subprocess.run(
        [sys.executable, "-c", "import tensorplate.speech"],
        capture_output=True,
        text=True,
        check=False,
    )

    if installed:
        assert result.returncode == 0, result.stderr
    else:
        assert result.returncode != 0
        assert "MissingDependencyError" in result.stderr
        assert _INSTALL_HINT in result.stderr


@pytest.mark.skipif(not _REQUIRE_EXTRA, reason="only the job that installs the pins")
def test_the_pins_are_the_extras_installed_closure_at_the_pinned_versions() -> None:
    pins = _pins()
    assert set(pins) == _closure(set(_extra()))
    assert {name: metadata.version(name) for name in pins} == pins
