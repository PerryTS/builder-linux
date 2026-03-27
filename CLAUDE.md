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
Advertises `["linux", "android", "windows"]` to the hub. Windows builds are
cross-compiled using `lld-link` + xwin sysroot, producing a precompiled bundle
that the hub re-queues to a Windows sign worker.

## Concurrent Builds
Supports running multiple builds in parallel (default 2, via `PERRY_MAX_CONCURRENT_BUILDS`).
Each build runs in its own Docker container (when `PERRY_DOCKER_ENABLED=true`).
Builds are spawned as tokio tasks with a shared WS write channel.

## Docker Isolation
When enabled (`PERRY_DOCKER_ENABLED=true`), builds run in Docker containers:
- Project dir mounted writable (perry writes .o files during compilation)
- Perry binary + libs mounted read-only from `/opt/perry-src/target/`
- Rust toolchain mounted read-only
- Resource limits: 4GB RAM, 2 CPUs, no-new-privileges
- Network enabled (`--network=host`) for native lib cargo builds

## Windows Cross-Compilation
- Uses `lld-link` (LLVM's MSVC-compatible linker) via Rust toolchain
- xwin sysroot at `PERRY_WINDOWS_SYSROOT` for Windows SDK import libraries
- `strip_duplicate_objects_from_lib` removes perry_runtime duplicates from UI staticlibs using rlib
- rlib for `perry-ui-windows` built locally during `update_perry` (cross-compile works for this crate)
- Windows .lib files (perry_stdlib, perry_runtime) copied from Azure VM via `update_windows_libs`

## Additional Environment Variables
- `PERRY_MAX_CONCURRENT_BUILDS` — Max parallel builds (default: 2)
- `PERRY_DOCKER_ENABLED` — Enable Docker isolation (default: false)
- `PERRY_DOCKER_IMAGE` — Docker image name (default: perry-build)
- `PERRY_WINDOWS_SYSROOT` — Path to xwin Windows SDK sysroot

## How It Works
1. Worker connects to hub WebSocket, sends `worker_hello` with capabilities + `max_concurrent`
2. Hub assigns jobs → worker receives `job_assign`, spawns build as async task
3. Each build: download tarball → compile (in Docker if enabled) → package → sign → upload
4. For Windows: cross-compile → create precompiled bundle → hub re-queues to sign worker
5. Progress/logs streamed via shared WS channel, multiple builds run concurrently

## Related Repos
- [hub](https://github.com/PerryTS/hub) — the hub server this worker connects to
- [builder-macos](https://github.com/PerryTS/builder-macos) — macOS/iOS builder
- [perry](https://github.com/PerryTS/perry) — compiler + CLI
