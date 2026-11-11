pub mod api;
pub mod bili_login;
pub mod config;
pub mod jobs;
pub mod runtime;
pub mod secrets;
pub mod sync_api;
pub mod update_manager;
pub mod youtube_oauth;

pub use api::{build_router, AppState};
pub use config::ServerConfig;
pub use jobs::{
    JobRegistration, RefreshJob, RefreshJobAccepted, RefreshJobKind, RefreshJobManager,
    RefreshJobProgress, RefreshJobStatus,
};
pub use runtime::{run_server, spawn_background_tasks};
pub use secrets::SecretVault;
pub use update_manager::{ServerUpdateManager, ServerUpdateSettings, ServerUpdateView};
