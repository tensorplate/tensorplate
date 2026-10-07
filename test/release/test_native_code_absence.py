#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise the guard with ELF files and maps recorded from the real GNU linker."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "native_code_absence", ROOT / "tools/release/check-native-code-absence.py"
)
assert SPEC and SPEC.loader
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)
PORTS = {"grpc": "libgrpc.a", "openssl": "libcrypto.a", "zlib": "libz.a"}


def native_toolchain_gap() -> str | None:
    """Return why this host cannot produce the real ELF/link-map controls."""
    if not sys.platform.startswith("linux"):
        return "native linker controls require a Linux ELF host"
    missing = [name for name in ("cc", "ld.bfd", "ar", "readelf") if shutil.which(name) is None]
    if missing:
        return f"no {', '.join(missing)} on PATH"
    for command, banner in (("ld.bfd", "GNU ld"), ("ar", "GNU ar"), ("readelf", "GNU readelf")):
        try:
            result = subprocess.run([command, "--version"], text=True, capture_output=True, timeout=10)
        except (OSError, subprocess.SubprocessError):
            return f"cannot run {command}"
        if result.returncode or not result.stdout.startswith(banner):
            return f"{command} on PATH is not the required GNU tool"
    try:
        with tempfile.TemporaryDirectory(prefix="tp-native-toolchain-") as directory:
            worker = pathlib.Path(directory) / "probe"
            result = subprocess.run(
                ["cc", "-fuse-ld=bfd", "-x", "c", "-", "-o", str(worker)],
                input="int main(void) { return 0; }\n", text=True, capture_output=True, timeout=30,
            )
            if result.returncode:
                return "cc cannot link a C executable with GNU ld.bfd"
            if worker.read_bytes()[:4] != b"\x7fELF":
                return "cc with GNU ld.bfd did not produce an ELF executable"
    except (OSError, subprocess.SubprocessError):
        return "cannot run the C compiler and linker prerequisite check"
    return None


def require_native_toolchain() -> None:
    """Skip unavailable local controls; missing hosted-CI prerequisites must fail."""
    gap = native_toolchain_gap()
    if gap:
        reason = f"{gap}; native code absence NOT verified here"
        if os.environ.get("CI") == "true":
            raise AssertionError(reason)
        raise unittest.SkipTest(reason)


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
    @classmethod
    def setUpClass(cls) -> None:
        require_native_toolchain()

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

    def test_cli_prints_the_verified_report_with_and_without_output(self) -> None:
        output = self.root / "absence.json"
        arguments = ["check-native-code-absence.py", "--worker", str(self.worker),
                     "--link-map", str(self.link_map), "--install-root", str(self.installed),
                     "--triplet", "x64-linux", "--port", "grpc"]
        for extra in ([], ["--output", str(output)]):
            with self.subTest(extra=bool(extra)), mock.patch.object(sys, "argv", arguments + extra):
                printed = io.StringIO()
                with contextlib.redirect_stdout(printed):
                    self.assertEqual(guard.main(), 0)
                self.assertTrue(printed.getvalue(), "verified report missing from stdout")
                self.assertEqual(json.loads(printed.getvalue()), self.verify(["grpc"]))
                if extra:
                    self.assertEqual(output.read_text(), printed.getvalue())

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


class NativeToolchainPrerequisiteTests(unittest.TestCase):
    def test_non_linux_host_has_an_explicit_reason(self) -> None:
        with mock.patch.object(sys, "platform", "darwin"):
            self.assertEqual(native_toolchain_gap(), "native linker controls require a Linux ELF host")

    def test_every_required_tool_is_checked(self) -> None:
        for missing in ("cc", "ld.bfd", "ar", "readelf"):
            with self.subTest(tool=missing), mock.patch.object(sys, "platform", "linux"), \
                    mock.patch.object(shutil, "which", side_effect=lambda name: None if name == missing else name):
                self.assertEqual(native_toolchain_gap(), f"no {missing} on PATH")

    @staticmethod
    def tool_result(arguments, **kwargs):
        banners = {"ld.bfd": "GNU ld", "ar": "GNU ar", "readelf": "GNU readelf"}
        return subprocess.CompletedProcess(arguments, 0, banners.get(arguments[0], ""), "")

    def test_non_gnu_tools_are_rejected(self) -> None:
        for tool in ("ld.bfd", "ar", "readelf"):
            def result(arguments, **kwargs):
                if arguments[0] == tool:
                    return subprocess.CompletedProcess(arguments, 0, "another implementation", "")
                return self.tool_result(arguments)
            with self.subTest(tool=tool), mock.patch.object(sys, "platform", "linux"), \
                    mock.patch.object(shutil, "which", return_value="available"), \
                    mock.patch.object(subprocess, "run", side_effect=result):
                self.assertEqual(native_toolchain_gap(), f"{tool} on PATH is not the required GNU tool")

    def test_compiler_link_failure_has_an_explicit_reason(self) -> None:
        def result(arguments, **kwargs):
            if arguments[0] == "cc":
                return subprocess.CompletedProcess(arguments, 1, "", "missing compiler runtime")
            return self.tool_result(arguments)
        with mock.patch.object(sys, "platform", "linux"), \
                mock.patch.object(shutil, "which", return_value="available"), \
                mock.patch.object(subprocess, "run", side_effect=result):
            self.assertEqual(native_toolchain_gap(), "cc cannot link a C executable with GNU ld.bfd")

    def test_compiler_output_must_be_elf(self) -> None:
        with mock.patch.object(sys, "platform", "linux"), \
                mock.patch.object(shutil, "which", return_value="available"), \
                mock.patch.object(subprocess, "run", side_effect=self.tool_result), \
                mock.patch.object(pathlib.Path, "read_bytes", return_value=b"not ELF"):
            self.assertEqual(native_toolchain_gap(), "cc with GNU ld.bfd did not produce an ELF executable")

    def test_unavailable_local_toolchain_skips_with_reason(self) -> None:
        with mock.patch.dict(os.environ, {"CI": "false"}), \
                mock.patch.dict(require_native_toolchain.__globals__, native_toolchain_gap=lambda: "no cc on PATH"):
            with self.assertRaisesRegex(unittest.SkipTest, "no cc on PATH; native code absence NOT verified here"):
                require_native_toolchain()

    def test_unavailable_ci_toolchain_fails_instead_of_skipping(self) -> None:
        with mock.patch.dict(os.environ, {"CI": "true"}), \
                mock.patch.dict(require_native_toolchain.__globals__, native_toolchain_gap=lambda: "no cc on PATH"):
            try:
                require_native_toolchain()
            except unittest.SkipTest:
                self.fail("missing CI prerequisites must fail, not skip")
            except AssertionError as exc:
                self.assertIn("no cc on PATH; native code absence NOT verified here", str(exc))
            else:
                self.fail("missing CI prerequisites were accepted")

    def test_available_toolchain_runs_locally_and_in_ci(self) -> None:
        for ci in ("false", "true"):
            with self.subTest(ci=ci), mock.patch.dict(os.environ, {"CI": ci}), \
                    mock.patch.dict(require_native_toolchain.__globals__, native_toolchain_gap=lambda: None):
                require_native_toolchain()

    def test_linker_cases_use_the_local_skip_guard(self) -> None:
        with mock.patch.dict(os.environ, {"CI": "false"}), \
                mock.patch.dict(require_native_toolchain.__globals__, native_toolchain_gap=lambda: "no cc on PATH"):
            result = unittest.TestResult()
            unittest.TestSuite([NativeCodeAbsenceTests("test_loaded_but_unextracted_archives_are_absent")]).run(result)
            self.assertEqual(len(result.skipped), 1)
            self.assertIn("native code absence NOT verified here", result.skipped[0][1])
            self.assertEqual(result.errors, [])


if __name__ == "__main__":
    unittest.main()
