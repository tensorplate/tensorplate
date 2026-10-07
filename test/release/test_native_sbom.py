#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for tools/release/native-sbom.py.

The install tree under fixtures/native-sbom/install-tree is what vcpkg wrote
for the release configuration (see the README beside it): its status file
and each port's SPDX document. Nearly every refusal is that tree, or the
document made from it, with one thing wrong, and must fail for its own
reason.
"""

from __future__ import annotations

import copy
import importlib.util
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
TOOL = REPO_ROOT / "tools/release/native-sbom.py"
FIXTURES = REPO_ROOT / "test/release/fixtures/native-sbom"
TREE = FIXTURES / "install-tree"
MANIFEST = REPO_ROOT / "vcpkg.json"
TRIPLET = "x64-linux"
CLOSURE = ["abseil", "c-ares", "grpc", "nlohmann-json", "openssl", "protobuf", "re2", "utf8-range", "zlib"]

spec = importlib.util.spec_from_file_location("native_sbom", TOOL)
tool = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(tool)


def run(*args: object) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(TOOL), *map(str, args)],
                          text=True, capture_output=True, check=False)


class Fixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.tree = self.root / "vcpkg_installed"
        shutil.copytree(TREE, self.tree)
        self.sbom = self.root / "closure.spdx.json"

    def collect(self, *extra: object, tree: pathlib.Path | None = None,
                manifest: pathlib.Path = MANIFEST) -> subprocess.CompletedProcess:
        return run("collect", "--install-root", tree or self.tree, "--manifest", manifest,
                   "--triplet", TRIPLET, "--output", self.sbom,
                   "--created", "2026-10-07T00:00:00Z", *extra)

    def check(self, sbom: pathlib.Path | None = None, *, triplet: str = TRIPLET,
              feature: str = "streaming-grpc",
              manifest: pathlib.Path = MANIFEST) -> subprocess.CompletedProcess:
        return run("check", "--sbom", sbom or self.sbom, "--manifest", manifest,
                   "--feature", feature, "--triplet", triplet)

    def document(self) -> dict:
        result = self.collect()
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(self.sbom.read_text())

    def status_text(self) -> str:
        return (self.tree / "vcpkg/status").read_text()


class Collect(Fixture):
    def test_the_document_names_the_closure_the_install_tree_records(self) -> None:
        doc = self.document()
        ports = {p["name"]: p for p in doc["packages"][1:]}
        self.assertEqual(sorted(ports), CLOSURE)
        recorded = tool.installed_ports(tool.parse_status(TREE / "vcpkg/status"), TRIPLET)
        for name, package in ports.items():
            self.assertEqual(package["versionInfo"], tool.full_version(recorded[name]), name)
            self.assertNotEqual(package["licenseConcluded"], "NOASSERTION", name)
            self.assertTrue(package["downloadLocation"].startswith("git+https://github.com/"), name)
        linked = {r["relatedSpdxElement"] for r in doc["relationships"]
                  if r["relationshipType"] == "DEPENDS_ON"}
        self.assertEqual(linked, {p["SPDXID"] for p in ports.values()})

    def test_what_the_worker_does_not_link_is_left_out_and_named_as_left_out(self) -> None:
        installed = tool.installed_ports(tool.parse_status(TREE / "vcpkg/status"), TRIPLET)
        self.assertLessEqual({"gtest", "vcpkg-cmake"}, set(installed))
        doc = self.document()
        names = {p["name"] for p in doc["packages"]}
        self.assertFalse(names & {"gtest", "vcpkg-cmake", "vcpkg-cmake-config"})
        self.assertTrue(tool.document_annotations(doc)["not-linked"].startswith("gtest (test framework"))

    def test_the_manifests_own_dependency_the_worker_compiles_against_is_in(self) -> None:
        ports = {p["name"]: p for p in self.document()["packages"][1:]}
        package = ports["nlohmann-json"]
        self.assertIn("Header-only", package["comment"])
        self.assertIn("LOOKUP_WITHOUT_POSITIVE_CONTROL:", package["comment"])
        self.assertEqual([r["referenceLocator"] for r in package["externalRefs"]
                          if r["referenceType"] == "purl"],
                         ["pkg:vcpkg/nlohmann-json@3.12.0%232"])

    def test_the_same_tree_yields_the_same_document_name_and_no_random_one(self) -> None:
        first = self.document()["documentNamespace"]
        self.assertEqual(self.document()["documentNamespace"], first)
        self.assertRegex(first, r"/sha256-[0-9a-f]{64}$")
        openssl = self.tree / TRIPLET / "share/openssl/vcpkg.spdx.json"
        doc = json.loads(openssl.read_text())
        doc["packages"][2]["checksums"][0]["checksumValue"] = "0" * 128
        openssl.write_text(json.dumps(doc))
        self.assertNotEqual(self.document()["documentNamespace"], first)

    def test_the_same_inputs_are_byte_reproducible(self) -> None:
        self.document()
        first = self.sbom.read_bytes()
        self.document()
        self.assertEqual(self.sbom.read_bytes(), first)

    def test_created_is_required_and_validated(self) -> None:
        result = run("collect", "--install-root", self.tree, "--manifest", MANIFEST,
                     "--triplet", TRIPLET, "--output", self.sbom)
        self.assertEqual(result.returncode, 2)
        self.assertIn("--created", result.stderr)
        self.assert_refused(self.collect("--created", "not-a-timestamp"),
                            "created must be an explicit UTC timestamp")

    def test_native_package_urls_do_not_leak_into_other_ecosystems(self) -> None:
        ports = self.document()["packages"][1:]
        for package in ports:
            refs = [r["referenceLocator"] for r in package["externalRefs"]
                    if r["referenceType"] == "purl"]
            self.assertEqual(refs, [tool.purl(package["name"], package["versionInfo"])])
        self.assertEqual(tool.purl("some+port", "1.2.3#4"), "pkg:vcpkg/some%2Bport@1.2.3%234")

    def test_each_port_carries_its_source_checksum(self) -> None:
        ports = {p["name"]: p for p in self.document()["packages"][1:]}
        for name in CLOSURE:
            self.assertEqual(ports[name]["checksums"][0]["algorithm"], "SHA512", name)

    def test_a_dependency_written_with_a_feature_or_triplet_is_followed(self) -> None:
        self.assertEqual(tool.dependency_name(" protobuf[core,libprotoc]:x64-linux (>= 6)"), "protobuf")
        status = self.status_text().replace("Depends: abseil, c-ares,", "Depends: abseil[core]:x64-linux, c-ares,")
        self.assertNotEqual(status, self.status_text())
        (self.tree / "vcpkg/status").write_text(status)
        self.assertEqual(sorted(p["name"] for p in self.document()["packages"][1:]), CLOSURE)

    def test_a_dependency_only_a_feature_paragraph_names_is_followed(self) -> None:
        paragraphs = self.status_text().split("\n\n")
        main = next(i for i, p in enumerate(paragraphs)
                    if p.startswith("Package: grpc\n") and "Feature:" not in p)
        feature = next(i for i, p in enumerate(paragraphs)
                       if p.startswith("Package: grpc\n") and "Feature:" in p)
        paragraphs[main] = paragraphs[main].replace(" c-ares,", "")
        paragraphs[feature] = paragraphs[feature].replace("Depends: protobuf", "Depends: protobuf, c-ares")
        (self.tree / "vcpkg/status").write_text("\n\n".join(paragraphs))
        self.assertIn("c-ares", [p["name"] for p in self.document()["packages"]])

    def test_a_closure_port_the_status_file_does_not_record(self) -> None:
        kept = [p for p in self.status_text().split("\n\n") if not p.startswith("Package: re2\n")]
        (self.tree / "vcpkg/status").write_text("\n\n".join(kept))
        self.assert_refused(self.collect(), "the closure reaches re2, which the status file does not record")

    def test_an_archive_asked_for_and_absent(self) -> None:
        (self.root / "archives").mkdir()
        self.assert_refused(self.collect("--archives", self.root / "archives"),
                            "no binary-cache archive for abi")

    def test_each_port_carries_its_cpe_or_the_reason_it_has_none(self) -> None:
        ports = {p["name"]: p for p in self.document()["packages"][1:]}
        cpes = {
            name: [r["referenceLocator"] for r in p["externalRefs"] if r["referenceType"] == "cpe23Type"]
            for name, p in ports.items()
        }
        version = {name: p["versionInfo"].split("#")[0] for name, p in ports.items()}
        self.assertEqual(cpes["openssl"], [f"cpe:2.3:a:openssl:openssl:{version['openssl']}:*:*:*:*:*:*:*"])
        self.assertEqual(cpes["grpc"], [f"cpe:2.3:a:grpc:grpc:{version['grpc']}:*:*:*:*:*:*:*"])
        self.assertEqual(cpes["zlib"], [f"cpe:2.3:a:zlib:zlib:{version['zlib']}:*:*:*:*:*:*:*"])
        self.assertEqual(len(cpes["c-ares"]), 3)
        self.assertEqual(cpes["protobuf"], [
            "cpe:2.3:a:google:protobuf:6.33.4:*:*:*:*:*:*:*",
            "cpe:2.3:a:google:protobuf-cpp:6.33.4:*:*:*:*:*:*:*",
        ])
        self.assertEqual(cpes["abseil"],
                         [f"cpe:2.3:a:abseil:common_libraries:{version['abseil']}:*:*:*:*:*:*:*"])
        self.assertEqual(cpes["nlohmann-json"], [
            "cpe:2.3:a:json-for-modern-cpp_project:json-for-modern-cpp:3.12.0:*:*:*:*:*:*:*",
            "cpe:2.3:a:nlohmann:json:3.12.0:*:*:*:*:*:*:*",
        ])
        for name in ("re2", "utf8-range"):
            self.assertEqual(cpes[name], [])
            self.assertIn("UNSCANNED: ", ports[name]["comment"], name)
        annotations = tool.document_annotations(self.document())
        self.assertEqual(annotations["unscanned"], "re2 utf8-range")
        self.assertEqual(annotations["lookup-without-positive-control"], "nlohmann-json")
        self.assertEqual(annotations["controlled"], "abseil c-ares grpc openssl protobuf zlib")

    def test_the_worker_and_the_archives_are_digested_when_given(self) -> None:
        worker = self.root / "tensorplate-serving"
        worker.write_bytes(b"worker")
        installed = tool.installed_ports(tool.parse_status(TREE / "vcpkg/status"), TRIPLET)
        for name in CLOSURE:
            abi = installed[name]["abi"]
            archive = self.root / "archives" / abi[:2] / f"{abi}.zip"
            archive.parent.mkdir(parents=True, exist_ok=True)
            archive.write_bytes(f"{name} archive".encode())
        abi = installed["zlib"]["abi"]
        archive = self.root / "archives" / abi[:2] / f"{abi}.zip"
        result = self.collect("--worker", worker, "--archives", self.root / "archives",
                              "--version", "1.2.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        doc = json.loads(self.sbom.read_text())
        self.assertEqual(doc["packages"][0]["versionInfo"], "1.2.3")
        self.assertEqual(doc["packages"][0]["checksums"][0]["checksumValue"],
                         tool.digest_of(worker)["sha256"])
        refs = {p["name"]: [r["referenceLocator"] for r in p["externalRefs"]
                            if r["referenceType"] == "vcpkg-binary-cache-archive"]
                for p in doc["packages"][1:]}
        self.assertEqual(refs["zlib"], [f"{abi[:2]}/{abi}.zip?sha256={tool.digest_of(archive)['sha256']}&size=12"])
        self.assertTrue(all(len(found) == 1 and " " not in found[0] for found in refs.values()))

    def assert_refused(self, result: subprocess.CompletedProcess, reason: str) -> None:
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(reason, result.stderr)
        self.assertFalse(self.sbom.exists())

    def test_a_tree_without_a_dependency_of_the_feature(self) -> None:
        kept = [p for p in self.status_text().split("\n\n") if not p.startswith("Package: grpc\n")]
        (self.tree / "vcpkg/status").write_text("\n\n".join(kept))
        self.assert_refused(self.collect(), "dependency 'grpc' is not installed")

    def test_a_port_installed_for_another_triplet_is_not_installed(self) -> None:
        status = self.tree / "vcpkg/status"
        status.write_text(self.status_text().replace(f"Architecture: {TRIPLET}", "Architecture: arm64-linux"))
        self.assert_refused(self.collect(), "is not installed for x64-linux")

    def test_a_port_that_is_not_fully_installed(self) -> None:
        paragraphs = self.status_text().split("\n\n")
        changed = [p.replace("install ok installed", "install ok half-installed")
                   if p.startswith("Package: grpc\n") and "Feature:" not in p else p for p in paragraphs]
        (self.tree / "vcpkg/status").write_text("\n\n".join(changed))
        self.assert_refused(self.collect(), "dependency 'grpc' is not installed")

    def test_the_recorded_status_file_has_continuation_lines_and_they_are_read(self) -> None:
        abseil = [p for p in tool.parse_status(TREE / "vcpkg/status") if p.get("Package") == "abseil"]
        self.assertIn("\n", abseil[0]["Description"])

    def test_a_status_file_with_a_line_that_is_no_field(self) -> None:
        (self.tree / "vcpkg/status").write_text("garbage\n" + self.status_text())
        self.assert_refused(self.collect(), "line without a field: 'garbage'")
        (self.tree / "vcpkg/status").write_text("  indented first\n" + self.status_text())
        self.assert_refused(self.collect(), "line without a field")

    def test_a_closure_port_without_its_spdx_document(self) -> None:
        (self.tree / TRIPLET / "share/re2/vcpkg.spdx.json").unlink()
        self.assert_refused(self.collect(), "share/re2/vcpkg.spdx.json")

    def test_a_document_that_disagrees_with_the_status_file(self) -> None:
        path = self.tree / TRIPLET / "share/openssl/vcpkg.spdx.json"
        doc = json.loads(path.read_text())
        doc["packages"][0]["versionInfo"] = "3.0.0"
        path.write_text(json.dumps(doc))
        self.assert_refused(self.collect(), "openssl: the SPDX document records version '3.0.0'")

    def test_a_closure_port_with_no_cpe_decision(self) -> None:
        status = self.status_text().replace("Package: re2\n", "Package: re3\n")
        status = "\n".join(
            line.replace("re2", "re3") if line.startswith("Depends:") else line
            for line in status.splitlines()
        ) + "\n"
        (self.tree / "vcpkg/status").write_text(status)
        (self.tree / TRIPLET / "share/re2").rename(self.tree / TRIPLET / "share/re3")
        result = self.collect()
        self.assertEqual(result.returncode, 1)
        self.assertIn("re3: no CPE decision recorded", result.stderr)

    def test_a_build_configured_without_the_feature(self) -> None:
        cache = self.root / "CMakeCache.txt"
        cache.write_text("TP_ENABLE_STREAMING_GRPC:BOOL=OFF\n")
        self.assert_refused(self.collect("--cmake-cache", cache),
                            "does not record TP_ENABLE_STREAMING_GRPC:BOOL=ON")
        cache.write_text("X:STRING=1\nTP_ENABLE_STREAMING_GRPC:BOOL=ON\n")
        self.assertEqual(self.collect("--cmake-cache", cache).returncode, 0)

    def test_a_manifest_without_a_baseline_or_the_feature(self) -> None:
        manifest = self.root / "vcpkg.json"
        doc = json.loads(MANIFEST.read_text())
        del doc["builtin-baseline"]
        manifest.write_text(json.dumps(doc))
        self.assert_refused(self.collect(manifest=manifest), "names no builtin-baseline")
        doc = json.loads(MANIFEST.read_text())
        del doc["features"]["streaming-grpc"]
        manifest.write_text(json.dumps(doc))
        self.assert_refused(self.collect(manifest=manifest), "declares no feature 'streaming-grpc'")


class Gate(Fixture):
    def changed(self, change) -> pathlib.Path:
        doc = self.document()
        change(doc)
        path = self.root / "changed.spdx.json"
        path.write_text(json.dumps(doc))
        return path

    def assert_refused(self, result: subprocess.CompletedProcess, reason: str) -> None:
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(reason, result.stderr)

    def test_the_collected_document_passes(self) -> None:
        self.document()
        result = self.check()
        self.assertEqual((result.returncode, result.stderr), (0, ""))

    def test_an_absent_document_fails(self) -> None:
        self.assert_refused(self.check(self.root / "none.spdx.json"), "no SBOM at")

    def test_a_document_that_names_none_of_the_ports_the_release_links(self) -> None:
        def strip(doc: dict) -> None:
            doc["packages"] = doc["packages"][:1]
            doc["relationships"] = doc["relationships"][:1]
            doc["annotations"] = [a for a in doc["annotations"] if not a["comment"].startswith("closure: ")]
        result = self.check(self.changed(strip))
        self.assert_refused(result, "records no closure")
        for name in ("grpc", "protobuf", "nlohmann-json"):
            self.assertIn(f"closure does not name {name}", result.stderr)

    def test_the_live_manifest_feature_is_what_the_gate_requires(self) -> None:
        _, roots = tool.manifest_facts(MANIFEST, "streaming-grpc")
        self.assertEqual(sorted(roots), ["grpc", "nlohmann-json", "protobuf"])
        declared = json.loads(MANIFEST.read_text())["dependencies"]
        self.assertEqual(sorted(set(declared) - set(roots)), sorted(tool.NOT_LINKED))

    def test_a_closure_that_lists_a_port_no_package_describes(self) -> None:
        def drop(doc: dict) -> None:
            doc["packages"] = [p for p in doc["packages"] if p["name"] != "openssl"]
        self.assert_refused(self.check(self.changed(drop)),
                            "closure names openssl but no package describes it")

    def test_a_port_the_worker_is_not_recorded_as_linking(self) -> None:
        def unlink(doc: dict) -> None:
            doc["relationships"] = [r for r in doc["relationships"]
                                    if r["relatedSpdxElement"] != "SPDXRef-port-openssl"]
        self.assert_refused(self.check(self.changed(unlink)),
                            "openssl is not recorded as a build dependency")

    def test_a_port_related_to_the_worker_some_other_way_or_to_something_else(self) -> None:
        def retype(doc: dict) -> None:
            for r in doc["relationships"]:
                if r["relatedSpdxElement"] == "SPDXRef-port-openssl":
                    r["relationshipType"] = "STATIC_LINK"
        self.assert_refused(self.check(self.changed(retype)), "openssl is not recorded as a build dependency")

        def resource(doc: dict) -> None:
            for r in doc["relationships"]:
                if r["relatedSpdxElement"] == "SPDXRef-port-openssl":
                    r["spdxElementId"] = "SPDXRef-port-zlib"
        self.assert_refused(self.check(self.changed(resource)), "openssl is not recorded as a build dependency")

    def test_a_cpe_reference_that_is_not_a_cpe(self) -> None:
        def garble(doc: dict) -> None:
            for package in doc["packages"]:
                if package["name"] == "openssl":
                    for r in package["externalRefs"]:
                        if r["referenceType"] == "cpe23Type":
                            r["referenceLocator"] = "openssl 3"
        self.assert_refused(self.check(self.changed(garble)), "openssl lacks its exact native CPE identifier set")

    def test_a_document_whose_lists_are_not_lists(self) -> None:
        self.sbom.write_text(json.dumps({"spdxVersion": "SPDX-2.3", "annotations": 5}))
        self.assert_refused(self.check(), "annotations is not a list")

    def test_a_port_with_neither_a_cpe_nor_a_reason(self) -> None:
        def uncpe(doc: dict) -> None:
            for package in doc["packages"]:
                if package["name"] == "openssl":
                    package["externalRefs"] = [r for r in package["externalRefs"]
                                               if r["referenceType"] != "cpe23Type"]
        self.assert_refused(self.check(self.changed(uncpe)),
                            "openssl lacks its exact native CPE identifier set")

        def uncomment(doc: dict) -> None:
            for package in doc["packages"]:
                package.pop("comment", None)
        self.assert_refused(self.check(self.changed(uncomment)),
                            "nlohmann-json lacks its exact lookup-without-positive-control coverage declaration")

    def test_a_name_collision_is_not_accepted_as_a_native_identifier(self) -> None:
        def npm(doc: dict) -> None:
            for package in doc["packages"]:
                if package["name"] == "nlohmann-json":
                    for ref in package["externalRefs"]:
                        if ref["referenceType"] == "purl":
                            ref["referenceLocator"] = "pkg:npm/nlohmann-json@3.12.0"
        self.assert_refused(self.check(self.changed(npm)), "nlohmann-json lacks its versioned vcpkg purl")

    def test_an_unscanned_port_is_not_reported_as_scanned(self) -> None:
        def cpe(doc: dict) -> None:
            package = next(p for p in doc["packages"] if p["name"] == "re2")
            package["externalRefs"].append({"referenceType": "cpe23Type",
                                           "referenceLocator": "cpe:2.3:a:example:re2:1.0:*:*:*:*:*:*:*"})
        self.assert_refused(self.check(self.changed(cpe)), "re2 lacks its explicit UNSCANNED declaration")

        def erase(doc: dict) -> None:
            doc["annotations"] = [a for a in doc["annotations"] if not a["comment"].startswith("unscanned:")]
        self.assert_refused(self.check(self.changed(erase)), "unscanned coverage must name")

    def test_both_forward_lookup_identifiers_are_required(self) -> None:
        for missing in ("json-for-modern-cpp_project", "nlohmann"):
            with self.subTest(vendor=missing):
                def remove(doc: dict) -> None:
                    package = next(p for p in doc["packages"] if p["name"] == "nlohmann-json")
                    package["externalRefs"] = [r for r in package["externalRefs"]
                                               if f":{missing}:" not in r["referenceLocator"]]
                self.assert_refused(self.check(self.changed(remove)),
                                    "nlohmann-json lacks its exact native CPE identifier set")

    def test_forward_lookup_requires_its_explicit_state(self) -> None:
        for state in (None, "UNSCANNED: no identifier", "CONTROLLED: positive control verified"):
            with self.subTest(state=state):
                def replace(doc: dict) -> None:
                    package = next(p for p in doc["packages"] if p["name"] == "nlohmann-json")
                    if state is None:
                        del package["comment"]
                    else:
                        package["comment"] = state
                self.assert_refused(self.check(self.changed(replace)),
                                    "nlohmann-json lacks its exact lookup-without-positive-control coverage declaration")

    def test_all_coverage_groups_are_required_and_exact(self) -> None:
        for key in ("controlled", "lookup-without-positive-control", "unscanned"):
            for replacement in (None, "none", "nlohmann-json re2"):
                with self.subTest(key=key, replacement=replacement):
                    def change(doc: dict) -> None:
                        annotation = next(a for a in doc["annotations"] if a["comment"].startswith(key + ": "))
                        if replacement is None:
                            doc["annotations"].remove(annotation)
                        else:
                            annotation["comment"] = f"{key}: {replacement}"
                    self.assert_refused(self.check(self.changed(change)), f"{key} coverage must name")

    def test_a_conflicting_duplicate_coverage_declaration_is_rejected(self) -> None:
        def duplicate(doc: dict) -> None:
            declaration = next(a for a in doc["annotations"]
                               if a["comment"].startswith("controlled: "))
            conflict = dict(declaration, comment="controlled: nlohmann-json")
            doc["annotations"].insert(0, conflict)
        self.assert_refused(self.check(self.changed(duplicate)),
                            "controlled coverage must be declared exactly once")

    def test_a_plausible_but_wrong_cpe_vendor_is_rejected(self) -> None:
        def vendor(doc: dict) -> None:
            for package in doc["packages"]:
                if package["name"] == "protobuf":
                    for ref in package["externalRefs"]:
                        if ref["referenceType"] == "cpe23Type":
                            ref["referenceLocator"] = ref["referenceLocator"].replace(":google:", ":other:")
        self.assert_refused(self.check(self.changed(vendor)), "protobuf lacks its exact native CPE identifier set")

    def test_each_native_cpe_version_must_match_the_package_version(self) -> None:
        for name in CLOSURE:
            if isinstance(tool.CPE_DECISIONS[name], str):
                continue
            with self.subTest(port=name):
                def wrong_version(doc: dict) -> None:
                    package = next(p for p in doc["packages"] if p["name"] == name)
                    for ref in package["externalRefs"]:
                        if ref["referenceType"] == "cpe23Type":
                            fields = ref["referenceLocator"].split(":")
                            fields[5] = "99.99.99"
                            ref["referenceLocator"] = ":".join(fields)
                self.assert_refused(self.check(self.changed(wrong_version)),
                                    f"{name} lacks its exact native CPE identifier set")

    def test_a_port_without_a_version(self) -> None:
        def unversion(doc: dict) -> None:
            for package in doc["packages"]:
                if package["name"] == "zlib":
                    package["versionInfo"] = ""
        self.assert_refused(self.check(self.changed(unversion)), "zlib has no version")

    def test_a_document_made_for_another_baseline_feature_or_triplet(self) -> None:
        self.document()
        self.assert_refused(self.check(triplet="arm64-linux"),
                            "vcpkg-triplet is 'x64-linux', the release needs 'arm64-linux'")
        manifest = self.root / "vcpkg.json"
        doc = json.loads(MANIFEST.read_text())
        doc["builtin-baseline"] = "0" * 40
        doc["features"]["other"] = copy.deepcopy(doc["features"]["streaming-grpc"])
        manifest.write_text(json.dumps(doc))
        self.assert_refused(self.check(manifest=manifest), "vcpkg-baseline is")
        self.assert_refused(self.check(manifest=manifest, feature="other"),
                            "vcpkg-feature is 'streaming-grpc', the release needs 'other'")

    def test_a_file_that_is_not_an_spdx_document(self) -> None:
        self.sbom.write_text("{}")
        self.assert_refused(self.check(), "is not an SPDX-2.3 document")
        self.sbom.write_text("not json")
        self.assert_refused(self.check(), "SBOM ")


class ScannerControls(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.report = self.root / "report.json"

    def report_matches(self) -> dict:
        matches = []
        for artifact_id, name, identity in tool.control_entries():
            value = tool.cpe(identity.vendor, identity.product, identity.control_version)
            matches.append({
                "artifact": {"id": artifact_id, "name": name, "type": "vcpkg",
                             "purl": tool.purl(name, identity.control_version)},
                "vulnerability": {"id": identity.control_advisory},
                "matchDetails": [{"type": "cpe-match",
                                  "searchedBy": {"namespace": "nvd:cpe", "cpes": [value]},
                                  "found": {"vulnerabilityID": identity.control_advisory,
                                            "cpes": [tool.cpe(identity.vendor, identity.product, "*")]}}],
            })
        return {"matches": matches}

    def check(self, report: dict) -> subprocess.CompletedProcess:
        self.report.write_text(json.dumps(report))
        return run("check-control", "--report", self.report)

    def test_controls_cover_every_controlled_pair_at_its_affected_version(self) -> None:
        output = self.root / "controls.json"
        result = run("control", "--output", output)
        self.assertEqual(result.returncode, 0, result.stderr)
        doc = json.loads(output.read_text())
        pairs = set()
        for package in doc["packages"]:
            refs = package["externalRefs"]
            self.assertEqual(refs[0]["referenceLocator"],
                             tool.purl(package["name"], package["versionInfo"]))
            values = refs[1]["referenceLocator"].split(":")
            self.assertEqual(values[5], package["versionInfo"])
            pairs.add(tuple(values[3:5]))
        self.assertEqual(pairs, {(i.vendor, i.product) for _, _, i in tool.control_entries()})
        self.assertEqual(len(doc["packages"]), 9)
        self.assertEqual(len({p["SPDXID"] for p in doc["packages"]}), 9)
        self.assertIn("not recorded build evidence", doc["comment"])
        result = self.check(self.report_matches())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("9/9", result.stdout)

    def test_forward_lookups_have_no_fabricated_positive_controls(self) -> None:
        forward = tool.CPE_DECISIONS["nlohmann-json"]
        self.assertIsInstance(forward, tool.ForwardLookup)
        self.assertEqual(forward.pairs, (("json-for-modern-cpp_project", "json-for-modern-cpp"),
                                       ("nlohmann", "json")))
        self.assertNotIn("nlohmann-json", {name for _, name, _ in tool.control_entries()})
        self.assertNotIn("nlohmann-json", {p["name"] for p in tool.control_document()["packages"]})
        for index, (vendor, product) in enumerate(forward.pairs):
            with self.subTest(pair=(vendor, product)):
                report = self.report_matches()
                report["matches"].append({
                    "artifact": {"id": f"control-nlohmann-json-{index}", "name": "nlohmann-json",
                                 "type": "vcpkg", "purl": tool.purl("nlohmann-json", "1.0")},
                    "vulnerability": {"id": "CVE-2000-0000"},
                })
                result = self.check(report)
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("unexpected control artifact", result.stderr)

    def test_every_pair_must_independently_match(self) -> None:
        for missing in range(len(tool.control_entries())):
            with self.subTest(missing=missing):
                report = self.report_matches()
                removed = report["matches"].pop(missing)
                result = self.check(report)
                self.assertEqual(result.returncode, 1)
                self.assertIn(removed["artifact"]["id"], result.stderr)

    def test_name_match_or_wrong_identity_does_not_satisfy_a_cpe_control(self) -> None:
        mutations = (
            lambda m: m["artifact"].update(type=""),
            lambda m: m["artifact"].update(purl="pkg:npm/zlib@1.2.11"),
            lambda m: m["artifact"].update(id="another-control"),
            lambda m: m["vulnerability"].update(id="CVE-2000-0000"),
            lambda m: m["matchDetails"][0].update(type="exact-direct-match"),
            lambda m: m["matchDetails"][0]["searchedBy"].update(cpes=[]),
            lambda m: m["matchDetails"][0]["found"].update(cpes=[]),
        )
        for index, mutation in enumerate(mutations):
            with self.subTest(mutation=index):
                report = self.report_matches()
                mutation(report["matches"][0])
                result = self.check(report)
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("no typed native CPE match", result.stderr)

    def test_a_report_without_a_matches_list_is_not_success(self) -> None:
        result = self.check({})
        self.assertEqual(result.returncode, 1)
        self.assertIn("no matches list", result.stderr)


if __name__ == "__main__":
    unittest.main()
