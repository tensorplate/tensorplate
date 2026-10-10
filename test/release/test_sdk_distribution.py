#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise the SDK distribution checker on archives built here, and its use by the release job.

The archives are stand-ins with the layout of a real wheel and sdist. The SDK's
own suite runs the checker on a real build (sdk/python/tests/test_distribution.py).
"""

from __future__ import annotations

import io
import pathlib
import re
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
CHECKER = ROOT / "tools/release/check-sdk-distribution.py"
RELEASE_WORKFLOW = ROOT / ".github/workflows/release.yml"

SCHEMA = b'syntax = "proto3";\npackage tensorplate.stream.v1;\n'
SOURCE = {
    "tensorplate/__init__.py": b'"""Stand-in package."""\n',
    "tensorplate/py.typed": b"",
    "tensorplate/stream/__init__.py": b"",
    "tensorplate/stream/v1/__init__.py": b"",
}
GENERATED = {
    "tensorplate/stream/v1/session_pb2.py": b"# generated messages\n",
    "tensorplate/stream/v1/session_pb2.pyi": b"# generated stubs\n",
    "tensorplate/stream/v1/session_pb2_grpc.py": b"# generated service\n",
}
WHEEL = {**SOURCE, **GENERATED, "tensorplate_python-0.0.0.dist-info/METADATA": b"Name: x\n"}
TOP = "tensorplate_python-0.0.0"
SDIST = {
    f"{TOP}/setup.py": b"# stand-in\n",
    f"{TOP}/proto/tensorplate/stream/v1/session.proto": SCHEMA,
    **{f"{TOP}/src/{name}": data for name, data in SOURCE.items()},
}


def without(files: dict[str, bytes], name: str) -> dict[str, bytes]:
    assert name in files, name
    return {key: data for key, data in files.items() if key != name}


# (what is wrong, the wheel's files, the sdist's files, what the checker must say)
REFUSALS = [
    *(
        (f"wheel without {name}", without(WHEEL, name), SDIST, f".whl: lacks {name}")
        for name in (*GENERATED, "tensorplate/py.typed", "tensorplate/stream/v1/__init__.py")
    ),
    (
        "wheel with a changed source file",
        {**WHEEL, "tensorplate/__init__.py": b"changed\n"},
        SDIST,
        ".whl: tensorplate/__init__.py differs from the source file",
    ),
    (
        "wheel with a module the source does not have",
        {**WHEEL, "tensorplate/stream/v1/other_pb2.py": b""},
        SDIST,
        ".whl: holds tensorplate/stream/v1/other_pb2.py, neither a source file nor",
    ),
    (
        "wheel with a file outside the package",
        {**WHEEL, "proto/tensorplate/stream/v1/session.proto": SCHEMA},
        SDIST,
        ".whl: holds proto/tensorplate/stream/v1/session.proto, outside the package",
    ),
    (
        "sdist without setup.py",
        WHEEL,
        without(SDIST, f"{TOP}/setup.py"),
        ".tar.gz: lacks setup.py",
    ),
    (
        "sdist without the schema",
        WHEEL,
        without(SDIST, f"{TOP}/proto/tensorplate/stream/v1/session.proto"),
        ".tar.gz: lacks proto/tensorplate/stream/v1/session.proto",
    ),
    (
        "sdist with another schema",
        WHEEL,
        {**SDIST, f"{TOP}/proto/tensorplate/stream/v1/session.proto": SCHEMA + b"// stale\n"},
        ".tar.gz: proto/tensorplate/stream/v1/session.proto differs from",
    ),
    (
        "sdist with a generated module",
        WHEEL,
        {**SDIST, f"{TOP}/src/tensorplate/stream/v1/session_pb2_grpc.py": b"# stale\n"},
        ".tar.gz: holds the generated module src/tensorplate/stream/v1/session_pb2_grpc.py",
    ),
    (
        "sdist with two top-level directories",
        WHEEL,
        {**SDIST, "other/setup.py": b""},
        ".tar.gz: expected one top-level directory",
    ),
]


class SdkDistributionCheckerTests(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.scratch = pathlib.Path(scratch.name)
        self.schema = self.scratch / "session.proto"
        self.schema.write_bytes(SCHEMA)
        self.source = self.scratch / "src"
        for name, data in SOURCE.items():
            (self.source / name).parent.mkdir(parents=True, exist_ok=True)
            (self.source / name).write_bytes(data)

    def check(self, wheel: dict[str, bytes], sdist: dict[str, bytes]) -> tuple[int, str]:
        wheel_path = self.scratch / "tensorplate_python-0.0.0-py3-none-any.whl"
        with zipfile.ZipFile(wheel_path, "w") as archive:
            for name, data in wheel.items():
                archive.writestr(name, data)
        sdist_path = self.scratch / f"{TOP}.tar.gz"
        with tarfile.open(sdist_path, "w:gz") as archive:
            for name, data in sdist.items():
                member = tarfile.TarInfo(name)
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
        command = [sys.executable, str(CHECKER), "--wheel", str(wheel_path)]
        command += ["--sdist", str(sdist_path), "--source", str(self.source)]
        result = subprocess.run(
            [*command, "--schema", str(self.schema)], capture_output=True, text=True, check=False
        )
        return result.returncode, result.stderr

    def test_accepts_the_source_package_with_the_generated_modules(self) -> None:
        self.assertEqual(self.check(WHEEL, SDIST), (0, ""))

    def test_ignores_what_an_editable_install_and_an_import_leave_in_the_source(self) -> None:
        for name, data in GENERATED.items():
            (self.source / name).write_bytes(b"# stale\n" + data)
        cache = self.source / "tensorplate/__pycache__"
        cache.mkdir()
        (cache / "__init__.cpython-312.pyc").write_bytes(b"\0")
        self.assertEqual(self.check(WHEEL, SDIST), (0, ""))

    def test_refuses_each_wrong_archive_and_names_what_is_wrong(self) -> None:
        for label, wheel, sdist, message in REFUSALS:
            with self.subTest(label):
                status, errors = self.check(wheel, sdist)
                self.assertEqual(status, 1, errors)
                self.assertEqual(errors.count("FAIL: "), 1, errors)
                self.assertIn(message, errors)

    def test_refuses_a_source_directory_without_the_package(self) -> None:
        self.source = self.scratch / "elsewhere"
        self.source.mkdir()
        status, errors = self.check(WHEEL, SDIST)
        self.assertNotEqual(status, 0)
        self.assertIn("holds no tensorplate package", errors)


class ReleaseJobTests(unittest.TestCase):
    def test_the_sdk_job_pins_its_build_environment_and_checks_what_it_built(self) -> None:
        jobs = re.split(r"(?m)^  (?=[a-z_]+:$)", RELEASE_WORKFLOW.read_text(encoding="utf-8"))
        [job] = [text for text in jobs if text.startswith("build_sdk_python:")]
        build = re.search(
            r'PIP_CONSTRAINT="\$\{PWD\}/sdk/python/constraints/build\.txt" \\\n'
            r" +\.sdkbuild/bin/python -m build sdk/python ",
            job,
        )
        self.assertIsNotNone(build, "the build does not install from the hashed build pins")
        check = job.index("tools/release/check-sdk-distribution.py")
        self.assertLess(build.end(), check)
        for argument in (
            "--wheel dist/sdk-python/tensorplate_python-*.whl",
            "--sdist dist/sdk-python/tensorplate_python-*.tar.gz",
            "--source sdk/python/src",
            "--schema protocol/proto/tensorplate/stream/v1/session.proto",
        ):
            self.assertIn(argument, job[check:])


if __name__ == "__main__":
    unittest.main()
