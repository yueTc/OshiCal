use std::env;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use oshical_core::settings::ReasoningEffort;
use oshical_core::RuntimeSecrets;

pub(crate) const ALLOWED_WEB_ORIGINS: [&str; 5] = [
    "http://localhost:1420",
    "http://127.0.0.1:1420",
    "http://tauri.localhost",
    "https://tauri.localhost",
    "tauri://localhost",
];

/// 并行个数的合法区间，与 `AppSettings::validate` 保持一致。
const LLM_PARALLELISM_MIN: u8 = 1;
const LLM_PARALLELISM_MAX: u8 = 8;

#[derive(Clone, Default)]
pub struct YouTubeConfig {
    pub oauth_access_token: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_redirect_uri: Option<String>,
    pub web_session_cookie: Option<String>,
}

impl fmt::Debug for YouTubeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YouTubeConfig")
            .field(
                "oauth_access_token_configured",
                &self.oauth_access_token.is_some(),
            )
            .field("oauth_client_id_configured", &self.oauth_client_id.is_some())
            .field(
                "oauth_client_secret_configured",
                &self.oauth_client_secret.is_some(),
            )
            .field(
                "oauth_redirect_uri_configured",
                &self.oauth_redirect_uri.is_some(),
            )
            .field(
                "web_session_cookie_configured",
                &self.web_session_cookie.is_some(),
            )
            .finish()
    }
}

impl YouTubeConfig {
    fn from_env(bind: SocketAddr) -> Result<Self, String> {
        let oauth_access_token = env_optional("OSHICAL_YOUTUBE_OAUTH_ACCESS_TOKEN");
        let oauth_client_id = env_optional("OSHICAL_YOUTUBE_CLIENT_ID");
        let oauth_client_secret = env_optional("OSHICAL_YOUTUBE_CLIENT_SECRET");
        let oauth_redirect_uri = env_optional("OSHICAL_YOUTUBE_OAUTH_REDIRECT_URI").or_else(|| {
            bind.ip()
                .is_loopback()
                .then(|| format!("http://{bind}/api/youtube/oauth/callback"))
        });
        if let Some(redirect_uri) = oauth_redirect_uri.as_deref() {
            validate_oauth_redirect_uri(redirect_uri)?;
        }
        if oauth_client_id.is_some() != oauth_client_secret.is_some() {
            tracing::warn!(
                "YouTube OAuth 配置不完整：OSHICAL_YOUTUBE_CLIENT_ID 与 OSHICAL_YOUTUBE_CLIENT_SECRET 必须同时设置"
            );
        }
        Ok(Self {
            oauth_access_token,
            oauth_client_id,
            oauth_client_secret,
            oauth_redirect_uri,
            web_session_cookie: env_optional("OSHICAL_YOUTUBE_WEB_SESSION_COOKIE"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub database_path: PathBuf,
    pub update_dir: PathBuf,
    pub api_token: Option<String>,
    pub secrets: RuntimeSecrets,
    pub llm_base_url: Option<String>,
    pub llm_model: Option<String>,
    /// `OSHICAL_LLM_REASONING_EFFORT` 覆盖；`None` 表示沿用设置里的值。
    pub llm_reasoning_effort: Option<ReasoningEffort>,
    /// `OSHICAL_LLM_PARALLELISM` 覆盖；`None` 表示沿用设置里的值。
    pub llm_parallelism: Option<u8>,
    pub live_poll_seconds: Option<u64>,
    pub dynamic_poll_seconds: Option<u64>,
    pub proxy_port: Option<u16>,
    pub youtube: YouTubeConfig,
}

impl ServerConfig {
    /// 环境变量优先于设置：部署方改 env 就能压过界面上保存的值。
    /// 非法值只打一行警告并忽略，不阻断启动（否则一次笔误就起不来）。
    pub fn effective_llm_reasoning_effort(
        &self,
        from_settings: ReasoningEffort,
    ) -> ReasoningEffort {
        self.llm_reasoning_effort.unwrap_or(from_settings)
    }

    /// 并行度覆盖；未设置时回落设置值，最后再夹到 `1..=8`。
    pub fn effective_llm_parallelism(&self, from_settings: u8) -> u8 {
        self.llm_parallelism
            .unwrap_or(from_settings)
            .clamp(LLM_PARALLELISM_MIN, LLM_PARALLELISM_MAX)
    }

    pub fn from_env() -> Result<Self, String> {
        let bind = env::var("OSHICAL_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8787".to_owned())
            .parse::<SocketAddr>()
            .map_err(|error| format!("OSHICAL_BIND 无效: {error}"))?;
        let api_token = env::var("OSHICAL_API_TOKEN")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if !bind.ip().is_loopback() {
            let token = api_token
                .as_deref()
                .ok_or_else(|| "服务器绑定非回环地址时必须设置 OSHICAL_API_TOKEN".to_owned())?;
            if token.len() < 32 || token.chars().any(char::is_control) {
                return Err(
                    "非回环部署的 OSHICAL_API_TOKEN 必须至少 32 字节且不能包含控制字符".to_owned(),
                );
            }
        }

        let database_path = env::var("OSHICAL_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./data/oshical.db"));
        let update_dir = env::var("OSHICAL_UPDATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                database_path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join("system")
            });
        let llm_base_url = env::var("OSHICAL_LLM_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let llm_model = env::var("OSHICAL_LLM_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let llm_reasoning_effort = parse_llm_reasoning_effort_env();
        let llm_parallelism = parse_llm_parallelism_env();
        let live_poll_seconds = env::var("OSHICAL_LIVE_POLL_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        let dynamic_poll_seconds = env::var("OSHICAL_DYNAMIC_POLL_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        let proxy_port = env::var("OSHICAL_PROXY_PORT")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse::<u16>()
                    .map_err(|error| format!("OSHICAL_PROXY_PORT 无效: {error}"))
            })
            .transpose()?;
        oshical_core::validate_proxy_port(proxy_port)
            .map_err(|error| format!("OSHICAL_PROXY_PORT 无效: {error}"))?;
        let youtube = YouTubeConfig::from_env(bind)?;
        let secrets = RuntimeSecrets {
            bili_cookie: env::var("OSHICAL_BILI_COOKIE")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            llm_api_key: env::var("OSHICAL_LLM_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            // The final user must save their own key through SecretVault.
            // This is intentionally never seeded from the process environment.
            youtube_api_key: None,
            youtube_oauth_access_token: youtube.oauth_access_token.clone(),
            youtube_web_cookie: youtube.web_session_cookie.clone(),
        };
        Ok(Self {
            bind,
            database_path,
            update_dir,
            api_token,
            secrets,
            llm_base_url,
            llm_model,
            llm_reasoning_effort,
            llm_parallelism,
            live_poll_seconds,
            dynamic_poll_seconds,
            proxy_port,
            youtube,
        })
    }
}

pub(crate) fn validate_oauth_redirect_uri(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value)
        .map_err(|error| format!("OSHICAL_YOUTUBE_OAUTH_REDIRECT_URI 无效: {error}"))?;
    if url.fragment().is_some() {
        return Err(
            "OSHICAL_YOUTUBE_OAUTH_REDIRECT_URI 不能包含 fragment".to_owned(),
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "OSHICAL_YOUTUBE_OAUTH_REDIRECT_URI 不能包含用户名或密码".to_owned(),
        );
    }
    let host = url
        .host_str()
        .ok_or_else(|| "OSHICAL_YOUTUBE_OAUTH_REDIRECT_URI 必须包含主机名".to_owned())?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_redirect_host(host) => Ok(()),
        "http" => Err(
            "OSHICAL_YOUTUBE_OAUTH_REDIRECT_URI 只有 localhost、127.0.0.1 或 [::1] 可使用 http；其它主机必须使用 https"
                .to_owned(),
        ),
        _ => Err(
            "OSHICAL_YOUTUBE_OAUTH_REDIRECT_URI 必须是 http 或 https 地址".to_owned(),
        ),
    }
}

fn is_loopback_redirect_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let ip_host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    ip_host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn env_optional(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// 读 `OSHICAL_LLM_REASONING_EFFORT`：`auto` / `low` / `medium` / `high`，大小写不敏感。
fn parse_llm_reasoning_effort_env() -> Option<ReasoningEffort> {
    let raw = env::var("OSHICAL_LLM_REASONING_EFFORT").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match ReasoningEffort::parse(trimmed) {
        Some(effort) => Some(effort),
        None => {
            warn_ignored_env(
                "OSHICAL_LLM_REASONING_EFFORT",
                trimmed,
                "auto|low|medium|high",
            );
            None
        }
    }
}

/// 读 `OSHICAL_LLM_PARALLELISM`：整数，合法区间 `1..=8`。
fn parse_llm_parallelism_env() -> Option<u8> {
    let raw = env::var("OSHICAL_LLM_PARALLELISM").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<u8>() {
        Ok(value) if (LLM_PARALLELISM_MIN..=LLM_PARALLELISM_MAX).contains(&value) => Some(value),
        _ => {
            warn_ignored_env("OSHICAL_LLM_PARALLELISM", trimmed, "1..=8");
            None
        }
    }
}

fn warn_ignored_env(name: &str, value: &str, expected: &str) {
    eprintln!("[oshical] 忽略无效的环境变量 {name}={value:?}（期望 {expected}），改用设置里的值");
}

#[cfg(test)]
mod tests {
    use super::validate_oauth_redirect_uri;

    #[test]
    fn oauth_redirect_allows_loopback_http_and_remote_https() {
        for value in [
            "http://localhost:8787/api/youtube/oauth/callback",
            "http://127.0.0.1:8787/api/youtube/oauth/callback",
            "http://[::1]:8787/api/youtube/oauth/callback",
            "https://oshical.example.com/api/youtube/oauth/callback",
        ] {
            validate_oauth_redirect_uri(value).unwrap_or_else(|error| panic!("{value}: {error}"));
        }
    }

    #[test]
    fn oauth_redirect_rejects_remote_http_and_unsafe_components() {
        for value in [
            "http://oshical.example.com/api/youtube/oauth/callback",
            "http://0.0.0.0:8787/api/youtube/oauth/callback",
            "https://user:pass@oshical.example.com/callback",
            "https://oshical.example.com/callback#fragment",
            "ftp://localhost/callback",
        ] {
            assert!(
                validate_oauth_redirect_uri(value).is_err(),
                "unsafe redirect URI was accepted: {value}"
            );
        }
    }
}
