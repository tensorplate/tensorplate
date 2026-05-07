# Changelog

All notable changes to TensorPlate will be documented in this file.

This project follows the spirit of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic versioning once public releases begin.

## [Unreleased]

### Added

- Top-level package skeleton for v0.1: `include/tensorplate/`, `runtime/`,
  `serving_worker/`, `agent/`, `cli/`, `observability/`, `protocol/schemas/`,
  `protocol/rust/`, `config/schemas/`, `test/`, `cmake/`, and
  `docs/architecture/` (V01-E01-F01).
- `docs/architecture/ownership.md` documenting per-package owners, allowed
  dependencies, and forbidden upward dependencies (V01-E01-F01-T02).
- Test tree layout for tiers T1 through T5 plus shared mocks and model
  fixtures, documented in `test/README.md` (V01-E01-F01-T03).

### Changed

- `README.md` repository layout block now reflects the realized v0.1
  package skeleton and links to the ownership document.

### Deprecated

### Removed

### Fixed

### Security
