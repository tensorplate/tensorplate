# Shared mocks

Fakes and Google Mock implementations consumed by `unit/`, `integration/`,
and `contract/` tests.

Tests must not redefine mocks for the same surface inline. Centralizing
mocks here keeps adapter contract surfaces consistent and prevents drift
between test suites.
