use serde::{Deserialize, Serialize};

use crate::{error::AppResult, store::SqliteStore};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DataMode {
    #[default]
    Local,
    Server,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub data_mode: DataMode,
    pub server_url: String,
    pub live_poll_seconds: u64,
    pub dynamic_poll_seconds: u64,
    pub auto_analyze_images: bool,
    pub notifications_enabled: bool,
    #[serde(default)]
    pub live_stats_enabled: bool,
    pub llm_base_url: String,
    pub llm_model: String,
    pub llm_prompt_version: String,
    #[serde(default)]
    pub proxy_port: Option<u16>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            data_mode: DataMode::Local,
            server_url: String::new(),
            live_poll_seconds: 20,
            dynamic_poll_seconds: 300,
            auto_analyze_images: true,
            notifications_enabled: true,
            live_stats_enabled: false,
            llm_base_url: "https://api.openai.com/v1".to_owned(),
            llm_model: String::new(),
            llm_prompt_version: "schedule-v1".to_owned(),
            proxy_port: None,
        }
    }
}

impl AppSettings {
    pub fn validate(&self) -> AppResult<()> {
        if !(10..=300).contains(&self.live_poll_seconds) {
            return Err(crate::error::AppError::validation(
                "开播探测间隔必须在 10 到 300 秒之间",
            ));
        }
        if !(300..=3600).contains(&self.dynamic_poll_seconds) {
            return Err(crate::error::AppError::validation(
                "动态刷新间隔必须在 300 到 3600 秒之间",
            ));
        }
        crate::network::validate_proxy_port(self.proxy_port)?;
        if self.data_mode == DataMode::Server {
            let url = url::Url::parse(&self.server_url)?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(crate::error::AppError::validation(
                    "服务器地址必须使用 http 或 https",
                ));
            }
        }
        if !self.llm_base_url.is_empty() {
            let url = url::Url::parse(&self.llm_base_url)?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(crate::error::AppError::validation(
                    "模型地址必须使用 http 或 https",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct RuntimeSecrets {
    pub bili_cookie: Option<String>,
    pub llm_api_key: Option<String>,
}

pub const SETTING_KEYS: [&str; 11] = [
    "data_mode",
    "server_url",
    "live_poll_seconds",
    "dynamic_poll_seconds",
    "auto_analyze_images",
    "notifications_enabled",
    "live_stats_enabled",
    "llm_base_url",
    "llm_model",
    "llm_prompt_version",
    "proxy_port",
];

pub async fn load_settings(store: &SqliteStore) -> AppResult<AppSettings> {
    let mut settings = AppSettings::default();
    for key in SETTING_KEYS {
        let Some(value) = store.get_setting(key).await? else {
            continue;
        };
        match key {
            "data_mode" => settings.data_mode = serde_json::from_str(&value)?,
            "server_url" => settings.server_url = serde_json::from_str(&value)?,
            "live_poll_seconds" => settings.live_poll_seconds = serde_json::from_str(&value)?,
            "dynamic_poll_seconds" => settings.dynamic_poll_seconds = serde_json::from_str(&value)?,
            "auto_analyze_images" => settings.auto_analyze_images = serde_json::from_str(&value)?,
            "notifications_enabled" => {
                settings.notifications_enabled = serde_json::from_str(&value)?
            }
            "live_stats_enabled" => settings.live_stats_enabled = serde_json::from_str(&value)?,
            "llm_base_url" => settings.llm_base_url = serde_json::from_str(&value)?,
            "llm_model" => settings.llm_model = serde_json::from_str(&value)?,
            "llm_prompt_version" => settings.llm_prompt_version = serde_json::from_str(&value)?,
            "proxy_port" => settings.proxy_port = serde_json::from_str(&value)?,
            _ => {}
        }
    }
    settings.validate()?;
    Ok(settings)
}

pub async fn save_settings(store: &SqliteStore, settings: &AppSettings) -> AppResult<()> {
    settings.validate()?;
    let values = [
        ("data_mode", serde_json::to_string(&settings.data_mode)?),
        ("server_url", serde_json::to_string(&settings.server_url)?),
        (
            "live_poll_seconds",
            serde_json::to_string(&settings.live_poll_seconds)?,
        ),
        (
            "dynamic_poll_seconds",
            serde_json::to_string(&settings.dynamic_poll_seconds)?,
        ),
        (
            "auto_analyze_images",
            serde_json::to_string(&settings.auto_analyze_images)?,
        ),
        (
            "notifications_enabled",
            serde_json::to_string(&settings.notifications_enabled)?,
        ),
        (
            "live_stats_enabled",
            serde_json::to_string(&settings.live_stats_enabled)?,
        ),
        (
            "llm_base_url",
            serde_json::to_string(&settings.llm_base_url)?,
        ),
        ("llm_model", serde_json::to_string(&settings.llm_model)?),
        (
            "llm_prompt_version",
            serde_json::to_string(&settings.llm_prompt_version)?,
        ),
        ("proxy_port", serde_json::to_string(&settings.proxy_port)?),
    ];
    store.set_settings(&values).await
}

impl std::fmt::Debug for RuntimeSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSecrets")
            .field("bili_cookie_configured", &self.bili_cookie.is_some())
            .field("llm_api_key_configured", &self.llm_api_key.is_some())
            .finish()
    }
}
