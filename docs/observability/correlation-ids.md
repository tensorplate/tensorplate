# Correlation IDs (V01-E12-F02)

Correlation IDs join logs, metrics, and typed errors across processes
without exposing implementation detail. The v0.1 policy is
deliberately conservative: bounded length, bounded character set, and
process-local generation when an upstream caller has not supplied a
value.

Wire format: every schema that carries a correlation id accepts a
string of `[A-Za-z0-9_-]{1,64}` bytes. Rust mirror:
[`protocol::correlation_id`](../../protocol/rust/src/correlation_id.rs).

## Identifier kinds

| Identifier       | Producer                                | Carrier                                                                                          |
| ---------------- | --------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `request_id`     | Serving ingress (per inference request) | Serving HTTP envelope, scheduler events, adapter events, sidecar IPC, logs, metric exemplars.    |
| `transaction_id` | Agent (per deploy transaction)          | Agent transaction journal, worker control requests and responses, rollback events, logs.         |
| `correlation_id` | Operator / agent / serving (free join)  | Logs, metrics exemplars, typed errors, status snapshot, doctor findings, runtime control frames. |

All three share the same lexical policy so consumers can validate them
with one helper
([`validate_correlation_id`](../../protocol/rust/src/correlation_id.rs)).
The agent control API, CLI, serving HTTP envelope, and sidecar IPC all
treat the field as a value carried on the request, not global state.

[`worker_control.json`](../../protocol/schemas/worker_control.json)
defines the messages of the runtime control channel between the agent and
a serving worker. The agent's member registry opens one for every worker
it starts, polls it with `ledger_status`, sends `admission_fence` and
`activate` around a deploy's or rollback's commit, and `retire` to the
worker the commit replaces or an unload stops; it sends no `quota_assign`
or `pressure_directive` yet. On it, every request carries a
`correlation_id` and the response that answers it echoes the same value.
`transaction_id` there names the committing deploy transaction for
`admission_fence`, `activate` and `retire` (a restarted worker's
`activate` and an unload's `retire`, which commit nothing, carry a `tx-`
id of their own), and is the literal `runtime` for `quota_assign`,
`ledger_status` and `pressure_directive`, which belong to no transaction.
Each request gets its own `correlation_id`. The schema lets the agent
repeat only the latest request it sent on a channel, with the id it was
first sent with, and asks the worker to answer a repeat with the outcome
it gave. The agent never repeats a request under its first id: a fence or
an activate not answered before the channel's next ledger poll is sent
again after that poll, as a new request with a new `correlation_id` and
the same `transaction_id`, until 3 seconds have passed since the first
attempt. The activate a failed transaction owes a worker that did not
answer it `ok` is sent again later, in calls of its own, with that
transaction's `transaction_id`. The worker keeps no record of
the latest request: a repeat of an applied `admission_fence`, `activate`
or `retire` gets the same answer because each depends only on the
member's admission state, and the operations whose repeat would act
twice are not implemented.

On the sidecar IPC, the speech job and session messages carry no
`correlation_id`. A job's messages are joined by its `job_id`, which the
serving layer assigns when it submits the job, its `session_key`, the serving
layer's key for the logical session, which no client sees, and its
`generation`, the deployment generation the session is bound to.

## Generation

When neither the CLI nor an upstream caller supplies a value,
producers generate one through
[`CorrelationId::from_seed`](../../protocol/rust/src/correlation_id.rs).
The format is `tp_<16 hex digits>` (19 bytes) so the result is in
policy by construction; the seed is a monotonic counter or
`Instant`-derived delta selected by the producer.

## Validation

- Empty strings, values longer than 64 bytes, and characters outside
  `[A-Za-z0-9_-]` are rejected with
  [`DecodeError::InvalidPayload`](../../protocol/rust/src/lib.rs).
- Producers that accept externally-supplied values (HTTP headers,
  sidecar IPC) call
  [`sanitise_or_generate`](../../protocol/rust/src/correlation_id.rs) so
  invalid values are replaced rather than echoed.
- High-cardinality external IDs MUST NOT be used as metric labels;
  metrics carry correlation IDs only as JSON sample-level exemplars
  through `MetricEvent.correlation_id`.

## Propagation paths

### Deploy

```
CLI (--correlation-id?)  ──►  Agent control API
        │                         │
        ▼                         ▼
   Transaction journal      Worker control RPC
        │                         │
        ▼                         ▼
   Agent log events         Serving worker logs
        │                         │
        ▼                         ▼
   Failure reason record    Status projection (`last_correlation_id`)
```

### Inference

```
Serving ingress  ──►  Scheduler  ──►  Session  ──►  Adapter  ──►  Sidecar IPC
       │                  │              │             │              │
       ▼                  ▼              ▼             ▼              ▼
   Serving logs     Scheduler logs   Session logs  Adapter logs  Sidecar logs
       │                  │              │             │              │
       └──────────────────┴──────────────┴─────────────┴──────────────┘
                                 │
                                 ▼
                       Metric exemplar (optional)
                                 │
                                 ▼
                       Typed error response
                                 │
                                 ▼
                Status projection (`last_correlation_id`)
```

Producers that cannot carry the id (legacy adapters) fail closed: the
serving worker rejects a request that loses its correlation id between
ingress and dispatch with a typed `internal` error rather than dropping
the id silently.
