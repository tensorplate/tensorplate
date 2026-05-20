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

| Identifier        | Producer                                | Carrier                                                                                       |
| ----------------- | --------------------------------------- | --------------------------------------------------------------------------------------------- |
| `request_id`      | Serving ingress (per inference request) | Serving HTTP envelope, scheduler events, adapter events, sidecar IPC, logs, metric exemplars. |
| `transaction_id`  | Agent (per deploy transaction)          | Agent transaction journal, worker control requests, rollback events, logs.                    |
| `correlation_id`  | Operator / agent / serving (free join)  | Logs, metrics exemplars, typed errors, status snapshot, doctor findings.                      |

All three share the same lexical policy so consumers can validate them
with one helper
([`validate_correlation_id`](../../protocol/rust/src/correlation_id.rs)).
The agent control API, CLI, serving HTTP envelope, and sidecar IPC all
treat the field as a value carried on the request, not global state.

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
