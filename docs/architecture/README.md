# Architecture documentation

Reference documents that describe TensorPlate's package layout, dependency
direction, and cross-component contracts. These documents are consumed by
contributors and reviewers; user-facing usage docs live alongside their
respective components.

## Index

- [Ownership and dependency direction](ownership.md) — package owners and
  the layering rules enforced at review time.
- [Versioning](versioning.md) — runtime, protocol, schema, and bundle
  format version conventions (V01-E01-F06).
- [Buffer plane ownership](buffer-plane.md) — `BufferRef`, the buffer
  manager, and cleanup-path contracts (V01-E03).
- [Protocol contracts](protocol.md) — JSON Schema format, versioning,
  and Rust/C++ binding strategy (V01-E02-F07).
- [Execution session](execution-session.md) — canonical
  `tensorplate::ExecutionSession` interface, NVI pattern, lifecycle state
  machine, async method shape, and event taxonomy (V01-E04).
- [Non-GPU lifecycle compatibility review](non-gpu-lifecycle-review.md) —
  v0.1.0 paper exercise confirming the V01-E04 contract is implementable
  by a future Kria/Vitis AI adapter without public interface revision
  (V01-E04-F07-T02).

Additional architecture topics are added as the v0.1.0 epics land.

## Related contributor docs

- [Jetson cross-compile setup](../contributing/jetson-cross-compile.md)
- [Jetson target validation](../contributing/jetson-target-validation.md)
- [Local validation commands](../contributing/local-validation.md)
