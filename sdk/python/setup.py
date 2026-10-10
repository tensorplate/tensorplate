"""Build steps that pyproject.toml cannot express: the stream schema's modules.

A wheel carries ``tensorplate/stream/v1/session_pb2.py``, ``session_pb2.pyi``
and ``session_pb2_grpc.py``, generated from the stream session schema by the
``grpcio-tools`` the build requirements pin. They are never committed. The
schema is the repository's ``protocol/proto``; an sdist carries its own copy
under ``proto/``, so it builds a wheel with no repository around it.
"""

from __future__ import annotations

import shutil
from pathlib import Path

from setuptools import setup
from setuptools.command.build_py import build_py
from setuptools.command.sdist import sdist
from setuptools.errors import SetupError

_HERE = Path(__file__).resolve().parent
_SCHEMA = "tensorplate/stream/v1/session.proto"
_SDIST_ROOT = _HERE / "proto"
_REPOSITORY_ROOT = _HERE.parent.parent / "protocol" / "proto"


def _schema_root() -> Path:
    for root in (_SDIST_ROOT, _REPOSITORY_ROOT):
        if (root / _SCHEMA).is_file():
            return root
    raise SetupError(f"{_SCHEMA} is under neither {_SDIST_ROOT} nor {_REPOSITORY_ROOT}")


def _generate(packages: Path) -> None:
    from grpc_tools import protoc

    outputs = [f"--{kind}_out={packages}" for kind in ("python", "pyi", "grpc_python")]
    if protoc.main(["protoc", f"--proto_path={_schema_root()}", *outputs, _SCHEMA]) != 0:
        raise SetupError(f"protoc generated no modules from {_SCHEMA}")


class BuildPy(build_py):
    def run(self) -> None:
        super().run()
        # An editable install serves the package from src/, so the modules go there.
        _generate(_HERE / "src" if self.editable_mode else Path(self.build_lib))


class Sdist(sdist):
    def make_release_tree(self, base_dir: str, files: list[str]) -> None:
        super().make_release_tree(base_dir, files)
        carried = Path(base_dir, "proto", _SCHEMA)
        carried.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(_schema_root() / _SCHEMA, carried)


setup(cmdclass={"build_py": BuildPy, "sdist": Sdist})
