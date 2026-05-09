"""TensorPlate Python/PyTorch backend.

V01-E01-F01 ships only the package skeleton. Real backend logic - model
loading, the IPC adapter to ``tensorplate-serving``, capability
publication, and the SmolVLA validation harness - lands in V01-E05.

The version constants below mirror ``include/tensorplate/version.hpp``
and ``tensorplate-protocol``'s Rust constants. Drift between the three
surfaces is caught by the language-specific consistency tests until the
schema files in ``protocol/schemas/`` become the single source of truth
in V01-E02.
"""

from __future__ import annotations

__all__ = [
    "BUNDLE_FORMAT_VERSION",
    "PROTOCOL_VERSION",
    "SKELETON_MARKER",
    "__version__",
]

#: Backend release version. Mirrors ``[project].version`` in pyproject.toml.
__version__: str = "0.1.0.dev0"

#: Cross-process protocol version (mirrors C++ ``kProtocolVersion`` and
#: Rust ``tensorplate_protocol::PROTOCOL_VERSION``).
PROTOCOL_VERSION: str = "0.1"

#: Model bundle on-disk format version (mirrors C++ ``kBundleFormatVersion``
#: and Rust ``tensorplate_protocol::BUNDLE_FORMAT_VERSION``).
BUNDLE_FORMAT_VERSION: str = "0.1"

#: Skeleton marker. Replaced by the real backend surface in V01-E05.
SKELETON_MARKER: str = "tensorplate-pytorch-backend-skeleton"
