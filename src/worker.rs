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

/// Run the perry update process: git pull + cargo build.
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

    let src_dir = std::path::Path::new(perry_binary)
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());

    let src_dir = match src_dir {
        Some(d) if d.join(".git").exists() => d,
        _ => {
            return (false, String::new(), Some("Cannot determine perry source directory from binary path".into()));
        }
    };

    tracing::info!(dir = %src_dir.display(), "Updating perry compiler...");

    // Clean stale git state from interrupted updates
    let _ = tokio::process::Command::new("find")
        .args([".git", "-name", "*.lock", "-delete"])
        .current_dir(src_dir).output().await;
    let _ = tokio::process::Command::new("rm")
        .args(["-rf", ".git/refs/remotes/origin", ".git/packed-refs"])
        .current_dir(src_dir).output().await;

    // Fetch + reset instead of pull to avoid stale ref issues
    let fetch = tokio::process::Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(src_dir)
        .output()
        .await;

    match fetch {
        Ok(ref o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            return (false, String::new(), Some(format!("git fetch failed: {stderr}")));
        }
        Err(e) => {
            return (false, String::new(), Some(format!("git fetch failed: {e}")));
        }
        _ => {}
    }

    let reset = tokio::process::Command::new("git")
        .args(["reset", "--hard", "origin/main"])
        .current_dir(src_dir)
        .output()
        .await;

    match reset {
        Ok(ref o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            return (false, String::new(), Some(format!("git reset failed: {stderr}")));
        }
        Err(e) => {
            return (false, String::new(), Some(format!("git reset failed: {e}")));
        }
        _ => {}
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    // Build packages one at a time to keep memory usage low on small VPS instances
    for pkg in &["perry", "perry-runtime", "perry-stdlib"] {
        let build = tokio::process::Command::new(&cargo)
            .args(["build", "--release", "-p", pkg])
            .current_dir(src_dir)
            .output()
            .await;

        match build {
            Ok(ref o) if !o.status.success() => {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                return (false, String::new(), Some(format!("cargo build -p {pkg} failed: {stderr}")));
            }
            Err(e) => {
                return (false, String::new(), Some(format!("cargo build -p {pkg} failed: {e}")));
            }
            _ => {}
        }
    }

    // Build Android-targeted libraries one at a time (memory-constrained server)
    // so perry's find_library resolves them from target/aarch64-linux-android/release/
    for pkg in &["perry-runtime", "perry-ui-android"] {
        let android_build = tokio::process::Command::new(&cargo)
            .args(["build", "--release", "-p", pkg, "--target", "aarch64-linux-android"])
            .current_dir(src_dir)
            .output()
            .await;

        match &android_build {
            Ok(o) if !o.status.success() => {
                tracing::warn!("Android {pkg} build failed (non-fatal): {}", String::from_utf8_lossy(&o.stderr));
            }
            Err(e) => {
                tracing::warn!("Android {pkg} build failed (non-fatal): {e}");
            }
            _ => {}
        }
    }

    // Build stdlib separately — exclude email feature to avoid openssl cross-compile
    let android_stdlib = tokio::process::Command::new(&cargo)
        .args(["build", "--release", "-p", "perry-stdlib", "--no-default-features",
               "--features", "http-server,http-client,database,crypto,compression,websocket,image,scheduler,ids,html-parser,rate-limit,validation",
               "--target", "aarch64-linux-android"])
        .current_dir(src_dir)
        .output()
        .await;

    match &android_stdlib {
        Ok(o) if !o.status.success() => {
            tracing::warn!("Android stdlib build failed (non-fatal): {}", String::from_utf8_lossy(&o.stderr));
        }
        Err(e) => {
            tracing::warn!("Android stdlib build failed (non-fatal): {e}");
        }
        _ => {
            tracing::info!("Android-targeted libraries built successfully");
        }
    }

    // Rebuild Windows .lib files on the Windows server and copy them here.
    // perry-stdlib/perry-runtime can't be cross-compiled locally (ring/cc-rs).
    update_windows_libs(src_dir).await;

    // Build perry-ui-windows rlib locally (cross-compile works for this crate).
    // The rlib is needed by strip_duplicate_objects_from_lib for proper dedup.
    let cargo = std::env::var("CARGO_HOME")
        .map(|h| format!("{h}/bin/cargo"))
        .unwrap_or_else(|_| "cargo".to_string());
    let ui_rlib = tokio::process::Command::new(&cargo)
        .args(["build", "--release", "-p", "perry-ui-windows", "--target", "x86_64-pc-windows-msvc"])
        .current_dir(src_dir)
        .output()
        .await;
    match &ui_rlib {
        Ok(o) if o.status.success() => {
            tracing::info!("Built perry-ui-windows rlib for Windows cross-compile dedup");
        }
        Ok(o) => {
            tracing::warn!("perry-ui-windows rlib build failed (non-fatal): {}", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or(""));
        }
        Err(e) => {
            tracing::warn!("perry-ui-windows rlib build failed (non-fatal): {e}");
        }
    }

    let new_version = get_perry_version(perry_binary).unwrap_or_default();
    tracing::info!(version = %new_version, "Perry update complete");
    (true, new_version, None)
}

/// Rebuild Windows runtime/stdlib/UI libs on the Windows build server
/// and copy them to the local cross-compilation target directory.
/// Uses SSH key auth (PERRY_WINDOWS_BUILD_HOST + PERRY_WINDOWS_BUILD_USER)
/// or password auth (+ PERRY_WINDOWS_BUILD_PASSWORD) to connect.
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
            "$env:PATH = 'C:\\Users\\{}\\.cargo\\bin;C:\\Program Files\\Git\\cmd;' + $env:PATH; ",
            "cd '{}'; ",
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

    for lib in &["perry_runtime.lib", "perry_stdlib.lib", "perry_ui_windows.lib"] {
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
        capabilities: vec!["linux".into(), "android".into(), "windows".into()],
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

    // Shared WS write channel — build tasks send messages here, main loop writes to WS
    let (ws_tx, mut ws_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // Per-job cancellation flags
    let cancel_flags: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let active_builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    loop {
        tokio::select! {
            biased;

            // Drain outbound WS messages from build tasks
            ws_msg = ws_rx.recv() => {
                match ws_msg {
                    Some(msg) => {
                        if let Err(e) = write.send(msg).await {
                            return Err(format!("Failed to send WS message: {e}"));
                        }
                    }
                    None => break,
                }
            }

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
                        let _ = write.send(Message::Pong(data)).await;
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
                            let _ = write.send(Message::Text(serde_json::to_string(&result).unwrap().into())).await;
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
