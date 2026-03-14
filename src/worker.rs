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

    let pull = tokio::process::Command::new("git")
        .arg("pull")
        .current_dir(src_dir)
        .output()
        .await;

    match pull {
        Ok(ref o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            return (false, String::new(), Some(format!("git pull failed: {stderr}")));
        }
        Err(e) => {
            return (false, String::new(), Some(format!("git pull failed: {e}")));
        }
        _ => {}
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let build = tokio::process::Command::new(&cargo)
        .args(["build", "--release", "-p", "perry", "-p", "perry-runtime", "-p", "perry-stdlib"])
        .current_dir(src_dir)
        .output()
        .await;

    match build {
        Ok(ref o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            return (false, String::new(), Some(format!("cargo build failed: {stderr}")));
        }
        Err(e) => {
            return (false, String::new(), Some(format!("cargo build failed: {e}")));
        }
        _ => {}
    }

    // Build Android UI library and copy next to perry binary
    let android_build = tokio::process::Command::new(&cargo)
        .args(["build", "--release", "-p", "perry-ui-android", "--target", "aarch64-linux-android"])
        .current_dir(src_dir)
        .output()
        .await;

    match android_build {
        Ok(ref o) if o.status.success() => {
            // Copy to target/release/ so perry can find it
            let android_lib = src_dir.join("target/aarch64-linux-android/release/libperry_ui_android.a");
            let dest = src_dir.join("target/release/libperry_ui_android.a");
            if android_lib.exists() {
                if let Err(e) = std::fs::copy(&android_lib, &dest) {
                    tracing::warn!("Failed to copy libperry_ui_android.a: {e}");
                }
            }
        }
        Ok(ref o) => {
            tracing::warn!("perry-ui-android build failed (non-fatal): {}", String::from_utf8_lossy(&o.stderr));
        }
        Err(e) => {
            tracing::warn!("perry-ui-android build failed (non-fatal): {e}");
        }
    }

    let new_version = get_perry_version(perry_binary).unwrap_or_default();
    tracing::info!(version = %new_version, "Perry update complete");
    (true, new_version, None)
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
    let (ws_stream, _) = connect_async(&config.hub_ws_url)
        .await
        .map_err(|e| format!("Failed to connect to hub: {e}"))?;

    let (mut write, mut read) = ws_stream.split();

    // Send worker_hello
    let perry_version = get_perry_version(&config.perry_binary);
    let hello = WorkerMessage::WorkerHello {
        capabilities: vec!["linux".into(), "android".into()],
        name: config.worker_name.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "worker".into())
        }),
        secret: config.hub_secret.clone(),
        perry_version,
    };

    write
        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .map_err(|e| format!("Failed to send worker_hello: {e}"))?;

    tracing::info!("Connected to hub, waiting for jobs...");

    // Track current cancellation flag
    let cancelled = Arc::new(AtomicBool::new(false));

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                return Err(format!("WebSocket error: {e}"));
            }
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
                tracing::info!(job_id = %job_id, "Received job assignment");

                // Reset cancellation flag
                cancelled.store(false, Ordering::Relaxed);

                // Parse manifest and credentials
                let manifest: crate::queue::job::BuildManifest =
                    match serde_json::from_value(manifest) {
                        Ok(m) => m,
                        Err(e) => {
                            let err_msg = format!("Invalid manifest: {e}");
                            tracing::error!("{err_msg}");
                            let error_json = serde_json::to_string(&ServerMessage::Error {
                                code: ErrorCode::InternalError,
                                message: err_msg,
                                stage: None,
                            })
                            .unwrap();
                            let _ = write.send(Message::Text(error_json.into())).await;
                            let complete_json =
                                serde_json::to_string(&serde_json::json!({
                                    "type": "complete",
                                    "job_id": job_id,
                                    "success": false,
                                    "duration_secs": 0.0,
                                    "artifacts": []
                                }))
                                .unwrap();
                            let _ = write.send(Message::Text(complete_json.into())).await;
                            continue;
                        }
                    };

                let credentials: crate::queue::job::BuildCredentials =
                    match serde_json::from_value(credentials) {
                        Ok(c) => c,
                        Err(e) => {
                            let err_msg = format!("Invalid credentials: {e}");
                            tracing::error!("{err_msg}");
                            let error_json = serde_json::to_string(&ServerMessage::Error {
                                code: ErrorCode::InternalError,
                                message: err_msg,
                                stage: None,
                            })
                            .unwrap();
                            let _ = write.send(Message::Text(error_json.into())).await;
                            let complete_json =
                                serde_json::to_string(&serde_json::json!({
                                    "type": "complete",
                                    "job_id": job_id,
                                    "success": false,
                                    "duration_secs": 0.0,
                                    "artifacts": []
                                }))
                                .unwrap();
                            let _ = write.send(Message::Text(complete_json.into())).await;
                            continue;
                        }
                    };

                // Download tarball from hub
                let tarball_path = match download_tarball(&tarball_url, &job_id, auth_token.as_deref()).await {
                    Ok(p) => p,
                    Err(e) => {
                        let err_msg = format!("Failed to download tarball: {e}");
                        tracing::error!(job_id = %job_id, "{err_msg}");
                        let error_json = serde_json::to_string(&ServerMessage::Error {
                            code: ErrorCode::InternalError,
                            message: err_msg,
                            stage: None,
                        })
                        .unwrap();
                        let _ = write.send(Message::Text(error_json.into())).await;
                        let complete_json =
                            serde_json::to_string(&serde_json::json!({
                                "type": "complete",
                                "job_id": job_id,
                                "success": false,
                                "duration_secs": 0.0,
                                "artifacts": []
                            }))
                            .unwrap();
                        let _ = write.send(Message::Text(complete_json.into())).await;
                        continue;
                    }
                };

                let build_target = manifest.targets.first().cloned().unwrap_or_else(|| "linux".into());

                let request = BuildRequest {
                    manifest,
                    credentials,
                    tarball_path,
                    job_id: job_id.clone(),
                };

                // Create progress sender that forwards to hub WS
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

                // Run build and forward progress, while also listening for cancel messages
                let build_config = config.clone();
                let cancelled_for_build = cancelled.clone();
                let (build_result_tx, build_result_rx) =
                    tokio::sync::oneshot::channel::<Result<PathBuf, String>>();

                // Spawn build task
                tokio::spawn(async move {
                    let result = pipeline::execute_build(
                        &request,
                        &build_config,
                        cancelled_for_build,
                        progress_tx,
                    )
                    .await;
                    // Clean up downloaded tarball
                    std::fs::remove_file(&request.tarball_path).ok();
                    let _ = build_result_tx.send(result);
                });

                // Select between progress messages, cancel messages from hub, and build completion
                let start = std::time::Instant::now();
                let mut build_result: Option<Result<PathBuf, String>> = None;

                // Pin the oneshot so we can borrow it across select iterations
                tokio::pin!(build_result_rx);
                let mut build_done = false;
                let mut progress_done = false;

                loop {
                    tokio::select! {
                        biased;

                        // Build completion (check first to avoid missing it)
                        result = &mut build_result_rx, if !build_done => {
                            build_result = result.ok();
                            build_done = true;
                            if progress_done {
                                break;
                            }
                        }

                        // Forward progress to hub
                        progress = progress_rx.recv(), if !progress_done => {
                            match progress {
                                Some(msg) => {
                                    // Add job_id to the message for hub routing
                                    let mut json_val = serde_json::to_value(&msg).unwrap_or_default();
                                    if let serde_json::Value::Object(ref mut map) = json_val {
                                        map.insert("job_id".into(), serde_json::Value::String(job_id.clone()));
                                    }
                                    let json = serde_json::to_string(&json_val).unwrap();
                                    let _ = write.send(Message::Text(json.into())).await;
                                }
                                None => {
                                    // Channel closed, build task done sending progress
                                    progress_done = true;
                                    if build_done {
                                        break;
                                    }
                                }
                            }
                        }

                        // Check for hub messages (cancel)
                        ws_msg = read.next() => {
                            match ws_msg {
                                Some(Ok(Message::Text(text))) => {
                                    if let Ok(hub_msg) = serde_json::from_str::<HubMessage>(&text) {
                                        if let HubMessage::Cancel { job_id: cancel_id } = hub_msg {
                                            if cancel_id == job_id {
                                                tracing::info!(job_id = %job_id, "Cancelling build");
                                                cancelled.store(true, Ordering::Relaxed);
                                            }
                                        }
                                    }
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = write.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    // Hub disconnected
                                    return Err("Hub disconnected during build".into());
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // Drain remaining progress messages
                while let Ok(msg) = progress_rx.try_recv() {
                    let mut json_val = serde_json::to_value(&msg).unwrap_or_default();
                    if let serde_json::Value::Object(ref mut map) = json_val {
                        map.insert("job_id".into(), serde_json::Value::String(job_id.clone()));
                    }
                    let json = serde_json::to_string(&json_val).unwrap();
                    let _ = write.send(Message::Text(json.into())).await;
                }

                let duration_secs = start.elapsed().as_secs_f64();

                match build_result {
                    Some(Ok(artifact_path)) => {
                        // Compute artifact metadata
                        let artifact_name = artifact_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("artifact")
                            .to_string();
                        let metadata = std::fs::metadata(&artifact_path).ok();
                        let size = metadata.map(|m| m.len()).unwrap_or(0);
                        let sha256 = compute_sha256(&artifact_path).unwrap_or_default();
                        let target = build_target.as_str();

                        // Upload artifact to hub via HTTP (hub notifies CLI clients)
                        if let Some(ref upload_url) = artifact_upload_url {
                            match upload_artifact(upload_url, &artifact_path, &artifact_name, &sha256, target, auth_token.as_deref()).await {
                                Ok(resp) => {
                                    tracing::info!(job_id = %job_id, "Artifact uploaded: {}", resp);
                                }
                                Err(e) => {
                                    tracing::error!(job_id = %job_id, "Artifact upload failed: {e}");
                                    let error_msg = serde_json::to_string(&serde_json::json!({
                                        "type": "error",
                                        "job_id": job_id,
                                        "code": "INTERNAL_ERROR",
                                        "message": format!("Artifact upload failed: {e}"),
                                    }))
                                    .unwrap();
                                    let _ = write.send(Message::Text(error_msg.into())).await;
                                }
                            }
                        } else {
                            // Fallback: send artifact_ready via WS (self-hosted / same-machine)
                            let artifact_msg = serde_json::to_string(&serde_json::json!({
                                "type": "artifact_ready",
                                "job_id": job_id,
                                "target": target,
                                "path": artifact_path.to_string_lossy(),
                                "artifact_name": artifact_name,
                                "sha256": sha256,
                                "size": size,
                            }))
                            .unwrap();
                            let _ = write.send(Message::Text(artifact_msg.into())).await;
                        }

                        // Clean up local artifact file
                        std::fs::remove_file(&artifact_path).ok();

                        // Send complete
                        let complete_msg = serde_json::to_string(&serde_json::json!({
                            "type": "complete",
                            "job_id": job_id,
                            "success": true,
                            "duration_secs": duration_secs,
                            "artifacts": [{
                                "name": artifact_name,
                                "size": size,
                                "sha256": sha256,
                            }]
                        }))
                        .unwrap();
                        let _ = write.send(Message::Text(complete_msg.into())).await;

                        tracing::info!(job_id = %job_id, "Build completed in {:.1}s", duration_secs);
                    }
                    Some(Err(err_msg)) => {
                        tracing::error!(job_id = %job_id, error = %err_msg, "Build failed");
                        let error_msg = serde_json::to_string(&serde_json::json!({
                            "type": "error",
                            "job_id": job_id,
                            "code": "INTERNAL_ERROR",
                            "message": err_msg,
                            "stage": serde_json::Value::Null,
                        }))
                        .unwrap();
                        let _ = write.send(Message::Text(error_msg.into())).await;

                        let complete_msg = serde_json::to_string(&serde_json::json!({
                            "type": "complete",
                            "job_id": job_id,
                            "success": false,
                            "duration_secs": duration_secs,
                            "artifacts": []
                        }))
                        .unwrap();
                        let _ = write.send(Message::Text(complete_msg.into())).await;
                    }
                    None => {
                        tracing::error!(job_id = %job_id, "Build task panicked");
                        let complete_msg = serde_json::to_string(&serde_json::json!({
                            "type": "complete",
                            "job_id": job_id,
                            "success": false,
                            "duration_secs": duration_secs,
                            "artifacts": []
                        }))
                        .unwrap();
                        let _ = write.send(Message::Text(complete_msg.into())).await;
                    }
                }
            }

            HubMessage::Cancel { job_id } => {
                tracing::info!(job_id = %job_id, "Cancel request (no active build for this job)");
            }

            HubMessage::UpdatePerry {} => {
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

    Ok(())
}

fn compute_sha256(path: &PathBuf) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path).map_err(|e| format!("Failed to read artifact: {e}"))?;
    Ok(hex::encode(Sha256::digest(&data)))
}
