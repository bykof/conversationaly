# GPU Acceleration Guide

Conversationaly runs two local inference engines, and both can use your GPU:

- **[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp)** — transcription (ggml).
- **`llama-helper`** — the bundled llama.cpp sidecar that runs Gemma 4 for summaries and audio-LLM transcription.

They are separate crates with separate feature flags, so a GPU build has to enable the backend in *both*. The helper scripts below do that for you.

## Supported Backends

| Backend | Platform | Cargo feature |
| --- | --- | --- |
| **Metal** | macOS (Apple Silicon and Intel) | `metal` — on by default |
| **CUDA** | Windows, Linux (NVIDIA) | `cuda` |
| **Vulkan** | Windows, Linux (AMD/Intel) | `vulkan` |
| **ROCm** | Linux (AMD) | `rocm` |
| **OpenMP** | any (CPU threading, not a GPU backend) | `openmp` |

Defaults per platform:

- **macOS** — Metal is enabled automatically. `transcribe-cpp` builds with it by default and nothing extra is needed.
- **Windows and Linux** — CPU-only unless you opt in. `transcribe-cpp` is pulled in there with `default-features = false`, so a plain `cargo build` gives you a CPU build that always works.

There is no CoreML backend (transcribe.cpp has no CoreML path — Metal covers Apple Silicon) and no OpenBLAS feature (the CPU path uses tinyBLAS). Both existed in the whisper-rs era and were removed with it; if you find either name still referenced somewhere, it is a leftover and will fail the build.

## Automatic Detection

`frontend/scripts/auto-detect-gpu.js` inspects the machine and prints the feature to enable. It is used by:

- `pnpm run tauri:dev` / `pnpm run tauri:build` (via `scripts/tauri-auto.js`)
- `./dev-gpu.sh` / `./build-gpu.sh` (and the `.bat` / `.ps1` equivalents), which additionally build `llama-helper` with the matching feature

Detection order:

1. **macOS** → `metal`, always.
2. **NVIDIA** → `cuda`, if `nvidia-smi` is present *and* the CUDA toolkit is installed (`CUDA_PATH` set or `nvcc` on `PATH`).
3. **AMD on Linux** → `rocm`, if `rocm-smi` is present *and* ROCm is installed (`ROCM_PATH` set or `hipcc` on `PATH`).
4. **Vulkan** → `vulkan`, if `vulkaninfo` is present (or `C:\VulkanSDK` exists on Windows) *and* `VULKAN_SDK` is set.

If a GPU is found but its toolkit is missing, detection says so and falls back to CPU rather than producing a build that fails to link.

To force a backend, set `TAURI_GPU_FEATURE` — detection is skipped entirely:

```bash
TAURI_GPU_FEATURE=vulkan pnpm run tauri:build
TAURI_GPU_FEATURE=none pnpm run tauri:dev     # force CPU
```

## Manual Builds

Pick the backend explicitly with the per-feature scripts:

```bash
pnpm run tauri:dev:metal      # or :cuda, :vulkan, :rocm, :cpu
pnpm run tauri:build:cuda     # same set for builds
```

Or drive cargo directly:

```bash
cd frontend/src-tauri
cargo build --release                     # macOS: Metal | Windows/Linux: CPU
cargo build --release --features cuda
cargo build --release --features vulkan
cargo build --release --features rocm     # Linux only
```

Building the app this way does **not** rebuild the sidecar. Match it manually, or the summaries will run on CPU while transcription uses the GPU:

```bash
cd llama-helper
cargo build --release --features cuda     # metal | cuda | vulkan | rocm
```

Note that `clean_run.sh` and `clean_build.sh` (the macOS scripts) run plain `tauri dev` / `tauri build` with no feature flag. On macOS that is already a Metal build, which is why they need no GPU handling — on other platforms use `dev-gpu.sh` / `build-gpu.sh` instead.

## Platform Requirements

### macOS

Nothing to install. Metal ships with the OS and is on by default.

### Windows

- **NVIDIA** — install the [CUDA Toolkit](https://developer.nvidia.com/cuda-downloads), then build with `cuda`. Detection needs `CUDA_PATH` or `nvcc`.
- **AMD / Intel** — install the [Vulkan SDK](https://vulkan.lunarg.com/), set `VULKAN_SDK`, then build with `vulkan`.

Also requires Visual Studio Build Tools with the C++ workload, plus cmake.

### Linux

- **NVIDIA** — CUDA toolkit; build with `cuda`. For older cards you may need to set `CMAKE_CUDA_ARCHITECTURES` (the helper scripts set `75` for CUDA builds).
- **AMD** — ROCm with `hipcc` on `PATH`; build with `rocm`.
- **Other GPUs** — Vulkan SDK with `VULKAN_SDK` set; build with `vulkan`.

See [building_in_linux.md](building_in_linux.md) for the full dependency list.

## Verifying It Worked

ggml prints its backend initialization to stderr when a model loads, so run the app from a terminal and watch the output while the first transcription model loads:

```bash
cd frontend
RUST_LOG=info ./clean_run.sh
```

Look for the ggml backend lines (`ggml_metal_init`, `ggml_cuda_init`, `ggml_vulkan`, …). A CPU build prints none of them.

If transcription runs but feels slower than real time, check that the backend you expect is actually there before tuning anything else — a silent CPU fallback is the usual cause.
</content>
