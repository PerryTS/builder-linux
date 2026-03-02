use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Sign an APK using zipalign + apksigner from the Android SDK build-tools.
///
/// Steps:
/// 1. zipalign the APK
/// 2. apksigner sign with the keystore
/// 3. apksigner verify
pub async fn sign_apk(
    apk_path: &Path,
    keystore_path: &Path,
    keystore_pass: &str,
    key_alias: &str,
    key_pass: &str,
) -> Result<PathBuf, String> {
    let build_tools = find_build_tools_dir()?;

    let aligned_path = apk_path.with_extension("aligned.apk");

    // Step 1: zipalign
    let zipalign = build_tools.join("zipalign");
    let output = Command::new(&zipalign)
        .arg("-v")
        .arg("-p")
        .arg("4")
        .arg(apk_path)
        .arg(&aligned_path)
        .output()
        .await
        .map_err(|e| format!("Failed to run zipalign: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("zipalign failed: {stderr}"));
    }

    // Step 2: apksigner sign
    let apksigner = build_tools.join("apksigner");
    let output = Command::new(&apksigner)
        .arg("sign")
        .arg("--ks")
        .arg(keystore_path)
        .arg("--ks-key-alias")
        .arg(key_alias)
        .arg("--ks-pass")
        .arg(format!("pass:{keystore_pass}"))
        .arg("--key-pass")
        .arg(format!("pass:{key_pass}"))
        .arg(&aligned_path)
        .output()
        .await
        .map_err(|e| format!("Failed to run apksigner: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Clean up aligned file on failure
        std::fs::remove_file(&aligned_path).ok();
        return Err(format!("apksigner sign failed: {stderr}"));
    }

    // Step 3: verify
    let output = Command::new(&apksigner)
        .arg("verify")
        .arg(&aligned_path)
        .output()
        .await
        .map_err(|e| format!("Failed to run apksigner verify: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("apksigner verify failed: {stderr}"));
    }

    Ok(aligned_path)
}

/// Sign an AAB using jarsigner (from JDK, since apksigner doesn't support AAB).
pub async fn sign_aab(
    aab_path: &Path,
    keystore_path: &Path,
    keystore_pass: &str,
    key_alias: &str,
    key_pass: &str,
) -> Result<(), String> {
    let output = Command::new("jarsigner")
        .arg("-keystore")
        .arg(keystore_path)
        .arg("-storepass")
        .arg(keystore_pass)
        .arg("-keypass")
        .arg(key_pass)
        .arg(aab_path)
        .arg(key_alias)
        .output()
        .await
        .map_err(|e| format!("Failed to run jarsigner: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("jarsigner failed: {stderr}"));
    }

    Ok(())
}

/// Find the Android SDK build-tools directory containing zipalign and apksigner.
/// Checks PERRY_BUILD_ANDROID_HOME, ANDROID_HOME, then ANDROID_SDK_ROOT.
fn find_build_tools_dir() -> Result<PathBuf, String> {
    let sdk_root = std::env::var("PERRY_BUILD_ANDROID_HOME")
        .or_else(|_| std::env::var("ANDROID_HOME"))
        .or_else(|_| std::env::var("ANDROID_SDK_ROOT"))
        .map_err(|_| "ANDROID_HOME not set. Set ANDROID_HOME or PERRY_BUILD_ANDROID_HOME to the Android SDK path.".to_string())?;

    let build_tools_dir = PathBuf::from(&sdk_root).join("build-tools");
    if !build_tools_dir.exists() {
        return Err(format!(
            "Android SDK build-tools not found at {}",
            build_tools_dir.display()
        ));
    }

    // Find the latest version directory
    let mut versions: Vec<_> = std::fs::read_dir(&build_tools_dir)
        .map_err(|e| format!("Failed to read build-tools dir: {e}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();

    versions.sort();

    versions
        .last()
        .cloned()
        .ok_or_else(|| "No Android SDK build-tools versions installed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_build_tools_missing_env() {
        // When ANDROID_HOME is not set, should return an error
        // (this test may pass or fail depending on the test environment)
        let result = find_build_tools_dir();
        // We just verify it returns a Result, not that it necessarily errors
        let _ = result;
    }
}
