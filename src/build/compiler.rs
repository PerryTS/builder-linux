use crate::queue::job::BuildManifest;
use crate::ws::messages::{LogStream, ServerMessage, StageName};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

pub async fn compile(
    manifest: &BuildManifest,
    progress: &UnboundedSender<ServerMessage>,
    cancelled: &Arc<AtomicBool>,
    perry_binary: &str,
    project_dir: &Path,
    output_path: &Path,
    target: Option<&str>,
) -> Result<(), String> {
    let entry = project_dir.join(&manifest.entry);

    if target.is_some() {
        setup_target_symlink(perry_binary, project_dir)?;
    }

    let mut cmd = Command::new(perry_binary);
    cmd.arg("compile")
        .arg(&entry)
        .arg("-o")
        .arg(output_path);

    if let Some(t) = target {
        cmd.arg("--target").arg(t);
    }

    cmd.current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn perry: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let tx_stdout = progress.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        let mut lines = Vec::new();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = tx_stdout.send(ServerMessage::Log {
                stage: StageName::Compiling,
                line: line.clone(),
                stream: LogStream::Stdout,
            });
            lines.push(line);
        }
        lines
    });

    let tx_stderr = progress.clone();
    let cancelled_clone = cancelled.clone();
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut lines = Vec::new();
        while let Ok(Some(line)) = reader.next_line().await {
            if cancelled_clone.load(Ordering::Relaxed) {
                break;
            }
            let _ = tx_stderr.send(ServerMessage::Log {
                stage: StageName::Compiling,
                line: line.clone(),
                stream: LogStream::Stderr,
            });
            lines.push(line);
        }
        lines
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait for perry: {e}"))?;

    let stdout_lines = stdout_task.await.unwrap_or_default();
    let stderr_lines = stderr_task.await.unwrap_or_default();

    if cancelled.load(Ordering::Relaxed) {
        return Err("Build cancelled".into());
    }

    if !status.success() {
        let mut err_detail = format!(
            "perry compile exited with code {}",
            status.code().unwrap_or(-1)
        );
        if !stderr_lines.is_empty() {
            err_detail.push_str(&format!("\n{}", stderr_lines.join("\n")));
        }
        if !stdout_lines.is_empty() {
            err_detail.push_str(&format!("\n{}", stdout_lines.join("\n")));
        }
        return Err(err_detail);
    }

    let ios_app_output = output_path.with_extension("app");
    if !output_path.exists() && !ios_app_output.exists() {
        return Err("Compiler produced no output binary".into());
    }

    Ok(())
}

fn setup_target_symlink(perry_binary: &str, project_dir: &Path) -> Result<(), String> {
    let perry_path = Path::new(perry_binary);

    let perry_path = if perry_path.is_relative() {
        std::env::current_dir()
            .map_err(|e| format!("Failed to get CWD: {e}"))?
            .join(perry_path)
    } else {
        perry_path.to_path_buf()
    };

    if let Some(bin_dir) = perry_path.parent() {
        if let Some(target_dir) = bin_dir.parent() {
            let link_path = project_dir.join("target");
            if !link_path.exists() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(target_dir, &link_path)
                    .map_err(|e| format!("Failed to symlink target dir: {e}"))?;
            }
        }
    }

    Ok(())
}
