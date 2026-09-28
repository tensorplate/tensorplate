"""Files a runner profile may open: the artifact set its bundle entry lists.

A runner entry names every file the runner reads by a path relative to the
entry's directory and a ``sha256:`` digest. :func:`load_artifact_set`
refuses unsafe paths and verifies every digest before a runner opens
anything, and a runner then reaches its files only through the returned
:class:`ArtifactSet`, so it never reads a file the bundle did not pin.
"""

from __future__ import annotations

import hashlib
import os
import re
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType
from typing import Any

from tensorplate_pytorch_backend.backends.base import BackendError
from tensorplate_pytorch_backend.protocol import ERR_CONFIG_INVALID, ERR_LOAD_FAILED

_DIGEST = re.compile(r"sha256:([0-9A-Fa-f]{64})")
_READ_CHUNK_BYTES = 1 << 20


def _is_safe_relative_path(path: str) -> bool:
    # The bundle verifier's rule (protocol/rust/src/bundle.rs,
    # artifact_path_is_safe), plus NUL, which no filesystem call accepts.
    if not path or path.startswith("/") or "\\" in path or "\0" in path:
        return False
    return all(segment not in ("", ".", "..") for segment in path.split("/"))


def _sha256_hex(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(_READ_CHUNK_BYTES):
            digest.update(chunk)
    return digest.hexdigest()


@dataclass(frozen=True, slots=True)
class ArtifactSet:
    """The verified files of one entry, keyed by their listed relative path."""

    root: Path
    files: Mapping[str, Path]

    def file(self, relative_path: str) -> Path:
        """Return the verified file the entry lists under ``relative_path``."""
        resolved = self.files.get(relative_path)
        if resolved is None:
            raise BackendError(
                ERR_CONFIG_INVALID, "the entry names a file its artifact_set does not list"
            )
        return resolved

    def directory(self, relative_path: str) -> Path:
        """Return a directory whose every file the entry lists and verified.

        For a library that loads a whole directory: anything else inside it,
        a symlink to a directory included, is refused rather than handed on,
        and so is a directory that cannot be listed in full.
        """
        if not _is_safe_relative_path(relative_path):
            raise BackendError(ERR_CONFIG_INVALID, "the entry names an unsafe directory path")
        directory = self.root / relative_path
        if directory.is_symlink():
            raise BackendError(ERR_LOAD_FAILED, "an artifact_set directory is a symlink")
        if not directory.is_dir():
            raise BackendError(ERR_LOAD_FAILED, "an artifact_set directory is missing")
        resolved = directory.resolve()
        if not resolved.is_relative_to(self.root):
            raise BackendError(
                ERR_LOAD_FAILED, "an artifact_set directory resolves outside the entry's directory"
            )
        prefix = f"{relative_path}/"
        if not any(listed.startswith(prefix) for listed in self.files):
            raise BackendError(ERR_CONFIG_INVALID, "the entry lists no file in that directory")
        for current, dirnames, filenames in os.walk(directory, onerror=_unlistable):
            base = Path(current).relative_to(self.root).as_posix()
            linked = any((Path(current) / name).is_symlink() for name in dirnames)
            if linked or any(f"{base}/{name}" not in self.files for name in filenames):
                raise BackendError(
                    ERR_LOAD_FAILED,
                    "an artifact_set directory holds a file its entry does not list",
                )
        return resolved


def _unlistable(_error: OSError) -> None:
    raise BackendError(ERR_LOAD_FAILED, "an artifact_set directory cannot be listed")


def load_artifact_set(entry_path: str | Path, entry: Mapping[str, Any]) -> ArtifactSet:
    """Resolve and verify the ``artifact_set`` of the entry at ``entry_path``.

    Messages name the failing item by its index in the list, never by its
    path, which may carry a voice or model name.
    """
    items = entry.get("artifact_set")
    if not isinstance(items, list) or not items:
        raise BackendError(ERR_CONFIG_INVALID, "the entry must list a non-empty artifact_set")
    root = Path(entry_path).parent.resolve()
    files: dict[str, Path] = {}
    for index, item in enumerate(items):
        if not isinstance(item, dict) or set(item) != {"path", "digest"}:
            raise BackendError(
                ERR_CONFIG_INVALID,
                f"artifact_set item {index} must hold exactly `path` and `digest`",
            )
        relative_path = item["path"]
        if not isinstance(relative_path, str) or not _is_safe_relative_path(relative_path):
            raise BackendError(ERR_CONFIG_INVALID, f"artifact_set item {index} has an unsafe path")
        if relative_path in files:
            raise BackendError(ERR_CONFIG_INVALID, f"artifact_set item {index} repeats a path")
        declared = item["digest"]
        match = _DIGEST.fullmatch(declared) if isinstance(declared, str) else None
        if match is None:
            raise BackendError(
                ERR_CONFIG_INVALID, f"artifact_set item {index} needs a sha256:<64 hex> digest"
            )
        try:
            resolved = (root / relative_path).resolve(strict=True)
        except (OSError, RuntimeError):
            raise BackendError(
                ERR_LOAD_FAILED, f"artifact_set item {index} is missing or unreadable"
            ) from None
        if not resolved.is_relative_to(root):
            raise BackendError(
                ERR_LOAD_FAILED, f"artifact_set item {index} resolves outside the entry's directory"
            )
        if not resolved.is_file():
            raise BackendError(ERR_LOAD_FAILED, f"artifact_set item {index} is not a regular file")
        try:
            actual = _sha256_hex(resolved)
        except OSError:
            raise BackendError(
                ERR_LOAD_FAILED, f"artifact_set item {index} is missing or unreadable"
            ) from None
        if actual != match.group(1).lower():
            raise BackendError(
                ERR_LOAD_FAILED, f"artifact_set item {index} does not match its digest"
            )
        files[relative_path] = resolved
    return ArtifactSet(root=root, files=MappingProxyType(files))


__all__ = ["ArtifactSet", "load_artifact_set"]
