"""Opt-in: stream sessions through a real agent and a serving worker built with streaming.

Set ``TENSORPLATE_AGENT_BIN``, ``TENSORPLATE_CLI_BIN`` and
``TENSORPLATE_STREAMING_WORKER_BIN`` to the ``tensorplate-agent``, ``tensorplate``
and ``tensorplate-serving`` binaries (the worker configured with
``TP_ENABLE_STREAMING_GRPC=ON``) to run these cases; they are skipped
otherwise. Each starts an agent in process mode with a stream port range, which
deploys with ``--streaming`` a speech bundle the sidecar serves with its fixture
backend: no GPU and no model. The agent's platform gate reads
``config/platform``, or ``TENSORPLATE_PLATFORM_REGISTRY_DIR`` on a host none of
its rows describes, such as one with a GPU of no row. The sidecar runs on this
interpreter.
"""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import json
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
from collections.abc import Awaitable, Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

from tensorplate.errors import MissingDependencyError

try:
    import tensorplate.speech as speech
except MissingDependencyError as missing:
    if os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1":
        raise
    pytest.skip(str(missing), allow_module_level=True)

from stream_server import REPOSITORY

_BINARIES = {
    "agent": "TENSORPLATE_AGENT_BIN",
    "cli": "TENSORPLATE_CLI_BIN",
    "worker": "TENSORPLATE_STREAMING_WORKER_BIN",
}
PCM = speech.AudioFormat("pcm_s16le", 16000)
FRAME = bytes(3200)  # 100 ms
# The fixture backend transcribes any audio as this.
FIXTURE_TEXT = "hello"
# An agent with this many stream port pairs can start a generation while the last one stops.
PAIRS = 3
ASCII_WHITESPACE = (" ", " \t\n", "\v\f\r")
OTHER_WHITESPACE = ("\u00a0", "\u2003", "\u3000", "\x1c", "\x85")


def _free_ports(count: int) -> int:
    """The first of `count` consecutive loopback ports free now, below the ephemeral range."""
    for _ in range(200):
        first = random.randrange(20000, 30000 - count)
        held: list[socket.socket] = []
        try:
            for port in range(first, first + count):
                sock = socket.socket()
                held.append(sock)
                sock.bind(("127.0.0.1", port))
            return first
        except OSError:
            continue
        finally:
            for sock in held:
                sock.close()
    pytest.fail(f"no {count} consecutive free loopback ports")


class Agent:
    """A ``tensorplate-agent`` started in a directory of its own, and the CLI that reaches it."""

    def __init__(self, binaries: dict[str, str], root: Path) -> None:
        self.binaries, self.root = binaries, root
        self.bundle = self._bundle()
        self.cli_config = root / "cli.json"
        self.socket = root / "agent.sock"
        self.env = self._environment()
        self.process: subprocess.Popen[bytes] | None = None

    def _bundle(self) -> Path:
        bundle = self.root / "bundle"
        bundle.mkdir()
        entry = b"{}"
        (bundle / "entry.json").write_bytes(entry)
        manifest = {
            "schema_version": "0.1",
            "name": "synthetic-speech-fixture",
            "version": "0.1.0",
            "format_version": "0.1",
            "model_class": "speech",
            "backend_hint": "python_pytorch",
            "artifacts": [
                {
                    "role": "model",
                    "kind": "python_pytorch_entry",
                    "path": "entry.json",
                    "digest": "sha256:" + hashlib.sha256(entry).hexdigest(),
                    "byte_size": len(entry),
                }
            ],
        }
        (bundle / "manifest.json").write_text(json.dumps(manifest))
        return bundle

    def _environment(self) -> dict[str, str]:
        root, first = self.root, _free_ports(2 * PAIRS)
        config = json.loads((REPOSITORY / "packaging/conf/agent.amd64.json").read_text())
        config.update(
            socket_path=str(self.socket),
            state_dir=str(root / "state"),
            staging_dir=str(root / "staging"),
        )
        config["worker"].update(
            serving_binary_path=os.path.abspath(self.binaries["worker"]),
            serving_config_dir=str(root / "worker-configs"),
            serving_bind_port=first,
            serving_candidate_bind_port=first + 1,
            serving_port_range={"first": first, "last": first + 2 * PAIRS - 1},
        )
        self.agent_config = root / "agent.json"
        self.agent_config.write_text(json.dumps(config))
        profile = {"mode": "local", "socket_path": str(self.socket)}
        self.cli_config.write_text(
            json.dumps({"schema_version": "0.1", "profiles": {"local": profile}})
        )
        # The backend as its package installs it, on this interpreter and without PyTorch,
        # which the fixture backend does not import.
        descriptor = json.loads(
            (REPOSITORY / "packaging/backend-metadata/python_pytorch.json").read_text()
        )
        descriptor["python"]["interpreter"] = sys.executable
        descriptor["pytorch"]["required"] = False
        backends = root / "backends/python_pytorch"
        backends.mkdir(parents=True)
        (backends / "backend.json").write_text(json.dumps(descriptor))
        # The platform gate admits the backend only as an installed package.
        tools = root / "bin"
        tools.mkdir()
        query = tools / "dpkg-query"
        query.write_text(
            "#!/bin/sh\n"
            '[ -x /usr/bin/dpkg-query ] && /usr/bin/dpkg-query "$@"\n'
            "echo 'tensorplate-backend-python-pytorch installed'\n"
        )
        query.chmod(0o755)
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("TP_", "TENSORPLATE_")) and key != "PYTHONPATH"
        }
        env.update(
            PATH=f"{tools}{os.pathsep}{env.get('PATH', '')}",
            PYTHONPATH=str(REPOSITORY / "backends/python_pytorch/src"),
            TP_PYTHON_PYTORCH_EXECUTABLE=sys.executable,
            TP_BACKEND_DESCRIPTOR_DIR=str(root / "backends"),
            TP_PLATFORM_REGISTRY_DIR=os.environ.get(
                "TENSORPLATE_PLATFORM_REGISTRY_DIR", str(REPOSITORY / "config/platform")
            ),
        )
        return env

    def start(self) -> None:
        self.stderr = self.root / "agent.stderr"
        with open(self.stderr, "wb") as stderr:
            self.process = subprocess.Popen(
                [self.binaries["agent"], "--config", str(self.agent_config)],
                env=self.env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=stderr,
                start_new_session=True,
            )
        # The socket is the agent's readiness: it is bound after recovery and before serving.
        for _ in range(600):
            if self.socket.exists() or self.process.poll() is not None:
                break
            with contextlib.suppress(subprocess.TimeoutExpired):
                self.process.wait(0.05)
        assert self.socket.exists(), f"the agent did not listen:\n{self.stderr.read_text()}"

    def cli(self, *arguments: str) -> dict[str, Any]:
        command = [self.binaries["cli"], "--config", str(self.cli_config), "--output", "json"]
        done = subprocess.run(
            [*command, *arguments], env=self.env, capture_output=True, text=True, timeout=120
        )
        assert done.returncode == 0, f"tensorplate {arguments}: {done.stdout}{done.stderr}"
        answer: dict[str, Any] = json.loads(done.stdout)
        return answer

    def deploy(self) -> dict[str, Any]:
        self.cli("deploy", str(self.bundle), "--deployment-id", "speech", "--streaming")
        return self.active()

    def active(self) -> dict[str, Any]:
        active: dict[str, Any] = self.cli("status")["payload"]["agent"]["active"]
        return active

    def stop(self) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        # The agent stops its workers, and they their sidecars.
        self.process.send_signal(signal.SIGTERM)
        try:
            self.process.wait(30)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait()


@pytest.fixture
def agent() -> Iterator[Agent]:
    binaries = {name: os.environ.get(variable, "") for name, variable in _BINARIES.items()}
    if not all(binaries.values()):
        pytest.skip("set " + ", ".join(_BINARIES.values()) + " to run the real-agent stream cases")
    # Short, as socket paths are bounded.
    root = Path(tempfile.mkdtemp(prefix="tp-e2e", dir="/tmp"))
    started = Agent(binaries, root)
    try:
        started.start()
        yield started
    finally:
        started.stop()
        shutil.rmtree(root, ignore_errors=True)


def run(main: Callable[[], Awaitable[Any]]) -> Any:
    return asyncio.run(asyncio.wait_for(main(), 120))


def generation(session: Any) -> tuple[str, int]:
    """The endpoint and generation the session opened on."""
    return session._resolved.endpoint, session._resolved.target.generation


def at(active: dict[str, Any]) -> tuple[str, int]:
    return active["stream_endpoint"], active["generation"]


async def transcribe(stream: speech.SpeechClient) -> tuple[str, int]:
    async with stream.transcribe(language="en", input_format=PCM) as session:
        for _ in range(5):
            await session.send_audio(FRAME)
        final = await session.finalize(timeout=30)
        totals = await session.close()
    assert [segment.text for segment in final.segments] == [FIXTURE_TEXT]
    assert totals.input_audio_samples == 5 * len(FRAME) // 2
    return generation(session)


async def synthesize(stream: speech.SpeechClient, text: str = "synthetic text") -> int:
    """Synthesize `text` and return the samples of audio read."""
    async with stream.synthesize(language="en", voice="synthetic_voice") as session:
        audio = asyncio.create_task(read_audio(session))
        await session.send_text(text)
        done = await session.finalize(timeout=30)
        totals = await session.close()
    samples = await audio
    assert (done.segment_count, done.total_samples) == (1, samples)
    assert totals.output_audio_samples == samples > 0
    return samples


async def read_audio(session: Any) -> int:
    samples = 0
    async for event in session:
        if isinstance(event, speech.AudioChunk):
            samples += event.sample_count
    return samples


def test_a_client_from_the_agent_transcribes_and_synthesizes_on_the_deployment(
    agent: Agent,
) -> None:
    active = agent.deploy()

    async def main() -> None:
        async with await speech.SpeechClient.from_agent(
            config_path=str(agent.cli_config)
        ) as stream:
            assert await transcribe(stream) == at(active)
            await synthesize(stream)

    run(main)


# How long the client leaves the first generation's port, which its channel met closed, before
# the third generation takes it: past gRPC's fifth reconnect attempt and short of its sixth.
IDLE_GAP_S = 20.0


def test_a_client_follows_each_redeploy_to_its_generation_on_whatever_port_it_takes(
    agent: Agent,
) -> None:
    first = agent.deploy()

    async def main() -> None:
        async with await speech.SpeechClient.from_agent(
            config_path=str(agent.cli_config)
        ) as stream:
            assert await transcribe(stream) == at(first)
            second = await asyncio.to_thread(agent.deploy)
            assert at(second) == (second["stream_endpoint"], first["generation"] + 1)
            assert second["stream_endpoint"] != first["stream_endpoint"]
            # The open's first call is refused on the stopped worker's port; it follows the agent.
            assert await transcribe(stream) == at(second)
            await asyncio.sleep(IDLE_GAP_S)
            held = stream.transcribe(language="en", input_format=PCM)
            await held.open()
            third = await asyncio.to_thread(agent.deploy)
            # The agent starts a generation on the lowest free pair: the first one's.
            assert at(third) == (first["stream_endpoint"], first["generation"] + 2)
            # A session open on the stopped worker ends with it, and stays where it was.
            with pytest.raises(speech.ServiceUnavailableError) as caught:
                await held.close()
            assert (caught.value.reason, caught.value.admitted) == ("worker_shutdown", True)
            assert generation(held) == at(second)
            assert await transcribe(stream) == at(third)
            await synthesize(stream)

    run(main)


def test_the_worker_refuses_only_the_whitespace_the_client_refuses_before_sending(
    agent: Agent,
) -> None:
    agent.deploy()

    async def main() -> None:
        async with await speech.SpeechClient.from_agent(
            config_path=str(agent.cli_config)
        ) as stream:
            for text in ASCII_WHITESPACE:
                session = stream.synthesize(language="en", voice="synthetic_voice")
                await session.open()
                with pytest.raises(ValueError, match="not only ASCII whitespace"):
                    await session.send_text(text)
                # Past the client's check, the worker refuses it as empty.
                await session._within(session._send_segment(text, "", len(text.encode())), None)
                with pytest.raises(speech.InvalidEventError) as caught:
                    await read_audio(session)
                assert caught.value.reason == "empty_input"
            for text in OTHER_WHITESPACE:
                assert await synthesize(stream, text) > 0

    run(main)


def test_a_session_ended_with_a_cause_has_totals_of_audio_it_never_read(agent: Agent) -> None:
    agent.deploy()

    async def main() -> None:
        async with await speech.SpeechClient.from_agent(
            config_path=str(agent.cli_config)
        ) as stream:
            session = stream.synthesize(language="en", voice="synthetic_voice")
            await session.open()
            await session.send_text("synthetic text " * 8)
            chunk = await session.__anext__()
            assert isinstance(chunk, speech.AudioChunk)
            await session.cancel()
            assert [event async for event in session] == []
            # The worker counts audio the client did not read, which is why the client
            # compares totals with what it read only for a session closed without a cause.
            closed = session._closed
            assert closed is not None
            assert closed.cause.reason == speech_reason("client_cancelled")
            assert closed.totals.output_audio_samples > chunk.sample_count

    run(main)


def speech_reason(name: str) -> int:
    from tensorplate.stream.v1 import session_pb2 as pb

    return int(pb.FailureReason.Value(f"FAILURE_REASON_{name.upper()}"))
