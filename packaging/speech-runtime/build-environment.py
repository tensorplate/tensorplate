#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Build the speech runtime environment from a hash-locked wheelhouse.

  fetch  download every locked artifact into a wheelhouse. The only step
         that uses the network; it skips what the wheelhouse already holds.
  build  install the wheelhouse into a virtual environment rooted at
         /usr/lib/tensorplate/speech-runtime and split it into one package
         tree per lock file. No network.

A lock directory holds one `<component>.txt` per package, each line
`name==version --hash=sha256:<digest>`; `index.txt`, the pip index options
for `fetch`; `sources.txt`, the pins that come from somewhere other than an
index; and optionally `self-test.py`, run inside the built environment, and
`undeclared-licenses.txt`, the distributions accepted without a declared
license, one `name reason` per line.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

ENV_ROOT = "/usr/lib/tensorplate/speech-runtime"
PACKAGE_PREFIX = "tensorplate-speech-runtime-"
BASE = "base"
SIDECAR = "tensorplate-pytorch-backend"
# Wheels built here are pinned by hash, so their archive timestamps are fixed.
BUILD_EPOCH = "315532800"
# What `python -m venv` creates and no distribution's RECORD lists.
VENV_FILES = {"pyvenv.cfg", "bin/python", "bin/python3", "bin/python3.12", "lib64"}
PIN = re.compile(r"^([A-Za-z0-9][A-Za-z0-9._-]*)==(\S+) --hash=sha256:([0-9a-f]{64})$")
SOURCE_KINDS = {"sdist", "wheel", "tree"}
NOT_COMPONENTS = {"index.txt", "sources.txt", "undeclared-licenses.txt"}
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying)", re.IGNORECASE)
INTERPRETER = "/usr/bin/python3.12"


class BuildError(Exception):
    pass


def normalize(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def run(argv: list[str], env: dict[str, str] | None = None) -> None:
    print("+", " ".join(argv), flush=True)
    result = subprocess.run(argv, check=False, env=env)
    if result.returncode != 0:
        step = next((Path(a).name for a in argv[1:] if not a.startswith("-")), argv[0])
        raise BuildError(f"{step} exited {result.returncode}")


class Lock:
    """The pins of a lock directory, by component."""

    def __init__(self, directory: Path) -> None:
        self.directory = directory
        self.pins: dict[str, tuple[str, str, str, str]] = {}
        self.components: list[str] = []
        self.sources: dict[str, tuple[str, str, str]] = {}
        self.undeclared: dict[str, str] = {}
        for path in sorted(directory.glob("*.txt")):
            if path.name in NOT_COMPONENTS:
                continue
            self.components.append(path.stem)
            for number, line in self._lines(path):
                match = PIN.match(line)
                if not match:
                    raise BuildError(f"{path}:{number}: not `name==version --hash=sha256:<digest>`")
                name = normalize(match.group(1))
                if name in self.pins:
                    raise BuildError(f"{path}:{number}: {name} is pinned twice")
                self.pins[name] = (match.group(1), match.group(2), match.group(3), path.stem)
        if BASE not in self.components:
            raise BuildError(f"{directory} has no {BASE}.txt")
        for number, line in self._lines(directory / "sources.txt"):
            fields = line.split()
            if len(fields) != 4 or fields[1] not in SOURCE_KINDS:
                raise BuildError(f"sources.txt:{number}: expected `name kind sha256 location`")
            name = normalize(fields[0])
            if name not in self.pins:
                raise BuildError(f"sources.txt:{number}: {name} is not pinned in any component")
            self.sources[name] = (fields[1], fields[2], fields[3])
        if (directory / "undeclared-licenses.txt").is_file():
            for number, line in self._lines(directory / "undeclared-licenses.txt"):
                name, _, reason = line.partition(" ")
                if normalize(name) not in self.pins or not reason.strip():
                    raise BuildError(
                        f"undeclared-licenses.txt:{number}: expected `pinned-name reason`"
                    )
                self.undeclared[normalize(name)] = reason.strip()

    @staticmethod
    def _lines(path: Path) -> list[tuple[int, str]]:
        if not path.is_file():
            raise BuildError(f"missing {path}")
        rows = enumerate(path.read_text(encoding="utf-8").splitlines(), start=1)
        return [(n, s.strip()) for n, s in rows if s.strip() and not s.lstrip().startswith("#")]

    def requirement(self, name: str) -> str:
        written, version, digest, _ = self.pins[name]
        return f"{written}=={version} --hash=sha256:{digest}"

    def component_file(self, component: str) -> Path:
        return self.directory / f"{component}.txt"


def tool_python(work: Path) -> str:
    """An interpreter with the distribution's own pip, outside the environment."""
    tool = work / "tool"
    run([sys.executable, "-m", "venv", str(tool)])
    return str(tool / "bin" / "python")


def fetch(lock: Lock, wheelhouse: Path, work: Path) -> None:
    wheelhouse.mkdir(parents=True, exist_ok=True)
    for name, (kind, digest, location) in sorted(lock.sources.items()):
        if kind == "tree":
            continue
        target = wheelhouse / location.rsplit("/", 1)[1]
        if not (target.is_file() and sha256_of(target) == digest):
            print(f"fetching {location}", flush=True)
            partial = target.with_name(target.name + ".part")
            with (
                urllib.request.urlopen(location, timeout=120) as response,
                partial.open("wb") as out,
            ):
                shutil.copyfileobj(response, out)
            if sha256_of(partial) != digest:
                partial.unlink()
                raise BuildError(f"{name}: {location} does not have the locked digest")
            partial.replace(target)
    held = {sha256_of(path) for path in wheelhouse.iterdir() if path.is_file()}
    wanted = [
        lock.requirement(name)
        for name, pin in sorted(lock.pins.items())
        if name not in lock.sources and pin[2] not in held
    ]
    if not wanted:
        print("the wheelhouse already holds every index pin")
        return
    requirements = work / "fetch.txt"
    requirements.write_text("".join(line + "\n" for line in wanted), encoding="utf-8")
    run(
        [
            tool_python(work), "-m", "pip", "--isolated", "download",
            "--disable-pip-version-check", "--no-deps", "--require-hashes",
            "--only-binary", ":all:", "--progress-bar", "off",
            "-r", str(lock.directory / "index.txt"), "-r", str(requirements),
            "-d", str(wheelhouse),
        ]
    )  # fmt: skip


def build_wheel(python: str, source: Path, out: Path) -> Path:
    before = set(out.glob("*.whl")) if out.is_dir() else set()
    env = dict(os.environ, SOURCE_DATE_EPOCH=BUILD_EPOCH, PYTHONHASHSEED="0")
    run(
        [
            python, "-m", "pip", "--isolated", "wheel", "--disable-pip-version-check",
            "--no-deps", "--no-index", "--no-build-isolation", "--use-pep517",
            "--no-cache-dir", "-w", str(out), str(source),
        ],
        env=env,
    )  # fmt: skip
    built = set(out.glob("*.whl")) - before
    if len(built) != 1:
        raise BuildError(f"building {source} produced {len(built)} wheels, expected one")
    return built.pop()


def build_source_wheels(lock: Lock, wheelhouse: Path, work: Path, python: str) -> Path:
    """Build each sdist and in-tree pin; the result must have the locked digest."""
    built = work / "built"
    built.mkdir()
    for name, (kind, digest, location) in sorted(lock.sources.items()):
        if kind == "wheel":
            continue
        if kind == "sdist":
            source = wheelhouse / location.rsplit("/", 1)[1]
            if not source.is_file() or sha256_of(source) != digest:
                raise BuildError(f"{name}: {source.name} is missing or not the locked archive")
        else:
            tree = (lock.directory / location).resolve()
            if not tree.is_dir():
                raise BuildError(f"{name}: no source tree at {tree}")
            source = work / "src" / name
            shutil.copytree(tree, source)
        wheel = build_wheel(python, source, built)
        actual = sha256_of(wheel)
        if actual != lock.pins[name][2]:
            raise BuildError(
                f"{name}: the wheel built here is sha256:{actual}, not the locked digest; "
                "the build did not reproduce or the lock is stale"
            )
    return built


def rewrite_staging_paths(env: Path) -> None:
    """Make the environment describe its installed location, not where it was built."""
    (env / "pyvenv.cfg").write_text(
        "home = /usr/bin\ninclude-system-site-packages = false\n", encoding="utf-8"
    )
    staged = f"#!{env}/bin/python".encode()
    # pip writes this form instead when the path is too long for a shebang.
    wrapped = f"#!/bin/sh\n'''exec' {env}/bin/python".encode()
    wrapped_end = b"\n' '''"
    installed = f"#!{ENV_ROOT}/bin/python".encode()
    for script in sorted((env / "bin").iterdir()):
        if script.is_symlink() or not script.is_file():
            continue
        if script.name.lower().startswith("activate"):
            # Shell activation hard-codes the build directory; nothing uses it.
            script.unlink()
            continue
        data = script.read_bytes()
        if data.startswith(staged):
            script.write_bytes(installed + data[data.index(b"\n") :])
        elif data.startswith(wrapped):
            end = data.index(wrapped_end) + len(wrapped_end)
            script.write_bytes(installed + data[end:])


def assert_no_staging_path(env: Path, work: Path) -> None:
    needle = str(work).encode()
    for directory, _, names in os.walk(env):
        for name in names:
            path = Path(directory) / name
            if path.is_symlink():
                if str(work) in os.readlink(path):
                    raise BuildError(f"{path} links into the build directory")
                continue
            tail = b""
            with path.open("rb") as handle:
                for block in iter(lambda: handle.read(1 << 20), b""):
                    if needle in tail + block:
                        raise BuildError(f"{path} records the build directory")
                    tail = block[-len(needle) :]


def read_metadata(dist_info: Path) -> dict[str, list[str]]:
    fields: dict[str, list[str]] = {}
    for line in (dist_info / "METADATA").read_text(encoding="utf-8", errors="replace").splitlines():
        if not line.strip():
            break
        if line[0] in " \t" or ": " not in line:
            continue
        key, value = line.split(": ", 1)
        fields.setdefault(key, []).append(value.strip())
    return fields


def owners(env: Path, site: Path) -> tuple[dict[Path, str], dict[str, Path]]:
    """Map every installed file to its distribution through the RECORD files."""
    owner: dict[Path, str] = {}
    dist_infos: dict[str, Path] = {}
    for dist_info in sorted(site.glob("*.dist-info")):
        name = normalize(read_metadata(dist_info)["Name"][0])
        dist_infos[name] = dist_info
        with (dist_info / "RECORD").open(newline="", encoding="utf-8") as handle:
            for row in csv.reader(handle):
                path = Path(os.path.normpath(site / row[0]))
                if env not in path.parents:
                    raise BuildError(f"{name} records {row[0]}, outside the environment")
                if owner.setdefault(path, name) != name:
                    raise BuildError(f"{row[0]} is recorded by both {owner[path]} and {name}")
    return owner, dist_infos


def split(lock: Lock, env: Path, site: Path, destdir: Path) -> dict[str, list[str]]:
    owner, dist_infos = owners(env, site)
    component_of = {name: pin[3] for name, pin in lock.pins.items()}
    component_of[SIDECAR] = BASE
    unlocked = sorted(set(dist_infos) - set(component_of))
    if unlocked:
        raise BuildError(f"installed but in no lock file: {', '.join(unlocked)}")
    absent = sorted(set(component_of) - set(dist_infos))
    if absent:
        raise BuildError(f"locked but not installed: {', '.join(absent)}")

    moves: list[tuple[Path, str]] = []
    for directory, dirnames, names in os.walk(env):
        here = Path(directory)
        linked = [d for d in dirnames if (here / d).is_symlink()]
        for name in names + linked:
            path = here / name
            relative = path.relative_to(env).as_posix()
            source = path
            if here.name == "__pycache__" and name.endswith(".pyc"):
                source = here.parent / (name.split(".", 1)[0] + ".py")
            if source in owner:
                moves.append((path, component_of[owner[source]]))
            elif relative in VENV_FILES:
                moves.append((path, BASE))
            else:
                raise BuildError(f"{relative} belongs to no distribution")
    contents: dict[str, list[str]] = {component: [] for component in lock.components}
    for path, component in moves:
        target = (
            destdir / (PACKAGE_PREFIX + component) / ENV_ROOT.lstrip("/") / path.relative_to(env)
        )
        target.parent.mkdir(parents=True, exist_ok=True)
        os.replace(path, target)
    for name, component in component_of.items():
        contents[component].append(name)
    empty = sorted(c for c, names in contents.items() if not names)
    if empty:
        raise BuildError(f"no distribution in: {', '.join(empty)}")
    write_license_manifests(lock, contents, dist_infos, env, destdir)
    return contents


def write_license_manifests(
    lock: Lock,
    contents: dict[str, list[str]],
    dist_infos: dict[str, Path],
    env: Path,
    destdir: Path,
) -> None:
    """Record each shipped distribution's declared license beside its package."""
    for component, names in contents.items():
        package = PACKAGE_PREFIX + component
        installed = destdir / package / ENV_ROOT.lstrip("/")
        entries = []
        for name in sorted(names):
            if name == SIDECAR:
                continue
            dist_info = installed / dist_infos[name].relative_to(env)
            meta = read_metadata(dist_info)
            files = sorted(
                p.relative_to(installed).as_posix()
                for p in dist_info.rglob("*")
                if p.is_file() and (p.parent.name == "licenses" or LICENSE_FILE.match(p.name))
            )
            declared = {
                "license_expression": meta.get("License-Expression", [None])[0],
                "license": next((v for v in meta.get("License", []) if v != "UNKNOWN"), None),
                "license_classifiers": [
                    c for c in meta.get("Classifier", []) if c.startswith("License ::")
                ],
            }
            licensed = bool(files) or any(declared.values())
            if not licensed and name not in lock.undeclared:
                raise BuildError(f"{name} declares no license and ships no license file")
            if licensed and name in lock.undeclared:
                raise BuildError(f"{name} declares a license; drop it from undeclared-licenses.txt")
            entries.append(
                {
                    "name": meta["Name"][0],
                    "version": meta["Version"][0],
                    "sha256": lock.pins[name][2],
                    **declared,
                    "license_files": files,
                    "undeclared_license_reason": lock.undeclared.get(name),
                }
            )
        doc = destdir / package / "usr/share/doc" / package
        doc.mkdir(parents=True, exist_ok=True)
        (doc / "third-party-licenses.json").write_text(
            json.dumps({"environment_root": ENV_ROOT, "distributions": entries}, indent=2) + "\n",
            encoding="utf-8",
        )


def build(lock: Lock, wheelhouse: Path, destdir: Path, source_root: Path, work: Path) -> None:
    # Only the compile step below may write caches into the environment.
    os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
    # Wheels record file modes, so the two pinned builds depend on the umask.
    os.umask(0o022)
    tool = tool_python(work)
    pip = [tool, "-m", "pip", "--isolated", "--disable-pip-version-check"]
    offline = ["--no-index", "--no-deps", "--find-links", str(wheelhouse)]
    if "setuptools" not in lock.pins:
        raise BuildError("the lock pins no setuptools to build source wheels with")
    backend = work / "build-backend.txt"
    backend.write_text(lock.requirement("setuptools") + "\n", encoding="utf-8")
    run([*pip, "install", *offline, "--require-hashes", "-r", str(backend)])

    built = build_source_wheels(lock, wheelhouse, work, tool)
    sidecar_source = work / "src" / SIDECAR
    sidecar_source.mkdir(parents=True, exist_ok=True)
    backend_tree = source_root / "backends" / "python_pytorch"
    for name in ("pyproject.toml", "README.md"):
        shutil.copy2(backend_tree / name, sidecar_source / name)
    shutil.copytree(
        backend_tree / "src", sidecar_source / "src", ignore=shutil.ignore_patterns("__pycache__")
    )
    sidecar = work / "sidecar"
    sidecar.mkdir()
    build_wheel(tool, sidecar_source, sidecar)

    root = work / "root"
    env = root / ENV_ROOT.lstrip("/")
    env.parent.mkdir(parents=True)
    run([INTERPRETER, "-m", "venv", "--without-pip", str(env)])
    python = str(env / "bin" / "python")
    links = [os.readlink(p) for p in (env / "bin").iterdir() if p.is_symlink()]
    if INTERPRETER not in links or any("/" in target for target in links if target != INTERPRETER):
        raise BuildError(f"the environment's interpreter must link to {INTERPRETER}: {links}")
    install = [*pip, "--python", python, "install", "--no-compile", "--no-warn-script-location"]
    locks = [arg for c in lock.components for arg in ("-r", str(lock.component_file(c)))]
    run([*install, *offline, "--find-links", str(built), "--require-hashes", *locks])
    run([*install, "--no-index", "--no-deps", "--find-links", str(sidecar), SIDECAR])
    run([*pip, "--python", python, "check"])

    self_test = lock.directory / "self-test.py"
    if self_test.is_file():
        scratch = work / "self-test"
        (scratch / "tmp").mkdir(parents=True)
        run(
            [python, str(self_test)],
            env={
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "HOME": str(scratch),
                "TMPDIR": str(scratch / "tmp"),
                "PYTHONDONTWRITEBYTECODE": "1",
                "ORT_DISABLE_TELEMETRY": "1",
            },
        )

    rewrite_staging_paths(env)
    # Checked-hash caches stay valid on a read-only root; -s/-p record the
    # installed path instead of the build directory.
    run(
        [
            python, "-m", "compileall", "-q", "-f", "-j", "0",
            "--invalidation-mode", "checked-hash",
            "-s", str(root), "-p", "/", str(env / "lib"),
        ]
    )  # fmt: skip
    assert_no_staging_path(env, work)

    site = next((env / "lib").glob("python3.*/site-packages"))
    contents = split(lock, env, site, destdir)
    for component in lock.components:
        print(f"{PACKAGE_PREFIX}{component}: {len(contents[component])} distributions")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("stage", choices=("fetch", "build"))
    parser.add_argument("--lock-dir", type=Path, required=True)
    parser.add_argument("--wheelhouse", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True, help="scratch; must not exist")
    parser.add_argument("--destdir", type=Path, help="build: the debian/ directory")
    parser.add_argument("--source-root", type=Path, help="build: the repository root")
    args = parser.parse_args()
    try:
        lock = Lock(args.lock_dir.resolve())
        work = args.work_dir.resolve()
        work.mkdir(parents=True)
        try:
            if args.stage == "fetch":
                fetch(lock, args.wheelhouse.resolve(), work)
            else:
                if args.destdir is None or args.source_root is None:
                    parser.error("build needs --destdir and --source-root")
                build(
                    lock,
                    args.wheelhouse.resolve(),
                    args.destdir.resolve(),
                    args.source_root.resolve(),
                    work,
                )
        finally:
            shutil.rmtree(work, ignore_errors=True)
    except (BuildError, OSError) as error:
        print(f"build-environment: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
