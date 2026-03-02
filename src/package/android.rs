use crate::queue::job::BuildManifest;
use crate::ws::messages::{LogStream, ServerMessage, StageName};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;

/// Set up the Android Gradle project from the perry-ui-android template.
///
/// 1. Resolve template from perry binary path
/// 2. Copy template tree to tmpdir
/// 3. Customize build.gradle.kts, settings.gradle.kts, AndroidManifest.xml
/// 4. Place compiled .so at jniLibs/arm64-v8a/
/// 5. Copy icons into res/
pub fn create_android_project(
    manifest: &BuildManifest,
    perry_binary: &str,
    so_path: &Path,
    icons_dir: Option<&Path>,
    tmpdir: &Path,
) -> Result<PathBuf, String> {
    let project_dir = tmpdir.join("android_project");

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
        let version_code = version_to_code(&manifest.version);
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

    // Copy icons into res/
    if let Some(icons) = icons_dir {
        if icons.exists() {
            let res_dir = project_dir.join("app/src/main/res");
            std::fs::create_dir_all(&res_dir)
                .map_err(|e| format!("Failed to create res dir: {e}"))?;
            // Copy mipmap-* directories
            if let Ok(entries) = std::fs::read_dir(icons) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with("mipmap-") && entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        copy_dir_recursive(&entry.path(), &res_dir.join(&*name_str))?;
                    }
                }
            }
        }
    }

    Ok(project_dir)
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

    let aab_path = project_dir
        .join("app/build/outputs/bundle/release/app-release.aab");

    if !aab_path.exists() {
        return Err(format!(
            "Gradle build succeeded but AAB not found at {}",
            aab_path.display()
        ));
    }

    Ok(aab_path)
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
                p.to_string()
            } else {
                format!("android.permission.{p}")
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

    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="{bundle_id}">{permissions_block}
    <application
        android:allowBackup="true"
        android:label="{app_name}"
        android:icon="@mipmap/ic_launcher"
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
        bundle_id = manifest.bundle_id,
        app_name = manifest.app_name,
    )
}

/// Run a Gradle task, streaming stdout/stderr.
async fn run_gradle(
    project_dir: &Path,
    task: &str,
    tx: Option<&broadcast::Sender<ServerMessage>>,
) -> Result<(), String> {
    let gradle_cmd = if cfg!(windows) { "gradlew.bat" } else { "./gradlew" };

    // Try project-local gradlew first, fall back to system gradle
    let cmd_path = project_dir.join(gradle_cmd);
    let (program, use_project_wrapper) = if cmd_path.exists() {
        (cmd_path.to_string_lossy().to_string(), true)
    } else {
        ("gradle".to_string(), false)
    };

    let mut cmd = Command::new(&program);
    if !use_project_wrapper {
        cmd.arg("-p").arg(project_dir);
    }
    cmd.arg(task)
        .arg("--no-daemon")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if use_project_wrapper {
        cmd.current_dir(project_dir);
    }

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
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(ref tx) = tx_err {
                let _ = tx.send(ServerMessage::Log {
                    stage: StageName::Bundling,
                    line,
                    stream: LogStream::Stderr,
                });
            }
        }
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait for gradle: {e}"))?;

    stdout_task.await.ok();
    stderr_task.await.ok();

    if !status.success() {
        return Err(format!(
            "Gradle {} failed with exit code {}",
            task,
            status.code().unwrap_or(-1)
        ));
    }

    Ok(())
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
        };

        let xml = generate_android_manifest_xml(&manifest);
        assert!(xml.contains("package=\"com.example.testapp\""));
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
        };

        let xml = generate_android_manifest_xml(&manifest);
        assert!(xml.contains("android.permission.INTERNET"));
        assert!(xml.contains("android.permission.ACCESS_FINE_LOCATION"));
        assert!(xml.contains("package=\"com.example.myapp\""));
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
        };

        let xml = generate_android_manifest_xml(&manifest);
        // Fully qualified permissions should be passed through as-is
        assert!(xml.contains("com.google.android.providers.gsf.permission.READ_GSERVICES"));
    }

    #[test]
    fn test_version_to_code() {
        assert_eq!(version_to_code("1.0.0"), 10000);
        assert_eq!(version_to_code("2.1.3"), 20103);
        assert_eq!(version_to_code("1.2"), 10200);
        assert_eq!(version_to_code("3"), 30000);
    }
}
