use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use oshical_core::settings::{LlmAccount, ReasoningEffort};
use oshical_core::RuntimeSecrets;
use serde::Deserialize;

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

/// 部署方用 `OSHICAL_LLM_ACCOUNTS` 声明的账号：账号定义 + 可选密钥。
///
/// 密钥只留在这个进程内存里，启动时交给
/// [`crate::secrets::seed_llm_secrets_from_env`] 写进持久化槽位，
/// 之后所有读路径仍然只有「按账号读持久化密钥」一条。
#[derive(Debug, Clone)]
pub struct EnvLlmAccount {
    pub account: LlmAccount,
    pub api_key: Option<String>,
}

impl EnvLlmAccount {
    pub fn id(&self) -> &str {
        &self.account.id
    }
}

/// `OSHICAL_LLM_ACCOUNTS` 里单个元素的原始形态（camelCase，与设置里的 `LlmAccount` 同名）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLlmAccount {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    api_key: Option<String>,
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
    /// `OSHICAL_LLM_ACCOUNTS` 声明的账号池；为空表示完全用设置里的账号。
    pub llm_accounts: Vec<EnvLlmAccount>,
    pub live_poll_seconds: Option<u64>,
    pub dynamic_poll_seconds: Option<u64>,
    pub proxy_port: Option<u16>,
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

    /// 环境变量声明的账号密钥，形态为 `(account_id, api_key, base_url)`。
    ///
    /// 启动时逐条写进持久化槽位，密钥最终只存在于数据库里；
    /// 没有 `apiKey` 的账号不出现（它的密钥由界面单独保存）。
    pub fn env_llm_account_keys(&self) -> Vec<(&str, &str, Option<&str>)> {
        self.llm_accounts
            .iter()
            .filter_map(|entry| {
                let api_key = entry.api_key.as_deref()?;
                let base_url =
                    Some(entry.account.base_url.as_str()).filter(|value| !value.trim().is_empty());
                Some((entry.account.id.as_str(), api_key, base_url))
            })
            .collect()
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
        let llm_accounts = parse_llm_accounts_env();
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
            llm_reasoning_effort,
            llm_parallelism,
            llm_accounts,
            live_poll_seconds,
            dynamic_poll_seconds,
            proxy_port,
        })
    }
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

/// 读 `OSHICAL_LLM_ACCOUNTS`：JSON 数组，元素 `{id,name,baseUrl,model,apiKey?}`。
///
/// 整段解析失败就忽略并打一行警告：账号池写坏时服务端仍要能启动，
/// 界面里保存的账号继续生效。单条 id 不合法或重复只丢这一条。
fn parse_llm_accounts_env() -> Vec<EnvLlmAccount> {
    let Ok(raw) = env::var("OSHICAL_LLM_ACCOUNTS") else {
        return Vec::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let raw_accounts: Vec<RawLlmAccount> = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            eprintln!(
                "[oshical] 忽略无效的环境变量 OSHICAL_LLM_ACCOUNTS（期望 JSON 数组）: {error}"
            );
            return Vec::new();
        }
    };
    let mut accounts: Vec<EnvLlmAccount> = Vec::with_capacity(raw_accounts.len());
    for raw_account in raw_accounts {
        let account_id = match crate::secrets::validate_llm_account_id(&raw_account.id) {
            Ok(value) => value,
            Err(_) => {
                eprintln!(
                    "[oshical] 忽略 OSHICAL_LLM_ACCOUNTS 中 id 不合法的账号 {:?}（只允许字母、数字与 `-`、`_`、`.`，且不超过 {} 个字符）",
                    raw_account.id,
                    crate::secrets::LLM_ACCOUNT_ID_MAX_CHARS
                );
                continue;
            }
        };
        if accounts
            .iter()
            .any(|existing| existing.account.id == account_id)
        {
            eprintln!("[oshical] 忽略 OSHICAL_LLM_ACCOUNTS 中重复的账号 {account_id}");
            continue;
        }
        let api_key = raw_account
            .api_key
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        accounts.push(EnvLlmAccount {
            account: LlmAccount {
                id: account_id,
                name: raw_account.name.trim().to_owned(),
                base_url: raw_account.base_url.trim().to_owned(),
                model: raw_account.model.trim().to_owned(),
            },
            api_key,
        });
    }
    accounts
}

fn warn_ignored_env(name: &str, value: &str, expected: &str) {
    eprintln!("[oshical] 忽略无效的环境变量 {name}={value:?}（期望 {expected}），改用设置里的值");
}
