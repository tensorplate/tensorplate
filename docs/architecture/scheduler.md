# Scheduler

This document is the authoritative description of the v0.1.0 scheduler
contract. It is referenced from
[`CONTRIBUTING.md`](../../CONTRIBUTING.md) and tested by the V01-E06
test tree. Changes here require tech lead approval.

## Canonical name decision (V01-E06-F01-T01)

The canonical public C++ type is **`tensorplate::InferScheduler`**.

There is exactly one canonical public method set:

```
admit          // admit + enqueue
next           // dispatch the next admitted request, or nullopt
on_completion  // remove in-flight accounting
cancel         // cancel queued or in-flight
expire_due     // sweep expired queued requests
on_pressure    // ingest memory / thermal pressure signal
shutdown       // drain queue, tombstone in-flight, reject further admission
metrics        // observer
policy_name    // observer
```

No second interface contract is introduced. Adding a new scheduler
policy means writing a new implementation and registering a factory
closure under a stable policy key; executor / serving-worker code must
not change.

## Strategy pattern

The scheduler is selected at construction time through the policy
registry in [`factory.hpp`](../../include/tensorplate/scheduler/factory.hpp).
The registry holds a `(policy_name, factory)` map; the factory closure
constructs a concrete `InferScheduler` from a validated
`SchedulerConfig` and a `SchedulerRuntimeHooks` struct.

```text
SchedulerConfig                       SchedulerRuntimeHooks
   policy = "fifo"                       event_sink   (optional)
   queue_capacity                        buffer_manager (optional)
   in_flight_capacity                    clock          (optional)
   deadline_margin
   default_service_estimate
   pressure_reject_threshold
        │
        ▼
SchedulerPolicyRegistry::create()
        │
        ▼
   FifoScheduler        ← the only v0.1.0 implementation
   (or future PriorityScheduler, DeadlineFirstScheduler, …)
        │
        ▼
   std::unique_ptr<InferScheduler>
```

v0.1.0 registers exactly one policy: `fifo`. Unknown policy strings
return `Error::Code::Unsupported`; malformed config fields return
`Error::Code::ConfigInvalid`.

## Monotonic deadline domain

Every deadline decision uses the injected `SchedulerClock` (defaulting
to `SystemSchedulerClock`, which wraps `std::chrono::steady_clock`).
Wall-clock time is **never** consulted. This keeps deadline behavior
immune to system-clock adjustments (NTP, DST, manual edits).

Tests inject a `FakeSchedulerClock` (see
[`test/mocks/fake_scheduler_clock.hpp`](../../test/mocks/fake_scheduler_clock.hpp))
so deadline scenarios are deterministic.

## Deadline-aware admission

Admission rejects a request when either:

1. The request is already past its deadline (`now >= deadline`).
2. The estimated completion exceeds `deadline + deadline_margin`.

The estimated completion is computed as

```text
estimated_completion = now
                     + default_service_estimate * queue_depth
                     + default_service_estimate * in_flight_count
                     + per_request_service_estimate
```

`per_request_service_estimate` falls back to
`default_service_estimate` when the request envelope provides no
estimate. v0.1.0 deliberately uses a coarse estimate to keep the
policy simple; finer-grained policies are deferred to v0.2+.

## Queued expiry

`next()` sweeps expired queued requests before returning a dispatchable
request. Callers may also drive the sweep explicitly via
`expire_due()`. Each removed request:

- emits `SchedulerEventKind::Expired` with `error_code = Timeout` and
  the queued wait time populated,
- has its input `BufferRef`s released through
  `release_request_buffers` when a `BufferManager` is wired into
  `SchedulerRuntimeHooks`.

## Cancellation paths

Both queued and in-flight requests can be cancelled by id.

- **Queued cancellation** removes the request from the queue, releases
  its input buffers, and emits `Cancelled` with the queued wait time
  populated.
- **In-flight cancellation** clears the in-flight accounting, records
  the id in `cancelled_in_flight_ids_` so a racing `on_completion`
  becomes a typed no-op, and emits `Cancelled`. The executor still
  owns the `SchedulerRequest` at that point and remains responsible
  for releasing its buffers on the in-flight side.
- **Unknown id** returns `Error::Code::NotReady`.

`shutdown()` cancels every queued and in-flight request with reason
`Shutdown`, releases queued buffers, and flips the scheduler to reject
all further admission with `Error::Code::NotReady`.

## Pressure inputs

`on_pressure()` accepts `PressureSignal` value objects. Each signal
carries source (memory / thermal), severity (normal / warning /
critical), a monotonic timestamp, and bounded free-text detail. The
scheduler records the most recent severity per source and emits the
corresponding `MemoryPressure` / `ThermalPressure` event.

`SchedulerConfig::pressure_reject_threshold` controls whether the
baseline policy rejects new admission while a pressure signal is at or
above the configured severity. Defaults to `critical`. Setting the
threshold to `normal` (or any value not strictly greater than
`Normal`) disables pressure-based rejection (record-only mode).

v0.1.0 baseline policy never cancels or evicts queued / in-flight
work as a direct result of a pressure signal; the threshold only
affects new admission.

## Metrics and events

`metrics()` returns a `SchedulerMetrics` snapshot covering queue
depth, in-flight count, admission counts, rejection counts by reason,
expiry, cancellation, completion outcomes, pressure counts, and
wait-time aggregates. Wait time is monotonic.

`SchedulerEventSink::on_event` receives one `SchedulerEvent` per
state transition. Events carry bounded labels (`endpoint`,
`backend_name`, `policy`) for downstream observability (V01-E12).
The event sink is invoked inside a `try { … } catch (...) {}`; a
throwing sink does not corrupt scheduler state.

## SmolVLA-style async pattern

SmolVLA-class async chunk requests use the same scheduler contract as
synchronous vision requests. Overlapping requests admit as separate
ids; the LeRobot stale-request marker
(`RequestMetadata::stale_after_sequence`) is consulted by the
serving-worker layer (V01-E07) to dispatch a `cancel()` call with
reason `StaleSequence`. The scheduler itself remains policy-neutral on
chunk semantics.

See V01-E06-F07 test fixtures for canonical examples.
