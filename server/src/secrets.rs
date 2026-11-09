use std::fmt;
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use chrono::{DateTime, Utc};
use oshical_core::{AppError, SqliteStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

/// 旧版（单密钥时代）的状态键。只作为 `default` 账号的兜底读取来源，
/// 写入一律走带账号后缀的键。
const LLM_SECRET_STATE_KEY: &str = "server:secret:llm_api_key:v1";
const BILI_SECRET_STATE_KEY: &str = "server:secret:bilibili_cookie:v1";
/// 旧版 AAD，与 [`LLM_SECRET_STATE_KEY`] 配套，只用于兜底解 `default` 的旧密文。
const LLM_AAD: &[u8] = b"oshical:server-secret:llm-api-key:v1";
/// 账号级 AAD 前缀，完整形态为 `{LLM_AAD_PREFIX}:{account_id}`。
const LLM_AAD_PREFIX: &str = "oshical:server-secret:llm-api-key:v1";
const BILI_AAD: &[u8] = b"oshical:server-secret:bilibili-cookie:v1";
const NONCE_BYTES: usize = 12;
const LLM_PENDING_URL_BINDING_TTL_SECONDS: i64 = 15 * 60;
/// 请求里没带 `accountId` 时落到的账号。旧客户端只会碰这个账号。
pub const DEFAULT_LLM_ACCOUNT_ID: &str = "default";
/// 账号 id 长度上限，与设置里 `llm_accounts[].id` 的约束对齐。
pub const LLM_ACCOUNT_ID_MAX_CHARS: usize = 64;
static SECRET_WRITE_LOCK: Mutex<()> = Mutex::const_new(());

/// 账号 id 归一化：去首尾空白，空值回落 `default`。
///
/// 状态键与 AAD 都从这里取值，所以 `" x "` 与 `"x"` 必须落到同一个槽位，
/// 否则同一个账号会写出两行互不可见的纪录。
pub fn normalize_llm_account_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        DEFAULT_LLM_ACCOUNT_ID.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// 校验并归一化账号 id。
///
/// 账号 id 会拼进状态键，所以只放行字母、数字与 `-_.`：否则一个带 `:` 或空白的
/// id 就能把密文写到命名空间的别处，或与别的账号共用槽位。
pub fn validate_llm_account_id(raw: &str) -> Result<String, AppError> {
    let account = normalize_llm_account_id(raw);
    let legal = account.chars().count() <= LLM_ACCOUNT_ID_MAX_CHARS
        && account
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'));
    if !legal {
        return Err(AppError::validation(format!(
            "账号 id 只能包含字母、数字、`-`、`_`、`.`，且不能超过 {LLM_ACCOUNT_ID_MAX_CHARS} 个字符"
        )));
    }
    Ok(account)
}

/// 一个账号的密钥槽位：状态键 + AAD 同时绑定账号 id。
///
/// 换账号就换 AAD，即使密文被搬到别的键下也解不开，避免串号。
#[derive(Debug, Clone)]
struct LlmSecretSlot {
    state_key: String,
    aad: Vec<u8>,
}

impl LlmSecretSlot {
    fn for_account(account_id: &str) -> Self {
        let normalized = normalize_llm_account_id(account_id);
        Self {
            state_key: format!("{LLM_SECRET_STATE_KEY}:{normalized}"),
            aad: format!("{LLM_AAD_PREFIX}:{normalized}").into_bytes(),
        }
    }

    /// 0.3.19 及更早版本的单密钥槽位：状态键无后缀，AAD 也不带后缀。
    fn legacy_default() -> Self {
        Self {
            state_key: LLM_SECRET_STATE_KEY.to_owned(),
            aad: LLM_AAD.to_vec(),
        }
    }
}

#[derive(Clone)]
pub struct SecretVault {
    key: [u8; 32],
}

impl fmt::Debug for SecretVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretVault")
            .field("key", &"[redacted]")
            .finish()
    }
}

impl SecretVault {
    pub fn from_env(api_token: Option<&str>, database_path: &Path) -> Self {
        let material = std::env::var("OSHICAL_SECRETS_KEY")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| match api_token {
                Some(token) => format!("oshical-api-token:{token}"),
                None => format!("oshical-local-db:{}", database_path.display()),
            });
        Self::from_material(&material)
    }

    pub fn from_material(material: &str) -> Self {
        let digest = Sha256::digest(material.as_bytes());
        let mut key = [0_u8; 32];
        key.copy_from_slice(&digest);
        Self { key }
    }

    fn encrypt_with_aad(&self, plaintext: &str, aad: &[u8]) -> Result<String, AppError> {
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|error| AppError::External {
            code: "SECRET_VAULT".to_owned(),
            message: format!("初始化密钥加密器失败: {error}"),
        })?;
        let nonce_bytes = Uuid::new_v4().into_bytes();
        let nonce = Nonce::from_slice(&nonce_bytes[..NONCE_BYTES]);
        let ciphertext = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad,
                },
            )
            .map_err(|_| AppError::External {
                code: "SECRET_VAULT".to_owned(),
                message: "服务端密钥加密失败".to_owned(),
            })?;
        let mut encoded = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        encoded.extend_from_slice(nonce);
        encoded.extend_from_slice(&ciphertext);
        Ok(BASE64.encode(encoded))
    }

    fn decrypt_with_aad(&self, encoded: &str, aad: &[u8]) -> Result<String, AppError> {
        let bytes = BASE64.decode(encoded).map_err(|_| AppError::External {
            code: "SECRET_VAULT".to_owned(),
            message: "服务端密钥密文格式无效".to_owned(),
        })?;
        if bytes.len() <= NONCE_BYTES {
            return Err(AppError::External {
                code: "SECRET_VAULT".to_owned(),
                message: "服务端密钥密文长度无效".to_owned(),
            });
        }
        let cipher = Aes256Gcm::new_from_slice(&self.key).map_err(|error| AppError::External {
            code: "SECRET_VAULT".to_owned(),
            message: format!("初始化密钥解密器失败: {error}"),
        })?;
        let (nonce, ciphertext) = bytes.split_at(NONCE_BYTES);
        let plaintext = cipher
            .decrypt(
                nonce.into(),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| AppError::External {
                code: "SECRET_VAULT".to_owned(),
                message: "服务端密钥解密失败，请重新保存该密钥".to_owned(),
            })?;
        String::from_utf8(plaintext).map_err(|_| AppError::External {
            code: "SECRET_VAULT".to_owned(),
            message: "服务端密钥明文编码无效".to_owned(),
        })
    }

    /// 旧版单密钥加密（AAD 不带账号后缀）。只用于兼容与联调，业务写入走
    /// [`SecretVault::encrypt_llm_key`]。
    pub fn encrypt(&self, plaintext: &str) -> Result<String, AppError> {
        self.encrypt_with_aad(plaintext, LLM_AAD)
    }

    /// 旧版单密钥解密（AAD 不带账号后缀），与 [`SecretVault::encrypt`] 配套。
    pub fn decrypt(&self, encoded: &str) -> Result<String, AppError> {
        self.decrypt_with_aad(encoded, LLM_AAD)
    }

    fn encrypt_llm_key(&self, plaintext: &str, slot: &LlmSecretSlot) -> Result<String, AppError> {
        self.encrypt_with_aad(plaintext, &slot.aad)
    }

    fn decrypt_llm_key(&self, encoded: &str, slot: &LlmSecretSlot) -> Result<String, AppError> {
        self.decrypt_with_aad(encoded, &slot.aad)
    }

    fn encrypt_bili_cookie(&self, cookie: &str) -> Result<String, AppError> {
        self.encrypt_with_aad(cookie, BILI_AAD)
    }

    fn decrypt_bili_cookie(&self, encoded: &str) -> Result<String, AppError> {
        self.decrypt_with_aad(encoded, BILI_AAD)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSecret {
    ciphertext: String,
    revision: i64,
    updated_at: String,
    #[serde(default)]
    bound_base_url: Option<String>,
    #[serde(default)]
    pending_url_binding: bool,
}

#[derive(Debug, Clone)]
pub struct PersistedLlmSecret {
    pub account_id: String,
    pub api_key: String,
    pub revision: i64,
    pub changed: bool,
    pub bound_base_url: Option<String>,
    pub pending_url_binding: bool,
    pub requires_url_binding: bool,
}

/// 「这个账号到底配没配密钥」的状态快照，不含明文。
#[derive(Debug, Clone, Default)]
pub struct LlmSecretStatus {
    pub configured: bool,
    pub revision: i64,
    pub bound_base_url: Option<String>,
    pub pending_url_binding: bool,
}

#[derive(Debug, Clone)]
pub struct PersistedBiliSecret {
    pub cookie: String,
    pub revision: i64,
    pub changed: bool,
}

async fn load_stored_llm_secret(
    store: &SqliteStore,
    account_id: &str,
) -> Result<Option<(StoredSecret, LlmSecretSlot)>, AppError> {
    let account = normalize_llm_account_id(account_id);
    let slot = LlmSecretSlot::for_account(&account);
    if let Some(raw) = store.get_client_sync_state(&slot.state_key).await? {
        return Ok(Some((serde_json::from_str(&raw)?, slot)));
    }
    // `default` 账号还要能读出 0.3.19 之前的单密钥密文（旧键 + 旧 AAD）。
    // 其它账号没有历史数据，直接返回未配置。
    if account == DEFAULT_LLM_ACCOUNT_ID {
        let legacy = LlmSecretSlot::legacy_default();
        if let Some(raw) = store.get_client_sync_state(&legacy.state_key).await? {
            return Ok(Some((serde_json::from_str(&raw)?, legacy)));
        }
    }
    Ok(None)
}

async fn store_llm_secret(
    store: &SqliteStore,
    slot: &LlmSecretSlot,
    stored: &StoredSecret,
) -> Result<(), AppError> {
    store
        .set_client_sync_state(&slot.state_key, &serde_json::to_string(stored)?)
        .await
}

fn pending_url_binding_is_fresh(stored: &StoredSecret) -> bool {
    if !stored.pending_url_binding {
        return false;
    }
    DateTime::parse_from_rfc3339(&stored.updated_at)
        .map(|updated_at| {
            let age_seconds = Utc::now()
                .signed_duration_since(updated_at.with_timezone(&Utc))
                .num_seconds();
            (0..=LLM_PENDING_URL_BINDING_TTL_SECONDS).contains(&age_seconds)
        })
        .unwrap_or(false)
}

fn persisted_llm_secret(
    stored: &StoredSecret,
    slot: &LlmSecretSlot,
    vault: &SecretVault,
    account_id: &str,
    changed: bool,
) -> Result<PersistedLlmSecret, AppError> {
    Ok(PersistedLlmSecret {
        account_id: normalize_llm_account_id(account_id),
        api_key: vault.decrypt_llm_key(&stored.ciphertext, slot)?,
        revision: stored.revision,
        changed,
        bound_base_url: stored.bound_base_url.clone(),
        pending_url_binding: pending_url_binding_is_fresh(stored),
        requires_url_binding: stored.pending_url_binding,
    })
}

/// 读取默认账号（`default`）的模型密钥，等价于改造前的全局单密钥入口。
pub async fn load_persisted_llm_secret(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedLlmSecret>, AppError> {
    load_persisted_llm_secret_for_account(store, vault, DEFAULT_LLM_ACCOUNT_ID).await
}

/// 按账号读取模型密钥。账号不存在时返回 `None`，不报错。
pub async fn load_persisted_llm_secret_for_account(
    store: &SqliteStore,
    vault: &SecretVault,
    account_id: &str,
) -> Result<Option<PersistedLlmSecret>, AppError> {
    let Some((stored, slot)) = load_stored_llm_secret(store, account_id).await? else {
        return Ok(None);
    };
    persisted_llm_secret(&stored, &slot, vault, account_id, false).map(Some)
}

/// 只回报「是否已配置」，不解出明文。
///
/// 解密仍然做一次：密文损坏（例如换了 `OSHICAL_SECRETS_KEY`）时必须报错，
/// 否则界面会显示「已配置」而实际调不通。
pub async fn llm_secret_status_for_account(
    store: &SqliteStore,
    vault: &SecretVault,
    account_id: &str,
) -> Result<LlmSecretStatus, AppError> {
    let Some((stored, slot)) = load_stored_llm_secret(store, account_id).await? else {
        return Ok(LlmSecretStatus::default());
    };
    vault.decrypt_llm_key(&stored.ciphertext, &slot)?;
    Ok(LlmSecretStatus {
        configured: true,
        revision: stored.revision,
        bound_base_url: stored.bound_base_url.clone(),
        pending_url_binding: pending_url_binding_is_fresh(&stored),
    })
}

/// 写入默认账号（`default`）的模型密钥，保留改造前签名供 `runtime` 等旧调用点使用。
pub async fn save_persisted_llm_secret(
    store: &SqliteStore,
    vault: &SecretVault,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<PersistedLlmSecret, AppError> {
    save_persisted_llm_secret_for_account(store, vault, DEFAULT_LLM_ACCOUNT_ID, api_key, base_url)
        .await
}

/// 按账号写入模型密钥。同值重写不涨 `revision`，只更新绑定状态。
pub async fn save_persisted_llm_secret_for_account(
    store: &SqliteStore,
    vault: &SecretVault,
    account_id: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<PersistedLlmSecret, AppError> {
    let account = normalize_llm_account_id(account_id);
    let slot = LlmSecretSlot::for_account(&account);
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let previous = load_stored_llm_secret(store, &account).await?;
    let previous_key = match previous.as_ref() {
        Some((stored, previous_slot)) => {
            Some(vault.decrypt_llm_key(&stored.ciphertext, previous_slot)?)
        }
        None => None,
    };
    let now = Utc::now().to_rfc3339();

    if previous_key.as_deref() == Some(api_key) {
        let (stored, previous_slot) = previous.as_ref().ok_or_else(|| AppError::External {
            code: "SECRET_CACHE_INCONSISTENT".to_owned(),
            message: "模型密钥缓存状态不一致".to_owned(),
        })?;
        let mut stored = stored.clone();
        stored.updated_at = now;
        match base_url {
            Some(base_url) => {
                stored.bound_base_url = Some(base_url.to_owned());
                stored.pending_url_binding = false;
            }
            None => stored.pending_url_binding = true,
        }
        // 命中的是旧版单密钥槽位时，趁这次「值没变」的写入把密文迁到账号槽位：
        // AAD 变了必须重新加密，但语义上没换密钥，`revision` 保持不变。
        let target_slot = if previous_slot.state_key == slot.state_key {
            previous_slot.clone()
        } else {
            stored.ciphertext = vault.encrypt_llm_key(api_key, &slot)?;
            slot.clone()
        };
        store_llm_secret(store, &target_slot, &stored).await?;
        return persisted_llm_secret(&stored, &target_slot, vault, &account, false);
    }

    let revision = previous
        .as_ref()
        .map_or(1, |(stored, _)| stored.revision.saturating_add(1));
    let stored = StoredSecret {
        ciphertext: vault.encrypt_llm_key(api_key, &slot)?,
        revision,
        updated_at: now,
        bound_base_url: base_url.map(ToOwned::to_owned),
        pending_url_binding: base_url.is_none(),
    };
    store_llm_secret(store, &slot, &stored).await?;
    persisted_llm_secret(&stored, &slot, vault, &account, true)
}

/// 绑定默认账号（`default`）的密钥到某个 base URL，保留改造前签名。
pub async fn bind_persisted_llm_secret_url(
    store: &SqliteStore,
    vault: &SecretVault,
    base_url: &str,
) -> Result<PersistedLlmSecret, AppError> {
    bind_persisted_llm_secret_url_for_account(store, vault, DEFAULT_LLM_ACCOUNT_ID, base_url).await
}

/// 按账号把已保存的密钥绑定到 `base_url`（首次成功探测后调用）。
pub async fn bind_persisted_llm_secret_url_for_account(
    store: &SqliteStore,
    vault: &SecretVault,
    account_id: &str,
    base_url: &str,
) -> Result<PersistedLlmSecret, AppError> {
    let account = normalize_llm_account_id(account_id);
    let slot = LlmSecretSlot::for_account(&account);
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let (stored, previous_slot) = load_stored_llm_secret(store, &account)
        .await?
        .ok_or_else(|| AppError::NotFound("模型密钥".to_owned()))?;
    let mut stored = stored;
    if previous_slot.state_key != slot.state_key {
        // 旧槽位的密文换了 AAD 就解不开，必须先解再按账号 AAD 重新加密。
        let plaintext = vault.decrypt_llm_key(&stored.ciphertext, &previous_slot)?;
        stored.ciphertext = vault.encrypt_llm_key(&plaintext, &slot)?;
    }
    stored.bound_base_url = Some(base_url.to_owned());
    stored.pending_url_binding = false;
    stored.updated_at = Utc::now().to_rfc3339();
    store_llm_secret(store, &slot, &stored).await?;
    persisted_llm_secret(&stored, &slot, vault, &account, false)
}

/// 把部署配置（例如 `OSHICAL_LLM_ACCOUNTS`）里的账号密钥写进持久化槽位。
///
/// 传 `(account_id, api_key, base_url)` 三元组：策略上环境变量优先，
/// 写进去以后所有读路径（`/api/secrets/llm`、`/api/llm/*`、分析循环）
/// 都只看持久化槽位，不需要第二套「从 env 取 key」的分支。
/// 值没变的账号不会涨 `revision`，见 [`save_persisted_llm_secret_for_account`]。
pub async fn seed_llm_secrets_from_env(
    store: &SqliteStore,
    vault: &SecretVault,
    accounts: &[(&str, &str, Option<&str>)],
) -> Result<Vec<String>, AppError> {
    let mut seeded = Vec::with_capacity(accounts.len());
    for &(account_id, api_key, base_url) in accounts {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            continue;
        }
        let account = validate_llm_account_id(account_id)?;
        let base_url = base_url.map(str::trim).filter(|value| !value.is_empty());
        save_persisted_llm_secret_for_account(store, vault, &account, api_key, base_url).await?;
        seeded.push(account);
    }
    Ok(seeded)
}

pub async fn load_persisted_bili_secret(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedBiliSecret>, AppError> {
    let Some(raw) = store.get_client_sync_state(BILI_SECRET_STATE_KEY).await? else {
        return Ok(None);
    };
    let stored: StoredSecret = serde_json::from_str(&raw)?;
    Ok(Some(PersistedBiliSecret {
        cookie: vault.decrypt_bili_cookie(&stored.ciphertext)?,
        revision: stored.revision,
        changed: false,
    }))
}

pub async fn save_persisted_bili_secret(
    store: &SqliteStore,
    vault: &SecretVault,
    cookie: &str,
) -> Result<PersistedBiliSecret, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let previous = load_persisted_bili_secret(store, vault).await?;
    let revision = match previous.as_ref() {
        Some(value) if value.cookie == cookie => return Ok(value.clone()),
        Some(value) => value.revision.saturating_add(1),
        None => 1,
    };
    let stored = StoredSecret {
        ciphertext: vault.encrypt_bili_cookie(cookie)?,
        revision,
        updated_at: Utc::now().to_rfc3339(),
        bound_base_url: None,
        pending_url_binding: false,
    };
    store
        .set_client_sync_state(BILI_SECRET_STATE_KEY, &serde_json::to_string(&stored)?)
        .await?;
    Ok(PersistedBiliSecret {
        cookie: cookie.to_owned(),
        revision,
        changed: true,
    })
}

pub async fn clear_persisted_bili_secret(store: &SqliteStore) -> Result<bool, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    store.delete_client_sync_state(BILI_SECRET_STATE_KEY).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_round_trips_and_rejects_tampering() {
        let vault = SecretVault::from_material("fixture-master-key");
        let encoded = vault.encrypt("sk-fixture-secret").unwrap();
        assert!(!encoded.contains("sk-fixture-secret"));
        assert_eq!(vault.decrypt(&encoded).unwrap(), "sk-fixture-secret");

        let mut tampered = encoded.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        assert!(vault
            .decrypt(std::str::from_utf8(&tampered).unwrap())
            .is_err());
    }

    #[test]
    fn bili_cookie_uses_a_distinct_encryption_context() {
        let vault = SecretVault::from_material("fixture-master-key");
        let encoded = vault.encrypt_bili_cookie("SESSDATA=fixture").unwrap();
        assert_eq!(
            vault.decrypt_bili_cookie(&encoded).unwrap(),
            "SESSDATA=fixture"
        );
        assert!(vault.decrypt(&encoded).is_err());
    }

    #[tokio::test]
    async fn llm_secret_tracks_url_binding_without_bumping_revision() {
        let store = SqliteStore::in_memory().await.unwrap();
        let vault = SecretVault::from_material("fixture-master-key");
        let pending = save_persisted_llm_secret(&store, &vault, "sk-one", None)
            .await
            .unwrap();
        assert_eq!(pending.revision, 1);
        assert_eq!(pending.bound_base_url, None);
        assert!(pending.pending_url_binding);

        let bound =
            save_persisted_llm_secret(&store, &vault, "sk-one", Some("https://api.example.com/v1"))
                .await
                .unwrap();
        assert_eq!(bound.revision, 1);
        assert_eq!(
            bound.bound_base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert!(!bound.pending_url_binding);

        let rebound = bind_persisted_llm_secret_url(&store, &vault, "https://api.example.com/v2")
            .await
            .unwrap();
        assert_eq!(rebound.revision, 1);
        assert_eq!(
            rebound.bound_base_url.as_deref(),
            Some("https://api.example.com/v2")
        );
        assert!(!rebound.pending_url_binding);
    }

    #[tokio::test]
    async fn persisted_secrets_increment_independently() {
        let store = SqliteStore::in_memory().await.unwrap();
        let vault = SecretVault::from_material("fixture-master-key");
        let first = save_persisted_llm_secret(&store, &vault, "sk-one", None)
            .await
            .unwrap();
        let second = save_persisted_llm_secret(&store, &vault, "sk-two", None)
            .await
            .unwrap();
        assert_eq!(first.revision, 1);
        assert!(first.changed);
        assert_eq!(second.revision, 2);
        assert!(second.changed);
        let unchanged = save_persisted_llm_secret(&store, &vault, "sk-two", None)
            .await
            .unwrap();
        assert_eq!(unchanged.revision, 2);
        assert!(!unchanged.changed);
        let bili_first = save_persisted_bili_secret(&store, &vault, "SESSDATA=one")
            .await
            .unwrap();
        let bili_second = save_persisted_bili_secret(&store, &vault, "SESSDATA=two")
            .await
            .unwrap();
        assert_eq!(bili_first.revision, 1);
        assert!(bili_first.changed);
        assert_eq!(bili_second.revision, 2);
        assert!(bili_second.changed);
        let bili_unchanged = save_persisted_bili_secret(&store, &vault, "SESSDATA=two")
            .await
            .unwrap();
        assert_eq!(bili_unchanged.revision, 2);
        assert!(!bili_unchanged.changed);
        assert_eq!(
            load_persisted_bili_secret(&store, &vault)
                .await
                .unwrap()
                .unwrap()
                .cookie,
            "SESSDATA=two"
        );
        assert!(clear_persisted_bili_secret(&store).await.unwrap());
        assert!(load_persisted_bili_secret(&store, &vault)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn llm_secrets_are_isolated_per_account() {
        let store = SqliteStore::in_memory().await.unwrap();
        let vault = SecretVault::from_material("fixture-master-key");
        let first = save_persisted_llm_secret_for_account(&store, &vault, "acct-a", "sk-a", None)
            .await
            .unwrap();
        assert_eq!(first.account_id, "acct-a");
        let second = save_persisted_llm_secret_for_account(&store, &vault, "acct-b", "sk-b", None)
            .await
            .unwrap();
        assert_eq!(second.account_id, "acct-b");
        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 1);

        let loaded_a = load_persisted_llm_secret_for_account(&store, &vault, "acct-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded_a.api_key, "sk-a");
        let loaded_b = load_persisted_llm_secret_for_account(&store, &vault, "acct-b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded_b.api_key, "sk-b");

        // 版本号各自递增，互不影响。
        let bumped_a =
            save_persisted_llm_secret_for_account(&store, &vault, "acct-a", "sk-a2", None)
                .await
                .unwrap();
        assert_eq!(bumped_a.revision, 2);
        let reloaded_b = load_persisted_llm_secret_for_account(&store, &vault, "acct-b")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded_b.revision, 1);

        // 账号 id 归一化：去空白，空值落到 default。
        assert_eq!(normalize_llm_account_id("  acct-a  "), "acct-a");
        assert_eq!(normalize_llm_account_id("   "), DEFAULT_LLM_ACCOUNT_ID);
        let blank = save_persisted_llm_secret_for_account(&store, &vault, "  ", "sk-default", None)
            .await
            .unwrap();
        assert_eq!(blank.account_id, DEFAULT_LLM_ACCOUNT_ID);
    }

    #[tokio::test]
    async fn default_account_reads_legacy_single_key_and_migrates_on_write() {
        let store = SqliteStore::in_memory().await.unwrap();
        let vault = SecretVault::from_material("fixture-master-key");
        // 模拟 0.3.19 的落库形态：旧键 + 旧 AAD。
        let legacy = StoredSecret {
            ciphertext: vault.encrypt("sk-legacy").unwrap(),
            revision: 7,
            updated_at: Utc::now().to_rfc3339(),
            bound_base_url: Some("https://legacy.example.com".to_owned()),
            pending_url_binding: false,
        };
        store
            .set_client_sync_state(
                LLM_SECRET_STATE_KEY,
                &serde_json::to_string(&legacy).unwrap(),
            )
            .await
            .unwrap();

        let status = llm_secret_status_for_account(&store, &vault, DEFAULT_LLM_ACCOUNT_ID)
            .await
            .unwrap();
        assert!(status.configured);
        assert_eq!(status.revision, 7);

        let loaded = load_persisted_llm_secret(&store, &vault)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.api_key, "sk-legacy");
        assert_eq!(
            loaded.bound_base_url.as_deref(),
            Some("https://legacy.example.com")
        );
        assert!(!loaded.changed);

        // 同值写入：密文迁到账号槽位（AAD 变了必须重新加密），revision 不涨。
        let migrated = save_persisted_llm_secret(&store, &vault, "sk-legacy", None)
            .await
            .unwrap();
        assert_eq!(migrated.revision, 7);
        assert!(!migrated.changed);

        let account_key = format!("{LLM_SECRET_STATE_KEY}:{DEFAULT_LLM_ACCOUNT_ID}");
        let account_raw = store
            .get_client_sync_state(&account_key)
            .await
            .unwrap()
            .expect("账号槽位应已写入");
        let account_stored: StoredSecret = serde_json::from_str(&account_raw).unwrap();
        let account_slot = LlmSecretSlot::for_account(DEFAULT_LLM_ACCOUNT_ID);
        assert_eq!(
            vault
                .decrypt_llm_key(&account_stored.ciphertext, &account_slot)
                .unwrap(),
            "sk-legacy"
        );
        // 旧 AAD 解不开账号槽位的密文。
        assert!(vault.decrypt(&account_stored.ciphertext).is_err());

        // 其它账号读不到 default 的密钥。
        assert!(
            load_persisted_llm_secret_for_account(&store, &vault, "acct-a")
                .await
                .unwrap()
                .is_none()
        );
    }
}
