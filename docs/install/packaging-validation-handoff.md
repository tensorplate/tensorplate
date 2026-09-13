# Packaging Validation Handoff

This document records the packaging artifacts, validation steps, package
versions, hardware assumptions, and known risks that must be reviewed
before release validation on Jetson Orin or Ubuntu 24.04 x86_64 hardware.
The installed backend and the validation procedure depend on the
architecture.

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

1. On Jetson, run
   [`clean-install-runbook.md`](./clean-install-runbook.md). On Ubuntu
   24.04 x86_64, use the
   [candidate asset install path](./external-install.md#runtime-install-on-ubuntu-2404-x86_64)
   and Python/PyTorch prerequisites. The cloud lifecycle procedure and
   hardware evidence remain pending; installing the packages does not
   complete that validation.
2. Run `test/packaging/run.sh` on the target after package install.
3. Run `tensorplate doctor` on a fresh install and again after deploy.
4. Probe service start, status, restart, and stop with systemd.
5. On Jetson, deploy a TensorRT vision bundle and a Python/PyTorch bundle
   when that optional backend is in scope. For release amd64 packages,
   validate a Python/PyTorch bundle that actually executes on the NVIDIA
   GPU. TensorRT is not compiled into that serving worker.

## Package Versions And Runtime Expectations

- Source/runtime package version comes from `packaging/VERSION`.
- Protocol version is `protocol/rust/src/lib.rs::PROTOCOL_VERSION`.
- Bundle format version is `BUNDLE_FORMAT_VERSION`.
- Schema version on every config and event is `0.1` for the v0.1 line.
- Final release preparation must remove development suffixes before the
  annotated release tag is created.

## Hardware Assumptions

- Jetson Orin Nano 8GB Super or Orin NX 16GB, or an x86_64 host with an
  NVIDIA GPU.
- JetPack 6.x with the L4T 36.x BSP on the Jetson; Ubuntu 24.04 on
  x86_64.
- On Jetson, the TensorRT validation path needs the platform's CUDA and
  TensorRT runtime. On x86_64, the release worker ships with TensorRT and
  LibTorch disabled and the Python/PyTorch sidecar enabled; installing
  those native SDKs does not add their adapters to that binary.
- Python 3.10+ for the `python_pytorch` backend. PyTorch wheel choice is
  platform-specific and remains operator policy. The x86_64 GPU path
  requires the optional backend package and a CUDA-capable PyTorch build
  usable by the descriptor's interpreter.

## Known Risks

- Packages must be installed from a trusted local copy, APT repository, or
  GitHub Release asset set; the packaging tree alone is not a publication
  channel.
- Host CI verifies package metadata and staged layout, but live
  `dpkg-buildpackage` and package install behavior must still be checked
  on the target platform.
- The Python/PyTorch backend package does not install PyTorch.
- Published v0.1.x releases contain no amd64 runtime set to upgrade from.
  The x86_64 install path targets the forthcoming v0.2.1 assets or a
  complete candidate artifact set; an upgrade baseline must be supplied
  explicitly or the upgrade stage recorded as skipped with a reason.
- The APT channel serves `jammy` only. The Ubuntu 24.04 x86_64 candidate
  path uses release or local build artifacts, not the Jetson APT recipe.
- Sites that require custom systemd hardening may need drop-in overrides,
  which must be documented in validation evidence.

## Sign-Off Criteria

Validation can accept the packaging handoff when:

1. `test/packaging/run.sh` is green on the target host.
2. `tensorplate doctor` reports no `fail` findings on a clean install.
3. `systemctl enable --now tensorplate-agent` brings the agent to `ready`
   and the serving worker reaches a steady supervisor state.
4. A bundle for the shipped backend deploys, serves, and is rollback-able:
   TensorRT on the Jetson path, or Python/PyTorch with actual CUDA execution
   on the x86_64 GPU path. A parser fixture or a CPU-only run is not GPU
   validation evidence.
