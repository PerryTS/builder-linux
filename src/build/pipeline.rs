use crate::build::assets::{generate_android_icons, generate_ico, encode_png};
use crate::build::cleanup::{cleanup_tmpdir, create_build_tmpdir};
use crate::build::compiler;
use crate::build::validate;
use crate::config::WorkerConfig;
use crate::package::{android, linux};
use crate::package::windows as win_package;
use crate::publish::playstore;
use crate::queue::job::{BuildCredentials, BuildManifest};
use crate::signing::android as android_signing;
use crate::ws::messages::{ServerMessage, StageName};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

/// Simplified build request for the worker (no queue/broadcast internals)
pub struct BuildRequest {
    pub manifest: BuildManifest,
    pub credentials: BuildCredentials,
    pub tarball_path: PathBuf,
    pub job_id: String,
}

/// Progress sender type alias
type ProgressSender = UnboundedSender<ServerMessage>;

pub async fn execute_build(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: Arc<AtomicBool>,
    progress: ProgressSender,
) -> Result<PathBuf, String> {
    validate::validate_manifest(&request.manifest)?;

    let tmpdir = create_build_tmpdir().map_err(|e| format!("Failed to create tmpdir: {e}"))?;

    let result = run_pipeline(request, config, &cancelled, &progress, &tmpdir).await;

    // Always clean up build tmpdir
    cleanup_tmpdir(&tmpdir);

    result
}

async fn run_pipeline(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
) -> Result<PathBuf, String> {
    let target = determine_target(&request.manifest.targets);

    // Stage 1: Extract tarball
    send_stage(progress, StageName::Extracting, "Extracting project archive");
    check_cancelled(cancelled)?;
    let project_dir = tmpdir.join("project");
    std::fs::create_dir_all(&project_dir)
        .map_err(|e| format!("Failed to create project dir: {e}"))?;
    extract_tarball(&request.tarball_path, &project_dir)?;
    send_progress(progress, StageName::Extracting, 100, None);

    // Stage 2: Compile
    send_stage(progress, StageName::Compiling, "Compiling TypeScript to native");
    check_cancelled(cancelled)?;
    let binary_name = if target == BuildTarget::Windows {
        format!("{}.exe", request.manifest.app_name)
    } else {
        request.manifest.app_name.clone()
    };
    let binary_path = tmpdir.join("output").join(&binary_name);
    std::fs::create_dir_all(binary_path.parent().unwrap())
        .map_err(|e| format!("Failed to create output dir: {e}"))?;

    let compiler_target = match target {
        BuildTarget::Android => Some("android"),
        BuildTarget::Windows => Some("windows"),
        BuildTarget::Ios => Some("ios"),
        BuildTarget::Macos => Some("macos"),
        BuildTarget::Linux => None, // native compilation on Linux host
    };
    compiler::compile(
        &request.manifest,
        progress,
        cancelled,
        config,
        &project_dir,
        &binary_path,
        compiler_target,
    )
    .await?;

    // For iOS, the compiler may produce a .app directory instead of a flat binary
    let actual_binary = if target == BuildTarget::Ios {
        let app_output = binary_path.with_extension("app");
        let inner = app_output.join(&request.manifest.app_name);
        if inner.exists() {
            inner
        } else if binary_path.exists() {
            binary_path.clone()
        } else {
            return Err("Compiler produced no output binary for iOS".into());
        }
    } else {
        if !binary_path.exists() {
            return Err(format!("Compiler produced no output binary at {}", binary_path.display()));
        }
        binary_path.clone()
    };
    send_progress(progress, StageName::Compiling, 100, None);

    match target {
        BuildTarget::Linux => {
            run_linux_pipeline(request, cancelled, progress, tmpdir, &actual_binary, &project_dir)
                .await
        }
        BuildTarget::Android => {
            run_android_pipeline(request, config, cancelled, progress, tmpdir, &actual_binary, &project_dir)
                .await
        }
        BuildTarget::Windows => {
            run_windows_pipeline(request, config, cancelled, progress, tmpdir, &actual_binary, &project_dir)
                .await
        }
        BuildTarget::Ios => {
            run_ios_pipeline(request, cancelled, progress, tmpdir, &actual_binary, &project_dir)
                .await
        }
        BuildTarget::Macos => {
            run_macos_pipeline(request, cancelled, progress, tmpdir, &actual_binary, &project_dir)
                .await
        }
    }
}

async fn run_linux_pipeline(
    request: &BuildRequest,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    binary_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // Stage 3: Generate assets (icon → PNG)
    send_stage(progress, StageName::GeneratingAssets, "Generating app icon");
    check_cancelled(cancelled)?;
    let icon_png_path = tmpdir.join("icon_256.png");
    if let Some(ref icon_name) = request.manifest.icon {
        let icon_src = project_dir.join(icon_name);
        if icon_src.exists() {
            // Resize icon to 256x256 PNG for Linux desktop
            let img = image::open(&icon_src)
                .map_err(|e| format!("Failed to open icon: {e}"))?;
            let resized = img.resize_exact(256, 256, image::imageops::FilterType::Lanczos3);
            let png_bytes = encode_png(&resized)?;
            std::fs::write(&icon_png_path, &png_bytes)
                .map_err(|e| format!("Write icon PNG: {e}"))?;
        }
    }
    send_progress(progress, StageName::GeneratingAssets, 100, None);

    // Stage 4: Bundle (AppImage / .deb / tar.gz)
    let format = linux::LinuxFormat::from_str_or_default(
        request.manifest.linux_format.as_deref(),
    );
    let format_label = format.extension();
    send_stage(
        progress,
        StageName::Bundling,
        &format!("Creating Linux package ({format_label})"),
    );
    check_cancelled(cancelled)?;

    let icon_opt = if icon_png_path.exists() {
        Some(icon_png_path.as_path())
    } else {
        None
    };
    let artifact = linux::package(&request.manifest, binary_path, icon_opt, format, tmpdir)?;
    send_progress(progress, StageName::Bundling, 100, None);

    // Stage 5: Signing (no-op for now)
    send_stage(progress, StageName::Signing, "Skipping signing (not required for Linux)");
    send_progress(progress, StageName::Signing, 100, None);

    // Copy artifact to stable location
    let ext = format.extension();
    let artifact_path = copy_artifact(&artifact, &request.manifest.app_name, &request.job_id, ext)?;
    Ok(artifact_path)
}

async fn run_windows_pipeline(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    binary_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // Stage 3: Generate assets (ICO)
    send_stage(progress, StageName::GeneratingAssets, "Generating Windows icon");
    check_cancelled(cancelled)?;
    let ico_path = tmpdir.join("app.ico");
    if let Some(ref icon_name) = request.manifest.icon {
        let icon_src = project_dir.join(icon_name);
        if icon_src.exists() {
            generate_ico(&icon_src, &ico_path)?;
        }
    }
    send_progress(progress, StageName::GeneratingAssets, 100, None);

    // Stage 4: Bundle — scan DLLs and create precompiled bundle
    send_stage(progress, StageName::Bundling, "Creating precompiled bundle for Windows");
    check_cancelled(cancelled)?;

    let dll_dir = tmpdir.join("dlls");
    let copied_dlls = win_package::scan_and_copy_dlls(binary_path, &dll_dir)?;
    if !copied_dlls.is_empty() {
        tracing::info!("Bundled {} non-system DLLs", copied_dlls.len());
    }

    let perry_version = get_perry_version(&config.perry_binary);
    let ico_opt = if ico_path.exists() { Some(ico_path.as_path()) } else { None };
    let dll_opt = if dll_dir.exists() { Some(dll_dir.as_path()) } else { None };
    let bundle_path = win_package::create_precompiled_bundle(
        &request.manifest,
        binary_path,
        ico_opt,
        dll_opt,
        &perry_version,
        tmpdir,
    )?;
    send_progress(progress, StageName::Bundling, 100, None);

    // Stage 5: Signing (skipped — Windows worker will sign)
    send_stage(progress, StageName::Signing, "Skipping signing (deferred to Windows worker)");
    send_progress(progress, StageName::Signing, 100, None);

    // Copy artifact to stable location
    let artifact_path = copy_artifact(&bundle_path, &request.manifest.app_name, &request.job_id, "tar.gz")?;
    Ok(artifact_path)
}

async fn run_android_pipeline(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    so_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // Stage 3: Generate Android assets (icons)
    send_stage(
        progress,
        StageName::GeneratingAssets,
        "Generating Android app icons",
    );
    check_cancelled(cancelled)?;
    let icons_dir = tmpdir.join("android_icons");
    if let Some(ref icon_name) = request.manifest.icon {
        let icon_src = project_dir.join(icon_name);
        if icon_src.exists() {
            generate_android_icons(&icon_src, &icons_dir)?;
        }
    }
    send_progress(progress, StageName::GeneratingAssets, 100, None);

    // Stage 4: Bundle — Create Android Gradle project and build APK
    send_stage(
        progress,
        StageName::Bundling,
        "Creating Android project and building APK",
    );
    check_cancelled(cancelled)?;

    let keystore_path = if let Some(ref b64) = request.credentials.android_keystore_base64 {
        let decoded = base64_decode(b64)?;
        let p = tmpdir.join("release.keystore");
        std::fs::write(&p, decoded)
            .map_err(|e| format!("Failed to write keystore: {e}"))?;
        Some(p)
    } else {
        None
    };

    let icons_opt = if icons_dir.exists() {
        Some(icons_dir.as_path())
    } else {
        None
    };

    let android_project = android::create_android_project(
        &request.manifest,
        &config.perry_binary,
        so_path,
        icons_opt,
        tmpdir,
    )?;

    let is_playstore = request.manifest.android_distribute.as_deref() == Some("playstore");

    let (gradle_tx, _) = tokio::sync::broadcast::channel(256);
    let artifact_path = if is_playstore {
        android::build_aab(&android_project, Some(&gradle_tx)).await?
    } else {
        android::build_apk(&android_project, true, Some(&gradle_tx)).await?
    };
    send_progress(progress, StageName::Bundling, 100, None);

    // Stage 5: Sign
    send_stage(progress, StageName::Signing, "Signing Android artifact");
    check_cancelled(cancelled)?;

    let final_artifact = if let Some(ref ks_path) = keystore_path {
        let ks_pass = request
            .credentials
            .android_keystore_password
            .as_deref()
            .unwrap_or("");
        let key_alias = request
            .credentials
            .android_key_alias
            .as_deref()
            .unwrap_or("key0");
        let key_pass = request
            .credentials
            .android_key_password
            .as_deref()
            .unwrap_or(ks_pass);

        if is_playstore {
            android_signing::sign_aab(&artifact_path, ks_path, ks_pass, key_alias, key_pass)
                .await?;
            artifact_path.clone()
        } else {
            android_signing::sign_apk(&artifact_path, ks_path, ks_pass, key_alias, key_pass)
                .await?
        }
    } else if is_playstore {
        return Err(
            "Google Play requires a signed bundle but no Android keystore was provided. \
             Generate one with: keytool -genkey -v -keystore release.keystore -alias key0 -keyalg RSA -keysize 2048 -validity 10000"
                .into(),
        );
    } else {
        let _ = progress.send(ServerMessage::Log {
            stage: StageName::Signing,
            line: "No keystore provided — skipping signing (APK will be unsigned)".into(),
            stream: crate::ws::messages::LogStream::Stderr,
        });
        artifact_path.clone()
    };

    if let Some(ref ks_path) = keystore_path {
        std::fs::remove_file(ks_path).ok();
    }
    send_progress(progress, StageName::Signing, 100, None);

    // Stage 6: Packaging
    send_stage(progress, StageName::Packaging, "Finalizing Android package");
    send_progress(progress, StageName::Packaging, 100, None);

    // Stage 7: Publishing
    if is_playstore {
        send_stage(progress, StageName::Publishing, "Uploading to Google Play");
        check_cancelled(cancelled)?;

        let play_track = request
            .manifest
            .android_distribute
            .as_deref()
            .and_then(|d| {
                if d == "playstore" { Some("internal") } else { None }
            })
            .unwrap_or("internal");

        match playstore::upload_to_playstore(
            &final_artifact,
            &request.manifest.bundle_id,
            request.credentials.google_play_service_account_json.as_deref(),
            play_track,
        ).await {
            Ok(result) => {
                let _ = progress.send(ServerMessage::Published {
                    platform: "android".into(),
                    message: result.message,
                    url: None,
                });
            }
            Err(e) => {
                let _ = progress.send(ServerMessage::Log {
                    stage: StageName::Publishing,
                    line: format!("Play Store upload skipped: {e}"),
                    stream: crate::ws::messages::LogStream::Stderr,
                });
            }
        }
        send_progress(progress, StageName::Publishing, 100, None);
    } else {
        send_stage(
            progress,
            StageName::Publishing,
            "Skipping store upload (distribute not set to playstore)",
        );
        send_progress(progress, StageName::Publishing, 100, None);
    }

    let ext = if is_playstore { "aab" } else { "apk" };
    let artifact_path =
        copy_artifact(&final_artifact, &request.manifest.app_name, &request.job_id, ext)?;
    Ok(artifact_path)
}

/// Copy artifact to a stable location (outside the build tmpdir that gets cleaned up)
/// iOS cross-compilation pipeline: use compiler's .app bundle → create .ipa
/// The perry compiler already generates a complete .app with Info.plist.
/// We just copy icons into it and package as .ipa.
/// Signing + App Store upload deferred to Mac worker.
async fn run_ios_pipeline(
    request: &BuildRequest,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    binary_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // The compiler produces a .app directory — use it directly
    let compiler_app = binary_path.parent()
        .and_then(|p| {
            // binary_path is inside the .app (e.g. output/Name.app/Name)
            if p.extension().map_or(false, |e| e == "app") {
                Some(p.to_path_buf())
            } else {
                // Try sibling .app
                let app = p.join(format!("{}.app", request.manifest.app_name));
                if app.exists() { Some(app) } else { None }
            }
        });
    let app_path = compiler_app.unwrap_or_else(|| tmpdir.join(format!("{}.app", request.manifest.app_name)));

    // Stage 3: Copy icons into the compiler's .app bundle
    send_stage(progress, StageName::GeneratingAssets, "Generating iOS icons");
    check_cancelled(cancelled)?;
    eprintln!("[ios-pipeline] app_path={} exists={}", app_path.display(), app_path.exists());
    eprintln!("[ios-pipeline] manifest.icon={:?}", request.manifest.icon);
    if let Some(ref icon_name) = request.manifest.icon {
        let icon_src = project_dir.join(icon_name);
        eprintln!("[ios-pipeline] icon_src={} exists={}", icon_src.display(), icon_src.exists());
        if icon_src.exists() {
            if let Ok(img) = image::open(&icon_src) {
                // Standard iOS icon naming: AppIcon{pt}x{pt}@{scale}x.png
                for (size, name) in &[
                    (1024u32, "Icon-1024.png"),
                    (180, "AppIcon60x60@3x.png"),   // iPhone @3x
                    (120, "AppIcon60x60@2x.png"),   // iPhone @2x
                    (152, "AppIcon76x76@2x.png"),   // iPad @2x
                    (76, "AppIcon76x76.png"),        // iPad @1x
                ] {
                    let resized = img.resize_exact(*size, *size, image::imageops::FilterType::Lanczos3);
                    let dest = app_path.join(name);
                    match resized.save(&dest) {
                        Ok(_) => eprintln!("[ios-pipeline] Saved icon {name} ({size}x{size}) to {}", dest.display()),
                        Err(e) => eprintln!("[ios-pipeline] Failed to save icon {name}: {e}"),
                    }
                }
            } else {
                eprintln!("[ios-pipeline] Failed to open icon image");
            }
        }
    }
    send_progress(progress, StageName::GeneratingAssets, 100, None);

    // Info.plist is already generated by the perry compiler inside the .app bundle
    // with all required DT* keys, CFBundleIconName, orientations, etc.

    // Stage 4: Create .ipa (zip Payload/App.app)
    send_stage(progress, StageName::Packaging, "Creating .ipa archive");
    check_cancelled(cancelled)?;
    let ipa_path = tmpdir.join(format!("{}.ipa", request.manifest.app_name));
    {
        let file = std::fs::File::create(&ipa_path)
            .map_err(|e| format!("Failed to create .ipa: {e}"))?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        // Walk the .app directory and add to Payload/
        let app_name = format!("{}.app", request.manifest.app_name);
        for entry in walkdir::WalkDir::new(&app_path) {
            let entry = entry.map_err(|e| format!("Walk error: {e}"))?;
            let path = entry.path();
            let relative = path.strip_prefix(&app_path).unwrap_or(path);
            let zip_path = format!("Payload/{}/{}", app_name, relative.display());

            if path.is_file() {
                zip.start_file(&zip_path, options)
                    .map_err(|e| format!("Zip error: {e}"))?;
                let data = std::fs::read(path)
                    .map_err(|e| format!("Read error: {e}"))?;
                use std::io::Write;
                zip.write_all(&data)
                    .map_err(|e| format!("Zip write error: {e}"))?;
            } else if path.is_dir() && path != app_path.as_path() {
                zip.add_directory(&zip_path, options)
                    .map_err(|e| format!("Zip dir error: {e}"))?;
            }
        }
        zip.finish().map_err(|e| format!("Zip finish error: {e}"))?;
    }
    send_progress(progress, StageName::Packaging, 100, None);

    // Stage 6: Signing (deferred to Mac worker)
    send_stage(progress, StageName::Signing, "Skipping signing (deferred to Mac worker)");
    send_progress(progress, StageName::Signing, 100, None);

    let artifact_path = copy_artifact(&ipa_path, &request.manifest.app_name, &request.job_id, "ipa")?;
    Ok(artifact_path)
}

/// macOS cross-compilation pipeline: compile → create .app bundle → tar.gz
/// Signing + notarization + DMG/pkg + App Store upload deferred to Mac worker.
async fn run_macos_pipeline(
    request: &BuildRequest,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    binary_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // Stage 3: Generate assets (icon → PNG, no ICNS on Linux)
    send_stage(progress, StageName::GeneratingAssets, "Generating macOS icon");
    check_cancelled(cancelled)?;
    let icon_png_path = tmpdir.join("icon.png");
    if let Some(ref icon_name) = request.manifest.icon {
        let icon_src = project_dir.join(icon_name);
        if icon_src.exists() {
            // Resize to 1024x1024 PNG (Mac worker will convert to .icns)
            if let Ok(img) = image::open(&icon_src) {
                let resized = img.resize_exact(1024, 1024, image::imageops::FilterType::Lanczos3);
                resized.save(&icon_png_path).ok();
            }
        }
    }
    send_progress(progress, StageName::GeneratingAssets, 100, None);

    // Stage 4: Create .app bundle
    send_stage(progress, StageName::Bundling, "Creating macOS .app bundle");
    check_cancelled(cancelled)?;
    let app_path = tmpdir.join(format!("{}.app", request.manifest.app_name));
    let contents = app_path.join("Contents");
    let macos_dir = contents.join("MacOS");
    let resources_dir = contents.join("Resources");
    std::fs::create_dir_all(&macos_dir)
        .map_err(|e| format!("Failed to create MacOS dir: {e}"))?;
    std::fs::create_dir_all(&resources_dir)
        .map_err(|e| format!("Failed to create Resources dir: {e}"))?;

    // Copy binary
    let dest_binary = macos_dir.join(&request.manifest.app_name);
    std::fs::copy(binary_path, &dest_binary)
        .map_err(|e| format!("Failed to copy binary: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest_binary, std::fs::Permissions::from_mode(0o755)).ok();
    }

    // Copy icon PNG (Mac worker will convert to .icns if needed)
    if icon_png_path.exists() {
        std::fs::copy(&icon_png_path, resources_dir.join("AppIcon.png")).ok();
    }

    // Generate Info.plist
    let bundle_id = if request.manifest.bundle_id.is_empty() { "com.example.app" } else { &request.manifest.bundle_id };
    let version = request.manifest.short_version.as_deref()
        .or(Some(&request.manifest.version))
        .unwrap_or("1.0.0");
    let build_number = request.manifest.version.as_str();
    let info_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{}</string>
    <key>CFBundleIdentifier</key>
    <string>{}</string>
    <key>CFBundleName</key>
    <string>{}</string>
    <key>CFBundleShortVersionString</key>
    <string>{}</string>
    <key>CFBundleVersion</key>
    <string>{}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>"#,
        request.manifest.app_name, bundle_id, request.manifest.app_name, version, build_number
    );
    std::fs::write(contents.join("Info.plist"), &info_plist)
        .map_err(|e| format!("Failed to write Info.plist: {e}"))?;
    send_progress(progress, StageName::Bundling, 100, None);

    // Stage 5: Package as tar.gz (Mac worker will create .dmg/.pkg)
    send_stage(progress, StageName::Packaging, "Creating precompiled bundle for macOS");
    check_cancelled(cancelled)?;
    let tarball_path = tmpdir.join(format!("{}-precompiled.tar.gz", request.manifest.app_name));
    {
        let file = std::fs::File::create(&tarball_path)
            .map_err(|e| format!("Failed to create tarball: {e}"))?;
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all(format!("{}.app", request.manifest.app_name), &app_path)
            .map_err(|e| format!("Failed to add .app to tarball: {e}"))?;
        tar.finish().map_err(|e| format!("Failed to finish tarball: {e}"))?;
    }
    send_progress(progress, StageName::Packaging, 100, None);

    // Stage 6: Signing (deferred to Mac worker)
    send_stage(progress, StageName::Signing, "Skipping signing (deferred to Mac worker)");
    send_progress(progress, StageName::Signing, 100, None);

    let artifact_path = copy_artifact(&tarball_path, &request.manifest.app_name, &request.job_id, "tar.gz")?;
    Ok(artifact_path)
}

fn copy_artifact(
    source: &std::path::Path,
    app_name: &str,
    job_id: &str,
    ext: &str,
) -> Result<PathBuf, String> {
    let artifact_dir = std::env::temp_dir().join("perry-artifacts");
    std::fs::create_dir_all(&artifact_dir)
        .map_err(|e| format!("Failed to create artifact dir: {e}"))?;

    let dest = artifact_dir.join(format!("{app_name}-{job_id}.{ext}"));
    std::fs::copy(source, &dest).map_err(|e| format!("Failed to copy artifact: {e}"))?;
    Ok(dest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildTarget {
    Linux,
    Android,
    Windows,
    Ios,
    Macos,
}

fn determine_target(targets: &[String]) -> BuildTarget {
    for t in targets {
        match t.to_lowercase().as_str() {
            "android" => return BuildTarget::Android,
            "windows" => return BuildTarget::Windows,
            "ios" => return BuildTarget::Ios,
            "macos" => return BuildTarget::Macos,
            _ => {}
        }
    }
    BuildTarget::Linux
}

fn check_cancelled(cancelled: &Arc<AtomicBool>) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("Build cancelled".into())
    } else {
        Ok(())
    }
}

fn send_stage(progress: &ProgressSender, stage: StageName, message: &str) {
    let _ = progress.send(ServerMessage::Stage {
        stage,
        message: message.to_string(),
    });
}

fn send_progress(progress: &ProgressSender, stage: StageName, percent: u8, message: Option<&str>) {
    let _ = progress.send(ServerMessage::Progress {
        stage,
        percent,
        message: message.map(String::from),
    });
}

fn extract_tarball(tarball_path: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    let file =
        std::fs::File::open(tarball_path).map_err(|e| format!("Failed to open tarball: {e}"))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    // Manually iterate entries to prevent path traversal attacks.
    // archive.unpack() does NOT validate paths — a malicious tarball could
    // write files outside the destination via ".." components or absolute paths.
    for entry in archive
        .entries()
        .map_err(|e| format!("Failed to read tarball entries: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("Failed to read tarball entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("Failed to read entry path: {e}"))?
            .into_owned();

        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!(
                "Tarball contains unsafe path (path traversal rejected): {}",
                path.display()
            ));
        }

        entry
            .unpack_in(dest)
            .map_err(|e| format!("Failed to extract {}: {e}", path.display()))?;
    }

    Ok(())
}

fn get_perry_version(perry_binary: &str) -> String {
    std::process::Command::new(perry_binary)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| format!("Invalid base64: {e}"))
}
