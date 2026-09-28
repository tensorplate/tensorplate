"""The sidecar's error edge, with canary payloads in every exception path."""

from __future__ import annotations

import contextlib
import io
import json
import logging
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import uuid
from collections.abc import Iterator
from pathlib import Path
from typing import Any, cast

import pytest

from tensorplate_pytorch_backend import codec, configuration, protocol, sanitize
from tensorplate_pytorch_backend.backends import Backend
from tensorplate_pytorch_backend.backends.base import BackendError, RuntimeCapability
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend
from tensorplate_pytorch_backend.runner import SidecarRunner

CANARY = "tp-canary-7f3a9c"
_CANARY_BYTES = CANARY.encode()


class OutOfMemoryError(RuntimeError):
    """Named like torch's CUDA out-of-memory error, which the edge keys on."""


class _Unprintable(Exception):
    def __str__(self) -> str:
        raise ValueError(CANARY)


def _chained(outer: BackendError, inner: BaseException) -> BackendError:
    try:
        raise inner
    except BaseException as exc:
        try:
            raise outer from exc
        except BackendError as err:
            return err


# ----------------------------------------------------------------------
# edge_error
# ----------------------------------------------------------------------


def test_every_error_code_has_a_fixed_message() -> None:
    codes = {value for name, value in vars(protocol).items() if name.startswith("ERR_")}
    assert set(sanitize.MESSAGES) == codes
    assert all(CANARY not in message for message in sanitize.MESSAGES.values())


def test_an_authored_backend_message_passes_the_edge() -> None:
    err = BackendError(protocol.ERR_LOAD_FAILED, "the checkpoint does not match its digest")
    assert sanitize.edge_error(err) == sanitize.EdgeError(
        protocol.ERR_LOAD_FAILED, "the checkpoint does not match its digest"
    )


@pytest.mark.parametrize(
    "render",
    [
        lambda exc: f"decode failed: {exc}",
        lambda exc: f"decode failed: {exc!r}",
        lambda exc: f"decode failed ({exc.args[0]})",
    ],
)
def test_a_message_that_repeats_upstream_text_is_replaced(render: Any) -> None:
    upstream = RuntimeError(f"cannot phonemize {CANARY}")
    err = _chained(BackendError(protocol.ERR_INFERENCE_FAILED, render(upstream)), upstream)
    edge = sanitize.edge_error(err)
    assert edge == sanitize.EdgeError(
        protocol.ERR_INFERENCE_FAILED, sanitize.MESSAGES[protocol.ERR_INFERENCE_FAILED]
    )


def test_a_message_that_repeats_an_upstream_file_name_is_replaced() -> None:
    upstream = FileNotFoundError(2, "No such file or directory", f"/voices/{CANARY}.pt")
    err = _chained(
        BackendError(protocol.ERR_LOAD_FAILED, f"voice file /voices/{CANARY}.pt is missing"),
        upstream,
    )
    assert sanitize.edge_error(err).message == sanitize.MESSAGES[protocol.ERR_LOAD_FAILED]


def test_upstream_text_is_found_below_the_sidecars_own_exceptions() -> None:
    upstream = OSError(f"read {CANARY} failed")
    own = configuration.ArtifactConfigError(f"read {CANARY} failed")
    own.__cause__ = upstream
    err = _chained(BackendError(protocol.ERR_CONFIG_INVALID, str(own)), own)
    assert sanitize.edge_error(err).message == sanitize.MESSAGES[protocol.ERR_CONFIG_INVALID]


def test_the_sidecars_own_exception_text_may_be_repeated() -> None:
    own = configuration.ArtifactConfigError("sidecar config not found")
    err = _chained(BackendError(protocol.ERR_CONFIG_INVALID, str(own)), own)
    assert sanitize.edge_error(err).message == "sidecar config not found"


def test_an_upstream_exception_that_cannot_be_printed_fails_closed() -> None:
    err = _chained(
        BackendError(protocol.ERR_LOAD_FAILED, "the model could not start"), _Unprintable()
    )
    assert sanitize.edge_error(err).message == sanitize.MESSAGES[protocol.ERR_LOAD_FAILED]


def _cause_chain(depth: int) -> BaseException:
    link: BaseException = RuntimeError(f"upstream {CANARY}")
    for _ in range(depth - 1):
        outer = RuntimeError("wrapped")
        outer.__cause__ = link
        link = outer
    return link


@pytest.mark.parametrize(("depth", "replaced"), [(16, False), (17, True)])
def test_a_chain_too_deep_to_read_fails_closed(depth: int, replaced: bool) -> None:
    err = BackendError(protocol.ERR_LOAD_FAILED, "the model could not start")
    err.__cause__ = _cause_chain(depth)
    expected = sanitize.MESSAGES[protocol.ERR_LOAD_FAILED] if replaced else err.code_message
    assert sanitize.edge_error(err).message == expected


class _Group(Exception):
    """Carries member exceptions as ``exceptions``, as an exception group does."""

    def __init__(self, members: Any) -> None:
        super().__init__("several failures")
        self.exceptions = members


def test_upstream_text_is_found_in_an_exception_groups_members() -> None:
    group = _Group((ValueError("first"), RuntimeError(f"voice {CANARY} unknown")))
    err = _chained(
        BackendError(protocol.ERR_UNSUPPORTED, f"refused: voice {CANARY} unknown"), group
    )
    assert sanitize.edge_error(err).message == sanitize.MESSAGES[protocol.ERR_UNSUPPORTED]


@pytest.mark.parametrize("members", [{"first": 1}, 3])
def test_an_exceptions_attribute_that_holds_no_exceptions_is_ignored(members: Any) -> None:
    err = _chained(
        BackendError(protocol.ERR_LOAD_FAILED, "the model could not start"), _Group(members)
    )
    assert sanitize.edge_error(err).message == "the model could not start"


def test_short_upstream_texts_do_not_replace_an_authored_message() -> None:
    err = _chained(BackendError(protocol.ERR_CONFIG_INVALID, "the key is invalid"), KeyError("key"))
    assert sanitize.edge_error(err).message == "the key is invalid"


def test_an_unknown_backend_code_becomes_internal() -> None:
    edge = sanitize.edge_error(BackendError("no_such_code", CANARY))
    assert edge == sanitize.EdgeError(
        protocol.ERR_INTERNAL, sanitize.MESSAGES[protocol.ERR_INTERNAL]
    )


@pytest.mark.parametrize(
    ("exc", "code"),
    [
        (RuntimeError(CANARY), protocol.ERR_INTERNAL),
        (ValueError(CANARY), protocol.ERR_INTERNAL),
        (MemoryError(CANARY), protocol.ERR_OOM_ERROR),
        (OutOfMemoryError(CANARY), protocol.ERR_OOM_ERROR),
        (type("CudaOutOfMemory", (OutOfMemoryError,), {})(CANARY), protocol.ERR_OOM_ERROR),
    ],
)
def test_an_upstream_exception_carries_a_fixed_message(exc: BaseException, code: str) -> None:
    assert sanitize.edge_error(exc) == sanitize.EdgeError(code, sanitize.MESSAGES[code])


def test_code_for_keeps_the_callers_default_for_other_classes() -> None:
    assert sanitize.code_for(RuntimeError(), protocol.ERR_LOAD_FAILED) == protocol.ERR_LOAD_FAILED
    assert sanitize.code_for(OutOfMemoryError(), protocol.ERR_LOAD_FAILED) == (
        protocol.ERR_OOM_ERROR
    )


def test_describe_names_the_class_and_frame_but_not_the_text() -> None:
    def fail_deep() -> None:
        raise RuntimeError(CANARY)

    try:
        fail_deep()
    except RuntimeError as exc:
        described = sanitize.describe(exc)
    assert described.startswith("RuntimeError at test_sanitize.py:")
    assert described.endswith(" in fail_deep")
    assert CANARY not in described
    assert sanitize.describe(OutOfMemoryError(CANARY)) == "test_sanitize.OutOfMemoryError"
    assert sanitize.describe(type(f"{CANARY} class", (Exception,), {})()) == "<unnamed>"


# ----------------------------------------------------------------------
# logging
# ----------------------------------------------------------------------


@contextlib.contextmanager
def _edge_log(level: int = logging.DEBUG) -> Iterator[io.StringIO]:
    stream = io.StringIO()
    handler = logging.StreamHandler(stream)
    handler.addFilter(sanitize.EdgeLogFilter())
    handler.setFormatter(logging.Formatter("%(levelname)s:%(name)s:%(message)s"))
    root = logging.getLogger()
    previous = root.level
    root.addHandler(handler)
    root.setLevel(level)
    try:
        yield stream
    finally:
        root.removeHandler(handler)
        root.setLevel(previous)


def test_another_librarys_record_is_withheld() -> None:
    with _edge_log() as log:
        try:
            raise RuntimeError(CANARY)
        except RuntimeError:
            logging.getLogger("some.dependency").warning("synthesising %s", CANARY, exc_info=True)
    assert log.getvalue() == (
        "WARNING:some.dependency:withheld a WARNING record from some.dependency\n"
    )


def test_the_sidecars_records_keep_messages_but_not_exception_text() -> None:
    own = logging.getLogger("tensorplate.sidecar")
    with _edge_log() as log:
        try:
            raise RuntimeError(CANARY)
        except RuntimeError as exc:
            own.error("load failed: %s", exc, exc_info=True)
            own.error(exc)
            own.error("load failed: %(why)s", {"why": exc})
            own.error("stack", stack_info=True)
    assert log.getvalue().splitlines() == [
        "ERROR:tensorplate.sidecar:load failed: RuntimeError",
        "ERROR:tensorplate.sidecar:RuntimeError",
        "ERROR:tensorplate.sidecar:load failed: RuntimeError",
        "ERROR:tensorplate.sidecar:stack",
    ]


# ----------------------------------------------------------------------
# the runner's surfaces: frame bytes, health, log
# ----------------------------------------------------------------------


def _request(kind: str, **extra: Any) -> codec.SidecarFrame:
    header: dict[str, Any] = {
        "schema_version": protocol.SCHEMA_VERSION,
        "message_id": uuid.uuid4().hex,
        "kind": kind,
    }
    header.update(extra)
    return codec.SidecarFrame(header=header)


def _exchange(client: socket.socket, frame: codec.SidecarFrame) -> tuple[bytes, codec.SidecarFrame]:
    client.sendall(codec.encode(frame))
    buf = bytearray()
    while True:
        try:
            decoded, consumed = codec.decode_one(bytes(buf))
        except codec.IncompleteFrame:
            chunk = client.recv(65536)
            if not chunk:
                raise AssertionError("runner closed before responding") from None
            buf.extend(chunk)
            continue
        return bytes(buf[:consumed]), decoded


def _model_spec(artifact_path: str = "/dev/null") -> dict[str, Any]:
    return {
        "schema_version": "0.1",
        "model_id": "canary-model",
        "model_class": "custom",
        "artifact_path": artifact_path,
        "backend_hint": "python_pytorch",
        "precision_hint": "auto",
    }


class _Failing(FixtureBackend):
    """Raises the exception a test installs, from load or from infer."""

    on_load: BaseException | None = None
    on_infer: BaseException | None = None

    def load(self, model_spec: dict[str, Any]) -> None:
        if _Failing.on_load is not None:
            raise _Failing.on_load
        super().load(model_spec)

    def infer(self, inputs: list[Any]) -> list[Any]:
        if _Failing.on_infer is not None:
            raise _Failing.on_infer
        return super().infer(inputs)


@pytest.fixture
def sidecar() -> Iterator[socket.socket]:
    _Failing.on_load = None
    _Failing.on_infer = None
    client, server = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    runner = SidecarRunner(server, backend_factories={"fixture": cast(type[Backend], _Failing)})
    thread = threading.Thread(target=runner.serve_forever, daemon=True)
    thread.start()
    yield client
    with contextlib.suppress(OSError):
        client.shutdown(socket.SHUT_RDWR)
    client.close()
    thread.join(timeout=5.0)


def _assert_edge(raw: bytes, response: codec.SidecarFrame, code: str, message: str) -> None:
    assert _CANARY_BYTES not in raw
    assert response.header["status"] == protocol.STATUS_ERROR
    assert response.header["error"] == {
        "schema_version": protocol.SCHEMA_VERSION,
        "code": code,
        "message": message,
    }


def _health_bytes(client: socket.socket) -> bytes:
    raw, response = _exchange(client, _request(protocol.KIND_HEALTH_CHECK))
    assert response.header["status"] == protocol.STATUS_OK
    return raw


def test_an_upstream_exception_in_load_reaches_no_surface(sidecar: socket.socket) -> None:
    _Failing.on_load = RuntimeError(f"cannot open /models/{CANARY}.bin")
    with _edge_log() as log:
        raw, response = _exchange(
            sidecar, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec())
        )
        health = _health_bytes(sidecar)
    internal = sanitize.MESSAGES[protocol.ERR_INTERNAL]
    _assert_edge(raw, response, protocol.ERR_INTERNAL, internal)
    assert _CANARY_BYTES not in health
    assert json.loads(health[codec.FRAME_PREFIX_BYTES :])["health"]["last_error"] == internal
    assert log.getvalue().startswith(
        "ERROR:tensorplate.sidecar:sidecar request failed: RuntimeError at test_sanitize.py:"
    )
    assert CANARY not in log.getvalue()


def test_an_out_of_memory_class_in_infer_is_oom_error(sidecar: socket.socket) -> None:
    _exchange(sidecar, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    _exchange(sidecar, _request(protocol.KIND_PRIME))
    _Failing.on_infer = OutOfMemoryError(f"CUDA out of memory while decoding {CANARY}")
    infer = _request(
        protocol.KIND_INFER,
        correlation_id="r",
        tensors=[
            {
                "name": "x",
                "tensor": {"dtype": "uint8", "shape": [1]},
                "payload_offset": 0,
                "payload_length": 1,
            }
        ],
    )
    infer.payload = b"\x00"
    raw, response = _exchange(sidecar, infer)
    _assert_edge(raw, response, protocol.ERR_OOM_ERROR, sanitize.MESSAGES[protocol.ERR_OOM_ERROR])
    assert _CANARY_BYTES not in _health_bytes(sidecar)


def test_a_backend_message_that_repeats_upstream_text_is_replaced_on_the_wire(
    sidecar: socket.socket,
) -> None:
    upstream = RuntimeError(f"voice {CANARY} unknown")
    _Failing.on_load = _chained(
        BackendError(protocol.ERR_UNSUPPORTED, f"voice rejected: {upstream}"), upstream
    )
    raw, response = _exchange(sidecar, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    _assert_edge(
        raw, response, protocol.ERR_UNSUPPORTED, sanitize.MESSAGES[protocol.ERR_UNSUPPORTED]
    )
    assert _CANARY_BYTES not in _health_bytes(sidecar)


def test_a_backend_error_keeps_its_runtime_capability(sidecar: socket.socket) -> None:
    capability = RuntimeCapability("python_pytorch", "2.9.1", "unknown", True, False, "x")
    _Failing.on_load = BackendError(
        protocol.ERR_UNSUPPORTED, "runtime not available", runtime_capability=capability
    )
    raw, response = _exchange(sidecar, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    _assert_edge(raw, response, protocol.ERR_UNSUPPORTED, "runtime not available")
    assert response.header["runtime_capability"] == capability.to_wire()


def test_a_missing_entry_named_after_a_canary_stays_out_of_the_error(
    sidecar: socket.socket, tmp_path: Path
) -> None:
    missing = tmp_path / CANARY / "entry.json"
    raw, response = _exchange(
        sidecar, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec(str(missing)))
    )
    _assert_edge(raw, response, protocol.ERR_CONFIG_INVALID, "sidecar config not found")


def test_an_unreadable_entry_named_after_a_canary_stays_out_of_the_error(
    sidecar: socket.socket, tmp_path: Path
) -> None:
    directory = tmp_path / f"{CANARY}.json"
    directory.mkdir()
    raw, response = _exchange(
        sidecar, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec(str(directory)))
    )
    _assert_edge(raw, response, protocol.ERR_CONFIG_INVALID, "sidecar config could not be read")


def test_a_canary_profile_name_stays_out_of_the_error(
    sidecar: socket.socket, tmp_path: Path
) -> None:
    entry = tmp_path / "entry.json"
    entry.write_text(json.dumps({"backend_profile": CANARY}), encoding="utf-8")
    raw, response = _exchange(
        sidecar, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec(str(entry)))
    )
    _assert_edge(
        raw,
        response,
        protocol.ERR_CONFIG_INVALID,
        "no sidecar backend is registered for the requested profile",
    )


def test_a_tensor_name_stays_out_of_the_error(sidecar: socket.socket) -> None:
    _exchange(sidecar, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    _exchange(sidecar, _request(protocol.KIND_PRIME))
    infer = _request(
        protocol.KIND_INFER,
        correlation_id="r",
        tensors=[
            {
                "name": CANARY,
                "tensor": {"dtype": "uint8", "shape": [4]},
                "payload_offset": 0,
                "payload_length": 4,
            }
        ],
    )
    raw, response = _exchange(sidecar, infer)
    _assert_edge(
        raw,
        response,
        protocol.ERR_SHAPE_MISMATCH,
        "a tensor's payload window exceeds the frame payload",
    )


# ----------------------------------------------------------------------
# the process entry point's standard streams
# ----------------------------------------------------------------------

_NOISY_SIDECAR = textwrap.dedent(
    """
    import logging, os, sys, threading, warnings
    from tensorplate_pytorch_backend import runner
    from tensorplate_pytorch_backend.backends.fixture import FixtureBackend

    CANARY = sys.argv[1]

    def fail() -> None:
        raise RuntimeError(CANARY)

    class Unraisable:
        def __del__(self) -> None:
            raise RuntimeError(CANARY)

    class Noisy(FixtureBackend):
        def load(self, model_spec):
            print(CANARY)
            print(CANARY, file=sys.stderr)
            sys.__stderr__.write(CANARY)
            sys.__stderr__.flush()
            os.write(1, CANARY.encode())
            os.write(2, CANARY.encode())
            logging.getLogger("noisy.dependency").warning("text %s", CANARY)
            warnings.warn(CANARY, stacklevel=1)
            thread = threading.Thread(target=fail)
            thread.start()
            thread.join()
            Unraisable()
            raise RuntimeError(CANARY)

    runner.default_backend_factories = lambda: {"fixture": Noisy}
    runner.main(sys.argv[2:])
    raise RuntimeError(CANARY)
    """
)


def test_the_entry_point_keeps_canaries_off_its_streams(tmp_path: Path) -> None:
    script = tmp_path / "noisy_sidecar.py"
    script.write_text(_NOISY_SIDECAR, encoding="utf-8")
    # A short directory: a socket path must fit sockaddr_un on every platform.
    socket_dir = Path(tempfile.mkdtemp(prefix="tp-", dir="/tmp"))
    socket_path = socket_dir / "s"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(socket_path))
    listener.listen(1)
    listener.settimeout(30.0)
    process = subprocess.Popen(
        [sys.executable, str(script), CANARY, "--socket", str(socket_path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        connection, _ = listener.accept()
        connection.settimeout(30.0)
        with connection:
            buf = bytearray()
            while True:
                try:
                    ready, _consumed = codec.decode_one(bytes(buf))
                    break
                except codec.IncompleteFrame:
                    buf.extend(connection.recv(65536))
            assert ready.header["kind"] == protocol.KIND_READY_EVENT
            raw, response = _exchange(
                connection, _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec())
            )
            health = _health_bytes(connection)
        stdout, stderr = process.communicate(timeout=30.0)
    finally:
        listener.close()
        shutil.rmtree(socket_dir, ignore_errors=True)
        if process.poll() is None:
            process.kill()
            process.communicate()
    internal = sanitize.MESSAGES[protocol.ERR_INTERNAL]
    _assert_edge(raw, response, protocol.ERR_INTERNAL, internal)
    assert _CANARY_BYTES not in health
    assert _CANARY_BYTES not in stdout
    assert _CANARY_BYTES not in stderr
    assert process.returncode == 1
    log = stderr.decode()
    # Each channel above was written to, and each was withheld.
    for notice in (
        "withheld text written to stdout",
        "withheld text written to stderr",
        "withheld a WARNING record from noisy.dependency",
        "withheld a WARNING record from py.warnings",
        "uncaught exception in a thread: RuntimeError at noisy_sidecar.py:",
        "unraisable exception: RuntimeError at noisy_sidecar.py:",
        "sidecar request failed: RuntimeError at noisy_sidecar.py:",
        "uncaught exception: RuntimeError at noisy_sidecar.py:",
    ):
        assert notice in log, log
