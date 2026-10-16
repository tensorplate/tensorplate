#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# NVIDIA's Ubuntu 22.04 amd64 apt packages for the device-free CI build.
# Source: https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64/
# TensorRT 10.3 matches the Jetson API family; x86 packages use CUDA 12.5.
NVIDIA_APT_URL=https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64
NVIDIA_KEYRING_VERSION=1.1-1
NVIDIA_KEYRING_SHA256=d93190d50b98ad4699ff40f4f7af50f16a76dac3bb8da1eaaf366d47898ff8df
TENSORRT_VERSION=10.3.0.26-1+cuda12.5
CUDA_VERSION=12.5.82-1
CUDAToolkit_ROOT=/usr/local/cuda-12.5

# Pin the NVIDIA dependency closure, including headers and toolkit config.
# No driver, samples, Python bindings, plugins or full-toolkit metapackage.
TENSORRT_SDK_PACKAGES=(
  "libnvinfer10=$TENSORRT_VERSION"
  "libnvinfer-dev=$TENSORRT_VERSION"
  "libnvinfer-headers-dev=$TENSORRT_VERSION"
  "cuda-cudart-12-5=$CUDA_VERSION"
  "cuda-cudart-dev-12-5=$CUDA_VERSION"
  "cuda-driver-dev-12-5=$CUDA_VERSION"
  "cuda-nvcc-12-5=$CUDA_VERSION"
  "cuda-nvvm-12-5=$CUDA_VERSION"
  "cuda-crt-12-5=$CUDA_VERSION"
  "cuda-toolkit-config-common=$CUDA_VERSION"
  "cuda-toolkit-12-config-common=$CUDA_VERSION"
  "cuda-toolkit-12-5-config-common=$CUDA_VERSION"
  cuda-cccl-12-5=12.5.39-1
)
