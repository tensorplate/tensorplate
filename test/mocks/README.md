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
above a bridge; link `tp::fixture_job_bridge`. Its options
(`FixtureJobBridgeOptions`) select the other outcome the bridge header
allows a backend: a full backend refused by `submit` itself, a cancelled
job that completes unacknowledged, and one progress fragment for a job that
allows progress.

`job_bridge_harness.hpp` is for the Python/PyTorch adapter's own bridge: the
session, bridge and channel its factory builds over a sidecar the test
supplies, a thread that plays a `ScriptedSidecar`'s backend with the fixture
bridge's results, and the waits that tell a test nothing is on its way any
more.
