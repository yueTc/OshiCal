pub mod bilibili;
pub mod error;
pub mod fertility;
pub mod live_stats;
pub mod llm;
pub mod models;
pub mod network;
pub mod schedule;
pub mod service;
pub mod settings;
pub mod store;
pub mod weekly;

pub use error::{AppError, AppResult};
pub use llm::{
    ContentAnalyzer, LlmConfig, LlmDiscoveryConfig, LlmEvent, LlmEventAction, LlmTestReport,
};
pub use models::*;
pub use network::{
    client_builder as proxy_client_builder, proxy_url, test_google as test_proxy_google,
    validate_proxy_port, ProxyTestInput, ProxyTestReport, PROXY_HOST,
};
pub use service::{AppService, FAN_POLL_INTERVAL_SECONDS};
pub use settings::{load_settings, save_settings, AppSettings, DataMode, RuntimeSecrets};
pub use store::{SqliteStore, SyncPrunePolicy, SYNC_SCHEMA_VERSION};
