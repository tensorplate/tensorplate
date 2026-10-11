# Shared mocks

Fakes and Google Mock implementations consumed by `unit/`, `integration/`,
and `contract/` tests.

Tests must not redefine mocks for the same surface inline. Centralizing
mocks here keeps adapter contract surfaces consistent and prevents drift
between test suites.

Two job bridges live here. `FakeBoundedJobBridge` is scripted: the test
plays the backend, queues each event and decides when it is delivered.
`FixtureJobBridge` has a backend of its own that runs decode and synthesis
jobs on a thread by the sidecar runner's rules, for tests of what sits
above a bridge; link `tp::fixture_job_bridge`.
