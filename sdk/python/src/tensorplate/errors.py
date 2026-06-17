"""Typed exceptions raised by the TensorPlate SDK."""

from __future__ import annotations


class TensorPlateError(Exception):
    """Base class for every error raised by the TensorPlate SDK.

    Transport, protocol, and serving-failure subtypes derive from this
    base so callers can catch the entire SDK error surface with a single
    ``except`` clause. The concrete subtypes are added alongside the
    serving client.
    """
