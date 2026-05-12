# Buffer Plane Ownership Model

This document is the authoritative description of how `BufferRef`, the
buffer manager, and downstream callers cooperate to keep payload memory
deterministic in v0.1.0. It is referenced from
[`CONTRIBUTING.md`](../../CONTRIBUTING.md) and tested by the V01-E03 test
tree. Changes require tech lead approval.

## Layers

| Component | Layer | Purpose |
| --- | --- | --- |
| [`include/tensorplate/buffer/buffer_ref.hpp`](../../include/tensorplate/buffer/buffer_ref.hpp) | public interface | Opaque value-object handle. No pointer. |
| [`include/tensorplate/buffer/tensor_view.hpp`](../../include/tensorplate/buffer/tensor_view.hpp) | public interface | Tensor metadata (dtype, shape, layout, byte-window). No memory. |
| [`include/tensorplate/buffer/buffer_manager.hpp`](../../include/tensorplate/buffer/buffer_manager.hpp) | public interface | CPU-owned buffer plane. Allocator, release, accounting, pressure events. |
| `runtime/src/buffer/*.cpp` | runtime core | Implementation of the above. |

## The rule, in one sentence

A `BufferRef` is an identity card; the buffer manager is the bank that
holds the money. Callers move identity cards around freely; only the
bank ever frees the money, and only once.

## Lifecycle states

1. **Owned** — The buffer manager has live storage backing this id.
   Exactly one logical release per id is expected. Owned handles flow
   through `InferRequest`, the scheduler, the execution session NVI, and
   adapters. Any holder may forward the handle to a downstream stage or
   call `BufferManager::release(handle)` — but the first release wins,
   and a second release returns `Error::Code::Internal`.

2. **Borrowed** — Reserved for v0.2+. v0.1.0 does not issue Borrowed
   handles from the manager, but the enum value is part of the public
   contract.

3. **Released** — The manager has freed the storage. Subsequent
   `release` calls on copies of the handle return
   `Error::Code::Internal`. Data accessors (`data`, `view`) also fail.
   The handle's `id()` and `size_bytes()` remain valid for log
   attribution.

## Copy and move semantics

`BufferRef` is a tiny value object (id + size + ownership tag). Copying
a `BufferRef` is bit-equivalent: both copies carry the same identity and
both point at the same manager record. Moving a `BufferRef` is
equivalent to a copy in the standard-library "valid but unspecified"
sense — the destination has the original identity, the source's fields
are left readable. **Holders that need unique-pointer-style "moved-out
invalidates source" semantics must call `mark_released()` on the source
explicitly.**

The buffer manager — not the type — enforces release-once. Two copies
of an Owned handle can both call `release()`; the first call frees
storage and returns success, the second call returns
`Error::Code::Internal` and increments the manager's `release_failures`
counter. The same is true if release is attempted through a copy that
the caller forgot to update after the first release; downstream
diagnostics will name the offending id.

## Cleanup paths

Three control flows must release every buffer they touched:

- **Cancellation.** The scheduler cancels an in-flight request before it
  reaches the adapter. Cleanup releases every input buffer in the
  request.
- **Timeout.** The request expired against its monotonic deadline.
  Cleanup is identical to cancellation.
- **Error path / partial outputs.** The session allocated one or more
  output buffers and then failed result assembly. Cleanup releases the
  partial outputs without touching the request's input buffers.

The helpers in [`runtime/src/buffer/cleanup.cpp`](../../runtime/src/buffer/cleanup.cpp)
(V01-E03-F03) implement these flows deterministically using
`BufferManager::release_if_owned`, which is idempotent on Released and
null-sentinel handles.

## What is **not** in scope for v0.1.0

- No GPU/CUDA memory pools. The Vitis AI / DPU adapter design review in
  V01-E15 confirms that DPU-compatible memory can be served through an
  adapter-owned copy without changing `BufferRef` semantics.
- No NVMM / DMA-BUF / CUDA IPC zero-copy.
- No fragmentation tuning. Each allocation is a fresh aligned heap block.
- No cross-process tensor transport. The Python sidecar (V01-E05) owns
  its own marshaling on top of the buffer manager's `data()` view.

## Tests that lock the contract

- [`test/unit/buffer_manager_test.cpp`](../../test/unit/buffer_manager_test.cpp)
  — allocation, accounting, release, double-release prevention,
  copy/move under release, view-bounds checks.
- [`test/unit/buffer_cleanup_test.cpp`](../../test/unit/buffer_cleanup_test.cpp)
  — request-buffer cleanup, partial-output cleanup, idempotency on
  duplicate ids.
- [`test/integration/buffer_plane_e2e_test.cpp`](../../test/integration/buffer_plane_e2e_test.cpp)
  — raw bytes → manager → `BufferRef` + `TensorView` → `InferRequest`
  → mock output → `InferResult`.
