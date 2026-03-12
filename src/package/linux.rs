//! Linux desktop packaging — AppImage, .deb, and .tar.gz
//!
//! External tools required:
//! - `appimagetool` (for AppImage output) — https://github.com/AppImage/appimagetool
//! - `dpkg-deb` (for .deb output) — available on Debian/Ubuntu

use crate::queue::job::BuildManifest;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Default freedesktop category if none is specified.
const DEFAULT_CATEGORY: &str = "Utility";

/// Supported Linux packaging formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxFormat {
    AppImage,
    Deb,
    Tarball,
}

impl LinuxFormat {
    pub fn from_str_or_default(s: Option<&str>) -> Self {
        match s {
            Some("deb") => Self::Deb,
            Some("tarball") | Some("tar.gz") => Self::Tarball,
            _ => Self::AppImage,
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            Self::AppImage => "AppImage",
            Self::Deb => "deb",
            Self::Tarball => "tar.gz",
        }
    }
}

/// Create a Linux package in the requested format. Returns the path to the final artifact.
pub fn package(
    manifest: &BuildManifest,
    binary_path: &Path,
    icon_path: Option<&Path>,
    format: LinuxFormat,
    tmpdir: &Path,
) -> Result<PathBuf, String> {
    match format {
        LinuxFormat::AppImage => create_appimage(manifest, binary_path, icon_path, tmpdir),
        LinuxFormat::Deb => create_deb(manifest, binary_path, icon_path, tmpdir),
        LinuxFormat::Tarball => create_tarball(manifest, binary_path, icon_path, tmpdir),
    }
}

/// Build an AppImage by constructing an AppDir and calling `appimagetool`.
fn create_appimage(
    manifest: &BuildManifest,
    binary_path: &Path,
    icon_path: Option<&Path>,
    tmpdir: &Path,
) -> Result<PathBuf, String> {
    let bin_name = binary_name(&manifest.app_name);
    let appdir = tmpdir.join(format!("{}.AppDir", manifest.app_name));

    // Create AppDir structure
    let usr_bin = appdir.join("usr/bin");
    let usr_share_icons = appdir.join("usr/share/icons/hicolor/256x256/apps");
    std::fs::create_dir_all(&usr_bin).map_err(|e| format!("Create usr/bin: {e}"))?;
    std::fs::create_dir_all(&usr_share_icons).map_err(|e| format!("Create icons dir: {e}"))?;

    // Copy binary
    std::fs::copy(binary_path, usr_bin.join(&bin_name))
        .map_err(|e| format!("Copy binary: {e}"))?;
    set_executable(&usr_bin.join(&bin_name))?;

    // Copy icon
    let icon_filename = format!("{bin_name}.png");
    if let Some(icon) = icon_path {
        if icon.exists() {
            std::fs::copy(icon, usr_share_icons.join(&icon_filename))
                .map_err(|e| format!("Copy icon to share: {e}"))?;
            // AppImage also wants icon at root
            std::fs::copy(icon, appdir.join(&icon_filename))
                .map_err(|e| format!("Copy icon to root: {e}"))?;
        }
    }

    // Generate .desktop file
    let category = manifest.linux_category.as_deref().unwrap_or(DEFAULT_CATEGORY);
    let desktop_content = generate_desktop_file(&manifest.app_name, &bin_name, &icon_filename, category);
    let desktop_path = appdir.join(format!("{bin_name}.desktop"));
    std::fs::write(&desktop_path, &desktop_content)
        .map_err(|e| format!("Write .desktop: {e}"))?;

    // Generate AppRun script
    let apprun_content = format!(
        "#!/bin/sh\nHERE=\"$(dirname \"$(readlink -f \"$0\")\")\"\nexec \"$HERE/usr/bin/{bin_name}\" \"$@\"\n"
    );
    let apprun_path = appdir.join("AppRun");
    std::fs::write(&apprun_path, &apprun_content)
        .map_err(|e| format!("Write AppRun: {e}"))?;
    set_executable(&apprun_path)?;

    // Run appimagetool
    let output_path = tmpdir.join(format!(
        "{}-{}-x86_64.AppImage",
        manifest.app_name, manifest.version
    ));
    let status = std::process::Command::new("appimagetool")
        .arg(&appdir)
        .arg(&output_path)
        .env("ARCH", "x86_64")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| format!("Failed to run appimagetool: {e}. Is it installed?"))?;

    if !status.success() {
        return Err(format!("appimagetool exited with status {status}"));
    }

    Ok(output_path)
}

/// Build a .deb package using dpkg-deb.
fn create_deb(
    manifest: &BuildManifest,
    binary_path: &Path,
    icon_path: Option<&Path>,
    tmpdir: &Path,
) -> Result<PathBuf, String> {
    let bin_name = binary_name(&manifest.app_name);
    let deb_root = tmpdir.join(format!("{}_{}_amd64", bin_name, manifest.version));

    // Create directory structure
    let debian_dir = deb_root.join("DEBIAN");
    let usr_bin = deb_root.join("usr/bin");
    let usr_share_apps = deb_root.join("usr/share/applications");
    let usr_share_icons = deb_root.join("usr/share/icons/hicolor/256x256/apps");

    for dir in [&debian_dir, &usr_bin, &usr_share_apps, &usr_share_icons] {
        std::fs::create_dir_all(dir).map_err(|e| format!("Create dir: {e}"))?;
    }

    // Copy binary
    std::fs::copy(binary_path, usr_bin.join(&bin_name))
        .map_err(|e| format!("Copy binary: {e}"))?;
    set_executable(&usr_bin.join(&bin_name))?;

    // Copy icon
    if let Some(icon) = icon_path {
        if icon.exists() {
            std::fs::copy(icon, usr_share_icons.join(format!("{bin_name}.png")))
                .map_err(|e| format!("Copy icon: {e}"))?;
        }
    }

    // Generate .desktop file
    // Sanitize category: strip newlines to prevent .desktop field injection
    let category = manifest.linux_category.as_deref().unwrap_or(DEFAULT_CATEGORY);
    let safe_category: String = category.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let desktop_content =
        generate_desktop_file(&manifest.app_name, &bin_name, &bin_name, &safe_category);
    std::fs::write(
        usr_share_apps.join(format!("{bin_name}.desktop")),
        &desktop_content,
    )
    .map_err(|e| format!("Write .desktop: {e}"))?;

    // Generate DEBIAN/control
    // Sanitize description: strip newlines to prevent deb control field injection
    let description = manifest
        .linux_description
        .as_deref()
        .unwrap_or(&manifest.app_name);
    let safe_description: String = description.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let control = generate_deb_control(&manifest.app_name, &manifest.version, &safe_description);
    std::fs::write(debian_dir.join("control"), &control)
        .map_err(|e| format!("Write control: {e}"))?;

    // Build .deb
    let output_path = tmpdir.join(format!("{}_{}_amd64.deb", bin_name, manifest.version));
    let status = std::process::Command::new("dpkg-deb")
        .args(["--build", "--root-owner-group"])
        .arg(&deb_root)
        .arg(&output_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| format!("Failed to run dpkg-deb: {e}. Is it installed?"))?;

    if !status.success() {
        return Err(format!("dpkg-deb exited with status {status}"));
    }

    Ok(output_path)
}

/// Create a simple .tar.gz archive.
fn create_tarball(
    manifest: &BuildManifest,
    binary_path: &Path,
    icon_path: Option<&Path>,
    tmpdir: &Path,
) -> Result<PathBuf, String> {
    let bin_name = binary_name(&manifest.app_name);
    let prefix = format!("{}-{}-linux-x86_64", bin_name, manifest.version);

    let output_path = tmpdir.join(format!("{prefix}.tar.gz"));
    let file = std::fs::File::create(&output_path)
        .map_err(|e| format!("Create tar.gz: {e}"))?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut ar = tar::Builder::new(encoder);

    // Add binary
    ar.append_path_with_name(binary_path, format!("{prefix}/bin/{bin_name}"))
        .map_err(|e| format!("Add binary to tar: {e}"))?;

    // Add icon if present
    if let Some(icon) = icon_path {
        if icon.exists() {
            ar.append_path_with_name(icon, format!("{prefix}/share/icons/{bin_name}.png"))
                .map_err(|e| format!("Add icon to tar: {e}"))?;
        }
    }

    ar.finish().map_err(|e| format!("Finish tar: {e}"))?;

    Ok(output_path)
}

/// Generate a freedesktop .desktop file.
fn generate_desktop_file(
    app_name: &str,
    bin_name: &str,
    icon_name: &str,
    category: &str,
) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={app_name}\n\
         Exec={bin_name}\n\
         Icon={icon_name}\n\
         Categories={category};\n\
         Terminal=false\n"
    )
}

/// Generate the DEBIAN/control file for a .deb package.
fn generate_deb_control(app_name: &str, version: &str, description: &str) -> String {
    let bin_name = binary_name(app_name);
    format!(
        "Package: {bin_name}\n\
         Version: {version}\n\
         Section: utils\n\
         Priority: optional\n\
         Architecture: amd64\n\
         Maintainer: Perry <noreply@perry.build>\n\
         Description: {description}\n"
    )
}

/// Convert an app name to a filesystem-safe binary name (lowercase, hyphens).
fn binary_name(app_name: &str) -> String {
    app_name
        .to_lowercase()
        .replace(' ', "-")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn set_executable(path: &Path) -> Result<(), String> {
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| format!("Set executable: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_name() {
        assert_eq!(binary_name("My App"), "my-app");
        assert_eq!(binary_name("HelloWorld"), "helloworld");
        assert_eq!(binary_name("test_app-2"), "test_app-2");
    }

    #[test]
    fn test_linux_format_parse() {
        assert_eq!(LinuxFormat::from_str_or_default(None), LinuxFormat::AppImage);
        assert_eq!(
            LinuxFormat::from_str_or_default(Some("deb")),
            LinuxFormat::Deb
        );
        assert_eq!(
            LinuxFormat::from_str_or_default(Some("tarball")),
            LinuxFormat::Tarball
        );
        assert_eq!(
            LinuxFormat::from_str_or_default(Some("tar.gz")),
            LinuxFormat::Tarball
        );
        assert_eq!(
            LinuxFormat::from_str_or_default(Some("unknown")),
            LinuxFormat::AppImage
        );
    }

    #[test]
    fn test_desktop_file() {
        let content = generate_desktop_file("My App", "my-app", "my-app.png", "Utility");
        assert!(content.contains("Name=My App"));
        assert!(content.contains("Exec=my-app"));
        assert!(content.contains("Icon=my-app.png"));
        assert!(content.contains("Categories=Utility;"));
    }

    #[test]
    fn test_deb_control() {
        let control = generate_deb_control("My App", "1.2.3", "A test application");
        assert!(control.contains("Package: my-app"));
        assert!(control.contains("Version: 1.2.3"));
        assert!(control.contains("Architecture: amd64"));
    }
}
