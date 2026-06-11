# `docs/install/`

Public external install guide, clean-room release smoke, operator runbooks,
and validation handoff documentation.

| Document | What it covers |
| --- | --- |
| [`tensorplate-ready.md`](./tensorplate-ready.md) | The primary install story: TensorPlate-ready hosts and the two-command APT install, image/provisioning runbook, stock-machine one-time bootstrap, the v0.1.1 → v0.1.2 upgrade flow, future-version discovery, and host validation with `tensorplate-ready-check.sh`. |
| [`macos-cli.md`](./macos-cli.md) | macOS Apple Silicon CLI-only install through the first-party Homebrew tap. |
| [`external-install.md`](./external-install.md) | Fallback install guide that starts from GitHub Release assets (no APT channel), plus unreleased branch snapshot builds, checksum verification, package install, service start, uninstall, and troubleshooting. |
| [`quickstart.md`](./quickstart.md) | External quickstart for deploy, inference, status, logs, metrics, optional backend, and rollback after package install. |
| [`clean-install-runbook.md`](./clean-install-runbook.md) | Step-by-step v0.1.0 install on a Jetson Orin Nano 8GB Super. The canonical procedure for the release validation run. |
| [`filesystem-layout.md`](./filesystem-layout.md) | The on-device path / owner / mode contract. Single source: [`protocol/rust/src/install_paths.rs`](../../protocol/rust/src/install_paths.rs). |
| [`services.md`](./services.md) | systemd units, restart policy, hardening profile, and lifecycle commands. |
| [`lifecycle.md`](./lifecycle.md) | Reinstall, upgrade, downgrade, remove, and purge policy. |
| [`python-pytorch-backend.md`](./python-pytorch-backend.md) | The separately installable Python/PyTorch backend, PyTorch wheel selection, and how doctor reports its status. |
| [`packaging-validation-handoff.md`](./packaging-validation-handoff.md) | Artifacts, validation steps, package versions, hardware assumptions, and known risks for the hardware validation gate. |

Doctor finding catalog: see [`docs/cli/doctor.md`](../cli/doctor.md).
