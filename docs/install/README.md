# `docs/install/`

V01-E14 operator docs, V01-E15 handoff documentation, and the reusable
public external install path.

| Document | What it covers |
| --- | --- |
| [`external-install.md`](./external-install.md) | Public install guide that starts from GitHub Release assets, verifies checksums, installs packages, starts services, and covers uninstall/troubleshooting. |
| [`quickstart.md`](./quickstart.md) | External quickstart for deploy, inference, status, logs, metrics, optional backend, and rollback after package install. |
| [`clean-install-runbook.md`](./clean-install-runbook.md) | Step-by-step v0.1.0 install on a Jetson Orin Nano 8GB Super. The canonical procedure for the V01-E15 validation run. |
| [`filesystem-layout.md`](./filesystem-layout.md) | The on-device path / owner / mode contract. Single source: [`protocol/rust/src/install_paths.rs`](../../protocol/rust/src/install_paths.rs). |
| [`services.md`](./services.md) | systemd units, restart policy, hardening profile, and lifecycle commands. |
| [`lifecycle.md`](./lifecycle.md) | Reinstall, upgrade, downgrade, remove, and purge policy. |
| [`python-pytorch-backend.md`](./python-pytorch-backend.md) | The separately installable Python/PyTorch backend, PyTorch wheel selection, and how doctor reports its status. |
| [`e15-handoff.md`](./e15-handoff.md) | Artifacts, validation steps, package versions, hardware assumptions, and known risks for the V01-E15 hardware gate. |

Doctor finding catalog: see [`docs/cli/doctor.md`](../cli/doctor.md).
