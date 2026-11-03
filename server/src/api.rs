use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::bili_login::BiliQrManager;
use crate::jobs::{
    job_progress_message, RefreshJob, RefreshJobAccepted, RefreshJobManager, RefreshJobProgress,
};
use crate::secrets::{load_persisted_llm_secret, save_persisted_llm_secret, SecretVault};
use crate::sync_api;
use crate::update_manager::{ServerUpdateManager, ServerUpdateSettings, ServerUpdateView};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::SinkExt;
use oshical_core::{
    load_settings, save_settings, AppError, AppService, AppSettings, LlmConfig, LlmDiscoveryConfig,
    NewSchedule, NewWeeklyScheduleRule, ProxyTestInput, SqliteStore, SyncMutation, UpdateSchedule,
    UpdateSubscription, UpdateWeeklyScheduleRule,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<AppService>,
    pub api_token: Option<Arc<String>>,
    pub secret_vault: SecretVault,
    pub hub: broadcast::Sender<Value>,
    pub jobs: RefreshJobManager,
    pub updates: ServerUpdateManager,
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
            AppError::Validation(_) | AppError::Url(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::NotConfigured(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Database(_) | AppError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_GATEWAY,
        };
        Self {
            code: value.code().to_owned(),
            message: value.to_string(),
            status,
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(value: serde_json::Error) -> Self {
        AppError::Json(value).into()
    }
}

impl ApiError {
    fn migration_id_conflict(message: impl Into<String>) -> Self {
        Self {
            code: "MIGRATION_ID_CONFLICT".to_owned(),
            message: message.into(),
            status: StatusCode::BAD_REQUEST,
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
        .allow_origin(AllowOrigin::list([
            HeaderValue::from_static("http://localhost:1420"),
            HeaderValue::from_static("http://127.0.0.1:1420"),
            HeaderValue::from_static("http://tauri.localhost"),
            HeaderValue::from_static("https://tauri.localhost"),
            HeaderValue::from_static("tauri://localhost"),
        ]))
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
        .merge(protected)
        .fallback_service(ServeDir::new(dist).not_found_service(ServeFile::new(index)))
        .layer(Extension(bili_qr_manager))
        .layer(Extension(migration_lock))
        .layer(TraceLayer::new_for_http())
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
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return ApiError::from(AppError::Unauthorized).into_response();
    }
    next.run(request).await
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
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
}

async fn search_accounts(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> ApiResult {
    let results = state.service.search_accounts(&query.q).await?;
    ok(results)
}

async fn list_subscriptions(State(state): State<AppState>) -> ApiResult {
    ok(state.service.list_subscriptions().await?)
}

async fn add_subscription(
    State(state): State<AppState>,
    Json(input): Json<oshical_core::models::AddSubscription>,
) -> ApiResult {
    ok(state.service.add_subscription(input).await?)
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
}

async fn add_linked_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<AddLinkInput>,
) -> ApiResult {
    ok(state.service.add_linked_account(id, &input.mid).await?)
}

async fn remove_linked_account(
    State(state): State<AppState>,
    Path((id, mid)): Path<(i64, String)>,
) -> ApiResult {
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
    let registration = state.jobs.create_all();
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
            match background.service.refresh_all().await {
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
    let registration = state
        .jobs
        .create_streamer(streamer.id, streamer.name.clone());
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
                .refresh_streamer_with_progress(id, move |progress| {
                    if let Some(job) =
                        progress_registry.update_streamer_progress(&progress_job_id, &progress)
                    {
                        let _ = progress_hub.send(job_progress_message(&job));
                    }
                })
                .await;
            match result {
                Ok(report) => {
                    let stage = if report.failed_streamers.is_empty() {
                        "主播刷新完成"
                    } else {
                        "主播刷新完成，部分来源失败"
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

async fn test_llm(State(state): State<AppState>, Json(config): Json<LlmConfig>) -> ApiResult {
    ok(state.service.test_llm(config).await?)
}

async fn test_proxy(State(state): State<AppState>, Json(input): Json<ProxyTestInput>) -> ApiResult {
    ok(state.service.test_proxy(input.proxy_port).await?)
}

async fn list_llm_models(
    State(state): State<AppState>,
    Json(config): Json<LlmDiscoveryConfig>,
) -> ApiResult {
    ok(state.service.list_llm_models(config).await?)
}

async fn list_live_status(State(state): State<AppState>) -> ApiResult {
    ok(state.service.list_live_statuses().await?)
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

async fn get_settings(State(state): State<AppState>) -> ApiResult {
    ok(load_settings(state.service.store()).await?)
}

async fn put_settings(
    State(state): State<AppState>,
    Json(settings): Json<AppSettings>,
) -> ApiResult {
    save_settings(state.service.store(), &settings).await?;
    state.service.update_proxy_port(settings.proxy_port).await?;
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LlmSecretResponse {
    configured: bool,
    revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

async fn get_llm_secret(State(state): State<AppState>) -> Result<Response, ApiError> {
    let response = if let Some(persisted) =
        load_persisted_llm_secret(state.service.store(), &state.secret_vault).await?
    {
        ok(LlmSecretResponse {
            configured: true,
            revision: persisted.revision,
            api_key: Some(persisted.api_key),
        })?
    } else {
        let api_key = state.service.llm_api_key().await;
        ok(LlmSecretResponse {
            configured: api_key.is_some(),
            revision: 0,
            api_key,
        })?
    };
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
    let persisted =
        save_persisted_llm_secret(state.service.store(), &state.secret_vault, api_key).await?;
    state.service.update_llm_api_key(api_key.to_owned()).await?;
    state
        .service
        .store()
        .append_sync_change(
            "server_secret",
            "llm_api_key",
            "upsert",
            persisted.revision,
            Some(json!({"id": "llm_api_key", "configured": true})),
        )
        .await?;
    ok(LlmSecretResponse {
        configured: true,
        revision: persisted.revision,
        api_key: None,
    })
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    after: Option<i64>,
}

async fn events_after(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> ApiResult {
    let events = state
        .service
        .store()
        .events_after(query.after.unwrap_or(0).max(0), 500)
        .await?;
    ok(events)
}

async fn sync_snapshot(State(state): State<AppState>) -> ApiResult {
    let subscriptions = state.service.list_subscriptions().await?;
    let schedules = state.service.store().list_all_schedules().await?;
    let weekly_rules = state.service.list_weekly_rules(None).await?;
    let latest_event_seq = state.service.store().latest_event_seq().await?;
    ok(json!({
        "subscriptions": subscriptions,
        "schedules": schedules,
        "weeklyRules": weekly_rules,
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
                let payload = mutation.payload.as_object().ok_or_else(|| {
                    AppError::validation("subscription update payload 必须是对象")
                })?;
                let id = payload
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| AppError::validation("缺少订阅 id"))?;
                if let Some(base_revision) = mutation.base_revision {
                    let current = state.service.store().get_streamer(id).await?;
                    if current.revision != base_revision {
                        return Err(AppError::External {
                            code: "SYNC_CONFLICT".to_owned(),
                            message: format!(
                                "订阅已变化：期望 revision {base_revision}，当前 {}",
                                current.revision
                            ),
                        });
                    }
                }
                let update: UpdateSubscription = serde_json::from_value(
                    payload
                        .get("update")
                        .cloned()
                        .ok_or_else(|| AppError::validation("缺少 update"))?,
                )?;
                state.service.update_subscription(id, &update).await?;
            }
            ("link", "create") => {
                let streamer_id = mutation
                    .payload
                    .get("streamerId")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| AppError::validation("缺少 streamerId"))?;
                let mid = mutation
                    .payload
                    .get("mid")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::validation("缺少官号 mid"))?;
                state.service.add_linked_account(streamer_id, mid).await?;
            }
            ("link", "delete") => {
                let streamer_id = mutation
                    .payload
                    .get("streamerId")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| AppError::validation("缺少 streamerId"))?;
                let mid = mutation
                    .payload
                    .get("mid")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::validation("缺少官号 mid"))?;
                state
                    .service
                    .store()
                    .remove_linked_account(streamer_id, mid)
                    .await?;
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

async fn websocket(
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let after = query.after.unwrap_or(0).max(0);
    let protocol = query.protocol.unwrap_or(1);
    upgrade
        .on_upgrade(move |socket| handle_websocket(socket, state, after, protocol))
        .into_response()
}

async fn handle_websocket(socket: WebSocket, state: AppState, after: i64, query_protocol: u32) {
    let mut socket = socket;
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
        None => true,
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

    // Subscribe before replay so a change committed during catch-up is queued.
    let mut receiver = state.hub.subscribe();
    let mut sync_cursor = after;
    if protocol_v2 {
        let latest = state.service.store().latest_sync_seq().await.unwrap_or(0);
        let min_retained = state
            .service
            .store()
            .min_retained_sync_seq()
            .await
            .unwrap_or(0);
        let reset_required = sync_cursor < min_retained.saturating_sub(1);
        let auth_ok = json!({
            "type": "auth_ok",
            "protocolVersion": 2,
            "deviceId": device_id,
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
            let reset = sync_api::reset_required_message(latest, min_retained, "cursor_expired");
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
        let latest = state.service.store().latest_event_seq().await.unwrap_or(0);
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
        if let Ok(events) = state.service.store().events_after(sync_cursor, 500).await {
            for event in events {
                if socket
                    .send(Message::Text(event.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
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
                            let resync = json!({
                                "type": "resync_required",
                                "skipped": skipped,
                                "latestSeq": state.service.store().latest_event_seq().await.unwrap_or(0)
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
        sync_message
            || matches!(
                message_type,
                "job_progress" | "refresh_progress" | "data_imported"
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
        let batches = sync_api::sync_batch_messages(&page.changes, page.latest_seq)
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
    ok(state.service.store().preview_snapshot(&snapshot).await?)
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
    if !matches!(input.strategy.as_str(), "server" | "client" | "skip") {
        return Err(ApiError::from(AppError::validation("迁移策略无效")));
    }

    let snapshot_digest = snapshot_digest(&input.snapshot)?;
    let receipt_key = migration_receipt_key(migration_id);
    let _guard = migration_lock.0.lock().await;
    if let Some(receipt) = load_migration_receipt(state.service.store(), &receipt_key).await? {
        if receipt.strategy == input.strategy && receipt.snapshot_digest == snapshot_digest {
            return ok(receipt.report);
        }
        return Err(ApiError::migration_id_conflict(
            "migrationId 已用于不同的迁移内容或策略",
        ));
    }

    let report = state
        .service
        .store()
        .import_snapshot(
            &input.snapshot,
            &input.strategy,
            &format!("migration:{client_id}"),
        )
        .await?;
    state
        .service
        .store()
        .append_sync_change(
            "sync_reset",
            migration_id,
            "upsert",
            1,
            Some(json!({"id": migration_id, "reason": "data_imported"})),
        )
        .await?;
    state
        .service
        .store()
        .append_event(
            "data_imported",
            &json!({
                "clientId": client_id,
                "migrationId": migration_id,
                "strategy": input.strategy,
                "subscriptions": report.subscriptions,
                "schedules": report.schedules,
                "weeklyRules": report.weekly_rules,
                "skipped": report.skipped,
            }),
        )
        .await?;
    let report_value = serde_json::to_value(&report)?;
    let receipt = MigrationReceipt {
        strategy: input.strategy,
        snapshot_digest,
        report: report_value.clone(),
        completed_at: Utc::now(),
    };
    state
        .service
        .store()
        .set_client_sync_state(&receipt_key, &serde_json::to_string(&receipt)?)
        .await?;
    ok(report_value)
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

async fn load_migration_receipt(
    store: &SqliteStore,
    key: &str,
) -> Result<Option<MigrationReceipt>, ApiError> {
    let Some(raw) = store.get_client_sync_state(key).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&raw)?))
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
    use super::*;

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
