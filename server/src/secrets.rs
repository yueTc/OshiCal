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

/// 模型密钥在 `client_sync_state` 里的状态键与 AAD。
const LLM_SECRET_STATE_KEY: &str = "server:secret:llm_api_key:v1";
const LLM_AAD: &[u8] = b"oshical:server-secret:llm-api-key:v1";
/// 0.3.20 ~ 0.3.26 曾把密钥按账号存到 `server:secret:llm_api_key:v1:{account}`，
/// AAD 也带账号后缀。账号池功能已删除，但线上库里可能还留着这份密文，
/// 读的时候要能认出来，写入统一落回 [`LLM_SECRET_STATE_KEY`]。
const LEGACY_ACCOUNT_SCOPED_LLM_STATE_KEY: &str = "server:secret:llm_api_key:v1:default";
const LEGACY_ACCOUNT_SCOPED_LLM_AAD: &[u8] = b"oshical:server-secret:llm-api-key:v1:default";
const BILI_SECRET_STATE_KEY: &str = "server:secret:bilibili_cookie:v1";
const BILI_AAD: &[u8] = b"oshical:server-secret:bilibili-cookie:v1";
const NONCE_BYTES: usize = 12;
const LLM_PENDING_URL_BINDING_TTL_SECONDS: i64 = 15 * 60;
static SECRET_WRITE_LOCK: Mutex<()> = Mutex::const_new(());

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

    /// 模型密钥加密（AAD 绑定单密钥槽位）。
    pub fn encrypt(&self, plaintext: &str) -> Result<String, AppError> {
        self.encrypt_with_aad(plaintext, LLM_AAD)
    }

    /// 模型密钥解密，与 [`SecretVault::encrypt`] 配套。
    pub fn decrypt(&self, encoded: &str) -> Result<String, AppError> {
        self.decrypt_with_aad(encoded, LLM_AAD)
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
    pub api_key: String,
    pub revision: i64,
    pub changed: bool,
    pub bound_base_url: Option<String>,
    pub pending_url_binding: bool,
    pub requires_url_binding: bool,
}

/// 「模型密钥到底配没配」的状态快照，不含明文。
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
) -> Result<Option<(StoredSecret, bool)>, AppError> {
    if let Some(raw) = store.get_client_sync_state(LLM_SECRET_STATE_KEY).await? {
        return Ok(Some((serde_json::from_str(&raw)?, false)));
    }
    if let Some(raw) = store
        .get_client_sync_state(LEGACY_ACCOUNT_SCOPED_LLM_STATE_KEY)
        .await?
    {
        return Ok(Some((serde_json::from_str(&raw)?, true)));
    }
    Ok(None)
}

/// `account_scoped` 为真时表示密文来自按账号存的旧槽位，AAD 不同。
fn decrypt_llm_ciphertext(
    vault: &SecretVault,
    stored: &StoredSecret,
    account_scoped: bool,
) -> Result<String, AppError> {
    if account_scoped {
        vault.decrypt_with_aad(&stored.ciphertext, LEGACY_ACCOUNT_SCOPED_LLM_AAD)
    } else {
        vault.decrypt(&stored.ciphertext)
    }
}

async fn store_llm_secret(store: &SqliteStore, stored: &StoredSecret) -> Result<(), AppError> {
    store
        .set_client_sync_state(LLM_SECRET_STATE_KEY, &serde_json::to_string(stored)?)
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
    vault: &SecretVault,
    account_scoped: bool,
    changed: bool,
) -> Result<PersistedLlmSecret, AppError> {
    Ok(PersistedLlmSecret {
        api_key: decrypt_llm_ciphertext(vault, stored, account_scoped)?,
        revision: stored.revision,
        changed,
        bound_base_url: stored.bound_base_url.clone(),
        pending_url_binding: pending_url_binding_is_fresh(stored),
        requires_url_binding: stored.pending_url_binding,
    })
}

pub async fn load_persisted_llm_secret(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedLlmSecret>, AppError> {
    let Some((stored, account_scoped)) = load_stored_llm_secret(store).await? else {
        return Ok(None);
    };
    persisted_llm_secret(&stored, vault, account_scoped, false).map(Some)
}

/// 只回报「是否已配置」，不解出明文。
///
/// 解密仍然做一次：密文损坏（例如换了 `OSHICAL_SECRETS_KEY`）时必须报错，
/// 否则界面会显示「已配置」而实际调不通。
pub async fn llm_secret_status(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<LlmSecretStatus, AppError> {
    let Some((stored, account_scoped)) = load_stored_llm_secret(store).await? else {
        return Ok(LlmSecretStatus::default());
    };
    decrypt_llm_ciphertext(vault, &stored, account_scoped)?;
    Ok(LlmSecretStatus {
        configured: true,
        revision: stored.revision,
        bound_base_url: stored.bound_base_url.clone(),
        pending_url_binding: pending_url_binding_is_fresh(&stored),
    })
}

/// 写入模型密钥。同值重写不涨 `revision`，只更新绑定状态。
pub async fn save_persisted_llm_secret(
    store: &SqliteStore,
    vault: &SecretVault,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<PersistedLlmSecret, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let previous = load_stored_llm_secret(store).await?;
    let previous_key = match previous.as_ref() {
        Some((stored, account_scoped)) => {
            Some(decrypt_llm_ciphertext(vault, stored, *account_scoped)?)
        }
        None => None,
    };
    let now = Utc::now().to_rfc3339();

    if previous_key.as_deref() == Some(api_key) {
        let (stored, _) = previous.as_ref().ok_or_else(|| AppError::External {
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
        // 值没变，但密文可能还躺在旧的按账号槽位里：趁这次写入搬回单密钥槽位。
        // AAD 变了必须重新加密，语义上没换密钥，所以 `revision` 保持不变。
        stored.ciphertext = vault.encrypt(api_key)?;
        store_llm_secret(store, &stored).await?;
        return persisted_llm_secret(&stored, vault, false, false);
    }

    let revision = previous
        .as_ref()
        .map_or(1, |(stored, _)| stored.revision.saturating_add(1));
    let stored = StoredSecret {
        ciphertext: vault.encrypt(api_key)?,
        revision,
        updated_at: now,
        bound_base_url: base_url.map(ToOwned::to_owned),
        pending_url_binding: base_url.is_none(),
    };
    store_llm_secret(store, &stored).await?;
    persisted_llm_secret(&stored, vault, false, true)
}

/// 把已保存的密钥绑定到 `base_url`（首次成功探测后调用）。
pub async fn bind_persisted_llm_secret_url(
    store: &SqliteStore,
    vault: &SecretVault,
    base_url: &str,
) -> Result<PersistedLlmSecret, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let (stored, account_scoped) = load_stored_llm_secret(store)
        .await?
        .ok_or_else(|| AppError::NotFound("模型密钥".to_owned()))?;
    let mut stored = stored;
    if account_scoped {
        // 旧槽位的密文换了 AAD 就解不开，先解再按单密钥 AAD 重新加密。
        let plaintext = decrypt_llm_ciphertext(vault, &stored, true)?;
        stored.ciphertext = vault.encrypt(&plaintext)?;
    }
    stored.bound_base_url = Some(base_url.to_owned());
    stored.pending_url_binding = false;
    stored.updated_at = Utc::now().to_rfc3339();
    store_llm_secret(store, &stored).await?;
    persisted_llm_secret(&stored, vault, false, false)
}

/// 把部署配置（`OSHICAL_LLM_API_KEY`）里的密钥写进持久化槽位。
///
/// 策略上环境变量优先：写进去以后所有读路径（`/api/secrets/llm`、
/// `/api/llm/*`、分析循环）都只看持久化槽位，不需要第二套「从 env 取 key」的分支。
/// 值没变时不会涨 `revision`。
pub async fn seed_llm_secret_from_env(
    store: &SqliteStore,
    vault: &SecretVault,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> Result<bool, AppError> {
    let Some(api_key) = api_key.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    let base_url = base_url.map(str::trim).filter(|value| !value.is_empty());
    save_persisted_llm_secret(store, vault, api_key, base_url).await?;
    Ok(true)
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
    async fn llm_secret_round_trips_with_a_single_slot() {
        let store = SqliteStore::in_memory().await.unwrap();
        let vault = SecretVault::from_material("fixture-master-key");
        assert!(load_persisted_llm_secret(&store, &vault)
            .await
            .unwrap()
            .is_none());
        let status = llm_secret_status(&store, &vault).await.unwrap();
        assert!(!status.configured);

        let first = save_persisted_llm_secret(&store, &vault, "sk-a", None)
            .await
            .unwrap();
        assert_eq!(first.revision, 1);
        assert!(first.changed);
        assert!(first.pending_url_binding);
        assert_eq!(first.api_key, "sk-a");

        let loaded = load_persisted_llm_secret(&store, &vault)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.api_key, "sk-a");
        assert!(!loaded.changed);

        // 同值重写只刷新时间与绑定状态，不涨 revision。
        let refreshed =
            save_persisted_llm_secret(&store, &vault, "sk-a", Some("https://m.example/v1"))
                .await
                .unwrap();
        assert_eq!(refreshed.revision, 1);
        assert!(!refreshed.changed);
        assert!(!refreshed.pending_url_binding);

        let bumped = save_persisted_llm_secret(&store, &vault, "sk-b", None)
            .await
            .unwrap();
        assert_eq!(bumped.revision, 2);
        assert_eq!(
            load_persisted_llm_secret(&store, &vault)
                .await
                .unwrap()
                .unwrap()
                .api_key,
            "sk-b"
        );
    }

    #[tokio::test]
    async fn account_scoped_ciphertext_is_still_readable_and_migrates_on_write() {
        let store = SqliteStore::in_memory().await.unwrap();
        let vault = SecretVault::from_material("fixture-master-key");
        // 模拟 0.3.20 ~ 0.3.26 的落库形态：按账号键 + 带后缀的 AAD。
        let stored = StoredSecret {
            ciphertext: vault
                .encrypt_with_aad("sk-scoped", LEGACY_ACCOUNT_SCOPED_LLM_AAD)
                .unwrap(),
            revision: 3,
            updated_at: Utc::now().to_rfc3339(),
            bound_base_url: Some("https://scoped.example.com/v1".to_owned()),
            pending_url_binding: false,
        };
        store
            .set_client_sync_state(
                LEGACY_ACCOUNT_SCOPED_LLM_STATE_KEY,
                &serde_json::to_string(&stored).unwrap(),
            )
            .await
            .unwrap();

        let status = llm_secret_status(&store, &vault).await.unwrap();
        assert!(status.configured);
        assert_eq!(status.revision, 3);
        let loaded = load_persisted_llm_secret(&store, &vault)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.api_key, "sk-scoped");

        // 同值写入：密文按单密钥 AAD 重新加密到旧键，revision 不涨。
        let migrated = save_persisted_llm_secret(&store, &vault, "sk-scoped", None)
            .await
            .unwrap();
        assert_eq!(migrated.revision, 3);
        assert!(!migrated.changed);
        let raw = store
            .get_client_sync_state(LLM_SECRET_STATE_KEY)
            .await
            .unwrap()
            .expect("单密钥槽位应已写入");
        let migrated_stored: StoredSecret = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            vault.decrypt(&migrated_stored.ciphertext).unwrap(),
            "sk-scoped"
        );
    }
}
