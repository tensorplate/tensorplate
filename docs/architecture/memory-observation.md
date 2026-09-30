# Memory observations

`platform::memory_sampler` parses captured command/proc output into
`tensorplate_protocol::MemoryObservation`. The parsers remain pure. The
`memory_sampler::sampling` submodule adds collection, scheduling and reduction
for operator measurements; neither layer applies admission or pressure policy.
Their observation contract is `protocol/schemas/memory_observation.json`.

Each record names its budget domain, actual source and a caller-supplied
monotonic timestamp relative to the sampling session. Each byte reading is
explicitly `available` with a nonnegative integer `bytes`, or `unavailable`
with a reason (`source_unavailable`, `missing_field`, `unsupported`). An absent
reading is never zero. Byte counts and timestamps are bounded at 2^53−1 for
exact JSON interchange; the timestamp range permits sessions up to 104 days.

| Field | Meaning |
| --- | --- |
| `allocated`, `reserved` | Allocator statistics; unavailable for all current command/proc sources. Neither process usage nor driver reservations substitutes for these. |
| `device_aggregate.capacity` | Physical domain capacity: framebuffer total for VRAM, `MemTotal` for guest RAM. |
| `device_aggregate.available` | Framebuffer free memory or host `MemAvailable`. `MemFree` is not a substitute. |
| `device_aggregate.used` | Driver `used` for VRAM; `MemTotal - MemAvailable` for guest RAM. |
| `device_aggregate.driver_reserved` | The XML framebuffer `reserved` field. The recorded CSV has none, and guest RAM has no corresponding reading. |
| `process_aggregate` | An available process table, including an observed empty table, or an unavailable query. Entries carry PID, caller-supplied role and an individual byte reading. |

`DomainMemory::consumed_bytes()` is capacity minus available, including
external/OS/driver consumption. A consumer must not add process totals or the
policy reserve to it. Driver MiB values are individually rounded: the recorded
XML's used + reserved + free differs from total by 1 MiB. Preserve the readings;
do not reconstruct a missing reserved value by subtraction. Host RSS is also an
aggregate that can include shared pages; summing RSS does not produce disjoint
physical usage or a budget's sidecar residual overhead.

Record availability is `available` when capacity, available memory and every
reported process reading are available. It is `partial` when some domain
reading or a process table is available, otherwise `unavailable`. Allocator
statistics and driver-reserved availability remain independent; an available
record does not claim those quantities exist. Pressure consumers need measured
capacity and available memory and must check them explicitly. Qualification
consumers additionally check every measurement their report requires.

The XML parser reads only direct children under the sole GPU's
`fb_memory_usage` and `processes/process_info`. The pinned pure-Rust
`roxmltree` 0.20.0 parser has no transitive dependencies or vendor linkage;
it accepts the recording's external DTD declaration without fetching it.
Capture input is capped at 4 MiB and XML at 100,000 nodes. Malformed XML,
duplicate fields, invalid units/integers, byte overflow, invalid/duplicate PIDs,
multiple GPUs and readings above capacity fail with `MemorySampleError`.

The CSV fallback consumes the recorded header-bearing `--query-gpu` and
`--query-compute-apps` shapes, including MiB units. Its two inputs belong to one
collection attempt; they are not atomic measurements. It requires exactly one
GPU row and joins process rows to that row by UUID, but publishes no device
identity. CSV and XML remain separate observations. If XML lacks a required
quantity, a collector can obtain a fresh CSV pair and record that source;
parsers never fill XML fields from CSV or the reverse.

The proc parser takes `MemTotal`, `MemAvailable`, and a status input for each
requested PID. It verifies the status PID and reads `VmRSS`, never `VmHWM`.
A missing status input preserves the requested process with unavailable bytes.
The NVIDIA process table contains only processes reported by the driver; an
unlisted PID is not an observed zero. Process roles come from the caller's
ownership map; all other reported PIDs are `external`. Neither executable
names nor sanitized host/device fields determine ownership or platform support.

`test/platform/memory_observation/` holds the immutable recorded inputs;
[their provenance](../../test/platform/accelerator/PROVENANCE.md#the-memory-observation-recordings)
states their limits. `protocol/rust/tests/fixtures/memory_observation_l4_idle.json`
is a projection of the recorded XML with a synthetic sample timestamp and
explicit test PID-to-role attribution. The parser replay compares every field.
These model-free idle readings establish neither peaks under load, model
budgets nor admission reserves. Recording and sampling a workload remains
necessary before a qualification claim.

The operator loop accepts an explicit domain/PID set and bounded sampling plan,
streams observations to a writer and reduces only physical consumption and the
requested individual process readings. Its injected clock and command/proc
boundary allow recorded-input tests. `WindowReducer` is shared library code for
future agent sampling; the recipe performs no separate measurement or reduction.
Missing samples preserve null values and availability counts. Complete coverage
requires every scheduled tick and requested value, with no late batch. An empty
attribution list measures domain totals only. The timestamp is relative to the
sampling session and marks the batch start, before any blocking source reads.

The operator binary is built as the `memory_sample` platform example and invoked
by `tools/validation/memory-sample.sh`; it is not a packaged CLI command. The
[recipe instructions](../validation/speech-candidate-recipes.md) document its
phase labels, output contract and blocking-call limits. The in-agent invocation
deadlines, lifecycle attribution, freshness policy, admission and hysteresis are
separate consumers of this mechanism.
