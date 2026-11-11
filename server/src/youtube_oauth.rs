use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use oshical_core::{load_settings, AppError, SqliteStore};
use oshical_core::youtube::{YouTubeChannel, YouTubeClient};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::api::{sync_runtime_secrets_from_vault, AppState};
use crate::config::{validate_oauth_redirect_uri, YouTubeConfig};
use crate::secrets::{
    clear_persisted_youtube_oauth, load_persisted_youtube_api_key,
    load_persisted_youtube_oauth, save_persisted_youtube_oauth, SecretVault,
    YoutubeOAuthToken,
};

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
const YOUTUBE_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/youtube.readonly";
const OAUTH_STATE_TTL_SECONDS: i64 = 10 * 60;
const OAUTH_REFRESH_SKEW_SECONDS: i64 = 120;
const MAX_PENDING_OAUTH_STATES: usize = 256;

static YOUTUBE_RUNTIME: OnceLock<Arc<YouTubeServer>> = OnceLock::new();
static OAUTH_REFRESH_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug)]
pub(crate) struct YouTubeError {
    pub code: String,
    pub message: String,
    pub status: StatusCode,
}

impl YouTubeError {
    fn not_configured(message: impl Into<String>) -> Self {
        Self {
            code: "YOUTUBE_NOT_CONFIGURED".to_owned(),
            message: message.into(),
            status: StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn storage(message: impl Into<String>) -> Self {
        Self {
            code: "YOUTUBE_STORAGE_ERROR".to_owned(),
            message: message.into(),
            status: StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self {
            code: "YOUTUBE_UPSTREAM_ERROR".to_owned(),
            message: message.into(),
            status: StatusCode::BAD_GATEWAY,
        }
    }
}

impl From<AppError> for YouTubeError {
    fn from(value: AppError) -> Self {
        match &value {
            AppError::Validation(_) | AppError::Url(_) => Self {
                code: "YOUTUBE_INVALID_INPUT".to_owned(),
                message: value.to_string(),
                status: StatusCode::BAD_REQUEST,
            },
            AppError::Http(_) | AppError::Json(_) => Self::upstream(value.to_string()),
            AppError::NotConfigured(message) => Self {
                code: "YOUTUBE_NOT_CONFIGURED".to_owned(),
                message: message.clone(),
                status: StatusCode::SERVICE_UNAVAILABLE,
            },
            AppError::Shared { code, message } if code.starts_with("YOUTUBE_") => Self {
                code: (*code).to_owned(),
                message: message.clone(),
                status: youtube_status_from_code(*code),
            },
            AppError::External { code, message } if code.starts_with("YOUTUBE_") => Self {
                code: code.clone(),
                message: message.clone(),
                status: youtube_status_from_code(code.as_str()),
            },
            _ => Self::storage(value.to_string()),
        }
    }
}

fn youtube_status_from_code(code: &str) -> StatusCode {
    if let Some(status) = code
        .strip_prefix("YOUTUBE_HTTP_")
        .and_then(|value| value.parse::<u16>().ok())
        .and_then(|value| StatusCode::from_u16(value).ok())
        .filter(|status| status.is_client_error() || status.is_server_error())
    {
        return status;
    }
    match code {
        "YOUTUBE_API_KEY_INVALID"
        | "YOUTUBE_BAD_REQUEST"
        | "YOUTUBE_INVALID_INPUT"
        | "YOUTUBE_OAUTH_REFRESH_TOKEN_MISSING"
        | "YOUTUBE_OAUTH_STATE_INVALID" => StatusCode::BAD_REQUEST,
        "YOUTUBE_UNAUTHORIZED" => StatusCode::UNAUTHORIZED,
        "YOUTUBE_FEATURE_DISABLED"
        | "YOUTUBE_FORBIDDEN"
        | "YOUTUBE_OAUTH_ACCESS_DENIED" => StatusCode::FORBIDDEN,
        "YOUTUBE_NOT_FOUND" => StatusCode::NOT_FOUND,
        "YOUTUBE_QUOTA_EXCEEDED" | "YOUTUBE_RATE_LIMITED" | "YOUTUBE_OAUTH_BUSY" => {
            StatusCode::TOO_MANY_REQUESTS
        }
        "YOUTUBE_STORAGE_ERROR" => StatusCode::INTERNAL_SERVER_ERROR,
        "YOUTUBE_NOT_CONFIGURED" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    }
}

pub(crate) struct YouTubeServer {
    http: Client,
    oauth_client_id: Option<String>,
    oauth_client_secret: Option<String>,
    oauth_redirect_uri: Option<String>,
    pending_oauth: Mutex<HashMap<String, PendingOAuth>>,
    last_oauth_error: Mutex<Option<String>>,
}

struct PendingOAuth {
    verifier: String,
    redirect_uri: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeOAuthStart {
    pub authorization_url: String,
    pub state: String,
    pub expires_at: DateTime<Utc>,
    pub expires_in_seconds: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YouTubeOAuthStatus {
    pub configured: bool,
    pub revision: i64,
    pub updated_at: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
    pub expired: bool,
    pub last_error: Option<String>,
}

impl YouTubeOAuthStatus {
    fn missing() -> Self {
        Self {
            configured: false,
            revision: 0,
            updated_at: None,
            expires_at: None,
            scopes: Vec::new(),
            expired: false,
            last_error: None,
        }
    }
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

pub(crate) fn init_youtube_runtime(
    config: YouTubeConfig,
) -> Result<Arc<YouTubeServer>, String> {
    if let Some(existing) = YOUTUBE_RUNTIME.get() {
        return Ok(existing.clone());
    }
    let server = Arc::new(YouTubeServer::new(config)?);
    let _ = YOUTUBE_RUNTIME.set(server.clone());
    Ok(YOUTUBE_RUNTIME.get().cloned().unwrap_or(server))
}

pub(crate) fn youtube_runtime() -> &'static Arc<YouTubeServer> {
    YOUTUBE_RUNTIME.get_or_init(|| {
        Arc::new(
            YouTubeServer::new(YouTubeConfig::default())
                .unwrap_or_else(|_| YouTubeServer::fallback()),
        )
    })
}

impl YouTubeServer {
    fn new(config: YouTubeConfig) -> Result<Self, String> {
        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent("OshiCal/YouTube")
            .build()
            .map_err(|_| "初始化 YouTube HTTP 客户端失败".to_owned())?;
        Ok(Self {
            http,
            oauth_client_id: config.oauth_client_id,
            oauth_client_secret: config.oauth_client_secret,
            oauth_redirect_uri: config.oauth_redirect_uri,
            pending_oauth: Mutex::new(HashMap::new()),
            last_oauth_error: Mutex::new(None),
        })
    }

    fn fallback() -> Self {
        Self {
            http: Client::new(),
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_redirect_uri: None,
            pending_oauth: Mutex::new(HashMap::new()),
            last_oauth_error: Mutex::new(None),
        }
    }

    fn oauth_parts(&self) -> Result<(&str, &str, &str), YouTubeError> {
        let client_id = self
            .oauth_client_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| YouTubeError::not_configured("YouTube OAuth Client ID 未配置"))?;
        let client_secret = self
            .oauth_client_secret
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| YouTubeError::not_configured("YouTube OAuth Client Secret 未配置"))?;
        let redirect_uri = self
            .oauth_redirect_uri
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| YouTubeError::not_configured("YouTube OAuth 回调地址未配置"))?;
        validate_oauth_redirect_uri(redirect_uri).map_err(|message| YouTubeError {
            code: "YOUTUBE_INVALID_INPUT".to_owned(),
            message,
            status: StatusCode::BAD_REQUEST,
        })?;
        Ok((client_id, client_secret, redirect_uri))
    }

    pub(crate) fn oauth_configured(&self) -> bool {
        self.oauth_parts().is_ok()
    }

    async fn client(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
    ) -> Result<YouTubeClient, YouTubeError> {
        let api_key = load_persisted_youtube_api_key(store, vault)
            .await?
            .map(|value| value.api_key)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| YouTubeError::not_configured("YouTube API Key 未配置"))?;
        Ok(YouTubeClient::new(api_key)?)
    }

    pub async fn search_channels(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
        query: &str,
    ) -> Result<Vec<Value>, YouTubeError> {
        let client = self.client(store, vault).await?;
        let channels = client.search_channels(query, 10).await?;
        Ok(channels.into_iter().map(channel_to_account).collect())
    }

    pub async fn resolve_channel_input(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
        input: &str,
    ) -> Result<Value, YouTubeError> {
        let client = self.client(store, vault).await?;
        let channel = client.resolve_channel(input).await?;
        Ok(channel_to_account(channel))
    }

    pub async fn start_oauth(&self) -> Result<YouTubeOAuthStart, YouTubeError> {
        let (client_id, _, redirect_uri) = self.oauth_parts()?;
        self.clean_pending_oauth().await;
        let state = random_url_token(32);
        let verifier = random_url_token(64);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let expires_at = Utc::now() + ChronoDuration::seconds(OAUTH_STATE_TTL_SECONDS);
        {
            let mut pending = self.pending_oauth.lock().await;
            if pending.len() >= MAX_PENDING_OAUTH_STATES {
                return Err(YouTubeError {
                    code: "YOUTUBE_OAUTH_BUSY".to_owned(),
                    message: "待完成的 YouTube 授权过多，请稍后再试".to_owned(),
                    status: StatusCode::TOO_MANY_REQUESTS,
                });
            }
            pending.insert(
                state.clone(),
                PendingOAuth {
                    verifier,
                    redirect_uri: redirect_uri.to_owned(),
                    expires_at,
                },
            );
        }
        let mut url = url::Url::parse(GOOGLE_AUTH_URL)
            .map_err(|_| YouTubeError::upstream("YouTube OAuth 地址无效"))?;
        url.query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", YOUTUBE_READONLY_SCOPE)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("include_granted_scopes", "true")
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(YouTubeOAuthStart {
            authorization_url: url.to_string(),
            state,
            expires_at,
            expires_in_seconds: OAUTH_STATE_TTL_SECONDS,
        })
    }

    pub async fn complete_oauth(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
        code: &str,
        state: &str,
    ) -> Result<YouTubeOAuthStatus, YouTubeError> {
        let pending = self.take_pending_oauth(state).await?;
        if pending.expires_at < Utc::now() {
            return Err(invalid_oauth_state());
        }
        let (client_id, client_secret, _) = self.oauth_parts()?;
        let token = self
            .token_request(vec![
                ("grant_type".to_owned(), "authorization_code".to_owned()),
                ("code".to_owned(), code.to_owned()),
                ("code_verifier".to_owned(), pending.verifier),
                ("client_id".to_owned(), client_id.to_owned()),
                ("client_secret".to_owned(), client_secret.to_owned()),
                ("redirect_uri".to_owned(), pending.redirect_uri),
            ])
            .await?;
        let previous_refresh = load_persisted_youtube_oauth(store, vault)
            .await?
            .map(|value| value.token.refresh_token);
        let refresh_token = token
            .refresh_token
            .or(previous_refresh)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| YouTubeError {
                code: "YOUTUBE_OAUTH_REFRESH_TOKEN_MISSING".to_owned(),
                message: "Google 未返回 refresh token，请撤销授权后重试".to_owned(),
                status: StatusCode::BAD_GATEWAY,
            })?;
        let saved = YoutubeOAuthToken {
            refresh_token,
            access_token: Some(token.access_token),
            access_token_expires_at: token
                .expires_in
                .map(|seconds| Utc::now() + ChronoDuration::seconds(seconds.max(0))),
            scopes: token
                .scope
                .as_deref()
                .map(|scope| scope.split_whitespace().map(ToOwned::to_owned).collect())
                .unwrap_or_else(|| vec![YOUTUBE_READONLY_SCOPE.to_owned()]),
            token_type: token.token_type.unwrap_or_else(|| "Bearer".to_owned()),
        };
        save_persisted_youtube_oauth(store, vault, &saved).await?;
        *self.last_oauth_error.lock().await = None;
        self.oauth_status(store, vault).await
    }

    pub async fn discard_oauth_state(&self, state: &str) {
        self.pending_oauth.lock().await.remove(state);
    }

    pub async fn oauth_status(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
    ) -> Result<YouTubeOAuthStatus, YouTubeError> {
        let Some(saved) = load_persisted_youtube_oauth(store, vault).await? else {
            return Ok(YouTubeOAuthStatus::missing());
        };
        let expired = saved
            .token
            .access_token_expires_at
            .is_some_and(|expires_at| expires_at <= Utc::now());
        Ok(YouTubeOAuthStatus {
            configured: !saved.token.refresh_token.trim().is_empty(),
            revision: saved.revision,
            updated_at: Some(saved.updated_at),
            expires_at: saved.token.access_token_expires_at,
            scopes: saved.token.scopes,
            expired,
            last_error: self.last_oauth_error.lock().await.clone(),
        })
    }

    pub async fn refresh_oauth(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
    ) -> Result<YouTubeOAuthStatus, YouTubeError> {
        let _guard = OAUTH_REFRESH_LOCK.lock().await;
        if let Err(error) = self.refresh_oauth_locked(store, vault).await {
            *self.last_oauth_error.lock().await = Some(error.message.clone());
            return Err(error);
        }
        *self.last_oauth_error.lock().await = None;
        self.oauth_status(store, vault).await
    }

    pub async fn refresh_oauth_if_needed(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
    ) -> Result<bool, YouTubeError> {
        let Some(saved) = load_persisted_youtube_oauth(store, vault).await? else {
            return Ok(false);
        };
        if saved.token.access_token.is_some()
            && saved
                .token
                .access_token_expires_at
                .is_some_and(|expires_at| {
                    expires_at > Utc::now() + ChronoDuration::seconds(OAUTH_REFRESH_SKEW_SECONDS)
                })
        {
            return Ok(false);
        }
        let _guard = OAUTH_REFRESH_LOCK.lock().await;
        if let Err(error) = self.refresh_oauth_locked(store, vault).await {
            *self.last_oauth_error.lock().await = Some(error.message.clone());
            return Err(error);
        }
        *self.last_oauth_error.lock().await = None;
        Ok(true)
    }

    async fn refresh_oauth_locked(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
    ) -> Result<(), YouTubeError> {
        let saved = load_persisted_youtube_oauth(store, vault)
            .await?
            .ok_or_else(|| YouTubeError::not_configured("YouTube OAuth 尚未授权"))?;
        let (client_id, client_secret, _) = self.oauth_parts()?;
        let token = self
            .token_request(vec![
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("refresh_token".to_owned(), saved.token.refresh_token.clone()),
                ("client_id".to_owned(), client_id.to_owned()),
                ("client_secret".to_owned(), client_secret.to_owned()),
            ])
            .await?;
        let next = YoutubeOAuthToken {
            refresh_token: token
                .refresh_token
                .filter(|value| !value.is_empty())
                .unwrap_or(saved.token.refresh_token),
            access_token: Some(token.access_token),
            access_token_expires_at: token
                .expires_in
                .map(|seconds| Utc::now() + ChronoDuration::seconds(seconds.max(0))),
            scopes: token
                .scope
                .as_deref()
                .map(|scope| scope.split_whitespace().map(ToOwned::to_owned).collect())
                .unwrap_or(saved.token.scopes),
            token_type: token
                .token_type
                .unwrap_or_else(|| saved.token.token_type.clone()),
        };
        save_persisted_youtube_oauth(store, vault, &next).await?;
        Ok(())
    }

    async fn token_request(
        &self,
        params: Vec<(String, String)>,
    ) -> Result<OAuthTokenResponse, YouTubeError> {
        let response = self
            .http
            .post(GOOGLE_TOKEN_URL)
            .form(&params)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| YouTubeError::upstream("请求 Google OAuth 令牌端点失败"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|_| YouTubeError::upstream("读取 Google OAuth 响应失败"))?;
        if !status.is_success() {
            return Err(YouTubeError {
                code: "YOUTUBE_OAUTH_EXCHANGE_FAILED".to_owned(),
                message: "Google OAuth 授权失败或已过期，请重新授权".to_owned(),
                status: StatusCode::BAD_GATEWAY,
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| YouTubeError {
            code: "YOUTUBE_OAUTH_BAD_RESPONSE".to_owned(),
            message: "Google OAuth 返回了无效响应".to_owned(),
            status: StatusCode::BAD_GATEWAY,
        })
    }

    pub async fn logout(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
    ) -> Result<YouTubeOAuthStatus, YouTubeError> {
        if let Some(saved) = load_persisted_youtube_oauth(store, vault).await? {
            let token = saved
                .token
                .access_token
                .filter(|value| !value.is_empty())
                .unwrap_or(saved.token.refresh_token);
            if !token.is_empty() {
                let _ = self
                    .http
                    .post(GOOGLE_REVOKE_URL)
                    .form(&[("token", token.as_str())])
                    .send()
                    .await;
            }
        }
        clear_persisted_youtube_oauth(store).await?;
        *self.last_oauth_error.lock().await = None;
        Ok(YouTubeOAuthStatus::missing())
    }

    pub async fn access_token(
        &self,
        store: &SqliteStore,
        vault: &SecretVault,
    ) -> Result<String, YouTubeError> {
        self.refresh_oauth_if_needed(store, vault).await?;
        let saved = load_persisted_youtube_oauth(store, vault)
            .await?
            .ok_or_else(|| YouTubeError::not_configured("YouTube OAuth 尚未授权"))?;
        saved
            .token
            .access_token
            .filter(|value| !value.is_empty())
            .ok_or_else(|| YouTubeError::not_configured("YouTube OAuth access token 不可用"))
    }

    async fn clean_pending_oauth(&self) {
        let now = Utc::now();
        let mut pending = self.pending_oauth.lock().await;
        pending.retain(|_, value| value.expires_at > now);
    }

    async fn take_pending_oauth(&self, state: &str) -> Result<PendingOAuth, YouTubeError> {
        let mut pending = self.pending_oauth.lock().await;
        pending.remove(state).ok_or_else(invalid_oauth_state)
    }
}

fn invalid_oauth_state() -> YouTubeError {
    YouTubeError {
        code: "YOUTUBE_OAUTH_STATE_INVALID".to_owned(),
        message: "YouTube OAuth state 无效、已过期或已被使用".to_owned(),
        status: StatusCode::BAD_REQUEST,
    }
}

fn random_url_token(bytes: usize) -> String {
    let mut material = Vec::with_capacity(bytes);
    while material.len() < bytes {
        material.extend_from_slice(Uuid::new_v4().as_bytes());
    }
    URL_SAFE_NO_PAD.encode(&material[..bytes])
}

fn channel_to_account(channel: YouTubeChannel) -> Value {
    json!({
        "platform": "youtube",
        "mid": channel.id,
        "name": channel.name,
        "avatarUrl": channel.avatar_url.unwrap_or_default(),
        "handle": channel.handle,
        "profileUrl": channel.profile_url,
        "sign": channel.description,
        "roomId": Value::Null,
        "fans": channel.subscriber_count.unwrap_or(0),
        "isLive": false,
        "liveUrl": Value::Null,
        "uploadsPlaylistId": channel.uploads_playlist_id,
    })
}

pub async fn youtube_feature_enabled(store: &SqliteStore) -> Result<bool, AppError> {
    Ok(load_settings(store).await?.youtube_enabled)
}

pub async fn set_youtube_feature_enabled(
    store: &SqliteStore,
    enabled: bool,
) -> Result<(), AppError> {
    store
        .set_settings(&[(
            "youtube_enabled",
            serde_json::to_string(&enabled)?,
        )])
        .await
}

#[derive(Deserialize)]
struct OAuthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn oauth_callback(
    State(state): State<AppState>,
    Query(query): Query<OAuthCallbackQuery>,
) -> Response {
    let callback_state = query.state.as_deref();
    let enabled = match youtube_feature_enabled(state.service.store()).await {
        Ok(value) => value,
        Err(_) => {
            discard_callback_oauth_state(callback_state).await;
            return callback_page_with_code(
                StatusCode::INTERNAL_SERVER_ERROR,
                "读取 YouTube 开关失败",
                "请返回 OshiCal 设置页重试。",
                Some("YOUTUBE_STORAGE_ERROR"),
            );
        }
    };
    if !enabled {
        discard_callback_oauth_state(callback_state).await;
        return callback_page_with_code(
            StatusCode::FORBIDDEN,
            "YouTube 功能未开启",
            "请先在 OshiCal 设置页开启 YouTube 功能，然后重新授权。",
            Some("YOUTUBE_FEATURE_DISABLED"),
        );
    }
    if let Some(error) = query.error.as_deref() {
        discard_callback_oauth_state(callback_state).await;
        return callback_page_with_code(
            StatusCode::BAD_REQUEST,
            "YouTube 授权未完成",
            &format!("Google 返回：{}", escape_html(error)),
            Some("YOUTUBE_OAUTH_ACCESS_DENIED"),
        );
    }
    let (Some(code), Some(validated_state)) = (query.code.as_deref(), query.state.as_deref()) else {
        discard_callback_oauth_state(callback_state).await;
        return callback_page_with_code(
            StatusCode::BAD_REQUEST,
            "YouTube 授权参数不完整",
            "请返回 OshiCal 重新发起授权。",
            Some("YOUTUBE_INVALID_INPUT"),
        );
    };
    match youtube_runtime()
        .complete_oauth(
            state.service.store(),
            &state.secret_vault,
            code,
            validated_state,
        )
        .await
    {
        Ok(_) => {
            if let Err(error) = sync_runtime_secrets_from_vault(&state).await {
                tracing::warn!(%error, "YouTube OAuth 完成后同步运行时凭据失败");
                callback_page_with_code(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "YouTube 授权已保存",
                    "运行时凭据同步失败，请返回 OshiCal 设置页重试。",
                    Some("YOUTUBE_STORAGE_ERROR"),
                )
            } else {
                callback_page(
                    StatusCode::OK,
                    "YouTube 授权成功",
                    "可以关闭此页面并返回 OshiCal。",
                )
            }
        }
        Err(error) => callback_page_with_code(
            error.status,
            "YouTube 授权失败",
            &escape_html(&error.message),
            Some(error.code.as_str()),
        ),
    }
}

async fn discard_callback_oauth_state(oauth_state: Option<&str>) {
    if let Some(oauth_state) = oauth_state {
        youtube_runtime().discard_oauth_state(oauth_state).await;
    }
}

fn callback_page(status: StatusCode, title: &str, message: &str) -> Response {
    callback_page_with_code(status, title, message, None)
}

fn callback_page_with_code(
    status: StatusCode,
    title: &str,
    message: &str,
    error_code: Option<&str>,
) -> Response {
    let error_code_attr = error_code
        .map(|code| format!(" data-error-code=\"{}\"", escape_html(code)))
        .unwrap_or_default();
    let body = format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>body{{font-family:system-ui,sans-serif;max-width:560px;margin:12vh auto;padding:24px;line-height:1.6;color:#1f2937}}h1{{font-size:24px}}.card{{border:1px solid #e5e7eb;border-radius:16px;padding:28px;box-shadow:0 12px 40px rgba(15,23,42,.08)}}</style></head><body><main class=\"card\"{error_code_attr}><h1>{title}</h1><p>{message}</p></main></body></html>"
    );
    let mut response = (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Html(body),
    )
        .into_response();
    if let Some(code) = error_code {
        if let Ok(value) = HeaderValue::from_bytes(code.as_bytes()) {
            response.headers_mut().insert("x-oshical-error-code", value);
        }
    }
    response
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_parts_reject_non_loopback_http_even_for_programmatic_config() {
        let server = YouTubeServer::new(YouTubeConfig {
            oauth_client_id: Some("client-id".to_owned()),
            oauth_client_secret: Some("client-secret".to_owned()),
            oauth_redirect_uri: Some("http://oshical.example.com/callback".to_owned()),
            ..YouTubeConfig::default()
        })
        .unwrap();
        let error = server.oauth_parts().unwrap_err();
        assert_eq!(error.code, "YOUTUBE_INVALID_INPUT");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn app_errors_preserve_every_youtube_code_and_map_only_http_status() {
        for code in [
            "YOUTUBE_BAD_REQUEST",
            "YOUTUBE_COMMUNITY_FORMAT",
            "YOUTUBE_LIVE_FORMAT",
            "YOUTUBE_NETWORK",
            "YOUTUBE_RESPONSE_SIZE",
            "YOUTUBE_HTTP_429",
        ] {
            let source = AppError::External {
                code: code.to_owned(),
                message: "fixture".to_owned(),
            };
            let converted = YouTubeError::from(source);
            assert_eq!(converted.code, code);
        }

        let rate_limited = YouTubeError::from(AppError::External {
            code: "YOUTUBE_HTTP_429".to_owned(),
            message: "fixture".to_owned(),
        });
        assert_eq!(rate_limited.status, StatusCode::TOO_MANY_REQUESTS);

        let format_error = YouTubeError::from(AppError::External {
            code: "YOUTUBE_COMMUNITY_FORMAT".to_owned(),
            message: "fixture".to_owned(),
        });
        assert_eq!(format_error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(format_error.code, "YOUTUBE_COMMUNITY_FORMAT");

        let shared = YouTubeError::from(AppError::Shared {
            code: "YOUTUBE_NETWORK",
            message: "fixture".to_owned(),
        });
        assert_eq!(shared.code, "YOUTUBE_NETWORK");
        assert_eq!(shared.status, StatusCode::BAD_GATEWAY);
    }
}
