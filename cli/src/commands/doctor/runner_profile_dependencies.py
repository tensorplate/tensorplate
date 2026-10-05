# tensorplate doctor: runner profile dependencies
# SPDX-License-Identifier: Apache-2.0
#
# Run by `tensorplate doctor` in a runner profile's interpreter, with the
# environment the sidecar launcher sets. Imports the modules and loads the
# libraries named in argv[1] and prints one JSON object of what it found.
import ctypes
import importlib
import json
import os
import sys

spec = json.loads(sys.argv[1])
facts = {"modules": {}, "libraries": {}}
loaded = {}


def describe(exc: Exception) -> str:
    return f"{type(exc).__name__}: {exc}"


for name in spec["modules"]:
    try:
        loaded[name] = importlib.import_module(name)
        version = getattr(loaded[name], "__version__", "unknown")
        facts["modules"][name] = {"version": str(version)}
    except Exception as exc:
        facts["modules"][name] = {"error": describe(exc)}


def mapped(soname: str) -> list[str]:
    with open("/proc/self/maps") as maps:
        fields = (line.split(None, 5) for line in maps)
        paths = {entry[5].strip() for entry in fields if len(entry) == 6}
    return sorted(p for p in paths if os.path.basename(p).startswith(soname))


# By soname, after the imports: how an engine loads the library at run time.
for soname in spec["libraries"]:
    try:
        ctypes.CDLL(soname)
        facts["libraries"][soname] = {"mapped": mapped(soname)}
    except OSError as exc:
        facts["libraries"][soname] = {"error": describe(exc)}

if "ctranslate2" in loaded:
    try:
        devices = loaded["ctranslate2"].get_cuda_device_count()
        types = loaded["ctranslate2"].get_supported_compute_types("cuda") if devices else []
        facts["ctranslate2"] = {"cuda_devices": devices, "compute_types": sorted(types)}
    except Exception as exc:
        facts["ctranslate2"] = {"error": describe(exc)}

if "torch" in loaded:
    try:
        facts["torch"] = {"cuda_devices": loaded["torch"].cuda.device_count()}
    except Exception as exc:
        facts["torch"] = {"error": describe(exc)}

if "espeakng_loader" in loaded:
    try:
        path = loaded["espeakng_loader"].get_library_path()
        library = ctypes.CDLL(path)
        library.espeak_Info.restype = ctypes.c_char_p
        version = library.espeak_Info(None).decode()
        facts["espeak_ng"] = {"library": path, "version": version}
    except Exception as exc:
        facts["espeak_ng"] = {"error": describe(exc)}

print(json.dumps(facts, sort_keys=True))
