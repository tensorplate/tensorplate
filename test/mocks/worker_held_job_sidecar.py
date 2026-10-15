# SPDX-License-Identifier: Apache-2.0
"""Real sidecar runner whose fixture backend holds a job, for stall and drain tests."""

import os
import sys
import threading
import time
from pathlib import Path

from tensorplate_pytorch_backend import runner
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend
from tensorplate_pytorch_backend.job_objects import JobRequest, JobResult


class HeldJobBackend(FixtureBackend):
    """Tell the parent which process holds a started job, then keep it.

    With TP_HELD_JOB_RELEASE_GATE set, the job ends as the fixture's once the
    parent creates `release`; without, it is kept until the process ends.
    """

    def run_job(self, request: JobRequest) -> JobResult:
        directory = Path(os.environ["TP_HELD_JOB_TEST_DIR"])
        job = directory / "job"
        # Renamed into place: the test reads the id as soon as the file exists.
        job.with_suffix(".writing").write_text(str(os.getpid()))
        job.with_suffix(".writing").rename(job)
        if os.environ.get("TP_HELD_JOB_RELEASE_GATE"):
            while not (directory / "release").exists():
                time.sleep(0.005)
            return super().run_job(request)
        threading.Event().wait()
        raise AssertionError("a held job never returns")


runner.default_backend_factories = lambda: {"fixture": HeldJobBackend}
# The adapter invokes an interpreter with `-m tensorplate_pytorch_backend`.
sys.exit(runner.main(sys.argv[3:]))
