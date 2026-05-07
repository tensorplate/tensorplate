#!/usr/bin/env bash
# Devcontainer post-create hook.
#
# Runs once after the container is created. Keep this idempotent and fast;
# longer setup belongs in the Dockerfile so it is cached across rebuilds.

set -euo pipefail

cd "$(dirname "$0")/.."

# Ensure the pinned Rust toolchain is materialized for the workspace.
# rust-toolchain.toml drives the version; rustup will install components
# as needed.
rustup show active-toolchain || rustup toolchain install
rustup component add rustfmt clippy

# Print a concise environment summary so contributors can verify the image
# matches what CI runs.
echo "---- TensorPlate dev container ready ----"
echo "clang   : $(clang --version | head -1)"
echo "cmake   : $(cmake --version | head -1)"
echo "ninja   : $(ninja --version)"
echo "rustc   : $(rustc --version)"
echo "cargo   : $(cargo --version)"
echo "vcpkg   : $(vcpkg version | head -1 || echo 'vcpkg not on PATH')"
echo "---- Local validation: docs/contributing/local-validation.md ----"
