---
name: Bug
about: Report incorrect behavior, regressions, crashes, or contract violations.
title: "[Bug]: "
labels: "type: bug"
---

## Overview

Describe the defect and why it matters.

Affected area:

- [ ] Runtime core
- [ ] ExecutionSession adapter
- [ ] Scheduler
- [ ] Serving mode
- [ ] Buffer plane
- [ ] Input adapter
- [ ] Telemetry or hooks
- [ ] Agent
- [ ] Observability
- [ ] CLI
- [ ] Python SDK
- [ ] ROS 2
- [ ] Config, schema, or protocol
- [ ] CI, build, or tooling
- [ ] Documentation
- [ ] Other:

Severity:

- [ ] P0 - safety, data loss, or release blocker
- [ ] P1 - core runtime, deployment, or CI blocker
- [ ] P2 - important defect with workaround
- [ ] P3 - minor defect or polish

## Reproduction

Steps:

1. 
2. 
3. 

Environment:

- Branch or commit:
- OS:
- Hardware target:
- CUDA/TensorRT/PyTorch version:
- Feature flags:

Expected:

- 

Actual:

- 

Logs, traces, or artifacts:

```text

```

## Acceptance Criteria

- [ ] Root cause is identified and documented in the PR.
- [ ] Fix preserves the relevant implementation contract.
- [ ] Regression test is added at the narrowest appropriate tier.
- [ ] Existing tests continue to pass.
- [ ] CI/static checks required for the affected area pass.
- [ ] Documentation, schema, migration, or changelog notes are updated if behavior changed.

## Technical Tasks

Investigation:

- [ ] Identify suspected files or package paths:
- [ ] Determine whether this violates a guideline contract:
- [ ] Confirm affected test tier or CI gate:

Possible contract impact:

- [ ] Layer dependency direction
- [ ] ExecutionSession NVI contract
- [ ] Backend registry/factory contract
- [ ] InferScheduler strategy contract
- [ ] `Result<T>` and typed error propagation
- [ ] Error handling boundary logging
- [ ] `BufferRef` / `TensorView` ownership
- [ ] RAII resource management
- [ ] Config-driven behavior
- [ ] Desired-state reconciliation
- [ ] Test tier or CI gate regression
- [ ] Unknown

Fix:

- [ ] Add or update regression coverage.
- [ ] Run affected tests and static checks.
- [ ] Document workaround or mitigation if applicable.

## Definition of Done

- [ ] Bug is fixed or explicitly closed as not reproducible / duplicate / expected behavior.
- [ ] Regression coverage exists unless a documented reason is given.
- [ ] Required tests and static checks pass.
- [ ] PR links back to this issue.
- [ ] Reviewer approval is complete.

## Workaround

None known.
