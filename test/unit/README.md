# T1 Unit tests

Single-class / single-function tests. No hardware, no process startup, no
filesystem dependency outside of `tmp` and committed fixtures.

Required for every PR. C++ unit tests use Google Test; Rust unit tests use
the `cargo test` harness colocated with each crate.
