"""Modules generated from the stream session schema; needs the ``speech`` extra.

``tensorplate.stream.v1`` holds ``session_pb2`` (the messages) and
``session_pb2_grpc`` (the ``SessionService`` stub and servicer). They are
generated from ``tensorplate/stream/v1/session.proto`` when the package is
built, so a source tree that was never built or installed has none. The
schema is a draft: a release may change these modules incompatibly.

Importing this package without ``tensorplate-python[speech]`` installed
raises :class:`~tensorplate.errors.MissingDependencyError`. ``import
tensorplate`` never imports it.
"""

from __future__ import annotations

from tensorplate._extras import require_speech

require_speech()
