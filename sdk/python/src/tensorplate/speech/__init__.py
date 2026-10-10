"""Streaming speech surface of the SDK; needs the ``speech`` extra.

Importing this package without ``tensorplate-python[speech]`` installed
raises :class:`~tensorplate.errors.MissingDependencyError`. ``import
tensorplate`` never imports it.
"""

from __future__ import annotations

from tensorplate._extras import require_speech

require_speech()
