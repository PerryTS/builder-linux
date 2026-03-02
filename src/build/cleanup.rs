use std::path::{Path, PathBuf};
use uuid::Uuid;

pub fn create_build_tmpdir() -> std::io::Result<PathBuf> {
    let id = Uuid::new_v4();
    let path = std::env::temp_dir().join(format!("perry-build-{id}"));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn cleanup_tmpdir(path: &Path) {
    if path.exists() {
        if let Err(e) = std::fs::remove_dir_all(path) {
            tracing::warn!(path = %path.display(), error = %e, "Failed to clean up build tmpdir");
        } else {
            // Verify it's actually gone
            if path.exists() {
                tracing::warn!(path = %path.display(), "Build tmpdir still exists after removal");
            }
        }
    }
}
