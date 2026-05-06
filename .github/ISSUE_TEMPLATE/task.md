---
name: Task
about: Track a concrete implementation, test, documentation, or validation task under a Feature.
title: "[Task]: "
labels: "type: task"
---

## Overview

Parent Feature: #

Parent Epic: #

Describe the exact work item and expected deliverable.

## Acceptance Criteria

- [ ] 
- [ ] 
- [ ] 

## Technical Tasks

Implementation:

- [ ] Confirm expected files or package paths:
- [ ] Preserve downward-only layer dependencies.
- [ ] Use `Result<T>` for fallible hardware-boundary operations where applicable.
- [ ] Keep deployment-specific behavior in config, not hardcoded logic.
- [ ] Use `BufferRef` / `TensorView` for cross-layer payload memory where applicable.
- [ ] Keep RAII hardware wrappers inside adapter internals where applicable.
- [ ] Put shared mocks in `test/mocks/`, not inline in test files.

Verification:

- [ ] T1 unit tests:
- [ ] T2 integration tests:
- [ ] T3 adapter contract tests:
- [ ] T4 hardware-in-loop:
- [ ] T5 benchmark regression:
- [ ] `clang-format`:
- [ ] `clang-tidy`:
- [ ] `cargo fmt`:
- [ ] `cargo clippy`:
- [ ] ASAN/UBSAN:

## Definition of Done

- [ ] Acceptance criteria are met.
- [ ] Required tests and static checks pass.
- [ ] Documentation, schema, feature-flag, protocol, or changelog updates are included if applicable.
- [ ] PR links back to this Task and its parent Feature.
- [ ] Reviewer approval is complete.

## Notes

- 
