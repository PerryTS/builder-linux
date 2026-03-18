//! Build job types — manifest, credentials, and status.
//! These types are shared between the hub (which creates them) and the worker (which uses them).

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildManifest {
    pub app_name: String,
    pub bundle_id: String,
    pub version: String,
    pub short_version: Option<String>,
    pub entry: String,
    pub icon: Option<String>,
    pub targets: Vec<String>,
    pub category: Option<String>,
    pub minimum_os_version: Option<String>,
    pub entitlements: Option<Vec<String>>,
    // iOS-specific fields
    #[serde(default)]
    pub ios_deployment_target: Option<String>,
    #[serde(default)]
    pub ios_device_family: Option<Vec<String>>,
    #[serde(default)]
    pub ios_orientations: Option<Vec<String>>,
    #[serde(default)]
    pub ios_capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub ios_distribute: Option<String>,
    // Android-specific fields
    #[serde(default)]
    pub android_min_sdk: Option<String>,
    #[serde(default)]
    pub android_target_sdk: Option<String>,
    #[serde(default)]
    pub android_permissions: Option<Vec<String>>,
    #[serde(default)]
    pub android_distribute: Option<String>,
    // Linux-specific fields
    #[serde(default)]
    pub linux_format: Option<String>,
    #[serde(default)]
    pub linux_category: Option<String>,
    #[serde(default)]
    pub linux_description: Option<String>,
    // Windows-specific fields (pass-through for cross-compiled bundles)
    #[serde(default)]
    pub windows_distribute: Option<String>,
    #[serde(default)]
    pub windows_uac_level: Option<String>,
    #[serde(default)]
    pub windows_dpi_aware: Option<String>,
    #[serde(default)]
    pub windows_file_description: Option<String>,
    #[serde(default)]
    pub windows_company_name: Option<String>,
    #[serde(default)]
    pub windows_copyright: Option<String>,
}

#[derive(Debug, Zeroize, ZeroizeOnDrop, serde::Deserialize)]
pub struct BuildCredentials {
    pub apple_team_id: Option<String>,
    pub apple_signing_identity: Option<String>,
    pub apple_key_id: Option<String>,
    pub apple_issuer_id: Option<String>,
    pub apple_p8_key: Option<String>,
    /// Base64-encoded provisioning profile for iOS
    #[serde(default)]
    pub provisioning_profile_base64: Option<String>,
    /// Base64-encoded .jks keystore for Android signing
    #[serde(default)]
    pub android_keystore_base64: Option<String>,
    #[serde(default)]
    pub android_keystore_password: Option<String>,
    #[serde(default)]
    pub android_key_alias: Option<String>,
    #[serde(default)]
    pub android_key_password: Option<String>,
    /// Google Play service account JSON for Play Store uploads
    #[serde(default)]
    pub google_play_service_account_json: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credentials_backward_compatible() {
        // Old payloads without google_play_service_account_json should still parse
        let json = r#"{
            "apple_team_id": "ABC123",
            "apple_signing_identity": "Developer ID Application: Test",
            "apple_key_id": null,
            "apple_issuer_id": null,
            "apple_p8_key": null
        }"#;
        let creds: BuildCredentials = serde_json::from_str(json).unwrap();
        assert_eq!(creds.apple_team_id.as_deref(), Some("ABC123"));
        assert!(creds.google_play_service_account_json.is_none());
        assert!(creds.android_keystore_base64.is_none());
    }

    #[test]
    fn test_credentials_with_google_play() {
        let json = r#"{
            "apple_team_id": null,
            "apple_signing_identity": null,
            "apple_key_id": null,
            "apple_issuer_id": null,
            "apple_p8_key": null,
            "android_keystore_base64": "dGVzdA==",
            "android_keystore_password": "pass123",
            "android_key_alias": "key0",
            "google_play_service_account_json": "{\"client_email\":\"test@gcp.iam\"}"
        }"#;
        let creds: BuildCredentials = serde_json::from_str(json).unwrap();
        assert!(creds.google_play_service_account_json.is_some());
        assert!(creds.google_play_service_account_json.clone().unwrap().contains("client_email"));
    }

    #[test]
    fn test_manifest_with_android_distribute() {
        let json = r#"{
            "app_name": "TestApp",
            "bundle_id": "com.test.app",
            "version": "1.0.0",
            "entry": "src/main.ts",
            "targets": ["android"],
            "android_distribute": "playstore"
        }"#;
        let manifest: BuildManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.android_distribute.as_deref(), Some("playstore"));
        assert_eq!(manifest.targets, vec!["android"]);
    }
}
