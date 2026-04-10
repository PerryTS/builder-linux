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

    // Pass project features (e.g. ios-game-loop) to the compiler
    if let Some(ref features) = manifest.features {
        if !features.is_empty() {
            cmd.arg("--features").arg(features.join(","));
        }
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

    // Resolve perry binary and its directory (which also contains runtime libs)
    let perry_path = std::fs::canonicalize(perry_binary)
        .map_err(|e| format!("Failed to resolve perry binary path: {e}"))?;
    let perry_dir = perry_path.parent()
        .ok_or("Perry binary has no parent directory")?;
    // The target/ dir is one level up — mount it so find_library can resolve
    // libs from exe.parent().parent()/target/<triple>/release/
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
    // Mount the entire perry release dir (contains binary + runtime libs)
    // so find_library can resolve libs from exe.parent().join(name)
    let container_perry_dir = "/perry/release";
    let container_perry = format!("{}/perry", container_perry_dir);
    let container_entry = format!("{}/{}", container_project, manifest.entry);
    let container_output = format!("{}/{}", container_output_dir, output_filename);

    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        // Allow network for native lib cargo builds (crate downloads from crates.io).
        // Isolation relies on read-only mounts + resource limits + no-new-privileges.
        .arg("--network=host")
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
        // Mount the entire target dir at /perry — this makes the binary at
        // /perry/release/perry and cross-compilation libs at /perry/{triple}/release/
        // which matches how find_library resolves paths via exe.parent().parent()
        .arg("-v").arg(format!("{}:/perry:ro", target_dir.display()))
        // Mount Rust toolchain so native library builds work (cargo build inside projects)
        .arg("-v").arg(format!("{}:/rust/rustup:ro", std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{}/.rustup", std::env::var("HOME").unwrap_or_else(|_| "/root".into())))))
        .arg("-v").arg(format!("{}:/rust/cargo:ro", std::env::var("CARGO_HOME").unwrap_or_else(|_| format!("{}/.cargo", std::env::var("HOME").unwrap_or_else(|_| "/root".into())))))
        .arg("-e").arg("RUSTUP_HOME=/rust/rustup")
        .arg("-e").arg("CARGO_HOME=/tmp/cargo-home")
        .arg("-e").arg("PATH=/usr/lib/llvm-18/bin:/usr/local/bin:/rust/cargo/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin")
        // Rust toolchain + system LLVM shared libraries (needed by clang, lld-link, ld64.lld, rust-lld)
        .arg("-e").arg("LD_LIBRARY_PATH=/rust/rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib:/usr/lib/llvm-18/lib");

    // Pass through build environment variables needed for cross-compilation
    // Set cargo linker for Android target so native lib builds use NDK linker, not host ld
    if let Ok(cc) = std::env::var("CC_aarch64_linux_android") {
        cmd.arg("-e").arg(format!("CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER={cc}"));
    }
    for var in &[
        "ANDROID_HOME", "ANDROID_SDK_ROOT", "ANDROID_NDK_HOME",
        "PERRY_WINDOWS_SYSROOT",
        "CC_aarch64_linux_android", "AR_aarch64_linux_android",
        // LLVM backend: clang + bitcode link tool overrides
        "PERRY_LLVM_CLANG", "PERRY_LLVM_LLVM_AS", "PERRY_LLVM_LLVM_LINK",
        "PERRY_LLVM_OPT", "PERRY_LLVM_LLC",
        "PERRY_LLVM_BITCODE_LINK", "PERRY_LLVM_KEEP_IR",
    ] {
        if let Ok(val) = std::env::var(var) {
            cmd.arg("-e").arg(format!("{var}={val}"));
        }
    }

    // Mount Android NDK if configured (needed for Android cross-compilation)
    if let Ok(ndk) = std::env::var("ANDROID_NDK_HOME") {
        cmd.arg("-v").arg(format!("{ndk}:{ndk}:ro"));
    }
    // Mount Windows sysroot if configured
    if let Ok(sysroot) = std::env::var("PERRY_WINDOWS_SYSROOT") {
        cmd.arg("-v").arg(format!("{sysroot}:{sysroot}:ro"));
    }
    // Mount lld-link and ld64.lld if they exist (for Windows/Apple cross-compilation)
    for tool in &["lld-link", "ld64.lld"] {
        if let Ok(output) = std::process::Command::new("which").arg(tool).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    cmd.arg("-v").arg(format!("{path}:{path}:ro"));
                }
            }
        }
    }
    // Mount system LLVM shared libs and tools (needed by clang, ld64.lld, llvm-link, etc.)
    if std::path::Path::new("/usr/lib/llvm-18/lib").exists() {
        cmd.arg("-v").arg("/usr/lib/llvm-18/lib:/usr/lib/llvm-18/lib:ro");
    }
    if std::path::Path::new("/usr/lib/llvm-18/bin").exists() {
        cmd.arg("-v").arg("/usr/lib/llvm-18/bin:/usr/lib/llvm-18/bin:ro");
    }

    // Mount Apple SDK sysroot if configured (for iOS/macOS cross-compilation)
    if let Ok(sysroot) = std::env::var("PERRY_IOS_SYSROOT") {
        cmd.arg("-v").arg(format!("{sysroot}:{sysroot}:ro"));
        cmd.arg("-e").arg(format!("PERRY_IOS_SYSROOT={sysroot}"));
    }
    if let Ok(sysroot) = std::env::var("PERRY_MACOS_SYSROOT") {
        cmd.arg("-v").arg(format!("{sysroot}:{sysroot}:ro"));
        cmd.arg("-e").arg(format!("PERRY_MACOS_SYSROOT={sysroot}"));
    }
    if let Ok(sysroot) = std::env::var("PERRY_TVOS_SYSROOT") {
        cmd.arg("-v").arg(format!("{sysroot}:{sysroot}:ro"));
        cmd.arg("-e").arg(format!("PERRY_TVOS_SYSROOT={sysroot}"));
    }

    cmd
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

    // Pass project features (e.g. ios-game-loop) to the compiler
    if let Some(ref features) = manifest.features {
        if !features.is_empty() {
            let features_str = features.join(",");
            tracing::info!("Passing --features {features_str} to perry compile (docker)");
            cmd.arg("--features").arg(&features_str);
        }
    }

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_and_stream(cmd, progress, cancelled).await?;

    let ios_app_output = output_path.with_extension("app");
    if !output_path.exists() && !ios_app_output.exists() {
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
