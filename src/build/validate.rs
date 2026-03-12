//! Input validation and sanitization for build manifests.
//!
//! All user-controlled fields must be validated before use in file paths,
//! shell commands, XML/plist generation, or Gradle DSL interpolation.

use crate::queue::job::BuildManifest;

/// Validate all user-controlled manifest fields before building.
pub fn validate_manifest(manifest: &BuildManifest) -> Result<(), String> {
    validate_app_name(&manifest.app_name)?;
    validate_bundle_id(&manifest.bundle_id)?;
    validate_version(&manifest.version)?;
    validate_entry(&manifest.entry)?;

    if let Some(ref sv) = manifest.short_version {
        validate_version_field(sv, "short_version")?;
    }
    if let Some(ref icon) = manifest.icon {
        validate_relative_path(icon, "icon")?;
    }
    if let Some(ref v) = manifest.minimum_os_version {
        validate_version_field(v, "minimum_os_version")?;
    }
    if let Some(ref v) = manifest.ios_deployment_target {
        validate_version_field(v, "ios_deployment_target")?;
    }
    if let Some(ref sdk) = manifest.android_min_sdk {
        validate_numeric(sdk, "android_min_sdk")?;
    }
    if let Some(ref sdk) = manifest.android_target_sdk {
        validate_numeric(sdk, "android_target_sdk")?;
    }
    if let Some(ref permissions) = manifest.android_permissions {
        for p in permissions {
            validate_permission(p)?;
        }
    }
    if let Some(ref entitlements) = manifest.entitlements {
        for e in entitlements {
            validate_entitlement(e)?;
        }
    }
    if let Some(ref capabilities) = manifest.ios_capabilities {
        for c in capabilities {
            validate_entitlement(c)?;
        }
    }
    if let Some(ref cat) = manifest.category {
        validate_reverse_dns(cat, "category")?;
    }

    Ok(())
}

fn validate_app_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("app_name cannot be empty".into());
    }
    if name.len() > 200 {
        return Err("app_name is too long (max 200 characters)".into());
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ' ')
    {
        return Err(format!(
            "app_name contains invalid characters (only alphanumeric, space, hyphen, underscore allowed): {name}"
        ));
    }
    Ok(())
}

fn validate_bundle_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("bundle_id cannot be empty".into());
    }
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(format!(
            "bundle_id contains invalid characters (only alphanumeric, dot, hyphen, underscore allowed): {id}"
        ));
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<(), String> {
    if version.is_empty() {
        return Err("version cannot be empty".into());
    }
    validate_version_field(version, "version")
}

fn validate_version_field(value: &str, field: &str) -> Result<(), String> {
    if !value.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(format!(
            "{field} must contain only digits and dots, got: {value}"
        ));
    }
    Ok(())
}

fn validate_entry(entry: &str) -> Result<(), String> {
    if entry.is_empty() {
        return Err("entry cannot be empty".into());
    }
    validate_relative_path(entry, "entry")
}

fn validate_relative_path(path: &str, field: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return Err(format!("{field} must be a relative path, got: {path}"));
    }
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("{field} contains path traversal (..): {path}"));
    }
    Ok(())
}

fn validate_numeric(value: &str, field: &str) -> Result<(), String> {
    if !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("{field} must be numeric, got: {value}"));
    }
    Ok(())
}

fn validate_permission(perm: &str) -> Result<(), String> {
    if !perm
        .chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '_')
    {
        return Err(format!(
            "android permission contains invalid characters: {perm}"
        ));
    }
    Ok(())
}

fn validate_entitlement(ent: &str) -> Result<(), String> {
    if !ent
        .chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(format!(
            "entitlement/capability contains invalid characters: {ent}"
        ));
    }
    Ok(())
}

fn validate_reverse_dns(value: &str, field: &str) -> Result<(), String> {
    if !value
        .chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(format!("{field} contains invalid characters: {value}"));
    }
    Ok(())
}

/// Escape XML special characters for safe interpolation into XML documents.
pub fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_app_name() {
        assert!(validate_app_name("MyApp").is_ok());
        assert!(validate_app_name("My App").is_ok());
        assert!(validate_app_name("my-app_2").is_ok());
    }

    #[test]
    fn test_invalid_app_name() {
        assert!(validate_app_name("").is_err());
        assert!(validate_app_name("../evil").is_err());
        assert!(validate_app_name("my/app").is_err());
        assert!(validate_app_name("app;rm -rf /").is_err());
    }

    #[test]
    fn test_valid_bundle_id() {
        assert!(validate_bundle_id("com.example.app").is_ok());
        assert!(validate_bundle_id("com.my-company.app_1").is_ok());
    }

    #[test]
    fn test_invalid_bundle_id() {
        assert!(validate_bundle_id("").is_err());
        assert!(validate_bundle_id("com.evil\"}.exec()").is_err());
    }

    #[test]
    fn test_entry_path_traversal() {
        assert!(validate_entry("src/main.ts").is_ok());
        assert!(validate_entry("../../etc/passwd").is_err());
        assert!(validate_entry("/absolute/path.ts").is_err());
    }

    #[test]
    fn test_escape_xml() {
        assert_eq!(escape_xml("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        assert_eq!(escape_xml("say \"hi\""), "say &quot;hi&quot;");
    }
}
