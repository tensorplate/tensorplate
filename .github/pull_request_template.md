## Summary

Describe the change and why it is needed.

Linked issue: #

## Acceptance Criteria

- [ ]
- [ ]
- [ ]

## Technical Tasks Completed

- [ ] Implementation:
- [ ] Tests:
- [ ] Documentation:
- [ ] Config/schema/protocol updates:
- [ ] Changelog:

## Implementation Guideline Checklist

- [ ] Change preserves downward-only layer dependencies.
- [ ] Runtime behavior that varies by deployment is config-driven.
- [ ] Fallible hardware-boundary operations return `Result<T>`.
- [ ] No exceptions are introduced at or below the `ModelLoader` interface.
- [ ] Cross-layer payloads use value objects and `BufferRef` / `TensorView` where applicable.
- [ ] Hardware resources are managed through RAII wrappers where applicable.
- [ ] No backend names, device paths, or magic numbers are hardcoded.
- [ ] New feature flags follow `TP_ENABLE_<FEATURE>`.
- [ ] New error codes include a `CHANGELOG.md` entry.

## Public Interface and Review Gates

- [ ] This PR does not change public interfaces.
- [ ] This PR changes `include/tensorplate/` and needs tech lead approval.
- [ ] This PR changes `ModelLoader` methods and includes written justification.
- [ ] This PR adds or changes an adapter and includes T3 contract test evidence.

ModelLoader/interface justification, if applicable:

```text

```

## Test Evidence

Commands run:

```text

```

Required gates:

- [ ] T1 unit tests
- [ ] T2 integration tests
- [ ] T3 adapter contract tests
- [ ] T4 hardware-in-loop validation
- [ ] T5 benchmark regression validation
- [ ] `clang-format`
- [ ] `clang-tidy`
- [ ] ASAN/UBSAN
- [ ] `cargo fmt`
- [ ] `cargo clippy`
- [ ] Python tests/lint/type checks

Skipped checks and reason:

```text

```

## Definition of Done

- [ ] Acceptance criteria are met.
- [ ] Required tests and static checks pass or skipped checks are justified.
- [ ] Documentation is updated where applicable.
- [ ] `CHANGELOG.md` is updated where applicable.
- [ ] Reviewer approval is complete.
- [ ] Tech lead approval is complete where required.
