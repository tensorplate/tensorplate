"""Low-level client for the TensorPlate v0.1 serving HTTP envelope."""

from __future__ import annotations


class ServingClient:
    """Synchronous client for the v0.1 ``/infer`` serving HTTP envelope.

    Placeholder: serving-envelope marshalling, typed-error mapping, the
    ``/health`` readiness check, and endpoint resolution are not yet
    implemented. The class stays model-class-neutral so higher-level
    helpers can build on it.
    """
