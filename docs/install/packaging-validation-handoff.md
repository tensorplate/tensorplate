# Packaging Validation Handoff

This document records the packaging artifacts, validation steps, package
versions, hardware assumptions, and known risks that must be reviewed
before release validation on Jetson Orin hardware.

## Packaging Artifacts

| Artifact | Path |
| --- | --- |
| Native package skeleton | `packaging/debian/` |
| Shared maintainer-script helpers | `packaging/scripts/` |
| Default install configs | `packaging/conf/*.json` |
| Backend descriptor | `packaging/backend-metadata/python_pytorch.json` |
| systemd units | `packaging/debian/tensorplate-{agent,observability}.service` |
| Packaging verification suite | `test/packaging/` |
| Operator docs | `docs/install/` |
| Doctor finding catalog | `docs/cli/doctor.md` |
| Single source of truth for paths | `protocol/rust/src/install_paths.rs` |

## Required Validation

1. Run [`clean-install-runbook.md`](./clean-install-runbook.md) on a
   Jetson Orin Nano 8GB Super or equivalent validation target.
2. Run `test/packaging/run.sh` on the target after package install.
3. Run `tensorplate doctor` on a fresh install and again after deploy.
4. Probe service start, status, restart, and stop with systemd.
5. Deploy a TensorRT vision bundle and a Python/PyTorch bundle when those
   paths are in release scope.

## Package Versions And Runtime Expectations

- Source/runtime package version comes from `packaging/VERSION`.
- Protocol version is `protocol/rust/src/lib.rs::PROTOCOL_VERSION`.
- Bundle format version is `BUNDLE_FORMAT_VERSION`.
- Schema version on every config and event is `0.1` for the v0.1 line.
- Final release preparation must remove development suffixes before the
  annotated release tag is created.

## Hardware Assumptions

- Jetson Orin Nano 8GB Super or Orin NX 16GB.
- JetPack 6.x with the L4T 36.x BSP.
- TensorRT, CUDA, and optionally LibTorch installed by the platform
  runtime.
- Python 3.10+ for the `python_pytorch` backend. PyTorch wheel choice is
  platform-specific and remains operator policy.

## Known Risks

- Packages must be installed from a trusted local copy, APT repository, or
  GitHub Release asset set; the packaging tree alone is not a publication
  channel.
- Host CI verifies package metadata and staged layout, but live
  `dpkg-buildpackage` and package install behavior must still be checked
  on the target platform.
- The Python/PyTorch backend package does not install PyTorch.
- Sites that require custom systemd hardening may need drop-in overrides,
  which must be documented in validation evidence.

## Sign-Off Criteria

Validation can accept the packaging handoff when:

1. `test/packaging/run.sh` is green on the target host.
2. `tensorplate doctor` reports no `fail` findings on a clean install.
3. `systemctl enable --now tensorplate-agent` brings the agent to `ready`
   and the serving worker reaches a steady supervisor state.
4. A TensorRT vision bundle deploys, serves, and is rollback-able.
