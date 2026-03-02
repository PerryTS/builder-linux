use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StageName {
    Queued,
    Extracting,
    Compiling,
    GeneratingAssets,
    Bundling,
    Signing,
    Notarizing,
    Packaging,
    Uploading,
    Publishing,
    Complete,
}

impl std::fmt::Display for StageName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Queued => "queued",
            Self::Extracting => "extracting",
            Self::Compiling => "compiling",
            Self::GeneratingAssets => "generating_assets",
            Self::Bundling => "bundling",
            Self::Signing => "signing",
            Self::Notarizing => "notarizing",
            Self::Packaging => "packaging",
            Self::Uploading => "uploading",
            Self::Publishing => "publishing",
            Self::Complete => "complete",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    LicenseInvalid,
    LicenseTier,
    UploadTooLarge,
    RateLimited,
    QueueFull,
    CompileFailed,
    SigningFailed,
    NotarizeFailed,
    PackageFailed,
    InternalError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    JobCreated {
        job_id: Uuid,
        position: usize,
        estimated_wait_secs: Option<u64>,
    },
    QueueUpdate {
        position: usize,
        estimated_wait_secs: Option<u64>,
    },
    Stage {
        stage: StageName,
        message: String,
    },
    Log {
        stage: StageName,
        line: String,
        stream: LogStream,
    },
    Progress {
        stage: StageName,
        percent: u8,
        message: Option<String>,
    },
    ArtifactReady {
        artifact_name: String,
        artifact_size: u64,
        sha256: String,
        download_url: String,
        expires_in_secs: u64,
    },
    Published {
        platform: String,
        message: String,
        url: Option<String>,
    },
    Error {
        code: ErrorCode,
        message: String,
        stage: Option<StageName>,
    },
    Complete {
        job_id: Uuid,
        success: bool,
        duration_secs: f64,
        artifacts: Vec<ArtifactInfo>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub download_url: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Cancel,
    Ping,
}
