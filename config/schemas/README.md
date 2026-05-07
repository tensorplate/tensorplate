# `config/schemas/`

JSON Schema definitions for runtime, agent, and observability configuration
files consumed at deploy and run time.

Expected content (populated in later epics):

- `runtime.schema.json` — runtime configuration consumed by the serving worker.
- `agent.schema.json` — agent configuration including desired-state store
  and bundle directory locations.
- `observability.schema.json` — observability service configuration.

Schema versioning rules are documented in
[`docs/architecture/versioning.md`](../../docs/architecture/versioning.md).
Unknown or unsupported schema versions must surface a typed error per
V01-E02.
