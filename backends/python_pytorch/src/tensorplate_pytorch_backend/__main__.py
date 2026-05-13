"""Process entry point for ``python -m tensorplate_pytorch_backend``."""

from __future__ import annotations

import sys

from tensorplate_pytorch_backend.runner import main

if __name__ == "__main__":
    sys.exit(main())
