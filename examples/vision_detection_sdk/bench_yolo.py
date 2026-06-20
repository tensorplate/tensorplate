#!/usr/bin/env python3
"""Benchmark YOLO-style detection through the TensorPlate Python SDK."""

from __future__ import annotations

import argparse
import json
import statistics
import sys
import time
from collections import Counter
from pathlib import Path

import tensorplate
from tensorplate.vision import _select_detection_output


def _percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round((pct / 100.0) * (len(ordered) - 1))))
    return ordered[index]


def _summary(values: list[float]) -> dict[str, float]:
    return {
        "p50_ms": _percentile(values, 50.0),
        "p95_ms": _percentile(values, 95.0),
        "p99_ms": _percentile(values, 99.0),
        "mean_ms": statistics.fmean(values) if values else 0.0,
    }


def _load_labels(path: str | None) -> list[str] | None:
    if path is None:
        return None
    with open(path, encoding="utf-8") as handle:
        return [line.strip() for line in handle if line.strip()]


def _render_markdown(report: dict[str, object]) -> str:
    stages = report["stages_ms"]
    assert isinstance(stages, dict)
    lines = [
        "# YOLO SDK Benchmark",
        "",
        f"- endpoint: `{report['endpoint']}`",
        f"- input_size: `{report['input_size']}`",
        f"- iters: `{report['iters']}`",
        f"- warmup: `{report['warmup']}`",
        f"- throughput_req_s: `{report['throughput_req_s']:.2f}`",
        f"- transports: `{report['transports']}`",
        f"- correctness: `{report['correct']}/{report['iters']}`",
        "",
        "| stage | p50 ms | p95 ms | p99 ms | mean ms |",
        "| --- | ---: | ---: | ---: | ---: |",
    ]
    for name, stats in stages.items():
        assert isinstance(stats, dict)
        lines.append(
            f"| {name} | {stats['p50_ms']:.3f} | {stats['p95_ms']:.3f} | "
            f"{stats['p99_ms']:.3f} | {stats['mean_ms']:.3f} |"
        )
    return "\n".join(lines) + "\n"


def run(args: argparse.Namespace) -> dict[str, object]:
    labels = _load_labels(args.labels)
    client = tensorplate.ServingClient(
        args.serving_url,
        discover=args.serving_url is None,
        timeout=args.timeout,
        preferred_transport=args.transport,
    )
    config = tensorplate.PreprocessConfig(input_size=(args.input_size, args.input_size))

    timings: dict[str, list[float]] = {
        "preprocess": [],
        "client_encode": [],
        "http_roundtrip": [],
        "client_decode": [],
        "infer_wall": [],
        "worker_execution": [],
        "postprocess": [],
        "end_to_end": [],
    }
    request_bytes: list[int] = []
    response_bytes: list[int] = []
    transports: Counter[str] = Counter()
    correct = 0

    total_start = time.perf_counter()
    for i in range(args.warmup + args.iters):
        measured = i >= args.warmup
        t0 = time.perf_counter_ns()
        tensor, transform = tensorplate.preprocess(args.image, config)
        t1 = time.perf_counter_ns()
        result = client.infer(args.endpoint, [tensor], profile=True)
        t2 = time.perf_counter_ns()
        output = _select_detection_output(result, args.output_name)
        detections = tensorplate.decode_detections(
            output,
            transform,
            score_threshold=args.score_threshold,
            nms_threshold=args.nms_threshold,
            labels=labels,
            transposed=args.transposed,
        )
        t3 = time.perf_counter_ns()
        if not measured:
            continue

        timings["preprocess"].append((t1 - t0) / 1e6)
        timings["infer_wall"].append((t2 - t1) / 1e6)
        timings["postprocess"].append((t3 - t2) / 1e6)
        timings["end_to_end"].append((t3 - t0) / 1e6)
        transports[result.transport] += 1
        if result.client_timing is not None:
            timings["client_encode"].append(result.client_timing.encode_ns / 1e6)
            timings["http_roundtrip"].append(result.client_timing.http_roundtrip_ns / 1e6)
            timings["client_decode"].append(result.client_timing.decode_ns / 1e6)
            request_bytes.append(result.client_timing.request_bytes)
            response_bytes.append(result.client_timing.response_bytes)
        if result.timing is not None and result.timing.execution_latency_ns is not None:
            timings["worker_execution"].append(result.timing.execution_latency_ns / 1e6)
        if args.expected_count is None or len(detections) == args.expected_count:
            correct += 1

    elapsed = time.perf_counter() - total_start
    measured_elapsed = sum(timings["end_to_end"]) / 1000.0
    return {
        "endpoint": args.endpoint,
        "image": str(args.image),
        "input_size": args.input_size,
        "iters": args.iters,
        "warmup": args.warmup,
        "transport_preference": args.transport,
        "transports": dict(transports),
        "throughput_req_s": args.iters / measured_elapsed if measured_elapsed > 0 else 0.0,
        "wall_elapsed_s": elapsed,
        "correct": correct,
        "expected_count": args.expected_count,
        "request_bytes_p50": _percentile([float(v) for v in request_bytes], 50.0),
        "response_bytes_p50": _percentile([float(v) for v in response_bytes], 50.0),
        "stages_ms": {name: _summary(values) for name, values in timings.items() if values},
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--serving-url", default=None)
    parser.add_argument("--input-size", type=int, default=640)
    parser.add_argument("--iters", type=int, default=300)
    parser.add_argument("--warmup", type=int, default=30)
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--transport", choices=["auto", "binary", "json"], default="auto")
    parser.add_argument("--output-name", default=None)
    parser.add_argument("--score-threshold", type=float, default=0.25)
    parser.add_argument("--nms-threshold", type=float, default=0.45)
    parser.add_argument("--transposed", action="store_true")
    parser.add_argument("--labels", default=None)
    parser.add_argument("--expected-count", type=int, default=None)
    parser.add_argument("--format", choices=["json", "markdown"], default="json")
    parser.add_argument("--output", default=None)
    args = parser.parse_args(argv)

    report = run(args)
    rendered = json.dumps(report, indent=2, sort_keys=True) if args.format == "json" else _render_markdown(report)
    if args.output:
        Path(args.output).write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
