"""The sidecar's error edge: what a failure may say outside the process.

Upstream libraries can put request text, file paths and voice names in their
exception messages. So an error the sidecar sends to the adapter or keeps
for health carries a code and either a message from :data:`MESSAGES` or a
message the sidecar's own code wrote, and its logs and standard streams
carry exception classes and code locations, never exception text.
"""

from __future__ import annotations

import io
import logging
import os
import re
import sys
import threading
import traceback
from collections.abc import Iterator, Mapping
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType, TracebackType
from typing import Final

from tensorplate_pytorch_backend import protocol
from tensorplate_pytorch_backend.backends.base import BackendError

logger = logging.getLogger("tensorplate.sidecar")

#: The message an error code carries when the sidecar cannot use its own.
MESSAGES: Final[Mapping[str, str]] = MappingProxyType(
    {
        protocol.ERR_CONFIG_INVALID: "the request or the model configuration is invalid",
        protocol.ERR_LOAD_FAILED: "the model could not be loaded",
        protocol.ERR_NOT_READY: "the backend is not ready",
        protocol.ERR_SHAPE_MISMATCH: "the input does not match what the model accepts",
        protocol.ERR_UNSUPPORTED: "the request is not supported",
        protocol.ERR_OOM_ERROR: "the backend ran out of memory",
        protocol.ERR_TIMEOUT: "the operation timed out",
        protocol.ERR_INFERENCE_FAILED: "inference failed",
        protocol.ERR_INTERNAL: "the sidecar hit an internal error",
        protocol.ERR_CANCELLED: "the operation was cancelled",
        protocol.ERR_UNAVAILABLE: "the backend is unavailable",
        protocol.ERR_RESOURCE_EXHAUSTED: "a resource limit was reached",
    }
)

_OWN_PACKAGE: Final[str] = "tensorplate_pytorch_backend"
_OWN_LOGGERS: Final[str] = "tensorplate"
_IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_.]{0,127}")
_OUT_OF_MEMORY_CLASSES: Final[frozenset[str]] = frozenset({"MemoryError", "OutOfMemoryError"})
_CHAIN_LIMIT: Final[int] = 16
# Shorter chained texts match ordinary words of an authored message by chance.
_MIN_MATCHED_TEXT: Final[int] = 4


@dataclass(frozen=True, slots=True)
class EdgeError:
    """The code and message a failure carries once it leaves the sidecar."""

    code: str
    message: str


def _identifier(name: str) -> str:
    return name if _IDENTIFIER.fullmatch(name) else "<unnamed>"


def exception_class(exc: BaseException) -> str:
    """Name ``exc``'s class, qualified by its module unless it is a builtin."""
    cls = type(exc)
    module = "" if cls.__module__ == "builtins" else f"{cls.__module__}."
    return _identifier(f"{module}{cls.__qualname__}")


def describe(exc: BaseException) -> str:
    """Name ``exc``'s class and innermost frame for a log line, never its text."""
    frames = traceback.extract_tb(exc.__traceback__)
    if not frames:
        return exception_class(exc)
    innermost = frames[-1]
    where = f"{_identifier(Path(innermost.filename).name)}:{innermost.lineno}"
    return f"{exception_class(exc)} at {where} in {_identifier(innermost.name)}"


def code_for(exc: BaseException, default: str) -> str:
    """The error code for an upstream exception: out-of-memory classes, else ``default``."""
    if any(cls.__name__ in _OUT_OF_MEMORY_CLASSES for cls in type(exc).__mro__):
        return protocol.ERR_OOM_ERROR
    return default


def _chain(exc: BaseException) -> Iterator[BaseException | None]:
    """Yield the exceptions chained below ``exc``, then None if any were left unread."""
    seen = {id(exc)}
    pending: list[BaseException | None] = [exc.__cause__, exc.__context__]
    while pending:
        link = pending.pop()
        if link is None or id(link) in seen:
            continue
        if len(seen) > _CHAIN_LIMIT:
            yield None
            return
        seen.add(id(link))
        yield link
        members = getattr(link, "exceptions", ())
        if isinstance(members, tuple):
            pending.extend(member for member in members if isinstance(member, BaseException))
        pending.extend((link.__cause__, link.__context__))


def _repeats_upstream_text(message: str, err: BaseException) -> bool:
    try:
        for link in _chain(err):
            if link is None:
                return True
            if type(link).__module__.partition(".")[0] == _OWN_PACKAGE:
                continue
            texts = [str(link), repr(link)]
            texts.extend(arg for arg in link.args if isinstance(arg, str))
            for attribute in ("filename", "filename2"):
                value = getattr(link, attribute, None)
                if isinstance(value, str):
                    texts.append(value)
            if any(len(text) >= _MIN_MATCHED_TEXT and text in message for text in texts):
                return True
    except Exception:
        return True
    return False


def edge_error(exc: BaseException) -> EdgeError:
    """What ``exc`` may say outside the sidecar.

    A :class:`BackendError` keeps its code and message unless the message
    repeats the text of a chained exception from outside this package;
    anything else becomes ``internal`` or ``oom_error`` with a fixed message.
    """
    if isinstance(exc, BackendError):
        if exc.code not in MESSAGES:
            return EdgeError(protocol.ERR_INTERNAL, MESSAGES[protocol.ERR_INTERNAL])
        message = exc.code_message
        if not isinstance(message, str) or _repeats_upstream_text(message, exc):
            return EdgeError(exc.code, MESSAGES[exc.code])
        return EdgeError(exc.code, message)
    code = code_for(exc, protocol.ERR_INTERNAL)
    return EdgeError(code, MESSAGES[code])


def _without_exception_text(value: object) -> object:
    return exception_class(value) if isinstance(value, BaseException) else value


class EdgeLogFilter(logging.Filter):
    """Keeps exception text and other libraries' messages out of a handler.

    The sidecar's own records keep their message with any exception argument
    reduced to its class; a record from any other logger is replaced by a
    notice naming its level and logger. No record keeps a traceback.
    """

    def filter(self, record: logging.LogRecord) -> bool:
        if record.name == _OWN_LOGGERS or record.name.startswith(f"{_OWN_LOGGERS}."):
            record.msg = _without_exception_text(record.msg)
            if isinstance(record.args, tuple):
                record.args = tuple(_without_exception_text(arg) for arg in record.args)
            elif isinstance(record.args, Mapping):
                record.args = {key: _without_exception_text(v) for key, v in record.args.items()}
        else:
            record.msg = "withheld a %s record from %s"
            record.args = (record.levelname, _identifier(record.name))
        record.exc_info = None
        record.exc_text = None
        record.stack_info = None
        return True


class _WithheldStream(io.TextIOWrapper):
    """A standard stream whose text is discarded, noted once in the log."""

    def __init__(self, name: str) -> None:
        super().__init__(open(os.devnull, "wb"), encoding="utf-8")  # noqa: SIM115
        self._stream_name = name
        self._noted = False

    def write(self, text: str) -> int:
        if text and not self._noted:
            self._noted = True
            logger.warning("withheld text written to %s", self._stream_name)
        return super().write(text)


def _log_uncaught(kind: str, exc: BaseException | None) -> None:
    logger.critical("%s: %s", kind, "unknown" if exc is None else describe(exc))


def configure_process_logging(level: str) -> None:
    """Route the process's logging and standard streams through the edge.

    The sidecar shares the serving worker's stdout and stderr, which the
    worker's supervisor may keep in the service journal. Only the filtered
    log keeps the inherited stderr; descriptors 1 and 2 are pointed at the
    null device, so what native code writes there is discarded as well.
    """
    sys.stdout.flush()
    sys.stderr.flush()
    log = os.fdopen(os.dup(2), "w", buffering=1, encoding="utf-8", errors="backslashreplace")
    null = os.open(os.devnull, os.O_WRONLY)
    os.dup2(null, 1)
    os.dup2(null, 2)
    os.close(null)
    handler = logging.StreamHandler(log)
    handler.addFilter(EdgeLogFilter())
    logging.basicConfig(level=level, handlers=[handler], force=True)
    logging.captureWarnings(True)
    sys.stdout = _WithheldStream("stdout")
    sys.stderr = _WithheldStream("stderr")

    def excepthook(
        _type: type[BaseException], value: BaseException, _tb: TracebackType | None
    ) -> None:
        _log_uncaught("uncaught exception", value)

    def thread_excepthook(args: threading.ExceptHookArgs) -> None:
        _log_uncaught("uncaught exception in a thread", args.exc_value)

    def unraisablehook(unraisable: sys.UnraisableHookArgs) -> None:
        _log_uncaught("unraisable exception", unraisable.exc_value)

    sys.excepthook = excepthook
    threading.excepthook = thread_excepthook
    sys.unraisablehook = unraisablehook


__all__ = [
    "MESSAGES",
    "EdgeError",
    "EdgeLogFilter",
    "code_for",
    "configure_process_logging",
    "describe",
    "edge_error",
    "exception_class",
]
