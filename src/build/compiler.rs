use crate::config::WorkerConfig;
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
    config: &WorkerConfig,
    project_dir: &Path,
    output_path: &Path,
    target: Option<&str>,
) -> Result<(), String> {
    let entry = project_dir.join(&manifest.entry);

    let canonical_project = project_dir
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize project dir: {e}"))?;
    let canonical_entry = entry
        .canonicalize()
        .map_err(|e| format!("Entry file not found or inaccessible: {e}"))?;
    if !canonical_entry.starts_with(&canonical_project) {
        return Err(format!(
            "Entry path escapes project directory: {}",
            manifest.entry
        ));
    }

    if config.docker_enabled {
        compile_in_docker(manifest, progress, cancelled, config, project_dir, output_path, target).await
    } else {
        compile_direct(&config.perry_binary, manifest, progress, cancelled, project_dir, output_path, target).await
    }
}

/// Run perry compile directly on the host (no isolation).
async fn compile_direct(
    perry_binary: &str,
    manifest: &BuildManifest,
    progress: &UnboundedSender<ServerMessage>,
    cancelled: &Arc<AtomicBool>,
    project_dir: &Path,
    output_path: &Path,
    target: Option<&str>,
) -> Result<(), String> {
    if target.is_some() {
        setup_target_symlink(perry_binary, project_dir)?;
    }

    let mut cmd = Command::new(perry_binary);
    cmd.arg("compile")
        .arg(project_dir.join(&manifest.entry))
        .arg("-o")
        .arg(output_path);

    if let Some(t) = target {
        cmd.arg("--target").arg(t);
    }

    cmd.current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_and_stream(cmd, progress, cancelled).await?;

    let ios_app_output = output_path.with_extension("app");
    if !output_path.exists() && !ios_app_output.exists() {
        return Err("Compiler produced no output binary".into());
    }

    Ok(())
}

/// Run perry compile inside a Docker container for full isolation.
/// - Project dir mounted read-only
/// - Output dir mounted writable
/// - Perry binary + libs mounted read-only from host
/// - No network access (--network=none)
/// - Container removed after build (--rm)
async fn compile_in_docker(
    manifest: &BuildManifest,
    progress: &UnboundedSender<ServerMessage>,
    cancelled: &Arc<AtomicBool>,
    config: &WorkerConfig,
    project_dir: &Path,
    output_path: &Path,
    target: Option<&str>,
) -> Result<(), String> {
    let perry_binary = &config.perry_binary;

    // Resolve perry binary and its parent dirs for mounting
    let perry_path = std::fs::canonicalize(perry_binary)
        .map_err(|e| format!("Failed to resolve perry binary path: {e}"))?;
    let perry_dir = perry_path.parent()
        .ok_or("Perry binary has no parent directory")?;
    // The target/ dir with runtime libs is one level up from the bin dir
    let target_dir = perry_dir.parent()
        .ok_or("Perry binary directory has no parent")?;

    let canonical_project = project_dir.canonicalize()
        .map_err(|e| format!("Failed to canonicalize project dir: {e}"))?;

    // Ensure output directory exists on host
    if let Some(output_parent) = output_path.parent() {
        std::fs::create_dir_all(output_parent)
            .map_err(|e| format!("Failed to create output dir: {e}"))?;
    }
    let canonical_output_parent = output_path.parent().unwrap()
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize output dir: {e}"))?;
    let output_filename = output_path.file_name()
        .ok_or("Output path has no filename")?
        .to_string_lossy();

    let container_project = "/build/project";
    let container_output_dir = "/build/output";
    let container_perry = "/perry/bin/perry";
    let container_target = "/perry/target";
    let container_entry = format!("{}/{}", container_project, manifest.entry);
    let container_output = format!("{}/{}", container_output_dir, output_filename);

    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("--network=none")
        // Memory limit to prevent abuse
        .arg("--memory=4g")
        .arg("--memory-swap=4g")
        // CPU limit
        .arg("--cpus=2")
        // No new privileges
        .arg("--security-opt").arg("no-new-privileges")
        // Run as root inside the container (project dir is owned by root on host;
        // isolation comes from network=none + read-only mounts, not user separation)
        .arg("--user").arg("0:0")
        // Mount project writable (perry writes .o files during compilation;
        // the project dir is a temp copy that gets cleaned up after the build)
        .arg("-v").arg(format!("{}:{}", canonical_project.display(), container_project))
        // Mount output dir writable
        .arg("-v").arg(format!("{}:{}:rw", canonical_output_parent.display(), container_output_dir))
        // Mount perry binary read-only
        .arg("-v").arg(format!("{}:{}:ro", perry_path.display(), container_perry))
        // Mount perry target dir (contains runtime libs) read-only
        .arg("-v").arg(format!("{}:{}:ro", target_dir.display(), container_target))
        // Set working directory to project
        .arg("-w").arg(container_project)
        // Use the build image
        .arg(&config.docker_image)
        // Run perry compile
        .arg(container_perry)
        .arg("compile")
        .arg(&container_entry)
        .arg("-o")
        .arg(&container_output);

    if let Some(t) = target {
        cmd.arg("--target").arg(t);
    }

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_and_stream(cmd, progress, cancelled).await?;

    if !output_path.exists() {
        return Err("Compiler produced no output binary".into());
    }

    Ok(())
}

/// Spawn a command, stream stdout/stderr to progress, wait for completion.
async fn run_and_stream(
    mut cmd: Command,
    progress: &UnboundedSender<ServerMessage>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn process: {e}"))?;

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
        .map_err(|e| format!("Failed to wait for process: {e}"))?;

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
