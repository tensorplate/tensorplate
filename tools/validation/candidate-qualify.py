#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Qualify one candidate speech bundle on the machine this runs on.

The recipe deploys an operator-named predecessor bundle, deploys the
candidate, samples warm-idle memory, runs the inventory's fixtures over the
serving worker's `/infer` route, samples memory under that load, drives the
negative paths, checks status, tears the candidate down by rollback and
writes one record conforming to
`config/schemas/candidate_qualification_record.json`.

Memory comes only from the platform sampler behind
`tools/validation/memory-sample.sh`; this tool never reads a device itself.
A record describes a candidate run on one machine and is never presented as
Production evidence. See docs/validation/speech-candidate-recipes.md.
"""

from __future__ import annotations

import argparse
import base64
import datetime as _dt
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))

import candidate_audio as audio  # noqa: E402
import candidate_record as record_mod  # noqa: E402

try:
    import tensorplate  # noqa: F401
except ImportError:
    sys.path.insert(0, str(REPO_ROOT / "sdk/python/src"))

from tensorplate.client import http_request  # noqa: E402
from tensorplate.errors import (  # noqa: E402
    ProtocolError,
    RequestTimeoutError,
    ServingError,
    TransportError,
)
from tensorplate.serving import ServingClient  # noqa: E402
from tensorplate.tensors import TensorInput  # noqa: E402

DEFAULT_INVENTORY = REPO_ROOT / "test/models/speech/candidate_fixture_inventory.json"
AUDIO_FRAMES = "audio_frames"
TEXT_UTF8 = "text_utf8"
RESULT_JSON = "result_json"
CLI_TIMEOUT_S = 600
UNDECLARED_LANGUAGE_CANDIDATES = ("zz", "qq", "xx")
UNDECLARED_VOICE = "undeclared_voice"
MAX_RESULT_JSON_BYTES = 1 << 20


class RecipeError(Exception):
    """The run cannot continue; the record written so far says how far it got."""


def utc_now() -> str:
    return _dt.datetime.now(tz=_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_duration_ms(text: str, flag: str) -> int:
    if text.endswith("ms") and text[:-2].isdigit():
        value = int(text[:-2])
    elif text.endswith("s") and text[:-1].isdigit():
        value = int(text[:-1]) * 1000
    else:
        raise RecipeError(
            f"{flag} takes a positive integer with an `s` or `ms` suffix, not {text!r}"
        )
    if value <= 0:
        raise RecipeError(f"{flag} must be positive")
    return value


def source_commit(explicit: str | None) -> str | None:
    if explicit:
        return explicit
    try:
        out = subprocess.run(
            ["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    value = out.stdout.strip()
    return value if out.returncode == 0 and len(value) == 40 else None


# --- bundle reading ----------------------------------------------------------


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise RecipeError(f"cannot read {path} as JSON ({exc.__class__.__name__})") from exc
    if not isinstance(value, dict):
        raise RecipeError(f"{path} is not a JSON object")
    return value


def read_bundle(bundle_dir: Path) -> dict[str, Any]:
    """The manifest, the runner entry and every artifact's verified digest."""
    manifest_path = bundle_dir / "manifest.json"
    manifest = read_json(manifest_path)
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, list) or not artifacts:
        raise RecipeError(f"{manifest_path} lists no artifacts")
    recorded = []
    entry_path = None
    for artifact in artifacts:
        rel = artifact.get("path")
        if not isinstance(rel, str) or not rel or rel.startswith("/") or ".." in Path(rel).parts:
            raise RecipeError(f"{manifest_path}: artifact path {rel!r} is not a safe relative path")
        file_path = bundle_dir / rel
        digest = record_mod.sha256_file(file_path)
        if digest != artifact.get("digest"):
            raise RecipeError(
                f"{rel}: digest {digest} does not match the manifest's {artifact.get('digest')!r}"
            )
        size = file_path.stat().st_size
        if size != artifact.get("byte_size"):
            raise RecipeError(
                f"{rel}: byte_size {size} does not match the manifest's "
                f"{artifact.get('byte_size')!r}"
            )
        recorded.append(
            {
                "path": rel,
                "role": artifact.get("role"),
                "kind": artifact.get("kind"),
                "digest": digest,
                "byte_size": size,
            }
        )
        if artifact.get("role") == "model" and artifact.get("kind") == "python_pytorch_entry":
            if entry_path is not None:
                raise RecipeError(f"{manifest_path} lists more than one runner entry")
            entry_path = rel
    if entry_path is None:
        raise RecipeError(f"{manifest_path} lists no python_pytorch_entry model artifact")
    entry = read_json(bundle_dir / entry_path)
    profile = entry.get("backend_profile")
    if not isinstance(profile, str) or not profile:
        raise RecipeError(
            f"{entry_path} declares no backend_profile; this tool qualifies candidate entries only"
        )
    return {
        "dir": bundle_dir,
        "manifest": manifest,
        "manifest_digest": record_mod.sha256_file(manifest_path),
        "artifacts": recorded,
        "entry_path": entry_path,
        "entry": entry,
        "backend_profile": profile,
    }


def stage_variant(source: dict[str, Any], staging_dir: Path, name: str, mutate) -> Path:
    """Copy the bundle, apply `mutate(bundle_dir, manifest)` and make it readable to the agent."""
    target = staging_dir / name
    if target.exists():
        shutil.rmtree(target)
    shutil.copytree(source["dir"], target)
    manifest = read_json(target / "manifest.json")
    mutate(target, manifest)
    (target / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    for root, dirs, files in os.walk(target):
        os.chmod(root, 0o755)
        for d in dirs:
            os.chmod(Path(root) / d, 0o755)
        for f in files:
            os.chmod(Path(root) / f, 0o644)
    return target


def corrupt_one_artifact(source: dict[str, Any]):
    candidates = [a for a in source["artifacts"] if a["path"] != source["entry_path"]]
    if not candidates:
        raise RecipeError("the bundle lists no artifact besides its entry to corrupt")
    victim = max(candidates, key=lambda a: a["byte_size"])["path"]

    def mutate(bundle_dir: Path, manifest: dict[str, Any]) -> None:
        path = bundle_dir / victim
        data = bytearray(path.read_bytes())
        if not data:
            raise RecipeError(f"{victim} is empty and cannot be corrupted by flipping a byte")
        data[-1] ^= 0x01
        path.write_bytes(bytes(data))

    return mutate


def repin_entry(source: dict[str, Any], change) -> Any:
    def mutate(bundle_dir: Path, manifest: dict[str, Any]) -> None:
        entry_path = bundle_dir / source["entry_path"]
        entry = read_json(entry_path)
        change(entry)
        data = (json.dumps(entry, indent=2) + "\n").encode("utf-8")
        entry_path.write_bytes(data)
        for artifact in manifest["artifacts"]:
            if artifact["path"] == source["entry_path"]:
                artifact["digest"] = record_mod.sha256_digest(data)
                artifact["byte_size"] = len(data)

    return mutate


# --- inventory ---------------------------------------------------------------


def load_inventory(path: Path, suite: str, profile: str) -> dict[str, Any]:
    inventory = read_json(path)
    if inventory.get("convention") != record_mod.CONVENTION:
        raise RecipeError(f"{path}: convention is not {record_mod.CONVENTION}")
    suites = inventory.get("suites")
    if not isinstance(suites, dict) or suite not in suites:
        raise RecipeError(f"{path}: no {suite} suite")
    selected = suites[suite]
    if profile not in selected.get("backend_profiles", []):
        raise RecipeError(f"{path}: the {suite} suite does not list backend_profile {profile!r}")
    return selected


def suite_for_profile(inventory_path: Path, profile: str) -> str:
    inventory = read_json(inventory_path)
    matches = [
        name
        for name, suite in inventory.get("suites", {}).items()
        if profile in suite.get("backend_profiles", [])
    ]
    if len(matches) != 1:
        raise RecipeError(
            f"{inventory_path}: backend_profile {profile!r} belongs to {len(matches)} suites, "
            "not one"
        )
    return matches[0]


def prepare_stt_clips(
    suite: dict[str, Any], clips_dir: Path | None, entry_rate: int
) -> list[dict[str, Any]]:
    """Read, verify and derive every clip; every one leaves here pinned and at the entry rate."""
    by_id: dict[str, dict[str, Any]] = {}
    prepared = []
    for clip in suite.get("clips", []):
        clip_id = clip["id"]
        if clip.get("derived_from") is None:
            if clips_dir is None:
                raise RecipeError("--clips is required: the inventory lists provisioned clips")
            path = clips_dir / f"{clip_id}.wav"
            if not path.is_file():
                raise RecipeError(f"clip {clip_id}: {path} is missing")
            digest = record_mod.sha256_file(path)
            if clip.get("digest") is None:
                raise RecipeError(
                    f"clip {clip_id} is unpinned in the inventory; pin its digest before qualifying"
                )
            if digest != clip["digest"]:
                raise RecipeError(
                    f"clip {clip_id}: digest {digest} does not match the inventory's pin"
                )
            rate, samples = audio.read_wav_pcm16_mono(path)
            if rate != clip["sample_rate_hz"]:
                raise RecipeError(
                    f"clip {clip_id}: WAV rate {rate} differs from the inventory's "
                    f"{clip['sample_rate_hz']}"
                )
        else:
            source = by_id.get(clip["derived_from"])
            if source is None:
                raise RecipeError(
                    f"clip {clip_id} derives from {clip['derived_from']!r}, "
                    "which is not listed before it"
                )
            derivation = clip.get("derivation") or {}
            kind = derivation.get("kind")
            if kind == "seeded_noise_mix":
                samples = audio.seeded_noise_mix(
                    source["samples"],
                    seed=int(derivation["seed"]),
                    snr_db=int(derivation["snr_db"]),
                )
                rate = source["rate"]
            elif kind == "telephony_8k_ulaw":
                if source["rate"] != 16000:
                    raise RecipeError(
                        f"clip {clip_id}: the telephony derivation needs a 16 kHz source"
                    )
                samples = audio.telephony_8k_ulaw(source["samples"])
                rate = 8000
            else:
                raise RecipeError(f"clip {clip_id}: unknown derivation {kind!r}")
            if rate != clip["sample_rate_hz"]:
                raise RecipeError(
                    f"clip {clip_id}: derived rate {rate} differs from the inventory's "
                    f"{clip['sample_rate_hz']}"
                )
            digest = record_mod.sha256_digest(audio.wav_pcm16_mono_bytes(rate, samples))
            if clip.get("digest") is None:
                raise RecipeError(
                    f"clip {clip_id} is unpinned in the inventory; its derived digest is {digest}"
                )
            if digest != clip["digest"]:
                raise RecipeError(
                    f"clip {clip_id}: derived digest {digest} does not match the inventory's pin"
                )
        bounds = clip.get("duration_s") or {}
        seconds = len(samples) / rate
        if not (bounds.get("min", 0) <= seconds <= bounds.get("max", float("inf"))):
            raise RecipeError(
                f"clip {clip_id}: {seconds:.2f} s is outside the inventory's duration bounds"
            )
        by_id[clip_id] = {"rate": rate, "samples": samples}
        request_samples = samples
        resampled_from = None
        resampler = None
        if rate != entry_rate:
            if rate * 2 == entry_rate:
                request_samples = audio.interpolate_2x(samples)
            elif rate == entry_rate * 2:
                request_samples = audio.decimate_2x(samples)
            else:
                raise RecipeError(
                    f"clip {clip_id}: no pinned resampling from {rate} Hz "
                    f"to the entry's {entry_rate} Hz"
                )
            resampled_from = rate
            resampler = audio.RESAMPLER_ID
        prepared.append(
            {
                "id": clip_id,
                "stratum": clip["stratum"],
                "language": clip["language"],
                "voice": None,
                "digest": digest,
                "derived_from": clip.get("derived_from"),
                "derivation": clip.get("derivation"),
                "request_samples": request_samples,
                "input": {
                    "sample_rate_hz": entry_rate,
                    "samples": len(request_samples),
                    "duration_us": audio.duration_us(len(request_samples), entry_rate),
                    "resampled_from_hz": resampled_from,
                    "resampler": resampler,
                    "text_bytes": None,
                    "text_chars": None,
                },
            }
        )
    if not prepared:
        raise RecipeError("the inventory's stt suite lists no clips")
    return prepared


def prepare_tts_texts(suite: dict[str, Any], entry: dict[str, Any]) -> list[dict[str, Any]]:
    language = suite.get("language")
    voice = suite.get("voice")
    if entry.get("language") != language or entry.get("voice") != voice:
        raise RecipeError(
            f"the entry selects language {entry.get('language')!r} and voice "
            f"{entry.get('voice')!r}; the inventory's tts suite is {language!r}/{voice!r}"
        )
    prepared = []
    for text in suite.get("texts", []):
        data = text["text"].encode("utf-8")
        digest = record_mod.sha256_digest(data)
        if digest != text.get("digest"):
            raise RecipeError(
                f"text {text['id']}: digest {digest} does not match the inventory's pin"
            )
        prepared.append(
            {
                "id": text["id"],
                "stratum": text["stratum"],
                "language": language,
                "voice": voice,
                "digest": digest,
                "derived_from": None,
                "derivation": None,
                "request_text": data,
                "input": {
                    "sample_rate_hz": None,
                    "samples": None,
                    "duration_us": None,
                    "resampled_from_hz": None,
                    "resampler": None,
                    "text_bytes": len(data),
                    "text_chars": len(text["text"]),
                },
            }
        )
    if not prepared:
        raise RecipeError("the inventory's tts suite lists no texts")
    return prepared


# --- the recipe --------------------------------------------------------------


class Recipe:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.evidence_dir = Path(args.evidence_dir)
        self.outputs_dir = self.evidence_dir / "outputs"
        self.staging_dir = (
            Path(args.staging_dir) if args.staging_dir else self.evidence_dir / "staging"
        )
        self.candidate = read_bundle(Path(args.candidate_bundle))
        self.predecessor = (
            read_bundle(Path(args.predecessor_bundle)) if args.predecessor_bundle else None
        )
        inventory_path = Path(args.inventory)
        self.suite_name = suite_for_profile(inventory_path, self.candidate["backend_profile"])
        self.suite = load_inventory(
            inventory_path, self.suite_name, self.candidate["backend_profile"]
        )
        self.error_codes = record_mod.load_error_codes()
        self.deployment_id = args.deployment_id
        self.predecessor_id = args.predecessor_deployment_id or f"{args.deployment_id}-predecessor"
        self.client: ServingClient | None = None
        self.serving_origin: str | None = None
        self.last_deploy_failure_code: str | None = None
        self.log_lines: list[str] = []
        entry = dict(self.candidate["entry"])
        entry.pop("artifact_set", None)
        pending = (
            "pending hardware" if args.provenance == "recorded" else "not run in this environment"
        )
        self.record = record_mod.new_record(
            provenance=args.provenance,
            recorded_at_utc=utc_now(),
            source_commit=source_commit(args.source_commit),
            transport_preference=args.transport,
            request_timeout_ms=args.request_timeout_ms,
            subject={
                "suite": self.suite_name,
                "backend_profile": self.candidate["backend_profile"],
                "bundle": {
                    "name": str(self.candidate["manifest"].get("name")),
                    "version": str(self.candidate["manifest"].get("version")),
                    "format_version": str(self.candidate["manifest"].get("format_version")),
                    "manifest_digest": self.candidate["manifest_digest"],
                    "entry_path": self.candidate["entry_path"],
                },
                "artifacts": self.candidate["artifacts"],
                "entry": entry,
            },
            deployment_id=self.deployment_id,
            pending_reason=pending,
        )
        if self.suite_name == "stt":
            rate = self.candidate["entry"].get("sample_rate_hz")
            if not isinstance(rate, int) or rate <= 0:
                raise RecipeError("the stt entry declares no positive sample_rate_hz")
            self.fixtures = prepare_stt_clips(
                self.suite, Path(args.clips) if args.clips else None, rate
            )
        else:
            self.fixtures = prepare_tts_texts(self.suite, self.candidate["entry"])
        self.window_ms = parse_duration_ms(args.window, "--window")
        self.interval_ms = parse_duration_ms(args.sample_interval, "--sample-interval")
        self.pids = self._explicit_pids()

    # -- logging and files --

    def log(self, message: str) -> None:
        line = f"{utc_now()} {message}"
        self.log_lines.append(line)
        sys.stderr.write(line + "\n")

    def write_record(self, stop_reason: str | None = None) -> None:
        record_mod.finalize(self.record, stop_reason)
        record_mod.validate_record(self.record)
        record_mod.validate_error_codes(self.record, self.error_codes)
        (self.evidence_dir / "record.json").write_text(
            record_mod.dump(self.record), encoding="utf-8"
        )
        (self.evidence_dir / "run.log").write_text(
            "\n".join(self.log_lines) + "\n", encoding="utf-8"
        )

    # -- the CLI --

    def cli(self, *argv: str, timeout_s: int = CLI_TIMEOUT_S) -> tuple[dict[str, Any], int]:
        command = [self.args.tensorplate, *argv, "--output", "json"]
        started = time.monotonic_ns()
        try:
            completed = subprocess.run(
                command, capture_output=True, text=True, timeout=timeout_s, check=False
            )
        except FileNotFoundError as exc:
            raise RecipeError(f"{self.args.tensorplate!r} is not runnable") from exc
        except subprocess.TimeoutExpired as exc:
            raise RecipeError(f"`{' '.join(argv)}` did not finish within {timeout_s} s") from exc
        wall_ms = (time.monotonic_ns() - started) // 1_000_000
        # A failed deploy prints its ok envelope, failure included, on stdout
        # and a second error envelope on stderr; a refused command prints
        # only the stderr one.
        envelope = None
        for stream in (completed.stdout, completed.stderr):
            try:
                envelope = json.loads(stream)
                break
            except ValueError:
                continue
        if envelope is None:
            raise RecipeError(
                f"`{' '.join(argv)}` printed no JSON envelope (exit {completed.returncode})"
            )
        if not isinstance(envelope, dict) or "status" not in envelope:
            raise RecipeError(f"`{' '.join(argv)}` printed an envelope without a status")
        self.log(
            f"cli {argv[0]}: status={envelope['status']} exit={completed.returncode} "
            f"wall_ms={wall_ms}"
        )
        return envelope, wall_ms

    @staticmethod
    def envelope_error(envelope: dict[str, Any]) -> dict[str, Any] | None:
        error = envelope.get("error")
        if isinstance(error, dict) and isinstance(error.get("code"), str):
            return {"code": error["code"], "message": str(error.get("message", ""))}
        return None

    def deploy(self, bundle_dir: Path, deployment_id: str) -> dict[str, Any]:
        envelope, wall_ms = self.cli(
            "deploy",
            str(bundle_dir),
            "--deployment-id",
            deployment_id,
            "--wait-timeout-ms",
            str(self.args.deploy_wait_timeout_ms),
        )
        payload = envelope.get("payload") if isinstance(envelope.get("payload"), dict) else {}
        failure = payload.get("failure") if isinstance(payload.get("failure"), dict) else None
        typed = None
        if failure is not None and isinstance(failure.get("error_code"), str):
            typed = {"code": failure["error_code"], "message": str(failure.get("message", ""))}
        elif envelope.get("status") != "ok":
            typed = self.envelope_error(envelope)
        phase = payload.get("phase")
        ok = envelope.get("status") == "ok" and phase == "active" and typed is None
        return {
            "deployment_id": deployment_id,
            "transaction_id": payload.get("transaction_id")
            if isinstance(payload.get("transaction_id"), str)
            else None,
            "phase": phase if isinstance(phase, str) else None,
            "bundle_digest": payload.get("bundle_digest")
            if isinstance(payload.get("bundle_digest"), str)
            else None,
            "wall_ms": wall_ms,
            "status": "ok" if ok else "failed",
            "failure": typed,
        }

    def status_snapshot(
        self, expected_deployment: str, after_failed_deploy: str | None = None
    ) -> dict[str, Any]:
        """Status and health as the agent and worker report them, judged against the expected id.

        A failed deploy leaves the agent `degraded` with that failure as its `last_error`
        until the next successful promote or rollback, so a snapshot taken after the deploy
        negatives passes `after_failed_deploy` (the last observed code) and accepts that state.
        """
        envelope, _ = self.cli("status")
        payload = envelope.get("payload") if isinstance(envelope.get("payload"), dict) else {}
        agent = payload.get("agent") if isinstance(payload.get("agent"), dict) else {}
        active = agent.get("active") if isinstance(agent.get("active"), dict) else {}
        serving_url = active.get("serving_url")
        health_state = None
        health_model = None
        if isinstance(serving_url, str) and serving_url:
            self.connect(serving_url)
            try:
                health = self.client.health()  # type: ignore[union-attr]
                health_state = health.state
                health_model = health.active_model_id
            except (TransportError, ProtocolError) as exc:
                self.log(f"health probe failed: {exc.__class__.__name__}")
        last_error = None
        raw_error = agent.get("last_error")
        if isinstance(raw_error, dict) and isinstance(raw_error.get("code"), str):
            last_error = {"code": raw_error["code"], "message": str(raw_error.get("message", ""))}
        agent_state = agent.get("agent_state")
        if after_failed_deploy is None:
            state_ok = agent_state == "ready" and last_error is None
        else:
            state_ok = agent_state in ("ready", "degraded") and (
                last_error is not None and last_error["code"] == after_failed_deploy
            )
        checks = {
            "agent_state_as_expected": state_ok,
            "active_deployment_matches": active.get("deployment_id") == expected_deployment,
            "active_backend_python_pytorch": active.get("backend") == "python_pytorch",
            "serving_url_reported": isinstance(serving_url, str) and bool(serving_url),
            "health_ready": health_state == "ready",
            "health_active_model_matches": health_model == expected_deployment,
        }
        return {
            "agent_state": agent_state if isinstance(agent_state, str) else None,
            "last_error": last_error,
            "active_deployment_id": active.get("deployment_id")
            if isinstance(active.get("deployment_id"), str)
            else None,
            "active_backend": active.get("backend")
            if isinstance(active.get("backend"), str)
            else None,
            "health_state": health_state,
            "health_active_model_id": health_model,
            "checks": checks,
        }

    def connect(self, serving_url: str) -> None:
        if self.client is not None and self.client.endpoint.url == serving_url:
            return
        self.client = ServingClient(
            serving_url,
            timeout=self.args.request_timeout_ms / 1000 + 5,
            discover=False,
            preferred_transport=self.args.transport,
        )
        self.serving_origin = self.client.endpoint.origin  # type: ignore[attr-defined]

    def rollback(self) -> dict[str, Any]:
        envelope, wall_ms = self.cli("rollback", "--reason", "candidate qualification teardown")
        payload = envelope.get("payload") if isinstance(envelope.get("payload"), dict) else {}
        restored = payload.get("restored_deployment_id")
        ok = envelope.get("status") == "ok" and restored == self.predecessor_id
        error = self.envelope_error(envelope)
        return {
            "method": "rollback",
            "status": "ok" if ok else "failed",
            "restored_deployment_id": restored if isinstance(restored, str) else None,
            "wall_ms": wall_ms,
            "reason": None
            if ok
            else (
                f"{error['code']}: {error['message']}"
                if error
                else f"restored {restored!r}, expected {self.predecessor_id!r}"
            ),
        }

    # -- processes and memory --

    def _explicit_pids(self) -> list[dict[str, Any]]:
        pids = []
        for item in self.args.pid:
            pid, _, role = item.partition(":")
            if (
                role not in ("agent", "serving_worker", "python_sidecar", "external")
                or not pid.isdigit()
                or int(pid) <= 0
            ):
                raise RecipeError(f"--pid takes <pid>:<role> with a known role, not {item!r}")
            pids.append({"pid": int(pid), "role": role})
        return pids

    def discover_pids(self) -> list[dict[str, Any]]:
        if self.pids:
            return list(self.pids)
        try:
            out = subprocess.run(
                ["systemctl", "show", "-p", "MainPID", "--value", self.args.agent_unit],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            return []
        agent_pid = out.stdout.strip()
        if out.returncode != 0 or not agent_pid.isdigit() or int(agent_pid) <= 0:
            return []
        found = [{"pid": int(agent_pid), "role": "agent"}]
        for worker in children_of(int(agent_pid)):
            found.append({"pid": worker, "role": "serving_worker"})
            found.extend(
                {"pid": sidecar, "role": "python_sidecar"} for sidecar in children_of(worker)
            )
        return found

    def sampler_processes(self, domain: str, pids: list[dict[str, Any]]) -> list[str]:
        roles = (
            ("python_sidecar",)
            if domain == "device_vram"
            else ("agent", "serving_worker", "python_sidecar")
        )
        return [f"{p['pid']}:{p['role']}" for p in pids if p["role"] in roles]

    def start_window(self, name: str, phase: str) -> dict[str, Any] | None:
        if not self.args.sampler:
            return None
        pids = self.discover_pids()
        self.record["memory"]["processes"] = pids
        if not any(p["role"] == "python_sidecar" for p in pids):
            self.record["memory"]["windows"][name] = record_mod.empty_window(
                name, "no python_sidecar PID was discovered or given"
            )
            return None
        self.record["memory"]["sampler"] = {
            "entry": record_mod.SAMPLER_ENTRY,
            "interval_ms": self.interval_ms,
            "duration_ms": self.window_ms,
        }
        handles = {}
        for domain in ("device_vram", "guest_ram"):
            out_path = self.evidence_dir / "memory" / f"{name}-{domain}.jsonl"
            out_path.parent.mkdir(parents=True, exist_ok=True)
            command = [
                str(REPO_ROOT / record_mod.SAMPLER_ENTRY),
                self.args.sampler,
                "--interval",
                self.args.sample_interval,
                "--duration",
                self.args.window,
                "--out",
                str(out_path),
                "--phase",
                phase,
                "--domain",
                domain,
            ]
            for process in self.sampler_processes(domain, pids):
                command += ["--process", process]
            handles[domain] = (
                subprocess.Popen(
                    command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
                ),
                out_path,
            )
        self.log(f"memory window {name}: sampling {phase} for {self.args.window}")
        return {"name": name, "phase": phase, "handles": handles}

    def finish_window(self, window: dict[str, Any] | None) -> None:
        if window is None:
            return
        name = window["name"]
        result: dict[str, Any] = {
            "status": "measured",
            "reason": None,
            "phase": window["phase"],
            "observations": [],
            "report": None,
            "domains": {},
        }
        reasons = []
        for domain, (process, out_path) in window["handles"].items():
            try:
                stdout, stderr = process.communicate(timeout=self.window_ms / 1000 * 4 + 60)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate()
                reasons.append(f"{domain}: the sampler did not finish")
                continue
            if out_path.is_file():
                result["observations"].append(str(out_path.relative_to(self.evidence_dir)))
            try:
                summary = json.loads(stdout)
            except ValueError:
                reasons.append(
                    f"{domain}: the sampler printed no summary (exit {process.returncode})"
                )
                continue
            if summary.get("phase") != window["phase"]:
                reasons.append(f"{domain}: the sampler reported phase {summary.get('phase')!r}")
                continue
            report = summary.get("report") or {}
            domains = report.get("domains") or []
            result["report"] = combine_reports(result["report"], report)
            if process.returncode != 0 or report.get("complete") is not True:
                reasons.append(f"{domain}: coverage incomplete (exit {process.returncode})")
            matching = [d for d in domains if d.get("domain") == domain]
            if len(matching) != 1:
                reasons.append(f"{domain}: the summary reports {len(matching)} entries for it")
                continue
            entry = matching[0]
            result["domains"][domain] = {
                "consumed": {
                    "max_bytes": entry["consumed"]["max_bytes"],
                    "available_samples": entry["consumed"]["available_samples"],
                },
                "processes": [
                    {
                        "pid": p["pid"],
                        "role": p["role"],
                        "peak": {
                            "max_bytes": p["peak"]["max_bytes"],
                            "available_samples": p["peak"]["available_samples"],
                        },
                    }
                    for p in entry.get("processes", [])
                ],
            }
            for p in entry.get("processes", []):
                if p["peak"]["max_bytes"] is None:
                    reasons.append(f"{domain}: pid {p['pid']} ({p['role']}) was never observed")
        if reasons:
            result["status"] = "incomplete"
            result["reason"] = "; ".join(reasons)
        self.record["memory"]["windows"][name] = result
        self.log(f"memory window {name}: {result['status']}")

    def window_running(self, window: dict[str, Any] | None) -> bool:
        return window is not None and any(
            process.poll() is None for process, _ in window["handles"].values()
        )

    # -- fixtures --

    def request_inputs(self, fixture: dict[str, Any]) -> list[TensorInput]:
        if self.suite_name == "stt":
            pcm = audio.pcm16_bytes(fixture["request_samples"])
            return [
                ServingClient.tensor_input(
                    AUDIO_FRAMES, pcm, "int16", [len(fixture["request_samples"])]
                ),
                ServingClient.tensor_input(
                    TEXT_UTF8,
                    fixture["language"].encode("utf-8"),
                    "uint8",
                    [len(fixture["language"].encode("utf-8"))],
                ),
            ]
        text = fixture["request_text"]
        return [ServingClient.tensor_input(TEXT_UTF8, text, "uint8", [len(text)])]

    def infer(self, inputs: list[TensorInput]) -> tuple[Any, dict[str, Any] | None, int]:
        """(result or None, typed error or None, wall microseconds)."""
        assert self.client is not None
        started = time.monotonic_ns()
        try:
            result = self.client.infer(
                self.args.endpoint, inputs, deadline_ms=self.args.request_timeout_ms, profile=True
            )
        except ServingError as exc:
            return (
                None,
                {"code": exc.code.value, "message": exc.message},
                (time.monotonic_ns() - started) // 1000,
            )
        except RequestTimeoutError:
            return (
                None,
                {"code": "timeout", "message": "the client deadline passed with no response"},
                (time.monotonic_ns() - started) // 1000,
            )
        except (TransportError, ProtocolError) as exc:
            return (
                None,
                {
                    "code": "unavailable",
                    "message": f"transport or protocol failure ({exc.__class__.__name__})",
                },
                (time.monotonic_ns() - started) // 1000,
            )
        return result, None, (time.monotonic_ns() - started) // 1000

    def run_fixture(self, fixture: dict[str, Any], phase: str, iteration: int) -> dict[str, Any]:
        result, error, wall_us = self.infer(self.request_inputs(fixture))
        sample: dict[str, Any] = {
            "iteration": iteration,
            "phase": phase,
            "status": "ok" if error is None else "failed",
            "transport": result.transport if result is not None else None,
            "exchange_wall_us": wall_us,
            "worker_timing_ns": None,
            "runner_timings_us": {},
            "rtf": None,
            "within_request_timeout": wall_us <= self.args.request_timeout_ms * 1000,
            "error": error,
            "output": None,
        }
        if result is None:
            return sample
        if result.timing is not None:
            sample["worker_timing_ns"] = {
                "queue_latency_ns": result.timing.queue_latency_ns,
                "execution_latency_ns": result.timing.execution_latency_ns,
                "total_latency_ns": result.timing.total_latency_ns,
            }
        try:
            metadata = self.read_result_json(result)
            sample["output"] = self.describe_output(fixture, result, metadata, phase, iteration)
            timings = (
                metadata.get("timings_us") if isinstance(metadata.get("timings_us"), dict) else {}
            )
            sample["runner_timings_us"] = {
                k: v
                for k, v in timings.items()
                if isinstance(v, int) and not isinstance(v, bool) and v >= 0
            }
            sample["rtf"] = self.real_time_factor(fixture, sample["runner_timings_us"], metadata)
            self.observe_device_facts(metadata)
        except RecipeError as exc:
            sample["status"] = "failed"
            sample["error"] = {"code": "internal", "message": f"result_json was not usable: {exc}"}
        return sample

    def read_result_json(self, result: Any) -> dict[str, Any]:
        try:
            tensor = result.output(RESULT_JSON)
        except KeyError as exc:
            raise RecipeError("the response carries no result_json tensor") from exc
        if tensor.dtype.value != "uint8" or len(tensor.data) > MAX_RESULT_JSON_BYTES:
            raise RecipeError("result_json is not a uint8 tensor within 1 MiB")
        try:
            metadata = json.loads(tensor.data.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as exc:
            raise RecipeError("result_json is not UTF-8 JSON") from exc
        if not isinstance(metadata, dict):
            raise RecipeError("result_json is not a JSON object")
        return metadata

    def describe_output(
        self,
        fixture: dict[str, Any],
        result: Any,
        metadata: dict[str, Any],
        phase: str,
        iteration: int,
    ) -> dict[str, Any]:
        self.outputs_dir.mkdir(parents=True, exist_ok=True)
        stem = f"{fixture['id']}-{phase}-{iteration}"
        if self.suite_name == "stt":
            segments = (
                metadata.get("segments") if isinstance(metadata.get("segments"), list) else []
            )
            text = metadata.get("text") if isinstance(metadata.get("text"), str) else ""
            transcript_path = self.outputs_dir / f"{stem}.transcript.json"
            transcript_path.write_text(
                json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
            )
            starts = [
                s.get("start_us")
                for s in segments
                if isinstance(s, dict) and isinstance(s.get("start_us"), int)
            ]
            ends = [
                s.get("end_us")
                for s in segments
                if isinstance(s, dict) and isinstance(s.get("end_us"), int)
            ]
            return {
                "language": metadata.get("language"),
                "text_chars": len(text),
                "segment_count": len(segments),
                "word_count": sum(
                    len(s.get("words", []))
                    for s in segments
                    if isinstance(s, dict) and isinstance(s.get("words"), list)
                ),
                "first_segment_start_us": min(starts) if starts else None,
                "last_segment_end_us": max(ends) if ends else None,
                "sample_count": metadata.get("sample_count"),
                "transcript_path": str(transcript_path.relative_to(self.evidence_dir)),
            }
        try:
            pcm = result.output(AUDIO_FRAMES)
        except KeyError as exc:
            raise RecipeError("the response carries no audio_frames tensor") from exc
        if pcm.dtype.value != "int16":
            raise RecipeError("audio_frames is not an int16 tensor")
        rate = metadata.get("sample_rate_hz")
        if not isinstance(rate, int) or rate <= 0:
            raise RecipeError("result_json declares no positive sample_rate_hz")
        samples = audio.pcm16_samples(pcm.data)
        wav = audio.wav_pcm16_mono_bytes(rate, samples)
        pcm_path = self.outputs_dir / f"{stem}.wav"
        pcm_path.write_bytes(wav)
        return {
            "language": metadata.get("language"),
            "voice": metadata.get("voice"),
            "sample_rate_hz": rate,
            "sample_count": len(samples),
            "duration_us": metadata.get("duration_us"),
            "clipped_samples": metadata.get("clipped_samples"),
            "dtype": metadata.get("dtype"),
            "pcm_path": str(pcm_path.relative_to(self.evidence_dir)),
            "pcm_digest": record_mod.sha256_digest(wav),
        }

    def real_time_factor(
        self, fixture: dict[str, Any], timings: dict[str, int], metadata: dict[str, Any]
    ) -> float | None:
        if self.suite_name == "stt":
            decode = timings.get("decode")
            duration = fixture["input"]["duration_us"]
            return None if decode is None or not duration else decode / duration
        synth = timings.get("synthesize")
        duration = metadata.get("duration_us")
        return (
            None
            if synth is None or not isinstance(duration, int) or duration <= 0
            else synth / duration
        )

    def observe_device_facts(self, metadata: dict[str, Any]) -> None:
        runtime = metadata.get("runtime")
        if (
            not isinstance(runtime, dict)
            or self.record["device_facts"]["source"] == "result_json.runtime"
        ):
            return
        facts = self.record["device_facts"]
        facts["source"] = "result_json.runtime"
        facts["device"] = runtime.get("device") if isinstance(runtime.get("device"), str) else None
        facts["compute_type_loaded"] = (
            runtime.get("compute_type") if isinstance(runtime.get("compute_type"), str) else None
        )
        facts["engine_versions"] = {
            k: v
            for k, v in runtime.items()
            if k not in ("device", "compute_type") and isinstance(v, str)
        }

    def fixture_entries(self) -> list[dict[str, Any]]:
        entries = []
        for fixture in self.fixtures:
            entries.append(
                {
                    "id": fixture["id"],
                    "stratum": fixture["stratum"],
                    "language": fixture["language"],
                    "voice": fixture["voice"],
                    "digest": fixture["digest"],
                    "derived_from": fixture["derived_from"],
                    "derivation": fixture["derivation"],
                    "input": fixture["input"],
                    "samples": [],
                    "summary": {
                        "ok_count": 0,
                        "failed_count": 0,
                        "exchange_wall_us": None,
                        "rtf": None,
                    },
                }
            )
        return entries

    def summarize_fixtures(self) -> None:
        for entry in self.record["fixtures"]:
            ok = [s for s in entry["samples"] if s["status"] == "ok"]
            entry["summary"] = {
                "ok_count": len(ok),
                "failed_count": len(entry["samples"]) - len(ok),
                "exchange_wall_us": record_mod.nearest_rank_distribution(
                    [s["exchange_wall_us"] for s in ok]
                ),
                "rtf": record_mod.nearest_rank_distribution(
                    [s["rtf"] for s in ok if s["rtf"] is not None]
                ),
            }

    def run_suite_once(self, phase: str, iteration: int) -> None:
        for fixture, entry in zip(self.fixtures, self.record["fixtures"], strict=True):
            entry["samples"].append(self.run_fixture(fixture, phase, iteration))

    # -- negatives --

    def negative(
        self, case: str, description: str, disposition: str, expected: list[str]
    ) -> dict[str, Any]:
        return {
            "case": case,
            "description": description,
            "disposition": disposition,
            "expected_codes": expected,
            "observed": None,
            "observed_outcome": None,
            "status": "not_run",
            "reason": None,
        }

    def judge(
        self, negative: dict[str, Any], observed: dict[str, Any] | None, outcome: str | None = None
    ) -> None:
        negative["observed"] = observed
        negative["observed_outcome"] = outcome
        if negative["disposition"] == "record_only":
            negative["status"] = "recorded"
        elif observed is not None and observed["code"] in negative["expected_codes"]:
            negative["status"] = "matched"
        else:
            negative["status"] = "mismatched"
            negative["reason"] = (
                "the request succeeded" if observed is None else f"observed {observed['code']}"
            )

    def request_negative(self, negative: dict[str, Any], inputs: list[TensorInput]) -> None:
        result, error, _ = self.infer(inputs)
        self.judge(negative, error, "success" if result is not None else None)

    def deploy_negative(
        self, negative: dict[str, Any], bundle_dir: Path, deployment_id: str
    ) -> None:
        outcome = self.deploy(bundle_dir, deployment_id)
        observed = outcome["failure"]
        if observed is not None:
            self.last_deploy_failure_code = observed["code"]
        if observed is None and outcome["status"] == "ok":
            # A successful promote clears the agent's last_error.
            self.last_deploy_failure_code = None
            self.judge(negative, None, f"deployed as {deployment_id}")
            return
        if observed is None:
            observed = {
                "code": "internal",
                "message": f"deploy ended in phase {outcome['phase']!r} with no typed failure",
            }
        self.judge(negative, observed, outcome["phase"])

    def run_negatives(self) -> None:
        negatives: list[dict[str, Any]] = []
        entry = self.candidate["entry"]
        if self.suite_name == "stt":
            declared = entry.get("languages") if isinstance(entry.get("languages"), list) else []
            undeclared = next(
                (c for c in UNDECLARED_LANGUAGE_CANDIDATES if c not in declared), None
            )
            pcm = audio.pcm16_bytes(self.fixtures[0]["request_samples"])
            language = self.fixtures[0]["language"].encode("utf-8")
            cases = [
                (
                    self.negative(
                        "malformed_audio_dtype",
                        "audio_frames sent as float32 instead of int16",
                        "must_match",
                        ["shape_mismatch"],
                    ),
                    [
                        ServingClient.tensor_input(
                            AUDIO_FRAMES,
                            pcm[: len(pcm) - len(pcm) % 4],
                            "float32",
                            [(len(pcm) // 4)],
                        ),
                        ServingClient.tensor_input(TEXT_UTF8, language, "uint8", [len(language)]),
                    ],
                ),
                (
                    self.negative(
                        "malformed_text_utf8",
                        "text_utf8 carrying a byte that is not UTF-8",
                        "must_match",
                        ["config_invalid"],
                    ),
                    [
                        ServingClient.tensor_input(AUDIO_FRAMES, pcm, "int16", [len(pcm) // 2]),
                        ServingClient.tensor_input(TEXT_UTF8, b"\xff", "uint8", [1]),
                    ],
                ),
            ]
            if undeclared is not None:
                code = undeclared.encode("utf-8")
                cases.append(
                    (
                        self.negative(
                            "unsupported_language",
                            f"a language code the entry does not declare ({undeclared})",
                            "must_match",
                            ["unsupported"],
                        ),
                        [
                            ServingClient.tensor_input(AUDIO_FRAMES, pcm, "int16", [len(pcm) // 2]),
                            ServingClient.tensor_input(TEXT_UTF8, code, "uint8", [len(code)]),
                        ],
                    )
                )
            for negative, inputs in cases:
                self.request_negative(negative, inputs)
                negatives.append(negative)
            if undeclared is None:
                skipped = self.negative(
                    "unsupported_language",
                    "a language code the entry does not declare",
                    "must_match",
                    ["unsupported"],
                )
                skipped["reason"] = "the entry declares every probe code this tool knows"
                negatives.append(skipped)
        else:
            cases = [
                (
                    self.negative(
                        "malformed_text_blank",
                        "text_utf8 holding only whitespace",
                        "must_match",
                        ["config_invalid"],
                    ),
                    [ServingClient.tensor_input(TEXT_UTF8, b"   ", "uint8", [3])],
                ),
                (
                    self.negative(
                        "malformed_text_dtype",
                        "text_utf8 sent as float32 instead of uint8",
                        "must_match",
                        ["shape_mismatch"],
                    ),
                    [ServingClient.tensor_input(TEXT_UTF8, b"\x00\x00\x80\x3f", "float32", [1])],
                ),
            ]
            for negative, inputs in cases:
                self.request_negative(negative, inputs)
                negatives.append(negative)
            voice_negative = self.negative(
                "unsupported_voice",
                f"an entry selecting a voice it does not declare ({UNDECLARED_VOICE})",
                "must_match",
                ["unsupported"],
            )
            try:
                variant = stage_variant(
                    self.candidate,
                    self.staging_dir,
                    f"{self.deployment_id}-unsupported-voice",
                    repin_entry(self.candidate, lambda e: e.__setitem__("voice", UNDECLARED_VOICE)),
                )
                self.deploy_negative(
                    voice_negative, variant, f"{self.deployment_id}-unsupported-voice"
                )
            except RecipeError as exc:
                voice_negative["reason"] = str(exc)
            negatives.append(voice_negative)
        corrupt = self.negative(
            "corrupt_artifact_digest",
            "a bundle whose largest non-entry artifact differs by one byte from its manifest "
            "digest",
            "must_match",
            ["load_failed"],
        )
        try:
            variant = stage_variant(
                self.candidate,
                self.staging_dir,
                f"{self.deployment_id}-corrupt",
                corrupt_one_artifact(self.candidate),
            )
            self.deploy_negative(corrupt, variant, f"{self.deployment_id}-corrupt")
        except RecipeError as exc:
            corrupt["reason"] = str(exc)
        negatives.append(corrupt)
        cancel = self.negative(
            "cancel_during_request",
            "submit one request through the asynchronous policy route and cancel it; the "
            "Python-backed worker refuses that route today, so the outcome is recorded, not judged",
            "record_only",
            [],
        )
        self.cancel_case(cancel)
        negatives.append(cancel)
        oom = self.negative(
            "oom_at_load",
            "deploy the candidate again while a ballast process holds device memory",
            "must_match",
            ["oom_error"],
        )
        if self.args.oom_ballast_bytes:
            self.oom_case(oom)
        else:
            oom["reason"] = "no --oom-ballast-bytes given"
        negatives.append(oom)
        self.record["negatives"] = negatives

    def cancel_case(self, negative: dict[str, Any]) -> None:
        if self.serving_origin is None:
            negative["reason"] = "no serving endpoint"
            return
        inputs = self.request_inputs(self.fixtures[0])
        body = {
            "schema_version": "0.1",
            "request_id": f"{self.deployment_id}-cancel",
            "endpoint": self.args.endpoint,
            "inputs": [
                {
                    "name": t.name,
                    "tensor": {
                        "dtype": t.dtype.value,
                        "layout": t.layout.value,
                        "shape": list(t.shape),
                    },
                    "payload_b64": base64.b64encode(t.data).decode("ascii"),
                }
                for t in inputs
            ],
        }
        timeout = self.args.request_timeout_ms / 1000 + 5
        try:
            status, raw = http_request(
                "POST",
                f"{self.serving_origin}/policy/infer",
                body=json.dumps(body).encode("utf-8"),
                headers={"Content-Type": "application/json"},
                timeout=timeout,
            )
            accepted = json.loads(raw)
            if (
                status != 202
                or not isinstance(accepted, dict)
                or accepted.get("status") != "accepted"
            ):
                error = (
                    accepted.get("error")
                    if isinstance(accepted, dict) and isinstance(accepted.get("error"), dict)
                    else None
                )
                self.judge(
                    negative,
                    {"code": str(error.get("code")), "message": str(error.get("message", ""))}
                    if error and isinstance(error.get("code"), str)
                    else None,
                    f"submit http {status}",
                )
                return
            _, cancel_raw = http_request(
                "POST",
                f"{self.serving_origin}{accepted['cancel_url']}",
                body=b"{}",
                headers={"Content-Type": "application/json"},
                timeout=timeout,
            )
            cancelled = json.loads(cancel_raw).get("cancelled")
            deadline = time.monotonic() + timeout
            final = None
            while time.monotonic() < deadline:
                _, result_raw = http_request(
                    "GET", f"{self.serving_origin}{accepted['result_url']}", timeout=timeout
                )
                final = json.loads(result_raw)
                if final.get("status") not in ("pending", "in_flight"):
                    break
                time.sleep(0.05)
            observed = None
            if (
                isinstance(final, dict)
                and isinstance(final.get("error"), dict)
                and isinstance(final["error"].get("code"), str)
            ):
                observed = {
                    "code": final["error"]["code"],
                    "message": str(final["error"].get("message", "")),
                }
            self.judge(
                negative,
                observed,
                f"cancelled={cancelled} "
                f"result={final.get('status') if isinstance(final, dict) else None}",
            )
        except (TransportError, ProtocolError, ValueError, KeyError) as exc:
            negative["reason"] = (
                f"the asynchronous route did not answer as expected ({exc.__class__.__name__})"
            )

    def oom_case(self, negative: dict[str, Any]) -> None:
        command = [
            self.args.oom_ballast_python,
            str(HERE / "vram_ballast.py"),
            str(self.args.oom_ballast_bytes),
        ]
        ballast = subprocess.Popen(
            command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        try:
            line = ballast.stdout.readline() if ballast.stdout else ""
            if not line.startswith("held "):
                ballast.kill()
                _, stderr = ballast.communicate()
                negative["reason"] = (
                    f"the ballast did not report holding memory (exit {ballast.returncode})"
                )
                return
            self.log(f"oom ballast holds {line.split()[1]} bytes")
            self.deploy_negative(negative, self.candidate["dir"], f"{self.deployment_id}-oom")
        finally:
            if ballast.poll() is None:
                ballast.terminate()
                try:
                    ballast.communicate(timeout=30)
                except subprocess.TimeoutExpired:
                    ballast.kill()
                    ballast.communicate()

    # -- the run --

    def run(self) -> int:
        self.evidence_dir.mkdir(parents=True, exist_ok=True)
        if any(self.evidence_dir.iterdir()):
            raise RecipeError(
                f"{self.evidence_dir} is not empty; a run needs a fresh evidence directory"
            )
        self.staging_dir.mkdir(parents=True, exist_ok=True)
        self.record["fixtures"] = self.fixture_entries()
        self.observe_versions()
        stop_reason = None
        try:
            self.run_steps()
        except RecipeError as exc:
            self.log(f"stopped: {exc}")
            stop_reason = str(exc)
        self.summarize_fixtures()
        self.write_record(stop_reason)
        status = self.record["result"]["status"]
        self.log(f"result: {status}")
        return {"pass": 0, "fail": 2, "incomplete": 3}[status]

    def observe_versions(self) -> None:
        try:
            envelope, _ = self.cli("version")
        except RecipeError as exc:
            self.log(f"version: {exc}")
            return
        payload = envelope.get("payload") if isinstance(envelope.get("payload"), dict) else {}
        facts = self.record["device_facts"]
        facts["cli_version"] = payload.get("cli") if isinstance(payload.get("cli"), str) else None
        facts["protocol_version"] = (
            payload.get("protocol") if isinstance(payload.get("protocol"), str) else None
        )
        facts["bundle_format_version"] = (
            payload.get("bundle_format") if isinstance(payload.get("bundle_format"), str) else None
        )

    def run_steps(self) -> None:
        lifecycle = self.record["lifecycle"]
        windows = self.record["memory"]["windows"]
        if self.predecessor is not None:
            outcome = self.deploy(self.predecessor["dir"], self.predecessor_id)
            lifecycle["predecessor"] = {
                k: outcome[k]
                for k in ("deployment_id", "bundle_digest", "phase", "wall_ms", "status")
            }
            if outcome["status"] != "ok":
                raise RecipeError("the predecessor deployment did not become active")
            self.finish_window(self.start_window("predecessor_idle", "warm-idle"))
        else:
            windows["predecessor_idle"] = record_mod.empty_window(
                "predecessor_idle", "no predecessor bundle given"
            )
            windows["after_teardown"] = record_mod.empty_window(
                "after_teardown", "no predecessor bundle given"
            )
            lifecycle["teardown"]["reason"] = (
                "no predecessor bundle given: rollback has nothing to restore"
            )
        lifecycle["deploy"] = self.deploy(self.candidate["dir"], self.deployment_id)
        if lifecycle["deploy"]["status"] != "ok":
            raise RecipeError("the candidate deployment did not become active")
        lifecycle["status_after_deploy"] = self.status_snapshot(self.deployment_id)
        if self.client is None:
            raise RecipeError("status reported no serving URL; nothing to send requests to")
        self.finish_window(self.start_window("candidate_warm_idle", "warm-idle"))
        for iteration in range(self.args.timing_iterations):
            self.run_suite_once("timing", iteration)
        load_window = self.start_window("candidate_load", "load")
        iteration = 0
        if load_window is None:
            windows["candidate_load"] = record_mod.empty_window(
                "candidate_load", windows["candidate_load"]["reason"] or "no sampler given"
            )
        while self.window_running(load_window):
            self.run_suite_once("load", iteration)
            iteration += 1
        self.finish_window(load_window)
        self.run_negatives()
        lifecycle["status_after_negatives"] = self.status_snapshot(
            self.deployment_id, self.last_deploy_failure_code
        )
        if self.predecessor is not None:
            lifecycle["teardown"] = self.rollback()
            if lifecycle["teardown"]["status"] == "ok":
                after = self.status_snapshot(self.predecessor_id)
                if not all(after["checks"].values()):
                    lifecycle["teardown"]["status"] = "failed"
                    lifecycle["teardown"]["reason"] = (
                        "status after rollback does not show the predecessor active and ready"
                    )
                self.finish_window(self.start_window("after_teardown", "warm-idle"))


def combine_reports(existing: dict[str, Any] | None, report: dict[str, Any]) -> dict[str, Any]:
    """One report per window: tick counts summed over its sampler invocations."""
    counts = {
        key: int(report.get(key) or 0)
        for key in ("expected_ticks", "completed_ticks", "late_ticks")
    }
    complete = report.get("complete") is True
    if existing is None:
        return {**counts, "complete": complete}
    return {
        **{key: existing[key] + counts[key] for key in counts},
        "complete": existing["complete"] and complete,
    }


def children_of(pid: int) -> list[int]:
    children = []
    proc = Path("/proc")
    if not proc.is_dir():
        return children
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text()
        except OSError:
            continue
        fields = stat[stat.rfind(")") + 2 :].split()
        if len(fields) > 1 and fields[1] == str(pid):
            children.append(int(entry.name))
    return sorted(children)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="candidate-qualify.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--candidate-bundle",
        required=True,
        help="the candidate bundle directory, readable by the agent",
    )
    parser.add_argument(
        "--predecessor-bundle", help="a bundle to deploy first, so the teardown can roll back to it"
    )
    parser.add_argument("--inventory", default=str(DEFAULT_INVENTORY))
    parser.add_argument(
        "--clips", help="directory holding <clip id>.wav for the inventory's provisioned STT clips"
    )
    parser.add_argument(
        "--evidence-dir",
        required=True,
        help="a fresh directory for the record, outputs and sampler files",
    )
    parser.add_argument(
        "--staging-dir",
        help="where variant bundles for negative cases are written; "
        "defaults to <evidence-dir>/staging",
    )
    parser.add_argument(
        "--deployment-id",
        default="candidate-qualify-"
        + _dt.datetime.now(tz=_dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ"),
    )
    parser.add_argument("--predecessor-deployment-id")
    parser.add_argument("--tensorplate", default="tensorplate", help="the tensorplate CLI to run")
    parser.add_argument(
        "--endpoint",
        default="candidate-qualify",
        help="the informational endpoint name on each request",
    )
    parser.add_argument("--transport", choices=("auto", "binary", "json"), default="binary")
    parser.add_argument(
        "--request-timeout-ms",
        type=int,
        default=8000,
        help="the serving worker's whole-exchange request timeout",
    )
    parser.add_argument("--deploy-wait-timeout-ms", type=int, default=300000)
    parser.add_argument("--timing-iterations", type=int, default=3)
    parser.add_argument(
        "--sampler", help="the memory_sample binary; without it every memory window is not_run"
    )
    parser.add_argument("--sample-interval", default="1s")
    parser.add_argument("--window", default="60s", help="length of each warm-idle and load window")
    parser.add_argument(
        "--pid",
        action="append",
        default=[],
        help="<pid>:<role> to sample, as the sampler takes it, instead of discovering PIDs "
        "from the agent unit",
    )
    parser.add_argument("--agent-unit", default="tensorplate-agent")
    parser.add_argument(
        "--oom-ballast-python",
        default=sys.executable,
        help="interpreter that can import the runtime's torch, for the ballast",
    )
    parser.add_argument(
        "--oom-ballast-bytes",
        type=int,
        default=0,
        help="device bytes the ballast holds; 0 skips the OOM case",
    )
    parser.add_argument("--source-commit")
    parser.add_argument("--provenance", choices=("recorded", "synthetic"), default="recorded")
    args = parser.parse_args(argv)
    if (
        args.request_timeout_ms <= 0
        or args.timing_iterations <= 0
        or args.deploy_wait_timeout_ms <= 0
        or args.oom_ballast_bytes < 0
    ):
        parser.error("timeouts and iteration counts must be positive")
    return args


def main(argv: list[str]) -> int:
    try:
        args = parse_args(argv)
        recipe = Recipe(args)
        return recipe.run()
    except RecipeError as exc:
        sys.stderr.write(f"candidate-qualify: {exc}\n")
        return 1
    except record_mod.RecordError as exc:
        sys.stderr.write(f"candidate-qualify: the record failed its own schema: {exc}\n")
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
