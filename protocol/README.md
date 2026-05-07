# `protocol/`

Language-neutral schemas and generated/hand-written bindings shared between
C++ and Rust components.

## Layout

- `schemas/` — schema source-of-truth (JSON Schema for config and bundles;
  protobuf or equivalent for binary IPC if introduced).
- `rust/` — `tensorplate-protocol` Rust crate consuming the schemas.
- C++ bindings live in [`include/tensorplate/`](../include/tensorplate/) and
  [`runtime/`](../runtime/) and are generated from or kept in sync with
  `schemas/`.

## Ownership

- **Layer:** cross-cutting (data plane <-> management plane contract)
- **Owner:** runtime tech lead with Rust agent reviewer

## Rules

- Schemas are versioned. Unknown schema versions are rejected with typed
  errors per V01-E02.
- Schema breaking changes require a `CHANGELOG.md` entry and bump of
  the protocol or schema version per V01-E01-F06.
- Protocol IDLs are the source of truth. Hand-written bindings are
  acceptable in v0.1 but must be documented against the schema.

Schema content lands in V01-E02.
