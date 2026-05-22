use crate::build::pipeline::{self, BuildRequest};
use crate::config::WorkerConfig;
use crate::ws::messages::{ErrorCode, ServerMessage};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// Upload a built artifact to the hub via HTTP POST (base64-encoded body).
async fn upload_artifact(
    url: &str,
    artifact_path: &std::path::Path,
    artifact_name: &str,
    sha256: &str,
    target: &str,
    auth_token: Option<&str>,
) -> Result<serde_json::Value, String> {
    use base64::Engine;
    let data =
        std::fs::read(artifact_path).map_err(|e| format!("Failed to read artifact: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);

    let client = reqwest::Client::new();
    let mut req = client
        .post(url)
        .header("Content-Type", "text/plain")
        .header("x-artifact-name", artifact_name)
        .header("x-artifact-sha256", sha256)
        .header("x-artifact-target", target);
    if let Some(token) = auth_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .body(b64)
        .send()
        .await
        .map_err(|e| format!("Artifact upload failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Hub returned HTTP {status} for artifact upload: {body}"));
    }

    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Failed to parse upload response: {e}"))
}

/// Download a base64-encoded tarball from the hub and write the decoded bytes to a temp file.
async fn download_tarball(url: &str, job_id: &str, auth_token: Option<&str>) -> Result<PathBuf, String> {
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if let Some(token) = auth_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Hub returned HTTP {}", resp.status()));
    }

    let b64_text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read tarball response body: {e}"))?;

    use base64::Engine;
    let tarball_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_text.trim())
        .map_err(|e| format!("Failed to base64-decode tarball: {e}"))?;

    let dl_dir = std::env::temp_dir().join("perry-worker-dl");
    std::fs::create_dir_all(&dl_dir)
        .map_err(|e| format!("Failed to create download dir: {e}"))?;

    let tarball_path = dl_dir.join(format!("{job_id}.tar.gz"));
    std::fs::write(&tarball_path, &tarball_bytes)
        .map_err(|e| format!("Failed to write tarball to disk: {e}"))?;

    Ok(tarball_path)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubMessage {
    JobAssign {
        job_id: String,
        manifest: serde_json::Value,
        credentials: serde_json::Value,
        tarball_url: String,
        #[serde(default)]
        artifact_upload_url: Option<String>,
        #[serde(default)]
        auth_token: Option<String>,
    },
    Cancel {
        job_id: String,
    },
    UpdatePerry {},
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerMessage {
    WorkerHello {
        capabilities: Vec<String>,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        perry_version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_concurrent: Option<usize>,
    },
    UpdateResult {
        success: bool,
        old_version: String,
        new_version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Get the perry compiler version by running `perry --version`.
fn get_perry_version(perry_binary: &str) -> Option<String> {
    std::process::Command::new(perry_binary)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            s.strip_prefix("perry ").map(|v| v.to_string()).or_else(|| {
                if s.is_empty() { None } else { Some(s) }
            })
        })
}

/// Update perry by DOWNLOADING prebuilt release bundles instead of
/// cross-compiling from source.
///
/// Background: this used to `git reset --hard origin/main` + `cargo build`
/// the host libs, then cross-compile perry-runtime/stdlib/ui for
/// android/ios/macos/tvos (+ an SSH-to-Azure dance for the Windows .libs).
/// That accumulated ~50 GB of cargo intermediates in
/// `target/<triple>/release/` per update cycle and repeatedly filled the
/// worker disk, and each target's cross toolchain was independently
/// fragile (AWS_LC jitterentropy, Azure-VM disk, NDK clang++ lookup, …).
///
/// perry release CI (PerryTS/perry#1083 / PR #1084) now ships:
///   - `perry-linux-x86_64.tar.gz`         host: perry + host .a's
///   - `perry-cross-<triple>.tar.gz`       per-target runtime/stdlib/ui .a's
///
/// So an update is now: resolve the latest release tag, download the host
/// bundle into `target/release/`, download each cross bundle into
/// `target/<triple>/release/`, and build ONLY the one artifact CI doesn't
/// ship — the `ios-game-loop` runtime variant (small, single crate) —
/// locally. Net: ~30 s of curl+tar instead of ~30 min of cross-compiles,
/// no Azure VM, and `target/` stays tiny.
async fn run_perry_update(perry_binary: &str) -> (bool, String, Option<String>) {
    // Prevent concurrent updates
    let lock_path = std::env::temp_dir().join("perry-update.lock");
    if lock_path.exists() {
        tracing::info!("Update already in progress, skipping");
        return (false, String::new(), Some("Update already in progress".into()));
    }
    let _ = std::fs::write(&lock_path, "");
    struct LockGuard(std::path::PathBuf);
    impl Drop for LockGuard { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
    let _lock = LockGuard(lock_path);

    // perry_binary = <src_dir>/target/release/perry → src_dir is 3 up.
    let src_dir = std::path::Path::new(perry_binary)
        .parent().and_then(|p| p.parent()).and_then(|p| p.parent());
    let src_dir = match src_dir {
        Some(d) => d.to_path_buf(),
        None => return (false, String::new(),
            Some("Cannot determine perry source directory from binary path".into())),
    };
    let target_dir = src_dir.join("target");
    let host_release = target_dir.join("release");

    let repo = std::env::var("PERRY_RELEASE_REPO")
        .unwrap_or_else(|_| "PerryTS/perry".to_string());

    // Resolve the release tag to install. Honour an explicit override
    // (PERRY_RELEASE_TAG) so the hub's expected_version can pin a tag;
    // otherwise take the most-recently-published release.
    let tag = match std::env::var("PERRY_RELEASE_TAG") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            let api = format!("https://api.github.com/repos/{repo}/releases?per_page=1");
            let out = tokio::process::Command::new("curl")
                .args(["-sSL", "-H", "User-Agent: perry-builder-linux", &api])
                .output().await;
            let body = match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
                Ok(o) => return (false, String::new(),
                    Some(format!("release lookup failed: {}", String::from_utf8_lossy(&o.stderr)))),
                Err(e) => return (false, String::new(),
                    Some(format!("release lookup failed: {e}"))),
            };
            // Avoid a jq dependency: pull the first "tag_name" out of the JSON.
            let t = body.split("\"tag_name\"").nth(1)
                .and_then(|s| s.split('"').nth(1))
                .map(|s| s.to_string());
            match t {
                Some(t) if !t.is_empty() => t,
                _ => return (false, String::new(),
                    Some("could not parse latest release tag from GitHub API".into())),
            }
        }
    };

    tracing::info!(tag = %tag, "Updating perry from release bundles");

    let base = format!("https://github.com/{repo}/releases/download/{tag}");
    let tmp = std::env::temp_dir().join(format!("perry-dl-{tag}"));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return (false, String::new(), Some(format!("mkdir tmp failed: {e}")));
    }
    struct TmpGuard(std::path::PathBuf);
    impl Drop for TmpGuard { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
    let _tmp_guard = TmpGuard(tmp.clone());

    // Download `url` to `tmp/<name>` then extract it into `dest` (created if
    // absent). Returns Err(msg) on any failure. tar strips nothing — the
    // bundles are flat (files at archive root), so `tar -C dest -xzf` lands
    // them directly in dest.
    async fn fetch_extract(
        url: &str, name: &str, tmp: &std::path::Path, dest: &std::path::Path,
    ) -> Result<(), String> {
        let archive = tmp.join(name);
        let dl = tokio::process::Command::new("curl")
            .args(["-fsSL", "-o", &archive.to_string_lossy(), url])
            .output().await
            .map_err(|e| format!("curl {url}: {e}"))?;
        if !dl.status.success() {
            return Err(format!("download {url} failed: {}",
                String::from_utf8_lossy(&dl.stderr)));
        }
        std::fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
        let ex = tokio::process::Command::new("tar")
            .args(["-xzf", &archive.to_string_lossy(),
                   "-C", &dest.to_string_lossy()])
            .output().await
            .map_err(|e| format!("tar {name}: {e}"))?;
        let _ = std::fs::remove_file(&archive);
        if !ex.status.success() {
            return Err(format!("extract {name} failed: {}",
                String::from_utf8_lossy(&ex.stderr)));
        }
        Ok(())
    }

    // 1. Host bundle — perry binary + Linux host .a's. FATAL on failure:
    //    without a fresh perry binary there's no point continuing.
    let host_url = format!("{base}/perry-linux-x86_64.tar.gz");
    if let Err(e) = fetch_extract(&host_url, "perry-host.tar.gz", &tmp, &host_release).await {
        return (false, String::new(), Some(format!("host bundle: {e}")));
    }
    // Bundle ships `perry` mode 644 from tar; ensure it's executable.
    let _ = tokio::process::Command::new("chmod")
        .args(["+x", &host_release.join("perry").to_string_lossy()])
        .output().await;
    tracing::info!("Installed host bundle (perry + Linux libs)");

    // 2. Cross bundles — one per target triple. Each extracts
    //    libperry_runtime.a / libperry_stdlib.a / libperry_ui_<t>.a into
    //    target/<triple>/release/ where find_library resolves them. A
    //    missing/!published bundle is non-fatal (that platform just keeps
    //    its prior libs) — mirrors the old per-target non-fatal behaviour.
    let cross_triples = [
        "aarch64-apple-ios",
        "aarch64-apple-darwin",
        "aarch64-apple-tvos",
        "aarch64-linux-android",
        "x86_64-pc-windows-msvc",
    ];
    for triple in cross_triples {
        let url = format!("{base}/perry-cross-{triple}.tar.gz");
        let dest = target_dir.join(triple).join("release");
        match fetch_extract(&url, &format!("perry-cross-{triple}.tar.gz"), &tmp, &dest).await {
            Ok(()) => tracing::info!("Installed cross bundle for {triple}"),
            Err(e) => tracing::warn!("cross bundle {triple} (non-fatal): {e}"),
        }
    }

    // 3. ios-game-loop runtime variant. CI does NOT ship this (it's a
    //    feature-flagged build of just perry-runtime); games like jump
    //    link `libperry_runtime_gameloop.a`. Build the single crate
    //    locally against the Apple sysroot — tiny vs. the old full matrix.
    //    Non-fatal: a failure only affects game (ios-game-loop) builds.
    if src_dir.join(".git").exists() {
        let cargo = std::env::var("CARGO_HOME")
            .map(|h| format!("{h}/bin/cargo"))
            .unwrap_or_else(|_| "cargo".to_string());
        let ios_sysroot = std::env::var("PERRY_IOS_SYSROOT")
            .unwrap_or_else(|_| "/opt/apple-sysroot/ios".to_string());
        let gl = tokio::process::Command::new(&cargo)
            .args(["build", "--release", "-p", "perry-runtime",
                   "--features", "ios-game-loop", "--target", "aarch64-apple-ios"])
            .current_dir(&src_dir)
            .env("CC_aarch64_apple_ios", "clang")
            .env("CXX_aarch64_apple_ios", "clang++")
            .env("CFLAGS_aarch64_apple_ios",
                 format!("--target=arm64-apple-ios17.0 -isysroot {ios_sysroot}"))
            .env("AWS_LC_SYS_NO_JITTER_ENTROPY", "1")
            .env("SDKROOT", &ios_sysroot)
            .output().await;
        match gl {
            Ok(o) if o.status.success() => {
                let rel = target_dir.join("aarch64-apple-ios").join("release");
                let _ = std::fs::copy(
                    rel.join("libperry_runtime.a"),
                    rel.join("libperry_runtime_gameloop.a"));
                tracing::info!("Built perry-runtime (ios-game-loop) variant");
                // The above cargo build overwrote the bundle's normal
                // libperry_runtime.a with a gameloop-featured one. Restore
                // the canonical one from the cross bundle by re-extracting.
                let url = format!("{base}/perry-cross-aarch64-apple-ios.tar.gz");
                let dest = target_dir.join("aarch64-apple-ios").join("release");
                if let Err(e) = fetch_extract(
                    &url, "perry-cross-ios-restore.tar.gz", &tmp, &dest).await {
                    tracing::warn!("restore ios runtime after gameloop (non-fatal): {e}");
                }
            }
            Ok(o) => tracing::warn!("ios-game-loop build failed (non-fatal): {}",
                String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("")),
            Err(e) => tracing::warn!("ios-game-loop build failed (non-fatal): {e}"),
        }
    } else {
        tracing::warn!("no .git in {} — skipping ios-game-loop local build",
            src_dir.display());
    }

    let new_version = get_perry_version(perry_binary).unwrap_or_default();
    tracing::info!(version = %new_version, tag = %tag, "Perry update complete (from release bundles)");
    (true, new_version, None)
}

/// Rebuild Windows runtime/stdlib/UI libs on the Windows build server
/// and copy them to the local cross-compilation target directory.
/// Uses SSH key auth (PERRY_WINDOWS_BUILD_HOST + PERRY_WINDOWS_BUILD_USER)
/// or password auth (+ PERRY_WINDOWS_BUILD_PASSWORD) to connect.
///
/// No longer called: `run_perry_update` now pulls Windows libs from the
/// `perry-cross-x86_64-pc-windows-msvc.tar.gz` release bundle, so the
/// Azure-VM SSH dance is gone. Retained as a documented fallback.
#[allow(dead_code)]
async fn update_windows_libs(perry_src_dir: &std::path::Path) {
    let win_host = std::env::var("PERRY_WINDOWS_BUILD_HOST").unwrap_or_default();
    let win_user = std::env::var("PERRY_WINDOWS_BUILD_USER").unwrap_or_default();
    let win_pass = std::env::var("PERRY_WINDOWS_BUILD_PASSWORD").ok();
    let win_perry_dir = std::env::var("PERRY_WINDOWS_BUILD_DIR")
        .unwrap_or_else(|_| "C:/Users/perryadmin/perry-compiler".into());

    if win_host.is_empty() || win_user.is_empty() {
        tracing::info!("Windows build host not configured, skipping Windows .lib update");
        return;
    }

    // First, start the Azure VM if configured (it may be deallocated)
    if let Some(azure) = crate::azure::AzureVmConfig::from_env() {
        tracing::info!("Starting Azure Windows VM for lib rebuild...");
        match crate::azure::start_vm(&azure).await {
            Ok(()) => {
                tracing::info!("Azure VM start triggered, waiting 90s for boot...");
                tokio::time::sleep(std::time::Duration::from_secs(90)).await;
            }
            Err(e) => tracing::warn!("Failed to start Azure VM (may already be running): {e}"),
        }
    }

    tracing::info!("Rebuilding Windows .lib files on {win_host}...");

    // Build SSH/SCP commands (key auth or password auth)
    let ssh_base = if let Some(ref pass) = win_pass {
        format!(
            "sshpass -p '{}' ssh -o PubkeyAuthentication=no -o StrictHostKeyChecking=no",
            pass
        )
    } else {
        "ssh -o StrictHostKeyChecking=no".into()
    };
    let scp_base = if let Some(ref pass) = win_pass {
        format!(
            "sshpass -p '{}' scp -o PubkeyAuthentication=no -o StrictHostKeyChecking=no",
            pass
        )
    } else {
        "scp -o StrictHostKeyChecking=no".into()
    };

    let remote = format!("{}@{}", win_user, win_host);
    let win_perry_posix = win_perry_dir.replace('\\', "/");

    // Pull and rebuild on Windows server
    // PowerShell commands work over SSH since we set DefaultShell to PowerShell
    let build_script = format!(
        concat!(
            "$env:PATH = \"C:\\Users\\{}\\.cargo\\bin;C:\\Program Files\\Git\\cmd;\" + $env:PATH; ",
            "cd \"{}\"; ",
            "git pull; ",
            "cargo build --release -p perry-runtime -p perry-ui-windows -p perry-stdlib"
        ),
        win_user, win_perry_dir
    );
    let build_cmd = format!("{} {} '{}'", ssh_base, remote, build_script);

    let build = tokio::process::Command::new("bash")
        .args(["-c", &build_cmd])
        .output()
        .await;

    match &build {
        Ok(o) if o.status.success() => {
            tracing::info!("Windows libs rebuilt successfully");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let stdout = String::from_utf8_lossy(&o.stdout);
            // cargo outputs "Finished" to stderr — check if it actually succeeded
            if stderr.contains("Finished") || stdout.contains("Finished") {
                tracing::info!("Windows libs rebuilt successfully");
            } else {
                tracing::warn!("Windows lib rebuild failed (non-fatal): {stderr}");
                return;
            }
        }
        Err(e) => {
            tracing::warn!("Windows lib rebuild failed (non-fatal): {e}");
            return;
        }
    }

    // Copy libs to local cross-compilation directory
    let dest_dir = perry_src_dir.join("target/x86_64-pc-windows-msvc/release");
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        tracing::warn!("Failed to create Windows lib dir: {e}");
        return;
    }

    // Only copy runtime and stdlib — perry_ui_windows.lib is cross-compiled locally
    // (along with its .rlib) to ensure strip-dedup has matching hashes
    for lib in &["perry_runtime.lib", "perry_stdlib.lib"] {
        let cp = format!(
            "{} '{}:{}/target/release/{}' '{}'",
            scp_base, remote, win_perry_posix, lib,
            dest_dir.join(lib).display()
        );

        let result = tokio::process::Command::new("bash")
            .args(["-c", &cp])
            .output()
            .await;

        match &result {
            Ok(o) if o.status.success() => {
                tracing::info!("Copied {lib} from Windows server");
            }
            Ok(o) => {
                tracing::warn!("Failed to copy {lib}: {}", String::from_utf8_lossy(&o.stderr));
            }
            Err(e) => {
                tracing::warn!("Failed to copy {lib}: {e}");
            }
        }
    }

    tracing::info!("Windows .lib files updated");
}

pub async fn run_worker(config: WorkerConfig) {
    tracing::info!("Perry Linux builder starting, connecting to hub: {}", config.hub_ws_url);

    loop {
        match connect_and_run(&config).await {
            Ok(_) => {
                tracing::info!("Connection to hub closed, reconnecting in 5s...");
            }
            Err(e) => {
                tracing::error!("Connection error: {e}, reconnecting in 5s...");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn connect_and_run(config: &WorkerConfig) -> Result<(), String> {
    let azure_config = crate::azure::AzureVmConfig::from_env();

    let (ws_stream, _) = connect_async(&config.hub_ws_url)
        .await
        .map_err(|e| format!("Failed to connect to hub: {e}"))?;

    let (mut write, mut read) = ws_stream.split();

    // Send worker_hello
    let perry_version = get_perry_version(&config.perry_binary);
    let hello = WorkerMessage::WorkerHello {
        capabilities: vec!["linux".into(), "android".into(), "windows".into(), "ios".into(), "macos".into(), "tvos".into()],
        name: config.worker_name.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "worker".into())
        }),
        secret: config.hub_secret.clone(),
        perry_version,
        max_concurrent: Some(config.max_concurrent_builds),
    };

    write
        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .map_err(|e| format!("Failed to send worker_hello: {e}"))?;

    tracing::info!(max_concurrent = config.max_concurrent_builds, "Connected to hub, waiting for jobs...");

    // Shared WS write channel — build tasks send messages here
    let (ws_tx, mut ws_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // Spawn dedicated WS writer task — drains ws_rx independently of main loop.
    // This prevents WS write backpressure from blocking job dispatch and
    // ensures complete/progress messages are always delivered.
    let ws_writer_error = Arc::new(std::sync::Mutex::new(None::<String>));
    let ws_writer_err_clone = ws_writer_error.clone();
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = ws_rx.recv().await {
            if let Err(e) = write.send(msg).await {
                *ws_writer_err_clone.lock().unwrap() = Some(format!("WS write failed: {e}"));
                break;
            }
        }
    });

    // Per-job cancellation flags
    let cancel_flags: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let active_builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    loop {
        // Check if WS writer died
        if let Some(err) = ws_writer_error.lock().unwrap().take() {
            return Err(err);
        }

        tokio::select! {
            biased;

            // Incoming WebSocket message
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        return Err(format!("WebSocket error: {e}"));
                    }
                    None => break,
                };

                let text = match msg {
                    Message::Text(t) => t,
                    Message::Ping(data) => {
                        let _ = ws_tx.send(Message::Pong(data));
                        continue;
                    }
                    Message::Close(_) => break,
                    _ => continue,
                };

                let hub_msg: HubMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("Failed to parse hub message: {e}");
                        continue;
                    }
                };

                match hub_msg {
                    HubMessage::JobAssign {
                        job_id,
                        manifest,
                        credentials,
                        tarball_url,
                        artifact_upload_url,
                        auth_token,
                    } => {
                        let n = active_builds.load(Ordering::Relaxed);
                        tracing::info!(job_id = %job_id, active = n, "Received job assignment");

                        let cancelled = Arc::new(AtomicBool::new(false));
                        cancel_flags.lock().unwrap().insert(job_id.clone(), cancelled.clone());

                        let build_config = config.clone();
                        let build_ws_tx = ws_tx.clone();
                        let build_active = active_builds.clone();
                        let build_cancel_flags = cancel_flags.clone();
                        let build_azure = azure_config.clone();
                        build_active.fetch_add(1, Ordering::Relaxed);

                        tokio::spawn(async move {
                            handle_build(
                                &build_config,
                                &build_ws_tx,
                                &cancelled,
                                build_azure.as_ref(),
                                job_id.clone(),
                                manifest,
                                credentials,
                                tarball_url,
                                artifact_upload_url,
                                auth_token,
                            ).await;

                            build_active.fetch_sub(1, Ordering::Relaxed);
                            build_cancel_flags.lock().unwrap().remove(&job_id);
                        });
                    }

                    HubMessage::Cancel { job_id } => {
                        if let Some(flag) = cancel_flags.lock().unwrap().get(&job_id) {
                            tracing::info!(job_id = %job_id, "Cancelling build");
                            flag.store(true, Ordering::Relaxed);
                        } else {
                            tracing::info!(job_id = %job_id, "Cancel request (no active build)");
                        }
                    }

                    HubMessage::UpdatePerry {} => {
                        let n = active_builds.load(Ordering::Relaxed);
                        if n > 0 {
                            tracing::info!("Deferring update_perry: {n} builds active");
                        } else {
                            tracing::info!("Received update_perry request from hub");
                            let old_version = get_perry_version(&config.perry_binary).unwrap_or_default();
                            let (success, new_version, error) = run_perry_update(&config.perry_binary).await;
                            let result = WorkerMessage::UpdateResult {
                                success,
                                old_version,
                                new_version,
                                error,
                            };
                            let _ = ws_tx.send(Message::Text(serde_json::to_string(&result).unwrap().into()));
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handle a single build job. Runs as a spawned task.
async fn handle_build(
    config: &WorkerConfig,
    ws_tx: &tokio::sync::mpsc::UnboundedSender<Message>,
    cancelled: &Arc<AtomicBool>,
    azure_config: Option<&crate::azure::AzureVmConfig>,
    job_id: String,
    manifest: serde_json::Value,
    credentials: serde_json::Value,
    tarball_url: String,
    artifact_upload_url: Option<String>,
    auth_token: Option<String>,
) {
    let manifest: crate::queue::job::BuildManifest = match serde_json::from_value(manifest) {
        Ok(m) => m,
        Err(e) => {
            let err_msg = format!("Invalid manifest: {e}");
            tracing::error!("{err_msg}");
            send_error(ws_tx, &job_id, &err_msg);
            send_complete(ws_tx, &job_id, false, 0.0);
            return;
        }
    };

    let credentials: crate::queue::job::BuildCredentials = match serde_json::from_value(credentials) {
        Ok(c) => c,
        Err(e) => {
            let err_msg = format!("Invalid credentials: {e}");
            tracing::error!("{err_msg}");
            send_error(ws_tx, &job_id, &err_msg);
            send_complete(ws_tx, &job_id, false, 0.0);
            return;
        }
    };

    let tarball_path = match download_tarball(&tarball_url, &job_id, auth_token.as_deref()).await {
        Ok(p) => p,
        Err(e) => {
            let err_msg = format!("Failed to download tarball: {e}");
            tracing::error!(job_id = %job_id, "{err_msg}");
            send_error(ws_tx, &job_id, &err_msg);
            send_complete(ws_tx, &job_id, false, 0.0);
            return;
        }
    };

    let build_target = manifest.targets.first().cloned().unwrap_or_else(|| "linux".into());

    let request = BuildRequest {
        manifest,
        credentials,
        tarball_path,
        job_id: job_id.clone(),
    };

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

    let build_config = config.clone();
    let cancelled_for_build = cancelled.clone();
    let (build_result_tx, build_result_rx) =
        tokio::sync::oneshot::channel::<Result<PathBuf, String>>();

    tokio::spawn(async move {
        let result = pipeline::execute_build(&request, &build_config, cancelled_for_build, progress_tx).await;
        std::fs::remove_file(&request.tarball_path).ok();
        let _ = build_result_tx.send(result);
    });

    let start = std::time::Instant::now();
    let mut build_result: Option<Result<PathBuf, String>> = None;
    tokio::pin!(build_result_rx);
    let mut build_done = false;
    let mut progress_done = false;

    loop {
        tokio::select! {
            biased;
            result = &mut build_result_rx, if !build_done => {
                build_result = result.ok();
                build_done = true;
                if progress_done { break; }
            }
            progress = progress_rx.recv(), if !progress_done => {
                match progress {
                    Some(msg) => {
                        let mut json_val = serde_json::to_value(&msg).unwrap_or_default();
                        if let serde_json::Value::Object(ref mut map) = json_val {
                            map.insert("job_id".into(), serde_json::Value::String(job_id.clone()));
                        }
                        let json = serde_json::to_string(&json_val).unwrap();
                        let _ = ws_tx.send(Message::Text(json.into()));
                    }
                    None => {
                        progress_done = true;
                        if build_done { break; }
                    }
                }
            }
        }
    }

    while let Ok(msg) = progress_rx.try_recv() {
        let mut json_val = serde_json::to_value(&msg).unwrap_or_default();
        if let serde_json::Value::Object(ref mut map) = json_val {
            map.insert("job_id".into(), serde_json::Value::String(job_id.clone()));
        }
        let json = serde_json::to_string(&json_val).unwrap();
        let _ = ws_tx.send(Message::Text(json.into()));
    }

    let duration_secs = start.elapsed().as_secs_f64();

    match build_result {
        Some(Ok(artifact_path)) => {
            let artifact_name = artifact_path.file_name().and_then(|n| n.to_str()).unwrap_or("artifact").to_string();
            let metadata = std::fs::metadata(&artifact_path).ok();
            let size = metadata.map(|m| m.len()).unwrap_or(0);
            let sha256 = compute_sha256(&artifact_path).unwrap_or_default();
            let target = match build_target.as_str() {
                "windows" => "windows-precompiled",
                "ios" => "ios-precompiled",
                "macos" => "macos-precompiled",
                other => other,
            };

            if let Some(ref upload_url) = artifact_upload_url {
                match upload_artifact(upload_url, &artifact_path, &artifact_name, &sha256, target, auth_token.as_deref()).await {
                    Ok(resp) => tracing::info!(job_id = %job_id, "Artifact uploaded: {}", resp),
                    Err(e) => {
                        tracing::error!(job_id = %job_id, "Artifact upload failed: {e}");
                        send_error(ws_tx, &job_id, &format!("Artifact upload failed: {e}"));
                    }
                }
            } else {
                let msg = serde_json::to_string(&serde_json::json!({
                    "type": "artifact_ready", "job_id": job_id, "target": target,
                    "path": artifact_path.to_string_lossy(), "artifact_name": artifact_name,
                    "sha256": sha256, "size": size,
                })).unwrap();
                let _ = ws_tx.send(Message::Text(msg.into()));
            }

            if build_target == "windows" {
                if let Some(azure) = azure_config {
                    tracing::info!(job_id = %job_id, "Starting Azure Windows VM for signing...");
                    match crate::azure::start_vm(azure).await {
                        Ok(()) => tracing::info!(job_id = %job_id, "Azure VM start triggered"),
                        Err(e) => tracing::warn!(job_id = %job_id, "Failed to start Azure VM: {e}"),
                    }
                }
            }

            std::fs::remove_file(&artifact_path).ok();

            let complete = serde_json::to_string(&serde_json::json!({
                "type": "complete", "job_id": job_id, "success": true, "duration_secs": duration_secs,
                "needs_finishing": match build_target.as_str() {
                    "windows" => Some("windows"),
                    "ios" => Some("ios"),
                    "macos" => Some("macos"),
                    _ => None,
                },
                "artifacts": [{"name": artifact_name, "size": size, "sha256": sha256}]
            })).unwrap();
            let _ = ws_tx.send(Message::Text(complete.into()));
            tracing::info!(job_id = %job_id, "Build completed in {:.1}s", duration_secs);
        }
        Some(Err(err_msg)) => {
            tracing::error!(job_id = %job_id, error = %err_msg, "Build failed");
            send_error(ws_tx, &job_id, &err_msg);
            send_complete(ws_tx, &job_id, false, duration_secs);
        }
        None => {
            tracing::error!(job_id = %job_id, "Build task panicked");
            send_complete(ws_tx, &job_id, false, duration_secs);
        }
    }
}

fn send_error(ws_tx: &tokio::sync::mpsc::UnboundedSender<Message>, job_id: &str, message: &str) {
    let json = serde_json::to_string(&serde_json::json!({
        "type": "error", "job_id": job_id, "code": "INTERNAL_ERROR", "message": message,
    })).unwrap();
    let _ = ws_tx.send(Message::Text(json.into()));
}

fn send_complete(ws_tx: &tokio::sync::mpsc::UnboundedSender<Message>, job_id: &str, success: bool, duration_secs: f64) {
    let json = serde_json::to_string(&serde_json::json!({
        "type": "complete", "job_id": job_id, "success": success, "duration_secs": duration_secs, "artifacts": []
    })).unwrap();
    let _ = ws_tx.send(Message::Text(json.into()));
}

fn compute_sha256(path: &PathBuf) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path).map_err(|e| format!("Failed to read artifact: {e}"))?;
    Ok(hex::encode(Sha256::digest(&data)))
}
