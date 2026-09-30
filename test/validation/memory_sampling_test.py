#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise the operator wrapper using recorded command output, never a GPU."""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "test/platform/memory_observation"
SAMPLER = Path(sys.argv[1]).resolve()

with tempfile.TemporaryDirectory(prefix="tp-memory-test-") as tmp:
    work = Path(tmp)
    stub = work / "nvidia-smi"
    log = work / "calls.jsonl"
    stub.write_text(
        f"#!{sys.executable}\n"
        "import json, os, sys\nfrom pathlib import Path\n"
        f"root = Path({str(FIXTURES)!r})\n"
        f"with open({str(log)!r}, 'a') as out: out.write(json.dumps(sys.argv[1:])+'\\n')\n"
        "mode = os.environ.get('TP_TEST_MEMORY_MODE', 'xml')\n"
        "if mode == 'unavailable': sys.exit(1)\n"
        "if sys.argv[1:] == ['-q', '-x']:\n"
        "    text = (root/'nvidia-smi-q-x.xml').read_text()\n"
        "    if mode == 'csv': text = text.replace('<free>22308 MiB</free>', '')\n"
        "elif sys.argv[1:] == ['--query-gpu=uuid,memory.total,memory.used,memory.free', '--format=csv']:\n"
        "    text = (root/'nvidia-smi-query-gpu.csv').read_text()\n"
        "elif sys.argv[1:] == ['--query-compute-apps=gpu_uuid,pid,used_memory', '--format=csv']:\n"
        "    text = (root/'nvidia-smi-query-compute-apps.csv').read_text()\n"
        "else: sys.exit(2)\n"
        "sys.stdout.write(text)\n"
    )
    stub.chmod(0o700)
    env = dict(os.environ, PATH=str(work) + os.pathsep + os.environ["PATH"])

    def invoke(path, extra=(), mode="xml", phase="warm-idle"):
        return subprocess.run(
            [
                str(ROOT / "tools/validation/memory-sample.sh"),
                str(SAMPLER),
                "--interval",
                "500ms",
                "--duration",
                "1s",
                "--out",
                str(path),
                "--phase",
                phase,
                "--domain",
                "device_vram",
                "--process",
                "5819:python_sidecar",
                *extra,
            ],
            env=dict(env, TP_TEST_MEMORY_MODE=mode),
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
        )

    for mode, phase in [
        ("xml", "warm-idle"),
        ("xml", "load"),
        ("csv", "warm-idle"),
        ("unavailable", "load"),
    ]:
        log.write_text("")
        path = work / f"{mode}-{phase}.jsonl"
        result = invoke(path, mode=mode, phase=phase)
        assert result.returncode == (3 if mode == "unavailable" else 0), result.stderr
        summary = json.loads(result.stdout)
        assert summary["phase"] == phase
        assert summary["interval_ms"] == 500 and summary["duration_ms"] == 1000
        report = summary["report"]
        assert report["complete"] == (mode != "unavailable")
        assert report["expected_ticks"] == report["completed_ticks"] == 2
        observations = [json.loads(line) for line in path.read_text().splitlines()]
        assert len(observations) == 2
        assert (
            observations[0]["sampled_monotonic_ns"]
            < observations[1]["sampled_monotonic_ns"]
            < 1_000_000_000
        )
        assert all(
            o["source"] == ("nvidia_smi_xml" if mode == "xml" else "nvidia_smi_csv")
            for o in observations
        )
        assert report["domains"][0]["consumed"]["max_bytes"] == (
            None if mode == "unavailable" else 726 * 1024 * 1024
        )
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        assert len(calls) == (2 if mode == "xml" else 6)
        if mode == "csv":
            assert all(
                o["device_aggregate"]["driver_reserved"]["availability"]
                == "unavailable"
                for o in observations
            )
        if mode == "xml":
            original = path.read_bytes()
            assert invoke(path).returncode == 1
            assert path.read_bytes() == original

    for index, extra in enumerate(
        [
            ["--phase", "load"],
            ["--domain", "device_vram"],
            ["--process", "5819:agent"],
            ["--process", "0:agent"],
            ["--domain", "shared_pool"],
            ["--unknown", "x"],
        ]
    ):
        path = work / f"invalid-{index}.jsonl"
        assert invoke(path, extra).returncode == 1
        assert not path.exists()

print(
    "memory sampling wrapper: recorded XML, fresh CSV fallback, unavailable and invalid inputs passed"
)
