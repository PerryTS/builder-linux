//! Windows cross-compilation packaging — creates a precompiled bundle tarball
//! that gets sent to a Windows worker for resource embedding + signing + packaging.

use crate::queue::job::BuildManifest;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// System DLLs that should NOT be bundled (lowercase for comparison).
/// Same list as the Windows worker to ensure consistent behavior.
const SYSTEM_DLLS: &[&str] = &[
    "kernel32.dll",
    "user32.dll",
    "gdi32.dll",
    "ntdll.dll",
    "advapi32.dll",
    "shell32.dll",
    "ole32.dll",
    "oleaut32.dll",
    "comctl32.dll",
    "comdlg32.dll",
    "ws2_32.dll",
    "wsock32.dll",
    "msvcrt.dll",
    "ucrtbase.dll",
    "msvcp140.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "api-ms-win-crt-runtime-l1-1-0.dll",
    "api-ms-win-crt-heap-l1-1-0.dll",
    "api-ms-win-crt-math-l1-1-0.dll",
    "api-ms-win-crt-stdio-l1-1-0.dll",
    "api-ms-win-crt-string-l1-1-0.dll",
    "api-ms-win-crt-locale-l1-1-0.dll",
    "api-ms-win-crt-time-l1-1-0.dll",
    "api-ms-win-crt-convert-l1-1-0.dll",
    "api-ms-win-crt-environment-l1-1-0.dll",
    "api-ms-win-crt-filesystem-l1-1-0.dll",
    "api-ms-win-crt-process-l1-1-0.dll",
    "api-ms-win-crt-utility-l1-1-0.dll",
    "bcrypt.dll",
    "crypt32.dll",
    "secur32.dll",
    "shlwapi.dll",
    "imm32.dll",
    "winmm.dll",
    "setupapi.dll",
    "cfgmgr32.dll",
    "wintrust.dll",
    "version.dll",
    "d3d11.dll",
    "dxgi.dll",
    "opengl32.dll",
    "dbghelp.dll",
    "psapi.dll",
    "iphlpapi.dll",
    "userenv.dll",
    "powrprof.dll",
    "rpcrt4.dll",
    "sspicli.dll",
    "nsi.dll",
    "normaliz.dll",
];

/// Metadata stored in the precompiled bundle for the Windows worker to validate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrecompiledMetadata {
    pub perry_version: String,
    pub compiled_by: String,
    pub compile_timestamp: String,
}

/// Scan PE imports using pelite and copy non-system DLLs from the binary's directory
/// into the destination directory. Works cross-platform since pelite reads raw bytes.
pub fn scan_and_copy_dlls(binary_path: &Path, dest_dir: &Path) -> Result<Vec<String>, String> {
    let data = std::fs::read(binary_path)
        .map_err(|e| format!("Failed to read binary for DLL scanning: {e}"))?;

    let imports: Vec<String> = match pelite::PeFile::from_bytes(&data) {
        Ok(pelite::Wrap::T64(pe)) => {
            use pelite::pe64::Pe;
            match pe.imports() {
                Ok(imp) => imp
                    .iter()
                    .filter_map(|desc| {
                        desc.dll_name().ok().map(|s| s.to_str().unwrap_or("").to_string())
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
        Ok(pelite::Wrap::T32(pe)) => {
            use pelite::pe32::Pe;
            match pe.imports() {
                Ok(imp) => imp
                    .iter()
                    .filter_map(|desc| {
                        desc.dll_name().ok().map(|s| s.to_str().unwrap_or("").to_string())
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
        Err(e) => {
            tracing::warn!("Failed to parse PE for DLL scanning: {e}");
            return Ok(Vec::new());
        }
    };

    let binary_dir = binary_path.parent().unwrap_or(Path::new("."));
    let mut copied = Vec::new();

    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed to create DLL dest dir: {e}"))?;

    for dll_name in imports {
        let dll_lower = dll_name.to_lowercase();
        if SYSTEM_DLLS.contains(&dll_lower.as_str()) || dll_lower.starts_with("api-ms-win-") {
            continue;
        }
        let dll_src = binary_dir.join(&dll_name);
        if dll_src.exists() {
            let dll_dest = dest_dir.join(&dll_name);
            if !dll_dest.exists() {
                std::fs::copy(&dll_src, &dll_dest)
                    .map_err(|e| format!("Failed to copy DLL {dll_name}: {e}"))?;
                copied.push(dll_name);
            }
        }
    }

    Ok(copied)
}

/// Create a precompiled bundle tarball for the Windows sign-only worker.
///
/// Bundle structure:
/// ```text
/// perry-precompiled/
///   metadata.json       # PrecompiledMetadata
///   manifest.json       # BuildManifest (for validation)
///   {AppName}.exe       # Cross-compiled binary
///   app.ico             # Optional icon
///   dlls/               # Non-system DLL dependencies
/// ```
pub fn create_precompiled_bundle(
    manifest: &BuildManifest,
    binary_path: &Path,
    ico_path: Option<&Path>,
    dll_dir: Option<&Path>,
    perry_version: &str,
    tmpdir: &Path,
) -> Result<PathBuf, String> {
    let bundle_dir = tmpdir.join("perry-precompiled");
    std::fs::create_dir_all(&bundle_dir)
        .map_err(|e| format!("Failed to create bundle dir: {e}"))?;

    // Write metadata
    let metadata = PrecompiledMetadata {
        perry_version: perry_version.to_string(),
        compiled_by: hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "linux-worker".into()),
        compile_timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let metadata_json = serde_json::to_string_pretty(&metadata)
        .map_err(|e| format!("Failed to serialize metadata: {e}"))?;
    std::fs::write(bundle_dir.join("metadata.json"), &metadata_json)
        .map_err(|e| format!("Failed to write metadata: {e}"))?;

    // Write manifest
    let manifest_json = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("Failed to serialize manifest: {e}"))?;
    std::fs::write(bundle_dir.join("manifest.json"), &manifest_json)
        .map_err(|e| format!("Failed to write manifest: {e}"))?;

    // Copy binary
    let exe_name = format!("{}.exe", manifest.app_name);
    std::fs::copy(binary_path, bundle_dir.join(&exe_name))
        .map_err(|e| format!("Failed to copy exe: {e}"))?;

    // Copy ICO if present
    if let Some(ico) = ico_path {
        if ico.exists() {
            std::fs::copy(ico, bundle_dir.join("app.ico"))
                .map_err(|e| format!("Failed to copy ico: {e}"))?;
        }
    }

    // Copy DLLs if present
    if let Some(dll_src) = dll_dir {
        if dll_src.exists() {
            let dll_dest = bundle_dir.join("dlls");
            std::fs::create_dir_all(&dll_dest)
                .map_err(|e| format!("Failed to create dlls dir: {e}"))?;
            if let Ok(entries) = std::fs::read_dir(dll_src) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        let name = entry.file_name();
                        std::fs::copy(&path, dll_dest.join(&name))
                            .map_err(|e| format!("Failed to copy DLL {}: {e}", name.to_string_lossy()))?;
                    }
                }
            }
        }
    }

    // Create tar.gz bundle
    let tarball_path = tmpdir.join(format!("{}-precompiled.tar.gz", manifest.app_name));
    let tar_file = std::fs::File::create(&tarball_path)
        .map_err(|e| format!("Failed to create tarball: {e}"))?;
    let encoder = flate2::write::GzEncoder::new(tar_file, flate2::Compression::default());
    let mut tar_builder = tar::Builder::new(encoder);

    tar_builder
        .append_dir_all("perry-precompiled", &bundle_dir)
        .map_err(|e| format!("Failed to build tarball: {e}"))?;

    tar_builder
        .finish()
        .map_err(|e| format!("Failed to finish tarball: {e}"))?;

    // Clean up the staging dir
    std::fs::remove_dir_all(&bundle_dir).ok();

    Ok(tarball_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_dlls_lowercase() {
        for dll in SYSTEM_DLLS {
            assert_eq!(*dll, dll.to_lowercase(), "SYSTEM_DLLS must be lowercase: {dll}");
        }
    }
}
