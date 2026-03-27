use std::env;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub hub_ws_url: String,
    pub perry_binary: String,
    pub android_home: Option<String>,
    pub android_ndk_home: Option<String>,
    pub worker_name: Option<String>,
    pub hub_secret: Option<String>,
    /// When true, compile steps run inside a Docker container for isolation.
    pub docker_enabled: bool,
    /// Docker image to use for isolated builds (default: "perry-build")
    pub docker_image: String,
    /// Max concurrent builds (default 2).
    pub max_concurrent_builds: usize,
}

impl WorkerConfig {
    pub fn from_env() -> Self {
        Self {
            hub_ws_url: env::var("PERRY_HUB_URL")
                .unwrap_or_else(|_| "ws://localhost:3457".into()),
            perry_binary: env::var("PERRY_BUILD_PERRY_BINARY")
                .unwrap_or_else(|_| "perry".into()),
            android_home: env::var("PERRY_BUILD_ANDROID_HOME")
                .or_else(|_| env::var("ANDROID_HOME"))
                .ok(),
            android_ndk_home: env::var("PERRY_BUILD_ANDROID_NDK_HOME")
                .or_else(|_| env::var("ANDROID_NDK_HOME"))
                .ok(),
            worker_name: env::var("PERRY_WORKER_NAME").ok(),
            hub_secret: env::var("PERRY_HUB_WORKER_SECRET").ok(),
            docker_enabled: env::var("PERRY_DOCKER_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            docker_image: env::var("PERRY_DOCKER_IMAGE")
                .unwrap_or_else(|_| "perry-build".into()),
            max_concurrent_builds: env::var("PERRY_MAX_CONCURRENT_BUILDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2),
        }
    }
}
