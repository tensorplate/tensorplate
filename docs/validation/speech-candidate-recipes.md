# Candidate memory sampling

Qualification recipes use the platform memory sampler for each warm-idle and
load window. The sampler performs no deploy, workload generation or admission
change. The operator starts it on the machine being measured after warming the
candidate, or while a separately controlled workload runs.

Build the operator binary once from the matching source checkout with the pinned
Rust toolchain; copy the binary to the measured machine if building elsewhere:

```sh
cargo build --locked --release -p tensorplate-platform --example memory_sample
```

Invoke that binary directly, or use the thin wrapper from the checkout:

```sh
tools/validation/memory-sample.sh target/release/examples/memory_sample \
  --interval 1s --duration 60s --phase warm-idle --domain guest_ram \
  --out /var/tmp/candidate-warm-idle.jsonl \
  --process 1001:agent --process 1002:serving_worker --process 1003:python_sidecar \
  > /var/tmp/candidate-warm-idle-summary.json
```

For VRAM, run a second domain-selected capture using `--domain device_vram`
and only the PIDs that own GPU allocations (for example, the Python sidecar).

The PIDs above are examples: supply the live PIDs and their actual roles. Repeat
with `--phase load` and different output files while the recipe drives its
workload. Phase is an operator assertion, never inferred from usage. Establish
workload completion separately. PID ownership must remain stable for the window;
stop and start a new window after a process restart. This tool does not resolve
deployment generations or detect PID reuse.

Both `guest_ram` and `device_vram` are selected by default. Repeat `--domain`
to select either or both explicitly. An absent NVIDIA process entry is unavailable
for that PID, not zero; agent/CPU-only PIDs may therefore make VRAM attribution
incomplete. To measure all host PIDs but only GPU-owning PIDs, use separate,
explicitly domain-selected invocations with the appropriate process sets. Never
sum RSS/process usage into physical domain consumption.

Each JSON Lines record conforms to `memory_observation.json`. Standard output is
one summary object with `schema_version: "0.1"`, `phase`, `interval_ms`,
`duration_ms` and `report`. The report contains `expected_ticks`,
`completed_ticks`, `late_ticks`, `complete` and `domains`. Each domain includes
`consumed` and `processes`; each process identifies its PID, role and `peak`.
Every peak has `max_bytes` (null if never observed) and `available_samples`.
The warm-idle value is the maximum in the explicitly warm-idle window; the load
peak is the maximum in the load window, using the same platform reducer. These
are sampled maxima, not continuous-time peaks, allocator values or reserves.

Recipes must require `report.complete == true` and every required PID/domain
measurement before importing those values. Exit status is 0 for complete
coverage, 3 for incomplete coverage, and 1 for an invocation/write failure.
Incomplete windows keep the observations and any observed maxima for diagnosis;
they do not establish qualification. A write failure leaves a partial output
file. Existing output files are refused rather than overwritten.

Intervals and durations accept positive integer `ms` or `s` values. Each is
at most 24 hours, with at most 100,000 scheduled ticks and 64 attributed PIDs.
Sampling starts at zero, on a fixed monotonic schedule, with one observation per
selected domain per batch. Overdue ticks are skipped, not replayed. Blocking
source calls can overrun the interval or duration; a late batch or skipped tick
makes coverage incomplete. A hung command needs operator interruption. This
operator tool supplies no hard invocation deadline or pressure directives.

Device sampling first runs one full `nvidia-smi -q -x`; missing/unusable required
readings trigger a fresh header-bearing CSV query pair. Records identify the
actual source and never blend XML and CSV. Host sampling uses `MemAvailable`
and per-PID `VmRSS`. Raw command output and device identities are not written.
Follow [fixture and evidence rules](fixture-and-evidence-rules.md) before
publishing results. The repository's model-free idle recordings test the parser
and loop; they supply no candidate memory or reserve qualification.
