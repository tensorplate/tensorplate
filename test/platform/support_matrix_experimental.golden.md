# Platform support matrix

Generated from `config/platform/`. Do not edit by hand — regenerate
with `UPDATE_GOLDEN=1 cargo test -p tensorplate-platform --test support_matrix`.

## Supported combinations

8 supported combination(s): 5 Production, 3 Preview.

### Production

| Row | OS | CPU | Accelerator | Validated on | Model classes |
| --- | --- | --- | --- | --- | --- |
| `jetson-orin-nano-8gb-jp62` | JetPack 6.2.3 (L4T r36.5.x (Ubuntu 22.04 base)) | arm64 (nvidia_soc) | Jetson Orin Nano 8GB Super | Jetson Orin Nano 8GB Super, Super power mode (in lab) | `chunked_policy` (Production) |
| `macos26-m1pro-16gb` | macOS 26 | arm64 (apple) | Apple M1 Pro | MacBook Pro, Apple M1 Pro 16GB (in lab) | `chunked_policy` (Preview) |
| `ubuntu2404-x86-a100-40g-a2hg1` | Ubuntu 24.04 | x86_64 (intel) | NVIDIA A100-SXM4-40GB | `a2-highgpu-1g` only | `chunked_policy` (Preview) |
| `ubuntu2404-x86-l4-g2s8` | Ubuntu 24.04 | x86_64 (intel) | NVIDIA L4 | `g2-standard-8` only | `chunked_policy` (Preview) |
| `ubuntu2404-x86-rtxpro6000se-g4s48` | Ubuntu 24.04 | x86_64 (amd) | NVIDIA RTX PRO 6000 Blackwell Server Edition | `g4-standard-48` only | `chunked_policy` (Production)<br>`autoregressive_action_tokens` (Production)<br>`flow_action_chunk` (Production) |

### Preview

| Row | OS | CPU | Accelerator | Validated on | Model classes |
| --- | --- | --- | --- | --- | --- |
| `macos26-apple-m-series-preview` | macOS 26 | arm64 (apple) | Apple M-series | Apple M-series compatibility envelope; MacBook Pro, Apple M1 Pro 16GB is the current in-lab validation target | `chunked_policy` (Preview) |
| `ubuntu2204-x86-cpu` | Ubuntu 22.04 | x86_64 (amd, intel) | none | Any x86_64 Ubuntu 22.04 host; smoke validated on GitHub-hosted runners | — |
| `ubuntu2404-x86-cpu` | Ubuntu 24.04 | x86_64 (amd, intel) | none | Any x86_64 Ubuntu 24.04 host; smoke validated on GitHub-hosted runners | — |

## Experimental

Experimental rows are **not** supported combinations. They are
listed for visibility only; no support claim attaches to them.

| Row | OS | CPU | Accelerator | Validated on | Model classes |
| --- | --- | --- | --- | --- | --- |
| `ubuntu2404-x86-experimental` | Ubuntu 25.04 | x86_64 (amd, intel) | none | Any x86_64 Ubuntu 25.04 host | — |

## Planned

Planned rows are defined but not validated. They carry no evidence
and are excluded from supported combinations.

| Row | OS | CPU | Accelerator | Validated on |
| --- | --- | --- | --- | --- |
| `jetson-agx-orin-32gb` | JetPack 6.2 (L4T r36.4.x (Ubuntu 22.04 base)) | arm64 (nvidia_soc) | Jetson AGX Orin 32GB | Jetson AGX Orin 32GB (hardware not yet in lab) |
| `jetson-agx-orin-64gb` | JetPack 6.2 (L4T r36.4.x (Ubuntu 22.04 base)) | arm64 (nvidia_soc) | Jetson AGX Orin 64GB | Jetson AGX Orin 64GB (hardware not yet in lab) |
| `jetson-orin-nx-16gb` | JetPack 6.2 (L4T r36.4.x (Ubuntu 22.04 base)) | arm64 (nvidia_soc) | Jetson Orin NX 16GB | Jetson Orin NX 16GB (hardware not yet in lab) |
| `ubuntu2404-x86-rtxpro6000we-physical` | Ubuntu 24.04 | x86_64 (amd) | NVIDIA RTX PRO 6000 Blackwell Workstation Edition | AMD x86_64 workstation (hardware not yet in lab) |

## Roadmap targets (not supported)

These are **not** platform support rows. They are future targets
that are not exact enough to be rows, are never matched against a
machine, and count toward no support total.

| Target | Intended release | Blocking dependency |
| --- | --- | --- |
| `blackwell-dc-single-gpu` — NVIDIA Blackwell datacenter, single GPU | v0.2.x follow-up | Exact SKU, machine shape, and reachable validation environment |
| `pkg-macos-notarized` — Signed and notarized .pkg delivery channel for macOS | follow-up | Signing identity and release tooling |
| `rocm-mi300x` — AMD Instinct MI300X through a PyTorch ROCm package profile | v0.2.2 | Exact machine shape and reachable validation environment |
| `rocm-mi400` — AMD Instinct MI400 family | post-v0.2.2 | Exact SKU and reachable validation environment |
