#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check what the tensorplate-python wheel and sdist hold.

The wheel's `tensorplate` package must be the files of --source (the SDK's
`src` directory) byte for byte, plus the three modules its build generates
from the stream session schema, and the wheel must hold nothing else beside
its metadata. The sdist must hold `setup.py` and the schema as --schema has
it, and no generated module: it is what builds the wheel, with no repository
around it.
"""

import argparse
import fnmatch
import sys
import tarfile
import zipfile
from pathlib import Path

PACKAGE = "tensorplate"
SCHEMA = "tensorplate/stream/v1/session.proto"
GENERATED = tuple(
    f"tensorplate/stream/v1/session_{suffix}" for suffix in ("pb2.py", "pb2.pyi", "pb2_grpc.py")
)
# The names sdk/python/MANIFEST.in keeps out of an sdist.
GENERATED_PATTERNS = ("*_pb2.py", "*_pb2.pyi", "*_pb2_grpc.py")


def is_generated(name):
    return any(fnmatch.fnmatch(name.rsplit("/", 1)[-1], pattern) for pattern in GENERATED_PATTERNS)


def source_files(source):
    """The package's files under `source`, less what an editable install generates there."""
    files = {}
    for path in sorted((source / PACKAGE).rglob("*")):
        name = path.relative_to(source).as_posix()
        if path.is_file() and "__pycache__" not in path.parts and not is_generated(name):
            files[name] = path.read_bytes()
    if not files:
        raise SystemExit(f"{source} holds no {PACKAGE} package")
    return files


def check_wheel(wheel, source):
    expected = source_files(source)
    with zipfile.ZipFile(wheel) as archive:
        held = {name: archive.read(name) for name in archive.namelist() if not name.endswith("/")}
    problems = []
    for name in sorted(held):
        top = name.split("/", 1)[0]
        if top == PACKAGE:
            if name not in expected and name not in GENERATED:
                problems.append(f"holds {name}, neither a source file nor a generated module")
            elif name in expected and held[name] != expected[name]:
                problems.append(f"{name} differs from the source file")
        elif not top.endswith(".dist-info"):
            problems.append(f"holds {name}, outside the package and its metadata")
    for name in sorted({*expected, *GENERATED} - set(held)):
        problems.append(f"lacks {name}")
    return [f"{wheel.name}: {problem}" for problem in problems]


def check_sdist(sdist, schema):
    with tarfile.open(sdist) as archive:
        files = {member.name: member for member in archive.getmembers() if member.isfile()}
        tops = sorted({name.split("/", 1)[0] for name in files})
        if len(tops) != 1:
            return [f"{sdist.name}: expected one top-level directory, found {tops}"]
        held = {name.split("/", 1)[1]: member for name, member in files.items() if "/" in name}
        problems = []
        if "setup.py" not in held:
            problems.append("lacks setup.py, which generates the stream modules")
        carried = held.get(f"proto/{SCHEMA}")
        if carried is None:
            problems.append(f"lacks proto/{SCHEMA}")
        elif archive.extractfile(carried).read() != schema.read_bytes():
            problems.append(f"proto/{SCHEMA} differs from {schema}")
        for name in sorted(held):
            if is_generated(name):
                problems.append(f"holds the generated module {name}")
    return [f"{sdist.name}: {problem}" for problem in problems]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--wheel", type=Path, required=True)
    parser.add_argument("--sdist", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True, help="the SDK's src directory")
    parser.add_argument("--schema", type=Path, required=True, help="the committed session.proto")
    args = parser.parse_args(argv)

    problems = check_wheel(args.wheel, args.source) + check_sdist(args.sdist, args.schema)
    for problem in problems:
        print(f"FAIL: {problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"{args.wheel.name} and {args.sdist.name} hold the package and the stream schema's modules")
    return 0


if __name__ == "__main__":
    sys.exit(main())
