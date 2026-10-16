#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# NVIDIA's Ubuntu 22.04 amd64 apt packages for the device-free CI build.
# Source: https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64/
# The x86 SDK follows the managed speech runtime CUDA version.
NVIDIA_APT_URL=https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64
NVIDIA_KEYRING_VERSION=1.1-1
NVIDIA_KEYRING_SHA256=d93190d50b98ad4699ff40f4f7af50f16a76dac3bb8da1eaaf366d47898ff8df
TENSORRT_VERSION=10.16.1.11-1+cuda12.9
CUDA_VERSION=12.9.79-1
CUDA_CRT_VERSION=12.9.86-1
CUDA_CCCL_VERSION=12.9.27-1
CUDAToolkit_ROOT=/usr/local/cuda-12.9

# Pin the NVIDIA dependency closure, including headers and toolkit config.
# No driver, samples, Python bindings, plugins or full-toolkit metapackage.
TENSORRT_SDK_PACKAGES=(
  "libnvinfer10=$TENSORRT_VERSION"
  "libnvinfer-headers-dev=$TENSORRT_VERSION"
  "cuda-cudart-12-9=$CUDA_VERSION"
  "cuda-cudart-dev-12-9=$CUDA_VERSION"
  "cuda-driver-dev-12-9=$CUDA_VERSION"
  "cuda-crt-12-9=$CUDA_CRT_VERSION"
  "cuda-toolkit-config-common=$CUDA_VERSION"
  "cuda-toolkit-12-config-common=$CUDA_VERSION"
  "cuda-toolkit-12-9-config-common=$CUDA_VERSION"
  "cuda-cccl-12-9=$CUDA_CCCL_VERSION"
)
