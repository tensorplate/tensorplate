#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Record release-native dependencies or check both architectures' filed records."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

TOOLS = Path(__file__).resolve().parent
TRIPLETS = {"amd64": "x64-linux", "arm64": "arm64-linux"}


def run(tool: str, *args: object) -> None:
    subprocess.run([sys.executable, str(TOOLS / tool), *map(str, args)], check=True)


def names(tag: str, arch: str) -> tuple[str, str]:
    return (f"tensorplate-{tag}-native-closure-{arch}.spdx.json",
            f"tensorplate-{tag}-vcpkg-cache-provenance-{arch}.json")


def check(directory: Path, tag: str, arches: list[str]) -> None:
    for arch in arches:
        sbom, record = [directory / name for name in names(tag, arch)]
        run("native-sbom.py", "check", "--sbom", sbom, "--manifest", "vcpkg.json",
            "--triplet", TRIPLETS[arch])
        run("native-cache-provenance.py", "check", "--record", record, "--sbom", sbom,
            "--manifest", "vcpkg.json", "--triplet", TRIPLETS[arch], "--version", tag[1:])


def record(args: argparse.Namespace) -> bool:
    args.directory.mkdir(parents=True, exist_ok=True)
    outputs = names(args.tag, args.architecture)
    for name in outputs:
        (args.directory / name).unlink(missing_ok=True)
    try:
        with tempfile.TemporaryDirectory(dir=args.directory) as staging:
            stage = Path(staging)
            sbom, provenance = [stage / name for name in outputs]
            commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
            created = subprocess.check_output(
                ["git", "show", "-s", "--format=%cI", "HEAD"], text=True).strip()
            from datetime import datetime, timezone
            created = datetime.fromisoformat(created).astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
            if args.architecture == "amd64":
                archives = Path(os.environ["RUNNER_TEMP"]) / "vcpkg-archives"
                key = subprocess.check_output([
                    sys.executable, str(TOOLS / "native-cache-provenance.py"), "key",
                    "--manifest", "vcpkg.json", "--profile", "tools/release/amd64-build-profile.sh",
                    "--action", ".github/actions/release-vcpkg/action.yml"], text=True).strip()
                if args.mode != "restore":
                    after = stage / "archives.json"
                    run("native-cache-provenance.py", "snapshot", "--archives", archives, "--output", after)
                    before = Path(os.environ["RUNNER_TEMP"]) / "native-cache-before.json"
                    if json.loads(before.read_text()) != json.loads(after.read_text()):
                        raise ValueError("build changed the restored cache archives")
                restore_started = (Path(os.environ["RUNNER_TEMP"]) /
                                   "native-cache-restore-started.txt").read_text().strip()
                restored_cache = json.loads((Path(os.environ["RUNNER_TEMP"]) /
                                             "native-cache-before-source.json").read_text())
                source = ["--source", "actions-cache", "--cache-key", key,
                          "--restored-cache-id", restored_cache["id"],
                          "--restore-started", restore_started,
                          "--repository", os.environ["GITHUB_REPOSITORY"],
                          "--ref", os.environ["GITHUB_REF"],
                          "--default-branch", os.environ["DEFAULT_BRANCH"]]
            else:
                match = re.fullmatch(r"clear;files,([^,;]+),read", os.environ["VCPKG_BINARY_SOURCES"])
                if not match:
                    raise ValueError("ARM64 requires one read-only filesystem cache")
                archives = Path(match[1])
                source = ["--source", "runner-filesystem", "--stamp", str(archives.parent / "provisioned.stamp")]
            packages = args.packages or (Path("dist/amd64") if args.architecture == "amd64" else args.directory)
            debs = list(packages.glob(f"tensorplate-serving_*_{args.architecture}.deb"))
            if len(debs) != 1:
                raise ValueError("expected exactly one serving package")
            expected = {"Package": "tensorplate-serving", "Architecture": args.architecture,
                        "Version": args.tag[1:].replace("-rc.", "~rc.") + "-1"}
            for field, value in expected.items():
                actual = subprocess.check_output(["dpkg-deb", "-f", str(debs[0]), field], text=True).strip()
                if actual != value:
                    raise ValueError("serving package identity mismatch")
            payload = stage / "payload"
            subprocess.run(["dpkg-deb", "-x", str(debs[0]), str(payload)], check=True)
            worker = payload / "usr/lib/tensorplate/tensorplate-serving"
            triplet = TRIPLETS[args.architecture]
            run("native-sbom.py", "collect", "--install-root", args.build_dir / "vcpkg_installed",
                "--manifest", "vcpkg.json", "--triplet", triplet,
                "--cmake-cache", args.build_dir / "CMakeCache.txt",
                "--worker", worker, "--archives", archives,
                "--version", args.tag[1:], "--created", created, "--output", sbom)
            run("native-cache-provenance.py", "record", *source, "--archives", archives,
                "--manifest", "vcpkg.json", "--triplet", triplet, "--created", created,
                "--commit", commit, "--version", args.tag[1:], "--sbom", sbom, "--output", provenance)
            check(stage, args.tag, [args.architecture])
            for name in outputs:
                (stage / name).replace(args.directory / name)
    except (subprocess.CalledProcessError, OSError, ValueError, TypeError, KeyError):
        for name in outputs:
            (args.directory / name).unlink(missing_ok=True)
        if args.mode == "restore":
            raise ValueError("required native closure records could not be produced") from None
        print("::warning::native closure recording unavailable in optional build mode", file=sys.stderr)
        return False
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for command in ("record", "check"):
        p = commands.add_parser(command)
        p.add_argument("--directory", type=Path, required=True)
        p.add_argument("--tag", required=True)
        if command == "record":
            p.add_argument("--mode", choices=("build", "build-and-save", "restore"), required=True)
            p.add_argument("--architecture", choices=TRIPLETS, required=True)
            p.add_argument("--build-dir", type=Path, default=Path("build/release"))
            p.add_argument("--packages", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "record" and "GITHUB_OUTPUT" in os.environ:
            with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
                output.write("recorded=false\n")
        if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-rc\.[1-9][0-9]*)?", args.tag):
            raise ValueError("invalid release tag")
        if args.command == "check":
            check(args.directory, args.tag, list(TRIPLETS))
        else:
            recorded = record(args)
            if "GITHUB_OUTPUT" in os.environ:
                with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
                    output.write(f"recorded={str(recorded).lower()}\n")
    except (subprocess.CalledProcessError, OSError, ValueError) as error:
        print(f"native closure records refused: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
