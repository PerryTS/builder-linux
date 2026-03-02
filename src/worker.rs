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

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubMessage {
    JobAssign {
        job_id: String,
        manifest: serde_json::Value,
        credentials: serde_json::Value,
        tarball_path: String,
    },
    Cancel {
        job_id: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerMessage {
    WorkerHello {
        capabilities: Vec<String>,
        name: String,
    },
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
    let hello = WorkerMessage::WorkerHello {
        capabilities: vec!["linux".into(), "android".into()],
        name: config.worker_name.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "worker".into())
        }),
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
                tarball_path,
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

                let request = BuildRequest {
                    manifest,
                    credentials,
                    tarball_path: PathBuf::from(tarball_path),
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

                        // Send artifact_ready to hub
                        let artifact_msg = serde_json::to_string(&serde_json::json!({
                            "type": "artifact_ready",
                            "job_id": job_id,
                            "path": artifact_path.to_string_lossy(),
                            "artifact_name": artifact_name,
                            "sha256": sha256,
                            "size": size,
                        }))
                        .unwrap();
                        let _ = write.send(Message::Text(artifact_msg.into())).await;

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
        }
    }

    Ok(())
}

fn compute_sha256(path: &PathBuf) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path).map_err(|e| format!("Failed to read artifact: {e}"))?;
    Ok(hex::encode(Sha256::digest(&data)))
}
