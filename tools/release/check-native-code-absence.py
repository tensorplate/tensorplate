#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check that a freshly linked ELF worker extracted no objects from named ports."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys


def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify_absence(
    worker: pathlib.Path,
    link_map: pathlib.Path,
    install_root: pathlib.Path,
    triplet: str,
    ports: list[str],
    build_dir: pathlib.Path | None = None,
) -> dict:
    """Validate the worker, GNU ld map and installed inventories, or raise ValueError."""
    try:
        worker = worker.resolve(strict=True)
        link_map = link_map.resolve(strict=True)
        install_root = install_root.resolve(strict=True)
        build_dir = (build_dir or link_map.parent).resolve(strict=True)
        text = link_map.read_text()
        if not ports or not re.fullmatch(r"[a-z0-9-]+", triplet):
            raise ValueError("nonempty ports and a valid triplet are required")
        if worker.read_bytes()[:4] != b"\x7fELF":
            raise ValueError("worker is not ELF")
        if abs(worker.stat().st_mtime - link_map.stat().st_mtime) > 60:
            raise ValueError("worker and link map must come from the same fresh link")
        for marker in ("Memory Configuration", "Linker script and memory map", "/DISCARD/"):
            if marker not in text:
                raise ValueError(f"incomplete GNU ld map: missing {marker}")
        outputs = re.findall(r"^OUTPUT\((.+) (elf\S+)\)$", text, re.MULTILINE)
        if len(outputs) != 1 or (build_dir / outputs[0][0]).resolve() != worker:
            raise ValueError("GNU ld map OUTPUT does not name this worker")
        elf = subprocess.run(
            ["readelf", "--wide", "--file-header", "--section-headers", "--dynamic", str(worker)],
            check=True, text=True, capture_output=True,
        ).stdout
        if not re.search(r"Type:\s+(EXEC|DYN)\b", elf):
            raise ValueError("worker is not an ELF executable")
        sections = re.findall(
            r"^\s*\[\s*\d+\]\s+\.text\s+PROGBITS\s+([0-9a-f]+)\s+[0-9a-f]+\s+([0-9a-f]+)\b",
            elf, re.MULTILINE,
        )
        mapped = re.findall(r"^\.text\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)\b", text, re.MULTILINE)
        if (len(sections) != 1 or len(mapped) != 1
                or tuple(int(v, 16) for v in sections[0]) != tuple(int(v, 16) for v in mapped[0])):
            raise ValueError("GNU ld map .text does not match the worker's ELF section")
        loads = {(build_dir / item).resolve() for item in
                 re.findall(r"^LOAD (.+)$", text, re.MULTILINE)}
        members = {pathlib.Path(item).name for item in re.findall(r"(\S+\.a)\([^()\n]+\)", text)}
        needed = re.findall(r"\(NEEDED\).*Shared library: \[([^\]]+)\]", elf)
        for port in sorted(set(ports)):
            if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", port):
                raise ValueError(f"invalid port: {port}")
            inventories = list((install_root / "vcpkg/info").glob(f"{port}_*_{triplet}.list"))
            if len(inventories) != 1:
                raise ValueError(f"{port}: expected one target-triplet installed inventory")
            archives = set()
            for entry in inventories[0].read_text().splitlines():
                if not entry.endswith(".a"):
                    continue
                path = (install_root / entry).resolve(strict=True)
                if not path.is_relative_to(install_root / triplet) or not path.is_file():
                    raise ValueError(f"{port}: archive outside target-triplet install tree")
                archives.add(path)
            if not archives:
                raise ValueError(f"{port}: inventory lists no installed archives")
            if not loads.intersection(archives):
                raise ValueError(f"{port}: map never loads an installed archive")
            names = {path.name for path in archives}
            included = sorted(names.intersection(members))
            if included:
                raise ValueError(f"{port}: worker extracted archive members from {', '.join(included)}")
            stems = {path.name[:-2] for path in archives}
            shared = {name for name in needed if name.split(".so")[0] in stems}
            shared.update(path.name for path in loads
                          if ".so" in path.name and path.name.split(".so")[0] in stems)
            if shared:
                raise ValueError(f"{port}: worker loads shared libraries: {', '.join(sorted(shared))}")
        return {
            "schema_version": 1,
            "worker_sha256": digest(worker),
            "link_map_sha256": digest(link_map),
            "triplet": triplet,
            "absent_ports": sorted(set(ports)),
        }
    except (OSError, UnicodeError, subprocess.SubprocessError) as exc:
        raise ValueError(f"cannot verify native code absence: {exc}") from exc


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", type=pathlib.Path, required=True)
    parser.add_argument("--link-map", type=pathlib.Path, required=True)
    parser.add_argument("--install-root", type=pathlib.Path, required=True)
    parser.add_argument("--triplet", required=True)
    parser.add_argument("--port", action="append", required=True, dest="ports")
    parser.add_argument("--build-dir", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    try:
        report = verify_absence(args.worker, args.link_map, args.install_root,
                                args.triplet, args.ports, args.build_dir)
        serialized = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.write_text(serialized)
        else:
            print(serialized, end="")
        return 0
    except (ValueError, OSError) as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
