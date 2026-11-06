use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use oshical_core::RuntimeSecrets;

pub(crate) const ALLOWED_WEB_ORIGINS: [&str; 5] = [
    "http://localhost:1420",
    "http://127.0.0.1:1420",
    "http://tauri.localhost",
    "https://tauri.localhost",
    "tauri://localhost",
];

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub database_path: PathBuf,
    pub update_dir: PathBuf,
    pub api_token: Option<String>,
    pub secrets: RuntimeSecrets,
    pub llm_base_url: Option<String>,
    pub llm_model: Option<String>,
    pub live_poll_seconds: Option<u64>,
    pub dynamic_poll_seconds: Option<u64>,
    pub proxy_port: Option<u16>,
}

impl ServerConfig {
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
        let secrets = RuntimeSecrets {
            bili_cookie: env::var("OSHICAL_BILI_COOKIE")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            llm_api_key: env::var("OSHICAL_LLM_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        };
        Ok(Self {
            bind,
            database_path,
            update_dir,
            api_token,
            secrets,
            llm_base_url,
            llm_model,
            live_poll_seconds,
            dynamic_poll_seconds,
            proxy_port,
        })
    }
}
