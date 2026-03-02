//! Linux publishing — currently a no-op.
//! Future: Flatpak/Snap store upload, GitHub Releases, etc.

use std::path::Path;

pub async fn publish_artifact(_artifact_path: &Path) -> Result<(), String> {
    // No centralized Linux app store to upload to.
    Ok(())
}
