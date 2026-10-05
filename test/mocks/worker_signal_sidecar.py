# SPDX-License-Identifier: Apache-2.0
"""Real sidecar runner with file-gated fixture load/unload for signal tests."""

import os
import sys
import time
from pathlib import Path
from typing import Any

from tensorplate_pytorch_backend import runner
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend


def gate(stage: str) -> None:
    """Tell the parent the lifecycle call began, then await its release."""
    directory = Path(os.environ["TP_SIGNAL_TEST_DIR"])
    (directory / stage).touch()
    deadline = time.monotonic() + 10
    while not (directory / (stage + "-release")).exists():
        if time.monotonic() >= deadline:
            raise RuntimeError("signal test gate timed out")
        time.sleep(0.005)


class GatedBackend(FixtureBackend):
    """Keep a real lifecycle operation in progress until the test signals it."""

    def load(self, model_spec: dict[str, Any]) -> None:
        gate("load")
        super().load(model_spec)

    def unload(self) -> None:
        gate("unload")
        super().unload()


runner.default_backend_factories = lambda: {"fixture": GatedBackend}
# The adapter invokes an interpreter with `-m tensorplate_pytorch_backend`.
sys.exit(runner.main(sys.argv[3:]))
