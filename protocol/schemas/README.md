# `protocol/schemas/`

Source-of-truth schemas for cross-component contracts.

Expected content (lands in V01-E02 and V01-E13):

- `desired_state.schema.json`
- `worker_status.schema.json`
- `health_event.schema.json`
- `deploy_transaction.schema.json`
- `infer_request.schema.json`
- `infer_result.schema.json`
- `bundle_manifest.schema.json`

## Versioning

Each schema carries a `schemaVersion` field. The bundle format carries a
`bundleFormatVersion` field. See [`docs/architecture/versioning.md`](../../docs/architecture/versioning.md).

Schemas in this directory are consumed as data files by the Rust protocol
crate, the C++ runtime, and the agent. Do not duplicate schema definitions in
language-specific source.
