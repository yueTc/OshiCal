use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use chrono::{Duration as ChronoDuration, Utc};
use oshical_core::{
    load_settings, AppService, SqliteStore, SyncPrunePolicy, FAN_POLL_INTERVAL_SECONDS,
    FERTILITY_POLL_INTERVAL_SECONDS,
};
use tokio::sync::broadcast;

use crate::api::{build_router, persist_changed_settings, sync_runtime_secrets_from_vault, AppState};
use crate::config::ServerConfig;
use crate::jobs::RefreshJobManager;
use crate::secrets::{
    load_persisted_bili_secret, load_persisted_llm_secret, load_persisted_youtube_api_key,
    load_persisted_youtube_oauth, load_persisted_youtube_web_session, seed_llm_secret_from_env,
    seed_youtube_web_session_from_env, SecretVault,
};
use crate::sync_api;
use crate::update_manager::ServerUpdateManager;
use crate::youtube_oauth::{
    init_youtube_runtime, runtime_youtube_access_token, youtube_feature_enabled, youtube_runtime,
};

// Retention bounds are checked hourly, so growth between checks is expected.
const SYNC_RETENTION_DAYS: i64 = 7;
const SYNC_RETENTION_ROWS: i64 = 10_000;
const SYNC_RETENTION_PAYLOAD_BYTES: i64 = 20 * 1024 * 1024;
const SYNC_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub async fn run_server(config: ServerConfig) -> Result<(), String> {
    let youtube_config = config.youtube.clone();
    let store = SqliteStore::connect(&config.database_path)
        .await
        .map_err(|error| format!("打开数据库失败: {error}"))?;
    let persisted_settings = load_settings(&store)
        .await
        .map_err(|error| format!("读取服务端设置失败: {error}"))?;
    let mut settings = persisted_settings.clone();
    if let Some(value) = &config.llm_base_url {
        settings.llm_base_url = value.clone();
    }
    if let Some(value) = &config.llm_model {
        settings.llm_model = value.clone();
    }
    if let Some(value) = config.live_poll_seconds {
        settings.live_poll_seconds = value;
    }
    if let Some(value) = config.dynamic_poll_seconds {
        settings.dynamic_poll_seconds = value;
    }
    if let Some(value) = config.proxy_port {
        settings.proxy_port = Some(value);
    }
    settings.llm_reasoning_effort =
        config.effective_llm_reasoning_effort(settings.llm_reasoning_effort);
    settings.llm_parallelism = config.effective_llm_parallelism(settings.llm_parallelism);
    persist_changed_settings(&store, &persisted_settings, &settings)
        .await
        .map_err(|error| format!("保存服务端设置失败: {error}"))?;
    let secret_vault = SecretVault::from_env(config.api_token.as_deref(), &config.database_path);
    // YouTube API Keys are user-scoped credentials. Never seed or inherit the
    // server's API Key from process configuration; only SecretVault may provide it.
    match seed_youtube_web_session_from_env(
        &store,
        &secret_vault,
        youtube_config.web_session_cookie.as_deref(),
    )
    .await
    {
        Ok(true) => tracing::info!("已从环境变量写入 YouTube 网页会话"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "写入环境变量 YouTube 网页会话失败"),
    }
    init_youtube_runtime(youtube_config)?;
    // YouTube 凭据只能来自 SecretVault；这里统一解密一次，确认已保存的配置
    // 可在启动后按需使用，同时只记录是否配置，不输出任何明文。
    let persisted_youtube_api_key =
        match load_persisted_youtube_api_key(&store, &secret_vault).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "读取服务端 YouTube API Key 失败");
                None
            }
        };
    let mut persisted_youtube_oauth =
        match load_persisted_youtube_oauth(&store, &secret_vault).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "读取服务端 YouTube OAuth 凭据失败");
                None
            }
        };
    if settings.youtube_enabled && persisted_youtube_oauth.is_some() {
        if let Err(error) = youtube_runtime()
            .refresh_oauth_if_needed(&store, &secret_vault)
            .await
        {
            tracing::warn!(code = error.code, "启动时刷新 YouTube OAuth 凭据失败");
        }
        // Reload even after an error: invalid_grant removes the dead grant and
        // must not leave the pre-refresh token in the runtime snapshot.
        persisted_youtube_oauth =
            match load_persisted_youtube_oauth(&store, &secret_vault).await {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(%error, "刷新后重新读取 YouTube OAuth 凭据失败");
                    None
                }
            };
    }
    let persisted_youtube_web_session =
        match load_persisted_youtube_web_session(&store, &secret_vault).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "读取服务端 YouTube 网页会话失败");
                None
            }
        };
    let youtube_api_key_configured = persisted_youtube_api_key.is_some();
    let youtube_oauth_configured = persisted_youtube_oauth
        .as_ref()
        .is_some_and(|persisted| !persisted.token.refresh_token.trim().is_empty());
    let youtube_web_session_configured = persisted_youtube_web_session.is_some();
    tracing::info!(
        api_key = youtube_api_key_configured,
        oauth = youtube_oauth_configured,
        web_session = youtube_web_session_configured,
        "YouTube 凭据状态已从 SecretVault 加载"
    );
    // 先把环境变量里的密钥写进 vault，再搬走 config.secrets。
    match seed_llm_secret_from_env(
        &store,
        &secret_vault,
        config.secrets.llm_api_key.as_deref(),
        config.llm_base_url.as_deref(),
    )
    .await
    {
        Ok(true) => tracing::info!("已从环境变量写入模型密钥"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "写入环境变量模型密钥失败"),
    }
    let mut runtime_secrets = config.secrets;
    // Enforce the user-configured boundary even for programmatically built configs.
    runtime_secrets.youtube_api_key = None;
    if let Some(persisted) = persisted_youtube_api_key {
        runtime_secrets.youtube_api_key = Some(persisted.api_key);
    }
    if let Some(persisted) = persisted_youtube_oauth {
        if let Some(access_token) = runtime_youtube_access_token(&persisted.token) {
            runtime_secrets.youtube_oauth_access_token = Some(access_token);
        }
    }
    if let Some(persisted) = persisted_youtube_web_session {
        runtime_secrets.youtube_web_cookie = Some(persisted.cookie);
    }
    let mut persisted_llm_binding = None;
    if let Some(persisted) = load_persisted_llm_secret(&store, &secret_vault)
        .await
        .map_err(|error| format!("读取服务端模型密钥失败: {error}"))?
    {
        let base_url = persisted
            .bound_base_url
            .or_else(|| (!persisted.requires_url_binding).then(|| settings.llm_base_url.clone()));
        runtime_secrets.llm_api_key = Some(persisted.api_key.clone());
        persisted_llm_binding = Some((persisted.api_key, base_url));
    }
    if let Some(persisted) = load_persisted_bili_secret(&store, &secret_vault)
        .await
        .map_err(|error| format!("读取服务端 B 站登录失败: {error}"))?
    {
        runtime_secrets.bili_cookie = Some(persisted.cookie);
    }
    let service = AppService::new_with_proxy(store, runtime_secrets, settings.proxy_port)
        .map_err(|error| format!("初始化服务失败: {error}"))?;
    if let Some((api_key, base_url)) = persisted_llm_binding {
        service
            .update_bound_llm_api_key(api_key, base_url)
            .await
            .map_err(|error| format!("恢复模型密钥绑定失败: {error}"))?;
    }
    let (hub, _) = broadcast::channel(256);
    let updates = ServerUpdateManager::new(config.update_dir.clone())
        .map_err(|error| format!("初始化服务器更新管理器失败: {error}"))?;
    let state = AppState {
        service: Arc::new(service),
        api_token: config.api_token.map(Arc::new),
        secret_vault,
        hub,
        jobs: RefreshJobManager::new(),
        updates,
    };
    spawn_background_tasks(state.clone());
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|error| format!("监听 {} 失败: {error}", config.bind))?;
    tracing::info!(address = %config.bind, "推历 OshiCal 服务器已启动");
    axum::serve(listener, router)
        .await
        .map_err(|error| format!("服务器运行失败: {error}"))
}

pub fn spawn_background_tasks(state: AppState) {
    // 历史数据把直播预约卡的 `rid`（预约 id）当成房间号写进 room_url，
    // 客户端双击卡片会打开别人的直播间。启动时先清一次这类地址。
    let repair_state = state.clone();
    tokio::spawn(async move {
        repair_state.service.repair_reserve_room_urls().await;
    });
    // Keep slow vision/dynamic work independent of live probes and event delivery.
    let live_state = state.clone();
    tokio::spawn(async move {
        let config_service = live_state.service.clone();
        run_poll_loop(
            move || {
                let service = config_service.clone();
                async move {
                    load_settings(service.store())
                        .await
                        .map(|settings| Duration::from_secs(settings.live_poll_seconds))
                        .unwrap_or(Duration::from_secs(20))
                }
            },
            move |first| {
                let service = live_state.service.clone();
                async move {
                    let result = if first {
                        service.prime_live_statuses().await
                    } else {
                        service.check_live_all().await
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error, "开播检测失败");
                    }
                }
            },
        )
        .await;
    });
    let dynamic_state = state.clone();
    tokio::spawn(async move {
        let config_service = dynamic_state.service.clone();
        run_poll_loop(
            move || {
                let service = config_service.clone();
                async move {
                    load_settings(service.store())
                        .await
                        .map(|settings| Duration::from_secs(settings.dynamic_poll_seconds))
                        .unwrap_or(Duration::from_secs(300))
                }
            },
            move |_| {
                let service = dynamic_state.service.clone();
                async move {
                    if let Err(error) = service.refresh_all().await {
                        tracing::warn!(%error, "动态刷新失败");
                    }
                }
            },
        )
        .await;
    });

    let fan_state = state.clone();
    tokio::spawn(async move {
        run_poll_loop(
            || async { Duration::from_secs(FAN_POLL_INTERVAL_SECONDS) },
            move |_| {
                let service = fan_state.service.clone();
                async move {
                    match service.refresh_all_fans().await {
                        Ok(report) if report.failed_streamers.is_empty() => {
                            tracing::info!(
                                refreshed = report.refreshed_streamers,
                                "粉丝数采集完成"
                            );
                        }
                        Ok(report) => tracing::warn!(
                            refreshed = report.refreshed_streamers,
                            failed = report.failed_streamers.len(),
                            "粉丝数采集完成，部分主播失败"
                        ),
                        Err(error) => tracing::warn!(%error, "粉丝数采集失败"),
                    }
                }
            },
        )
        .await;
    });

    let fertility_state = state.clone();
    tokio::spawn(async move {
        run_poll_loop(
            || async { Duration::from_secs(FERTILITY_POLL_INTERVAL_SECONDS) },
            move |_| {
                let service = fertility_state.service.clone();
                async move {
                    // 开关关闭时 refresh_fertility_if_enabled 直接返回 Ok(None)，不会发出请求。
                    match service.refresh_fertility_if_enabled().await {
                        Ok(Some(matched)) => {
                            tracing::info!(matched, "LAPLACE 生理期数据采集完成")
                        }
                        Ok(None) => {}
                        Err(error) => tracing::warn!(%error, "LAPLACE 生理期数据采集失败"),
                    }
                }
            },
        )
        .await;
    });

    let youtube_oauth_state = state.clone();
    tokio::spawn(async move {
        run_poll_loop(
            || async { Duration::from_secs(300) },
            move |_| {
                let state = youtube_oauth_state.clone();
                let service = state.service.clone();
                async move {
                    match youtube_feature_enabled(service.store()).await {
                        Ok(true) => {}
                        Ok(false) => return,
                        Err(error) => {
                            tracing::warn!(%error, "读取 YouTube 功能开关失败");
                            return;
                        }
                    }
                    let runtime = youtube_runtime();
                    if !runtime.oauth_configured() {
                        return;
                    }
                    match runtime
                        .refresh_oauth_if_needed(service.store(), &state.secret_vault)
                        .await
                    {
                        Ok(true) => {
                            if let Err(error) =
                                sync_runtime_secrets_from_vault(&state).await
                            {
                                tracing::warn!(%error, "YouTube OAuth 刷新后同步运行时凭据失败");
                            }
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(code = error.code, "YouTube OAuth 后台刷新失败");
                            if let Err(sync_error) =
                                sync_runtime_secrets_from_vault(&state).await
                            {
                                tracing::warn!(%sync_error, "YouTube OAuth 后台刷新失败后同步运行时凭据失败");
                            }
                        }
                    }
                }
            },
        )
        .await;
    });

    let sync_state = state.clone();
    tokio::spawn(async move {
        // start at current head: new connections replay history themselves, this
        // worker only needs to fan out changes committed while the server runs.
        let mut last_seq = loop {
            match sync_state.service.store().latest_sync_seq().await {
                Ok(seq) => break seq,
                Err(error) => {
                    tracing::warn!(%error, "读取同步游标失败，重试中");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Err(error) = sync_api::publish_sync_feed(&sync_state, &mut last_seq).await {
                tracing::warn!(%error, "读取同步变更日志失败");
            }
        }
    });

    let event_state = state.clone();
    tokio::spawn(async move {
        // Start at the current head so a server restart does not rebroadcast
        // historical live transitions as fresh notifications. Reconnecting
        // clients replay their own cursor or bootstrap from sync state.
        let mut last_seq = loop {
            match event_state.service.store().latest_event_seq().await {
                Ok(seq) => break seq,
                Err(error) => {
                    tracing::warn!(%error, "读取事件游标失败，重试中");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            match event_state
                .service
                .store()
                .events_after(last_seq, 200)
                .await
            {
                Ok(events) => {
                    for event in events {
                        if let Some(seq) = event.get("seq").and_then(serde_json::Value::as_i64) {
                            last_seq = last_seq.max(seq);
                        }
                        let _ = event_state.hub.send(event);
                    }
                }
                Err(error) => tracing::warn!(%error, "读取事件日志失败"),
            }
        }
    });
    let update_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        loop {
            if let Err(error) = update_state.updates.tick().await {
                tracing::warn!(%error, "服务器更新检查失败");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });

    let prune_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            let store = prune_state.service.store();
            if let Err(error) = store
                .prune_sync_changes(SyncPrunePolicy::Before(
                    Utc::now() - ChronoDuration::days(SYNC_RETENTION_DAYS),
                ))
                .await
            {
                tracing::warn!(%error, "按时间清理同步日志失败");
            }
            if let Err(error) = store
                .prune_sync_changes(SyncPrunePolicy::KeepLatest(SYNC_RETENTION_ROWS))
                .await
            {
                tracing::warn!(%error, "按数量清理同步日志失败");
            }
            if let Err(error) = store
                .prune_sync_changes(SyncPrunePolicy::KeepPayloadBytes(
                    SYNC_RETENTION_PAYLOAD_BYTES,
                ))
                .await
            {
                tracing::warn!(%error, "按容量清理同步日志失败");
            }
            tokio::time::sleep(SYNC_PRUNE_INTERVAL).await;
        }
    });
}

// One worker per poll family: no overlapping runs within a worker.
async fn run_poll_loop<P, PF, O, OF>(period: P, mut operation: O)
where
    P: Fn() -> PF,
    PF: Future<Output = Duration>,
    O: FnMut(bool) -> OF,
    OF: Future<Output = ()>,
{
    let mut previous = None;
    let mut first = true;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let interval = period().await;
        if previous.is_none_or(|started: Instant| started.elapsed() >= interval) {
            previous = Some(Instant::now());
            operation(first).await;
            first = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn slow_dynamic_does_not_block_live_worker() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let live = tokio::spawn(run_poll_loop(
            || async { Duration::from_secs(20) },
            move |first| {
                let count = count.clone();
                async move {
                    let previous = count.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(first, previous == 0);
                }
            },
        ));
        let dynamic = tokio::spawn(run_poll_loop(
            || async { Duration::from_secs(300) },
            |_| async { tokio::time::sleep(Duration::from_secs(90)).await },
        ));
        tokio::task::yield_now().await;
        for _ in 0..65 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        live.abort();
        dynamic.abort();
        assert!(calls.load(Ordering::SeqCst) >= 4);
    }
}
