#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The serving worker's native closure: its SBOM and the check on it.

The worker links gRPC, protobuf, OpenSSL and the rest of the `streaming-grpc`
feature's closure statically from the vcpkg baseline, and compiles against
the manifest's nlohmann-json. What was installed for it is recorded by the
build itself: vcpkg writes `vcpkg/status` under its install
tree and an SPDX document per installed port under `share/<port>/`. This
tool reads those, never a version list of its own.

collect  reads a build's install tree and writes one SPDX 2.3 JSON document
         for the closure of the manifest's dependencies and the feature's,
         less the ports the worker does not link. It refuses a tree that
         lacks any of those ports, a port without vcpkg's SPDX document, a
         closure port without a CPE decision, and a CMake cache that does
         not record the streaming feature ON.
check    fails unless the document exists, was made for the manifest's
         baseline, feature and triplet, and names every port the manifest
         and the feature name, each with a CPE or a recorded reason for
         having none. It checks the document's shape and its agreement with
         the manifest; it cannot tell which build a document came from.

Exit 0: done. Exit 1: refused, with each reason on stderr. Exit 2: usage.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import pathlib
import re
import sys

TOOL = "tensorplate-native-sbom"
WORKER = "tensorplate-serving"
BASELINE_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
INSTALLED = "install ok installed"

# CPE vendor and product for each closure port, verified against the NVD CPE
# dictionary (services.nvd.nist.gov/rest/json/cpes/2.0, cpeMatchString) on
# 2026-10-07. `version` says which version the CPE carries: the vcpkg port's
# version without its port-version, or the upstream release the port's
# resource names when the two numberings differ. A port the dictionary has no
# entry for is listed with None and the fact, so it is named in the document
# and not scanned by CPE; a closure port absent from this table refuses the
# document, because an unmapped port would be silently unscanned.
CPE_DECISIONS: dict[str, list[tuple[str, str, str]] | str] = {
    "grpc": [("grpc", "grpc", "port")],
    # NVD numbers protobuf by its upstream release (33.4), the port by the
    # Python-style version (6.33.4).
    "protobuf": [("google", "protobuf", "resource")],
    "openssl": [("openssl", "openssl", "port")],
    # NVD has used both vendors for c-ares over the years.
    "c-ares": [("c-ares", "c-ares", "port"), ("c-ares_project", "c-ares", "port")],
    "zlib": [("zlib", "zlib", "port")],
    # NVD has entries under both names for nlohmann's JSON library.
    "nlohmann-json": [("json-for-modern-cpp_project", "json-for-modern-cpp", "port"),
                      ("nlohmann", "json", "port")],
    "abseil": "no entry in the NVD CPE dictionary for abseil under any vendor on 2026-10-07",
    "re2": "no entry in the NVD CPE dictionary for re2 under any vendor on 2026-10-07",
    "utf8-range": (
        "no entry in the NVD CPE dictionary for utf8-range under any vendor on 2026-10-07; "
        "it is built from protobuf's source tree, whose CPE is on the protobuf package"
    ),
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


def resource_version(doc: dict, name: str) -> str:
    """The upstream release the port's first source resource names, as a bare version."""
    resource = spdx_package(doc, "SPDXRef-resource-0")
    location = resource.get("downloadLocation", "") if resource else ""
    ref = location.rsplit("@", 1)[1] if "@" in location else ""
    match = re.match(r"^(?:[A-Za-z-]+[-_])?v?([0-9][0-9A-Za-z.]*)$", ref)
    if not match:
        raise Refused([f"{name}: cannot read an upstream version from resource {location!r}"])
    return match.group(1)


def cpe_version(name: str, source: str, port: dict, doc: dict) -> str:
    """The version a port's CPE carries; an upstream numbering must be the tail of the port's."""
    if source == "port":
        return port["version"]
    upstream = resource_version(doc, name)
    if port["version"] != upstream and not port["version"].endswith("." + upstream):
        raise Refused([f"{name}: its source resource names {upstream}, which is not the port "
                       f"version {port['version']} or its tail"])
    return upstream


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


def created_now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- collect ----------------------------------------------------------------


def collect(args: argparse.Namespace) -> dict:
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
        refs: list[dict] = []
        decision = CPE_DECISIONS[name]
        if isinstance(decision, str):
            cpe_note = decision
        else:
            cpe_note = None
            for vendor, product, source in decision:
                version = cpe_version(name, source, ports[name], doc)
                refs.append({"referenceCategory": "SECURITY", "referenceType": "cpe23Type",
                             "referenceLocator": cpe(vendor, product, version)})
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
        if cpe_note:
            package["comment"] = f"no CPE: {cpe_note}"
        packages.append(package)
        relationships.append(
            {"spdxElementId": "SPDXRef-worker", "relationshipType": "STATIC_LINK",
             "relatedSpdxElement": spdx_id}
        )
    annotations = {
        "vcpkg-baseline": baseline,
        "vcpkg-feature": args.feature,
        "vcpkg-triplet": args.triplet,
        "closure": " ".join(closure),
        "not-linked": "; ".join(f"{name} ({NOT_LINKED[name]})" for name in left_out) or "none",
    }
    created = args.created or created_now()
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
        "comment": "The vcpkg ports the serving worker links or compiles against, read from the "
                   "install tree of a build: each port's version, source, license and CPE. "
                   "Annotations record the manifest baseline, feature and triplet, and the "
                   "installed ports left out because the worker does not link them.",
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
        and r.get("relationshipType") == "STATIC_LINK"
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
            reasons.append(f"{args.sbom}: {name} is not recorded as statically linked by the worker")
        if not package.get("versionInfo"):
            reasons.append(f"{args.sbom}: {name} has no version")
        has_cpe = any(
            r.get("referenceType") == "cpe23Type" and str(r.get("referenceLocator", "")).startswith("cpe:2.3:a:")
            for r in package.get("externalRefs", []) if isinstance(r, dict)
        )
        if not has_cpe and not str(package.get("comment", "")).startswith("no CPE: "):
            reasons.append(f"{args.sbom}: {name} has neither a CPE nor a recorded reason for none")
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
    p.add_argument("--created", help="the document's creation time (default: now, UTC)")

    p = commands.add_parser("check", help="fail unless the document is the gate's SBOM for this manifest")
    p.add_argument("--sbom", type=pathlib.Path, required=True)
    p.add_argument("--manifest", type=pathlib.Path, required=True)
    p.add_argument("--feature", default="streaming-grpc")
    p.add_argument("--triplet", required=True)

    args = parser.parse_args(argv)
    try:
        if args.command == "collect":
            doc = collect(args)
            args.output.write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")
            print(f"{TOOL}: {args.output}: {len(doc['packages']) - 1} ports in the closure")
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
