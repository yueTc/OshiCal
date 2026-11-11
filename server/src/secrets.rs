use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

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
const YOUTUBE_API_KEY_STATE_KEY: &str = "server:secret:youtube_api_key:v1";
const YOUTUBE_API_KEY_AAD: &[u8] = b"oshical:server-secret:youtube-api-key:v1";
const YOUTUBE_OAUTH_STATE_KEY: &str = "server:secret:youtube_oauth:v1";
const YOUTUBE_OAUTH_AAD: &[u8] = b"oshical:server-secret:youtube-oauth:v1";
const YOUTUBE_WEB_SESSION_STATE_KEY: &str = "server:secret:youtube_web_session:v1";
const YOUTUBE_WEB_SESSION_AAD: &[u8] = b"oshical:server-secret:youtube-web-session:v1";
const NONCE_BYTES: usize = 12;
const MASTER_KEY_BYTES: usize = 32;
const MASTER_KEY_FILE_ENV: &str = "OSHICAL_SECRETS_KEY_FILE";
const MASTER_KEY_FILE_SUFFIX: &str = ".secrets-key";
const LLM_PENDING_URL_BINDING_TTL_SECONDS: i64 = 15 * 60;
static SECRET_WRITE_LOCK: Mutex<()> = Mutex::const_new(());
static LEGACY_KEY_WARNING: std::sync::Once = std::sync::Once::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecretKeySource {
    Explicit,
    GeneratedFile,
    Unavailable,
}

#[derive(Clone)]
pub struct SecretVault {
    key: Option<[u8; 32]>,
    /// Compatibility-only key for ciphertext written by the old predictable
    /// derivation. It is never used to encrypt new credentials.
    legacy_key: Option<[u8; 32]>,
    key_source: SecretKeySource,
    key_file: Option<PathBuf>,
    key_error: Option<String>,
}

impl fmt::Debug for SecretVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretVault")
            .field("key", &"[redacted]")
            .field("key_source", &self.key_source)
            .field("key_file", &self.key_file)
            .field("legacy_key_available", &self.legacy_key.is_some())
            .field("key_error", &self.key_error)
            .finish()
    }
}

impl SecretVault {
    pub fn from_env(api_token: Option<&str>, database_path: &Path) -> Self {
        let explicit_material = std::env::var("OSHICAL_SECRETS_KEY")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let key_file_override = std::env::var(MASTER_KEY_FILE_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        Self::from_config_material(
            api_token,
            database_path,
            explicit_material.as_deref(),
            key_file_override.as_deref().map(Path::new),
        )
    }

    pub fn from_material(material: &str) -> Self {
        Self {
            key: Some(derive_key(material)),
            legacy_key: None,
            key_source: SecretKeySource::Explicit,
            key_file: None,
            key_error: None,
        }
    }

    fn from_config_material(
        api_token: Option<&str>,
        database_path: &Path,
        explicit_material: Option<&str>,
        key_file_override: Option<&Path>,
    ) -> Self {
        let legacy_key = Some(derive_key(&legacy_key_material(api_token, database_path)));
        if let Some(material) = explicit_material {
            // An operator may introduce OSHICAL_SECRETS_KEY during rollout. New
            // writes use that key, but credentials written by the intermediate
            // predictable-key release still need a one-time compatible read.
            return Self {
                key: Some(derive_key(material)),
                legacy_key,
                key_source: SecretKeySource::Explicit,
                key_file: None,
                key_error: None,
            };
        }
        let key_file = key_file_override
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_master_key_file(database_path));
        match load_or_create_master_key(&key_file) {
            Ok(key) => {
                tracing::info!(
                    key_file = %key_file.display(),
                    "服务端使用随机主密钥文件加密凭据；请与数据库一起备份"
                );
                Self {
                    key: Some(key),
                    legacy_key,
                    key_source: SecretKeySource::GeneratedFile,
                    key_file: Some(key_file),
                    key_error: None,
                }
            }
            Err(error) => {
                tracing::error!(
                    key_file = %key_file.display(),
                    %error,
                    "安全主密钥文件不可用；不会退回可预测默认密钥"
                );
                Self {
                    key: None,
                    legacy_key,
                    key_source: SecretKeySource::Unavailable,
                    key_file: Some(key_file),
                    key_error: Some(error),
                }
            }
        }
    }

    fn require_write_key(&self) -> Result<&[u8; 32], AppError> {
        self.key.as_ref().ok_or_else(|| AppError::External {
            code: "SECRET_MASTER_KEY_UNAVAILABLE".to_owned(),
            message: format!(
                "无法保存服务端凭据：安全主密钥不可用。请修复 {} 或设置 OSHICAL_SECRETS_KEY 后重启，再重新保存 YouTube 凭据；已有 B 站或模型凭据不会被删除。原因：{}",
                self.key_file
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "服务端主密钥配置".to_owned()),
                self.key_error.as_deref().unwrap_or("未生成安全主密钥")
            ),
        })
    }

    fn encrypt_with_aad(&self, plaintext: &str, aad: &[u8]) -> Result<String, AppError> {
        let key = self.require_write_key()?;
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|error| AppError::External {
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

    fn decrypt_with_aad_detailed(
        &self,
        encoded: &str,
        aad: &[u8],
    ) -> Result<(String, bool), AppError> {
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
        let (nonce, ciphertext) = bytes.split_at(NONCE_BYTES);

        if let Some(key) = self.key.as_ref() {
            if let Some(plaintext) = decrypt_bytes(key, nonce, ciphertext, aad) {
                return decode_plaintext(plaintext).map(|value| (value, false));
            }
        }
        if let Some(key) = self.legacy_key.as_ref() {
            if let Some(plaintext) = decrypt_bytes(key, nonce, ciphertext, aad) {
                LEGACY_KEY_WARNING.call_once(|| {
                    tracing::warn!(
                        "读取到旧版可预测默认密钥加密的凭据；YouTube 凭据会尝试自动迁移，B 站/模型凭据在后续显式更新时会迁移。安全主密钥不可用时将拒绝新写入"
                    );
                });
                return decode_plaintext(plaintext).map(|value| (value, true));
            }
        }

        Err(AppError::External {
            code: "SECRET_VAULT_DECRYPT_FAILED".to_owned(),
            message: "服务端密钥解密失败：主密钥文件或 OSHICAL_SECRETS_KEY 与数据库密文不匹配。请恢复原密钥，或在设置页重新保存对应凭据；旧数据不会被自动删除。".to_owned(),
        })
    }

    fn decrypt_with_aad(&self, encoded: &str, aad: &[u8]) -> Result<String, AppError> {
        self.decrypt_with_aad_detailed(encoded, aad)
            .map(|(plaintext, _)| plaintext)
    }

    fn decrypt_youtube_api_key_detailed(
        &self,
        encoded: &str,
    ) -> Result<(String, bool), AppError> {
        self.decrypt_with_aad_detailed(encoded, YOUTUBE_API_KEY_AAD)
    }

    fn decrypt_youtube_oauth_detailed(
        &self,
        encoded: &str,
    ) -> Result<(String, bool), AppError> {
        self.decrypt_with_aad_detailed(encoded, YOUTUBE_OAUTH_AAD)
    }

    fn decrypt_youtube_web_session_detailed(
        &self,
        encoded: &str,
    ) -> Result<(String, bool), AppError> {
        self.decrypt_with_aad_detailed(encoded, YOUTUBE_WEB_SESSION_AAD)
    }

    fn encrypt_bili_cookie(&self, cookie: &str) -> Result<String, AppError> {
        self.encrypt_with_aad(cookie, BILI_AAD)
    }

    fn decrypt_bili_cookie(&self, encoded: &str) -> Result<String, AppError> {
        self.decrypt_with_aad(encoded, BILI_AAD)
    }

    fn encrypt_youtube_api_key(&self, api_key: &str) -> Result<String, AppError> {
        self.encrypt_with_aad(api_key, YOUTUBE_API_KEY_AAD)
    }


    fn encrypt_youtube_oauth(&self, token: &str) -> Result<String, AppError> {
        self.encrypt_with_aad(token, YOUTUBE_OAUTH_AAD)
    }


    fn encrypt_youtube_web_session(&self, cookie: &str) -> Result<String, AppError> {
        self.encrypt_with_aad(cookie, YOUTUBE_WEB_SESSION_AAD)
    }


}

fn derive_key(material: &str) -> [u8; 32] {
    let digest = Sha256::digest(material.as_bytes());
    let mut key = [0_u8; MASTER_KEY_BYTES];
    key.copy_from_slice(&digest);
    key
}

fn legacy_key_material(api_token: Option<&str>, database_path: &Path) -> String {
    match api_token {
        Some(token) => format!("oshical-api-token:{token}"),
        None => format!("oshical-local-db:{}", database_path.display()),
    }
}

fn decrypt_bytes(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .ok()
}

fn decode_plaintext(plaintext: Vec<u8>) -> Result<String, AppError> {
    String::from_utf8(plaintext).map_err(|_| AppError::External {
        code: "SECRET_VAULT".to_owned(),
        message: "服务端密钥明文编码无效".to_owned(),
    })
}

fn default_master_key_file(database_path: &Path) -> PathBuf {
    let mut file_name = database_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("oshical.db"))
        .to_os_string();
    file_name.push(MASTER_KEY_FILE_SUFFIX);
    database_path.with_file_name(file_name)
}

fn load_or_create_master_key(path: &Path) -> Result<[u8; 32], String> {
    if path.exists() {
        return read_master_key_file(path);
    }

    if let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "创建主密钥目录 {} 失败: {error}",
                parent.display()
            )
        })?;
    }

    let key = random_master_key();
    let encoded = format!("{}\n", BASE64.encode(key));
    let temp_path = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("master-key"),
        Uuid::new_v4()
    ));
    if let Err(error) = write_private_file(&temp_path, encoded.as_bytes()) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    // A hard link gives create-if-absent semantics. Unlike rename on Unix, it
    // cannot silently replace a key concurrently created by another process.
    match fs::hard_link(&temp_path, path) {
        Ok(()) => {
            let _ = fs::remove_file(&temp_path);
        }
        Err(_) if path.exists() => {
            let _ = fs::remove_file(&temp_path);
            return read_master_key_file(path);
        }
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            return Err(format!(
                "发布主密钥文件 {} 失败: {error}",
                path.display()
            ));
        }
    }
    restrict_key_file_permissions(path)?;
    read_master_key_file(path)
}

fn read_master_key_file(path: &Path) -> Result<[u8; 32], String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("读取主密钥文件 {} 元数据失败: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "主密钥路径 {} 不是普通文件；拒绝符号链接或特殊文件",
            path.display()
        ));
    }
    // Tighten ACLs before reading key bytes so a pre-existing broad ACL cannot
    // expose the key during startup.
    restrict_key_file_permissions(path)?;
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("读取主密钥文件 {} 失败: {error}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 256 {
        return Err(format!(
            "主密钥文件 {} 内容无效；请恢复备份或设置 OSHICAL_SECRETS_KEY",
            path.display()
        ));
    }
    let bytes = BASE64.decode(trimmed).map_err(|_| {
        format!(
            "主密钥文件 {} 内容无效；请恢复备份或设置 OSHICAL_SECRETS_KEY",
            path.display()
        )
    })?;
    if bytes.len() != MASTER_KEY_BYTES {
        return Err(format!(
            "主密钥文件 {} 长度无效；请恢复备份或设置 OSHICAL_SECRETS_KEY",
            path.display()
        ));
    }
    let mut key = [0_u8; MASTER_KEY_BYTES];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn random_master_key() -> [u8; MASTER_KEY_BYTES] {
    let mut key = [0_u8; MASTER_KEY_BYTES];
    key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    key
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("创建主密钥临时文件 {} 失败: {error}", path.display()))?;
    // Tighten Windows ACLs before writing key bytes. Unix already created the
    // file with mode 0600 through OpenOptionsExt.
    restrict_key_file_permissions(path)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("写入主密钥临时文件 {} 失败: {error}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_key_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        format!(
            "收紧主密钥文件 {} 权限失败: {error}",
            path.display()
        )
    })?;
    let mode = fs::metadata(path)
        .map_err(|error| format!("读取主密钥文件 {} 权限失败: {error}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "主密钥文件 {} 权限不安全（{:o}），必须仅当前用户可读写",
            path.display(),
            mode & 0o777
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn run_icacls(path: &Path, args: &[&str]) -> Result<(), String> {
    use std::process::Command;

    let status = Command::new("icacls")
        .arg(path)
        .args(args)
        .status()
        .map_err(|error| format!("调用 icacls 收紧主密钥文件权限失败: {error}"))?;
    if !status.success() {
        return Err(format!(
            "收紧主密钥文件 {} 的 Windows ACL 失败；请改设 OSHICAL_SECRETS_KEY",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_key_file_permissions(path: &Path) -> Result<(), String> {
    use std::process::Command;

    let account = Command::new("whoami")
        .output()
        .map_err(|error| format!("读取当前 Windows 账号失败: {error}"))?;
    if !account.status.success() {
        return Err("读取当前 Windows 账号失败，无法收紧主密钥文件权限".to_owned());
    }
    let account = String::from_utf8(account.stdout)
        .map_err(|_| "当前 Windows 账号编码无效".to_owned())?
        .trim()
        .to_owned();
    if account.is_empty() || account.chars().any(char::is_control) {
        return Err("当前 Windows 账号无效，无法收紧主密钥文件权限".to_owned());
    }

    // Inheritance removal alone preserves explicit ACEs (for example Everyone
    // or Users). Reset first to discard them, then strip inherited entries and
    // grant only the service account.
    run_icacls(path, &["/reset"])?;
    run_icacls(path, &["/inheritance:r"])?;
    let grant = format!("{account}:F");
    run_icacls(path, &["/grant:r", &grant])?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn restrict_key_file_permissions(_path: &Path) -> Result<(), String> {
    Err("当前平台无法安全限制主密钥文件权限；请设置 OSHICAL_SECRETS_KEY".to_owned())
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

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeSecretStatus {
    pub configured: bool,
    pub revision: i64,
    pub updated_at: Option<String>,
}

pub struct PersistedYoutubeApiKey {
    pub api_key: String,
    pub revision: i64,
    pub updated_at: String,
    pub changed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeOAuthToken {
    pub refresh_token: String,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub access_token_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "Bearer".to_owned()
}

pub struct PersistedYoutubeOAuthToken {
    pub token: YoutubeOAuthToken,
    pub revision: i64,
    pub updated_at: String,
    pub changed: bool,
}

pub struct PersistedYoutubeWebSession {
    pub cookie: String,
    pub revision: i64,
    pub updated_at: String,
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

async fn load_stored_youtube_secret(
    store: &SqliteStore,
    key: &str,
) -> Result<Option<StoredSecret>, AppError> {
    store
        .get_client_sync_state(key)
        .await?
        .map(|raw| serde_json::from_str(&raw).map_err(AppError::from))
        .transpose()
}

async fn existing_youtube_secret_revision(
    store: &SqliteStore,
    key: &str,
) -> Result<Option<i64>, AppError> {
    let Some(raw) = store.get_client_sync_state(key).await? else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<StoredSecret>(&raw)
        .ok()
        .map(|stored| stored.revision))
}

pub(crate) fn is_recoverable_youtube_secret_read_error(error: &AppError) -> bool {
    match error {
        AppError::Json(_) => true,
        AppError::External { code, .. } => {
            code.as_str() == "SECRET_VAULT" || code.as_str() == "SECRET_VAULT_DECRYPT_FAILED"
        }
        _ => false,
    }
}

async fn store_youtube_secret(
    store: &SqliteStore,
    key: &str,
    stored: &StoredSecret,
) -> Result<(), AppError> {
    store
        .set_client_sync_state(key, &serde_json::to_string(stored)?)
        .await
}

pub async fn load_persisted_youtube_api_key(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedYoutubeApiKey>, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    load_persisted_youtube_api_key_unlocked(store, vault).await
}

async fn load_persisted_youtube_api_key_unlocked(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedYoutubeApiKey>, AppError> {
    let Some(mut stored) = load_stored_youtube_secret(store, YOUTUBE_API_KEY_STATE_KEY).await?
    else {
        return Ok(None);
    };
    let (api_key, legacy_ciphertext) =
        vault.decrypt_youtube_api_key_detailed(&stored.ciphertext)?;
    if legacy_ciphertext {
        stored.ciphertext = vault.encrypt_youtube_api_key(&api_key)?;
        store_youtube_secret(store, YOUTUBE_API_KEY_STATE_KEY, &stored).await?;
        tracing::info!("已将 YouTube API Key 从旧版可预测默认密钥迁移到安全主密钥");
    }
    Ok(Some(PersistedYoutubeApiKey {
        api_key,
        revision: stored.revision,
        updated_at: stored.updated_at,
        changed: false,
    }))
}

pub async fn youtube_api_key_status(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<YoutubeSecretStatus, AppError> {
    let Some(stored) = load_persisted_youtube_api_key(store, vault).await? else {
        return Ok(YoutubeSecretStatus::default());
    };
    Ok(YoutubeSecretStatus {
        configured: true,
        revision: stored.revision,
        updated_at: Some(stored.updated_at),
    })
}

pub async fn save_persisted_youtube_api_key(
    store: &SqliteStore,
    vault: &SecretVault,
    api_key: &str,
) -> Result<PersistedYoutubeApiKey, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let fallback_revision =
        existing_youtube_secret_revision(store, YOUTUBE_API_KEY_STATE_KEY).await?;
    let previous = match load_persisted_youtube_api_key_unlocked(store, vault).await {
        Ok(value) => value,
        Err(error) if is_recoverable_youtube_secret_read_error(&error) => {
            tracing::warn!(code = error.code(), "现有 YouTube API Key 密文损坏，将允许显式覆盖");
            None
        }
        Err(error) => return Err(error),
    };
    if let Some(previous) = previous.as_ref().filter(|value| value.api_key == api_key) {
        return Ok(PersistedYoutubeApiKey {
            api_key: previous.api_key.clone(),
            revision: previous.revision,
            updated_at: previous.updated_at.clone(),
            changed: false,
        });
    }
    let revision = previous
        .as_ref()
        .map(|value| value.revision)
        .or(fallback_revision)
        .map_or(1, |value| value.saturating_add(1));
    let updated_at = Utc::now().to_rfc3339();
    let stored = StoredSecret {
        ciphertext: vault.encrypt_youtube_api_key(api_key)?,
        revision,
        updated_at: updated_at.clone(),
        bound_base_url: None,
        pending_url_binding: false,
    };
    store_youtube_secret(store, YOUTUBE_API_KEY_STATE_KEY, &stored).await?;
    Ok(PersistedYoutubeApiKey {
        api_key: api_key.to_owned(),
        revision,
        updated_at,
        changed: true,
    })
}

pub async fn clear_persisted_youtube_api_key(store: &SqliteStore) -> Result<bool, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    store.delete_client_sync_state(YOUTUBE_API_KEY_STATE_KEY).await
}

pub async fn load_persisted_youtube_oauth(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedYoutubeOAuthToken>, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    load_persisted_youtube_oauth_unlocked(store, vault).await
}

async fn load_persisted_youtube_oauth_unlocked(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedYoutubeOAuthToken>, AppError> {
    let Some(mut stored) = load_stored_youtube_secret(store, YOUTUBE_OAUTH_STATE_KEY).await? else {
        return Ok(None);
    };
    let (plaintext, legacy_ciphertext) =
        vault.decrypt_youtube_oauth_detailed(&stored.ciphertext)?;
    if legacy_ciphertext {
        stored.ciphertext = vault.encrypt_youtube_oauth(&plaintext)?;
        store_youtube_secret(store, YOUTUBE_OAUTH_STATE_KEY, &stored).await?;
        tracing::info!("已将 YouTube OAuth 凭据从旧版可预测默认密钥迁移到安全主密钥");
    }
    Ok(Some(PersistedYoutubeOAuthToken {
        token: serde_json::from_str(&plaintext)?,
        revision: stored.revision,
        updated_at: stored.updated_at,
        changed: false,
    }))
}

pub async fn save_persisted_youtube_oauth(
    store: &SqliteStore,
    vault: &SecretVault,
    token: &YoutubeOAuthToken,
) -> Result<PersistedYoutubeOAuthToken, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    // A new authorization must be able to replace an unreadable OAuth slot.
    // Only the non-secret revision is needed from the previous envelope.
    let previous_revision =
        existing_youtube_secret_revision(store, YOUTUBE_OAUTH_STATE_KEY).await?;
    let revision = previous_revision.map_or(1, |value| value.saturating_add(1));
    let updated_at = Utc::now().to_rfc3339();
    let stored = StoredSecret {
        ciphertext: vault.encrypt_youtube_oauth(&serde_json::to_string(token)?)?,
        revision,
        updated_at: updated_at.clone(),
        bound_base_url: None,
        pending_url_binding: false,
    };
    store_youtube_secret(store, YOUTUBE_OAUTH_STATE_KEY, &stored).await?;
    Ok(PersistedYoutubeOAuthToken {
        token: token.clone(),
        revision,
        updated_at,
        changed: true,
    })
}

pub async fn clear_persisted_youtube_oauth(store: &SqliteStore) -> Result<bool, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    store.delete_client_sync_state(YOUTUBE_OAUTH_STATE_KEY).await
}

pub async fn load_persisted_youtube_web_session(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedYoutubeWebSession>, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    load_persisted_youtube_web_session_unlocked(store, vault).await
}

async fn load_persisted_youtube_web_session_unlocked(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<Option<PersistedYoutubeWebSession>, AppError> {
    let Some(mut stored) =
        load_stored_youtube_secret(store, YOUTUBE_WEB_SESSION_STATE_KEY).await?
    else {
        return Ok(None);
    };
    let (cookie, legacy_ciphertext) =
        vault.decrypt_youtube_web_session_detailed(&stored.ciphertext)?;
    if legacy_ciphertext {
        stored.ciphertext = vault.encrypt_youtube_web_session(&cookie)?;
        store_youtube_secret(store, YOUTUBE_WEB_SESSION_STATE_KEY, &stored).await?;
        tracing::info!("已将 YouTube 网页会话从旧版可预测默认密钥迁移到安全主密钥");
    }
    Ok(Some(PersistedYoutubeWebSession {
        cookie,
        revision: stored.revision,
        updated_at: stored.updated_at,
        changed: false,
    }))
}

pub async fn youtube_web_session_status(
    store: &SqliteStore,
    vault: &SecretVault,
) -> Result<YoutubeSecretStatus, AppError> {
    let Some(stored) = load_persisted_youtube_web_session(store, vault).await? else {
        return Ok(YoutubeSecretStatus::default());
    };
    Ok(YoutubeSecretStatus {
        configured: true,
        revision: stored.revision,
        updated_at: Some(stored.updated_at),
    })
}

pub async fn save_persisted_youtube_web_session(
    store: &SqliteStore,
    vault: &SecretVault,
    cookie: &str,
) -> Result<PersistedYoutubeWebSession, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    let fallback_revision =
        existing_youtube_secret_revision(store, YOUTUBE_WEB_SESSION_STATE_KEY).await?;
    let previous = match load_persisted_youtube_web_session_unlocked(store, vault).await {
        Ok(value) => value,
        Err(error) if is_recoverable_youtube_secret_read_error(&error) => {
            tracing::warn!(code = error.code(), "现有 YouTube 网页会话密文损坏，将允许显式覆盖");
            None
        }
        Err(error) => return Err(error),
    };
    if let Some(previous) = previous.as_ref().filter(|value| value.cookie == cookie) {
        return Ok(PersistedYoutubeWebSession {
            cookie: previous.cookie.clone(),
            revision: previous.revision,
            updated_at: previous.updated_at.clone(),
            changed: false,
        });
    }
    let revision = previous
        .as_ref()
        .map(|value| value.revision)
        .or(fallback_revision)
        .map_or(1, |value| value.saturating_add(1));
    let updated_at = Utc::now().to_rfc3339();
    let stored = StoredSecret {
        ciphertext: vault.encrypt_youtube_web_session(cookie)?,
        revision,
        updated_at: updated_at.clone(),
        bound_base_url: None,
        pending_url_binding: false,
    };
    store_youtube_secret(store, YOUTUBE_WEB_SESSION_STATE_KEY, &stored).await?;
    Ok(PersistedYoutubeWebSession {
        cookie: cookie.to_owned(),
        revision,
        updated_at,
        changed: true,
    })
}

pub async fn clear_persisted_youtube_web_session(
    store: &SqliteStore,
) -> Result<bool, AppError> {
    let _guard = SECRET_WRITE_LOCK.lock().await;
    store
        .delete_client_sync_state(YOUTUBE_WEB_SESSION_STATE_KEY)
        .await
}

pub async fn seed_youtube_web_session_from_env(
    store: &SqliteStore,
    vault: &SecretVault,
    cookie: Option<&str>,
) -> Result<bool, AppError> {
    let Some(cookie) = cookie.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    save_persisted_youtube_web_session(store, vault, cookie).await?;
    Ok(true)
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

    fn temporary_test_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "oshical-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn generated_master_key_is_random_private_and_stable() {
        let root = temporary_test_dir("master-key");
        let database_path = root.join("oshical.db");
        let key_file = default_master_key_file(&database_path);
        let first = load_or_create_master_key(&key_file).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let second = load_or_create_master_key(&key_file).unwrap();

        assert_eq!(first, second);
        assert_ne!(
            first,
            derive_key(&legacy_key_material(None, &database_path))
        );
        assert!(key_file.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&key_file).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "key file is readable by other users");
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_master_key_does_not_create_generated_key_file() {
        let root = temporary_test_dir("explicit-key");
        let database_path = root.join("oshical.db");
        let key_file = root.join("must-not-exist.secrets-key");
        let vault = SecretVault::from_config_material(
            None,
            &database_path,
            Some("explicit-test-master-key"),
            Some(&key_file),
        );

        assert_eq!(vault.key_source, SecretKeySource::Explicit);
        assert!(!key_file.exists());
        let ciphertext = vault.encrypt_youtube_api_key("AIza-explicit-key").unwrap();
        assert_eq!(
            vault.decrypt_youtube_api_key_detailed(&ciphertext).unwrap(),
            ("AIza-explicit-key".to_owned(), false)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn explicit_master_key_can_migrate_legacy_youtube_ciphertext() {
        let root = temporary_test_dir("explicit-legacy-migration");
        let database_path = root.join("oshical.db");
        let key_file = root.join("must-not-exist.secrets-key");
        let legacy = SecretVault::from_material(&legacy_key_material(None, &database_path));
        let store = SqliteStore::in_memory().await.unwrap();
        let stored = StoredSecret {
            ciphertext: legacy
                .encrypt_youtube_api_key("AIza-legacy-before-explicit-key")
                .unwrap(),
            revision: 3,
            updated_at: Utc::now().to_rfc3339(),
            bound_base_url: None,
            pending_url_binding: false,
        };
        store_youtube_secret(&store, YOUTUBE_API_KEY_STATE_KEY, &stored)
            .await
            .unwrap();

        let explicit = SecretVault::from_config_material(
            None,
            &database_path,
            Some("new-explicit-master-key"),
            Some(&key_file),
        );
        let loaded = load_persisted_youtube_api_key(&store, &explicit)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.api_key, "AIza-legacy-before-explicit-key");
        let raw = store
            .get_client_sync_state(YOUTUBE_API_KEY_STATE_KEY)
            .await
            .unwrap()
            .unwrap();
        let migrated: StoredSecret = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            explicit
                .decrypt_youtube_api_key_detailed(&migrated.ciphertext)
                .unwrap(),
            ("AIza-legacy-before-explicit-key".to_owned(), false)
        );
        assert!(!key_file.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn corrupt_youtube_slots_can_be_explicitly_overwritten() {
        let store = SqliteStore::in_memory().await.unwrap();
        let vault = SecretVault::from_material("replacement-master-key");
        for key in [
            YOUTUBE_API_KEY_STATE_KEY,
            YOUTUBE_OAUTH_STATE_KEY,
            YOUTUBE_WEB_SESSION_STATE_KEY,
        ] {
            store_youtube_secret(
                &store,
                key,
                &StoredSecret {
                    ciphertext: "not-valid-base64".to_owned(),
                    revision: 9,
                    updated_at: Utc::now().to_rfc3339(),
                    bound_base_url: None,
                    pending_url_binding: false,
                },
            )
            .await
            .unwrap();
        }

        let api_key = save_persisted_youtube_api_key(&store, &vault, "AIza-replacement")
            .await
            .unwrap();
        assert_eq!(api_key.revision, 10);
        assert_eq!(api_key.api_key, "AIza-replacement");

        let token = YoutubeOAuthToken {
            refresh_token: "replacement-refresh".to_owned(),
            access_token: Some("replacement-access".to_owned()),
            access_token_expires_at: Some(Utc::now() + chrono::Duration::minutes(5)),
            scopes: vec![
                "https://www.googleapis.com/auth/youtube.readonly".to_owned(),
            ],
            token_type: "Bearer".to_owned(),
        };
        let oauth = save_persisted_youtube_oauth(&store, &vault, &token)
            .await
            .unwrap();
        assert_eq!(oauth.revision, 10);

        let session = save_persisted_youtube_web_session(&store, &vault, "SAPISID=replacement")
            .await
            .unwrap();
        assert_eq!(session.revision, 10);

        assert_eq!(
            load_persisted_youtube_api_key(&store, &vault)
                .await
                .unwrap()
                .unwrap()
                .api_key,
            "AIza-replacement"
        );
        assert_eq!(
            load_persisted_youtube_oauth(&store, &vault)
                .await
                .unwrap()
                .unwrap()
                .token
                .refresh_token,
            "replacement-refresh"
        );
        assert_eq!(
            load_persisted_youtube_web_session(&store, &vault)
                .await
                .unwrap()
                .unwrap()
                .cookie,
            "SAPISID=replacement"
        );
    }

    #[tokio::test]
    async fn legacy_youtube_ciphertext_migrates_without_breaking_bili_or_llm_secrets() {
        let root = temporary_test_dir("legacy-migration");
        let database_path = root.join("oshical.db");
        let key_file = default_master_key_file(&database_path);
        let legacy = SecretVault::from_material(&legacy_key_material(None, &database_path));
        let store = SqliteStore::in_memory().await.unwrap();

        let youtube_stored = StoredSecret {
            ciphertext: legacy.encrypt_youtube_api_key("AIza-legacy-youtube").unwrap(),
            revision: 4,
            updated_at: Utc::now().to_rfc3339(),
            bound_base_url: None,
            pending_url_binding: false,
        };
        store_youtube_secret(&store, YOUTUBE_API_KEY_STATE_KEY, &youtube_stored)
            .await
            .unwrap();
        let bili_stored = StoredSecret {
            ciphertext: legacy.encrypt_bili_cookie("SESSDATA=legacy").unwrap(),
            revision: 2,
            updated_at: Utc::now().to_rfc3339(),
            bound_base_url: None,
            pending_url_binding: false,
        };
        store
            .set_client_sync_state(
                BILI_SECRET_STATE_KEY,
                &serde_json::to_string(&bili_stored).unwrap(),
            )
            .await
            .unwrap();
        let llm_stored = StoredSecret {
            ciphertext: legacy.encrypt("sk-legacy-llm").unwrap(),
            revision: 7,
            updated_at: Utc::now().to_rfc3339(),
            bound_base_url: Some("https://llm.example/v1".to_owned()),
            pending_url_binding: false,
        };
        store
            .set_client_sync_state(
                LLM_SECRET_STATE_KEY,
                &serde_json::to_string(&llm_stored).unwrap(),
            )
            .await
            .unwrap();

        let safe = SecretVault::from_config_material(
            None,
            &database_path,
            None,
            Some(&key_file),
        );
        assert_eq!(safe.key_source, SecretKeySource::GeneratedFile);

        let youtube = load_persisted_youtube_api_key(&store, &safe)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(youtube.api_key, "AIza-legacy-youtube");
        let raw = store
            .get_client_sync_state(YOUTUBE_API_KEY_STATE_KEY)
            .await
            .unwrap()
            .unwrap();
        let migrated: StoredSecret = serde_json::from_str(&raw).unwrap();
        let (plaintext, used_legacy) = safe
            .decrypt_youtube_api_key_detailed(&migrated.ciphertext)
            .unwrap();
        assert_eq!(plaintext, "AIza-legacy-youtube");
        assert!(!used_legacy, "YouTube ciphertext was not migrated");
        assert!(
            legacy
                .decrypt_youtube_api_key_detailed(&migrated.ciphertext)
                .is_err(),
            "migrated YouTube ciphertext is still decryptable by the legacy key"
        );

        let bili = load_persisted_bili_secret(&store, &safe)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bili.cookie, "SESSDATA=legacy");
        let llm = load_persisted_llm_secret(&store, &safe)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(llm.api_key, "sk-legacy-llm");
        save_persisted_llm_secret(&store, &safe, "sk-legacy-llm", None)
            .await
            .unwrap();
        let llm_raw = store
            .get_client_sync_state(LLM_SECRET_STATE_KEY)
            .await
            .unwrap()
            .unwrap();
        let llm_migrated: StoredSecret = serde_json::from_str(&llm_raw).unwrap();
        assert_eq!(
            safe.decrypt(&llm_migrated.ciphertext).unwrap(),
            "sk-legacy-llm"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unavailable_safe_key_rejects_youtube_migration_but_preserves_legacy_data() {
        let root = temporary_test_dir("unavailable-key");
        let database_path = root.join("oshical.db");
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"blocker").unwrap();
        let unavailable_key_file = blocker.join("master.secrets-key");
        let legacy = SecretVault::from_material(&legacy_key_material(None, &database_path));
        let store = SqliteStore::in_memory().await.unwrap();

        let youtube_stored = StoredSecret {
            ciphertext: legacy.encrypt_youtube_api_key("AIza-must-survive").unwrap(),
            revision: 1,
            updated_at: Utc::now().to_rfc3339(),
            bound_base_url: None,
            pending_url_binding: false,
        };
        store_youtube_secret(&store, YOUTUBE_API_KEY_STATE_KEY, &youtube_stored)
            .await
            .unwrap();
        let bili_stored = StoredSecret {
            ciphertext: legacy.encrypt_bili_cookie("SESSDATA=still-readable").unwrap(),
            revision: 1,
            updated_at: Utc::now().to_rfc3339(),
            bound_base_url: None,
            pending_url_binding: false,
        };
        store
            .set_client_sync_state(
                BILI_SECRET_STATE_KEY,
                &serde_json::to_string(&bili_stored).unwrap(),
            )
            .await
            .unwrap();

        let unavailable = SecretVault::from_config_material(
            None,
            &database_path,
            None,
            Some(&unavailable_key_file),
        );
        assert_eq!(unavailable.key_source, SecretKeySource::Unavailable);
        let error = match load_persisted_youtube_api_key(&store, &unavailable).await {
            Ok(_) => panic!("unsafe key unexpectedly accepted"),
            Err(error) => error,
        };
        match error {
            AppError::External { code, message } => {
                assert_eq!(code, "SECRET_MASTER_KEY_UNAVAILABLE");
                assert!(message.contains("OSHICAL_SECRETS_KEY"));
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(store
            .get_client_sync_state(YOUTUBE_API_KEY_STATE_KEY)
            .await
            .unwrap()
            .is_some());
        let bili = load_persisted_bili_secret(&store, &unavailable)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bili.cookie, "SESSDATA=still-readable");

        let fresh_store = SqliteStore::in_memory().await.unwrap();
        let save_error = match save_persisted_youtube_api_key(
            &fresh_store,
            &unavailable,
            "AIza-new-must-be-rejected",
        )
        .await
        {
            Ok(_) => panic!("new YouTube credential was written with an unsafe key"),
            Err(error) => error,
        };
        match save_error {
            AppError::External { code, .. } => {
                assert_eq!(code, "SECRET_MASTER_KEY_UNAVAILABLE");
            }
            other => panic!("unexpected error: {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }
}
