use serde::{Deserialize, Serialize};

use crate::{error::AppResult, store::SqliteStore};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DataMode {
    #[default]
    Local,
    Server,
}

/// 模型「思考程度」。`Auto` 表示请求体不带 `reasoning_effort` 字段。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Auto,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// `Auto` 返回 `None`，请求体不带该字段。
    pub fn as_request_value(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
        }
    }

    /// 大小写不敏感；未知值返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
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
    /// YouTube 功能总开关；关闭时不发起任何 YouTube 请求。
    #[serde(default)]
    pub youtube_enabled: bool,
    /// 「寻血猎犬」：开启后才抓取 LAPLACE 生理期数据，默认关闭。
    #[serde(default)]
    pub fertility_enabled: bool,
    pub llm_base_url: String,
    pub llm_model: String,
    pub llm_prompt_version: String,
    /// 思考程度，默认 `Auto`（请求体不带 `reasoning_effort`）。
    #[serde(default)]
    pub llm_reasoning_effort: ReasoningEffort,
    /// 并行个数：同时处理的独立主播任务上限，同主播动态串行；模型请求同样受限，合法1..=8。
    /// 裸 `#[serde(default)]` 对 u8 会给 0（不合法），所以缺字段时必须回落到 1。
    #[serde(default = "default_llm_parallelism")]
    pub llm_parallelism: u8,
    #[serde(default)]
    pub proxy_port: Option<u16>,
}

/// `#[serde(default)]` 对 u8 会给出 0（不合法），缺字段时回落到 1 才符合「默认 1」。
pub const fn default_llm_parallelism() -> u8 {
    1
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
            youtube_enabled: false,
            fertility_enabled: false,
            llm_base_url: "https://api.openai.com/v1".to_owned(),
            llm_model: String::new(),
            llm_prompt_version: "schedule-v1".to_owned(),
            llm_reasoning_effort: ReasoningEffort::Auto,
            llm_parallelism: default_llm_parallelism(),
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
        if !(1..=8).contains(&self.llm_parallelism) {
            return Err(crate::error::AppError::validation(
                "并行个数必须在 1 到 8 之间",
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
    pub youtube_api_key: Option<String>,
    pub youtube_oauth_access_token: Option<String>,
    pub youtube_web_cookie: Option<String>,
}

pub const SETTING_KEYS: [&str; 15] = [
    "data_mode",
    "server_url",
    "live_poll_seconds",
    "dynamic_poll_seconds",
    "auto_analyze_images",
    "notifications_enabled",
    "live_stats_enabled",
    "youtube_enabled",
    "fertility_enabled",
    "llm_base_url",
    "llm_model",
    "llm_prompt_version",
    "llm_reasoning_effort",
    "llm_parallelism",
    "proxy_port",
];

/// 兼容 `"high"`（JSON 字符串）与裸 `high`，大小写不敏感；无法识别时告警并保留默认值。
fn parse_reasoning_effort_setting(raw: &str) -> Option<ReasoningEffort> {
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    if let Some(effort) = ReasoningEffort::parse(unquoted) {
        return Some(effort);
    }
    tracing::warn!(value = raw, "思考程度设置无法识别，保留默认值");
    None
}

/// 兼容 `1` 与 `"1"`；越界（0 或 9 以上）同样回退默认值，避免一条脏数据锁死全部设置。
fn parse_parallelism_setting(raw: &str) -> Option<u8> {
    let parsed = serde_json::from_str::<u8>(raw)
        .ok()
        .or_else(|| raw.trim().parse::<u8>().ok());
    match parsed {
        Some(value) if (1..=8).contains(&value) => Some(value),
        _ => {
            tracing::warn!(value = raw, "并行个数设置无法识别，保留默认值");
            None
        }
    }
}

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
            "youtube_enabled" => settings.youtube_enabled = serde_json::from_str(&value)?,
            "fertility_enabled" => settings.fertility_enabled = serde_json::from_str(&value)?,
            "llm_base_url" => settings.llm_base_url = serde_json::from_str(&value)?,
            "llm_model" => settings.llm_model = serde_json::from_str(&value)?,
            "llm_prompt_version" => settings.llm_prompt_version = serde_json::from_str(&value)?,
            "llm_reasoning_effort" => {
                if let Some(effort) = parse_reasoning_effort_setting(&value) {
                    settings.llm_reasoning_effort = effort;
                }
            }
            "llm_parallelism" => {
                if let Some(parallelism) = parse_parallelism_setting(&value) {
                    settings.llm_parallelism = parallelism;
                }
            }
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
            "youtube_enabled",
            serde_json::to_string(&settings.youtube_enabled)?,
        ),
        (
            "fertility_enabled",
            serde_json::to_string(&settings.fertility_enabled)?,
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
        (
            "llm_reasoning_effort",
            serde_json::to_string(&settings.llm_reasoning_effort)?,
        ),
        (
            "llm_parallelism",
            serde_json::to_string(&settings.llm_parallelism)?,
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
            .field(
                "youtube_api_key_configured",
                &self.youtube_api_key.is_some(),
            )
            .field(
                "youtube_oauth_access_token_configured",
                &self.youtube_oauth_access_token.is_some(),
            )
            .field(
                "youtube_web_cookie_configured",
                &self.youtube_web_cookie.is_some(),
            )
            .finish()
    }
}
