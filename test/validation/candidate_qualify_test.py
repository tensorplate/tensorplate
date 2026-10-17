#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Drive tools/validation/candidate-qualify.py end to end against a fake appliance.

The fake `tensorplate` CLI, serving worker, memory sampler and ballast answer
in the shapes the real ones use, so the tool's reading of every seam is
exercised without a GPU; every guard is then broken once to show the record
turns `fail` or `incomplete` for its own reason. Set
TP_CANDIDATE_QUALIFY_UPDATE_GOLDEN=1 to rewrite the committed synthetic record.
`--filed-records [DIR]` checks only the records filed from hardware runs.
"""

from __future__ import annotations

import base64
import contextlib
import errno
import functools
import http.server
import importlib.util
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
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools/validation/candidate-qualify.py"
INVENTORY = ROOT / "test/models/speech/candidate_fixture_inventory.json"
GOLDEN = ROOT / "test/validation/fixtures/candidate_qualification_record_synthetic.json"
FAKES = ROOT / "test/validation/fixtures/candidate_qualify_fakes"
BUNDLES = ROOT / "test/models/bundles/v0_1"
EVIDENCE = ROOT / "docs/validation/evidence"
sys.path.insert(0, str(ROOT / "tools/validation"))
sys.path.insert(0, str(ROOT / "sdk/python/src"))
import candidate_audio as audio  # noqa: E402
import candidate_record as record_mod  # noqa: E402
import jsonschema  # noqa: E402
import vram_ballast as ballast_mod  # noqa: E402

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
            str(Path(__file__).resolve()),
            "--run-tool",
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
            command += [
                "--oom-ballast-python",
                str(self.ballast),
                "--oom-ballast-bytes",
                "1024",
            ]
        command += list(extra)
        completed = subprocess.run(
            command,
            capture_output=True,
            text=True,
            timeout=300,
            env=env or self.env(),
            check=False,
        )
        record_path = evidence / "record.json"
        record = json.loads(record_path.read_text()) if record_path.exists() else None
        return completed, record, evidence


def check_schema(record: dict, schema: dict) -> None:
    jsonschema.validate(record, schema)
    record_mod.validate_record(record, schema)
    record_mod.validate_error_codes(record, record_mod.load_error_codes())
    record_mod.validate_ballast(record)


def check_filed_records(schema: dict, evidence: Path = EVIDENCE) -> int:
    """Every filed record is valid, candidate-only and has the result its own steps derive."""
    filed_paths = sorted(evidence.glob("speech-candidate-*/run-*/record.json"))
    assert filed_paths, "no recorded candidate run is filed"
    for path in filed_paths:
        filed = json.loads(path.read_text())
        check_schema(filed, schema)
        assert filed["provenance"] == "recorded", path
        assert filed["qualification"]["production_evidence"] is False, path
        stop = next(
            (r[len("stopped: ") :] for r in filed["result"]["reasons"] if r[:9] == "stopped: "),
            None,
        )
        derived = record_mod.finalize(json.loads(json.dumps(filed)), stop)["result"]
        assert derived == filed["result"], (path, derived)
        for window in filed["memory"]["windows"].values():
            for name in window.get("observations") or []:
                assert (path.parent / name).is_file(), (path, name)
        assert not (path.parent / "outputs").exists(), f"{path.parent}: outputs/ is private"
    return len(filed_paths)


def check_filed_record_guard(schema: dict) -> None:
    """The filed-record check refuses a broken record, and the mode reports what it checked."""
    filed = len(list(EVIDENCE.glob("speech-candidate-*/run-*/record.json")))
    completed = subprocess.run(
        [sys.executable, __file__, "--filed-records"], capture_output=True, text=True, check=False
    )
    assert completed.returncode == 0 and f"{filed} filed records" in completed.stdout, completed
    source = sorted(EVIDENCE.glob("speech-candidate-*/run-1"))[0]
    with tempfile.TemporaryDirectory() as raw_tmp:
        run = Path(raw_tmp) / source.parent.name / source.name
        shutil.copytree(source, run)
        assert check_filed_records(schema, Path(raw_tmp)) == 1
        good = (run / "record.json").read_text()
        for field, value in (("extra", 1), ("provenance", "synthetic")):
            (run / "record.json").write_text(json.dumps({**json.loads(good), field: value}))
            try:
                check_filed_records(schema, Path(raw_tmp))
            except (AssertionError, jsonschema.ValidationError):
                continue
            raise AssertionError(f"a filed record with {field} set to {value!r} passed")
        completed = subprocess.run(
            [sys.executable, __file__, "--filed-records", raw_tmp], capture_output=True, check=False
        )
        assert completed.returncode != 0, "the mode passed a broken record"
        shutil.rmtree(run)
        try:
            check_filed_records(schema, Path(raw_tmp))
        except AssertionError:
            return
        raise AssertionError("an evidence tree with no filed record passed")


def check_ballast_script(harness: Harness) -> None:
    """The fake ballast is the real script on a fake device: one usage, one report line."""
    script = str(ROOT / "tools/validation/vram_ballast.py")
    for bad in (
        [],
        ["0"],
        ["-5"],
        ["1e3"],
        ["5", "6"],
        ["--leave-free"],
        ["--leave-free", "0"],
        ["--leave-free", "x"],
        ["--leave-free=5"],
        ["--leave-free", "5", "6"],
        ["5", "--leave-free", "5"],
        ["--hold", "5"],
    ):
        answers = [
            subprocess.run(
                [program, script, *bad], capture_output=True, text=True, env=harness.env()
            )
            for program in (sys.executable, str(harness.ballast))
        ]
        for done in answers:
            assert (done.returncode, done.stdout, done.stderr) == (2, "", ballast_mod.USAGE), bad
        assert not harness.oom_marker.exists(), bad
    for rule, count in ((ballast_mod.BYTES, 1024), (ballast_mod.LEAVE_FREE, 4000)):
        arguments = ballast_mod.command_arguments(rule, count)
        assert ballast_mod.parse_rule(arguments) == (rule, count), arguments
    report = ballast_mod.parse_report
    assert report("held 6000 free 10000 left 3984\n") == {"held": 6000, "free": 10000, "left": 3984}
    assert report("held 5 free unknown left unknown\n") == {"held": 5, "free": None, "left": None}
    assert report("refused free 10000\n") == {"held": None, "free": 10000, "left": None}
    assert report("refused free unknown\n") == {"held": None, "free": None, "left": None}
    for line in (
        "",
        "held 5\n",
        "held 5 free 9 left 4",
        "held 5 free 9 left 4 \n",
        "held -5 free 9 left 4\n",
        "held 5 free 9\n",
        "held unknown free 9 left 4\n",
        "refused\n",
        "refused free 9 left 4\n",
        "note: held 5 free 9 left 4\n",
    ):
        assert report(line) is None, line


def oom_of(record: dict) -> dict:
    return next(n for n in record["negatives"] if n["case"] == "oom_at_load")


def ballast_case(
    harness: Harness, schema: dict, *sizing: str, sampler: bool = False, **device: str
) -> tuple[int, dict, dict]:
    completed, record, _ = harness.run(
        "stt",
        "--oom-ballast-python",
        str(harness.ballast),
        *sizing,
        oom=False,
        sampler=sampler,
        env=harness.env(**device),
    )
    assert record is not None, completed.stderr
    check_schema(record, schema)
    assert not harness.oom_marker.exists(), "the ballast was not terminated"
    return completed.returncode, record, oom_of(record)


def ballast_figures(rule: str, requested: int, free, held, left, reason=None) -> dict:
    return {
        "rule": rule,
        "requested_bytes": requested,
        "free_bytes": free,
        "held_bytes": held,
        "left_bytes": left,
        "reason": reason,
    }


def check_ballast_rules(harness: Harness, schema: dict) -> None:
    """The out-of-memory case on the fake device: 10,000 bytes free, an allocator that
    takes blocks of 64, and a second load of the candidate that needs 9,500."""
    case, figures = functools.partial(ballast_case, harness, schema), ballast_figures
    not_run = "negative case oom_at_load did not run"
    code, record, oom = case("--oom-ballast-leave-free", "4000", sampler=True)
    assert code == 0 and record["result"] == {"status": "pass", "reasons": []}, record["result"]
    assert (oom["status"], oom["observed"]["code"]) == ("matched", "oom_error"), oom
    # What was left is what the device reported while the ballast held, not the 4,000 asked for.
    assert oom["ballast"] == figures("leave_free", 4000, 10000, 6000, 3984), oom["ballast"]

    # Leaving as much as the load needs: the load fits, and the case is a mismatch.
    code, record, oom = case("--oom-ballast-leave-free", "9600")
    assert code == 2 and record["result"]["status"] == "fail", record["result"]
    assert (oom["status"], oom["reason"]) == ("mismatched", "the request succeeded"), oom
    assert oom["observed_outcome"] == "deployed as qualify-stt-oom"
    assert oom["ballast"] == figures("leave_free", 9600, 10000, 400, 9552), oom["ballast"]

    # Leaving less than the allocator takes beyond what it is asked for: the device refuses.
    reason = "the ballast was refused its allocation by the device (exit 4)"
    code, record, oom = case("--oom-ballast-leave-free", "10")
    assert code == 3 and not_run in record["result"]["reasons"], record["result"]
    assert (oom["status"], oom["reason"], oom["observed"]) == ("not_run", reason, None), oom
    assert oom["ballast"] == figures("leave_free", 10, 10000, None, None, reason), oom["ballast"]

    # Free memory not above what is to be left: nothing is held and no size is guessed.
    for asked in (10000, 20000):
        reason = (
            f"the ballast held nothing: 10000 bytes were free, not more than the {asked} "
            "to leave free (exit 6)"
        )
        code, record, oom = case("--oom-ballast-leave-free", str(asked))
        assert code == 3 and not_run in record["result"]["reasons"], record["result"]
        assert (oom["status"], oom["reason"], oom["observed"]) == ("not_run", reason, None), oom
        assert oom["ballast"] == figures("leave_free", asked, 10000, None, None, reason)
    code, record, oom = case("--oom-ballast-leave-free", "9999")
    assert oom["status"] == "mismatched" and oom["ballast"]["held_bytes"] == 1, oom

    reason = "the ballast could not read the device's free memory (exit 5)"
    code, record, oom = case("--oom-ballast-leave-free", "4000", TP_FAKE_DEVICE_UNREADABLE="1")
    assert code == 3 and not_run in record["result"]["reasons"], record["result"]
    assert (oom["status"], oom["reason"], oom["observed"]) == ("not_run", reason, None), oom
    assert oom["ballast"] == figures("leave_free", 4000, None, None, None, reason), oom["ballast"]

    # A byte count needs no free figure; the figures it could not read are absent with why.
    code, record, oom = case("--oom-ballast-bytes", "1024", TP_FAKE_DEVICE_UNREADABLE="1")
    assert (oom["status"], oom["reason"]) == ("matched", None), oom
    reason = "the ballast could not read the device's free memory"
    assert oom["ballast"] == figures("bytes", 1024, None, 1024, None, reason), oom["ballast"]
    broken = json.loads(json.dumps(record))
    oom_of(broken)["ballast"]["reason"] = None
    for validate in (
        lambda v: jsonschema.validate(v, schema),
        lambda v: record_mod.validate_record(v, schema),
    ):
        try:
            validate(broken)
        except (jsonschema.ValidationError, record_mod.RecordError):
            continue
        raise AssertionError("a ballast figure absent with no reason passed validation")

    # A byte count the device has no room for is refused by the device, as before.
    reason = "the ballast was refused its allocation by the device (exit 4)"
    code, record, oom = case("--oom-ballast-bytes", "10001")
    assert (oom["status"], oom["reason"]) == ("not_run", reason), oom
    assert oom["ballast"] == figures("bytes", 10001, 10000, None, None, reason), oom["ballast"]

    negative = "takes a count of bytes, 0 to skip the case, not -1"
    for sizing, needle in (
        (("--oom-ballast-bytes", "1024", "--oom-ballast-leave-free", "4000"), "not allowed with"),
        (("--oom-ballast-leave-free", "-1"), f"--oom-ballast-leave-free {negative}"),
        (("--oom-ballast-bytes", "-1"), f"--oom-ballast-bytes {negative}"),
        (("--oom-ballast-report-wait-s", "0"), "must be positive"),
    ):
        completed, record, _ = harness.run("stt", *sizing, oom=False, sampler=False)
        assert completed.returncode == 2 and record is None, (sizing, completed.returncode)
        assert needle in completed.stderr, completed.stderr


def check_ballast_faults(harness: Harness, schema: dict) -> None:
    """A ballast that cannot start, does not report, or is gone before the deploy is judged:
    the case is `not_run` with the reason, in a record that has every earlier case."""
    case = functools.partial(ballast_case, harness, schema, "--oom-ballast-leave-free", "4000")
    tool = load_tool()
    earlier = {
        "malformed_audio_dtype": "matched",
        "malformed_text_utf8": "matched",
        "unsupported_language": "matched",
        "corrupt_artifact_digest": "matched",
        "cancel_during_request": "recorded",
        "oom_at_load": "not_run",
    }

    def kept(record: dict) -> None:
        assert {n["case"]: n["status"] for n in record["negatives"]} == earlier, record["negatives"]
        assert len(record["fixtures"]) == 5
        for fixture in record["fixtures"]:
            summary = fixture["summary"]
            assert summary["failed_count"] == 0 and summary["ok_count"] >= 2, fixture["id"]
        assert record["lifecycle"]["status_after_negatives"]["checks"]["health_ready"] is True

    def not_run(answer: tuple[int, dict, dict], reason: str) -> None:
        """Nothing held or deployed: the candidate is rolled back and the run is incomplete."""
        code, record, oom = answer
        kept(record)
        assert (oom["reason"], oom["observed"], oom["observed_outcome"]) == (reason, None, None), (
            oom
        )
        assert oom["ballast"] == ballast_figures("leave_free", 4000, None, None, None, reason), oom
        assert (
            record["lifecycle"]["status_after_negatives"]["active_deployment_id"] == "qualify-stt"
        )
        teardown = record["lifecycle"]["teardown"]
        assert (teardown["method"], teardown["status"]) == ("rollback", "ok"), teardown
        assert teardown["restored_deployment_id"] == "qualify-stt-predecessor", teardown
        reasons = [r for r in record["result"]["reasons"] if not r.startswith("memory window")]
        assert code == 3 and record["result"]["status"] == "incomplete", record["result"]
        assert reasons == ["negative case oom_at_load did not run"], record["result"]

    # An interpreter that is not there, and one with the executable bit that is not a program.
    garbage = harness.bin / "not-a-program"
    garbage.write_bytes(b"\x00\x01\x02\x03 not a program\n")
    garbage.chmod(0o755)
    for python, error in (
        (harness.bin / "no-such-python", f"FileNotFoundError: {os.strerror(errno.ENOENT)}"),
        (garbage, f"OSError: {os.strerror(errno.ENOEXEC)}"),
    ):
        answer = case("--oom-ballast-python", str(python), sampler=True)
        not_run(answer, f"the ballast interpreter {python} could not be started ({error})")
        assert answer[1]["memory"]["windows"]["after_teardown"]["status"] == "measured"
        assert len(answer[1]["result"]["reasons"]) == 1, answer[1]["result"]

    # No device: the status has its own reason, and the ballast's stderr says which kind.
    not_run(
        case(TP_FAKE_DEVICE_ABSENT="1"),
        "the ballast found no device to hold memory on (exit 3); its last error line: "
        "vram_ballast: no CUDA device",
    )
    # Device code that raises: the last line of the traceback is kept, shortened.
    raised = "ValueError: the fake device's code failed " + "x" * 400
    assert len(raised) > tool.BALLAST_ERROR_CHARS
    not_run(
        case(TP_FAKE_DEVICE_BROKEN="1"),
        "the ballast did not report holding memory (exit 1); its last error line: "
        + raised[: tool.BALLAST_ERROR_CHARS],
    )
    # No report: the wait given runs out, and the ballast is stopped.
    not_run(
        case("--oom-ballast-report-wait-s", "1", TP_FAKE_BALLAST_FAULT="silent"),
        "the ballast did not report within 1 s and was stopped (signal 15)",
    )
    # A first line that is not a report: stopped at once, with no wait for it to end.
    not_run(
        case(TP_FAKE_BALLAST_FAULT="chatter"),
        "the ballast printed a first line that is not a report "
        "('a notice before the report') and was stopped (signal 15)",
    )
    # No end to the first line: stopped once it is longer than a report, not at the wait's end.
    not_run(
        case("--oom-ballast-report-wait-s", "1", TP_FAKE_BALLAST_FAULT="flood"),
        "the ballast printed a first line that is not a report "
        f"({'x' * tool.BALLAST_ERROR_CHARS!r}) and was stopped (signal 15)",
    )
    # Memory another tenant frees between the two reads: more left than was free.
    not_run(
        case(TP_FAKE_DEVICE_FREED="7000"),
        "the ballast's report contradicts itself and is not recorded (held 6000 free 10000 "
        "left 10984: 10984 bytes left free are more than the 10000 free before)",
    )

    # A ballast gone by the time its report is read: the deploy fits, and judges nothing.
    reason = (
        "the ballast exited before the deploy was judged (exit 1); its last error line: "
        "fake ballast: the device was lost"
    )
    code, record, oom = case(TP_FAKE_BALLAST_FAULT="exits")
    kept(record)
    assert (oom["reason"], oom["observed"]) == (reason, None), oom
    assert oom["observed_outcome"] == "deployed as qualify-stt-oom", oom
    assert oom["ballast"] == ballast_figures("leave_free", 4000, 10000, 6000, 3984, reason), oom
    assert "negative case oom_at_load did not run" in record["result"]["reasons"], record["result"]
    assert not any("unexpected outcome" in r for r in record["result"]["reasons"]), record["result"]

    # The tool's last check before it writes refuses figures that contradict each other.
    held = json.loads(GOLDEN.read_text())
    oom_of(held)["ballast"]["held_bytes"] = 10001
    recipe = tool.Recipe.__new__(tool.Recipe)
    recipe.record, recipe.evidence_dir = held, harness.work / "refused"
    recipe.error_codes, recipe.log_lines = record_mod.load_error_codes(), []
    recipe.evidence_dir.mkdir()
    try:
        recipe.write_record()
    except record_mod.RecordError as exc:
        assert "10001 bytes held are more than the 10000 free before" in str(exc), exc
    else:
        raise AssertionError("the tool wrote a record whose ballast figures contradict")
    assert not any(recipe.evidence_dir.iterdir())


def check_ballast_record_rules(schema: dict) -> None:
    """Each rule across the ballast's figures, broken alone in a record the schema accepts."""
    by_bytes = json.loads(GOLDEN.read_text())
    left_free = json.loads(GOLDEN.read_text())
    oom_of(left_free)["ballast"].update(
        rule="leave_free", requested_bytes=4000, held_bytes=6000, left_bytes=3984
    )
    moved = json.loads(GOLDEN.read_text())
    moved["negatives"][0]["ballast"] = dict(oom_of(moved)["ballast"])
    assert moved["negatives"][0]["status"] == "matched"
    broken = [(moved, "/negatives/0/ballast: only the oom_at_load case carries a ballast")]
    for good, change, contradiction in (
        (
            by_bytes,
            {"requested_bytes": 10001, "held_bytes": 10001},
            "10001 bytes held are more than the 10000 free before",
        ),
        (by_bytes, {"held_bytes": 1023}, "1023 bytes held are not the 1024 asked for"),
        (left_free, {"left_bytes": 4001}, "4001 bytes left free are more than the 4000 to leave"),
        (by_bytes, {"left_bytes": 10001}, "10001 bytes left free are more than the 10000 free"),
        (left_free, {"held_bytes": 5999}, "5999 bytes held are not the 10000 free less the 4000"),
        (
            left_free,
            {"free_bytes": None, "reason": "unread"},
            "6000 bytes held under leave_free with no free figure",
        ),
        (
            by_bytes,
            {"held_bytes": None, "reason": "unheld"},
            "the case is matched with no bytes held",
        ),
        (
            by_bytes,
            {"held_bytes": None, "reason": "unheld", "status": "mismatched"},
            "the case is mismatched with no bytes held",
        ),
    ):
        check_schema(good, schema)
        record = json.loads(json.dumps(good))
        oom_of(record)["status"] = change.pop("status", "matched")
        oom_of(record)["ballast"].update(change)
        broken.append((record, f"/negatives/5/ballast: {contradiction}"))
    for record, message in broken:
        jsonschema.validate(record, schema)
        record_mod.validate_record(record, schema)
        try:
            record_mod.validate_ballast(record)
        except record_mod.RecordError as exc:
            assert message in str(exc), (message, exc)
        else:
            raise AssertionError(f"passed the record's check: {message}")


def load_tool():
    spec = importlib.util.spec_from_file_location("candidate_qualify", TOOL)
    tool = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(tool)
    return tool


def check_retirement(harness: Harness, schema: dict) -> None:
    tool = load_tool()
    assert tool.RETIRE_WAIT_MS == 60_000

    wait_schema = {"$ref": "#/definitions/RetirementWait", "definitions": schema["definitions"]}
    good_wait = {
        "status": "complete",
        "bound_ms": 60000,
        "wall_ms": 1000,
        "worker_pids": [102],
        "remaining_pids": [],
    }
    invalid_waits = [None, [], {**good_wait, "unexpected": True}]
    for field in good_wait:
        missing = dict(good_wait)
        del missing[field]
        invalid_waits.append(missing)
    for field, value in (
        ("status", "unknown"),
        ("bound_ms", 0),
        ("bound_ms", 1.5),
        ("wall_ms", -1),
        ("wall_ms", 0.5),
        ("worker_pids", "102"),
        ("worker_pids", [0]),
        ("worker_pids", [1.5]),
        ("remaining_pids", "102"),
        ("remaining_pids", [0]),
        ("remaining_pids", [1.5]),
    ):
        invalid_waits.append({**good_wait, field: value})
    for validate in (
        lambda value: jsonschema.validate(value, wait_schema),
        lambda value: record_mod.validate_record(value, wait_schema),
    ):
        validate(good_wait)
        for invalid in invalid_waits:
            try:
                validate(invalid)
            except (jsonschema.ValidationError, record_mod.RecordError):
                continue
            raise AssertionError(f"malformed retirement wait accepted: {invalid}")

    class Clock:
        now_ms = 0

        def monotonic(self):
            return self.now_ms / 1000

    # The fake sampler still runs as a subprocess. Only worker lifetime and
    # the retirement clock are synthetic, so the 60-second bound costs no sleep.
    for case in (
        "exits",
        "stays",
        "none",
        "gone",
        "unreadable",
        "poll_error",
        "bad_event",
        "two_workers",
        "multiple",
        "operation_fails",
        "reaped",
        "no_sampler",
    ):
        clock = Clock()
        registered = set()
        polls = []
        captured = []
        closed = []
        operated = False

        def pidfd_open(pid):
            assert not operated, "worker must be captured before replacement"
            captured.append(pid)
            if case == "gone":
                raise ProcessLookupError()
            if case == "unreadable":
                raise PermissionError("test refusal")
            return pid + 1000

        class Poll:
            def register(self, fd, event):
                assert event == tool.select.POLLIN
                registered.add(fd)

            def unregister(self, fd):
                registered.remove(fd)

            def poll(self, timeout):
                assert operated, "exit wait must follow replacement"
                polls.append(timeout)
                assert len(polls) <= 3, "wait did not stop at its deadline"
                if case == "poll_error":
                    raise OSError("test poll refusal")
                if case == "bad_event":
                    return [(min(registered), tool.select.POLLNVAL)]
                if case == "stays":
                    clock.now_ms += timeout
                    return []
                clock.now_ms += 1000
                return [
                    (
                        min(registered),
                        tool.select.POLLHUP if case == "reaped" else tool.select.POLLIN,
                    )
                ]

        recipe = tool.Recipe.__new__(tool.Recipe)
        recipe.args = SimpleNamespace(
            sampler=None if case == "no_sampler" else str(harness.sampler),
            window="20ms",
            sample_interval="10ms",
        )
        recipe.evidence_dir = harness.work / f"retirement-{case}"
        recipe.evidence_dir.mkdir()
        recipe.window_ms, recipe.interval_ms = 20, 10
        recipe.record = {"memory": {"windows": {}}}
        recipe.log = lambda message: None

        def discover():
            if not operated:
                workers = [] if case == "none" else [102, 102]
                if case == "multiple":
                    workers.append(104)
            else:
                workers = [106, 108] if case == "two_workers" else [106]
            return (
                [{"pid": 101, "role": "agent"}]
                + [{"pid": pid, "role": "serving_worker"} for pid in workers]
                + [{"pid": 107, "role": "python_sidecar"}]
            )

        recipe.discover_pids = discover
        with (
            mock.patch.object(
                tool, "os", SimpleNamespace(pidfd_open=pidfd_open, close=closed.append)
            ),
            mock.patch.object(
                tool,
                "select",
                SimpleNamespace(
                    poll=Poll,
                    POLLIN=tool.select.POLLIN,
                    POLLHUP=tool.select.POLLHUP,
                    POLLNVAL=tool.select.POLLNVAL,
                ),
            ),
            mock.patch.object(tool, "time", clock),
        ):
            try:
                with recipe.idle_after_replacement("candidate_warm_idle"):
                    operated = True
                    if case == "operation_fails":
                        raise tool.RecipeError("test deploy failure")
            except tool.RecipeError:
                assert case == "operation_fails"
            else:
                assert case != "operation_fails", "operation failure was swallowed"
        expected_closed = [] if case in ("none", "gone", "unreadable", "no_sampler") else [1102]
        if case == "multiple":
            expected_closed.append(1104)
        assert closed == expected_closed, (case, closed)
        if case in ("operation_fails", "no_sampler"):
            assert not polls and not recipe.record["memory"]["windows"]
            if case == "no_sampler":
                assert not captured
            continue
        window = recipe.record["memory"]["windows"]["candidate_warm_idle"]
        jsonschema.validate(
            window, {"$ref": "#/definitions/MemoryWindow", "definitions": schema["definitions"]}
        )
        derived = json.loads(GOLDEN.read_text())
        derived["memory"]["windows"]["candidate_warm_idle"] = window
        record_mod.finalize(derived)
        check_schema(derived, schema)
        expected_result = "incomplete" if window["status"] == "incomplete" else "pass"
        assert derived["result"]["status"] == expected_result, (case, derived["result"])
        wait = window["retirement_wait"]
        assert wait["bound_ms"] == 60_000
        if case in ("stays", "unreadable", "poll_error", "bad_event", "two_workers"):
            assert window["status"] == "incomplete", (case, window)
            assert not window["observations"] and not (recipe.evidence_dir / "memory").exists()
            assert "102" in window["reason"] or case == "two_workers", window["reason"]
            if case == "stays":
                assert wait["status"] == "incomplete" and wait["wall_ms"] == 60_000
                assert wait["remaining_pids"] == [102]
                assert "exit not confirmed" in window["reason"]
            elif case == "two_workers":
                assert "106, 108" in window["reason"]
                recipe.finish_window(recipe.start_window("candidate_load", "load"))
                assert (
                    recipe.record["memory"]["windows"]["candidate_load"]["status"] == "measured"
                )
            else:
                assert wait["status"] == "incomplete"
                if case == "poll_error":
                    assert "cannot observe worker exit: test poll refusal" in window["reason"]
                elif case == "bad_event":
                    assert "poll event" in window["reason"]
                else:
                    assert "cannot watch serving worker PID 102" in window["reason"]
        else:
            assert window["status"] == "measured" and window["report"]["complete"]
            assert wait["remaining_pids"] == []
            if case in ("none", "gone"):
                assert wait["status"] == "not_needed" and wait["wall_ms"] == 0
                assert not polls and wait["worker_pids"] == []
            else:
                assert wait["status"] == "complete"
                assert wait["wall_ms"] == (2000 if case == "multiple" else 1000)
                assert wait["worker_pids"] == ([102, 104] if case == "multiple" else [102])
                assert captured == wait["worker_pids"], "non-worker or duplicate PID captured"

    # Check the ordering at every recipe call site, including the initial deploy
    # (which may replace a worker left by an earlier qualification run).
    recipe = tool.Recipe.__new__(tool.Recipe)
    recipe.predecessor = recipe.candidate = {"dir": Path("unused")}
    recipe.predecessor_id, recipe.deployment_id = "predecessor", "candidate"
    recipe.record = {
        "lifecycle": {},
        "memory": {"windows": {"candidate_load": {"reason": None}}},
    }
    recipe.args = SimpleNamespace(timing_iterations=0)
    recipe.client = object()
    recipe.last_deploy_failure_code = None
    order = []

    @contextlib.contextmanager
    def idle(name):
        order.append(("capture", name))
        yield
        order.append(("wait", name))

    def deploy(bundle, deployment):
        order.append(("deploy", deployment))
        return dict(
            deployment_id=deployment,
            bundle_digest="digest",
            phase="active",
            wall_ms=0,
            status="ok",
        )

    def rollback():
        order.append(("rollback", "predecessor"))
        return {"status": "ok"}

    recipe.idle_after_replacement = idle
    recipe.deploy, recipe.rollback = deploy, rollback
    recipe.status_snapshot = lambda *args: {"checks": {"ready": True}}
    recipe.start_window = lambda *args: None
    recipe.finish_window = lambda *args: None
    recipe.window_running = lambda *args: False
    recipe.run_negatives = lambda: None
    recipe.run_steps()
    assert order == [
        ("capture", "predecessor_idle"),
        ("deploy", "predecessor"),
        ("wait", "predecessor_idle"),
        ("capture", "candidate_warm_idle"),
        ("deploy", "candidate"),
        ("wait", "candidate_warm_idle"),
        ("capture", "after_teardown"),
        ("rollback", "predecessor"),
        ("wait", "after_teardown"),
    ], order

    order.clear()
    recipe.rollback = lambda: {"status": "failed"}
    try:
        recipe.run_steps()
    except tool.RecipeError as exc:
        assert "rollback did not restore" in str(exc)
    else:
        raise AssertionError("failed rollback opened an idle window")
    assert order[-1] == ("capture", "after_teardown")

    # A real kernel PID handle signals exit even before its parent reaps it.
    with subprocess.Popen([sys.executable, "-c", "pass"]) as child:
        fd = os.pidfd_open(child.pid)
        try:
            wait, reason = recipe.wait_for_retirement({child.pid: fd}, [])
            assert reason is None and wait["status"] == "complete"
            assert wait["remaining_pids"] == []
            assert child.pid not in tool.children_of(os.getpid()), "zombie included in sampling"
        finally:
            os.close(fd)


def main() -> int:
    if sys.argv[1:2] == ["--run-tool"]:
        # The fake appliance's PID 102 is synthetic; never observe a host process
        # that happens to have that number. Retirement has its own lifetime fake.
        tool = load_tool()
        with mock.patch.object(tool.os, "pidfd_open", side_effect=ProcessLookupError):
            return tool.main(sys.argv[2:])
    schema = record_mod.load_schema()
    if sys.argv[1:2] == ["--retirement-only"]:
        with tempfile.TemporaryDirectory(prefix="tp-retirement-") as tmp:
            harness = Harness(Path(tmp))
            try:
                check_retirement(harness, schema)
            finally:
                harness.server.shutdown()
        print("candidate qualify: retirement guards and kernel PID exit passed")
        return 0
    if sys.argv[1:2] == ["--filed-records"]:
        count = check_filed_records(schema, Path(sys.argv[2]) if sys.argv[2:] else EVIDENCE)
        print(f"candidate qualify: {count} filed records are valid and derive their own result")
        return 0
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
        check_retirement(harness, schema)
        check_ballast_rules(harness, schema)
        check_ballast_faults(harness, schema)
        check_ballast_record_rules(schema)
        check_ballast_script(harness)

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
        assert record["negatives"][5]["ballast"] == {
            "rule": "bytes",
            "requested_bytes": 1024,
            "free_bytes": 10000,
            "held_bytes": 1024,
            "left_bytes": 8976,
            "reason": None,
        }
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
            "reason": "no --oom-ballast-bytes or --oom-ballast-leave-free given",
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

        check_filed_records(schema)
        check_filed_record_guard(schema)

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

        # A deploy timeout not above the agent's warm timeout cuts a load failure off as `timeout`.
        completed, record, evidence = harness.run("tts", "--agent-timeout-ms", "30000")
        assert completed.returncode == 2 and record["result"]["status"] == "fail"
        cut_off = {n["case"]: n for n in record["negatives"]}
        for case in ("unsupported_voice", "oom_at_load"):
            assert (
                cut_off[case]["status"] == "mismatched"
                and cut_off[case]["observed"]["code"] == "timeout"
            ), cut_off[case]
        assert cut_off["corrupt_artifact_digest"]["status"] == "matched"
        after = record["lifecycle"]["status_after_negatives"]
        assert after["last_error"]["code"] == "load_failed", after
        assert (
            "status_after_negatives checks failed: agent_state_as_expected"
            in (record["result"]["reasons"])
        )
        log = (evidence / "run.log").read_text()
        assert log.count("cli rollback: status=busy exit=5") == 1, log
        assert record["lifecycle"]["teardown"]["status"] == "ok"
        assert record["memory"]["windows"]["after_teardown"]["status"] == "measured"

        completed, record, evidence = harness.run(
            "stt", env=harness.env(TP_FAKE_CLI_BUSY_ROLLBACKS="2")
        )
        assert completed.returncode == 0, record["result"]
        log = (evidence / "run.log").read_text()
        assert log.count("cli rollback: status=busy exit=5") == 2, log
        assert log.count("cli rollback: status=ok exit=0") == 1, log
        assert record["lifecycle"]["teardown"]["restored_deployment_id"] == (
            "qualify-stt-predecessor"
        )

        completed, record, evidence = harness.run(
            "stt",
            "--teardown-busy-wait-ms",
            "1500",
            env=harness.env(TP_FAKE_CLI_BUSY_ROLLBACKS="1000"),
        )
        assert completed.returncode == 2 and record["result"]["status"] == "fail"
        teardown = record["lifecycle"]["teardown"]
        assert teardown["status"] == "failed" and teardown["reason"] == (
            "not_ready: agent is busy with an in-flight transaction (still busy after 1500 ms)"
        ), teardown
        assert teardown["wall_ms"] >= 1500
        assert record["memory"]["windows"]["after_teardown"]["status"] == "not_run"
        polls = (evidence / "run.log").read_text().count("cli rollback: status=busy exit=5")
        assert 2 <= polls <= 4, polls

        # A restore slower than the CLI's default timeout still completes.
        completed, record, _ = harness.run(
            "stt", env=harness.env(TP_FAKE_CLI_SLOW_ROLLBACK_MS="40000")
        )
        assert completed.returncode == 0, record["lifecycle"]["teardown"]

        # Slower than the timeout the tool gives it: an error, and only `busy` is retried.
        completed, record, evidence = harness.run(
            "stt", env=harness.env(TP_FAKE_CLI_SLOW_ROLLBACK_MS="200000")
        )
        assert completed.returncode == 2 and record["result"]["status"] == "fail"
        assert record["lifecycle"]["teardown"]["reason"] == (
            "timeout: operation timed out after 120000 ms"
        ), record["lifecycle"]["teardown"]
        assert (evidence / "run.log").read_text().count("cli rollback:") == 1

        for flag, value in (("--agent-timeout-ms", "0"), ("--teardown-busy-wait-ms", "-1")):
            completed, record, _ = harness.run("stt", flag, value)
            assert completed.returncode == 2 and record is None, (flag, completed.returncode)
            assert "must be positive" in completed.stderr, completed.stderr

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

        unpinned = json.loads(harness.inventory.read_text())
        unpinned["suites"]["stt"]["clips"][0]["digest"] = None
        unpinned_path = harness.work / "inventory-unpinned-provisioned.json"
        unpinned_path.write_text(json.dumps(unpinned))
        completed, record, _ = harness.run("stt", "--inventory", str(unpinned_path))
        assert completed.returncode == 1 and record is None
        assert "en-clean-16k-01 is unpinned" in completed.stderr

        # The committed pins are the provisioned clips'; the harness's synthetic clips are not them.
        committed = json.loads(INVENTORY.read_text())["suites"]["stt"]["clips"]
        assert all(isinstance(clip["digest"], str) for clip in committed)
        completed, record, _ = harness.run("stt", "--inventory", str(INVENTORY))
        assert completed.returncode == 1 and record is None
        assert "en-clean-16k-01: digest" in completed.stderr
        assert "does not match the inventory's pin" in completed.stderr

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
