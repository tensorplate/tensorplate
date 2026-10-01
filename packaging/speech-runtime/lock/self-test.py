# SPDX-License-Identifier: Apache-2.0
"""Run inside the built environment: what both runner profiles need is here."""

import importlib.metadata
import importlib.util
import sys
import types
from pathlib import Path

assert importlib.util.find_spec("pip") is None, "the environment must not contain pip"

site = Path(sys.prefix) / "lib" / f"python3.{sys.version_info.minor}" / "site-packages"
# CTranslate2 loads cuBLAS by name from the runner's library search path.
assert (site / "nvidia" / "cublas" / "lib" / "libcublas.so.12").is_file()
import ctranslate2  # noqa: E402
import faster_whisper  # noqa: E402, F401

assert ctranslate2.__version__ == importlib.metadata.version("ctranslate2")

# misaki fetches this spaCy model at first use when it is not installed.
importlib.metadata.version("en_core_web_sm")
import espeakng_loader  # noqa: E402

assert importlib.metadata.version("espeakng-loader").endswith("+tensorplate.1")
assert Path(espeakng_loader.get_library_path()).is_file()
assert Path(espeakng_loader.get_data_path()).is_dir()
from misaki import espeak  # noqa: E402

phonemes, _ = espeak.EspeakFallback(british=False)(types.SimpleNamespace(text="tensorplate"))
assert phonemes, "the espeak fallback produced no phonemes"

import kokoro  # noqa: E402, F401
import torch  # noqa: E402

assert torch.version.cuda.startswith("12."), torch.version.cuda
print("speech runtime self-test: ok")
