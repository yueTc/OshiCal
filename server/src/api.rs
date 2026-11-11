use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::bili_login::BiliQrManager;
use crate::config::ALLOWED_WEB_ORIGINS;
use crate::jobs::{
    job_progress_message, RefreshJob, RefreshJobAccepted, RefreshJobManager, RefreshJobProgress,
};
use crate::secrets::{
    bind_persisted_llm_secret_url, clear_persisted_youtube_api_key,
    clear_persisted_youtube_web_session, llm_secret_status, load_persisted_bili_secret,
    load_persisted_llm_secret, load_persisted_youtube_api_key, load_persisted_youtube_oauth,
    load_persisted_youtube_web_session, save_persisted_llm_secret, save_persisted_youtube_api_key,
    save_persisted_youtube_web_session, youtube_api_key_status,
    youtube_web_session_status, SecretVault, YoutubeSecretStatus,
};
use crate::sync_api;
use crate::update_manager::{ServerUpdateManager, ServerUpdateSettings, ServerUpdateView};
use crate::youtube_oauth::{
    oauth_callback, runtime_youtube_access_token, youtube_feature_enabled, youtube_runtime,
    YouTubeError, YouTubeOAuthStatus,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::uri::{Authority, Uri};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::SinkExt;
use oshical_core::settings::ReasoningEffort;
use oshical_core::{
    load_settings, AppError, AppService, AppSettings, LlmConfig, LlmDiscoveryConfig, NewSchedule,
    NewWeeklyScheduleRule, ProxyTestInput, RuntimeSecrets, SqliteStore, SyncMutation, UpdateSchedule,
    UpdateSubscription, UpdateWeeklyScheduleRule,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{broadcast, Mutex};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

static SERVER_SETTINGS_WRITE_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<AppService>,
    pub api_token: Option<Arc<String>>,
    pub secret_vault: SecretVault,
    pub hub: broadcast::Sender<Value>,
    pub jobs: RefreshJobManager,
    pub updates: ServerUpdateManager,
}

/// Rebuild the complete runtime secret set from the encrypted vault.
///
/// YouTube credential changes must not wipe unrelated runtime values. This
/// helper only reads persisted secrets and swaps the service's clients/secret
/// snapshot; it never refreshes OAuth or performs another network request.
pub(crate) async fn sync_runtime_secrets_from_vault(state: &AppState) -> Result<(), AppError> {
    let store = state.service.store();
    let mut first_error = None;
    // A corrupt or temporarily unreadable slot must clear that credential from
    // the live runtime, not leave an older plaintext copy usable in memory.
    let youtube_api_key = match load_persisted_youtube_api_key(store, &state.secret_vault).await {
        Ok(value) => value.map(|persisted| persisted.api_key),
        Err(error) => {
            tracing::warn!(%error, "读取 YouTube API Key 以同步运行时凭据失败");
            first_error.get_or_insert(error);
            None
        }
    };
    let youtube_oauth_access_token =
        match load_persisted_youtube_oauth(store, &state.secret_vault).await {
            Ok(value) => value.and_then(|persisted| {
                runtime_youtube_access_token(&persisted.token)
            }),
            Err(error) => {
                tracing::warn!(%error, "读取 YouTube OAuth 以同步运行时凭据失败");
                first_error.get_or_insert(error);
                None
            }
        };
    let youtube_web_cookie =
        match load_persisted_youtube_web_session(store, &state.secret_vault).await {
            Ok(value) => value.map(|persisted| persisted.cookie),
            Err(error) => {
                tracing::warn!(%error, "读取 YouTube 网页会话以同步运行时凭据失败");
                first_error.get_or_insert(error);
                None
            }
        };
    let bili_cookie = match load_persisted_bili_secret(store, &state.secret_vault).await {
        Ok(value) => value
            .map(|persisted| persisted.cookie)
            .or_else(|| std::env::var("OSHICAL_BILI_COOKIE").ok()),
        Err(error) => {
            tracing::warn!(%error, "读取 B 站登录以同步运行时凭据失败");
            first_error.get_or_insert(error);
            None
        }
    };
    // Keep the live model key (including an env/bootstrap-only value) untouched.
    let llm_api_key = state.service.llm_api_key().await;

    state
        .service
        .update_secrets(RuntimeSecrets {
            bili_cookie: non_empty_secret(bili_cookie),
            llm_api_key: non_empty_secret(llm_api_key),
            youtube_api_key: non_empty_secret(youtube_api_key),
            youtube_oauth_access_token: non_empty_secret(youtube_oauth_access_token),
            youtube_web_cookie: non_empty_secret(youtube_web_cookie),
        })
        .await?;
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn non_empty_secret(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Serialize)]
struct ErrorPayload {
    code: String,
    message: String,
}

#[derive(Debug)]
pub(crate) struct ApiError {
    code: String,
    message: String,
    status: StatusCode,
}

impl From<AppError> for ApiError {
    fn from(value: AppError) -> Self {
        let status = match &value {
            AppError::Shared { code, .. } => match *code {
                "VALIDATION_ERROR" | "URL_ERROR" => StatusCode::BAD_REQUEST,
                "REMOTE_UNAUTHORIZED" => StatusCode::UNAUTHORIZED,
                "NOT_FOUND" => StatusCode::NOT_FOUND,
                "NOT_CONFIGURED" => StatusCode::SERVICE_UNAVAILABLE,
                "DATABASE_ERROR" | "IO_ERROR" => StatusCode::INTERNAL_SERVER_ERROR,
                code if code.starts_with("YOUTUBE_") => youtube_error_status(code),
                _ => StatusCode::BAD_GATEWAY,
            },
            AppError::Validation(_) | AppError::Url(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::NotConfigured(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Database(_) | AppError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::External { code, .. } if code.starts_with("YOUTUBE_") => {
                youtube_error_status(code)
            }
            AppError::External { code, .. }
                if matches!(
                    code.as_str(),
                    "MIGRATION_STALE_PREVIEW" | "MIGRATION_PREVIEW_CONTENTION"
                ) =>
            {
                StatusCode::CONFLICT
            }
            _ => StatusCode::BAD_GATEWAY,
        };
        // `AppError::api_code` preserves owned YOUTUBE_* codes while
        // `AppError::code` retains the static projection used internally.
        let code = value.api_code().to_owned();
        Self {
            code,
            message: value.to_string(),
            status,
        }
    }
}

fn youtube_error_status(code: &str) -> StatusCode {
    if let Some(status) = code
        .strip_prefix("YOUTUBE_HTTP_")
        .and_then(|value| value.parse::<u16>().ok())
        .and_then(|value| StatusCode::from_u16(value).ok())
        .filter(|status| status.is_client_error() || status.is_server_error())
    {
        return status;
    }
    match code {
        "YOUTUBE_FEATURE_DISABLED" | "YOUTUBE_FORBIDDEN" | "YOUTUBE_OAUTH_ACCESS_DENIED" => {
            StatusCode::FORBIDDEN
        }
        "YOUTUBE_API_KEY_INVALID"
        | "YOUTUBE_INVALID_INPUT"
        | "YOUTUBE_BAD_REQUEST"
        | "YOUTUBE_OAUTH_REFRESH_TOKEN_MISSING"
        | "YOUTUBE_OAUTH_STATE_INVALID" => StatusCode::BAD_REQUEST,
        "YOUTUBE_NOT_CONFIGURED" => StatusCode::SERVICE_UNAVAILABLE,
        "YOUTUBE_QUOTA_EXCEEDED" | "YOUTUBE_RATE_LIMITED" | "YOUTUBE_OAUTH_BUSY" => {
            StatusCode::TOO_MANY_REQUESTS
        }
        "YOUTUBE_UNAUTHORIZED" => StatusCode::UNAUTHORIZED,
        "YOUTUBE_NOT_FOUND" => StatusCode::NOT_FOUND,
        "YOUTUBE_STORAGE_ERROR" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_GATEWAY,
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(value: serde_json::Error) -> Self {
        AppError::Json(value).into()
    }
}

impl From<YouTubeError> for ApiError {
    fn from(value: YouTubeError) -> Self {
        Self {
            code: value.code.to_owned(),
            message: value.message,
            status: value.status,
        }
    }
}

impl ApiError {
    fn forbidden(message: impl Into<String>) -> Self {
        Self {
            code: "FORBIDDEN".to_owned(),
            message: message.into(),
            status: StatusCode::FORBIDDEN,
        }
    }

    fn migration_id_conflict(message: impl Into<String>) -> Self {
        Self {
            code: "MIGRATION_ID_CONFLICT".to_owned(),
            message: message.into(),
            status: StatusCode::BAD_REQUEST,
        }
    }

    fn youtube_disabled() -> Self {
        Self {
            code: "YOUTUBE_FEATURE_DISABLED".to_owned(),
            message: "YouTube 功能未开启".to_owned(),
            status: StatusCode::FORBIDDEN,
        }
    }

}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let payload = ErrorPayload {
            code: self.code,
            message: self.message,
        };
        (
            self.status,
            Json(json!({
                "ok": false,
                "error": payload
            })),
        )
            .into_response()
    }
}

pub(crate) type ApiResult = Result<Json<Value>, ApiError>;

pub(crate) fn ok<T: Serialize>(data: T) -> ApiResult {
    Ok(Json(json!({"ok": true, "data": data})))
}

#[derive(Clone)]
struct MigrationApplyLock(Arc<Mutex<()>>);

impl Default for MigrationApplyLock {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(())))
    }
}

pub fn build_router(state: AppState) -> Router {
    let migration_lock = MigrationApplyLock::default();
    let protected = Router::new()
        .route("/api/search", get(search_accounts))
        .route("/api/youtube/search", get(search_youtube_accounts))
        .route(
            "/api/youtube/subscriptions",
            post(add_youtube_subscription_route),
        )
        .route(
            "/api/youtube/subscriptions/{id}/links",
            post(add_youtube_linked_account_route),
        )
        .route(
            "/api/youtube/subscriptions/{id}/links/{mid}",
            delete(remove_youtube_linked_account),
        )
        .route(
            "/api/subscriptions",
            get(list_subscriptions).post(add_subscription),
        )
        .route("/api/subscriptions/reorder", post(reorder_subscriptions))
        .route(
            "/api/subscriptions/{id}",
            patch(update_subscription).delete(delete_subscription),
        )
        .route("/api/subscriptions/{id}/links", post(add_linked_account))
        .route(
            "/api/subscriptions/{id}/links/{mid}",
            delete(remove_linked_account),
        )
        .route("/api/subscriptions/{id}/refresh", post(refresh_streamer))
        .route("/api/subscriptions/{id}/fans", get(fan_history))
        .route(
            "/api/subscriptions/{id}/fans/refresh",
            post(refresh_streamer_fans),
        )
        .route("/api/refresh", post(refresh_all))
        .route("/api/schedules", get(list_schedules).post(create_schedule))
        .route(
            "/api/schedules/{id}",
            patch(update_schedule).delete(delete_schedule),
        )
        .route(
            "/api/weekly-rules",
            get(list_weekly_rules).post(create_weekly_rule),
        )
        .route(
            "/api/weekly-rules/{id}",
            patch(update_weekly_rule).delete(delete_weekly_rule),
        )
        .route("/api/schedules/{id}/cancel", post(cancel_schedule))
        .route("/api/live-sessions", get(list_live_sessions))
        .route(
            "/api/live-sessions/{id}/online-samples",
            get(list_live_online_samples),
        )
        .route(
            "/api/live-sessions/by-schedule/{id}",
            get(live_session_for_schedule),
        )
        .route("/api/live-status", get(list_live_status))
        .route("/api/fertility", get(list_fertility_statuses))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route(
            "/api/system/update",
            get(get_server_update).put(put_server_update_settings),
        )
        .route("/api/system/update/check", post(check_server_update))
        .route("/api/system/update/install", post(install_server_update))
        .route("/api/secrets/llm", get(get_llm_secret).put(put_llm_secret))
        .route(
            "/api/secrets/youtube",
            get(get_youtube_secrets)
                .put(put_youtube_api_key)
                .delete(delete_youtube_api_key),
        )
        .route("/api/youtube/oauth/start", post(start_youtube_oauth))
        .route("/api/youtube/oauth/complete", post(complete_youtube_oauth))
        .route("/api/youtube/oauth/refresh", post(refresh_youtube_oauth))
        .route("/api/youtube/oauth/logout", post(logout_youtube_oauth))
        .route(
            "/api/youtube/session",
            get(get_youtube_session)
                .put(put_youtube_session)
                .delete(delete_youtube_session),
        )
        .route("/api/youtube/session/status", get(get_youtube_session))
        .route("/api/youtube/session/start", post(start_youtube_session))
        .route("/api/youtube/status", get(get_youtube_status))
        .route(
            "/api/bilibili/login/qr",
            post(crate::bili_login::start_qr_login),
        )
        .route(
            "/api/bilibili/login/qr/{session_id}",
            get(crate::bili_login::poll_qr_login),
        )
        .route("/api/bilibili/status", get(crate::bili_login::status))
        .route("/api/bilibili/logout", post(crate::bili_login::logout))
        .route("/api/proxy/test", post(test_proxy))
        .route("/api/llm/test", post(test_llm))
        .route("/api/llm/models", post(list_llm_models))
        .route("/api/jobs/refresh", post(refresh_job_all))
        .route("/api/jobs/refresh/{id}", post(refresh_job_streamer))
        .route("/api/jobs/{id}", get(refresh_job_status))
        .route("/api/events", get(events_after))
        .route("/api/sync/snapshot", get(sync_snapshot))
        .route("/api/sync/bootstrap", get(sync_api::bootstrap))
        .route("/api/sync/changes", get(sync_api::changes))
        .route("/api/sync/ack", post(sync_api::ack))
        .route("/api/sync/mutations", post(apply_mutations))
        .route("/api/sync/conflicts", get(sync_conflicts))
        .route("/api/devices/register", post(sync_api::register_device))
        .route("/api/devices", get(sync_api::list_devices))
        .route("/api/devices/{device_id}", delete(sync_api::delete_device))
        .route("/api/migration/preview", post(migration_preview))
        .route("/api/migration/apply", post(migration_apply))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(
            ALLOWED_WEB_ORIGINS.map(HeaderValue::from_static),
        ))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::PUT,
            Method::DELETE,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);

    let dist =
        PathBuf::from(std::env::var("OSHICAL_WEB_DIR").unwrap_or_else(|_| "./dist".to_owned()));
    let index = dist.join("index.html");
    let bili_qr_manager = BiliQrManager::default();
    Router::new()
        .route("/api/health", get(health))
        .route("/api/ws", get(websocket))
        .route("/api/youtube/oauth/callback", get(oauth_callback))
        .merge(protected)
        .fallback_service(ServeDir::new(dist).not_found_service(ServeFile::new(index)))
        .layer(Extension(bili_qr_manager))
        .layer(Extension(migration_lock))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            reject_cross_site_unsafe_http,
        ))
        .layer(
            TraceLayer::new_for_http().make_span_with(
                |request: &Request<axum::body::Body>| {
                    tracing::debug_span!(
                        "http",
                        method = %request.method(),
                        path = %request.uri().path()
                    )
                },
            ),
        )
        .layer(cors)
        .with_state(state)
}

async fn require_auth(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(expected) = &state.api_token else {
        return next.run(request).await;
    };
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token)
        .unwrap_or_default();
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return ApiError::from(AppError::Unauthorized).into_response();
    }
    next.run(request).await
}

async fn reject_cross_site_unsafe_http(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if request.method() == Method::GET
        || request.method() == Method::HEAD
        || request.method() == Method::OPTIONS
    {
        return next.run(request).await;
    }

    // With a bearer token, a cross-site browser cannot add the required
    // Authorization header without a successful CORS preflight. Tokenless
    // loopback deployments rely on an explicit same-origin/allowed-origin gate.
    if state.api_token.is_some() {
        return next.run(request).await;
    }

    if !unsafe_http_origin_allowed(request.headers()) {
        return ApiError::forbidden("拒绝跨站浏览器写入请求").into_response();
    }
    next.run(request).await
}

fn unsafe_http_origin_allowed(headers: &HeaderMap) -> bool {
    if headers.contains_key(header::ORIGIN) {
        return websocket_origin_allowed(headers);
    }
    !headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
}

fn bearer_token(value: &str) -> Option<&str> {
    let mut parts = value.split_ascii_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if !scheme.eq_ignore_ascii_case("bearer") || parts.next().is_some() {
        return None;
    }
    Some(token)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    // Hash both sides first so unequal lengths still execute a fixed-size
    // constant-time comparison and do not leak token length through timing.
    let left = Sha256::digest(left);
    let right = Sha256::digest(right);
    bool::from(left.as_slice().ct_eq(right.as_slice()))
}

async fn health(State(state): State<AppState>) -> ApiResult {
    let health = state.service.store().health().await?;
    ok(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "database": health
    }))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default)]
    platform: Option<String>,
}

async fn require_youtube_enabled(state: &AppState) -> Result<(), ApiError> {
    if youtube_feature_enabled(state.service.store()).await? {
        Ok(())
    } else {
        Err(ApiError::youtube_disabled())
    }
}

async fn require_streamer_youtube_enabled(
    state: &AppState,
    streamer_id: i64,
) -> Result<(), ApiError> {
    let streamer = state.service.store().get_streamer(streamer_id).await?;
    if streamer.platform == oshical_core::models::Platform::Youtube {
        require_youtube_enabled(state).await?;
    }
    Ok(())
}

async fn search_accounts(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> ApiResult {
    let platform = query
        .platform
        .as_deref()
        .unwrap_or("bilibili")
        .to_ascii_lowercase();
    if platform == "youtube" {
        require_youtube_enabled(&state).await?;
        let results = youtube_runtime()
            .search_channels(state.service.store(), &state.secret_vault, &query.q)
            .await?;
        return ok(results);
    }
    if platform != "bilibili" {
        return Err(ApiError::from(AppError::validation(
            "platform 只支持 bilibili 或 youtube",
        )));
    }
    let results = state.service.search_accounts(&query.q).await?;
    ok(results)
}

async fn search_youtube_accounts(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let results = youtube_runtime()
        .search_channels(state.service.store(), &state.secret_vault, &query.q)
        .await?;
    ok(results)
}

async fn list_subscriptions(State(state): State<AppState>) -> ApiResult {
    ok(state.service.list_subscriptions().await?)
}

async fn add_youtube_subscription_route(
    State(state): State<AppState>,
    Json(mut payload): Json<Value>,
) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let object = payload.as_object_mut().ok_or_else(|| {
        ApiError::from(AppError::validation("添加主播请求必须是 JSON 对象"))
    })?;
    // The dedicated route is YouTube regardless of whether the caller supplied
    // a platform field. This prevents a disabled YouTube feature from falling
    // through to the Bilibili upstream path.
    object.insert("platform".to_owned(), json!("youtube"));
    add_subscription(State(state), Json(payload)).await
}

async fn add_subscription(
    State(state): State<AppState>,
    Json(mut payload): Json<Value>,
) -> ApiResult {
    let platform = payload
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or("bilibili")
        .to_ascii_lowercase();
    if platform == "youtube" {
        require_youtube_enabled(&state).await?;
        payload = prepare_youtube_subscription(&state, payload).await?;
    } else if platform != "bilibili" {
        return Err(ApiError::from(AppError::validation(
            "platform 只支持 bilibili 或 youtube",
        )));
    }
    let input: oshical_core::models::AddSubscription = serde_json::from_value(payload)?;
    if platform == "youtube" {
        return ok(state.service.store().add_subscription(&input).await?);
    }
    ok(state.service.add_subscription(input).await?)
}

async fn prepare_youtube_subscription(
    state: &AppState,
    mut payload: Value,
) -> Result<Value, ApiError> {
    let input = payload
        .get("mid")
        .or_else(|| payload.get("channel"))
        .or_else(|| payload.get("handle"))
        .or_else(|| payload.get("url"))
        .or_else(|| payload.get("input"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::from(AppError::validation(
                "添加 YouTube 主播需要频道 ID、handle 或频道 URL",
            ))
        })?;
    let channel = youtube_runtime()
        .resolve_channel_input(state.service.store(), &state.secret_vault, input)
        .await?;
    let object = payload.as_object_mut().ok_or_else(|| {
        ApiError::from(AppError::validation("添加主播请求必须是 JSON 对象"))
    })?;
    object.insert("platform".to_owned(), json!("youtube"));
    object.insert(
        "mid".to_owned(),
        channel.get("mid").cloned().unwrap_or_default(),
    );
    object.insert(
        "name".to_owned(),
        channel.get("name").cloned().unwrap_or_default(),
    );
    object.insert(
        "avatarUrl".to_owned(),
        channel.get("avatarUrl").cloned().unwrap_or_default(),
    );
    object.insert(
        "profileUrl".to_owned(),
        channel.get("profileUrl").cloned().unwrap_or_default(),
    );
    object.insert(
        "handle".to_owned(),
        channel.get("handle").cloned().unwrap_or_default(),
    );
    object.insert(
        "uploadsPlaylistId".to_owned(),
        channel
            .get("uploadsPlaylistId")
            .cloned()
            .unwrap_or_default(),
    );
    object.insert("roomId".to_owned(), Value::Null);
    apply_youtube_subscription_defaults(object);
    Ok(payload)
}

fn apply_youtube_subscription_defaults(object: &mut serde_json::Map<String, Value>) {
    object
        .entry("scheduleEnabled".to_owned())
        .or_insert_with(|| json!(true));
    object
        .entry("liveNotifyEnabled".to_owned())
        .or_insert_with(|| json!(true));
    // Keep the server aligned with the model and desktop default. A newly added
    // YouTube streamer starts with live statistics disabled until the user opts in.
    object
        .entry("liveStatsEnabled".to_owned())
        .or_insert_with(|| json!(false));
}

async fn update_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(update): Json<UpdateSubscription>,
) -> ApiResult {
    ok(state.service.update_subscription(id, &update).await?)
}

#[derive(Debug, Deserialize)]
struct ReorderSubscriptionsInput {
    ids: Vec<i64>,
}

async fn reorder_subscriptions(
    State(state): State<AppState>,
    Json(input): Json<ReorderSubscriptionsInput>,
) -> ApiResult {
    ok(state.service.reorder_subscriptions(&input.ids).await?)
}

async fn delete_subscription(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    state.service.delete_subscription(id).await?;
    ok(json!({"deleted": id}))
}

#[derive(Debug, Deserialize)]
struct AddLinkInput {
    mid: String,
    #[serde(default)]
    platform: Option<String>,
}

async fn add_linked_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<Value>,
) -> ApiResult {
    let platform = payload
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or("bilibili")
        .to_ascii_lowercase();
    if platform == "youtube" {
        require_youtube_enabled(&state).await?;
        return add_youtube_linked_account(state, id, payload).await;
    }
    if platform != "bilibili" {
        return Err(ApiError::from(AppError::validation(
            "platform 只支持 bilibili 或 youtube",
        )));
    }
    let input: AddLinkInput = serde_json::from_value(payload)?;
    ok(state.service.add_linked_account(id, &input.mid).await?)
}

async fn add_youtube_linked_account_route(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<Value>,
) -> ApiResult {
    require_youtube_enabled(&state).await?;
    add_youtube_linked_account(state, id, payload).await
}

async fn add_youtube_linked_account(
    state: AppState,
    id: i64,
    payload: Value,
) -> ApiResult {
    let input = payload
        .get("mid")
        .or_else(|| payload.get("channel"))
        .or_else(|| payload.get("handle"))
        .or_else(|| payload.get("url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::from(AppError::validation(
                "关联 YouTube 账号需要频道 ID、handle 或频道 URL",
            ))
        })?;
    let parent = state
        .service
        .list_subscriptions()
        .await?
        .into_iter()
        .find(|streamer| streamer.id == id)
        .ok_or_else(|| ApiError::from(AppError::not_found(format!("主播 {id}"))))?;
    if parent.platform != oshical_core::models::Platform::Youtube {
        return Err(ApiError::from(AppError::validation(
            "只能为 YouTube 主播关联 YouTube 频道",
        )));
    }
    let channel = youtube_runtime()
        .resolve_channel_input(state.service.store(), &state.secret_vault, input)
        .await?;
    let mid = channel
        .get("mid")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::from(AppError::validation("YouTube 频道 ID 缺失")))?;
    let name = channel.get("name").and_then(Value::as_str).unwrap_or_default();
    let avatar = channel
        .get("avatarUrl")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let handle = channel.get("handle").and_then(Value::as_str);
    let profile_url = channel.get("profileUrl").and_then(Value::as_str);
    let linked = state
        .service
        .store()
        .add_linked_account_for_platform(
            id,
            oshical_core::models::Platform::Youtube,
            mid,
            name,
            avatar,
            handle,
            profile_url,
        )
        .await?;
    ok(linked)
}

async fn remove_linked_account(
    State(state): State<AppState>,
    Path((id, mid)): Path<(i64, String)>,
) -> ApiResult {
    let youtube_link = state
        .service
        .list_subscriptions()
        .await?
        .into_iter()
        .find(|streamer| streamer.id == id)
        .is_some_and(|streamer| {
            streamer.linked_accounts.iter().any(|account| {
                account.mid == mid
                    && account.platform == oshical_core::models::Platform::Youtube
            })
        });
    if youtube_link {
        require_youtube_enabled(&state).await?;
    }
    state
        .service
        .store()
        .remove_linked_account(id, &mid)
        .await?;
    ok(json!({"streamerId": id, "mid": mid}))
}

async fn remove_youtube_linked_account(
    State(state): State<AppState>,
    Path((id, mid)): Path<(i64, String)>,
) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let parent = state
        .service
        .list_subscriptions()
        .await?
        .into_iter()
        .find(|streamer| streamer.id == id)
        .ok_or_else(|| ApiError::from(AppError::not_found(format!("主播 {id}"))))?;
    if parent.platform != oshical_core::models::Platform::Youtube {
        return Err(ApiError::from(AppError::validation(
            "只能删除 YouTube 主播的 YouTube 关联频道",
        )));
    }
    state
        .service
        .store()
        .remove_linked_account(id, &mid)
        .await?;
    ok(json!({"streamerId": id, "mid": mid}))
}

async fn refresh_all(State(state): State<AppState>) -> ApiResult {
    ok(state.service.refresh_all().await?)
}

async fn refresh_streamer(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    require_streamer_youtube_enabled(&state, id).await?;
    let hub = state.hub.clone();
    let report = state
        .service
        .refresh_streamer_with_progress(id, move |progress| {
            let _ = hub.send(json!({
                "type": "refresh_progress",
                "createdAt": Utc::now(),
                "payload": progress
            }));
        })
        .await?;
    ok(report)
}

fn publish_job_progress(state: &AppState, job: &RefreshJob) {
    let _ = state.hub.send(job_progress_message(job));
}

async fn refresh_job_all(State(state): State<AppState>) -> ApiResult {
    let prepared = state.service.prepare_refresh_request(None).await?;
    let registration = state
        .jobs
        .create_all_with_key(prepared.request_key().to_owned());
    let accepted = RefreshJobAccepted::from(&registration);
    if registration.created {
        publish_job_progress(&state, &registration.job);
        let background = state.clone();
        let job_id = registration.job.job_id;
        tokio::spawn(async move {
            if let Some(job) = background.jobs.begin(&job_id) {
                publish_job_progress(&background, &job);
            }
            if let Some(job) = background.jobs.update_progress(
                &job_id,
                RefreshJobProgress {
                    stage: "准备全量刷新".to_owned(),
                    completed: 0,
                    total: 1,
                    percent: 0,
                    done: false,
                    failed: false,
                },
            ) {
                publish_job_progress(&background, &job);
            }
            let progress_registry = background.jobs.clone();
            let progress_hub = background.hub.clone();
            let progress_job_id = job_id.clone();
            match background
                .service
                .refresh_prepared_with_progress(prepared, move |progress| {
                    if let Some(job) =
                        progress_registry.update_streamer_progress(&progress_job_id, &progress)
                    {
                        let _ = progress_hub.send(job_progress_message(&job));
                    }
                })
                .await
            {
                Ok(report) => {
                    let stage = if report.failed_streamers.is_empty() {
                        "全量刷新完成"
                    } else {
                        "全量刷新完成，部分来源失败"
                    };
                    if let Some(job) = background.jobs.finish(&job_id, report, stage) {
                        publish_job_progress(&background, &job);
                    }
                }
                Err(error) => {
                    if let Some(job) = background.jobs.fail(&job_id, error.to_string()) {
                        publish_job_progress(&background, &job);
                    }
                }
            }
        });
    }
    ok(accepted)
}

async fn refresh_job_streamer(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    let streamer = state.service.store().get_streamer(id).await?;
    if streamer.platform == oshical_core::models::Platform::Youtube {
        require_youtube_enabled(&state).await?;
    }
    let prepared = state
        .service
        .prepare_refresh_request(Some(streamer.id))
        .await?;
    let registration = state.jobs.create_streamer_with_key(
        streamer.id,
        streamer.name.clone(),
        prepared.request_key().to_owned(),
    );
    let accepted = RefreshJobAccepted::from(&registration);
    if registration.created {
        publish_job_progress(&state, &registration.job);
        let background = state.clone();
        let job_id = registration.job.job_id;
        tokio::spawn(async move {
            if let Some(job) = background.jobs.begin(&job_id) {
                publish_job_progress(&background, &job);
            }
            let progress_registry = background.jobs.clone();
            let progress_hub = background.hub.clone();
            let progress_job_id = job_id.clone();
            let result = background
                .service
                .refresh_prepared_with_progress(prepared, move |progress| {
                    if let Some(job) =
                        progress_registry.update_streamer_progress(&progress_job_id, &progress)
                    {
                        let _ = progress_hub.send(job_progress_message(&job));
                    }
                })
                .await;
            match result {
                Ok(report) => {
                    let shared_all = background
                        .jobs
                        .get(&job_id)
                        .is_some_and(|job| job.progress.stage.contains("共享全量结果"));
                    let stage = match (shared_all, report.failed_streamers.is_empty()) {
                        (true, true) => "已完成共享全量刷新",
                        (true, false) => "共享全量刷新完成，部分来源失败",
                        (false, true) => "主播刷新完成",
                        (false, false) => "主播刷新完成，部分来源失败",
                    };
                    if let Some(job) = background.jobs.finish(&job_id, report, stage) {
                        publish_job_progress(&background, &job);
                    }
                }
                Err(error) => {
                    if let Some(job) = background.jobs.fail(&job_id, error.to_string()) {
                        publish_job_progress(&background, &job);
                    }
                }
            }
        });
    }
    ok(accepted)
}

async fn refresh_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> ApiResult {
    let job = state
        .jobs
        .get(job_id.trim())
        .ok_or_else(|| ApiError::from(AppError::not_found("刷新任务不存在或已过期")))?;
    ok(job)
}

/// `/api/llm/test` 的请求体。
///
/// 与 `LlmConfig` 的差别只在宽松度：`apiKey` 可缺省，
/// 缺省时由服务端用已保存的密钥补齐，所以客户端不必回显明文。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LlmProbeInput {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: String,
    #[serde(default)]
    prompt_version: Option<String>,
    /// `auto` / `low` / `medium` / `high`；`auto` 等价于不发该字段。
    #[serde(default)]
    reasoning_effort: Option<String>,
}

/// `/api/llm/models` 的请求体。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LlmModelsInput {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    /// 与 `/api/llm/test` 保持同一份请求体形状。列表接口不发送思考程度，
    /// 但传了非法值仍然报错，免得界面以为写错了字段却毫无反应。
    #[serde(default)]
    reasoning_effort: Option<String>,
}

/// 把 `reasoningEffort` 字符串翻译成枚举。
///
/// 空值 / 缺省视为 `Auto`；能解析就直接用；无法解析时明确报错，
/// 免得界面写错一个值却被静默降级成默认行为。
fn parse_reasoning_effort(raw: Option<&str>) -> Result<ReasoningEffort, ApiError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(ReasoningEffort::Auto);
    };
    ReasoningEffort::parse(raw).ok_or_else(|| {
        ApiError::from(AppError::validation(
            "思考程度必须是 auto、low、medium 或 high 之一",
        ))
    })
}

/// 解析一次 LLM 探测要用的密钥。
///
/// 请求里带了非空 `apiKey` 就直接用（方便临时试别的 key）；
/// 否则返回空串，交给 `AppService` 用它掌握的运行时绑定关系补默认密钥。
fn llm_probe_api_key(provided: Option<&str>) -> String {
    provided
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

async fn test_llm(State(state): State<AppState>, Json(input): Json<LlmProbeInput>) -> ApiResult {
    let api_key = llm_probe_api_key(input.api_key.as_deref());
    let prompt_version = match input
        .prompt_version
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        Some(value) => value,
        None => {
            load_settings(state.service.store())
                .await?
                .llm_prompt_version
        }
    };
    let config = LlmConfig {
        base_url: input.base_url,
        api_key,
        model: input.model,
        prompt_version,
        reasoning_effort: parse_reasoning_effort(input.reasoning_effort.as_deref())?,
    };
    ok(state.service.test_llm(config).await?)
}

async fn list_llm_models(
    State(state): State<AppState>,
    Json(input): Json<LlmModelsInput>,
) -> ApiResult {
    // 只做校验：列表请求本身不带思考程度，传了就照同一套规则判合法。
    parse_reasoning_effort(input.reasoning_effort.as_deref())?;
    let api_key = llm_probe_api_key(input.api_key.as_deref());
    let config = LlmDiscoveryConfig {
        base_url: input.base_url,
        api_key,
    };
    ok(state.service.list_llm_models(config).await?)
}

async fn test_proxy(State(state): State<AppState>, Json(input): Json<ProxyTestInput>) -> ApiResult {
    ok(state.service.test_proxy(input.proxy_port).await?)
}

async fn list_live_status(State(state): State<AppState>) -> ApiResult {
    ok(state.service.list_live_statuses().await?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FanHistoryQuery {
    days: Option<i64>,
}

async fn fan_history(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<FanHistoryQuery>,
) -> ApiResult {
    ok(state
        .service
        .get_fan_history(id, query.days.unwrap_or(30))
        .await?)
}

async fn refresh_streamer_fans(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    require_streamer_youtube_enabled(&state, id).await?;
    ok(state.service.refresh_streamer_fans(id).await?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveSessionQuery {
    streamer_id: Option<i64>,
    limit: Option<i64>,
}

async fn list_live_sessions(
    State(state): State<AppState>,
    Query(query): Query<LiveSessionQuery>,
) -> ApiResult {
    ok(state
        .service
        .list_live_sessions(query.streamer_id, query.limit.unwrap_or(50))
        .await?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveOnlineSamplesQuery {
    after_id: Option<i64>,
    limit: Option<i64>,
}

async fn list_live_online_samples(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<LiveOnlineSamplesQuery>,
) -> ApiResult {
    ok(state
        .service
        .store()
        .list_live_online_samples(id, query.after_id.unwrap_or(0), query.limit.unwrap_or(500))
        .await?)
}

async fn live_session_for_schedule(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult {
    ok(state.service.live_session_for_schedule(id).await?)
}

#[derive(Debug, Deserialize)]
struct FertilityQuery {
    refresh: Option<bool>,
}

/// 读取 LAPLACE 生理期数据。开关关闭时返回 `enabled=false` 的空快照，不触网；
/// 缓存过期时立即返回缓存（`stale=true`）并触发后台刷新，`refresh=1` 最多等待 15 秒。
async fn list_fertility_statuses(
    State(state): State<AppState>,
    Query(query): Query<FertilityQuery>,
) -> ApiResult {
    ok(state
        .service
        .list_fertility_statuses(query.refresh.unwrap_or(false))
        .await?)
}

#[derive(Debug, Deserialize)]
struct ScheduleQuery {
    from: Option<String>,
    to: Option<String>,
}

async fn list_schedules(
    State(state): State<AppState>,
    Query(query): Query<ScheduleQuery>,
) -> ApiResult {
    let now = Utc::now();
    let from = query
        .from
        .as_deref()
        .map(parse_datetime)
        .transpose()?
        .unwrap_or(now);
    let to = query
        .to
        .as_deref()
        .map(parse_datetime)
        .transpose()?
        .unwrap_or_else(|| from + ChronoDuration::days(7));
    ok(state.service.list_schedules(from, to).await?)
}

async fn create_schedule(
    State(state): State<AppState>,
    Json(input): Json<NewSchedule>,
) -> ApiResult {
    ok(state.service.create_schedule(&input).await?)
}

async fn update_schedule(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(update): Json<UpdateSchedule>,
) -> ApiResult {
    ok(state.service.update_schedule(id, &update).await?)
}

async fn delete_schedule(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    state.service.delete_schedule(id).await?;
    ok(json!({"deleted": id}))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WeeklyRuleQuery {
    streamer_id: Option<i64>,
}

async fn list_weekly_rules(
    State(state): State<AppState>,
    Query(query): Query<WeeklyRuleQuery>,
) -> ApiResult {
    ok(state.service.list_weekly_rules(query.streamer_id).await?)
}

async fn create_weekly_rule(
    State(state): State<AppState>,
    Json(input): Json<NewWeeklyScheduleRule>,
) -> ApiResult {
    ok(state.service.create_weekly_rule(&input).await?)
}

async fn update_weekly_rule(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(update): Json<UpdateWeeklyScheduleRule>,
) -> ApiResult {
    ok(state.service.update_weekly_rule(id, &update).await?)
}

async fn delete_weekly_rule(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    state.service.delete_weekly_rule(id).await?;
    ok(json!({"deleted": id}))
}

async fn cancel_schedule(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    ok(state.service.store().cancel_schedule(id).await?)
}

fn llm_secret_url_binding_allows(
    bound_base_url: Option<&str>,
    pending_url_binding: bool,
    target_base_url: &str,
) -> bool {
    bound_base_url == Some(target_base_url) || pending_url_binding
}
fn changed_setting_values(
    current: &AppSettings,
    next: &AppSettings,
) -> oshical_core::AppResult<Vec<(&'static str, String)>> {
    let mut values = Vec::new();
    macro_rules! changed {
        ($key:literal, $field:ident) => {
            if current.$field != next.$field {
                values.push(($key, serde_json::to_string(&next.$field)?));
            }
        };
    }
    changed!("data_mode", data_mode);
    changed!("server_url", server_url);
    changed!("live_poll_seconds", live_poll_seconds);
    changed!("dynamic_poll_seconds", dynamic_poll_seconds);
    changed!("auto_analyze_images", auto_analyze_images);
    changed!("notifications_enabled", notifications_enabled);
    changed!("live_stats_enabled", live_stats_enabled);
    changed!("fertility_enabled", fertility_enabled);
    changed!("youtube_enabled", youtube_enabled);
    changed!("llm_base_url", llm_base_url);
    changed!("llm_model", llm_model);
    changed!("llm_prompt_version", llm_prompt_version);
    // 下面三项由 docs/38 引入：不加进来 PUT /api/settings 就会静默丢弃它们。
    changed!("llm_reasoning_effort", llm_reasoning_effort);
    changed!("llm_parallelism", llm_parallelism);
    changed!("proxy_port", proxy_port);
    Ok(values)
}

pub(crate) async fn persist_changed_settings(
    store: &SqliteStore,
    current: &AppSettings,
    next: &AppSettings,
) -> oshical_core::AppResult<bool> {
    next.validate()?;
    let values = changed_setting_values(current, next)?;
    if values.is_empty() {
        return Ok(false);
    }
    store.set_settings(&values).await?;
    Ok(true)
}

async fn get_settings(State(state): State<AppState>) -> ApiResult {
    ok(load_settings(state.service.store()).await?)
}

async fn put_settings(
    State(state): State<AppState>,
    Json(settings): Json<AppSettings>,
) -> ApiResult {
    let _guard = SERVER_SETTINGS_WRITE_LOCK.lock().await;
    let current = load_settings(state.service.store()).await?;
    settings.validate()?;
    let llm_url_changed = settings.llm_base_url.trim() != current.llm_base_url.trim();
    let target_base_url = if !settings.llm_base_url.trim().is_empty() {
        Some(oshical_core::llm::normalized_llm_base_url(
            &settings.llm_base_url,
        )?)
    } else {
        None
    };
    let mut bind_secret_to_url = false;
    let mut validate_target = target_base_url.is_some() && llm_url_changed;
    if let Some(target_base_url) = target_base_url.as_deref() {
        if state.service.llm_api_key().await.is_some() {
            let persisted =
                load_persisted_llm_secret(state.service.store(), &state.secret_vault).await?;
            if let Some(persisted) = persisted {
                let bound_matches = persisted.bound_base_url.as_deref() == Some(target_base_url);
                if llm_url_changed
                    && !llm_secret_url_binding_allows(
                        persisted.bound_base_url.as_deref(),
                        persisted.pending_url_binding,
                        target_base_url,
                    )
                {
                    return Err(ApiError::from(AppError::validation(
                        "当前 API Key 未绑定到目标模型地址，请先为新地址重新保存 API Key",
                    )));
                }
                // Consume a fresh key-first permit even when the URL did not change.
                // This prevents it from authorizing an unrelated URL later in its TTL.
                bind_secret_to_url =
                    persisted.pending_url_binding || (llm_url_changed && !bound_matches);
                validate_target |= persisted.pending_url_binding;
            } else if llm_url_changed {
                return Err(ApiError::from(AppError::validation(
                    "更改模型地址前必须先保存该地址对应的 API Key",
                )));
            }
        }
    }
    if validate_target {
        if let Some(target_base_url) = target_base_url.as_deref() {
            oshical_core::llm::validate_llm_base_url_target(target_base_url, "日程模型地址")
                .await?;
        }
    }
    // Rebuild on every request, including no-op retries. A previous request
    // may have been cancelled after swapping clients but before committing SQL.
    state.service.update_proxy_port(settings.proxy_port).await?;
    if let Err(error) = persist_changed_settings(state.service.store(), &current, &settings).await {
        if let Err(rollback_error) = state.service.update_proxy_port(current.proxy_port).await {
            tracing::error!(%rollback_error, "恢复运行时代理失败；重试设置请求会重新应用配置");
        }
        return Err(ApiError::from(error));
    }
    // Bind after settings commit. If this write fails, the new settings and
    // runtime stay live; the next identical request can consume the still-pending
    // permit and finish the operation without a revision bump.
    if bind_secret_to_url {
        if let Some(target_base_url) = target_base_url.as_deref() {
            let bound = bind_persisted_llm_secret_url(
                state.service.store(),
                &state.secret_vault,
                target_base_url,
            )
            .await?;
            state
                .service
                .update_bound_llm_api_key(bound.api_key, bound.bound_base_url)
                .await?;
        }
    }
    ok(settings)
}

async fn get_server_update(State(state): State<AppState>) -> ApiResult {
    update_view(&state).await
}

async fn put_server_update_settings(
    State(state): State<AppState>,
    Json(settings): Json<ServerUpdateSettings>,
) -> ApiResult {
    let update = state.updates.save_settings(settings).await?;
    emit_server_update(&state, &update);
    ok(update)
}

async fn check_server_update(State(state): State<AppState>) -> ApiResult {
    let update = state.updates.check_now().await?;
    emit_server_update(&state, &update);
    ok(update)
}

async fn install_server_update(State(state): State<AppState>) -> ApiResult {
    let update = state.updates.install_now().await?;
    emit_server_update(&state, &update);
    ok(update)
}

async fn update_view(state: &AppState) -> ApiResult {
    ok(state.updates.view().await?)
}

fn emit_server_update(state: &AppState, update: &ServerUpdateView) {
    if let Ok(value) = serde_json::to_value(update) {
        let _ = state.hub.send(json!({
            "type": "server_update_changed",
            "data": value
        }));
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LlmSecretInput {
    api_key: String,
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LlmSecretResponse {
    configured: bool,
    revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bound_base_url: Option<String>,
}

async fn get_llm_secret(State(state): State<AppState>) -> Result<Response, ApiError> {
    let status = llm_secret_status(state.service.store(), &state.secret_vault).await?;
    // 历史上可能只有环境变量注入的密钥、没有持久化记录；
    // 这里保持改造前的语义：运行时有没有 key 也要算进 configured。
    let configured = status.configured || state.service.llm_api_key().await.is_some();
    let response = ok(LlmSecretResponse {
        configured,
        revision: status.revision,
        api_key: None,
        bound_base_url: status.bound_base_url,
    })?;
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        response,
    )
        .into_response())
}

async fn put_llm_secret(
    State(state): State<AppState>,
    Json(input): Json<LlmSecretInput>,
) -> ApiResult {
    let api_key = input.api_key.trim();
    if api_key.is_empty() || api_key.chars().count() > 4096 {
        return Err(ApiError::from(AppError::validation(
            "模型 API Key 长度必须在 1 到 4096 字符之间",
        )));
    }
    if api_key.chars().any(char::is_control) {
        return Err(ApiError::from(AppError::validation(
            "模型 API Key 不能包含控制字符",
        )));
    }
    let bound_base_url = match input
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(base_url) => {
            oshical_core::llm::validate_llm_base_url_target(base_url, "日程模型地址").await?;
            Some(oshical_core::llm::normalized_llm_base_url(base_url)?)
        }
        None => None,
    };
    let _guard = SERVER_SETTINGS_WRITE_LOCK.lock().await;
    let persisted = save_persisted_llm_secret(
        state.service.store(),
        &state.secret_vault,
        api_key,
        bound_base_url.as_deref(),
    )
    .await?;
    // 运行时单密钥槽：分析循环、探测与 URL 绑定都读它。
    state
        .service
        .update_bound_llm_api_key(api_key.to_owned(), persisted.bound_base_url.clone())
        .await?;
    // Re-emit the same revision when the value is unchanged. This makes a
    // retry repair a previous failure after the secret row was persisted but
    // its sync notification was not.
    let entity_id = "llm_api_key";
    state
        .service
        .store()
        .append_sync_change(
            "server_secret",
            entity_id,
            "upsert",
            persisted.revision,
            Some(json!({
                "id": entity_id,
                "configured": true
            })),
        )
        .await?;
    ok(LlmSecretResponse {
        configured: true,
        revision: persisted.revision,
        api_key: None,
        bound_base_url: persisted.bound_base_url,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct YoutubeApiKeyInput {
    api_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct YoutubeWebSessionInput {
    cookie: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct YoutubeSecretsView {
    api_key: YoutubeSecretStatus,
    oauth: YouTubeOAuthStatus,
    web_session: YoutubeSecretStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

async fn youtube_secrets_view(state: &AppState) -> Result<YoutubeSecretsView, ApiError> {
    let oauth = youtube_runtime()
        .oauth_status(state.service.store(), &state.secret_vault)
        .await?;
    let last_error = oauth.last_error.clone();
    Ok(YoutubeSecretsView {
        api_key: youtube_api_key_status(state.service.store(), &state.secret_vault).await?,
        oauth,
        web_session: youtube_web_session_status(
            state.service.store(),
            &state.secret_vault,
        )
        .await?,
        last_error,
    })
}

async fn get_youtube_secrets(State(state): State<AppState>) -> Result<Response, ApiError> {
    require_youtube_enabled(&state).await?;
    let response = ok(youtube_secrets_view(&state).await?)?;
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        response,
    )
        .into_response())
}

async fn put_youtube_api_key(
    State(state): State<AppState>,
    Json(input): Json<YoutubeApiKeyInput>,
) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let api_key = input.api_key.trim();
    if api_key.is_empty() || api_key.chars().count() > 4096 {
        return Err(ApiError::from(AppError::validation(
            "YouTube API Key 长度必须在 1 到 4096 字符之间",
        )));
    }
    if api_key.chars().any(char::is_control) {
        return Err(ApiError::from(AppError::validation(
            "YouTube API Key 不能包含控制字符",
        )));
    }
    let persisted = save_persisted_youtube_api_key(
        state.service.store(),
        &state.secret_vault,
        api_key,
    )
    .await?;
    if let Err(error) = sync_runtime_secrets_from_vault(&state).await {
        tracing::warn!(code = error.code(), "保存 YouTube API Key 后同步运行时凭据失败");
    }
    state
        .service
        .store()
        .append_sync_change(
            "server_secret",
            "youtube_api_key",
            "upsert",
            persisted.revision,
            Some(json!({"id": "youtube_api_key", "configured": true})),
        )
        .await?;
    ok(youtube_api_key_status(state.service.store(), &state.secret_vault).await?)
}

async fn delete_youtube_api_key(State(state): State<AppState>) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let previous_revision = match youtube_api_key_status(
        state.service.store(),
        &state.secret_vault,
    )
    .await
    {
        Ok(status) => status.revision,
        Err(error) => {
            tracing::warn!(code = error.code(), "读取损坏的 YouTube API Key 状态失败；仍允许清除");
            0
        }
    };
    let deleted = clear_persisted_youtube_api_key(state.service.store()).await?;
    if let Err(error) = sync_runtime_secrets_from_vault(&state).await {
        tracing::warn!(code = error.code(), "清除 YouTube API Key 后同步运行时凭据失败");
    }
    state
        .service
        .store()
        .append_sync_change(
            "server_secret",
            "youtube_api_key",
            "delete",
            previous_revision.saturating_add(1).max(1),
            Some(json!({"id": "youtube_api_key", "configured": false})),
        )
        .await?;
    ok(json!({"deleted": deleted, "configured": false}))
}

async fn start_youtube_oauth(State(state): State<AppState>) -> ApiResult {
    require_youtube_enabled(&state).await?;
    ok(youtube_runtime().start_oauth().await?)
}

#[derive(Deserialize)]
struct YoutubeOAuthCompleteInput {
    code: String,
    state: String,
}

async fn complete_youtube_oauth(
    State(state): State<AppState>,
    Json(input): Json<YoutubeOAuthCompleteInput>,
) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let code = input.code.trim();
    let oauth_state = input.state.trim();
    if code.is_empty() || oauth_state.is_empty() {
        return Err(ApiError::from(AppError::validation(
            "YouTube OAuth code 和 state 不能为空",
        )));
    }
    let result = youtube_runtime()
        .complete_oauth(
            state.service.store(),
            &state.secret_vault,
            code,
            oauth_state,
        )
        .await;
    match result {
        Ok(status) => {
            if let Err(sync_error) = sync_runtime_secrets_from_vault(&state).await {
                tracing::warn!(code = sync_error.code(), "YouTube OAuth 完成后同步运行时凭据失败");
            }
            ok(status)
        }
        Err(error) => {
            // Even a failed exchange can leave an expired or insufficiently
            // scoped token in the runtime; re-project the vault fail-closed.
            if let Err(sync_error) = sync_runtime_secrets_from_vault(&state).await {
                tracing::warn!(%sync_error, "YouTube OAuth 失败后同步运行时凭据失败");
            }
            Err(error.into())
        }
    }
}

async fn refresh_youtube_oauth(State(state): State<AppState>) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let result = youtube_runtime()
        .refresh_oauth(state.service.store(), &state.secret_vault)
        .await;
    match result {
        Ok(status) => {
            if let Err(sync_error) = sync_runtime_secrets_from_vault(&state).await {
                tracing::warn!(code = sync_error.code(), "YouTube OAuth 刷新后同步运行时凭据失败");
            }
            ok(status)
        }
        Err(error) => {
            if let Err(sync_error) = sync_runtime_secrets_from_vault(&state).await {
                tracing::warn!(%sync_error, "YouTube OAuth 刷新失败后同步运行时凭据失败");
            }
            Err(error.into())
        }
    }
}

async fn logout_youtube_oauth(State(state): State<AppState>) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let status = youtube_runtime()
        .logout(state.service.store(), &state.secret_vault)
        .await?;
    if let Err(error) = sync_runtime_secrets_from_vault(&state).await {
        tracing::warn!(code = error.code(), "YouTube OAuth 登出后同步运行时凭据失败");
    }
    ok(status)
}

async fn get_youtube_session(State(state): State<AppState>) -> ApiResult {
    require_youtube_enabled(&state).await?;
    ok(youtube_web_session_status(
        state.service.store(),
        &state.secret_vault,
    )
    .await?)
}

async fn put_youtube_session(
    State(state): State<AppState>,
    Json(input): Json<YoutubeWebSessionInput>,
) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let cookie = input.cookie.trim();
    if cookie.is_empty() || cookie.chars().count() > 16_384 {
        return Err(ApiError::from(AppError::validation(
            "YouTube 网页会话长度必须在 1 到 16384 字符之间",
        )));
    }
    if cookie
        .chars()
        .any(|value| value.is_control() && value != '\t')
    {
        return Err(ApiError::from(AppError::validation(
            "YouTube 网页会话不能包含控制字符",
        )));
    }
    let persisted = save_persisted_youtube_web_session(
        state.service.store(),
        &state.secret_vault,
        cookie,
    )
    .await?;
    if let Err(error) = sync_runtime_secrets_from_vault(&state).await {
        tracing::warn!(code = error.code(), "保存 YouTube 网页会话后同步运行时凭据失败");
    }
    state
        .service
        .store()
        .append_sync_change(
            "server_secret",
            "youtube_web_session",
            "upsert",
            persisted.revision,
            Some(json!({"id": "youtube_web_session", "configured": true})),
        )
        .await?;
    ok(youtube_web_session_status(
        state.service.store(),
        &state.secret_vault,
    )
    .await?)
}

async fn delete_youtube_session(State(state): State<AppState>) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let previous_revision = match youtube_web_session_status(
        state.service.store(),
        &state.secret_vault,
    )
    .await
    {
        Ok(status) => status.revision,
        Err(error) => {
            tracing::warn!(code = error.code(), "读取损坏的 YouTube 网页会话状态失败；仍允许清除");
            0
        }
    };
    let deleted = clear_persisted_youtube_web_session(state.service.store()).await?;
    if let Err(error) = sync_runtime_secrets_from_vault(&state).await {
        tracing::warn!(code = error.code(), "清除 YouTube 网页会话后同步运行时凭据失败");
    }
    state
        .service
        .store()
        .append_sync_change(
            "server_secret",
            "youtube_web_session",
            "delete",
            previous_revision.saturating_add(1).max(1),
            Some(json!({"id": "youtube_web_session", "configured": false})),
        )
        .await?;
    ok(json!({"deleted": deleted, "configured": false}))
}

async fn start_youtube_session(State(state): State<AppState>) -> ApiResult {
    require_youtube_enabled(&state).await?;
    ok(json!({
        "started": false,
        "mode": "manual",
        "loginUrl": "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F",
        "importEndpoint": "/api/youtube/session",
        "note": "服务端无法安全捕获浏览器 Cookie；请在受控浏览器登录后，将会话 Cookie 通过 importEndpoint 导入。"
    }))
}

async fn get_youtube_status(State(state): State<AppState>) -> ApiResult {
    require_youtube_enabled(&state).await?;
    let secrets = youtube_secrets_view(&state).await?;
    ok(json!({
        "enabled": true,
        "secrets": secrets
    }))
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

async fn events_after(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> ApiResult {
    let limit = query.limit.unwrap_or(500).clamp(1, 500);
    let events = state
        .service
        .store()
        .events_after(query.after.unwrap_or(0).max(0), limit)
        .await?;
    ok(events)
}

async fn sync_snapshot(State(state): State<AppState>) -> ApiResult {
    let bootstrap = state.service.store().sync_bootstrap().await?;
    let latest_event_seq = state.service.store().latest_event_seq().await?;
    ok(json!({
        "schemaVersion": oshical_core::store::SERVER_SNAPSHOT_SCHEMA_VERSION,
        "subscriptions": bootstrap.subscriptions,
        "schedules": bootstrap.schedules,
        "weeklyRules": bootstrap.weekly_rules,
        "weeklyExceptions": bootstrap.weekly_exceptions,
        "scheduleSources": bootstrap.schedule_sources,
        "sourceSuppressions": bootstrap.source_suppressions,
        "latestEventSeq": latest_event_seq,
        "exportedAt": Utc::now()
    }))
}

async fn sync_conflicts(State(state): State<AppState>) -> ApiResult {
    ok(state.service.store().list_blocked_mutations(500).await?)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationOutcome {
    mutation_id: String,
    status: String,
    error: Option<String>,
}

async fn apply_mutations(
    State(state): State<AppState>,
    Json(mutations): Json<Vec<SyncMutation>>,
) -> ApiResult {
    if mutations.len() > 200 {
        return Err(ApiError::from(AppError::validation(
            "一次最多提交 200 个同步操作",
        )));
    }
    let mut outcomes = Vec::new();
    for mutation in mutations {
        let outcome = apply_mutation(&state, &mutation).await;
        outcomes.push(MutationOutcome {
            mutation_id: mutation.mutation_id,
            status: outcome.0,
            error: outcome.1,
        });
    }
    ok(outcomes)
}

async fn apply_mutation(state: &AppState, mutation: &SyncMutation) -> (String, Option<String>) {
    let result: Result<(), AppError> = async {
        match (mutation.entity.as_str(), mutation.operation.as_str()) {
            ("schedule", _) => {
                state
                    .service
                    .store()
                    .apply_schedule_mutation(mutation)
                    .await?;
            }
            ("subscription", "update") => {
                state
                    .service
                    .store()
                    .apply_subscription_mutation(mutation)
                    .await?;
            }
            ("link", _) => {
                state.service.store().apply_link_mutation(mutation).await?;
            }
            _ => {
                return Err(AppError::validation(format!(
                    "不支持的同步操作: {}/{}",
                    mutation.entity, mutation.operation
                )));
            }
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => ("applied".to_owned(), None),
        Err(AppError::External { code, message }) if code == "SYNC_CONFLICT" => {
            ("conflict".to_owned(), Some(message))
        }
        Err(error) => ("failed".to_owned(), Some(error.to_string())),
    }
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>, ApiError> {
    Ok(DateTime::parse_from_rfc3339(value)
        .map_err(|error| ApiError::from(AppError::validation(format!("时间格式错误: {error}"))))?
        .with_timezone(&Utc))
}

#[derive(Debug, Deserialize)]
struct WsQuery {
    after: Option<i64>,
    protocol: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsAuthFrame {
    #[serde(rename = "type")]
    frame_type: String,
    token: Option<String>,
    device_id: Option<String>,
    protocol_version: Option<u32>,
}

fn websocket_origin_allowed(headers: &HeaderMap) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let origin = match (origins.next(), origins.next()) {
        (None, None) => return true,
        (Some(origin), None) => match origin.to_str() {
            Ok(origin) => origin,
            Err(_) => return false,
        },
        _ => return false,
    };
    ALLOWED_WEB_ORIGINS.contains(&origin) || websocket_origin_matches_loopback_host(origin, headers)
}

fn websocket_origin_matches_loopback_host(origin: &str, headers: &HeaderMap) -> bool {
    let Ok(origin) = origin.parse::<Uri>() else {
        return false;
    };
    if !matches!(origin.scheme_str(), Some("http") | Some("https")) {
        return false;
    }
    let Some(origin_authority) = origin.authority() else {
        return false;
    };
    let Some(request_host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(request_authority) = request_host.parse::<Authority>() else {
        return false;
    };
    if !origin_authority
        .host()
        .eq_ignore_ascii_case(request_authority.host())
        || origin_authority.port_u16() != request_authority.port_u16()
    {
        return false;
    }
    loopback_host(request_authority.host())
}

fn loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

async fn websocket(
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let after = query.after.unwrap_or(0).max(0);
    let protocol = query.protocol.unwrap_or(1);
    let origin_allowed = state.api_token.is_some() || websocket_origin_allowed(&headers);
    upgrade
        .on_upgrade(move |socket| handle_websocket(socket, state, after, protocol, origin_allowed))
        .into_response()
}

async fn handle_websocket(
    socket: WebSocket,
    state: AppState,
    after: i64,
    query_protocol: u32,
    origin_allowed: bool,
) {
    let mut socket = socket;
    if state.api_token.is_none() && !origin_allowed {
        reject_websocket(&mut socket, "REMOTE_UNAUTHORIZED").await;
        return;
    }
    let auth = tokio::time::timeout(Duration::from_secs(10), socket.recv()).await;
    let auth_frame = match auth {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<WsAuthFrame>(&text)
            .ok()
            .filter(|frame| frame.frame_type == "auth"),
        _ => None,
    };
    let Some(auth_frame) = auth_frame else {
        reject_websocket(&mut socket, "REMOTE_UNAUTHORIZED").await;
        return;
    };
    let authorized = match &state.api_token {
        Some(expected) => auth_frame
            .token
            .as_deref()
            .is_some_and(|provided| constant_time_eq(provided.as_bytes(), expected.as_bytes())),
        None => origin_allowed,
    };
    if !authorized {
        reject_websocket(&mut socket, "REMOTE_UNAUTHORIZED").await;
        return;
    }

    let requested_protocol = auth_frame.protocol_version.unwrap_or(query_protocol);
    if !matches!(requested_protocol, 1 | 2) {
        reject_websocket(&mut socket, "UNSUPPORTED_PROTOCOL").await;
        return;
    }
    let protocol_v2 = requested_protocol == 2;
    let device_id = auth_frame
        .device_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if protocol_v2 && device_id.is_none() {
        reject_websocket(&mut socket, "INVALID_DEVICE_ID").await;
        return;
    }
    if let Some(device_id) = device_id.as_deref() {
        // A successful authenticated socket counts as activity even when no new
        // changes arrive and therefore no ACK is sent.
        if let Err(error) = state.service.store().touch_device(device_id).await {
            tracing::warn!(%error, device_id, "WebSocket 设备活跃时间更新失败");
        }
    }

    // Subscribe before replay so a change committed during catch-up is queued.
    let mut receiver = state.hub.subscribe();
    let mut sync_cursor = after;
    if protocol_v2 {
        let feed_id = match state.service.store().sync_feed_id().await {
            Ok(feed_id) => feed_id,
            Err(error) => {
                tracing::warn!(%error, "WebSocket 鉴权读取同步源标识失败");
                reject_websocket(&mut socket, "STORE_UNAVAILABLE").await;
                return;
            }
        };
        let latest = match state.service.store().latest_sync_seq().await {
            Ok(seq) => seq,
            Err(error) => {
                tracing::warn!(%error, "WebSocket 鉴权读取同步游标失败");
                reject_websocket(&mut socket, "STORE_UNAVAILABLE").await;
                return;
            }
        };
        let min_retained = match state.service.store().min_retained_sync_seq().await {
            Ok(seq) => seq,
            Err(error) => {
                tracing::warn!(%error, "WebSocket 鉴权读取最小保留游标失败");
                reject_websocket(&mut socket, "STORE_UNAVAILABLE").await;
                return;
            }
        };
        let reset_required = sync_cursor < min_retained.saturating_sub(1) || sync_cursor > latest;
        let auth_ok = json!({
            "type": "auth_ok",
            "protocolVersion": 2,
            "deviceId": device_id,
            "feedId": &feed_id,
            "latestSeq": latest,
            "minRetainedSeq": min_retained,
            "resetRequired": reset_required,
        });
        if socket
            .send(Message::Text(auth_ok.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
        if reset_required {
            sync_cursor = latest;
            let reset =
                sync_api::reset_required_message(latest, min_retained, &feed_id, "cursor_expired");
            if socket
                .send(Message::Text(reset.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        } else if send_sync_after(&mut socket, state.service.store(), &mut sync_cursor)
            .await
            .is_err()
        {
            return;
        }
    } else {
        let latest = match state.service.store().latest_event_seq().await {
            Ok(seq) => seq,
            Err(error) => {
                tracing::warn!(%error, "WebSocket 鉴权读取事件游标失败");
                reject_websocket(&mut socket, "STORE_UNAVAILABLE").await;
                return;
            }
        };
        if socket
            .send(Message::Text(
                json!({"type": "auth_ok", "latestSeq": latest})
                    .to_string()
                    .into(),
            ))
            .await
            .is_err()
        {
            return;
        }
        let mut replay_cursor = sync_cursor;
        loop {
            let events = match state.service.store().events_after(replay_cursor, 500).await {
                Ok(events) => events,
                Err(error) => {
                    tracing::warn!(%error, "WebSocket V1 历史事件读取失败");
                    reject_websocket(&mut socket, "STORE_UNAVAILABLE").await;
                    return;
                }
            };
            let page_len = events.len();
            let mut advanced = false;
            for event in events {
                if let Some(seq) = event.get("seq").and_then(Value::as_i64) {
                    replay_cursor = replay_cursor.max(seq);
                    advanced = true;
                }
                if socket
                    .send(Message::Text(event.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if page_len < 500 || !advanced {
                break;
            }
        }
        sync_cursor = replay_cursor;
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sync_tick = tokio::time::interval(Duration::from_millis(500));
    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) if protocol_v2 => {
                        if let Some(seq) = parse_ws_ack(&text) {
                            if let Some(device_id) = device_id.as_deref() {
                                if let Err(error) = state.service.store().ack_device(device_id, seq).await {
                                    tracing::warn!(%error, device_id, seq, "WebSocket ACK 保存失败");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            event = receiver.recv() => {
                match event {
                    Ok(event) => {
                        if !forward_hub_message(protocol_v2, &event) {
                            continue;
                        }
                        if protocol_v2 {
                            match event.get("type").and_then(Value::as_str) {
                                Some("sync_batch") => {
                                    let from_seq = event.get("fromSeq").and_then(Value::as_i64).unwrap_or(0);
                                    let to_seq = event.get("toSeq").and_then(Value::as_i64).unwrap_or(0);
                                    if from_seq > sync_cursor.saturating_add(1) {
                                        if send_sync_after(&mut socket, state.service.store(), &mut sync_cursor).await.is_err() {
                                            break;
                                        }
                                        continue;
                                    }
                                    if to_seq <= sync_cursor {
                                        continue;
                                    }
                                    sync_cursor = sync_cursor.max(to_seq);
                                }
                                Some("reset_required") => {
                                    sync_cursor = event.get("latestSeq").and_then(Value::as_i64).unwrap_or(sync_cursor);
                                }
                                _ => {}
                            }
                        }
                        if socket.send(Message::Text(event.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        if protocol_v2 {
                            if send_sync_after(&mut socket, state.service.store(), &mut sync_cursor).await.is_err() {
                                break;
                            }
                        } else {
                            let latest_seq = match state.service.store().latest_event_seq().await {
                                Ok(seq) => seq,
                                Err(error) => {
                                    tracing::warn!(%error, "广播积压后读取事件游标失败");
                                    break;
                                }
                            };
                            let resync = json!({
                                "type": "resync_required",
                                "skipped": skipped,
                                "latestSeq": latest_seq
                            });
                            if socket.send(Message::Text(resync.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = sync_tick.tick(), if protocol_v2 => {
                if send_sync_after(&mut socket, state.service.store(), &mut sync_cursor).await.is_err() {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if let Some(device_id) = device_id.as_deref() {
                    if let Err(error) = state.service.store().touch_device(device_id).await {
                        tracing::warn!(%error, device_id, "WebSocket 心跳更新设备活跃时间失败");
                    }
                }
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn parse_ws_ack(text: &str) -> Option<i64> {
    let value: Value = serde_json::from_str(text).ok()?;
    (value.get("type").and_then(Value::as_str) == Some("ack"))
        .then(|| value.get("seq").and_then(Value::as_i64))
        .flatten()
        .filter(|seq| *seq >= 0)
}

fn forward_hub_message(protocol_v2: bool, value: &Value) -> bool {
    let message_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let sync_message = matches!(message_type, "sync_batch" | "reset_required");
    if protocol_v2 {
        // Live status transitions are edge notifications, not state snapshots.
        // V2 still receives state through sync batches, but needs this event to
        // distinguish a real offline -> live transition from a startup snapshot.
        sync_message
            || matches!(
                message_type,
                "live_status" | "job_progress" | "refresh_progress" | "data_imported"
            )
    } else {
        !sync_message
    }
}

async fn send_sync_after(
    socket: &mut WebSocket,
    store: &SqliteStore,
    after: &mut i64,
) -> Result<(), String> {
    loop {
        let page = store
            .sync_changes_after(*after, 500)
            .await
            .map_err(|error| error.to_string())?;
        if page.reset_required {
            let reset = sync_api::reset_required_message(
                page.latest_seq,
                page.min_retained_seq,
                &page.feed_id,
                "cursor_expired",
            );
            socket
                .send(Message::Text(reset.to_string().into()))
                .await
                .map_err(|error| error.to_string())?;
            *after = page.latest_seq;
            return Ok(());
        }
        if page.changes.is_empty() {
            *after = (*after).max(page.latest_seq);
            return Ok(());
        }

        let previous = *after;
        let batches = sync_api::sync_batch_messages(&page.changes, page.latest_seq, &page.feed_id)
            .map_err(|error| error.to_string())?;
        for batch in batches {
            socket
                .send(Message::Text(batch.to_string().into()))
                .await
                .map_err(|error| error.to_string())?;
        }
        *after = page
            .changes
            .last()
            .map(|change| change.seq)
            .unwrap_or(*after);
        if *after <= previous || !page.has_more || *after >= page.latest_seq {
            return Ok(());
        }
    }
}

async fn reject_websocket(socket: &mut WebSocket, code: &str) {
    let _ = socket
        .send(Message::Text(
            json!({"type": "error", "code": code}).to_string().into(),
        ))
        .await;
    let _ = socket.close().await;
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrationApplyInput {
    #[serde(default)]
    migration_id: String,
    client_id: String,
    strategy: String,
    #[serde(default)]
    expected_server_digest: String,
    snapshot: oshical_core::store::ServerSnapshot,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrationReceipt {
    strategy: String,
    snapshot_digest: String,
    report: Value,
    completed_at: DateTime<Utc>,
}

async fn migration_preview(
    State(state): State<AppState>,
    Json(snapshot): Json<oshical_core::store::ServerSnapshot>,
) -> ApiResult {
    let (preview, server_state_digest) =
        migration_preview_with_consistent_digest(state.service.store(), &snapshot).await?;
    let mut response = serde_json::to_value(orient_migration_preview_for_http(preview))?;
    if let Some(object) = response.as_object_mut() {
        object.insert("serverStateDigest".to_owned(), json!(server_state_digest));
    }
    ok(response)
}

async fn migration_preview_with_consistent_digest(
    store: &SqliteStore,
    snapshot: &oshical_core::store::ServerSnapshot,
) -> Result<(oshical_core::store::ImportPreview, String), ApiError> {
    for _ in 0..8 {
        let before = store.migration_state_digest().await?;
        let preview = store.preview_snapshot(snapshot).await?;
        let after = store.migration_state_digest().await?;
        if before == after {
            return Ok((preview, after));
        }
    }
    Err(AppError::External {
        code: "MIGRATION_PREVIEW_CONTENTION".to_owned(),
        message: "服务器数据持续变化，无法生成一致的迁移预览，请稍后重试".to_owned(),
    }
    .into())
}

fn orient_migration_preview_for_http(
    mut preview: oshical_core::store::ImportPreview,
) -> oshical_core::store::ImportPreview {
    // Store previews describe "incoming snapshot vs local store". During HTTP
    // migration the incoming snapshot is the client and the local store is the
    // server, so expose that orientation to the UI without changing store semantics.
    for conflict in &mut preview.conflicts {
        std::mem::swap(&mut conflict.local_id, &mut conflict.remote_id);
        std::mem::swap(&mut conflict.local, &mut conflict.remote);
    }
    preview
}

async fn migration_apply(
    State(state): State<AppState>,
    Extension(migration_lock): Extension<MigrationApplyLock>,
    Json(input): Json<MigrationApplyInput>,
) -> ApiResult {
    let migration_id = input.migration_id.trim();
    validate_migration_id(migration_id)?;
    let client_id = input.client_id.trim();
    if client_id.is_empty() || client_id.chars().count() > 128 {
        return Err(ApiError::from(AppError::validation("clientId 无效")));
    }
    let internal_strategy = migration_store_strategy(&input.strategy)?;
    if input.expected_server_digest.trim().is_empty() {
        return Err(ApiError::from(AppError::validation(
            "expectedServerDigest 不能为空",
        )));
    }

    let snapshot_digest = snapshot_digest(&input.snapshot)?;
    let receipt_key = migration_receipt_key(migration_id);
    let event_payload = json!({
        "clientId": client_id,
        "migrationId": migration_id,
        "strategy": &input.strategy,
    });
    let _guard = migration_lock.0.lock().await;
    let outcome = state
        .service
        .store()
        .import_snapshot_migration(
            &input.snapshot,
            internal_strategy,
            &format!("migration:{client_id}"),
            migration_id,
            &receipt_key,
            &input.strategy,
            &snapshot_digest,
            Some(input.expected_server_digest.as_str()),
            "data_imported",
            &event_payload,
        )
        .await?;

    match outcome {
        oshical_core::store::MigrationApplyOutcome::Existing(raw) => {
            let receipt: MigrationReceipt = serde_json::from_str(&raw)?;
            if receipt.strategy == input.strategy && receipt.snapshot_digest == snapshot_digest {
                ok(receipt.report)
            } else {
                Err(ApiError::migration_id_conflict(
                    "migrationId 已用于不同的迁移内容或策略",
                ))
            }
        }
        oshical_core::store::MigrationApplyOutcome::Applied(report) => {
            ok(serde_json::to_value(&report)?)
        }
    }
}

fn migration_store_strategy(strategy: &str) -> Result<&'static str, ApiError> {
    match strategy {
        // HTTP migration snapshots are supplied by the client. "Keep server"
        // therefore maps to the store strategy that preserves the local store.
        "server" => Ok("client"),
        // "Use client" means the supplied snapshot overwrites the server store.
        "client" => Ok("server"),
        "skip" => Ok("skip"),
        _ => Err(ApiError::from(AppError::validation("迁移策略无效"))),
    }
}

fn validate_migration_id(migration_id: &str) -> Result<(), ApiError> {
    if migration_id.is_empty()
        || migration_id.len() > 128
        || !migration_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'.'))
    {
        return Err(ApiError::from(AppError::validation(
            "migrationId 必须为 1 到 128 位字母、数字、下划线、连字符、冒号或句点",
        )));
    }
    Ok(())
}

fn migration_receipt_key(migration_id: &str) -> String {
    // The public settings API only accepts fixed AppSettings keys. Reuse the existing
    // persistent key-value table so receipts survive restarts without changing core schema.
    format!("server:migration_receipt:v1:{migration_id}")
}

fn snapshot_digest(snapshot: &oshical_core::store::ServerSnapshot) -> Result<String, ApiError> {
    let mut value = serde_json::to_value(snapshot)?;
    canonicalize_json(&mut value);
    let bytes = serde_json::to_vec(&value)?;
    Ok(format!("sha256:{}", sha256_hex(&bytes)))
}

fn canonicalize_json(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                canonicalize_json(value);
            }
        }
        Value::Object(map) => {
            let mut entries = std::mem::take(map)
                .into_iter()
                .map(|(key, mut value)| {
                    canonicalize_json(&mut value);
                    (key, value)
                })
                .collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            for (key, value) in entries {
                map.insert(key, value);
            }
        }
        _ => {}
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUNDS: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((bytes.len() + 72) & !63);
    padded.extend_from_slice(bytes);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for chunk in padded.as_chunks::<64>().0 {
        let mut words = [0_u32; 64];
        for (index, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(ROUNDS[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut digest = [0_u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod protocol_tests {
    #[test]
    fn shared_refresh_errors_keep_the_original_http_classification() {
        use axum::http::StatusCode;
        use oshical_core::AppError;
        for (code, status) in [
            ("VALIDATION_ERROR", StatusCode::BAD_REQUEST),
            ("NOT_FOUND", StatusCode::NOT_FOUND),
            ("DATABASE_ERROR", StatusCode::INTERNAL_SERVER_ERROR),
            ("NETWORK_ERROR", StatusCode::BAD_GATEWAY),
            ("NOT_CONFIGURED", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let error = super::ApiError::from(AppError::Shared {
                code,
                message: "shared fixture".to_owned(),
            });
            assert_eq!(error.status, status);
            assert_eq!(error.code, code);
        }
    }

    use super::*;

    #[test]
    fn llm_secret_input_keeps_legacy_payload_compatible() {
        let legacy: LlmSecretInput =
            serde_json::from_value(json!({"apiKey": "sk-legacy"})).unwrap();
        assert_eq!(legacy.api_key, "sk-legacy");
        assert!(legacy.base_url.is_none());

        let bound: LlmSecretInput = serde_json::from_value(json!({
            "apiKey": "sk-bound",
            "baseUrl": "https://api.example.com/v1"
        }))
        .unwrap();
        assert_eq!(
            bound.base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
    }

    #[test]
    fn secret_url_binding_supports_legacy_key_first_and_exact_binding() {
        assert!(llm_secret_url_binding_allows(
            None,
            true,
            "https://api.example.com/v2"
        ));
        assert!(llm_secret_url_binding_allows(
            Some("https://api.example.com/v2"),
            false,
            "https://api.example.com/v2"
        ));
        assert!(!llm_secret_url_binding_allows(
            Some("https://api.example.com/v1"),
            false,
            "https://api.example.com/v2"
        ));
        assert!(!llm_secret_url_binding_allows(
            None,
            false,
            "https://api.example.com/v2"
        ));
    }
    #[test]
    fn unchanged_settings_produce_no_revision_write() {
        let current = AppSettings::default();
        assert!(changed_setting_values(&current, &current)
            .unwrap()
            .is_empty());
        let mut next = current.clone();
        next.live_poll_seconds += 1;
        let values = changed_setting_values(&current, &next).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0, "live_poll_seconds");
    }
    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn tokenless_http_writes_reject_cross_site_browser_requests() {
        let mut headers = HeaderMap::new();
        assert!(unsafe_http_origin_allowed(&headers));
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!unsafe_http_origin_allowed(&headers));
        headers.remove("sec-fetch-site");
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8787"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8787"),
        );
        assert!(unsafe_http_origin_allowed(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(!unsafe_http_origin_allowed(&headers));
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_and_comparison_handles_lengths() {
        assert_eq!(bearer_token("Bearer secret"), Some("secret"));
        assert_eq!(bearer_token("bearer secret"), Some("secret"));
        assert_eq!(bearer_token("BEARER secret"), Some("secret"));
        assert_eq!(bearer_token("Basic secret"), None);
        assert_eq!(bearer_token("Bearer secret extra"), None);
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[test]
    fn websocket_origin_policy_allows_known_and_loopback_same_origin_only() {
        let mut headers = HeaderMap::new();
        assert!(websocket_origin_allowed(&headers));

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://tauri.localhost"),
        );
        assert!(websocket_origin_allowed(&headers));

        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8787"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8787"),
        );
        assert!(websocket_origin_allowed(&headers));

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(!websocket_origin_allowed(&headers));
    }

    #[test]
    fn migration_preview_reports_client_and_server_in_ui_order() {
        let preview = oshical_core::store::ImportPreview {
            subscriptions: 1,
            schedules: 1,
            weekly_rules: 0,
            conflicts: vec![oshical_core::store::ImportConflict {
                entity: "schedule".to_owned(),
                label: "冲突".to_owned(),
                local_id: 42,
                remote_id: 7,
                local: serde_json::json!({"source": "server"}),
                remote: serde_json::json!({"source": "client"}),
            }],
        };
        let preview = orient_migration_preview_for_http(preview);
        let conflict = &preview.conflicts[0];
        assert_eq!(conflict.local_id, 7);
        assert_eq!(conflict.remote_id, 42);
        assert_eq!(conflict.local["source"], "client");
        assert_eq!(conflict.remote["source"], "server");
    }

    #[test]
    fn stale_migration_preview_maps_to_conflict_and_specific_error_code() {
        let error = ApiError::from(AppError::External {
            code: "MIGRATION_STALE_PREVIEW".to_owned(),
            message: "stale".to_owned(),
        });
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.code, "MIGRATION_STALE_PREVIEW");
    }

    #[test]
    fn migration_http_strategy_is_mapped_to_store_direction() {
        assert_eq!(migration_store_strategy("server").unwrap(), "client");
        assert_eq!(migration_store_strategy("client").unwrap(), "server");
        assert_eq!(migration_store_strategy("skip").unwrap(), "skip");
        assert!(migration_store_strategy("unknown").is_err());
    }

    #[test]
    fn migration_id_validation_is_strict() {
        assert!(validate_migration_id("device-a:2026.10.29_1").is_ok());
        assert!(validate_migration_id("").is_err());
        assert!(validate_migration_id("contains space").is_err());
        assert!(validate_migration_id("路径").is_err());
        assert!(validate_migration_id(&"a".repeat(129)).is_err());
        assert_eq!(
            migration_receipt_key("device-a:1"),
            "server:migration_receipt:v1:device-a:1"
        );
    }

    #[test]
    fn live_status_is_visible_to_both_websocket_protocols() {
        let message = json!({
            "seq": 7,
            "type": "live_status",
            "payload": {
                "previousIsLive": false,
                "isLive": true
            }
        });
        assert!(forward_hub_message(false, &message));
        assert!(forward_hub_message(true, &message));
    }

    #[test]
    fn youtube_subscription_defaults_do_not_enable_live_stats() {
        let mut payload = json!({});
        apply_youtube_subscription_defaults(payload.as_object_mut().unwrap());
        assert_eq!(payload["scheduleEnabled"], true);
        assert_eq!(payload["liveNotifyEnabled"], true);
        assert_eq!(payload["liveStatsEnabled"], false);
    }

    #[test]
    fn youtube_error_prefix_keeps_exact_code_and_specific_status() {
        let cases = [
            ("YOUTUBE_API_KEY_INVALID", StatusCode::BAD_REQUEST),
            ("YOUTUBE_BAD_REQUEST", StatusCode::BAD_REQUEST),
            ("YOUTUBE_COMMUNITY_FORMAT", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_FEATURE_DISABLED", StatusCode::FORBIDDEN),
            ("YOUTUBE_FORBIDDEN", StatusCode::FORBIDDEN),
            ("YOUTUBE_INVALID_INPUT", StatusCode::BAD_REQUEST),
            ("YOUTUBE_LIVE_FORMAT", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_NETWORK", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_NON_JSON", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_NOT_CONFIGURED", StatusCode::SERVICE_UNAVAILABLE),
            ("YOUTUBE_NOT_FOUND", StatusCode::NOT_FOUND),
            ("YOUTUBE_OAUTH_ACCESS_DENIED", StatusCode::FORBIDDEN),
            ("YOUTUBE_OAUTH_BAD_RESPONSE", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_OAUTH_BUSY", StatusCode::TOO_MANY_REQUESTS),
            ("YOUTUBE_OAUTH_EXCHANGE_FAILED", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_OAUTH_REFRESH_TOKEN_MISSING", StatusCode::BAD_REQUEST),
            ("YOUTUBE_OAUTH_STATE_INVALID", StatusCode::BAD_REQUEST),
            ("YOUTUBE_QUOTA_EXCEEDED", StatusCode::TOO_MANY_REQUESTS),
            ("YOUTUBE_RATE_LIMITED", StatusCode::TOO_MANY_REQUESTS),
            ("YOUTUBE_RESPONSE_SIZE", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_STORAGE_ERROR", StatusCode::INTERNAL_SERVER_ERROR),
            ("YOUTUBE_SUBSCRIBER_COUNT_MISSING", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_UNAUTHORIZED", StatusCode::UNAUTHORIZED),
            ("YOUTUBE_UPSTREAM_ERROR", StatusCode::BAD_GATEWAY),
            ("YOUTUBE_HTTP_404", StatusCode::NOT_FOUND),
            ("YOUTUBE_HTTP_429", StatusCode::TOO_MANY_REQUESTS),
        ];
        for (code, expected_status) in cases {
            let error = ApiError::from(AppError::External {
                code: code.to_owned(),
                message: "fixture".to_owned(),
            });
            assert_eq!(error.code, code);
            assert_eq!(error.status, expected_status);
        }
    }

    #[test]
    fn job_progress_is_visible_to_both_websocket_protocols() {
        let message = json!({
            "type": "job_progress",
            "jobId": "job-1",
            "kind": "refresh_all"
        });
        assert!(forward_hub_message(false, &message));
        assert!(forward_hub_message(true, &message));
    }
}
