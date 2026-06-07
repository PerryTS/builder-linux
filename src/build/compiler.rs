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

/// If the project has a `package.json`, install its npm dependencies so
/// perry compile can resolve them. `perry publish` excludes `node_modules`
/// from the tarball, so the builder is responsible for re-materializing
/// them. Prefers `npm ci` (deterministic from lockfile); falls back to
/// `npm install` when no `package-lock.json` is present.
async fn run_npm_install(
    project_dir: &Path,
    progress: &UnboundedSender<ServerMessage>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), String> {
    if !project_dir.join("package.json").exists() {
        tracing::info!("no package.json — skipping npm install");
        return Ok(());
    }
    let has_lock = project_dir.join("package-lock.json").exists();
    let mut cmd = Command::new("npm");
    if has_lock {
        tracing::info!("running npm ci in {}", project_dir.display());
        cmd.args(["ci", "--no-audit", "--no-fund", "--prefer-offline"]);
    } else {
        tracing::warn!("no package-lock.json — using npm install in {}", project_dir.display());
        cmd.args(["install", "--no-audit", "--no-fund", "--prefer-offline"]);
    }
    cmd.current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_and_stream(cmd, progress, cancelled).await
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

    // `perry publish` excludes `node_modules` from the tarball, so the
    // builder must re-populate it before invoking the compiler. No-op for
    // projects without a package.json (pure-perry sources).
    run_npm_install(project_dir, progress, cancelled).await?;

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

    // Ensure LLVM tools are on PATH for perry's LLVM backend
    let path = std::env::var("PATH").unwrap_or_default();
    if Path::new("/usr/lib/llvm-18/bin").exists() && !path.contains("/usr/lib/llvm-18/bin") {
        cmd.env("PATH", format!("/usr/lib/llvm-18/bin:{path}"));
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

    // Pass through build environment variables needed for cross-compilation.
    // Set cargo linker for Android target so native lib builds use NDK linker,
    // not host ld. Also explicitly set CXX + AR with absolute NDK paths —
    // some cc-rs versions (e.g. the one oboe-sys 0.6.1 pulls) fail target-
    // specific env-var lookup and search PATH for `aarch64-linux-android-clang++`
    // (no API version), which NDK doesn't provide. Absolute paths sidestep
    // that. We anchor on NDK API 21 (broadly-compatible baseline); CC may
    // still point at a higher API for perry's own runtime build.
    if let Ok(cc) = std::env::var("CC_aarch64_linux_android") {
        cmd.arg("-e").arg(format!("CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER={cc}"));
    }
    if let Ok(ndk) = std::env::var("ANDROID_NDK_HOME") {
        let ndk_bin = format!("{ndk}/toolchains/llvm/prebuilt/linux-x86_64/bin");
        let cxx = format!("{ndk_bin}/aarch64-linux-android21-clang++");
        let cc_abs = format!("{ndk_bin}/aarch64-linux-android21-clang");
        let ar = format!("{ndk_bin}/llvm-ar");
        // Set both underscore- and dash-form keys; cc-rs/cargo normalize
        // differently across versions and oboe-sys's old cc-rs has been seen
        // to miss the underscore form.
        for var in &["CXX_aarch64_linux_android", "CXX_aarch64-linux-android"] {
            cmd.arg("-e").arg(format!("{var}={cxx}"));
        }
        for var in &["AR_aarch64_linux_android", "AR_aarch64-linux-android"] {
            cmd.arg("-e").arg(format!("{var}={ar}"));
        }
        // Don't override CC if the host service env already set it — perry's
        // own android build uses API 24. But if the user's project bumps
        // hit a "tool not found" again, set CC_aarch64-linux-android absolute too.
        if std::env::var("CC_aarch64-linux-android").is_err() {
            cmd.arg("-e").arg(format!("CC_aarch64-linux-android={cc_abs}"));
        }
    }
    for var in &[
        "ANDROID_HOME", "ANDROID_SDK_ROOT", "ANDROID_NDK_HOME",
        "PERRY_WINDOWS_SYSROOT",
        "CC_aarch64_linux_android", "AR_aarch64_linux_android",
        "PERRY_LLVM_BITCODE_LINK", "PERRY_LLVM_KEEP_IR",
    ] {
        if let Ok(val) = std::env::var(var) {
            cmd.arg("-e").arg(format!("{var}={val}"));
        }
    }

    // LLVM backend tools — set defaults pointing to LLVM 18 in container, allow host env overrides
    for (var, default) in &[
        ("PERRY_LLVM_CLANG", "/usr/lib/llvm-18/bin/clang"),
        ("PERRY_LLVM_LLVM_AS", "/usr/lib/llvm-18/bin/llvm-as"),
        ("PERRY_LLVM_LLVM_LINK", "/usr/lib/llvm-18/bin/llvm-link"),
        ("PERRY_LLVM_OPT", "/usr/lib/llvm-18/bin/opt"),
        ("PERRY_LLVM_LLC", "/usr/lib/llvm-18/bin/llc"),
    ] {
        let val = std::env::var(var).unwrap_or_else(|_| default.to_string());
        cmd.arg("-e").arg(format!("{var}={val}"));
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
    let ios_sysroot = std::env::var("PERRY_IOS_SYSROOT")
        .unwrap_or_else(|_| "/opt/apple-sysroot/ios".to_string());
    let macos_sysroot = std::env::var("PERRY_MACOS_SYSROOT")
        .unwrap_or_else(|_| "/opt/apple-sysroot/macos".to_string());
    let tvos_sysroot = std::env::var("PERRY_TVOS_SYSROOT")
        .unwrap_or_else(|_| ios_sysroot.clone());
    if std::path::Path::new(&ios_sysroot).exists() {
        cmd.arg("-v").arg(format!("{ios_sysroot}:{ios_sysroot}:ro"));
        cmd.arg("-e").arg(format!("PERRY_IOS_SYSROOT={ios_sysroot}"));
    }
    if std::path::Path::new(&macos_sysroot).exists() {
        cmd.arg("-v").arg(format!("{macos_sysroot}:{macos_sysroot}:ro"));
        cmd.arg("-e").arg(format!("PERRY_MACOS_SYSROOT={macos_sysroot}"));
    }
    if std::path::Path::new(&tvos_sysroot).exists() && tvos_sysroot != ios_sysroot {
        cmd.arg("-v").arg(format!("{tvos_sysroot}:{tvos_sysroot}:ro"));
        cmd.arg("-e").arg(format!("PERRY_TVOS_SYSROOT={tvos_sysroot}"));
    }

    // Per-target cross-compile env for cargo/cc-rs when perry compile
    // builds user-project native lib crates (e.g.
    // @bloomengine/engine/native/ios/). Without these, cc-rs falls back
    // to `xcrun` which doesn't exist on Linux. Mirrors the env the
    // worker's perry-update flow uses for perry's own crates
    // (worker.rs:305-384). Also disable aws-lc-sys jitterentropy —
    // its build pulls CoreServices/CoreServices.h which isn't in the
    // iOS/tvOS SDK sysroot (set globally on the host via systemd
    // drop-in; not inherited by docker containers).
    cmd.arg("-e").arg("AWS_LC_SYS_NO_JITTER_ENTROPY=1");
    // Per-target cross-compile env for cargo/cc-rs. CXX is set so user-project
    // native crates with C++ deps (Jolt, oboe-sys, etc.) link. BINDGEN_EXTRA_
    // CLANG_ARGS_<triple> is forwarded so aws-lc-sys's bindgen — which invokes
    // libclang directly, ignoring CFLAGS — can find Apple framework headers
    // (CoreServices/CoreServices.h etc.) inside the iOS/tvOS sysroot.
    // clang/clang++/clang-cl/llvm-lib are all in /usr/lib/llvm-18/bin
    // (mounted + on PATH).
    match target {
        Some("ios") => {
            let cflags = format!("--target=arm64-apple-ios17.0 -isysroot {ios_sysroot}");
            let bindgen_args = format!(
                "--sysroot={ios_sysroot} -isysroot {ios_sysroot} --target=arm64-apple-ios17.0"
            );
            cmd.arg("-e").arg("CC_aarch64_apple_ios=clang");
            cmd.arg("-e").arg("CXX_aarch64_apple_ios=clang++");
            cmd.arg("-e").arg(format!("CFLAGS_aarch64_apple_ios={cflags}"));
            cmd.arg("-e").arg(format!("CXXFLAGS_aarch64_apple_ios={cflags}"));
            cmd.arg("-e").arg(format!("BINDGEN_EXTRA_CLANG_ARGS_aarch64_apple_ios={bindgen_args}"));
            cmd.arg("-e").arg(format!("SDKROOT={ios_sysroot}"));
        }
        Some("macos") => {
            let cflags = format!("--target=arm64-apple-macos13.0 -isysroot {macos_sysroot}");
            let bindgen_args = format!(
                "--sysroot={macos_sysroot} -isysroot {macos_sysroot} --target=arm64-apple-macos13.0"
            );
            cmd.arg("-e").arg("CC_aarch64_apple_darwin=clang");
            cmd.arg("-e").arg("CXX_aarch64_apple_darwin=clang++");
            cmd.arg("-e").arg(format!("CFLAGS_aarch64_apple_darwin={cflags}"));
            cmd.arg("-e").arg(format!("CXXFLAGS_aarch64_apple_darwin={cflags}"));
            cmd.arg("-e").arg(format!(
                "BINDGEN_EXTRA_CLANG_ARGS_aarch64_apple_darwin={bindgen_args}"
            ));
            cmd.arg("-e").arg(format!("SDKROOT={macos_sysroot}"));
        }
        Some("tvos") => {
            let cflags = format!("--target=arm64-apple-tvos17.0 -isysroot {tvos_sysroot}");
            let bindgen_args = format!(
                "--sysroot={tvos_sysroot} -isysroot {tvos_sysroot} --target=arm64-apple-tvos17.0"
            );
            cmd.arg("-e").arg("CC_aarch64_apple_tvos=clang");
            cmd.arg("-e").arg("CXX_aarch64_apple_tvos=clang++");
            cmd.arg("-e").arg(format!("CFLAGS_aarch64_apple_tvos={cflags}"));
            cmd.arg("-e").arg(format!("CXXFLAGS_aarch64_apple_tvos={cflags}"));
            cmd.arg("-e").arg(format!("BINDGEN_EXTRA_CLANG_ARGS_aarch64_apple_tvos={bindgen_args}"));
            cmd.arg("-e").arg(format!("SDKROOT={tvos_sysroot}"));
        }
        Some("windows") => {
            // Windows MSVC cross-compile from Linux. cc-rs's default Windows
            // path invokes `lib.exe` (the MSVC archiver) by name, which doesn't
            // exist on Linux — minimp3-sys hits this. LLVM's `llvm-lib` is an
            // MSVC-compatible archiver and `clang-cl` is the MSVC-compatible
            // clang driver. Both ship in LLVM 18 (mounted + on PATH).
            //
            // xwin-fetched sysroot layout used here:
            //   $sysroot/crt/include              MSVC CRT headers
            //   $sysroot/sdk/include/{ucrt,um,shared}   Windows SDK headers
            let win_sysroot = std::env::var("PERRY_WINDOWS_SYSROOT")
                .unwrap_or_else(|_| "/opt/win-sysroot".to_string());
            let cflags = format!(
                "--target=x86_64-pc-windows-msvc /imsvc {ws}/crt/include /imsvc {ws}/sdk/include/ucrt /imsvc {ws}/sdk/include/um /imsvc {ws}/sdk/include/shared",
                ws = win_sysroot
            );
            for ar_var in &["AR_x86_64_pc_windows_msvc", "AR_x86_64-pc-windows-msvc"] {
                cmd.arg("-e").arg(format!("{ar_var}=llvm-lib"));
            }
            for cc_var in &["CC_x86_64_pc_windows_msvc", "CC_x86_64-pc-windows-msvc"] {
                cmd.arg("-e").arg(format!("{cc_var}=clang-cl"));
            }
            for cxx_var in &["CXX_x86_64_pc_windows_msvc", "CXX_x86_64-pc-windows-msvc"] {
                cmd.arg("-e").arg(format!("{cxx_var}=clang-cl"));
            }
            cmd.arg("-e").arg(format!("CFLAGS_x86_64_pc_windows_msvc={cflags}"));
            cmd.arg("-e").arg(format!("CXXFLAGS_x86_64_pc_windows_msvc={cflags}"));
        }
        Some("linux-arm64") | Some("linux-aarch64") => {
            // aarch64 Linux cross-compile from an x86_64 host. Point cc-rs at
            // the GNU cross toolchain so user-project native C/C++ deps build
            // for aarch64 instead of the host. perry's own final link also uses
            // aarch64-linux-gnu-gcc (see perry link/platform_cmd.rs), so the
            // build container must provide the `gcc-aarch64-linux-gnu` package.
            for cc_var in &["CC_aarch64_unknown_linux_gnu", "CC_aarch64-unknown-linux-gnu"] {
                cmd.arg("-e").arg(format!("{cc_var}=aarch64-linux-gnu-gcc"));
            }
            for cxx_var in &["CXX_aarch64_unknown_linux_gnu", "CXX_aarch64-unknown-linux-gnu"] {
                cmd.arg("-e").arg(format!("{cxx_var}=aarch64-linux-gnu-g++"));
            }
            for ar_var in &["AR_aarch64_unknown_linux_gnu", "AR_aarch64-unknown-linux-gnu"] {
                cmd.arg("-e").arg(format!("{ar_var}=aarch64-linux-gnu-ar"));
            }
            // cargo's own linker var for the aarch64 leg (build-deps / proc-macro
            // stay on host; only the target leg is redirected).
            cmd.arg("-e").arg(
                "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc",
            );
        }
        _ => {}
    }

    // npm cache: previously a shared host mount (~/.npm → /root/.npm), but
    // concurrent builds racing on the same cache produced "npm error
    // Invalid Version:" failures on this worker. Drop the shared mount —
    // each container now uses an ephemeral cache inside its writable layer.
    // Trade-off: re-downloads npm packages per build (~10-30s extra on the
    // first npm-using job per platform), but no races. If repeat-build
    // performance becomes important again, switch to a per-job subdir
    // (e.g. /tmp/npm-cache-<uuid>) instead of going back to the shared mount.

    // Build the perry compile command as a single shell line. Container
    // paths are all builder-controlled absolute paths so POSIX single-quoting
    // is sufficient. `perry publish` excludes node_modules from the tarball
    // (publish.rs:2643), so we materialize them here via `npm ci` (lockfile
    // present) or `npm install` (no lockfile) inside the same container
    // before running perry compile. No-op for projects without a package.json.
    fn shq(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    let mut perry_compile_cmd = format!(
        "exec {} compile {} -o {}",
        shq(&container_perry),
        shq(&container_entry),
        shq(&container_output),
    );
    if let Some(t) = target {
        perry_compile_cmd.push_str(&format!(" --target {}", shq(t)));
    }
    if let Some(ref features) = manifest.features {
        if !features.is_empty() {
            let features_str = features.join(",");
            tracing::info!("Passing --features {features_str} to perry compile (docker)");
            perry_compile_cmd.push_str(&format!(" --features {}", shq(&features_str)));
        }
    }

    let build_script = format!(
        "set -e\n\
         cd {project}\n\
         if [ -f package.json ]; then\n\
             if [ -f package-lock.json ]; then\n\
                 echo '[builder] npm ci'\n\
                 npm ci --no-audit --no-fund --prefer-offline\n\
             else\n\
                 echo '[builder] WARN: no package-lock.json — using npm install' >&2\n\
                 npm install --no-audit --no-fund --prefer-offline\n\
             fi\n\
         else\n\
             echo '[builder] no package.json — skipping npm step'\n\
         fi\n\
         {perry_compile}",
        project = shq(container_project),
        perry_compile = perry_compile_cmd,
    );

    cmd
        // Set working directory to project
        .arg("-w").arg(container_project)
        // Use the build image
        .arg(&config.docker_image)
        // sh -c wrapper that runs npm (if needed) then perry compile
        .arg("sh")
        .arg("-c")
        .arg(&build_script);

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
