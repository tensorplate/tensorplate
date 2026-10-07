"""Streaming speech surface of the SDK; needs the ``speech`` extra.

Importing this package without ``tensorplate-python[speech]`` installed
raises :class:`~tensorplate.errors.MissingDependencyError`. ``import
tensorplate`` never imports it.
"""

from __future__ import annotations

from importlib import import_module

from tensorplate.errors import MissingDependencyError

_EXTRA = "speech"

# Import name to the distribution that provides it.
_REQUIRED = {"grpc": "grpcio", "google.protobuf": "protobuf"}


def _require_dependencies() -> None:
    for module, distribution in _REQUIRED.items():
        try:
            import_module(module)
        except ModuleNotFoundError as exc:
            # Absent means the module or its top-level package was not found;
            # any other missing module is a broken install of the dependency.
            if exc.name not in (module, module.partition(".")[0]):
                raise
            raise MissingDependencyError(_EXTRA, distribution, module) from exc


_require_dependencies()
