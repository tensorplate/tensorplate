#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Records what a real Python 3.12 answers to the queries `tensorplate doctor`
# sends a runner profile's interpreter, in each state the cases beside this
# script name. The interpreter is a virtual environment at the speech
# runtime's path, created as the runtime's base package creates it; the
# `ct2` cases install the locked `faster_whisper` closure without its
# cuBLAS package. No case has a GPU: every recording here is of a host on
# which the profiles cannot serve.
#
# The two `ct2_*_library` cases record which file the loader maps for the
# soname, not cuBLAS: the library under that name is an empty one built here.
#
# Writes into the system, so run it as root in a throwaway container, from
# the repository root, on an image with Python 3.12, `venv`, `gcc` and
# `libespeak-ng1`:
#
#   docker run --rm -v "$PWD:/src:ro" \
#     -v "$PWD/cli/tests/fixtures/doctor_runner_profiles/recordings:/rec" \
#     <ubuntu 24.04 image> /src/cli/tests/fixtures/doctor_runner_profiles/record.sh

# The two files under `mountinfo/` are the `/tmp` line of
# /proc/self/mountinfo in a container started with `--tmpfs /tmp:rw,exec`
# and with `--tmpfs /tmp:rw,noexec`, after a root line cut down to its
# mount point and options.

set -Eeuo pipefail

src=/src
out=/rec
root=/usr/lib/tensorplate/speech-runtime
python="$root/bin/python"
site="$root/lib/python3.12/site-packages"
cublas="$site/nvidia/cublas/lib"
interpreter_query="$(cat "$src/protocol/rust/src/backend_probe_interpreter.py")"
dependency_query="$(cat "$src/cli/src/commands/doctor/runner_profile_dependencies.py")"
# The lists `requirements` holds in cli/src/commands/doctor/runner_profiles.rs;
# a test there fails when these two lines differ from them.
faster_whisper='{"libraries":["libcublas.so.12"],"modules":["ctranslate2","faster_whisper","av"]}'
kokoro='{"libraries":[],"modules":["torch","kokoro","misaki","en_core_web_sm","espeakng_loader"]}'

# One query in the environment the sidecar launcher sets.
ask() {
  local name="$1" search="$2" rc=0
  shift 2
  LD_LIBRARY_PATH="$search" ORT_DISABLE_TELEMETRY=1 TMPDIR=/tmp \
    "$@" >"$name.stdout" 2>"$name.stderr" || rc=$?
  printf '%s\n' "$rc" >"$name.status"
}

record() {
  local dir="$out/$1"
  rm -rf "$dir"
  mkdir -p "$dir"
  ask "$dir/interpreter" "$cublas" "$python" -c "$interpreter_query"
  ask "$dir/import" "$cublas" "$python" -c 'import tensorplate_pytorch_backend'
  ask "$dir/dependencies.faster_whisper" "$cublas" "$python" -c "$dependency_query" "$faster_whisper"
  ask "$dir/dependencies.kokoro" "" "$python" -c "$dependency_query" "$kokoro"
}

# The backend package's own interpreter: the module on the system path, no PyTorch.
mkdir -p /usr/lib/tensorplate/backends/python_pytorch
cp -R "$src/backends/python_pytorch/src" /usr/lib/tensorplate/backends/python_pytorch/src
cp "$src/packaging/python/tensorplate_pytorch_backend.pth" /usr/lib/python3/dist-packages/
rm -rf "$out/own"
mkdir -p "$out/own"
ask "$out/own/version" "" /usr/bin/python3 -c \
  'import sys;print(f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}")'
ask "$out/own/import" "" /usr/bin/python3 -c 'import tensorplate_pytorch_backend'
ask "$out/own/torch" "" /usr/bin/python3 -c "import torch; print(getattr(torch, '__version__', 'unknown'))"

python3 -m venv --without-pip "$root"
cp -R "$src/backends/python_pytorch/src/tensorplate_pytorch_backend" "$site/"
cp -R "$src/packaging/speech-runtime/espeakng-loader/espeakng_loader" "$site/"
record environment

mv "$root/pyvenv.cfg" /tmp/pyvenv.cfg
record environment_without_marker
mv /tmp/pyvenv.cfg "$root/pyvenv.cfg"

mv /usr/lib/x86_64-linux-gnu/libespeak-ng.so.1 /tmp/libespeak-ng.so.1
record environment_without_espeak
mv /tmp/libespeak-ng.so.1 /usr/lib/x86_64-linux-gnu/libespeak-ng.so.1

python3 -m venv /opt/installer
lock="$src/packaging/speech-runtime/lock"
/opt/installer/bin/pip install --quiet --disable-pip-version-check --no-deps --require-hashes \
  --target "$site" -r "$lock/base.txt" -r "$lock/ct2.txt" -r "$lock/vad.txt"
record ct2

# As on an image that carries its own CUDA toolkit: known to the loader's cache.
stand_in() {
  printf 'int tp_stand_in(void) { return 0; }\n' |
    gcc -shared -fPIC -Wl,-soname,libcublas.so.12 -o "$1" -x c -
}
stand_in /usr/local/lib/libcublas.so.12.0.0
ldconfig
record ct2_system_library

mkdir -p "$cublas"
stand_in "$cublas/libcublas.so.12"
record ct2_profile_library

# An absent file replays as empty output.
find "$out" -type f -empty -delete
chmod -R a+rX "$out"
