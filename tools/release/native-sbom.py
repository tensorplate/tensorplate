#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Record and check the serving worker's native build dependency closure.

Versions, source checksums and licenses come from the vcpkg install tree.
The manifest and installed dependency records infer the closure; this
inventory does not assert which archive members the linker retains.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import pathlib
import re
import sys
from typing import NamedTuple
from urllib.parse import quote

TOOL = "tensorplate-native-sbom"
WORKER = "tensorplate-serving"
BASELINE_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
INSTALLED = "install ok installed"


class CPEIdentity(NamedTuple):
    vendor: str
    product: str
    control_version: str
    control_advisory: str


# Verified in Grype's v6.1.10 database built 2026-10-07T06:31:48Z, using
# cpes/affected_cpe_handles/blobs. Each emitted pair has a vulnerable control.
CPE_DECISIONS: dict[str, list[CPEIdentity] | str] = {
    "grpc": [CPEIdentity("grpc", "grpc", "1.51.0", "CVE-2023-1428")],
    "protobuf": [
        CPEIdentity("google", "protobuf", "3.14.0", "CVE-2021-22570"),
        CPEIdentity("google", "protobuf-cpp", "3.21.5", "CVE-2022-1941"),
    ],
    "openssl": [CPEIdentity("openssl", "openssl", "1.0.1", "CVE-2014-0160")],
    "c-ares": [
        CPEIdentity("c-ares", "c-ares", "1.26.0", "CVE-2024-25629"),
        CPEIdentity("c-ares_project", "c-ares", "1.26.0", "CVE-2024-25629"),
        CPEIdentity("daniel_stenberg", "c-ares", "1.3.2", "CVE-2007-3152"),
    ],
    "zlib": [CPEIdentity("zlib", "zlib", "1.2.11", "CVE-2018-25032")],
    "abseil": [
        CPEIdentity("abseil", "common_libraries", "20240722.0", "CVE-2025-0838"),
    ],
    "nlohmann-json": "no native CPE in the 2026-10-07 Grype database; the npm namesake is unrelated",
    "re2": "no native CPE in the 2026-10-07 Grype database; distro and npm namesakes do not identify this port",
    "utf8-range": "no package or CPE in the 2026-10-07 Grype database; protobuf's identifier does not cover this port",
}


# Manifest dependencies the worker does not link, with the reason; they are
# installed, left out of the closure, and named in the document as left out.
NOT_LINKED = {
    "gtest": "test framework; the release configuration builds no tests (TP_BUILD_TESTS=OFF)",
}


class Refused(Exception):
    """The reasons the command cannot do what was asked."""

    def __init__(self, reasons: list[str]):
        super().__init__("; ".join(reasons))
        self.reasons = reasons


# --- inputs ----------------------------------------------------------------


def read_json(path: pathlib.Path, what: str) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as err:
        raise Refused([f"{what} {path}: {err}"]) from None


def manifest_facts(manifest: pathlib.Path, feature: str) -> tuple[str, list[str]]:
    """The baseline and the port names the manifest and the feature depend on directly."""
    doc = read_json(manifest, "manifest")
    reasons: list[str] = []
    baseline = doc.get("builtin-baseline") if isinstance(doc, dict) else None
    if not isinstance(baseline, str) or not BASELINE_RE.match(baseline):
        reasons.append(f"{manifest} names no builtin-baseline of 40 lowercase hex digits")
    features = doc.get("features", {}) if isinstance(doc, dict) else {}
    entry = features.get(feature) if isinstance(features, dict) else None
    deps: list[str] = []
    listed = list(doc.get("dependencies", [])) if isinstance(doc, dict) else []
    if not isinstance(entry, dict):
        reasons.append(f"{manifest} declares no feature {feature!r}")
    else:
        of_feature = entry.get("dependencies", [])
        if not of_feature:
            reasons.append(f"{manifest}: feature {feature!r} names no dependency")
        listed += of_feature
    for dep in listed:
        name = dep.get("name") if isinstance(dep, dict) else dep
        if not isinstance(name, str) or not name:
            reasons.append(f"{manifest}: a dependency has no name")
        elif name not in NOT_LINKED and name not in deps:
            deps.append(name)
    if reasons:
        raise Refused(reasons)
    return baseline, deps


def dependency_name(spec: str) -> str:
    """`grpc[core]:x64-linux (>=1)` -> `grpc`."""
    return re.split(r"[\[:(\s]", spec.strip(), maxsplit=1)[0]


def parse_status(path: pathlib.Path) -> list[dict[str, str]]:
    """The paragraphs of vcpkg's status file, as field maps."""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as err:
        raise Refused([f"status file {path}: {err}"]) from None
    paragraphs: list[dict[str, str]] = []
    current: dict[str, str] = {}
    key = ""
    for line in text.splitlines():
        if not line.strip():
            if current:
                paragraphs.append(current)
                current = {}
            key = ""
            continue
        # A line that starts with blank space continues the field above it.
        if line[0] in " \t" and key:
            current[key] += "\n" + line.strip()
            continue
        if ":" not in line or line[0] in " \t":
            raise Refused([f"status file {path}: line without a field: {line!r}"])
        key, _, value = line.partition(":")
        key = key.strip()
        current[key] = value.strip()
    if current:
        paragraphs.append(current)
    return paragraphs


def installed_ports(status: list[dict[str, str]], triplet: str) -> dict[str, dict]:
    """Each installed port of the triplet: version, port version, abi and the names it depends on."""
    ports: dict[str, dict] = {}
    for paragraph in status:
        if paragraph.get("Architecture") != triplet or paragraph.get("Status") != INSTALLED:
            continue
        name = paragraph.get("Package", "")
        if not name:
            continue
        port = ports.setdefault(
            name, {"version": None, "port_version": "0", "abi": None, "depends": set()}
        )
        depends = {dependency_name(d) for d in paragraph.get("Depends", "").split(",") if d.strip()}
        port["depends"] |= depends - {name}
        if "Feature" in paragraph:
            continue
        port["version"] = paragraph.get("Version")
        port["port_version"] = paragraph.get("Port-Version", "0")
        port["abi"] = paragraph.get("Abi")
    return {name: port for name, port in ports.items() if port["version"] is not None}


def closure_of(roots: list[str], ports: dict[str, dict]) -> list[str]:
    """`roots` and everything they depend on, by name, sorted; tool ports excluded."""
    seen: set[str] = set()
    todo = list(roots)
    while todo:
        name = todo.pop()
        if name in seen or name.startswith("vcpkg-"):
            continue
        if name not in ports:
            raise Refused([f"the closure reaches {name}, which the status file does not record as installed"])
        seen.add(name)
        todo.extend(sorted(ports[name]["depends"]))
    return sorted(seen)


def full_version(port: dict) -> str:
    if port["port_version"] not in ("0", ""):
        return f"{port['version']}#{port['port_version']}"
    return port["version"]


def port_document(install_root: pathlib.Path, triplet: str, name: str) -> dict:
    path = install_root / triplet / "share" / name / "vcpkg.spdx.json"
    doc = read_json(path, "vcpkg SPDX document")
    if not isinstance(doc, dict) or not isinstance(doc.get("packages"), list):
        raise Refused([f"{path} is not an SPDX document with packages"])
    return doc


def spdx_package(doc: dict, spdx_id: str) -> dict | None:
    for package in doc["packages"]:
        if isinstance(package, dict) and package.get("SPDXID") == spdx_id:
            return package
    return None


def cpe(vendor: str, product: str, version: str) -> str:
    return f"cpe:2.3:a:{vendor}:{product}:{version}:*:*:*:*:*:*:*"


def archive_facts(archives: pathlib.Path | None, name: str, abi: str | None) -> dict | None:
    """The binary-cache archive of a port, by its abi, with its digest; asked for and absent refuses."""
    if archives is None:
        return None
    path = archives / str(abi)[:2] / f"{abi}.zip"
    if not abi or not path.is_file():
        raise Refused([f"{name}: no binary-cache archive for abi {abi} under {archives}"])
    return {"path": str(path.relative_to(archives)), **digest_of(path)}


def digest_of(path: pathlib.Path) -> dict:
    sha = hashlib.sha256()
    size = 0
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            sha.update(chunk)
            size += len(chunk)
    return {"sha256": sha.hexdigest(), "size_bytes": size}


def cmake_records_streaming(cache: pathlib.Path) -> None:
    try:
        lines = cache.read_text(encoding="utf-8").splitlines()
    except OSError as err:
        raise Refused([f"CMake cache {cache}: {err}"]) from None
    if "TP_ENABLE_STREAMING_GRPC:BOOL=ON" not in lines:
        raise Refused([f"{cache} does not record TP_ENABLE_STREAMING_GRPC:BOOL=ON"])


def validate_created(value: str) -> str:
    try:
        dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError:
        raise Refused(["created must be an explicit UTC timestamp (YYYY-MM-DDTHH:MM:SSZ)"]) from None
    return value


def purl(name: str, version: str) -> str:
    return f"pkg:vcpkg/{quote(name, safe='')}@{quote(version, safe='')}"


def purl_reference(name: str, version: str) -> dict:
    return {"referenceCategory": "PACKAGE-MANAGER", "referenceType": "purl",
            "referenceLocator": purl(name, version)}


# --- collect ----------------------------------------------------------------


def collect(args: argparse.Namespace) -> dict:
    created = validate_created(args.created)
    baseline, roots = manifest_facts(args.manifest, args.feature)
    if args.cmake_cache:
        cmake_records_streaming(args.cmake_cache)
    ports = installed_ports(parse_status(args.install_root / "vcpkg" / "status"), args.triplet)
    reasons: list[str] = []
    missing = [name for name in roots if name not in ports]
    if missing:
        raise Refused(
            [f"{args.install_root}: feature {args.feature!r} dependency {name!r} is not installed "
             f"for {args.triplet}" for name in missing]
        )
    closure = closure_of(roots, ports)
    left_out = sorted(name for name in NOT_LINKED if name in ports)
    documents: dict[str, dict] = {}
    for name in closure:
        try:
            documents[name] = port_document(args.install_root, args.triplet, name)
        except Refused as err:
            reasons += err.reasons
            continue
        port = spdx_package(documents[name], "SPDXRef-port")
        if port is None or port.get("versionInfo") != full_version(ports[name]):
            reasons.append(
                f"{name}: the SPDX document records version "
                f"{port.get('versionInfo') if port else None!r}, the status file "
                f"{full_version(ports[name])!r}"
            )
        if name not in CPE_DECISIONS:
            reasons.append(f"{name}: no CPE decision recorded in {pathlib.Path(__file__).name}")
    if reasons:
        raise Refused(reasons)

    creators = {f"Tool: {TOOL}"}
    packages: list[dict] = []
    relationships: list[dict] = []
    worker: dict = {
        "SPDXID": "SPDXRef-worker",
        "name": WORKER,
        "versionInfo": args.version or "NOASSERTION",
        "downloadLocation": "NOASSERTION",
        "filesAnalyzed": False,
        "licenseConcluded": "Apache-2.0",
        "licenseDeclared": "Apache-2.0",
        "copyrightText": "NOASSERTION",
    }
    if args.worker:
        try:
            worker["checksums"] = [
                {"algorithm": "SHA256", "checksumValue": digest_of(args.worker)["sha256"]}
            ]
        except OSError as err:
            raise Refused([f"worker binary {args.worker}: {err}"]) from None
    packages.append(worker)
    relationships.append(
        {"spdxElementId": "SPDXRef-DOCUMENT", "relationshipType": "DESCRIBES",
         "relatedSpdxElement": "SPDXRef-worker"}
    )
    for name in closure:
        doc = documents[name]
        for creator in doc.get("creationInfo", {}).get("creators", []):
            if isinstance(creator, str) and creator.startswith("Tool: vcpkg"):
                creators.add(creator)
        port = spdx_package(doc, "SPDXRef-port") or {}
        resource = spdx_package(doc, "SPDXRef-resource-0") or {}
        spdx_id = f"SPDXRef-port-{name}"
        refs: list[dict] = [purl_reference(name, full_version(ports[name]))]
        decision = CPE_DECISIONS[name]
        if isinstance(decision, str):
            cpe_note = decision
        else:
            cpe_note = None
            for identity in decision:
                version = ports[name]["version"]
                refs.append({"referenceCategory": "SECURITY", "referenceType": "cpe23Type",
                             "referenceLocator": cpe(identity.vendor, identity.product, version)})
        refs.append({"referenceCategory": "OTHER", "referenceType": "vcpkg-port",
                     "referenceLocator": port.get("downloadLocation", "NOASSERTION")})
        if ports[name]["abi"]:
            refs.append({"referenceCategory": "OTHER", "referenceType": "vcpkg-abi",
                         "referenceLocator": ports[name]["abi"]})
        archive = archive_facts(args.archives, name, ports[name]["abi"])
        if archive:
            refs.append({"referenceCategory": "OTHER", "referenceType": "vcpkg-binary-cache-archive",
                         "referenceLocator": f"{archive['path']}?sha256={archive['sha256']}"
                                             f"&size={archive['size_bytes']}"})
        package = {
            "SPDXID": spdx_id,
            "name": name,
            "versionInfo": full_version(ports[name]),
            "downloadLocation": resource.get("downloadLocation", "NOASSERTION"),
            "filesAnalyzed": False,
            "licenseConcluded": port.get("licenseConcluded", "NOASSERTION"),
            "licenseDeclared": port.get("licenseDeclared", "NOASSERTION"),
            "copyrightText": "NOASSERTION",
            "externalRefs": refs,
        }
        if resource.get("checksums"):
            package["checksums"] = resource["checksums"]
        notes = []
        if name == "nlohmann-json":
            notes.append("Header-only build dependency; code is compiled from its headers.")
        if cpe_note:
            notes.append(f"UNSCANNED: {cpe_note}")
        if notes:
            package["comment"] = " ".join(notes)
        packages.append(package)
        relationships.append(
            {"spdxElementId": "SPDXRef-worker", "relationshipType": "DEPENDS_ON",
             "relatedSpdxElement": spdx_id}
        )
    annotations = {
        "vcpkg-baseline": baseline,
        "vcpkg-feature": args.feature,
        "vcpkg-triplet": args.triplet,
        "closure": " ".join(closure),
        "unscanned": " ".join(name for name in closure if isinstance(CPE_DECISIONS[name], str)) or "none",
        "not-linked": "; ".join(f"{name} ({NOT_LINKED[name]})" for name in left_out) or "none",
    }
    # Named by what the document says, so the same build yields the same name.
    identity = hashlib.sha256(json.dumps(
        [annotations, [(p["name"], p["versionInfo"], p.get("checksums")) for p in packages]],
        sort_keys=True).encode()).hexdigest()
    return {
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": f"{WORKER}-native-closure-{args.triplet}",
        "documentNamespace": f"https://tensorplate.com/spdxdocs/{WORKER}-native-closure/"
                             f"{args.triplet}/{baseline}/sha256-{identity}",
        "creationInfo": {"created": created, "creators": sorted(creators)},
        "comment": "Build dependency closure inferred from the manifest and vcpkg install tree, "
                   "with versions, source checksums, licenses and scanner identifiers. DEPENDS_ON "
                   "does not assert archive members survived linking. UNSCANNED ports have no "
                   "native identifier in the inspected scanner database; zero findings for them "
                   "are not a clean vulnerability result.",
        "annotations": [
            {"annotator": f"Tool: {TOOL}", "annotationDate": created, "annotationType": "OTHER",
             "comment": f"{key}: {value}"}
            for key, value in annotations.items()
        ],
        "packages": packages,
        "relationships": relationships,
    }


# --- check ------------------------------------------------------------------


def document_annotations(doc: dict) -> dict[str, str]:
    facts: dict[str, str] = {}
    for annotation in doc.get("annotations", []):
        comment = annotation.get("comment", "") if isinstance(annotation, dict) else ""
        key, sep, value = comment.partition(": ")
        if sep:
            facts[key] = value
    return facts


def check(args: argparse.Namespace) -> list[str]:
    """Every reason the document is not the gate's SBOM for this manifest, or an empty list."""
    baseline, roots = manifest_facts(args.manifest, args.feature)
    if not args.sbom.is_file():
        return [f"no SBOM at {args.sbom}"]
    try:
        doc = read_json(args.sbom, "SBOM")
    except Refused as err:
        return err.reasons
    reasons: list[str] = []
    if not isinstance(doc, dict) or doc.get("spdxVersion") != "SPDX-2.3":
        return [f"{args.sbom} is not an SPDX-2.3 document"]
    for key in ("annotations", "packages", "relationships"):
        if not isinstance(doc.get(key), list):
            return [f"{args.sbom}: {key} is not a list"]
    facts = document_annotations(doc)
    for key, want in (("vcpkg-baseline", baseline), ("vcpkg-feature", args.feature),
                      ("vcpkg-triplet", args.triplet)):
        if facts.get(key) != want:
            reasons.append(f"{args.sbom}: {key} is {facts.get(key)!r}, the release needs {want!r}")
    packages = {p.get("name"): p for p in doc.get("packages", []) if isinstance(p, dict)}
    linked = {
        r.get("relatedSpdxElement")
        for r in doc.get("relationships", [])
        if isinstance(r, dict) and r.get("spdxElementId") == "SPDXRef-worker"
        and r.get("relationshipType") == "DEPENDS_ON"
    }
    closure = facts.get("closure", "").split()
    if not closure:
        reasons.append(f"{args.sbom}: records no closure")
    for name in roots:
        if name not in closure:
            reasons.append(f"{args.sbom}: closure does not name {name}, which the manifest or {args.feature} depends on")
    for name in closure:
        package = packages.get(name)
        if package is None:
            reasons.append(f"{args.sbom}: closure names {name} but no package describes it")
            continue
        if package.get("SPDXID") not in linked:
            reasons.append(f"{args.sbom}: {name} is not recorded as a build dependency of the worker")
        if not package.get("versionInfo"):
            reasons.append(f"{args.sbom}: {name} has no version")
        refs = [r for r in package.get("externalRefs", []) if isinstance(r, dict)]
        recorded_purls = [r.get("referenceLocator") for r in refs if r.get("referenceType") == "purl"]
        if recorded_purls != [purl(name, package.get("versionInfo", ""))]:
            reasons.append(f"{args.sbom}: {name} lacks its versioned vcpkg purl")
        decision = CPE_DECISIONS.get(name)
        recorded_cpes = [str(r.get("referenceLocator", "")) for r in refs
                         if r.get("referenceType") == "cpe23Type"]
        if isinstance(decision, str):
            if recorded_cpes or f"UNSCANNED: {decision}" not in str(package.get("comment", "")):
                reasons.append(f"{args.sbom}: {name} lacks its explicit UNSCANNED declaration")
        elif decision:
            version = package.get("versionInfo", "").split("#", 1)[0]
            expected_cpes = sorted(cpe(identity.vendor, identity.product, version) for identity in decision)
            if sorted(recorded_cpes) != expected_cpes:
                reasons.append(f"{args.sbom}: {name} lacks its exact native CPE identifier set")
        else:
            reasons.append(f"{args.sbom}: {name} has no scanner identifier decision")
    unscanned = " ".join(name for name in closure if isinstance(CPE_DECISIONS.get(name), str)) or "none"
    if facts.get("unscanned") != unscanned:
        reasons.append(f"{args.sbom}: unscanned coverage must name {unscanned!r}")
    return reasons


# --- scanner controls -------------------------------------------------------


def control_entries() -> list[tuple[str, str, CPEIdentity]]:
    return [(f"control-{name}-{index}", name, identity)
            for name, decision in sorted(CPE_DECISIONS.items()) if isinstance(decision, list)
            for index, identity in enumerate(decision)]


def control_document() -> dict:
    packages = []
    for artifact_id, name, identity in control_entries():
        packages.append({
            "SPDXID": f"SPDXRef-{artifact_id}", "name": name,
            "versionInfo": identity.control_version, "downloadLocation": "NOASSERTION",
            "filesAnalyzed": False, "licenseConcluded": "NOASSERTION",
            "licenseDeclared": "NOASSERTION", "copyrightText": "NOASSERTION",
            "externalRefs": [purl_reference(name, identity.control_version),
                             {"referenceCategory": "SECURITY", "referenceType": "cpe23Type",
                              "referenceLocator": cpe(identity.vendor, identity.product,
                                                      identity.control_version)}],
        })
    identity = hashlib.sha256(json.dumps(packages, sort_keys=True).encode()).hexdigest()
    return {
        "spdxVersion": "SPDX-2.3", "dataLicense": "CC0-1.0", "SPDXID": "SPDXRef-DOCUMENT",
        "name": "native-closure-scan-controls",
        "documentNamespace": f"https://tensorplate.com/spdxdocs/native-scan-controls/sha256-{identity}",
        "creationInfo": {"created": "2026-10-07T00:00:00Z", "creators": [f"Tool: {TOOL}"]},
        "comment": "Constructed scanner controls, not recorded build evidence. Each native CPE "
                   "vendor/product pair is checked at a known affected version from the database.",
        "packages": packages,
        "relationships": [{"spdxElementId": "SPDXRef-DOCUMENT", "relationshipType": "DESCRIBES",
                           "relatedSpdxElement": p["SPDXID"]} for p in packages],
    }


def check_control(report: pathlib.Path) -> list[str]:
    doc = read_json(report, "scanner control report")
    if not isinstance(doc, dict) or not isinstance(doc.get("matches"), list):
        return ["scanner control report has no matches list"]
    reasons = []
    for artifact_id, name, identity in control_entries():
        expected_cpe = cpe(identity.vendor, identity.product, identity.control_version)
        found = False
        for match in doc["matches"]:
            if not isinstance(match, dict):
                continue
            artifact = match.get("artifact", {})
            vulnerability = match.get("vulnerability", {})
            if (artifact.get("id") != artifact_id or artifact.get("name") != name
                    or artifact.get("type") != "vcpkg"
                    or artifact.get("purl") != purl(name, identity.control_version)
                    or vulnerability.get("id") != identity.control_advisory):
                continue
            for detail in match.get("matchDetails", []):
                searched = detail.get("searchedBy", {})
                matched = detail.get("found", {})
                pairs = {":".join(value.split(":")[3:5]) for value in matched.get("cpes", [])}
                if (detail.get("type") == "cpe-match"
                        and searched.get("namespace") == "nvd:cpe"
                        and expected_cpe in searched.get("cpes", [])
                        and f"{identity.vendor}:{identity.product}" in pairs
                        and matched.get("vulnerabilityID") == identity.control_advisory):
                    found = True
                    break
        if not found:
            reasons.append(f"control {artifact_id}: no typed native CPE match for "
                           f"{identity.vendor}:{identity.product} at {identity.control_version} "
                           f"({identity.control_advisory})")
    return reasons


# --- main -------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)

    p = commands.add_parser("collect", help="write the closure's SPDX document from an install tree")
    p.add_argument("--install-root", type=pathlib.Path, required=True, help="the vcpkg_installed directory")
    p.add_argument("--manifest", type=pathlib.Path, required=True, help="vcpkg.json")
    p.add_argument("--feature", default="streaming-grpc")
    p.add_argument("--triplet", required=True)
    p.add_argument("--output", type=pathlib.Path, required=True)
    p.add_argument("--worker", type=pathlib.Path, help="the worker binary, for its digest")
    p.add_argument("--cmake-cache", type=pathlib.Path, help="refuse unless it records the feature ON")
    p.add_argument("--archives", type=pathlib.Path, help="the binary cache, for each port's archive digest")
    p.add_argument("--version", help="the worker's version, recorded on its package")
    p.add_argument("--created", required=True, help="stable creation time, YYYY-MM-DDTHH:MM:SSZ")

    p = commands.add_parser("check", help="fail unless the document is the gate's SBOM for this manifest")
    p.add_argument("--sbom", type=pathlib.Path, required=True)
    p.add_argument("--manifest", type=pathlib.Path, required=True)
    p.add_argument("--feature", default="streaming-grpc")
    p.add_argument("--triplet", required=True)

    p = commands.add_parser("control", help="generate one vulnerable control per native CPE pair")
    p.add_argument("--output", type=pathlib.Path, required=True)

    p = commands.add_parser("check-control", help="require every native CPE control to match")
    p.add_argument("--report", type=pathlib.Path, required=True)

    args = parser.parse_args(argv)
    try:
        if args.command == "collect":
            doc = collect(args)
            args.output.write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")
            print(f"{TOOL}: {args.output}: {len(doc['packages']) - 1} ports in the closure")
        elif args.command == "control":
            doc = control_document()
            args.output.write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")
            print(f"{TOOL}: {len(doc['packages'])} native CPE pair controls written")
        elif args.command == "check-control":
            reasons = check_control(args.report)
            if reasons:
                raise Refused(reasons)
            count = len(control_entries())
            print(f"{TOOL}: native CPE controls verified: {count}/{count}")
        else:
            reasons = check(args)
            if reasons:
                raise Refused(reasons)
            print(f"{TOOL}: {args.sbom} covers the manifest's closure with {args.feature} for {args.triplet}")
    except Refused as err:
        for reason in err.reasons:
            print(f"{TOOL}: {reason}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
