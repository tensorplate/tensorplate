# T4 Hardware-in-loop tests

Full-stack tests run on target hardware (Jetson Orin in v0.1.0). Validates
end-to-end deploy and inference, agent supervision, observability heartbeat
loss, and crash-loop transitions.

Gated to release branches. Not required for ordinary PRs. Requires a
physical device; see [`docs/architecture/`](../../docs/architecture/) for
device setup once release validation lands.

Until the automated release validation HIL suite lands, the manual Jetson
adapter/runtime target validation runbook lives at
[`docs/contributing/jetson-target-validation.md`](../../docs/contributing/jetson-target-validation.md).
That runbook runs T1/T2/T3 natively on the device with TensorRT enabled;
it is target evidence, not a replacement for the future full-stack HIL
gate.
