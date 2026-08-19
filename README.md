<div align="center" style="border-bottom: none">
    <h1>Conversationaly</h1>
    <a href="https://github.com/bykof/conversationaly/stargazers"><img alt="GitHub Repo stars" src="https://img.shields.io/github/stars/bykof/conversationaly?style=flat"></a>
    <a href="LICENSE.md"><img src="https://img.shields.io/badge/License-MIT-blue" alt="License"></a>
    <img src="https://img.shields.io/badge/Supported_OS-macOS,_Windows,_Linux-white" alt="Supported OS">
    <h3>Privacy-First AI Meeting Assistant</h3>
    <p align="center">

Records your meetings, transcribes them live, and writes the summary — on your machine, with no account and no cloud round-trip unless you deliberately configure one.

</p>

<img src="docs/imgs/live-recording.gif" alt="Conversationaly opening, a recording starting, and the first spoken words being transcribed live" width="100%">

</div>

<details>
<summary>Table of Contents</summary>

- [Introduction](#introduction)
- [Features](#features)
- [Installation](#installation)
- [How It Works](#how-it-works)
- [System Architecture](#system-architecture)
- [For Developers](#for-developers)
- [Contributing](#contributing)
- [License](#license)
- [Acknowledgments](#acknowledgments)

</details>

## Introduction

Conversationaly is a desktop app (macOS, Windows, Linux) that captures your microphone and system audio, transcribes the meeting as it happens, and generates a summary. Transcription models and the summary LLM both run locally by default — nothing is sent anywhere. Cloud providers are available if you want them, but they are opt-in, per-feature.

It is a fork of [Meetily](https://github.com/Zackriya-Solutions/meeting-minutes), rebuilt around [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) and a bundled llama.cpp sidecar. It is fully free — there is no paid tier, no license key, no telemetry.

## Features

- **Local transcription** — ~87 models across 16 families (Whisper, Parakeet, Nemotron, Canary, Voxtral, Qwen3-ASR, SenseVoice, Moonshine, GigaAM, …), downloaded on demand. Default: `nemotron-3.5-asr-streaming-0.6b-q8`, multilingual across 39 locales.
- **Live transcript** — streaming-native models transcribe continuously as you speak; batch-only models are segmented by voice activity and still work live.
- **Built-in AI, no Ollama required** — a bundled `llama-helper` sidecar runs Gemma 4 locally for summaries, and can also transcribe directly as an audio LLM.
- **Bring your own LLM** — summaries via Built-in AI, Ollama, Claude, Groq, OpenRouter, OpenAI, or any OpenAI-compatible endpoint.
- **Optional cloud STT** — Deepgram, ElevenLabs, Groq, or OpenAI, if you prefer a hosted transcriber.
- **Professional audio mixing** — microphone and system audio captured together with RMS-based ducking and clipping prevention.
- **Import & enhance** `Beta` — transcribe existing audio files, or re-transcribe a past meeting with a different model or language.
- **Summary templates** — pick or write the structure your summaries follow, and set the summary language independently of the spoken one.
- **GPU acceleration** — Metal on Apple Silicon, CUDA (NVIDIA), Vulkan (AMD/Intel), ROCm (AMD on Linux).
- **Local storage** — meetings, transcripts, and models live in a SQLite database and a model directory on your disk.

<p align="center">
  <img src="docs/imgs/transcription_settings.png" alt="Transcription settings listing downloadable local models with quality, WER, speed and size" width="49%">
  <img src="docs/imgs/summary_models.png" alt="Summary settings listing the built-in Gemma 4 and Qwen 3.5 models" width="49%">
</p>

## Installation

Prebuilt installers (macOS `.dmg`, Windows `.exe`, Linux `.deb`/`.rpm`/`.AppImage`) are published on the [Releases page](https://github.com/bykof/conversationaly/releases) when a version is tagged.

### Build from source

Requires Rust, Node.js, pnpm, and cmake. See [docs/BUILDING.md](docs/BUILDING.md) for per-platform prerequisites.

```bash
git clone https://github.com/bykof/conversationaly
cd conversationaly/frontend
pnpm install

# macOS
./clean_build.sh

# Linux (auto-detects GPU backend)
./build-gpu.sh

# Windows
clean_build_windows.bat
```

Linux specifics: [docs/building_in_linux.md](docs/building_in_linux.md).

### Permissions

- **macOS** — microphone, plus screen recording for system audio (ScreenCaptureKit, macOS 13+).
- **Windows** — microphone; system audio uses WASAPI loopback.

## How It Works

On first launch, onboarding downloads one transcription model and one Gemma 4 tier. From then on:

1. Microphone and system audio are captured, mixed, and written to a recording.
2. The same mixed audio is resampled to 16 kHz and fed to the transcription engine, which emits transcript lines as the meeting runs.
3. When you ask for a summary, the transcript goes to whichever LLM provider you configured — the local sidecar by default.

![A finished meeting: the transcript on the left, the generated summary on the right](docs/imgs/meeting_transcription_summary.png)

Everything above is a local process. Cloud STT and cloud summary providers are the only paths that leave your machine, and only when you select one and supply a key.

## System Architecture

A single Tauri application: a Rust core (audio capture, transcription, storage, summary orchestration) and a Next.js frontend, communicating over Tauri commands and events. There is no separate server to run.

Details: [docs/architecture.md](docs/architecture.md).

## For Developers

```bash
cd frontend
pnpm install

./clean_run.sh              # macOS: build and run (info logging)
./clean_run.sh debug        # verbose logging
clean_run_windows.bat       # Windows
./dev-gpu.sh                # Linux

pnpm run tauri:dev          # plain dev mode
pnpm run tauri:dev:metal    # force a specific GPU backend
pnpm run tauri:dev:cuda
pnpm run tauri:dev:vulkan
pnpm run tauri:dev:cpu
```

Architecture notes and conventions for contributors live in [CLAUDE.md](CLAUDE.md); GPU backend details in [docs/GPU_ACCELERATION.md](docs/GPU_ACCELERATION.md).

> The `backend/` directory is an archived Python/FastAPI service from before the Tauri rewrite. It is unsupported and not needed to build or run the app.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for project structure and guidelines.

## License

MIT — see [LICENSE.md](LICENSE.md).

## Acknowledgments

- Conversationaly is a fork of [Meetily](https://github.com/Zackriya-Solutions/meeting-minutes) by Zackriya Solutions, which it builds on under the MIT license.
- Transcription runs on [transcribe.cpp](https://github.com/handy-computer/transcribe.cpp), built on [ggml](https://github.com/ggerganov/ggml) / [whisper.cpp](https://github.com/ggerganov/whisper.cpp).
- Local LLM inference uses [llama.cpp](https://github.com/ggerganov/llama.cpp) via [llama-cpp-2](https://crates.io/crates/llama-cpp-2).
- We borrowed some code from [Screenpipe](https://github.com/mediar-ai/screenpipe) and [transcribe-rs](https://crates.io/crates/transcribe-rs).
- Import & Enhance was contributed by [Jeremi Joslin](https://github.com/jeremi), improved by [Vishnu P S](https://github.com/p-s-vishnu) and [Mohammed Safvan](https://github.com/mohammedsafvan).
- Thanks to **NVIDIA** for the **Parakeet** and **Nemotron** speech models, and to the teams behind the other model families in the catalog.
</content>
