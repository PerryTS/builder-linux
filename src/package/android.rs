use crate::build::validate::escape_xml;
use crate::queue::job::BuildManifest;
use crate::ws::messages::{LogStream, ServerMessage, StageName};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;

/// Detect a project-provided Android Gradle harness at `<source>/android`.
///
/// A Bloom-engine game (or any project whose linked native library provides its
/// own JNI bridge) ships a hand-written Gradle app under `android/` with its own
/// Activity + bridge that `System.loadLibrary("<name>")` and calls its own JNI
/// entry points. The compiled `.so` carries those symbols (the engine is a
/// `perry.nativeLibrary` that static-links in), so the generated
/// PerryActivity/PerryBridge template does NOT apply: the engine never
/// implements `PerryBridge.nativeInit`, so the published AAB crashes at launch
/// with `UnsatisfiedLinkError`. When such a harness is present we build it
/// instead of the template — matching what `perry compile --target android` plus
/// the project's own Gradle produces.
///
/// Returns the harness directory when `android/` looks like a Gradle project.
pub(crate) fn detect_project_android_harness(source_dir: Option<&Path>) -> Option<PathBuf> {
    let android = source_dir?.join("android");
    if !android.is_dir() {
        return None;
    }
    let markers = [
        "settings.gradle.kts",
        "settings.gradle",
        "app/build.gradle.kts",
        "app/build.gradle",
    ];
    markers
        .iter()
        .any(|m| android.join(m).exists())
        .then_some(android)
}

/// Find the base name a harness passes to `System.loadLibrary("...")`, e.g.
/// `bloom_jump` → the `.so` must be installed as `libbloom_jump.so`. Scans the
/// harness's Kotlin/Java sources. Falls back to `perry_app` (the template's
/// soname) when no call is found.
fn detect_native_lib_name(harness_dir: &Path) -> String {
    fn scan(dir: &Path, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if out.is_some() {
                return;
            }
            let path = entry.path();
            let Ok(ty) = entry.file_type() else { continue };
            if ty.is_dir() {
                scan(&path, out);
            } else if matches!(path.extension().and_then(|e| e.to_str()), Some("kt" | "java")) {
                if let Ok(src) = std::fs::read_to_string(&path) {
                    if let Some(name) = parse_load_library(&src) {
                        *out = Some(name);
                        return;
                    }
                }
            }
        }
    }
    let mut found = None;
    scan(&harness_dir.join("app/src/main"), &mut found);
    found.unwrap_or_else(|| "perry_app".to_string())
}

/// Extract the first `System.loadLibrary("X")` argument from a source string.
fn parse_load_library(src: &str) -> Option<String> {
    let idx = src.find("loadLibrary")?;
    let after = &src[idx + "loadLibrary".len()..];
    let open = after.find('(')?;
    let q1 = after[open..].find('"')? + open;
    let rest = &after[q1 + 1..];
    let q2 = rest.find('"')?;
    let name = &rest[..q2];
    (!name.is_empty()).then(|| name.to_string())
}

/// Set up the Android Gradle project from the perry-ui-android template.
///
/// 1. Resolve template from perry binary path
/// 2. Copy template tree to tmpdir
/// 3. Customize build.gradle.kts, settings.gradle.kts, AndroidManifest.xml
/// 4. Place compiled .so at jniLibs/arm64-v8a/
/// 5. Copy icons into res/
///
/// When the project ships its own `android/` Gradle harness (see
/// [`detect_project_android_harness`]) that harness is built instead.
pub fn create_android_project(
    manifest: &BuildManifest,
    perry_binary: &str,
    so_path: &Path,
    icons_dir: Option<&Path>,
    tmpdir: &Path,
    source_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let project_dir = tmpdir.join("android_project");

    if let Some(harness) = detect_project_android_harness(source_dir) {
        return setup_project_harness_android(
            manifest,
            &harness,
            source_dir,
            so_path,
            icons_dir,
            &project_dir,
        );
    }

    // Resolve template path from perry binary
    let perry_path = Path::new(perry_binary);
    let perry_path = if perry_path.is_relative() {
        std::env::current_dir()
            .map_err(|e| format!("Failed to get CWD: {e}"))?
            .join(perry_path)
    } else {
        perry_path.to_path_buf()
    };

    // Perry binary at <repo>/target/release/perry
    // Template at <repo>/crates/perry-ui-android/template/
    let template_dir = perry_path
        .parent() // target/release/
        .and_then(|p| p.parent()) // target/
        .and_then(|p| p.parent()) // <repo>/
        .map(|repo| repo.join("crates/perry-ui-android/template"))
        .ok_or_else(|| "Cannot resolve perry-ui-android template path from perry binary".to_string())?;

    if template_dir.exists() {
        copy_dir_recursive(&template_dir, &project_dir)?;
    } else {
        // If template doesn't exist, create a minimal project structure
        create_minimal_project(&project_dir)?;
    }

    // Customize build.gradle.kts
    let build_gradle = project_dir.join("app/build.gradle.kts");
    if build_gradle.exists() {
        let content = std::fs::read_to_string(&build_gradle)
            .map_err(|e| format!("Failed to read build.gradle.kts: {e}"))?;
        let min_sdk = manifest.android_min_sdk.as_deref().unwrap_or("24");
        let target_sdk = manifest.android_target_sdk.as_deref().unwrap_or("34");
        let version_code = resolved_version_code(manifest);
        let content = content
            .replace("com.perry.template", &manifest.bundle_id)
            .replace("minSdk = 24", &format!("minSdk = {min_sdk}"))
            .replace("targetSdk = 34", &format!("targetSdk = {target_sdk}"))
            .replace("versionCode = 1", &format!("versionCode = {version_code}"))
            .replace("versionName = \"1.0\"", &format!("versionName = \"{}\"", manifest.version));
        std::fs::write(&build_gradle, content)
            .map_err(|e| format!("Failed to write build.gradle.kts: {e}"))?;
    }

    // Customize settings.gradle.kts
    let settings_gradle = project_dir.join("settings.gradle.kts");
    if settings_gradle.exists() {
        let content = std::fs::read_to_string(&settings_gradle)
            .map_err(|e| format!("Failed to read settings.gradle.kts: {e}"))?;
        let content = content.replace("perry-template", &manifest.app_name);
        std::fs::write(&settings_gradle, content)
            .map_err(|e| format!("Failed to write settings.gradle.kts: {e}"))?;
    }

    // Generate AndroidManifest.xml
    let manifest_xml = generate_android_manifest_xml(manifest);
    let manifest_path = project_dir.join("app/src/main/AndroidManifest.xml");
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create manifest dir: {e}"))?;
    }
    std::fs::write(&manifest_path, manifest_xml)
        .map_err(|e| format!("Failed to write AndroidManifest.xml: {e}"))?;

    // Place .so at jniLibs/arm64-v8a/
    let jni_dir = project_dir.join("app/src/main/jniLibs/arm64-v8a");
    std::fs::create_dir_all(&jni_dir)
        .map_err(|e| format!("Failed to create jniLibs dir: {e}"))?;
    std::fs::copy(so_path, jni_dir.join("libperry_app.so"))
        .map_err(|e| format!("Failed to copy .so library: {e}"))?;
    copy_companion_libraries(so_path, &jni_dir)?;

    // Copy resource directories (assets/, logo/, etc.) into APK assets
    let apk_assets = project_dir.join("app/src/main/assets");
    copy_project_assets(source_dir, so_path, &apk_assets)?;

    // Copy icons into res/
    copy_launcher_icons(icons_dir, &project_dir.join("app/src/main/res"))?;

    Ok(project_dir)
}

/// Build a Gradle project from a project-provided `android/` harness instead of
/// the generated PerryActivity/PerryBridge template (see
/// [`detect_project_android_harness`]). The compiled `.so` is installed under
/// the soname the harness loads, and the harness's own Activity / manifest /
/// `build.gradle` are left intact.
fn setup_project_harness_android(
    manifest: &BuildManifest,
    harness_dir: &Path,
    source_dir: Option<&Path>,
    so_path: &Path,
    icons_dir: Option<&Path>,
    project_dir: &Path,
) -> Result<PathBuf, String> {
    // Copy the harness into the build dir, skipping stale build outputs and the
    // checked-in Gradle wrapper — the worker always drives the build with the
    // system `gradle`, never a tarball-provided `gradlew` (see run_gradle's
    // SECURITY note).
    copy_harness_tree(harness_dir, project_dir)?;

    let lib_name = detect_native_lib_name(project_dir);
    eprintln!(
        "[android] using project android/ harness: installing .so as lib{lib_name}.so \
         (System.loadLibrary(\"{lib_name}\"))"
    );

    // Install the compiled .so under the name the harness loads.
    let jni_dir = project_dir.join("app/src/main/jniLibs/arm64-v8a");
    std::fs::create_dir_all(&jni_dir)
        .map_err(|e| format!("Failed to create jniLibs dir: {e}"))?;
    std::fs::copy(so_path, jni_dir.join(format!("lib{lib_name}.so")))
        .map_err(|e| format!("Failed to copy .so library: {e}"))?;
    copy_companion_libraries(so_path, &jni_dir)?;

    // Best-effort: stamp versionCode / versionName from the manifest so Play
    // Store uploads get a unique, increasing versionCode. Only literal
    // assignments are rewritten; a harness that computes them is left untouched.
    stamp_gradle_version(project_dir, manifest);

    // Mirror the template path: copy project asset dirs (assets/, logo/, …) into
    // the APK assets, and merge generated launcher icons into res/ without
    // clobbering the harness's own resources.
    copy_project_assets(source_dir, so_path, &project_dir.join("app/src/main/assets"))?;
    copy_launcher_icons(icons_dir, &project_dir.join("app/src/main/res"))?;

    Ok(project_dir.to_path_buf())
}

/// Copy companion shared libraries (`.so`) that sit alongside the main binary.
/// The Perry compiler places these next to the output (e.g.
/// `libhone_editor_android.so`); `libperry_app.so` records them in DT_NEEDED so
/// they must land in the same `jniLibs/<abi>/` dir.
fn copy_companion_libraries(so_path: &Path, jni_dir: &Path) -> Result<(), String> {
    let Some(so_dir) = so_path.parent() else {
        return Ok(());
    };
    let main_name = so_path.file_name().unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir(so_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with(".so")
                && name_str != "libperry_app.so"
                && name.as_os_str() != main_name
            {
                std::fs::copy(entry.path(), jni_dir.join(&*name_str))
                    .map_err(|e| format!("Failed to copy companion library {name_str}: {e}"))?;
            }
        }
    }
    Ok(())
}

/// Copy project resource directories (`assets/`, `logo/`, …) into the APK
/// `assets/` tree. Falls back to the `.so` parent when no source dir is known.
fn copy_project_assets(
    source_dir: Option<&Path>,
    so_path: &Path,
    apk_assets: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(apk_assets)
        .map_err(|e| format!("Failed to create assets dir: {e}"))?;
    let asset_source =
        source_dir.unwrap_or_else(|| so_path.parent().unwrap_or(std::path::Path::new(".")));
    for dir_name in &["logo", "assets", "resources", "images"] {
        let resource_dir = asset_source.join(dir_name);
        if resource_dir.is_dir() {
            let _ = copy_dir_recursive(&resource_dir, &apk_assets.join(dir_name));
        }
    }
    Ok(())
}

/// Copy generated `mipmap-*` launcher icon directories into `res/`.
fn copy_launcher_icons(icons_dir: Option<&Path>, res_dir: &Path) -> Result<(), String> {
    let Some(icons) = icons_dir else {
        return Ok(());
    };
    if !icons.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(res_dir).map_err(|e| format!("Failed to create res dir: {e}"))?;
    if let Ok(entries) = std::fs::read_dir(icons) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("mipmap-")
                && entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            {
                copy_dir_recursive(&entry.path(), &res_dir.join(&*name_str))?;
            }
        }
    }
    Ok(())
}

/// Copy a project's `android/` harness into the build dir, skipping build
/// outputs and the checked-in Gradle wrapper (which the worker never executes).
fn copy_harness_tree(src: &Path, dst: &Path) -> Result<(), String> {
    fn is_skipped(name: &str) -> bool {
        matches!(
            name,
            // Build outputs and the dev machine's SDK path; the worker drives the
            // build with system `gradle` + its own ANDROID_HOME, never a
            // tarball-provided wrapper (see run_gradle's SECURITY note).
            "build"
                | ".gradle"
                | ".kotlin"
                | "gradlew"
                | "gradlew.bat"
                | "local.properties"
        )
    }
    fn recurse(src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dst).map_err(|e| format!("Failed to create dir: {e}"))?;
        for entry in std::fs::read_dir(src)
            .map_err(|e| format!("Failed to read dir {}: {e}", src.display()))?
        {
            let entry = entry.map_err(|e| format!("Dir entry error: {e}"))?;
            let name = entry.file_name();
            if is_skipped(&name.to_string_lossy()) {
                continue;
            }
            let ty = entry.file_type().map_err(|e| format!("File type error: {e}"))?;
            let dest_path = dst.join(&name);
            if ty.is_dir() {
                recurse(&entry.path(), &dest_path)?;
            } else if !ty.is_symlink() {
                std::fs::copy(entry.path(), &dest_path).map_err(|e| format!("Copy error: {e}"))?;
            }
        }
        Ok(())
    }
    // Drop the bundled wrapper jar too — we drive the build with system gradle.
    recurse(src, dst)?;
    let _ = std::fs::remove_file(dst.join("gradle/wrapper/gradle-wrapper.jar"));
    Ok(())
}

/// Best-effort rewrite of literal `versionCode` / `versionName` assignments in a
/// harness's `app/build.gradle[.kts]` to match the build manifest, so Play Store
/// uploads carry a unique, increasing versionCode.
fn stamp_gradle_version(project_dir: &Path, manifest: &BuildManifest) {
    let version_code = resolved_version_code(manifest);
    for rel in ["app/build.gradle.kts", "app/build.gradle"] {
        let path = project_dir.join(rel);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stamped = replace_gradle_assignment(&content, "versionCode", &version_code.to_string());
        let stamped =
            replace_gradle_assignment(&stamped, "versionName", &format!("\"{}\"", manifest.version));
        if stamped != content {
            let _ = std::fs::write(&path, stamped);
        }
    }
}

/// Replace the value of the first `<key> = <value>` (or `<key> <value>`)
/// assignment in a Gradle file, preserving everything else. No-op if absent.
fn replace_gradle_assignment(content: &str, key: &str, new_value: &str) -> String {
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let after_key = trimmed.strip_prefix(key);
            // The char right after the key must be `=` or whitespace, so we
            // don't match a longer identifier like `versionCodeOverride`.
            let next = after_key.and_then(|s| s.chars().next());
            if matches!(next, Some('=') | Some(' ') | Some('\t')) {
                let after_key = after_key.unwrap();
                let indent = &line[..line.len() - trimmed.len()];
                let sep = if after_key.trim_start().starts_with('=') {
                    " = "
                } else {
                    " "
                };
                format!("{indent}{key}{sep}{new_value}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build an APK using Gradle.
pub async fn build_apk(
    project_dir: &Path,
    release: bool,
    tx: Option<&broadcast::Sender<ServerMessage>>,
) -> Result<PathBuf, String> {
    let task = if release { "assembleRelease" } else { "assembleDebug" };

    run_gradle(project_dir, task, tx).await?;

    let variant = if release { "release" } else { "debug" };
    let apk_name = if release { "app-release-unsigned.apk" } else { "app-debug.apk" };
    let apk_path = project_dir
        .join("app/build/outputs/apk")
        .join(variant)
        .join(apk_name);

    if !apk_path.exists() {
        // Try alternate name
        let alt = project_dir
            .join("app/build/outputs/apk")
            .join(variant)
            .join(format!("app-{variant}.apk"));
        if alt.exists() {
            return Ok(alt);
        }
        return Err(format!(
            "Gradle build succeeded but APK not found at {}",
            apk_path.display()
        ));
    }

    Ok(apk_path)
}

/// Build an AAB (Android App Bundle) using Gradle.
pub async fn build_aab(
    project_dir: &Path,
    tx: Option<&broadcast::Sender<ServerMessage>>,
) -> Result<PathBuf, String> {
    run_gradle(project_dir, "bundleRelease", tx).await?;

    let aab_path = project_dir.join("app/build/outputs/bundle/release/app-release.aab");
    if aab_path.exists() {
        return Ok(aab_path);
    }

    // A project-provided harness (see detect_project_android_harness) may name
    // the module or release flavor differently, so the file isn't always
    // `app-release.aab`. Search the bundle output tree for any `.aab`.
    if let Some(found) = find_first_with_extension(&project_dir.join("app/build/outputs/bundle"), "aab")
    {
        return Ok(found);
    }

    Err(format!(
        "Gradle build succeeded but AAB not found at {}",
        aab_path.display()
    ))
}

/// Recursively find the first file with the given extension under `dir`.
fn find_first_with_extension(dir: &Path, ext: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            return Some(path);
        }
    }
    subdirs.iter().find_map(|d| find_first_with_extension(d, ext))
}

/// Generate AndroidManifest.xml from BuildManifest fields.
pub fn generate_android_manifest_xml(manifest: &BuildManifest) -> String {
    let permissions = manifest
        .android_permissions
        .as_deref()
        .unwrap_or(&[]);
    let permissions_xml: String = permissions
        .iter()
        .map(|p| {
            let perm = if p.contains('.') {
                escape_xml(p)
            } else {
                format!("android.permission.{}", escape_xml(p))
            };
            format!("    <uses-permission android:name=\"{perm}\" />")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let permissions_block = if permissions_xml.is_empty() {
        String::new()
    } else {
        format!("\n{permissions_xml}\n")
    };

    let has_icon = manifest.icon.is_some();
    let icon_attr = if has_icon {
        "\n        android:icon=\"@mipmap/ic_launcher\""
    } else {
        ""
    };

    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android">{permissions_block}
    <application
        android:allowBackup="true"
        android:label="{app_name}"{icon_attr}
        android:supportsRtl="true"
        android:theme="@style/Theme.Perry">
        <activity
            android:name=".PerryActivity"
            android:exported="true"
            android:configChanges="orientation|keyboardHidden|screenSize">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>"#,
        app_name = escape_xml(&manifest.app_name),
        icon_attr = icon_attr,
    )
}

/// Run a Gradle task, streaming stdout/stderr.
///
/// SECURITY: Always uses system `gradle`, never executes `gradlew` from the project
/// directory. A malicious tarball could include a `gradlew` script that runs
/// arbitrary code with the worker's privileges.
async fn run_gradle(
    project_dir: &Path,
    task: &str,
    tx: Option<&broadcast::Sender<ServerMessage>>,
) -> Result<(), String> {
    let mut cmd = Command::new("gradle");
    cmd.arg("-p")
        .arg(project_dir)
        .arg(task)
        .arg("--no-daemon")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn gradle: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let tx_out = tx.cloned();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(ref tx) = tx_out {
                let _ = tx.send(ServerMessage::Log {
                    stage: StageName::Bundling,
                    line,
                    stream: LogStream::Stdout,
                });
            }
        }
    });

    let tx_err = tx.cloned();
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut lines = Vec::new();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(ref tx) = tx_err {
                let _ = tx.send(ServerMessage::Log {
                    stage: StageName::Bundling,
                    line: line.clone(),
                    stream: LogStream::Stderr,
                });
            }
            lines.push(line);
        }
        lines
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait for gradle: {e}"))?;

    stdout_task.await.ok();
    let stderr_lines = stderr_task.await.unwrap_or_default();

    if !status.success() {
        let detail = if stderr_lines.is_empty() {
            String::new()
        } else {
            format!("\n{}", stderr_lines.join("\n"))
        };
        return Err(format!(
            "Gradle {} failed with exit code {}{}",
            task,
            status.code().unwrap_or(-1),
            detail
        ));
    }

    Ok(())
}

/// Resolve the Android `versionCode`: an explicit `[android] version_code` from
/// perry.toml (carried in `manifest.android_version_code`) wins; otherwise it is
/// derived from the manifest version (`build_number`) via [`version_to_code`].
/// The explicit override lets a project keep `versionCode` monotonic across
/// CI/build-number changes without changing the marketing version.
fn resolved_version_code(manifest: &BuildManifest) -> u32 {
    manifest
        .android_version_code
        .unwrap_or_else(|| version_to_code(&manifest.version))
}

/// Convert a semver version string to an Android versionCode integer.
fn version_to_code(version: &str) -> u32 {
    let parts: Vec<u32> = version
        .split('.')
        .filter_map(|s| s.parse().ok())
        .collect();
    let major = parts.first().copied().unwrap_or(1);
    let minor = parts.get(1).copied().unwrap_or(0);
    let patch = parts.get(2).copied().unwrap_or(0);
    major * 10000 + minor * 100 + patch
}

/// Create a minimal Android project structure when template is not available.
fn create_minimal_project(project_dir: &Path) -> Result<(), String> {
    let app_dir = project_dir.join("app/src/main");
    std::fs::create_dir_all(&app_dir)
        .map_err(|e| format!("Failed to create app dir: {e}"))?;

    // Minimal build.gradle.kts
    let build_gradle = r#"plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.perry.template"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.perry.template"
        minSdk = 24
        targetSdk = 34
        versionCode = 1
        versionName = "1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}
"#;
    std::fs::create_dir_all(project_dir.join("app"))
        .map_err(|e| format!("Failed to create app dir: {e}"))?;
    std::fs::write(project_dir.join("app/build.gradle.kts"), build_gradle)
        .map_err(|e| format!("Failed to write build.gradle.kts: {e}"))?;

    // Minimal settings.gradle.kts
    let settings = r#"pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolution {
    repositories {
        google()
        mavenCentral()
    }
}
rootProject.name = "perry-template"
include(":app")
"#;
    std::fs::write(project_dir.join("settings.gradle.kts"), settings)
        .map_err(|e| format!("Failed to write settings.gradle.kts: {e}"))?;

    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("Failed to create dir: {e}"))?;
    for entry in
        std::fs::read_dir(src).map_err(|e| format!("Failed to read dir {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| format!("Dir entry error: {e}"))?;
        let ty = entry
            .file_type()
            .map_err(|e| format!("File type error: {e}"))?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else if ty.is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(entry.path())
                    .map_err(|e| format!("Symlink read error: {e}"))?;
                std::os::unix::fs::symlink(target, &dest_path)
                    .map_err(|e| format!("Symlink create error: {e}"))?;
            }
        } else {
            std::fs::copy(entry.path(), &dest_path)
                .map_err(|e| format!("Copy error: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> BuildManifest {
        BuildManifest {
            app_name: "TestApp".into(),
            bundle_id: "com.example.testapp".into(),
            version: "1.0.0".into(),
            short_version: None,
            entry: "src/main.ts".into(),
            icon: None,
            targets: vec!["android".into()],
            category: None,
            minimum_os_version: None,
            entitlements: None,
            ios_deployment_target: None,
            ios_device_family: None,
            ios_orientations: None,
            ios_capabilities: None,
            ios_distribute: None,
            android_min_sdk: None,
            android_target_sdk: None,
            android_permissions: None,
            android_distribute: None,
            android_version_code: None,
            linux_format: None,
            linux_category: None,
            linux_description: None,
            windows_distribute: None,
            windows_uac_level: None,
            windows_dpi_aware: None,
            windows_file_description: None,
            windows_company_name: None,
            windows_copyright: None,
            ios_encryption_exempt: None,
            ios_info_plist: None,
            macos_distribute: None,
            macos_encryption_exempt: None,
            release_notes: None,
            features: None,
        }
    }

    #[test]
    fn test_android_manifest_xml_no_permissions() {
        let manifest = BuildManifest {
            app_name: "TestApp".into(),
            bundle_id: "com.example.testapp".into(),
            version: "1.0.0".into(),
            short_version: None,
            entry: "src/main.ts".into(),
            icon: None,
            targets: vec!["android".into()],
            category: None,
            minimum_os_version: None,
            entitlements: None,
            ios_deployment_target: None,
            ios_device_family: None,
            ios_orientations: None,
            ios_capabilities: None,
            ios_distribute: None,
            android_min_sdk: None,
            android_target_sdk: None,
            android_permissions: None,
            android_distribute: None,
            android_version_code: None,
            linux_format: None,
            linux_category: None,
            linux_description: None,
            windows_distribute: None,
            windows_uac_level: None,
            windows_dpi_aware: None,
            windows_file_description: None,
            windows_company_name: None,
            windows_copyright: None,
            ios_encryption_exempt: None,
            ios_info_plist: None,
            macos_distribute: None,
            macos_encryption_exempt: None,
            release_notes: None,
            features: None,
        };

        let xml = generate_android_manifest_xml(&manifest);
        // The applicationId/namespace now live in build.gradle, not the manifest.
        assert!(xml.contains("<manifest"));
        assert!(xml.contains("android:label=\"TestApp\""));
        assert!(xml.contains("PerryActivity"));
        assert!(xml.contains("android.intent.action.MAIN"));
        assert!(xml.contains("android.intent.category.LAUNCHER"));
        assert!(!xml.contains("uses-permission"));
    }

    #[test]
    fn test_android_manifest_xml_with_permissions() {
        let manifest = BuildManifest {
            app_name: "MyApp".into(),
            bundle_id: "com.example.myapp".into(),
            version: "2.0.0".into(),
            short_version: None,
            entry: "src/main.ts".into(),
            icon: None,
            targets: vec!["android".into()],
            category: None,
            minimum_os_version: None,
            entitlements: None,
            ios_deployment_target: None,
            ios_device_family: None,
            ios_orientations: None,
            ios_capabilities: None,
            ios_distribute: None,
            android_min_sdk: Some("26".into()),
            android_target_sdk: Some("35".into()),
            android_permissions: Some(vec![
                "INTERNET".into(),
                "ACCESS_FINE_LOCATION".into(),
            ]),
            android_distribute: None,
            android_version_code: None,
            linux_format: None,
            linux_category: None,
            linux_description: None,
            windows_distribute: None,
            windows_uac_level: None,
            windows_dpi_aware: None,
            windows_file_description: None,
            windows_company_name: None,
            windows_copyright: None,
            ios_encryption_exempt: None,
            ios_info_plist: None,
            macos_distribute: None,
            macos_encryption_exempt: None,
            release_notes: None,
            features: None,
        };

        let xml = generate_android_manifest_xml(&manifest);
        assert!(xml.contains("android.permission.INTERNET"));
        assert!(xml.contains("android.permission.ACCESS_FINE_LOCATION"));
        // The applicationId/namespace now live in build.gradle, not the manifest.
        assert!(xml.contains("<manifest"));
        assert!(xml.contains("android:label=\"MyApp\""));
    }

    #[test]
    fn test_android_manifest_xml_fully_qualified_permission() {
        let manifest = BuildManifest {
            app_name: "App".into(),
            bundle_id: "com.test.app".into(),
            version: "1.0.0".into(),
            short_version: None,
            entry: "src/main.ts".into(),
            icon: None,
            targets: vec!["android".into()],
            category: None,
            minimum_os_version: None,
            entitlements: None,
            ios_deployment_target: None,
            ios_device_family: None,
            ios_orientations: None,
            ios_capabilities: None,
            ios_distribute: None,
            android_min_sdk: None,
            android_target_sdk: None,
            android_permissions: Some(vec![
                "com.google.android.providers.gsf.permission.READ_GSERVICES".into(),
            ]),
            android_distribute: None,
            android_version_code: None,
            linux_format: None,
            linux_category: None,
            linux_description: None,
            windows_distribute: None,
            windows_uac_level: None,
            windows_dpi_aware: None,
            windows_file_description: None,
            windows_company_name: None,
            windows_copyright: None,
            ios_encryption_exempt: None,
            ios_info_plist: None,
            macos_distribute: None,
            macos_encryption_exempt: None,
            release_notes: None,
            features: None,
        };

        let xml = generate_android_manifest_xml(&manifest);
        // Fully qualified permissions should be passed through as-is
        assert!(xml.contains("com.google.android.providers.gsf.permission.READ_GSERVICES"));
    }

    #[test]
    fn test_resolved_version_code_prefers_override() {
        let mut m = sample_manifest();
        m.version = "80".into(); // build_number -> derived 800000
        assert_eq!(resolved_version_code(&m), 800000);
        m.android_version_code = Some(800078);
        assert_eq!(resolved_version_code(&m), 800078);
    }

    #[test]
    fn test_version_to_code() {
        assert_eq!(version_to_code("1.0.0"), 10000);
        assert_eq!(version_to_code("2.1.3"), 20103);
        assert_eq!(version_to_code("1.2"), 10200);
        assert_eq!(version_to_code("3"), 30000);
    }

    #[test]
    fn test_parse_load_library() {
        assert_eq!(
            parse_load_library(r#"System.loadLibrary("bloom_jump")"#).as_deref(),
            Some("bloom_jump")
        );
        assert_eq!(
            parse_load_library("System.loadLibrary( \"perry_app\" )").as_deref(),
            Some("perry_app")
        );
        assert_eq!(parse_load_library("// no load here"), None);
        assert_eq!(parse_load_library(r#"loadLibrary("")"#), None);
    }

    #[test]
    fn test_replace_gradle_assignment() {
        let kts = "    versionCode = 1\n    versionName = \"1.0\"\n";
        let out = replace_gradle_assignment(kts, "versionCode", "10077");
        assert!(out.contains("versionCode = 10077"), "{out}");
        assert!(out.contains("versionName = \"1.0\""), "{out}");
        // Groovy style (no `=`).
        assert_eq!(
            replace_gradle_assignment("    versionCode 1", "versionCode", "5"),
            "    versionCode 5"
        );
        // Absent key is left untouched.
        assert_eq!(
            replace_gradle_assignment("foo = 1", "versionCode", "9"),
            "foo = 1"
        );
    }

    #[test]
    fn test_detect_project_android_harness_and_lib_name() {
        let base = std::env::temp_dir().join(format!("perry_harness_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let android = base.join("android");
        let main = android.join("app/src/main/java/com/x");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(android.join("settings.gradle.kts"), "include(\":app\")").unwrap();
        std::fs::write(
            main.join("BloomActivity.kt"),
            "fun x() { System.loadLibrary(\"bloom_jump\") }",
        )
        .unwrap();

        assert_eq!(
            detect_project_android_harness(Some(&base)).as_deref(),
            Some(android.as_path())
        );
        assert_eq!(detect_native_lib_name(&android), "bloom_jump");

        // No harness when there is no android/ dir.
        assert!(detect_project_android_harness(Some(&base.join("missing"))).is_none());

        // Falls back to the template soname when no loadLibrary call is present.
        std::fs::remove_file(main.join("BloomActivity.kt")).unwrap();
        assert_eq!(detect_native_lib_name(&android), "perry_app");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_copy_harness_tree_skips_build_outputs() {
        let base = std::env::temp_dir().join(format!("perry_harness_copy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("android");
        std::fs::create_dir_all(src.join("app/build/intermediates")).unwrap();
        std::fs::create_dir_all(src.join("gradle/wrapper")).unwrap();
        std::fs::create_dir_all(src.join("app/src/main")).unwrap();
        std::fs::write(src.join("settings.gradle.kts"), "x").unwrap();
        std::fs::write(src.join("gradlew"), "#!/bin/sh\nrm -rf /\n").unwrap();
        std::fs::write(src.join("gradle/wrapper/gradle-wrapper.jar"), "JAR").unwrap();
        std::fs::write(src.join("app/build/intermediates/stale"), "old").unwrap();
        std::fs::write(src.join("app/src/main/AndroidManifest.xml"), "<manifest/>").unwrap();

        let dst = base.join("out");
        copy_harness_tree(&src, &dst).unwrap();

        assert!(dst.join("settings.gradle.kts").exists());
        assert!(dst.join("app/src/main/AndroidManifest.xml").exists());
        // Build outputs, the wrapper jar, and gradlew must not be copied.
        assert!(!dst.join("app/build").exists());
        assert!(!dst.join("gradlew").exists());
        assert!(!dst.join("gradle/wrapper/gradle-wrapper.jar").exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
