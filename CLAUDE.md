# Perry Builder (Linux)

Rust-based build worker for the Perry ecosystem. Handles ALL compilation for
every platform: Linux, Android, Windows, iOS, and macOS. Cross-compiles
iOS/macOS using ld64.lld + Apple SDK sysroot, and Windows using lld-link + xwin.
Connects to perry-hub via WebSocket, receives build jobs, and reports artifacts back.
Precompiled bundles for iOS/macOS/Windows are re-queued by the hub to sign-only workers.

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
Advertises `["linux", "android", "windows", "ios", "macos"]` to the hub.
- **Linux/Android**: native compilation, full pipeline
- **Windows**: cross-compiled using `lld-link` + xwin sysroot → precompiled bundle → hub re-queues to Windows sign worker (Azure VM)
- **iOS**: cross-compiled using `ld64.lld` + Apple SDK sysroot at `/opt/apple-sysroot/ios/` → precompiled bundle → hub re-queues as `ios-sign` to macOS worker
- **macOS**: cross-compiled using `ld64.lld` + Apple SDK sysroot at `/opt/apple-sysroot/macos/` → precompiled bundle → hub re-queues as `macos-sign` to macOS worker

## Apple Cross-Compilation
- Uses `ld64.lld` (LLVM's Mach-O linker) + `libLLVM.so.18`, mounted into Docker containers
- Apple SDK sysroot (~140MB): headers + .tbd stubs at `/opt/apple-sysroot/{ios,macos}/`
- All Apple libs (perry-runtime, perry-stdlib, perry-ui) cross-compile from Linux using `clang` + `SDKROOT`
- `CC_aarch64_apple_ios=clang` + `SDKROOT` env vars for ring/cc-rs crates
- Linker flags: `-dead_strip` directly (not `-Wl,-dead_strip`) for ld64.lld compatibility

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
- `PERRY_IOS_SYSROOT` — Path to iOS Apple SDK sysroot (default: `/opt/apple-sysroot/ios`)
- `PERRY_MACOS_SYSROOT` — Path to macOS Apple SDK sysroot (default: `/opt/apple-sysroot/macos`)

## How It Works
1. Worker connects to hub WebSocket, sends `worker_hello` with capabilities + `max_concurrent`
2. Hub assigns jobs → worker receives `job_assign`, spawns build as async task
3. Each build: download tarball → compile (in Docker if enabled) → package → sign → upload
4. For cross-platform targets:
   - **Windows**: cross-compile → precompiled bundle → hub re-queues as `windows-sign` to Azure VM
   - **iOS**: cross-compile → precompiled .app bundle → hub re-queues as `ios-sign` to macOS worker
   - **macOS**: cross-compile → precompiled .app bundle → hub re-queues as `macos-sign` to macOS worker
5. Progress/logs streamed via shared WS channel, multiple builds run concurrently

## Related Repos
- [hub](https://github.com/PerryTS/hub) — the hub server this worker connects to
- [builder-macos](https://github.com/PerryTS/builder-macos) — macOS/iOS sign-only worker
- [perry](https://github.com/PerryTS/perry) — compiler + CLI
