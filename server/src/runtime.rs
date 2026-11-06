use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use chrono::{Duration as ChronoDuration, Utc};
use oshical_core::{
    load_settings, AppService, SqliteStore, SyncPrunePolicy, FAN_POLL_INTERVAL_SECONDS,
};
use tokio::sync::broadcast;

use crate::api::{build_router, persist_changed_settings, AppState};
use crate::config::ServerConfig;
use crate::jobs::RefreshJobManager;
use crate::secrets::{load_persisted_bili_secret, load_persisted_llm_secret, SecretVault};
use crate::sync_api;
use crate::update_manager::ServerUpdateManager;

pub async fn run_server(config: ServerConfig) -> Result<(), String> {
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
    persist_changed_settings(&store, &persisted_settings, &settings)
        .await
        .map_err(|error| format!("保存服务端设置失败: {error}"))?;
    let secret_vault = SecretVault::from_env(config.api_token.as_deref(), &config.database_path);
    let mut runtime_secrets = config.secrets;
    if let Some(persisted) = load_persisted_llm_secret(&store, &secret_vault)
        .await
        .map_err(|error| format!("读取服务端模型密钥失败: {error}"))?
    {
        runtime_secrets.llm_api_key = Some(persisted.api_key);
    }
    if let Some(persisted) = load_persisted_bili_secret(&store, &secret_vault)
        .await
        .map_err(|error| format!("读取服务端 B 站登录失败: {error}"))?
    {
        runtime_secrets.bili_cookie = Some(persisted.cookie);
    }
    let service = AppService::new_with_proxy(store, runtime_secrets, settings.proxy_port)
        .map_err(|error| format!("初始化服务失败: {error}"))?;
    let (hub, _) = broadcast::channel(256);
    let updates = ServerUpdateManager::new(config.update_dir.clone())
        .map_err(|error| format!("初始化服务器更新管理器失败: {error}"))?;
    let runtime_proxy_port = Arc::new(tokio::sync::Mutex::new(settings.proxy_port));
    let state = AppState {
        service: Arc::new(service),
        api_token: config.api_token.map(Arc::new),
        secret_vault,
        hub,
        jobs: RefreshJobManager::new(),
        updates,
        runtime_proxy_port,
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
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let store = prune_state.service.store();
            if let Err(error) = store
                .prune_sync_changes(SyncPrunePolicy::Before(
                    Utc::now() - ChronoDuration::days(30),
                ))
                .await
            {
                tracing::warn!(%error, "按时间清理同步日志失败");
            }
            if let Err(error) = store
                .prune_sync_changes(SyncPrunePolicy::KeepLatest(100_000))
                .await
            {
                tracing::warn!(%error, "按数量清理同步日志失败");
            }
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
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
