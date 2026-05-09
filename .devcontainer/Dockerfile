# TensorPlate development container.
#
# Pinned to ubuntu:22.04 to match the GitHub-hosted runner image used by
# .github/workflows/cpp.yml and .github/workflows/rust.yml. Keep the
# package set in sync with that workflow so local repro and CI agree.
#
# Jetson cross-compile toolchains and SDKs (TensorRT, CUDA) are NOT
# installed here; they require JetPack-provided artifacts and are documented
# separately in docs/contributing/jetson-cross-compile.md.

FROM ubuntu:22.04

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_TOOLCHAIN=1.78.0

ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    PATH=/usr/local/cargo/bin:/usr/local/rustup/bin:/opt/vcpkg:${PATH} \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    VCPKG_ROOT=/opt/vcpkg \
    VCPKG_DEFAULT_BINARY_CACHE=/var/cache/vcpkg \
    CC=clang-15 \
    CXX=clang++-15

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang-15 \
        clang++-15 \
        clang-format-15 \
        clang-tidy-15 \
        cmake \
        curl \
        git \
        gnupg \
        lld-15 \
        ninja-build \
        pkg-config \
        python3 \
        python3-pip \
        sudo \
        tar \
        unzip \
        wget \
        xz-utils \
        zip \
    ; \
    rm -rf /var/lib/apt/lists/*; \
    update-alternatives --install /usr/bin/clang        clang        /usr/bin/clang-15        100; \
    update-alternatives --install /usr/bin/clang++      clang++      /usr/bin/clang++-15      100; \
    update-alternatives --install /usr/bin/clang-format clang-format /usr/bin/clang-format-15 100; \
    update-alternatives --install /usr/bin/clang-tidy   clang-tidy   /usr/bin/clang-tidy-15   100

# Install rustup and the pinned project toolchain. rust-toolchain.toml will
# pin per-checkout if the version drifts; this layer just primes the cache.
RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "${RUST_TOOLCHAIN}"; \
    rustup component add rustfmt clippy; \
    chmod -R a+rwX "${RUSTUP_HOME}" "${CARGO_HOME}"

# Bootstrap vcpkg in manifest mode. CI uses the runner-provided vcpkg, but
# the dev container ships its own so contributors can build offline once
# the binary cache is warm.
RUN set -eux; \
    git clone --depth 1 https://github.com/microsoft/vcpkg "${VCPKG_ROOT}"; \
    "${VCPKG_ROOT}/bootstrap-vcpkg.sh" -disableMetrics; \
    mkdir -p "${VCPKG_DEFAULT_BINARY_CACHE}"; \
    chmod -R a+rwX "${VCPKG_ROOT}" "${VCPKG_DEFAULT_BINARY_CACHE}"

# Non-root user for VS Code / GitHub Codespaces compatibility.
ARG USERNAME=tp
ARG USER_UID=1000
ARG USER_GID=${USER_UID}
RUN set -eux; \
    groupadd --gid "${USER_GID}" "${USERNAME}"; \
    useradd --uid "${USER_UID}" --gid "${USER_GID}" -m -s /bin/bash "${USERNAME}"; \
    echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/${USERNAME}; \
    chmod 0440 /etc/sudoers.d/${USERNAME}

USER ${USERNAME}
WORKDIR /workspaces/tensorplate

CMD ["/bin/bash"]
