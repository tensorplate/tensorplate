#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise the guard with ELF files and maps recorded from the real GNU linker."""

from __future__ import annotations

import importlib.util
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "native_code_absence", ROOT / "tools/release/check-native-code-absence.py"
)
assert SPEC and SPEC.loader
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)
PORTS = {"grpc": "libgrpc.a", "openssl": "libcrypto.a", "zlib": "libz.a"}


def build_fixture(
    root: pathlib.Path, included_port: str | None = None, shared_port: str | None = None,
    thin_port: str | None = None, inventory_symlink_port: str | None = None,
    linked_alias_port: str | None = None,
) -> pathlib.Path:
    """Compile synthetic ports and record real ELF/link-map output below root/native."""
    native = root / "native"
    native.mkdir(parents=True, exist_ok=True)
    worker = native / "serving_worker/tensorplate-serving"
    worker.parent.mkdir(exist_ok=True)
    installed = native / "vcpkg_installed"
    libraries = installed / "x64-linux/lib"
    info = installed / "vcpkg/info"
    libraries.mkdir(parents=True, exist_ok=True)
    info.mkdir(parents=True, exist_ok=True)
    archives = []
    for port, name in PORTS.items():
        source = native / f"{port}.c"
        source.write_text(f"int tp_control_{port}(void) {{ return 17; }}\n")
        obj = native / f"{port}.o"
        subprocess.run(["cc", "-fPIC", "-c", str(source), "-o", str(obj)], check=True)
        archive = libraries / name
        archive.unlink(missing_ok=True)
        subprocess.run(["ar", "rcsT" if port == thin_port else "rcs", str(archive), str(obj)], check=True)
        (info / f"{port}_1.0_x64-linux.list").write_text(f"x64-linux/lib/{name}\n")
        if port == inventory_symlink_port:
            regular = archive.with_name(name.replace(".a", "-real.a"))
            archive.replace(regular)
            archive.symlink_to(regular)
        if port == linked_alias_port:
            alias = native / f"{port}-alias.bin"
            alias.unlink(missing_ok=True)
            alias.symlink_to(archive)
            archive = alias
        archives.append(str(archive))
    main = native / "main.c"
    expression = "0"
    declaration = ""
    if included_port:
        declaration = f"int tp_control_{included_port}(void);\n"
        expression = f"tp_control_{included_port}()"
    main.write_text(f"{declaration}int main(void) {{ return {expression}; }}\n")
    command = ["cc", "-fuse-ld=bfd", str(main), *archives]
    if shared_port:
        shared = libraries / PORTS[shared_port].replace(".a", ".so.1")
        subprocess.run(["cc", "-shared", f"-Wl,-soname,{shared.name}",
                        str(native / f"{shared_port}.o"), "-o", str(shared)], check=True)
        command.extend(["-Wl,--no-as-needed", str(shared)])
    command.extend(["-Wl,-Map,worker.map", "-o", "serving_worker/tensorplate-serving"])
    subprocess.run(command, cwd=native, check=True)
    return native


class NativeCodeAbsenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="tp-native-absence-")
        self.root = pathlib.Path(self.temp.name)
        self.native = build_fixture(self.root)
        self.worker = self.native / "serving_worker/tensorplate-serving"
        self.link_map = self.native / "worker.map"
        self.installed = self.native / "vcpkg_installed"

    def tearDown(self) -> None:
        self.temp.cleanup()

    def verify(self, ports: list[str] | None = None) -> dict:
        return guard.verify_absence(self.worker, self.link_map, self.installed,
                                    "x64-linux", ports or list(PORTS))

    def test_loaded_but_unextracted_archives_are_absent(self) -> None:
        report = self.verify()
        self.assertEqual(report["absent_ports"], sorted(PORTS))
        self.assertEqual(report["worker_sha256"], guard.digest(self.worker))
        self.assertEqual(report["link_map_sha256"], guard.digest(self.link_map))

    def test_real_extraction_of_each_port_is_rejected(self) -> None:
        for port in PORTS:
            with self.subTest(port=port):
                build_fixture(self.root, included_port=port)
                with self.assertRaisesRegex(ValueError, f"{port}: worker extracted archive members"):
                    self.verify()

    def test_real_shared_link_of_each_port_is_rejected(self) -> None:
        for port in PORTS:
            with self.subTest(port=port):
                build_fixture(self.root, shared_port=port)
                with self.assertRaisesRegex(ValueError, f"{port}: worker loads shared libraries"):
                    self.verify()

    def test_real_extracted_thin_archive_of_each_port_is_rejected(self) -> None:
        for port in PORTS:
            with self.subTest(port=port):
                build_fixture(self.root, included_port=port, thin_port=port)
                with self.assertRaisesRegex(ValueError, f"{port}: inventory contains an unsupported archive format"):
                    self.verify()

    def test_real_unextracted_thin_archive_of_each_port_is_rejected(self) -> None:
        for port in PORTS:
            with self.subTest(port=port):
                build_fixture(self.root, thin_port=port)
                with self.assertRaisesRegex(ValueError, f"{port}: inventory contains an unsupported archive format"):
                    self.verify()

    def test_unloaded_thin_archive_in_inventory_is_rejected(self) -> None:
        archive = self.installed / "x64-linux/lib/libunused.a"
        subprocess.run(["ar", "rcsT", str(archive), str(self.native / "grpc.o")], check=True)
        inventory = self.installed / "vcpkg/info/grpc_1.0_x64-linux.list"
        inventory.write_text(inventory.read_text() + "x64-linux/lib/libunused.a\n")
        with self.assertRaisesRegex(ValueError, "grpc: inventory contains an unsupported archive format"):
            self.verify()

    def test_corrupt_archive_header_is_rejected(self) -> None:
        (self.installed / "x64-linux/lib/libgrpc.a").write_bytes(b"!<arc")
        with self.assertRaisesRegex(ValueError, "grpc: inventory contains an unsupported archive format"):
            self.verify()

    def test_real_extracted_inventory_symlink_of_each_port_is_rejected(self) -> None:
        for port in PORTS:
            with self.subTest(port=port):
                build_fixture(self.root, included_port=port, inventory_symlink_port=port)
                with self.assertRaisesRegex(ValueError, f"{port}: inventory contains an archive path alias"):
                    self.verify()

    def test_real_unextracted_inventory_symlink_of_each_port_is_rejected(self) -> None:
        for port in PORTS:
            with self.subTest(port=port):
                build_fixture(self.root, inventory_symlink_port=port)
                with self.assertRaisesRegex(ValueError, f"{port}: inventory contains an archive path alias"):
                    self.verify()

    def test_inventory_archive_parent_symlink_is_rejected(self) -> None:
        libraries = self.installed / "x64-linux/lib"
        regular = libraries.with_name("lib-real")
        libraries.rename(regular)
        libraries.symlink_to(regular, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "grpc: inventory contains an archive path alias"):
            self.verify()

    def test_real_extracted_linker_alias_of_each_port_is_rejected(self) -> None:
        for port in PORTS:
            with self.subTest(port=port):
                build_fixture(self.root, included_port=port, linked_alias_port=port)
                with self.assertRaisesRegex(ValueError, f"{port}: worker extracted archive members"):
                    self.verify()

    def test_real_unextracted_linker_alias_is_absent(self) -> None:
        build_fixture(self.root, linked_alias_port="grpc")
        self.assertEqual(self.verify()["absent_ports"], sorted(PORTS))

    def test_empty_map_is_rejected(self) -> None:
        self.link_map.write_text("")
        with self.assertRaisesRegex(ValueError, "incomplete GNU ld map"):
            self.verify()

    def test_truncated_map_is_rejected(self) -> None:
        self.link_map.write_text(self.link_map.read_text().split("OUTPUT(")[0])
        with self.assertRaisesRegex(ValueError, "OUTPUT does not name this worker"):
            self.verify()

    def test_map_for_another_output_is_rejected(self) -> None:
        self.link_map.write_text(self.link_map.read_text().replace(
            "OUTPUT(serving_worker/tensorplate-serving ", "OUTPUT(other-worker "
        ))
        with self.assertRaisesRegex(ValueError, "OUTPUT does not name this worker"):
            self.verify()

    def test_different_worker_is_rejected_even_at_same_path(self) -> None:
        other = build_fixture(self.root / "other", included_port="grpc")
        shutil.copyfile(other / "serving_worker/tensorplate-serving", self.worker)
        with self.assertRaisesRegex(ValueError, r"\.text does not match"):
            self.verify()

    def test_stale_map_is_rejected(self) -> None:
        past = self.worker.stat().st_mtime - 120
        os.utime(self.link_map, (past, past))
        with self.assertRaisesRegex(ValueError, "same fresh link"):
            self.verify()

    def test_non_elf_worker_is_rejected(self) -> None:
        self.worker.write_bytes(b"not an executable")
        with self.assertRaisesRegex(ValueError, "worker is not ELF"):
            self.verify()

    def test_missing_map_is_rejected(self) -> None:
        self.link_map.unlink()
        with self.assertRaisesRegex(ValueError, "cannot verify native code absence"):
            self.verify()

    def test_unknown_port_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "expected one target-triplet installed inventory"):
            self.verify(["unknown"])

    def test_host_inventory_does_not_replace_target_inventory(self) -> None:
        path = self.installed / "vcpkg/info/grpc_1.0_x64-linux.list"
        path.rename(path.with_name("grpc_1.0_arm64-linux.list"))
        with self.assertRaisesRegex(ValueError, "grpc: expected one target-triplet"):
            self.verify()

    def test_inventory_with_no_archives_is_rejected(self) -> None:
        (self.installed / "vcpkg/info/grpc_1.0_x64-linux.list").write_text("x64-linux/include/grpc.h\n")
        with self.assertRaisesRegex(ValueError, "grpc: inventory lists no installed archives"):
            self.verify()

    def test_missing_archive_is_rejected(self) -> None:
        (self.installed / "x64-linux/lib/libgrpc.a").unlink()
        with self.assertRaisesRegex(ValueError, "cannot verify native code absence"):
            self.verify()

    def test_no_archive_load_is_not_an_absence_claim(self) -> None:
        self.link_map.write_text("\n".join(
            line for line in self.link_map.read_text().splitlines()
            if not (line.startswith("LOAD ") and line.endswith("libgrpc.a"))
        ) + "\n")
        with self.assertRaisesRegex(ValueError, "grpc: map never loads an installed archive"):
            self.verify()

    def test_archive_escape_is_rejected(self) -> None:
        archive = self.installed / "x64-linux/lib/libgrpc.a"
        outside = self.root / "outside.a"
        archive.rename(outside)
        archive.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "grpc: archive outside target-triplet"):
            self.verify()


if __name__ == "__main__":
    unittest.main()
