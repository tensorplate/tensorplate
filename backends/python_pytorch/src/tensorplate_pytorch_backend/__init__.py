"""TensorPlate Python/PyTorch backend.

V01-E05-F04 lands the IPC codec, the schema-only ``protocol`` constants,
and the ``runner`` module that drives one execution session per
process. The bundled :class:`tensorplate_pytorch_backend.backends.FixtureBackend`
satisfies the V01-E05-F04 contract tests without requiring PyTorch or
SmolVLA; the real TorchScript / SmolVLA backend lands in V01-E05-F05.

The version constants below mirror ``include/tensorplate/version.hpp``
and ``tensorplate-protocol``'s Rust constants. Drift between the three
surfaces is caught by the language-specific consistency tests until the
schema files in ``protocol/schemas/`` become the single source of truth
in V01-E02.
"""

from __future__ import annotations

from tensorplate_pytorch_backend import codec, protocol, runner

__all__ = [
    "BUNDLE_FORMAT_VERSION",
    "PROTOCOL_VERSION",
    "SKELETON_MARKER",
    "__version__",
    "codec",
    "protocol",
    "runner",
]

#: Backend release version. Mirrors ``[project].version`` in pyproject.toml.
__version__: str = "0.1.0.dev0"

#: Cross-process protocol version (mirrors C++ ``kProtocolVersion`` and
#: Rust ``tensorplate_protocol::PROTOCOL_VERSION``).
PROTOCOL_VERSION: str = "0.1"

#: Model bundle on-disk format version (mirrors C++ ``kBundleFormatVersion``
#: and Rust ``tensorplate_protocol::BUNDLE_FORMAT_VERSION``).
BUNDLE_FORMAT_VERSION: str = "0.1"

#: Skeleton marker. Preserved for V01-E01 compatibility; the real
#: backend surface is exposed through ``codec``, ``protocol``, and
#: ``runner``.
SKELETON_MARKER: str = "tensorplate-pytorch-backend-skeleton"
