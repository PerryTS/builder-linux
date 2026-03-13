# Perry Builder (Linux)

Build worker for the [Perry](https://github.com/PerryTS/perry) ecosystem, targeting **Linux** and **Android**. Connects to [Perry Hub](https://github.com/PerryTS/hub) via WebSocket, receives build jobs, and returns signed artifacts.

## How It Works

```
Perry Hub ──WebSocket──► This Worker
   │                        │
   │  job_assign            ├─ compile (perry compiler)
   │  (manifest + tarball)  ├─ package (AppImage / .deb / .apk)
   │                        ├─ sign (keystore)
   │  ◄── progress/logs ────├─ publish (Play Store)
   │  ◄── artifacts ────────┘
```

1. Worker connects to hub, sends `worker_hello` with platform capabilities
2. Hub assigns a job with manifest, credentials, and tarball
3. Worker runs: **compile** → **package** → (optional) **sign** → (optional) **publish**
4. Progress and logs stream back to hub in real-time
5. Built artifacts are uploaded for CLI download

## Building

```sh
cargo build --release
```

## Running

```sh
PERRY_BUILD_PERRY_BINARY=/path/to/perry \
PERRY_HUB_URL=wss://hub.perryts.com/ws \
./target/release/perry-builder-linux
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `PERRY_HUB_URL` | `wss://hub.perryts.com/ws` | Hub WebSocket URL |
| `PERRY_HUB_WORKER_SECRET` | *(empty)* | Shared secret for hub authentication |
| `PERRY_BUILD_PERRY_BINARY` | `perry` | Path to the Perry compiler binary |
| `PERRY_WORKER_NAME` | hostname | Worker display name |
| `PERRY_BUILD_ANDROID_HOME` | `$ANDROID_HOME` | Android SDK path |
| `PERRY_BUILD_ANDROID_NDK_HOME` | `$ANDROID_NDK_HOME` | Android NDK path |

## Capabilities

This worker advertises `["linux", "android"]` to the hub:

- **Linux** — AppImage (default), `.deb`, or `.tar.gz`
- **Android** — `.apk`/`.aab` via Gradle, keystore signing, optional Play Store upload

## Linux Packaging Formats

| Format | Tool Required | Notes |
|---|---|---|
| AppImage | `appimagetool` | Default, portable single-file |
| `.deb` | `dpkg-deb` | Debian/Ubuntu package |
| `.tar.gz` | *(none)* | Simple archive |

## Prerequisites

- [Perry compiler](https://github.com/PerryTS/perry)
- Android SDK + NDK (for Android builds)
- `appimagetool` (for AppImage packaging)

## Related Repos

- [perry](https://github.com/PerryTS/perry) — The Perry compiler and CLI
- [hub](https://github.com/PerryTS/hub) — Central build server
- [builder-macos](https://github.com/PerryTS/builder-macos) — macOS/iOS build worker
- [builder-windows](https://github.com/PerryTS/builder-windows) — Windows build worker

## License

MIT
