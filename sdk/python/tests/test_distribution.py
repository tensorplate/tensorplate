"""What a build of the package produces, and the pins of its build environment.

The build test needs git, the repository around the package and the build
requirements installed from ``constraints/build.txt``; it skips without them.
The CI job that installs the pins sets ``TENSORPLATE_REQUIRE_SPEECH_EXTRAS=1``,
which turns their absence into a failure.
"""

from __future__ import annotations

import importlib.util
import os
import re
import shutil
import subprocess
import sys
import zipfile
from importlib import metadata
from pathlib import Path

import pytest
from packaging.requirements import Requirement
from packaging.utils import canonicalize_name

import tensorplate
from test_speech_import import _closure, _pins

_SDK = Path(__file__).resolve().parents[1]
_REPOSITORY = _SDK.parents[1]
_BUILD_PINS = _SDK / "constraints" / "build.txt"
_CHECKER = "tools/release/check-sdk-distribution.py"
_SCHEMA = "protocol/proto/tensorplate/stream/v1/session.proto"
_GENERATED = ("session_pb2.py", "session_pb2.pyi", "session_pb2_grpc.py")
_REQUIRED = os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1"


def _unless_required(reason: str) -> None:
    assert not _REQUIRED, reason
    pytest.skip(reason)


def _tracked_or_new(*paths: str) -> list[str]:
    """The files under `paths` that git tracks or would add, so nothing it ignores."""
    if shutil.which("git") is None or not (_REPOSITORY / ".git").exists():
        _unless_required("not a git checkout")
    listed = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "--", *paths],
        cwd=_REPOSITORY,
        capture_output=True,
        text=True,
        check=True,
    )
    return [name for name in listed.stdout.splitlines() if (_REPOSITORY / name).is_file()]


def _build_requirements() -> dict[str, Requirement]:
    declared = re.search(
        r"(?m)^requires = \[(.*)\]$", (_SDK / "pyproject.toml").read_text(encoding="utf-8")
    )
    assert declared, "pyproject.toml declares no build requirements on one line"
    requirements = [Requirement(item) for item in re.findall(r'"([^"]+)"', declared[1])]
    return {canonicalize_name(requirement.name): requirement for requirement in requirements}


def test_git_holds_and_would_add_no_generated_module() -> None:
    package = _tracked_or_new("sdk/python")
    assert "sdk/python/setup.py" in package
    assert [name for name in package if "_pb2" in name] == []


def test_the_installed_package_holds_the_generated_modules() -> None:
    package = Path(tensorplate.__file__).parent / "stream" / "v1"
    assert sorted(path.name for path in package.glob("session_pb2*")) == sorted(_GENERATED)


def test_the_build_pins_hold_the_build_requirements_at_the_speech_pins_versions() -> None:
    pins = _pins(_BUILD_PINS)
    requirements = _build_requirements()
    for name, requirement in requirements.items():
        assert requirement.specifier.contains(pins[name]), f"{requirement} excludes its pin"
    # The generator is the release with the number of the pinned grpcio.
    assert str(requirements["grpcio-tools"].specifier) == f"=={pins['grpcio']}"
    speech = _pins()
    assert {name: pins.get(name) for name in speech} == speech


def test_the_build_pins_are_the_build_requirements_installed_closure() -> None:
    pins = _pins(_BUILD_PINS)
    if importlib.util.find_spec("grpc_tools") is None:
        _unless_required("the build requirements are not installed here")
    assert set(pins) == _closure(set(_build_requirements()))
    assert {name: metadata.version(name) for name in pins} == pins


def test_the_sdist_builds_the_wheel_with_no_repository_and_no_index(
    tmp_path_factory: pytest.TempPathFactory,
) -> None:
    for module in ("grpc_tools", "setuptools", "pip"):
        if importlib.util.find_spec(module) is None:
            _unless_required(f"{module} is not installed here")
    tree = tmp_path_factory.mktemp("tree")
    for name in _tracked_or_new("sdk/python", "protocol/proto", _CHECKER):
        (tree / name).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(_REPOSITORY / name, tree / name)
    # What an editable install leaves in the source tree must not reach either archive.
    for name in _GENERATED:
        (tree / "sdk/python/src/tensorplate/stream/v1" / name).write_text(
            "# left by an earlier build\n"
        )
    dist = tmp_path_factory.mktemp("dist")
    build_sdist = (
        "import sys; from setuptools import build_meta; build_meta.build_sdist(sys.argv[1])"
    )
    subprocess.run(
        [sys.executable, "-c", build_sdist, str(dist)], cwd=tree / "sdk/python", check=True
    )
    [sdist] = dist.glob("tensorplate_python-*.tar.gz")

    # pip unpacks the sdist into a temporary directory of its own and builds there.
    offline = ["--no-index", "--no-build-isolation", "--no-deps"]
    subprocess.run(
        [sys.executable, "-m", "pip", "wheel", *offline, "--wheel-dir", str(dist), str(sdist)],
        cwd=tmp_path_factory.mktemp("clean"),
        check=True,
    )
    [wheel] = dist.glob("tensorplate_python-*.whl")

    command = [sys.executable, str(tree / _CHECKER), "--wheel", str(wheel), "--sdist", str(sdist)]
    command += ["--source", str(tree / "sdk/python/src"), "--schema", str(tree / _SCHEMA)]
    checked = subprocess.run(command, capture_output=True, text=True, check=False)
    assert checked.returncode == 0, checked.stderr

    # A wheel built from the tree itself generates over what was left there.
    direct = tmp_path_factory.mktemp("direct")
    subprocess.run(
        [sys.executable, "-m", "pip", "wheel", *offline, "--wheel-dir", str(direct), "."],
        cwd=tree / "sdk/python",
        check=True,
    )
    [from_tree] = direct.glob("tensorplate_python-*.whl")
    with zipfile.ZipFile(wheel) as from_sdist, zipfile.ZipFile(from_tree) as other:
        for name in _GENERATED:
            generated = from_sdist.read(f"tensorplate/stream/v1/{name}")
            assert b"left by an earlier build" not in generated, name
            assert other.read(f"tensorplate/stream/v1/{name}") == generated, name
