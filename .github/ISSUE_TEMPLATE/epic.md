---
name: Epic
about: Track a roadmap outcome delivered through child Feature issues.
title: "[Epic]: "
labels: "type: epic"
---

## Overview

Describe the product, operator, or platform outcome this Epic delivers.

## Goals

- [ ] 
- [ ] 
- [ ] 

## Non-goals

- 
- 

## Child Features

Link Feature issues that implement this Epic.

- [ ] #
- [ ] #

## Implementation Guidelines Impact

Relevant architecture contracts from `tensorplate-internal-docs/implementation-guidlines.md`:

- [ ] ModelLoader Non-Virtual Interface (NVI)
- [ ] Backend Abstract Factory + Registry
- [ ] InferScheduler Strategy Pattern
- [ ] Fallback or hook Chain of Responsibility
- [ ] Telemetry or safety Observer/Event Emitter
- [ ] Value Objects for inter-layer data
- [ ] RAII for hardware resources
- [ ] Desired-state reconciliation
- [ ] Config/schema/protocol changes
- [ ] Public interface changes
- [ ] Other:

Expected package areas:

- 

Review gates:

- [ ] Tech lead approval required if `include/tensorplate/` changes.
- [ ] Written justification required if `ModelLoader` methods change.
- [ ] New adapters require T3 adapter contract test evidence.

## Epic Acceptance Criteria

- [ ] All child Feature issues are complete.
- [ ] Integrated behavior is verified across affected runtime, control-plane, or hardware boundaries.
- [ ] Required config, schema, protocol, or feature-flag changes are documented.
- [ ] Release, rollout, or hardware validation risks are documented.
- [ ] `CHANGELOG.md` is updated when user-visible behavior or public contracts change.

## Definition of Done

- [ ] The Epic outcome is demonstrably working end to end.
- [ ] Required child Feature tests and CI checks have passed.
- [ ] Documentation is updated for public APIs, config schemas, feature flags, or workflows.
- [ ] Open follow-up work is captured in linked issues.
