//! Google Play Store upload via Google Play Developer API v3
//!
//! Uses a service account JSON key to authenticate and upload AAB files
//! to Google Play for internal, alpha, beta, or production distribution.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug)]
pub struct UploadResult {
    pub message: String,
}

/// Upload an AAB to Google Play Store.
///
/// Steps:
/// 1. Parse service account JSON to get client_email and private_key
/// 2. Generate a JWT and exchange it for an OAuth2 access token
/// 3. Create an edit
/// 4. Upload the AAB bundle
/// 5. Assign the bundle to a track (internal, alpha, beta, production)
/// 6. Commit the edit
pub async fn upload_to_playstore(
    artifact_path: &Path,
    package_name: &str,
    service_account_json: Option<&str>,
    track: &str,
) -> Result<UploadResult, String> {
    let sa_json = service_account_json.ok_or_else(|| {
        "Google Play upload requires a service account JSON key. \
         Pass --google-play-key <path> or set PERRY_GOOGLE_PLAY_KEY_PATH."
            .to_string()
    })?;

    let sa: ServiceAccount =
        serde_json::from_str(sa_json).map_err(|e| format!("Invalid service account JSON: {e}"))?;

    // Step 1: Get OAuth2 access token
    let access_token = get_access_token(&sa.client_email, &sa.private_key).await?;

    let client = reqwest::Client::new();
    let api_base = format!(
        "https://androidpublisher.googleapis.com/androidpublisher/v3/applications/{package_name}"
    );

    // Step 2: Create an edit
    let edit_resp = client
        .post(format!("{api_base}/edits"))
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|e| format!("Failed to create edit: {e}"))?;

    if !edit_resp.status().is_success() {
        let status = edit_resp.status();
        let body = edit_resp.text().await.unwrap_or_default();
        return Err(format!("Failed to create edit ({status}): {body}"));
    }

    let edit: EditResponse = edit_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse edit response: {e}"))?;

    let edit_id = &edit.id;
    tracing::info!(edit_id, "Created Google Play edit");

    // Step 3: Upload AAB
    let aab_data = std::fs::read(artifact_path)
        .map_err(|e| format!("Failed to read AAB file: {e}"))?;
    let aab_size = aab_data.len();

    let upload_url = format!(
        "https://androidpublisher.googleapis.com/upload/androidpublisher/v3/applications/{package_name}/edits/{edit_id}/bundles"
    );

    let upload_resp = client
        .post(&upload_url)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(aab_data)
        .send()
        .await
        .map_err(|e| format!("Failed to upload AAB: {e}"))?;

    if !upload_resp.status().is_success() {
        let status = upload_resp.status();
        let body = upload_resp.text().await.unwrap_or_default();
        // Clean up: delete the edit on failure
        let _ = client
            .delete(format!("{api_base}/edits/{edit_id}"))
            .header(AUTHORIZATION, format!("Bearer {access_token}"))
            .send()
            .await;
        return Err(format!("Failed to upload AAB ({status}): {body}"));
    }

    let bundle: BundleResponse = upload_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse bundle response: {e}"))?;

    let version_code = bundle.version_code;
    tracing::info!(version_code, "Uploaded AAB bundle");

    // Step 4: Assign to track
    let track_url = format!("{api_base}/edits/{edit_id}/tracks/{track}");
    let track_body = TrackUpdate {
        track: track.to_string(),
        releases: vec![TrackRelease {
            version_codes: vec![version_code.to_string()],
            status: "completed".to_string(),
        }],
    };

    let track_resp = client
        .put(&track_url)
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(CONTENT_TYPE, "application/json")
        .json(&track_body)
        .send()
        .await
        .map_err(|e| format!("Failed to update track: {e}"))?;

    if !track_resp.status().is_success() {
        let status = track_resp.status();
        let body = track_resp.text().await.unwrap_or_default();
        let _ = client
            .delete(format!("{api_base}/edits/{edit_id}"))
            .header(AUTHORIZATION, format!("Bearer {access_token}"))
            .send()
            .await;
        return Err(format!(
            "Failed to assign bundle to {track} track ({status}): {body}"
        ));
    }

    tracing::info!(track, version_code, "Assigned bundle to track");

    // Step 5: Commit the edit
    let commit_resp = client
        .post(format!("{api_base}/edits/{edit_id}:commit"))
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .send()
        .await
        .map_err(|e| format!("Failed to commit edit: {e}"))?;

    if !commit_resp.status().is_success() {
        let status = commit_resp.status();
        let body = commit_resp.text().await.unwrap_or_default();
        return Err(format!("Failed to commit edit ({status}): {body}"));
    }

    tracing::info!(edit_id, "Committed Google Play edit");

    Ok(UploadResult {
        message: format!(
            "Uploaded AAB ({:.1} MB) to Google Play {track} track (version code {version_code})",
            aab_size as f64 / 1_048_576.0
        ),
    })
}

/// Exchange a service account's private key for an OAuth2 access token.
async fn get_access_token(client_email: &str, private_key: &str) -> Result<String, String> {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("System time error: {e}"))?
        .as_secs();

    let claims = JwtClaims {
        iss: client_email.to_string(),
        scope: "https://www.googleapis.com/auth/androidpublisher".to_string(),
        aud: "https://oauth2.googleapis.com/token".to_string(),
        iat: now,
        exp: now + 3600,
    };

    let header = Header::new(Algorithm::RS256);
    let key = EncodingKey::from_rsa_pem(private_key.as_bytes())
        .map_err(|e| format!("Invalid RSA private key in service account: {e}"))?;

    let jwt = encode(&header, &claims, &key)
        .map_err(|e| format!("Failed to sign JWT: {e}"))?;

    // Exchange JWT for access token
    let client = reqwest::Client::new();
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &jwt),
        ])
        .send()
        .await
        .map_err(|e| format!("Failed to request OAuth2 token: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("OAuth2 token request failed ({status}): {body}"));
    }

    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse token response: {e}"))?;

    Ok(token_resp.access_token)
}

// --- API types ---

#[derive(Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
}

#[derive(Serialize)]
struct JwtClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct EditResponse {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BundleResponse {
    version_code: u64,
}

#[derive(Serialize)]
struct TrackUpdate {
    track: String,
    releases: Vec<TrackRelease>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackRelease {
    version_codes: Vec<String>,
    status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_service_account_json() {
        let json = r#"{
            "type": "service_account",
            "project_id": "test-project",
            "client_email": "test@test-project.iam.gserviceaccount.com",
            "private_key": "-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n-----END RSA PRIVATE KEY-----\n"
        }"#;
        let sa: ServiceAccount = serde_json::from_str(json).unwrap();
        assert_eq!(sa.client_email, "test@test-project.iam.gserviceaccount.com");
        assert!(sa.private_key.contains("BEGIN RSA PRIVATE KEY"));
    }

    #[test]
    fn test_parse_service_account_invalid_json() {
        let json = r#"{ "not_valid": true }"#;
        let result: Result<ServiceAccount, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_upload_rejects_missing_credentials() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(upload_to_playstore(
            std::path::Path::new("/nonexistent.aab"),
            "com.example.app",
            None,
            "internal",
        ));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("service account JSON key"));
    }

    #[test]
    fn test_upload_rejects_invalid_json() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(upload_to_playstore(
            std::path::Path::new("/nonexistent.aab"),
            "com.example.app",
            Some("not json"),
            "internal",
        ));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid service account JSON"));
    }

    #[test]
    fn test_jwt_claims_serialization() {
        let claims = JwtClaims {
            iss: "test@example.com".into(),
            scope: "https://www.googleapis.com/auth/androidpublisher".into(),
            aud: "https://oauth2.googleapis.com/token".into(),
            iat: 1000,
            exp: 4600,
        };
        let json = serde_json::to_string(&claims).unwrap();
        assert!(json.contains("androidpublisher"));
        assert!(json.contains("test@example.com"));
    }

    #[test]
    fn test_track_update_serialization() {
        let update = TrackUpdate {
            track: "internal".into(),
            releases: vec![TrackRelease {
                version_codes: vec!["42".into()],
                status: "completed".into(),
            }],
        };
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("\"versionCodes\""), "should use camelCase: {json}");
        assert!(json.contains("\"42\""));
        assert!(json.contains("\"completed\""));
    }

    #[test]
    fn test_bundle_response_deserialization() {
        let json = r#"{"versionCode": 42, "sha256": "abc123"}"#;
        let bundle: BundleResponse = serde_json::from_str(json).unwrap();
        assert_eq!(bundle.version_code, 42);
    }

    #[test]
    fn test_edit_response_deserialization() {
        let json = r#"{"id": "edit-123", "expiryTimeSeconds": "3600"}"#;
        let edit: EditResponse = serde_json::from_str(json).unwrap();
        assert_eq!(edit.id, "edit-123");
    }
}
