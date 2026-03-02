//! Linux binary signing — currently a no-op.
//! Future: GPG signing support.

use std::path::Path;

pub async fn sign_binary(_binary_path: &Path) -> Result<(), String> {
    // Linux desktop apps don't require code signing.
    Ok(())
}
