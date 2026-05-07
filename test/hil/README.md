# T4 Hardware-in-loop tests

Full-stack tests run on target hardware (Jetson Orin in v0.1). Validates
end-to-end deploy and inference, agent supervision, observability heartbeat
loss, and crash-loop transitions.

Gated to release branches. Not required for ordinary PRs. Requires a
physical device; see [`docs/architecture/`](../../docs/architecture/) for
device setup once V01-E15 lands.
