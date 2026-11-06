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

const LLM_SECRET_STATE_KEY: &str = "server:secret:llm_api_key:v1";
const BILI_SECRET_STATE_KEY: &str = "server:secret:bilibili_cookie:v1";
const LLM_AAD: &[u8] = b"oshical:server-secret:llm-api-key:v1";
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

    pub fn encrypt(&self, plaintext: &str) -> Result<String, AppError> {
        self.encrypt_with_aad(plaintext, LLM_AAD)
    }

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
}

#[derive(Debug, Clone)]
pub struct PersistedBiliSecret {
    pub cookie: String,
    pub revision: i64,
    pub changed: bool,
}

async fn load_stored_llm_secret(store: &SqliteStore) -> Result<Option<StoredSecret>, AppError> {
    let Some(raw) = store.get_client_sync_state(LLM_SECRET_STATE_KEY).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&raw)?))
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
    changed: bool,
) -> Result<PersistedLlmSecret, AppError> {
    Ok(PersistedLlmSecret {
        api_key: vault.decrypt(&stored.ciphertext)?,
        revision: stored.revision,
        changed,
        bound_base_url: stored.bound_base_url.clone(),
        pending_url_binding: pending_url_binding_is_fresh(stored),
    })
}

pub async fn load_persisted_llm_secret(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedLlmSecret>, AppError> {
    load_stored_llm_secret(store)
        .await?
        .as_ref()
        .map(|stored| persisted_llm_secret(stored, vault, false))
        .transpose()
}

pub async fn save_persisted_llm_secret(
    store: &SqliteStore,
    vault: &SecretVault,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<PersistedLlmSecret, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let previous_stored = load_stored_llm_secret(store).await?;
    let previous_key = previous_stored
        .as_ref()
        .map(|stored| vault.decrypt(&stored.ciphertext))
        .transpose()?;
    let now = Utc::now().to_rfc3339();

    if previous_key.as_deref() == Some(api_key) {
        let mut stored = previous_stored.ok_or_else(|| AppError::External {
            code: "SECRET_CACHE_INCONSISTENT".to_owned(),
            message: "模型密钥缓存状态不一致".to_owned(),
        })?;
        stored.updated_at = now;
        match base_url {
            Some(base_url) => {
                stored.bound_base_url = Some(base_url.to_owned());
                stored.pending_url_binding = false;
            }
            None => stored.pending_url_binding = true,
        }
        store_llm_secret(store, &stored).await?;
        return persisted_llm_secret(&stored, vault, false);
    }

    let revision = previous_stored
        .as_ref()
        .map_or(1, |stored| stored.revision.saturating_add(1));
    let stored = StoredSecret {
        ciphertext: vault.encrypt(api_key)?,
        revision,
        updated_at: now,
        bound_base_url: base_url.map(ToOwned::to_owned),
        pending_url_binding: base_url.is_none(),
    };
    store_llm_secret(store, &stored).await?;
    persisted_llm_secret(&stored, vault, true)
}

pub async fn bind_persisted_llm_secret_url(
    store: &SqliteStore,
    vault: &SecretVault,
    base_url: &str,
) -> Result<PersistedLlmSecret, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let mut stored = load_stored_llm_secret(store)
        .await?
        .ok_or_else(|| AppError::NotFound("模型密钥".to_owned()))?;
    stored.bound_base_url = Some(base_url.to_owned());
    stored.pending_url_binding = false;
    stored.updated_at = Utc::now().to_rfc3339();
    store_llm_secret(store, &stored).await?;
    persisted_llm_secret(&stored, vault, false)
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

        let bound = save_persisted_llm_secret(
            &store,
            &vault,
            "sk-one",
            Some("https://api.example.com/v1"),
        )
        .await
        .unwrap();
        assert_eq!(bound.revision, 1);
        assert_eq!(
            bound.bound_base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert!(!bound.pending_url_binding);

        let rebound = bind_persisted_llm_secret_url(
            &store,
            &vault,
            "https://api.example.com/v2",
        )
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
}
