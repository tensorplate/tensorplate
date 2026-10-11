"""Checks that the dependencies of an optional extra are installed."""

from __future__ import annotations

from importlib import import_module

from tensorplate.errors import MissingDependencyError

# Import name to the distribution that provides it.
SPEECH_REQUIRED = {"grpc": "grpcio", "google.protobuf": "protobuf"}


def require_speech() -> None:
    """Raise :class:`MissingDependencyError` unless the ``speech`` extra is installed."""
    for module, distribution in SPEECH_REQUIRED.items():
        try:
            import_module(module)
        except ModuleNotFoundError as exc:
            # Absent means the module or its top-level package was not found;
            # any other missing module is a broken install of the dependency.
            if exc.name not in (module, module.partition(".")[0]):
                raise
            raise MissingDependencyError("speech", distribution, module) from exc
