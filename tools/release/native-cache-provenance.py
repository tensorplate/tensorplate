#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Record native dependency-cache provenance and verify its binding to an SBOM."""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import importlib.util
import json
import pathlib
import re
import shutil
import subprocess
import sys

spec = importlib.util.spec_from_file_location("native_sbom", pathlib.Path(__file__).with_name("native-sbom.py"))
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)
Refused = native.Refused
ARCHES = {"x64-linux": "amd64", "arm64-linux": "arm64"}
WORKFLOWS = {".github/workflows/release-dependencies.yml", ".github/workflows/release.yml"}
LIMITATION = ("Provisioning ran outside Actions, so no workflow run or cache entry is recorded. "
              "The stamp's archives_sha256 hashes sorted archive names, not contents; "
              "the archives list records content digests separately.")


def need(condition: object, message: str) -> None:
    if not condition:
        raise Refused([message])


def matches(pattern: str, value: object) -> bool:
    return isinstance(value, str) and re.fullmatch(pattern, value) is not None


def timestamp(value: object) -> dt.datetime:
    need(matches(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{1,9})?Z", value), "invalid UTC timestamp")
    try:
        base, separator, fraction = value[:-1].partition(".")
        # Python 3.10 accepts three or six fractional digits; GitHub logs use seven.
        normalized = base + ("." + (fraction + "000000")[:6] if separator else "")
        return dt.datetime.fromisoformat(normalized + "+00:00")
    except ValueError:
        raise Refused(["invalid UTC timestamp"]) from None


def time_ns(value: str) -> int:
    parsed = timestamp(value)
    fraction = value.partition(".")[2].removesuffix("Z")
    return int(parsed.replace(microsecond=0).timestamp()) * 10**9 + int((fraction + "0" * 9)[:9])


def manifest_identity(path: pathlib.Path) -> tuple[str, str]:
    baseline, _ = native.manifest_facts(path, "streaming-grpc")
    doc = native.read_json(path, "manifest")
    for key in ("version", "version-string", "version-semver", "version-date"):
        doc.pop(key, None)
    canonical = json.dumps(doc, sort_keys=True, separators=(",", ":"))
    return baseline, canonical


def command(args: list[str]) -> str:
    for attempt in range(2):
        try:
            result = subprocess.run(args, capture_output=True, text=True, timeout=60, check=False)
        except (OSError, subprocess.TimeoutExpired):
            break
        if result.returncode == 0:
            return result.stdout
        if attempt or args[:2] != ["gh", "api"] or "pass --allow-escape-sequences" not in result.stderr:
            break
        # Newer gh refuses escape bytes even when captured. Nothing raw is emitted.
        args = [*args, "--allow-escape-sequences"]
    raise Refused([f"{args[0]} could not provide provenance evidence"])


def cache_key(args: argparse.Namespace) -> str:
    baseline, canonical = manifest_identity(args.manifest)
    compiler = shutil.which("clang++")
    need(compiler, "clang++ is not on PATH")
    version = command(["clang", "--version"]).splitlines()[0]
    digests = [native.digest_of(path)["sha256"] for path in
               (pathlib.Path(compiler).resolve(), args.profile, args.action)]
    digest = hashlib.sha256("\n".join((canonical, version, *digests)).encode()).hexdigest()
    return f"vcpkg-release-Linux-x64-linux-{baseline}-{digest}"


def archive_list(root: pathlib.Path, allow_empty: bool = False) -> list[dict]:
    need(not root.is_symlink(), "archive root is a symlink")
    if allow_empty and not root.exists():
        return []
    need(root.is_dir(), "archive directory is absent")
    result = []
    for path in sorted(root.rglob("*")):
        need(not path.is_symlink(), "archive tree contains a symlink")
        if path.is_file():
            relative = path.relative_to(root).as_posix()
            need(matches(r"([0-9a-f]{2})/\1[0-9a-f]{62}\.zip", relative), "unexpected archive path")
            result.append({"path": relative, **native.digest_of(path)})
    need(result or allow_empty, "archive directory is empty")
    return result


def names_digest(archives: list[dict]) -> str:
    return hashlib.sha256("".join(a["path"] + "\n" for a in archives).encode()).hexdigest()


def api(repository: str, endpoint: str, field: str, **query: str) -> list[dict]:
    argv = ["gh", "api", "--method", "GET", f"repos/{repository}/actions/{endpoint}",
            "--paginate", "--slurp", "-f", "per_page=100"]
    for key, value in query.items():
        argv.extend(["-f", f"{key}={value}"])
    try:
        pages = json.loads(command(argv))
    except ValueError:
        raise Refused(["GitHub returned malformed provenance evidence"]) from None
    need(isinstance(pages, list), "GitHub returned no provenance pages")
    rows = []
    for page in pages:
        need(isinstance(page, dict) and isinstance(page.get(field), list), "GitHub evidence has no records list")
        if field == "workflow_runs":
            need(type(page.get("total_count")) is int and page["total_count"] <= 1000,
                 "workflow-run query exceeds GitHub's searchable result bound")
        rows.extend(page[field])
    need(all(isinstance(row, dict) for row in rows), "GitHub evidence contains a malformed row")
    return rows


def select_cache(args: argparse.Namespace) -> dict:
    restore_started = time_ns(getattr(args, "restore_started", None))
    need(matches(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repository) and ".." not in args.repository, "invalid repository")
    need(matches(r"refs/(heads|tags)/[A-Za-z0-9_./-]+", args.ref) and ".." not in args.ref, "invalid ref")
    need(matches(r"[A-Za-z0-9_./-]+", args.default_branch) and ".." not in args.default_branch,
         "invalid default branch")
    need(matches(r"vcpkg-release-Linux-x64-linux-[0-9a-f]{40}-[0-9a-f]{64}", args.cache_key), "invalid cache key")
    entries = []
    for ref in dict.fromkeys((args.ref, f"refs/heads/{args.default_branch}")):
        entries = [e for e in api(args.repository, "caches", "actions_caches", key=args.cache_key, ref=ref)
                   if e.get("key") == args.cache_key and e.get("ref") == ref]
        if entries:
            break
    need(len(entries) == 1, "expected exactly one cache entry for the exact key and eligible ref")
    entry = entries[0]
    need(time_ns(entry.get("created_at")) <= restore_started,
         "selected cache entry was created after restore started; its source cannot be established")
    need(type(entry.get("id")) is int and entry["id"] > 0
         and matches(r"[0-9a-f]{64}", entry.get("version")), "invalid cache identity")
    return {key: entry[key] for key in ("id", "key", "ref", "version", "created_at")}


def actions_source(args: argparse.Namespace) -> dict:
    identity = getattr(args, "restored_cache_id", None)
    need(type(identity) is int and identity > 0, "restored cache id must be a positive integer")
    entry = select_cache(args)
    need(entry["id"] == identity, "selected cache differs from the cache selected before restore")
    created = timestamp(entry["created_at"])
    start = (created - dt.timedelta(days=1)).strftime("%Y-%m-%dT%H:%M:%SZ")
    end = (created + dt.timedelta(seconds=60)).strftime("%Y-%m-%dT%H:%M:%SZ")
    branch = entry["ref"].split("/", 2)[2]
    runs = api(args.repository, "runs", "workflow_runs", branch=branch, status="success", created=f"{start}..{end}")
    saved = []
    for run in runs:
        if (run.get("path") not in WORKFLOWS or run.get("conclusion") != "success"
                or run.get("head_branch") != branch):
            continue
        need(type(run.get("id")) is int and run["id"] > 0, "workflow run has no valid id")
        jobs = api(args.repository, f"runs/{run['id']}/jobs", "jobs", filter="all")
        for job in jobs:
            if job.get("conclusion") != "success":
                continue
            began, ended = timestamp(job.get("started_at")), timestamp(job.get("completed_at"))
            if not began <= created < ended + dt.timedelta(seconds=1):
                continue
            need(type(job.get("id")) is int and job["id"] > 0, "workflow job has no valid id")
            logs = command(["gh", "api", f"repos/{args.repository}/actions/jobs/{job['id']}/logs"])
            pattern = r"(?m)^(\S+) Cache saved with key: " + re.escape(args.cache_key) + r"\r?$"
            for match in re.finditer(pattern, logs):
                if 0 <= time_ns(match[1]) - time_ns(entry["created_at"]) <= 60 * 10**9:
                    saved.append({"workflow": run["path"], "run_id": run["id"],
                                  "run_attempt": job.get("run_attempt"), "job_id": job["id"],
                                  "head_sha": run.get("head_sha"), "saved_at": match[1]})
    need(len(saved) == 1, "expected exactly one successful job with a matching cache-save log line")
    return {"repository": args.repository, "restore_started": args.restore_started,
            "cache": {key: entry.get(key) for key in ("id", "key", "ref", "version", "created_at")},
            "saved_by": saved[0]}


def runner_source(args: argparse.Namespace, archives: list[dict], baseline: str, canonical: str) -> dict:
    fields = ("baseline", "dependencies_sha256", "triplet", "feature", "vcpkg_version", "compiler",
              "provisioned_at", "archives_sha256")
    stamp = {}
    for line in args.stamp.read_text(encoding="utf-8").splitlines():
        key, separator, value = line.partition("=")
        need(separator and key in fields and key not in stamp and value and value.isprintable(), "malformed provisioning stamp")
        stamp[key] = value
    need(set(stamp) == set(fields), "provisioning stamp is incomplete")
    expected = {"baseline": baseline, "dependencies_sha256": hashlib.sha256(canonical.encode()).hexdigest(),
                "triplet": args.triplet, "feature": "streaming-grpc", "archives_sha256": names_digest(archives)}
    need(all(stamp[k] == v for k, v in expected.items()), "provisioning stamp does not match manifest, triplet or archive names")
    timestamp(stamp["provisioned_at"])
    for key in ("compiler", "vcpkg_version"):
        stamp[key + "_sha256"] = hashlib.sha256(stamp.pop(key).encode()).hexdigest()
    return {"stamp": stamp, "limitation": LIMITATION}


def check_record(record: object, args: argparse.Namespace) -> None:
    need(isinstance(record, dict), "provenance record is not an object")
    baseline, canonical = manifest_identity(args.manifest)
    common = {"schema_version", "architecture", "triplet", "feature", "baseline", "commit", "version",
              "created", "sbom_sha256", "archives", "source"}
    source = record.get("source")
    additions = {"actions-cache": {"repository", "restore_started", "cache", "saved_by"}, "runner-filesystem": {"stamp", "limitation"}}
    need(isinstance(source, str) and source in additions, "unsupported provenance source")
    need(set(record) == common | additions[source], "unexpected provenance record fields")
    expected = {"schema_version": 1, "architecture": ARCHES[args.triplet], "triplet": args.triplet,
                "feature": "streaming-grpc", "baseline": baseline, "version": args.version,
                "sbom_sha256": native.digest_of(args.sbom)["sha256"]}
    need(all(type(record.get(k)) is type(v) and record[k] == v for k, v in expected.items()),
         "provenance identity or SBOM digest mismatch")
    need(matches(r"[0-9a-f]{40}", record["commit"]), "invalid source commit")
    need(matches(r"[0-9]+\.[0-9]+\.[0-9]+(?:-rc\.[1-9][0-9]*)?", record["version"]), "invalid release version")
    timestamp(record["created"])
    archives = record["archives"]
    need(isinstance(archives, list) and archives, "provenance has no archives")
    previous = ""
    for archive in archives:
        need(isinstance(archive, dict) and set(archive) == {"path", "sha256", "size_bytes"}, "malformed archive record")
        need(matches(r"([0-9a-f]{2})/\1[0-9a-f]{62}\.zip", archive["path"])
             and archive["path"] > previous, "archive paths are invalid, duplicate or unsorted")
        need(matches(r"[0-9a-f]{64}", archive["sha256"]) and type(archive["size_bytes"]) is int
             and archive["size_bytes"] > 0, "invalid archive digest or size")
        previous = archive["path"]
    available = {f"{a['path']}?sha256={a['sha256']}&size={a['size_bytes']}" for a in archives}
    sbom = native.read_json(args.sbom, "SBOM")
    packages = sbom.get("packages") if isinstance(sbom, dict) else None
    need(isinstance(packages, list) and len(packages) > 1, "SBOM has no native packages")
    workers = [p for p in packages if isinstance(p, dict) and p.get("SPDXID") == "SPDXRef-worker"]
    need(len(workers) == 1, "SBOM must name exactly one worker")
    for package in packages:
        need(isinstance(package, dict), "malformed SBOM package")
        if package.get("SPDXID") == "SPDXRef-worker":
            need(package.get("versionInfo") == args.version, "SBOM worker version mismatch")
            checksums = package.get("checksums", [])
            need(isinstance(checksums, list) and len(checksums) == 1 and isinstance(checksums[0], dict)
                 and checksums[0].get("algorithm") == "SHA256"
                 and matches(r"[0-9a-f]{64}", checksums[0].get("checksumValue")), "SBOM worker has no SHA256 digest")
            continue
        refs = package.get("externalRefs", [])
        need(isinstance(refs, list), "malformed SBOM archive references")
        selected = [r.get("referenceLocator") for r in refs if isinstance(r, dict)
                    and r.get("referenceType") == "vcpkg-binary-cache-archive"]
        need(len(selected) == 1 and isinstance(selected[0], str) and selected[0] in available,
             "SBOM archive reference does not match provenance")
    if source == "actions-cache":
        need(args.triplet == "x64-linux" and matches(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", record["repository"]) and ".." not in record["repository"], "invalid Actions source")
        cache, saved = record["cache"], record["saved_by"]
        need(isinstance(cache, dict) and set(cache) == {"id", "key", "ref", "version", "created_at"}, "malformed cache identity")
        need(isinstance(saved, dict) and set(saved) == {"workflow", "run_id", "run_attempt", "job_id", "head_sha", "saved_at"}, "malformed cache writer")
        need(type(cache["id"]) is int and cache["id"] > 0 and matches(r"[0-9a-f]{64}", cache["version"])
             and matches(r"vcpkg-release-Linux-x64-linux-" + baseline + r"-[0-9a-f]{64}", cache["key"])
             and matches(r"refs/(heads|tags)/[A-Za-z0-9_./-]+", cache["ref"]) and ".." not in cache["ref"], "invalid cache identity")
        need(saved["workflow"] in WORKFLOWS and matches(r"[0-9a-f]{40}", saved["head_sha"])
             and all(type(saved[k]) is int and saved[k] > 0 for k in ("run_id", "run_attempt", "job_id")), "invalid cache writer")
        need(time_ns(cache["created_at"]) <= time_ns(record["restore_started"]),
             "selected cache entry was created after restore started; its source cannot be established")
        need(0 <= time_ns(saved["saved_at"]) - time_ns(cache["created_at"]) <= 60 * 10**9,
             "cache-save log timestamp is outside its cache creation interval")
    else:
        stamp = record["stamp"]
        expected_stamp = {"baseline": baseline, "dependencies_sha256": hashlib.sha256(canonical.encode()).hexdigest(),
                          "triplet": args.triplet, "feature": "streaming-grpc", "archives_sha256": names_digest(archives)}
        need(args.triplet == "arm64-linux" and record["limitation"] == LIMITATION
             and isinstance(stamp, dict) and set(stamp) == set(expected_stamp) | {"provisioned_at", "compiler_sha256", "vcpkg_version_sha256"}, "malformed runner stamp projection")
        need(all(stamp[k] == v for k, v in expected_stamp.items()) and
             all(matches(r"[0-9a-f]{64}", stamp[k]) for k in ("compiler_sha256", "vcpkg_version_sha256")), "runner stamp identity mismatch")
        timestamp(stamp["provisioned_at"])


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    key = commands.add_parser("key", help="compute the hosted action's unchanged cache key")
    key.add_argument("--manifest", type=pathlib.Path, required=True)
    key.add_argument("--profile", type=pathlib.Path, required=True)
    key.add_argument("--action", type=pathlib.Path, required=True)
    cache = commands.add_parser("cache", help="record the eligible cache identity before restoring")
    cache.add_argument("--output", type=pathlib.Path, required=True)
    for flag in ("cache-key", "ref", "default-branch", "repository", "restore-started"):
        cache.add_argument("--" + flag, required=True)
    snapshot = commands.add_parser("snapshot", help="digest archives before an optional cache may be extended")
    snapshot.add_argument("--archives", type=pathlib.Path, required=True)
    snapshot.add_argument("--output", type=pathlib.Path, required=True)
    for command_name in ("record", "check"):
        cmd = commands.add_parser(command_name)
        cmd.add_argument("--manifest", type=pathlib.Path, required=True)
        cmd.add_argument("--triplet", choices=ARCHES, required=True)
        cmd.add_argument("--version", required=True)
        cmd.add_argument("--sbom", type=pathlib.Path, required=True)
        if command_name == "check":
            cmd.add_argument("--record", type=pathlib.Path, required=True)
            continue
        cmd.add_argument("--source", choices=("actions-cache", "runner-filesystem"), required=True)
        cmd.add_argument("--archives", type=pathlib.Path, required=True)
        cmd.add_argument("--created", required=True)
        cmd.add_argument("--commit", required=True)
        cmd.add_argument("--output", type=pathlib.Path, required=True)
        cmd.add_argument("--stamp", type=pathlib.Path)
        cmd.add_argument("--restored-cache-id", type=int)
        for flag in ("cache-key", "ref", "default-branch", "repository", "restore-started"):
            cmd.add_argument("--" + flag)
    args = parser.parse_args(argv)
    try:
        if args.command == "key":
            print(cache_key(args))
        elif args.command == "cache":
            args.output.write_text(json.dumps(select_cache(args)) + "\n", encoding="utf-8")
        elif args.command == "snapshot":
            args.output.write_text(json.dumps(archive_list(args.archives, allow_empty=True)) + "\n", encoding="utf-8")
        elif args.command == "check":
            check_record(native.read_json(args.record, "provenance"), args)
        else:
            required = ("stamp",) if args.source == "runner-filesystem" else ("cache_key", "ref", "default_branch", "repository", "restore_started", "restored_cache_id")
            missing = ["--" + field.replace("_", "-") for field in required if not getattr(args, field)]
            need(not missing, "required source arguments are absent: " + ", ".join(missing))
            baseline, canonical = manifest_identity(args.manifest)
            archives = archive_list(args.archives)
            source = actions_source(args) if args.source == "actions-cache" else runner_source(args, archives, baseline, canonical)
            record = {"schema_version": 1, "architecture": ARCHES[args.triplet], "triplet": args.triplet,
                      "feature": "streaming-grpc", "baseline": baseline, "commit": args.commit, "version": args.version,
                      "created": native.validate_created(args.created), "sbom_sha256": native.digest_of(args.sbom)["sha256"],
                      "archives": archives, "source": args.source, **source}
            check_record(record, args)
            args.output.write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    except (Refused, OSError, ValueError, TypeError, KeyError, IndexError) as error:
        print("native-cache-provenance: " + (str(error) if isinstance(error, Refused) else "malformed or unreadable provenance input"), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
