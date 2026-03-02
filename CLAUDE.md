# Perry Builder (Linux)

Rust-based build worker for the Perry ecosystem, targeting Linux and Android.
Connects to perry-hub via WebSocket, receives build jobs, compiles perry
projects into native Linux desktop apps or Android APKs, and reports artifacts back.

## Tech Stack
- **Rust** (tokio async runtime)
- WebSocket client: tokio-tungstenite
- HTTP client: reqwest

## Project Structure
```
src/
  main.rs              # Entry point, CLI args
  worker.rs            # WebSocket connection to hub, job dispatch loop
  config.rs            # Configuration (env vars)
  lib.rs               # Library root
  build/
    pipeline.rs        # Build orchestration (Linux + Android pipelines)
    compiler.rs        # Invokes perry compiler
    assets.rs          # Icon generation (PNG, Android mipmap densities)
    cleanup.rs         # Temp directory management
  package/
    linux.rs           # AppImage, .deb, .tar.gz packaging
    android.rs         # Gradle project generation, APK/AAB building
  signing/
    linux.rs           # No-op (future: GPG signing)
    android.rs         # Android keystore signing
  publish/
    linux.rs           # No-op (future: Flatpak/Snap/GitHub Releases)
    playstore.rs       # Google Play Store upload (REST API)
  queue/
    job.rs             # Job manifest and credential types
  ws/
    messages.rs        # WebSocket protocol message types
```

## Build & Run
```sh
cargo build --release
PERRY_BUILD_PERRY_BINARY=~/projects/perry/target/release/perry ./target/release/perry-builder-linux
```

## Environment Variables
- `PERRY_HUB_URL` — Hub WebSocket URL (default: `ws://localhost:3457`)
- `PERRY_BUILD_PERRY_BINARY` — Path to the perry compiler binary
- `PERRY_BUILD_ANDROID_HOME` — Android SDK path (falls back to `ANDROID_HOME`)
- `PERRY_WORKER_NAME` — Optional worker name

## Linux Packaging Formats
- **AppImage** (default) — requires `appimagetool` on PATH
- **.deb** — requires `dpkg-deb` on PATH
- **tar.gz** — no external tools needed

## Worker Capabilities
This worker advertises `["linux", "android"]` to the hub. The hub routes
jobs to workers based on matching target capabilities.

## How It Works
1. Worker connects to hub WebSocket, sends `worker_hello` with capabilities
2. Hub assigns jobs -> worker receives `job_assign` with manifest + tarball path
3. Worker runs build pipeline: compile -> package -> (optional) sign -> (optional) publish
4. Progress/logs streamed back to hub in real-time via WS
5. Finished artifacts registered with hub for CLI download

## Related Repos
- [hub](https://github.com/PerryTS/hub) — the hub server this worker connects to
- [builder-macos](https://github.com/PerryTS/builder-macos) — macOS/iOS builder
- [perry](https://github.com/PerryTS/perry) — compiler + CLI
