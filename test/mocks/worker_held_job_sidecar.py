# SPDX-License-Identifier: Apache-2.0
"""Real sidecar runner whose fixture backend never ends a job, for stall tests."""

import os
import sys
import threading
from pathlib import Path

from tensorplate_pytorch_backend import runner
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend
from tensorplate_pytorch_backend.job_objects import JobRequest, JobResult


class HeldJobBackend(FixtureBackend):
    """Tell the parent a job started, then keep it until the process ends."""

    def run_job(self, request: JobRequest) -> JobResult:
        (Path(os.environ["TP_HELD_JOB_TEST_DIR"]) / "job").touch()
        threading.Event().wait()
        raise AssertionError("a held job never returns")


runner.default_backend_factories = lambda: {"fixture": HeldJobBackend}
# The adapter invokes an interpreter with `-m tensorplate_pytorch_backend`.
sys.exit(runner.main(sys.argv[3:]))
