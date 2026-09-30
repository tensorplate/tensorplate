#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Drive tools/validation/candidate-qualify.py end to end against a fake appliance.

The fake `tensorplate` CLI, serving worker, memory sampler and ballast answer
in the shapes the real ones use, so the tool's reading of every seam is
exercised without a GPU; every guard is then broken once to show the record
turns `fail` or `incomplete` for its own reason. Set
TP_CANDIDATE_QUALIFY_UPDATE_GOLDEN=1 to rewrite the committed synthetic record.
"""

from __future__ import annotations

import base64
import http.server
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time
import warnings
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools/validation/candidate-qualify.py"
INVENTORY = ROOT / "test/models/speech/candidate_fixture_inventory.json"
GOLDEN = ROOT / "test/validation/fixtures/candidate_qualification_record_synthetic.json"
FAKES = ROOT / "test/validation/fixtures/candidate_qualify_fakes"
BUNDLES = ROOT / "test/models/bundles/v0_1"
sys.path.insert(0, str(ROOT / "tools/validation"))
sys.path.insert(0, str(ROOT / "sdk/python/src"))
import candidate_audio as audio  # noqa: E402
import candidate_record as record_mod  # noqa: E402
import jsonschema  # noqa: E402

VOLATILE_KEYS = {
    "recorded_at_utc",
    "source_commit",
    "wall_ms",
    "exchange_wall_us",
    "worker_timing_ns",
}


class FakeWorker(http.server.BaseHTTPRequestHandler):
    """The serving worker's /infer, /health and asynchronous policy routes for both profiles."""

    state_file: Path
    mutate = ""
    # Paces requests so the one-second load window holds a handful of samples
    # per fixture and the committed golden record stays readable.
    delay_s = 0.1

    def log_message(self, *_args) -> None:  # noqa: D401
        return

    def active_entry(self):
        state = json.loads(self.state_file.read_text())
        return state["active"]

    def send_json(self, code: int, body: dict) -> None:
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def failure(self, request_id: str, code: str, message: str, http_code: int = 400) -> None:
        self.send_json(
            http_code,
            {
                "schema_version": "0.1",
                "request_id": request_id,
                "status": "failure",
                "error": {"schema_version": "0.1", "code": code, "message": message},
            },
        )

    def do_GET(self) -> None:  # noqa: N802
        active = self.active_entry()
        if self.path == "/health":
            self.send_json(
                200,
                {
                    "schema_version": "0.1",
                    "state": "ready",
                    "endpoint": "fake",
                    "backend": "python_pytorch",
                    "active_model_id": active["deployment_id"] if active else None,
                },
            )
        elif self.path.startswith("/policy/result/"):
            self.send_json(
                200,
                {
                    "schema_version": "0.1",
                    "request_id": self.path.rsplit("/", 1)[1],
                    "status": "completed",
                },
            )
        else:
            self.send_json(404, {"error": "no route"})

    def do_POST(self) -> None:  # noqa: N802
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path == "/policy/infer":
            body = json.loads(raw)
            if self.mutate != "async_accept":
                # The router refuses the asynchronous route for a backend without
                # that capability, as the Python-backed worker is today.
                self.failure(
                    body["request_id"], "unsupported", "async policy route unsupported", 501
                )
                return
            self.send_json(
                202,
                {
                    "schema_version": "0.1",
                    "status": "accepted",
                    "request_id": body["request_id"],
                    "result_url": f"/policy/result/{body['request_id']}",
                    "cancel_url": f"/policy/cancel/{body['request_id']}",
                },
            )
            return
        if self.path.startswith("/policy/cancel/"):
            self.send_json(
                200,
                {
                    "schema_version": "0.1",
                    "request_id": self.path.rsplit("/", 1)[1],
                    "cancelled": False,
                },
            )
            return
        if self.path != "/infer":
            self.send_json(404, {"error": "no route"})
            return
        binary = self.headers.get("Content-Type", "").startswith(
            "application/vnd.tensorplate.infer.binary.v1"
        )
        if binary:
            (length,) = struct.unpack("<I", raw[8:12])
            metadata = json.loads(raw[12 : 12 + length])
            payload = raw[12 + length :]
            inputs = [
                (
                    i["name"],
                    i["tensor"],
                    payload[i["payload_offset"] : i["payload_offset"] + i["payload_size"]],
                )
                for i in metadata["inputs"]
            ]
        else:
            metadata = json.loads(raw)
            inputs = [
                (i["name"], i["tensor"], base64.b64decode(i["payload_b64"]))
                for i in metadata["inputs"]
            ]
        request_id = metadata["request_id"]
        entry = self.active_entry()["entry"]
        profile = entry["backend_profile"]
        time.sleep(self.delay_s)
        try:
            outputs, timings = self.run_profile(profile, entry, inputs)
        except LookupError as exc:
            code, message = exc.args
            self.failure(request_id, code, message)
            return
        payload = b"".join(data for _, _, data in outputs)
        offset = 0
        rendered = []
        for name, tensor, data in outputs:
            item = {"name": name, "tensor": dict(tensor, byte_offset=0, byte_size=len(data))}
            if binary:
                item.update(payload_offset=offset, payload_size=len(data))
            else:
                item["payload_b64"] = base64.b64encode(data).decode()
            rendered.append(item)
            offset += len(data)
        body = {
            "schema_version": "0.1",
            "request_id": request_id,
            "status": "success",
            "outputs": rendered,
            "timing": {
                "queue_latency_ns": 1000,
                "execution_latency_ns": timings * 1000,
                "total_latency_ns": timings * 1000 + 1000,
            },
        }
        if binary:
            meta = json.dumps(body).encode()
            data = b"TPRESULT1" + struct.pack("<I", len(meta)) + meta + payload
            self.send_response(200)
            self.send_header("Content-Type", "application/vnd.tensorplate.infer.binary.v1")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        else:
            self.send_json(200, body)

    def run_profile(self, profile: str, entry: dict, inputs: list):
        by_name = {name: (tensor, data) for name, tensor, data in inputs}
        if profile == "faster_whisper":
            if set(by_name) != {"audio_frames", "text_utf8"}:
                raise LookupError(
                    "shape_mismatch",
                    "a request carries exactly one audio_frames and one text_utf8 tensor",
                )
            tensor, pcm = by_name["audio_frames"]
            if tensor["dtype"] != "int16" or len(tensor["shape"]) != 1:
                raise LookupError(
                    "shape_mismatch",
                    "audio_frames must be a one-dimensional int16 tensor of its payload",
                )
            try:
                language = by_name["text_utf8"][1].decode("utf-8")
            except UnicodeDecodeError:
                raise LookupError("config_invalid", "`text_utf8` is not valid UTF-8") from None
            if language not in entry["languages"]:
                raise LookupError(
                    "unsupported" if self.mutate != "wrong_code" else "internal",
                    "the request's language is not declared",
                )
            samples = len(pcm) // 2
            duration_us = -(-samples * 1_000_000 // entry["sample_rate_hz"])
            decode_us = duration_us // 10
            result = {
                "language": language,
                "text": "synthetic transcript",
                "segments": [
                    {
                        "start_us": 0,
                        "end_us": duration_us,
                        "text": "synthetic transcript",
                        "tokens": [1, 2],
                        "words": [
                            {
                                "start_us": 0,
                                "end_us": duration_us // 2,
                                "text": "synthetic",
                                "probability": 0.5,
                            },
                            {
                                "start_us": duration_us // 2,
                                "end_us": duration_us,
                                "text": "transcript",
                                "probability": 0.5,
                            },
                        ],
                    }
                ],
                "sample_rate_hz": entry["sample_rate_hz"],
                "sample_count": samples,
                "decode_options": {
                    "beam_size": 1,
                    "temperature": 0.0,
                    "condition_on_previous_text": False,
                    "vad_filter": False,
                    "word_timestamps": True,
                },
                "runtime": {
                    "device": "cpu",
                    "compute_type": "float16",
                    "faster_whisper": "0.0-fake",
                    "ctranslate2": "0.0-fake",
                },
                "timings_us": {
                    "load": 5000,
                    "artifact_verify": 1000,
                    "model_build": 4000,
                    "decode": decode_us,
                },
            }
            data = json.dumps(result).encode()
            return [
                (
                    "result_json",
                    {"dtype": "uint8", "layout": "row_major", "shape": [len(data)]},
                    data,
                )
            ], decode_us
        if profile == "kokoro":
            if set(by_name) != {"text_utf8"}:
                raise LookupError(
                    "shape_mismatch", "a request carries exactly one text_utf8 tensor"
                )
            tensor, text = by_name["text_utf8"]
            if tensor["dtype"] != "uint8":
                raise LookupError("shape_mismatch", "text_utf8 must be a uint8 tensor")
            if not text.decode("utf-8", "replace").strip():
                raise LookupError("config_invalid", "text_utf8 must not be blank")
            rate = entry["sample_rate_hz"]
            count = 240 * len(text)
            samples = [(((n * 37) % 200) - 100) * 50 for n in range(count)]
            pcm = struct.pack(f"<{count}h", *samples)
            duration_us = -(-count * 1_000_000 // rate)
            synth_us = duration_us // 20
            result = {
                "language": entry["language"],
                "voice": entry["voice"],
                "model_digest": "sha256:" + "0" * 64,
                "config_digest": "sha256:" + "0" * 64,
                "voice_digest": "sha256:" + "0" * 64,
                "sample_rate_hz": rate,
                "sample_count": count,
                "duration_us": duration_us,
                "dtype": "float32",
                "clipped_samples": 0,
                "runtime": {
                    "device": "cpu",
                    "compute_type": "float32",
                    "kokoro": "0.0-fake",
                    "torch": "0.0-fake",
                },
                "timings_us": {
                    "load": 5000,
                    "artifact_verify": 1000,
                    "model_build": 4000,
                    "phonemize": 100,
                    "synthesize": synth_us,
                    "pcm": 10,
                },
            }
            data = json.dumps(result).encode()
            return [
                ("audio_frames", {"dtype": "int16", "layout": "row_major", "shape": [count]}, pcm),
                (
                    "result_json",
                    {"dtype": "uint8", "layout": "row_major", "shape": [len(data)]},
                    data,
                ),
            ], synth_us
        raise LookupError("unsupported", "no fake for this profile")


def write_executable(path: Path, source: Path) -> Path:
    path.write_text(source.read_text())
    path.chmod(0o700)
    return path


def square_wave(rate: int, seconds: float, period: int, amplitude: int) -> list[int]:
    count = int(rate * seconds)
    return [amplitude if (n // period) % 2 == 0 else -amplitude for n in range(count)]


def normalize(value):
    """Strip what legitimately differs between two runs: clocks, and how many
    load-phase requests fit in the window (with the summaries derived from them)."""
    if isinstance(value, dict):
        out = {}
        for k, v in value.items():
            if k in VOLATILE_KEYS or k == "summary":
                out[k] = None
            elif k == "samples" and isinstance(v, list):
                out[k] = [
                    normalize(s)
                    for s in v
                    if not (isinstance(s, dict) and s.get("phase") == "load")
                ]
            else:
                out[k] = normalize(v)
        return out
    if isinstance(value, list):
        return [normalize(v) for v in value]
    return value


def diff_pointers(left, right, pointer="", out=None):
    out = [] if out is None else out
    if isinstance(left, dict) and isinstance(right, dict):
        for key in sorted(set(left) | set(right)):
            diff_pointers(left.get(key), right.get(key), f"{pointer}/{key}", out)
    elif isinstance(left, list) and isinstance(right, list) and len(left) == len(right):
        for index, (a, b) in enumerate(zip(left, right, strict=True)):
            diff_pointers(a, b, f"{pointer}/{index}", out)
    elif left != right:
        out.append(f"{pointer}: {left!r} != {right!r}")
    return out


class Harness:
    def __init__(self, work: Path) -> None:
        self.work = work
        self.state_file = work / "state.json"
        self.oom_marker = work / "oom.marker"
        self.bin = work / "bin"
        self.bin.mkdir()
        self.cli = write_executable(self.bin / "tensorplate", FAKES / "tensorplate.py")
        self.sampler = write_executable(self.bin / "memory_sample", FAKES / "memory_sample.py")
        self.ballast = write_executable(self.bin / "fake-python", FAKES / "ballast.py")
        FakeWorker.state_file = self.state_file
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeWorker)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.serving_url = f"http://127.0.0.1:{self.server.server_address[1]}/infer"
        self.bundles = work / "bundles"
        for name in ("stt_whisper_candidate", "tts_kokoro_candidate", "x86_fixture_smoke"):
            shutil.copytree(BUNDLES / name, self.bundles / name)
        self.clips = work / "clips"
        self.clips.mkdir()
        self.inventory = work / "inventory.json"
        inventory = json.loads(INVENTORY.read_text())
        samples_by_id = {}
        for clip in inventory["suites"]["stt"]["clips"]:
            if clip["derived_from"] is None:
                samples = square_wave(16000, 1.5, 40 if clip["language"] == "en" else 60, 6000)
                wav = audio.wav_pcm16_mono_bytes(16000, samples)
                (self.clips / f"{clip['id']}.wav").write_bytes(wav)
            else:
                source = samples_by_id[clip["derived_from"]]
                derivation = clip["derivation"]
                if derivation["kind"] == "seeded_noise_mix":
                    samples = audio.seeded_noise_mix(
                        source, seed=derivation["seed"], snr_db=derivation["snr_db"]
                    )
                else:
                    samples = audio.telephony_8k_ulaw(source)
                wav = audio.wav_pcm16_mono_bytes(clip["sample_rate_hz"], samples)
            samples_by_id[clip["id"]] = samples
            clip["digest"] = record_mod.sha256_digest(wav)
        self.inventory.write_text(json.dumps(inventory, indent=2))
        self.unpinned_derived = self.work / "inventory-unpinned-derived.json"
        unpinned = json.loads(json.dumps(inventory))
        unpinned["suites"]["stt"]["clips"][-1]["digest"] = None
        self.unpinned_derived.write_text(json.dumps(unpinned))
        self.runs = 0

    def env(self, **extra: str) -> dict[str, str]:
        env = dict(
            os.environ,
            TP_FAKE_STATE_FILE=str(self.state_file),
            TP_FAKE_SERVING_URL=self.serving_url,
            TP_FAKE_OOM_MARKER=str(self.oom_marker),
        )
        env.update(extra)
        return env

    def reset(self) -> None:
        if self.state_file.exists():
            self.state_file.unlink()
        if self.oom_marker.exists():
            self.oom_marker.unlink()
        FakeWorker.mutate = ""

    def run(
        self,
        suite: str,
        *extra: str,
        predecessor: bool = True,
        sampler: bool = True,
        oom: bool = True,
        env: dict | None = None,
        mutate: str = "",
    ) -> tuple[subprocess.CompletedProcess, dict | None, Path]:
        self.reset()
        FakeWorker.mutate = mutate
        self.runs += 1
        evidence = self.work / f"evidence-{self.runs}"
        bundle = self.bundles / (
            "stt_whisper_candidate" if suite == "stt" else "tts_kokoro_candidate"
        )
        command = [
            sys.executable,
            str(TOOL),
            "--candidate-bundle",
            str(bundle),
            "--evidence-dir",
            str(evidence),
            "--inventory",
            str(self.inventory),
            "--clips",
            str(self.clips),
            "--tensorplate",
            str(self.cli),
            "--deployment-id",
            f"qualify-{suite}",
            "--provenance",
            "synthetic",
            "--window",
            "1s",
            "--sample-interval",
            "500ms",
            "--timing-iterations",
            "2",
            "--pid",
            "101:agent",
            "--pid",
            "102:serving_worker",
            "--pid",
            "103:python_sidecar",
            "--source-commit",
            "0" * 40,
        ]
        if predecessor:
            command += ["--predecessor-bundle", str(self.bundles / "x86_fixture_smoke")]
        if sampler:
            command += ["--sampler", str(self.sampler)]
        if oom:
            command += ["--oom-ballast-python", str(self.ballast), "--oom-ballast-bytes", "1024"]
        command += list(extra)
        completed = subprocess.run(
            command, capture_output=True, text=True, timeout=300, env=env or self.env(), check=False
        )
        record_path = evidence / "record.json"
        record = json.loads(record_path.read_text()) if record_path.exists() else None
        return completed, record, evidence


def check_schema(record: dict, schema: dict) -> None:
    jsonschema.validate(record, schema)
    record_mod.validate_record(record, schema)
    record_mod.validate_error_codes(record, record_mod.load_error_codes())


def main() -> int:
    schema = record_mod.load_schema()
    assert audio.fir_taps_from_formula() == audio.FIR_TAPS_Q15, (
        "the pinned FIR taps drifted from their formula"
    )
    assert sum(audio.FIR_TAPS_Q15) == 32768
    for sample in (-32768, -1000, -1, 0, 1, 1000, 32767):
        decoded = audio.ulaw_decode(audio.ulaw_encode(sample))
        assert abs(decoded - sample) <= max(8, abs(sample) // 16), (sample, decoded)
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", DeprecationWarning)
            import audioop  # removed in Python 3.13; the sweep runs where it still exists
    except ImportError:
        audioop = None
    if audioop is not None:
        for value in range(-32768, 32768):
            reference = audioop.lin2ulaw(struct.pack("<h", value), 2)[0]
            assert audio.ulaw_encode(value) == reference, value
            assert (
                audio.ulaw_decode(reference)
                == struct.unpack("<h", audioop.ulaw2lin(bytes([reference]), 2))[0]
            ), reference
    tone = square_wave(16000, 0.5, 40, 6000)
    assert audio.seeded_noise_mix(tone, seed=7, snr_db=10) == audio.seeded_noise_mix(
        tone, seed=7, snr_db=10
    )
    assert audio.seeded_noise_mix(tone, seed=7, snr_db=10) != audio.seeded_noise_mix(
        tone, seed=8, snr_db=10
    )
    assert len(audio.interpolate_2x(audio.decimate_2x(tone))) == len(tone)

    with tempfile.TemporaryDirectory(prefix="tp-candidate-qualify-") as tmp:
        harness = Harness(Path(tmp))

        completed, record, evidence = harness.run("stt")
        assert completed.returncode == 0, completed.stderr
        assert record["result"] == {"status": "pass", "reasons": []}, record["result"]
        check_schema(record, schema)
        assert (
            record["provenance"] == "synthetic"
            and record["qualification"]["production_evidence"] is False
        )
        assert record["convention"] == "candidate_only_text_utf8_result_json"
        assert [f["id"] for f in record["fixtures"]] == [
            "en-clean-16k-01",
            "ar-clean-16k-01",
            "en-noisy-16k-01",
            "ar-noisy-16k-01",
            "en-telephony-8k-01",
        ]
        telephony = record["fixtures"][4]
        assert (
            telephony["input"]["resampled_from_hz"] == 8000
            and telephony["input"]["resampler"] == audio.RESAMPLER_ID
        )
        assert (
            telephony["input"]["sample_rate_hz"] == 16000 and telephony["input"]["samples"] == 24000
        )
        for fixture in record["fixtures"]:
            assert fixture["summary"]["failed_count"] == 0 and fixture["summary"]["ok_count"] >= 2
            assert all(s["transport"] == "binary" for s in fixture["samples"])
            assert all(
                s["rtf"] is not None and s["output"]["transcript_path"] for s in fixture["samples"]
            )
            assert {s["phase"] for s in fixture["samples"]} == {"timing", "load"}, (
                "the load window ran no workload"
            )
            assert "text" not in fixture["samples"][0]["output"], (
                "transcript text belongs in outputs/, not the record"
            )
        assert {n["case"]: n["status"] for n in record["negatives"]} == {
            "malformed_audio_dtype": "matched",
            "malformed_text_utf8": "matched",
            "unsupported_language": "matched",
            "corrupt_artifact_digest": "matched",
            "cancel_during_request": "recorded",
            "oom_at_load": "matched",
        }
        cancel = record["negatives"][4]
        assert cancel["observed_outcome"] == "submit http 501"
        assert cancel["observed"]["code"] == "unsupported" and cancel["expected_codes"] == []
        after = record["lifecycle"]["status_after_negatives"]
        assert after["agent_state"] == "degraded" and after["last_error"]["code"] == "oom_error"
        assert after["checks"]["agent_state_as_expected"] is True
        assert record["lifecycle"]["status_after_deploy"]["last_error"] is None
        assert all(w["status"] == "measured" for w in record["memory"]["windows"].values()), record[
            "memory"
        ]
        load = record["memory"]["windows"]["candidate_load"]
        assert load["domains"]["device_vram"]["consumed"]["max_bytes"] == 4_000_000_001
        assert load["report"] == {
            "expected_ticks": 4,
            "completed_ticks": 4,
            "late_ticks": 0,
            "complete": True,
        }
        assert record["memory"]["windows"]["candidate_warm_idle"]["domains"]["device_vram"][
            "processes"
        ] == [
            {
                "pid": 103,
                "role": "python_sidecar",
                "peak": {"max_bytes": 1_000_000_103, "available_samples": 2},
            }
        ]
        assert [
            p["pid"]
            for p in record["memory"]["windows"]["candidate_warm_idle"]["domains"]["guest_ram"][
                "processes"
            ]
        ] == [101, 102, 103]
        assert record["lifecycle"]["teardown"] == {
            "method": "rollback",
            "status": "ok",
            "restored_deployment_id": "qualify-stt-predecessor",
            "wall_ms": record["lifecycle"]["teardown"]["wall_ms"],
            "reason": None,
        }
        assert (
            record["device_facts"]["source"] == "result_json.runtime"
            and record["device_facts"]["compute_type_loaded"] == "float16"
        )
        assert record["device_facts"]["cli_version"] == "0.3.1"
        assert not harness.oom_marker.exists(), "the ballast was not terminated"
        for name in (
            "predecessor_idle-device_vram.jsonl",
            "candidate_load-guest_ram.jsonl",
            "after_teardown-device_vram.jsonl",
        ):
            assert (evidence / "memory" / name).is_file()
        stt_record = record

        derived_digests = {
            f["id"]: f["digest"] for f in stt_record["fixtures"] if f["derived_from"]
        }
        completed, record, _ = harness.run("stt", oom=False, mutate="async_accept")
        assert completed.returncode == 3 and record["result"]["status"] == "incomplete", (
            completed.stderr
        )
        assert {
            f["id"]: f["digest"] for f in record["fixtures"] if f["derived_from"]
        } == derived_digests, "derivations are not deterministic"
        accepted = [n for n in record["negatives"] if n["case"] == "cancel_during_request"][0]
        assert accepted["status"] == "recorded" and accepted["observed"] is None
        assert accepted["observed_outcome"] == "cancelled=False result=completed"
        after = record["lifecycle"]["status_after_negatives"]
        assert after["agent_state"] == "degraded" and after["last_error"]["code"] == "load_failed"
        assert [n for n in record["negatives"] if n["case"] == "oom_at_load"][0] == {
            "case": "oom_at_load",
            "description": record["negatives"][-1]["description"],
            "disposition": "must_match",
            "expected_codes": ["oom_error"],
            "observed": None,
            "observed_outcome": None,
            "status": "not_run",
            "reason": "no --oom-ballast-bytes given",
        }
        assert record["result"]["reasons"] == ["negative case oom_at_load did not run"]

        completed, record, evidence = harness.run("tts")
        assert completed.returncode == 0, completed.stderr
        assert record["result"] == {"status": "pass", "reasons": []}, record["result"]
        check_schema(record, schema)
        assert len(record["fixtures"]) == 14 and record["subject"]["entry"]["voice"] == "af_heart"
        assert "artifact_set" not in record["subject"]["entry"]
        for fixture in record["fixtures"]:
            for sample in fixture["samples"]:
                wav = evidence / sample["output"]["pcm_path"]
                assert record_mod.sha256_digest(wav.read_bytes()) == sample["output"]["pcm_digest"]
                assert sample["output"]["sample_count"] == 240 * fixture["input"]["text_bytes"]
        assert {n["case"]: n["status"] for n in record["negatives"]} == {
            "malformed_text_blank": "matched",
            "malformed_text_dtype": "matched",
            "unsupported_voice": "matched",
            "corrupt_artifact_digest": "matched",
            "cancel_during_request": "recorded",
            "oom_at_load": "matched",
        }
        assert (
            record["lifecycle"]["status_after_negatives"]["active_deployment_id"] == "qualify-tts"
        )

        golden = normalize(stt_record)
        if os.environ.get("TP_CANDIDATE_QUALIFY_UPDATE_GOLDEN") == "1":
            GOLDEN.write_text(record_mod.dump(stt_record))
        committed = json.loads(GOLDEN.read_text())
        check_schema(committed, schema)
        assert normalize(committed) == golden, (
            "the committed synthetic record differs from a fresh run; regenerate it deliberately:\n"
            + "\n".join(diff_pointers(normalize(committed), golden)[:20])
        )

        # -- guards, each broken once --
        completed, record, _ = harness.run("stt", mutate="wrong_code")
        assert completed.returncode == 2 and record["result"]["status"] == "fail", (
            completed.returncode,
            record["result"],
            completed.stderr[-2000:],
        )
        bad = [n for n in record["negatives"] if n["case"] == "unsupported_language"][0]
        assert (
            bad["status"] == "mismatched"
            and bad["observed"]["code"] == "internal"
            and bad["reason"] == "observed internal"
        )

        completed, record, _ = harness.run("stt", env=harness.env(TP_FAKE_CLI_STALE_STATUS="1"))
        assert completed.returncode == 2 and record["result"]["status"] == "fail"
        snapshot = record["lifecycle"]["status_after_deploy"]
        assert snapshot["active_deployment_id"] == "qualify-stt-predecessor"
        assert snapshot["checks"]["active_deployment_matches"] is False
        assert snapshot["checks"]["health_active_model_matches"] is True
        assert (
            "status_after_deploy checks failed: active_deployment_matches"
            in (record["result"]["reasons"])
        )

        completed, record, _ = harness.run(
            "stt", env=harness.env(TP_FAKE_CLI_FAIL_CANDIDATE="stt-whisper-candidate")
        )
        assert completed.returncode == 2 and record["result"]["status"] == "fail"
        assert record["lifecycle"]["deploy"]["status"] == "failed"
        assert record["lifecycle"]["deploy"]["failure"]["code"] == "load_failed"
        assert record["fixtures"][0]["samples"] == []
        assert record["result"]["reasons"][:2] == [
            "stopped: the candidate deployment did not become active",
            "lifecycle step deploy failed",
        ]

        completed, record, _ = harness.run("stt", env=harness.env(TP_FAKE_CLI_WRONG_RESTORE="1"))
        assert completed.returncode == 2 and record["result"]["status"] == "fail"
        teardown = record["lifecycle"]["teardown"]
        assert (
            teardown["status"] == "failed" and teardown["restored_deployment_id"] == "someone-else"
        )
        assert teardown["reason"] == "restored 'someone-else', expected 'qualify-stt-predecessor'"
        assert record["memory"]["windows"]["after_teardown"]["status"] == "not_run"

        completed, record, _ = harness.run("stt", "--inventory", str(harness.unpinned_derived))
        assert completed.returncode == 1 and record is None
        assert "en-telephony-8k-01 is unpinned" in completed.stderr
        assert derived_digests["en-telephony-8k-01"] in completed.stderr

        completed, record, _ = harness.run("stt", env=harness.env(TP_FAKE_CLI_SKIP_VERIFY="1"))
        assert record["result"]["status"] == "fail"
        corrupt = [n for n in record["negatives"] if n["case"] == "corrupt_artifact_digest"][0]
        assert (
            corrupt["status"] == "mismatched"
            and corrupt["reason"] == "the request succeeded"
            and corrupt["observed_outcome"] == "deployed as qualify-stt-corrupt"
        )

        completed, record, _ = harness.run(
            "stt", env=harness.env(TP_FAKE_SAMPLER_MODE="incomplete")
        )
        assert completed.returncode == 3 and record["result"]["status"] == "incomplete"
        assert all(
            w["status"] == "incomplete" and "coverage incomplete" in w["reason"]
            for w in record["memory"]["windows"].values()
        )
        assert all(w["report"]["complete"] is False for w in record["memory"]["windows"].values())

        completed, record, _ = harness.run("stt", predecessor=False)
        assert completed.returncode == 3 and record["result"]["status"] == "incomplete"
        assert (
            record["lifecycle"]["predecessor"] is None
            and record["lifecycle"]["teardown"]["method"] == "none"
        )
        assert record["memory"]["windows"]["after_teardown"]["status"] == "not_run"
        assert (
            "no predecessor deployment: teardown by rollback not exercised"
            in record["result"]["reasons"]
        )

        completed, record, _ = harness.run("stt", sampler=False)
        assert completed.returncode == 3 and record["memory"]["sampler"] is None
        assert all(
            w["status"] == "not_run" and w["reason"] == "not run in this environment"
            for w in record["memory"]["windows"].values()
        )

        completed, record, _ = harness.run("stt", "--request-timeout-ms", "1")
        assert completed.returncode == 2 and record["result"]["status"] == "fail"
        assert any("exceeded the request timeout" in r for r in record["result"]["reasons"])

        completed, record, _ = harness.run("stt", "--inventory", str(INVENTORY))
        assert completed.returncode == 1 and record is None and "unpinned" in completed.stderr

        wrong = json.loads(harness.inventory.read_text())
        wrong["suites"]["stt"]["clips"][0]["digest"] = "sha256:" + "f" * 64
        wrong_path = harness.work / "inventory-wrong.json"
        wrong_path.write_text(json.dumps(wrong))
        completed, record, _ = harness.run("stt", "--inventory", str(wrong_path))
        assert (
            completed.returncode == 1 and "does not match the inventory's pin" in completed.stderr
        )

        completed, record, _ = harness.run("stt", "--transport", "json")
        assert completed.returncode == 0 and all(
            s["transport"] == "json" for f in record["fixtures"] for s in f["samples"]
        )

        # The record's own validator and the reference library agree on malformed records.
        for mutate in (
            lambda r: r.__setitem__("extra", 1),
            lambda r: r["result"].__setitem__("status", "maybe"),
            lambda r: r["qualification"].__setitem__("production_evidence", True),
            lambda r: r["fixtures"][0]["samples"][0].__setitem__("transport", "grpc"),
            lambda r: r["memory"]["windows"]["candidate_load"].__setitem__("status", "peak"),
            lambda r: r["subject"]["artifacts"][0].__setitem__("digest", "sha256:short"),
        ):
            broken = json.loads(json.dumps(stt_record))
            mutate(broken)
            for validate in (
                lambda v: jsonschema.validate(v, schema),
                lambda v: record_mod.validate_record(v, schema),
            ):
                try:
                    validate(broken)
                except (jsonschema.ValidationError, record_mod.RecordError):
                    continue
                raise AssertionError("a malformed record passed validation")
        broken = json.loads(json.dumps(stt_record))
        broken["negatives"][0]["observed"] = {"code": "not_a_code", "message": ""}
        try:
            record_mod.validate_error_codes(broken, record_mod.load_error_codes())
        except record_mod.RecordError:
            pass
        else:
            raise AssertionError("an unknown error code passed")
        harness.server.shutdown()
    print("candidate qualify: fake appliance runs, golden record, guards and validators passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
