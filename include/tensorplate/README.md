# `include/tensorplate/`

Public C++ headers for the TensorPlate runtime.

## Ownership

- **Layer:** runtime / public interface
- **Language:** C++20
- **Owner:** runtime tech lead

## Scope

- Public types and interfaces consumed by the serving worker, adapters, and tests.
- Includes core value objects (`InferRequest`, `InferResult`, `BufferRef`,
  `TensorView`, `Result<T>`, `Error`) and the `ModelLoader` / `ExecutionSession`
  Non-Virtual Interface in later milestones.
- Includes runtime version constants (`version.hpp`) introduced under V01-E01-F06.

## Rules

- No vendor SDK types (TensorRT, ONNX Runtime, CUDA) appear in this directory.
- No upward dependency on `serving_worker/`, `agent/`, `cli/`, or `observability/`.
- Changes here require tech lead approval per [CONTRIBUTING.md](../../CONTRIBUTING.md).
- Public methods get Doxygen comments before merge.

This directory is initialized as part of V01-E01-F01 and populated by V01-E02
and later epics.
