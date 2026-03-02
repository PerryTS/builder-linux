use std::env;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub hub_ws_url: String,
    pub perry_binary: String,
    pub android_home: Option<String>,
    pub android_ndk_home: Option<String>,
    pub worker_name: Option<String>,
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
        }
    }
}
