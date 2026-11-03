mod sync_client;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use oshical_core::{
    load_settings, save_settings as save_settings_to_store, AppError, AppService, AppSettings,
    DataMode, FanHistory, FertilitySnapshot, LlmConfig, LlmDiscoveryConfig, LlmTestReport,
    NewSchedule, NewWeeklyScheduleRule, ProxyTestInput, ProxyTestReport, RuntimeSecrets, Schedule,
    Streamer, SyncMutation, UpdateSchedule, UpdateSubscription, UpdateWeeklyScheduleRule,
    WeeklyScheduleRule, FAN_POLL_INTERVAL_SECONDS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::webview::Cookie;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Mutex;
const KEYRING_SERVICE: &str = "com.oshical.app";
const KEY_LLM_API_KEY: &str = "llm_api_key";
const KEY_BILI_COOKIE: &str = "bili_cookie";
const KEY_SERVER_TOKEN: &str = "server_api_token";
const APP_UPDATE_DIRECT_ENDPOINT: &str =
    "https://github.com/yueTc/OshiCal/releases/latest/download/latest.json";

struct DesktopState {
    service: Arc<AppService>,
    remote_task: Arc<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
    sync_stores: Arc<std::sync::Mutex<HashMap<String, oshical_core::SqliteStore>>>,
    server_cache_root: PathBuf,
}

fn server_cache_key(server_url: &str) -> Result<String, String> {
    let normalized = sync_client::normalize_server_key(server_url)?;
    let digest = Sha256::digest(normalized.as_bytes());
    Ok(format!("{digest:x}"))
}

async fn server_cache_store(
    sync_stores: Arc<std::sync::Mutex<HashMap<String, oshical_core::SqliteStore>>>,
    cache_root: PathBuf,
    main_store: oshical_core::SqliteStore,
    server_url: String,
) -> Result<oshical_core::SqliteStore, String> {
    let normalized = sync_client::normalize_server_key(&server_url)?;
    {
        let stores = sync_stores.lock().map_err(|error| error.to_string())?;
        if let Some(store) = stores.get(&normalized) {
            return Ok(store.clone());
        }
    }
    let directory = cache_root.join(server_cache_key(&normalized)?);
    let opened = tokio::task::spawn_blocking(move || {
        tauri::async_runtime::block_on(async move {
            let stable_device_id = sync_client::device_id(&main_store).await?;
            let store = oshical_core::SqliteStore::connect_path(directory.join("cache.db"))
                .await
                .map_err(|error| format!("打开服务器缓存数据库失败: {error}"))?;
            sync_client::ensure_schema(&store).await?;
            sync_client::set_device_id(&store, &stable_device_id).await?;
            Ok::<_, String>(store)
        })
    })
    .await
    .map_err(|error| format!("服务器缓存初始化任务失败: {error}"))??;
    let mut stores = sync_stores.lock().map_err(|error| error.to_string())?;
    if let Some(existing) = stores.get(&normalized) {
        return Ok(existing.clone());
    }
    stores.insert(normalized, opened.clone());
    Ok(opened)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SecretInput {
    llm_api_key: Option<String>,
    bili_cookie: Option<String>,
    server_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SecretStatus {
    llm_configured: bool,
    bili_configured: bool,
    server_token_configured: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BiliQrLoginStartResponse {
    qrcode_key: String,
    url: String,
    expires_in_seconds: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BiliQrLoginPollResponse {
    status: String,
    message: String,
    cookie_saved: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    settings: AppSettings,
    subscriptions: Vec<Streamer>,
    live_statuses: Vec<oshical_core::LiveStatus>,
    secrets: SecretStatus,
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let data_dir = match std::env::var("OSHICAL_DATA_DIR") {
                Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
                _ => app
                    .path()
                    .app_data_dir()
                    .map_err(|error| format!("无法获取应用数据目录: {error}"))?,
            };
            std::fs::create_dir_all(&data_dir)?;
            let server_cache_root = data_dir.join("servers");
            std::fs::create_dir_all(&server_cache_root)?;
            let db_path = data_dir.join("oshical.db");
            let store =
                tauri::async_runtime::block_on(oshical_core::SqliteStore::connect(&db_path))
                    .map_err(|error| format!("打开本地数据库失败: {error}"))?;
            tauri::async_runtime::block_on(sync_client::ensure_schema(&store))
                .map_err(|error| format!("初始化同步数据库失败: {error}"))?;
            let settings = tauri::async_runtime::block_on(load_settings(&store))
                .map_err(|error| format!("读取本地设置失败: {error}"))?;
            let secrets = load_runtime_secrets();
            let service = Arc::new(
                AppService::new_with_proxy(store, secrets, settings.proxy_port)
                    .map_err(|error| format!("初始化本地服务失败: {error}"))?,
            );
            app.manage(DesktopState {
                service: service.clone(),
                remote_task: Arc::new(Mutex::new(None)),
                sync_stores: Arc::new(std::sync::Mutex::new(HashMap::new())),
                server_cache_root,
            });
            setup_window(app.handle());
            setup_tray(app.handle())?;
            spawn_local_monitor(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            search_accounts,
            add_subscription,
            update_subscription,
            reorder_subscriptions,
            delete_subscription,
            add_linked_account,
            remove_linked_account,
            refresh_all,
            refresh_streamer,
            list_schedules,
            create_schedule,
            update_schedule,
            delete_schedule,
            list_weekly_rules,
            create_weekly_rule,
            update_weekly_rule,
            delete_weekly_rule,
            cancel_schedule,
            list_live_statuses,
            list_live_sessions,
            get_fan_history,
            refresh_streamer_fans,
            live_session_for_schedule,
            list_fertility_statuses,
            check_live_now,
            get_settings,
            save_settings,
            test_proxy,
            test_llm,
            list_llm_models,
            save_secrets,
            get_secret_status,
            open_bilibili_browser_login,
            start_bilibili_qr_login,
            poll_bilibili_qr_login,
            clear_bilibili_cookie,
            get_server_token,
            get_device_id,
            check_app_update,
            install_app_update,
            get_sync_meta,
            list_sync_devices,
            get_sync_cursor,
            set_sync_ack,
            apply_sync_batch,
            apply_sync_bootstrap,
            start_remote_monitor,
            stop_remote_monitor,
            rebuild_remote_sync,
            list_pending_mutations,
            enqueue_mutation,
            update_mutation_status,
            import_server_snapshot,
            preview_server_snapshot,
            export_client_snapshot,
            cached_schedules,
            cached_schedule,
            cache_create_schedule,
            cache_update_schedule,
            cache_delete_schedule,
            purge_offline_schedules,
            remote_entity,
            open_external_url
        ])
        .run(tauri::generate_context!())
        .expect("运行推历 OshiCal 桌面端失败");
}

fn setup_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let window_to_hide = window.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window_to_hide.hide();
            }
        });
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开推历 OshiCal", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh", "立即刷新", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &refresh, &quit])?;
    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("推历 OshiCal · 直播日程")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }
            "refresh" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = run_refresh_action(&app).await {
                        let _ = app.emit("monitor-error", error);
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateInfo {
    current_version: String,
    version: String,
    date: Option<String>,
    body: Option<String>,
}

fn normalize_mirror_base(mirror_url: Option<&str>) -> Result<Option<String>, String> {
    let Some(value) = mirror_url.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let url = url::Url::parse(value).map_err(|error| format!("GitHub 镜像地址无效: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("GitHub 镜像地址必须使用 http 或 https".to_owned());
    }
    let base = url.to_string();
    Ok(Some(if base.ends_with('/') {
        base
    } else {
        format!("{base}/")
    }))
}

fn app_update_endpoints(mirror_url: Option<&str>) -> Result<Vec<url::Url>, String> {
    let direct = url::Url::parse(APP_UPDATE_DIRECT_ENDPOINT)
        .map_err(|error| format!("更新地址无效: {error}"))?;
    let Some(base) = normalize_mirror_base(mirror_url)? else {
        return Ok(vec![direct]);
    };
    let mirror = url::Url::parse(&format!("{base}{APP_UPDATE_DIRECT_ENDPOINT}"))
        .map_err(|error| format!("镜像更新地址无效: {error}"))?;
    Ok(vec![mirror, direct])
}

async fn resolve_app_update(
    app: &AppHandle,
    mirror_url: Option<&str>,
) -> Result<Option<tauri_plugin_updater::Update>, String> {
    let endpoints = app_update_endpoints(mirror_url)?;
    let updater = app
        .updater_builder()
        .endpoints(endpoints)
        .map_err(|error| format!("配置更新端点失败: {error}"))?
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|error| format!("初始化更新检查失败: {error}"))?;
    let mut update = updater
        .check()
        .await
        .map_err(|error| format!("检查更新失败: {error}"))?;
    if let (Some(update), Some(base)) = (update.as_mut(), normalize_mirror_base(mirror_url)?) {
        if update.download_url.scheme() == "https"
            && !update.download_url.as_str().starts_with(&base)
        {
            update.download_url = url::Url::parse(&format!("{base}{}", update.download_url))
                .map_err(|error| format!("镜像下载地址无效: {error}"))?;
        }
    }
    Ok(update)
}

async fn install_resolved_update(
    app: &AppHandle,
    update: &tauri_plugin_updater::Update,
) -> Result<(), String> {
    let downloaded = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let chunk_app = app.clone();
    let chunk_downloaded = downloaded.clone();
    let finish_app = app.clone();
    let finish_downloaded = downloaded.clone();
    update
        .download_and_install(
            move |chunk_length, content_length| {
                let current = chunk_downloaded
                    .fetch_add(chunk_length as u64, std::sync::atomic::Ordering::Relaxed)
                    + chunk_length as u64;
                let _ = chunk_app.emit(
                    "app-update-progress",
                    json!({
                        "downloaded": current,
                        "total": content_length,
                        "done": false
                    }),
                );
            },
            move || {
                let current = finish_downloaded.load(std::sync::atomic::Ordering::Relaxed);
                let _ = finish_app.emit(
                    "app-update-progress",
                    json!({
                        "downloaded": current,
                        "total": current,
                        "done": true
                    }),
                );
            },
        )
        .await
        .map_err(|error| format!("下载或安装更新失败: {error}"))
}

#[tauri::command]
async fn check_app_update(
    app: AppHandle,
    mirror_url: Option<String>,
) -> Result<Option<AppUpdateInfo>, String> {
    let update = resolve_app_update(&app, mirror_url.as_deref()).await?;
    Ok(update.as_ref().map(|update| AppUpdateInfo {
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        date: update.date.map(|value| value.to_string()),
        body: update.body.clone(),
    }))
}

#[tauri::command]
async fn install_app_update(app: AppHandle, mirror_url: Option<String>) -> Result<(), String> {
    let update = resolve_app_update(&app, mirror_url.as_deref()).await?;
    let Some(update) = update else {
        return Err("当前已是最新版本".to_owned());
    };
    match install_resolved_update(&app, &update).await {
        Ok(()) => Ok(()),
        Err(first_error) => {
            let Some(_) = normalize_mirror_base(mirror_url.as_deref())? else {
                return Err(first_error);
            };
            let direct_update = resolve_app_update(&app, None).await?;
            let Some(direct_update) = direct_update else {
                return Err(first_error);
            };
            install_resolved_update(&app, &direct_update).await
        }
    }
}

async fn run_refresh_action(app: &AppHandle) -> Result<Value, String> {
    let state = app.state::<DesktopState>();
    let settings = load_settings(state.service.store())
        .await
        .map_err(|error| error.to_string())?;
    if settings.data_mode == DataMode::Server {
        let token =
            get_server_token().ok_or_else(|| "服务器模式缺少服务器 Token，无法刷新".to_owned())?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|error| error.to_string())?;
        let job = sync_client::request_refresh_job(
            &client,
            settings.server_url.trim_end_matches('/'),
            &token,
            None,
        )
        .await?;
        let value = json!({
            "mode": "server",
            "jobId": job.job_id,
            "status": job.status,
        });
        let _ = app.emit("refresh-job-started", value.clone());
        return Ok(value);
    }

    let report = state
        .service
        .refresh_all()
        .await
        .map_err(|error| error.to_string())?;
    Ok(json!(report))
}

fn keyring_entry(user: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|error| error.to_string())
}

fn get_secret(user: &str) -> Option<String> {
    keyring_entry(user)
        .ok()?
        .get_password()
        .ok()
        .filter(|value| !value.is_empty())
}

fn set_secret(user: &str, value: Option<String>) -> Result<(), String> {
    let entry = keyring_entry(user)?;
    match value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        Some(value) => entry
            .set_password(&value)
            .map_err(|error| error.to_string()),
        None => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        },
    }
}

fn load_runtime_secrets() -> RuntimeSecrets {
    RuntimeSecrets {
        llm_api_key: get_secret(KEY_LLM_API_KEY),
        bili_cookie: get_secret(KEY_BILI_COOKIE),
    }
}

fn secret_status() -> SecretStatus {
    SecretStatus {
        llm_configured: get_secret(KEY_LLM_API_KEY).is_some(),
        bili_configured: get_secret(KEY_BILI_COOKIE).is_some(),
        server_token_configured: get_secret(KEY_SERVER_TOKEN).is_some(),
    }
}

fn map_error(error: AppError) -> String {
    format!("{}: {error}", error.code())
}

#[tauri::command]
async fn bootstrap(state: State<'_, DesktopState>) -> Result<Bootstrap, String> {
    let settings = load_settings(state.service.store())
        .await
        .map_err(map_error)?;
    let subscriptions = state
        .service
        .list_subscriptions()
        .await
        .map_err(map_error)?;
    let live_statuses = state
        .service
        .list_live_statuses()
        .await
        .map_err(map_error)?;
    Ok(Bootstrap {
        settings,
        subscriptions,
        live_statuses,
        secrets: secret_status(),
    })
}

#[tauri::command]
async fn search_accounts(
    state: State<'_, DesktopState>,
    keyword: String,
) -> Result<Vec<oshical_core::AccountSearchResult>, String> {
    state
        .service
        .search_accounts(&keyword)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn add_subscription(
    state: State<'_, DesktopState>,
    input: oshical_core::AddSubscription,
) -> Result<Streamer, String> {
    state
        .service
        .add_subscription(input)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn update_subscription(
    state: State<'_, DesktopState>,
    id: i64,
    update: UpdateSubscription,
) -> Result<Streamer, String> {
    state
        .service
        .update_subscription(id, &update)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn reorder_subscriptions(
    state: State<'_, DesktopState>,
    ids: Vec<i64>,
) -> Result<Vec<Streamer>, String> {
    state
        .service
        .reorder_subscriptions(&ids)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn delete_subscription(state: State<'_, DesktopState>, id: i64) -> Result<(), String> {
    state
        .service
        .delete_subscription(id)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn add_linked_account(
    state: State<'_, DesktopState>,
    id: i64,
    mid: String,
) -> Result<oshical_core::LinkedAccount, String> {
    state
        .service
        .add_linked_account(id, &mid)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn remove_linked_account(
    state: State<'_, DesktopState>,
    id: i64,
    mid: String,
) -> Result<(), String> {
    state
        .service
        .store()
        .remove_linked_account(id, &mid)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn refresh_all(
    state: State<'_, DesktopState>,
) -> Result<oshical_core::RefreshReport, String> {
    state.service.refresh_all().await.map_err(map_error)
}

#[tauri::command]
async fn refresh_streamer(
    app: AppHandle,
    state: State<'_, DesktopState>,
    id: i64,
) -> Result<oshical_core::RefreshReport, String> {
    let progress_app = app.clone();
    state
        .service
        .refresh_streamer_with_progress(id, move |progress| {
            let _ = progress_app.emit("refresh-progress", progress);
        })
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn list_schedules(
    state: State<'_, DesktopState>,
    from: String,
    to: String,
) -> Result<Vec<Schedule>, String> {
    let from = parse_datetime(&from)?;
    let to = parse_datetime(&to)?;
    state
        .service
        .list_schedules(from, to)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn create_schedule(
    state: State<'_, DesktopState>,
    input: NewSchedule,
) -> Result<Schedule, String> {
    state
        .service
        .create_schedule(&input)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn update_schedule(
    state: State<'_, DesktopState>,
    id: i64,
    update: UpdateSchedule,
) -> Result<Schedule, String> {
    state
        .service
        .update_schedule(id, &update)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn delete_schedule(state: State<'_, DesktopState>, id: i64) -> Result<(), String> {
    state.service.delete_schedule(id).await.map_err(map_error)
}

#[tauri::command]
async fn list_weekly_rules(
    state: State<'_, DesktopState>,
    streamer_id: Option<i64>,
) -> Result<Vec<WeeklyScheduleRule>, String> {
    state
        .service
        .list_weekly_rules(streamer_id)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn create_weekly_rule(
    state: State<'_, DesktopState>,
    input: NewWeeklyScheduleRule,
) -> Result<WeeklyScheduleRule, String> {
    state
        .service
        .create_weekly_rule(&input)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn update_weekly_rule(
    state: State<'_, DesktopState>,
    id: i64,
    update: UpdateWeeklyScheduleRule,
) -> Result<WeeklyScheduleRule, String> {
    state
        .service
        .update_weekly_rule(id, &update)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn delete_weekly_rule(state: State<'_, DesktopState>, id: i64) -> Result<(), String> {
    state
        .service
        .delete_weekly_rule(id)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn cancel_schedule(state: State<'_, DesktopState>, id: i64) -> Result<Schedule, String> {
    state
        .service
        .store()
        .cancel_schedule(id)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn list_live_statuses(
    state: State<'_, DesktopState>,
) -> Result<Vec<oshical_core::LiveStatus>, String> {
    state.service.list_live_statuses().await.map_err(map_error)
}

#[tauri::command]
async fn list_live_sessions(
    state: State<'_, DesktopState>,
    streamer_id: Option<i64>,
    limit: Option<i64>,
) -> Result<Vec<oshical_core::LiveSession>, String> {
    state
        .service
        .list_live_sessions(streamer_id, limit.unwrap_or(50))
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn get_fan_history(
    state: State<'_, DesktopState>,
    streamer_id: i64,
    days: Option<i64>,
) -> Result<FanHistory, String> {
    state
        .service
        .get_fan_history(streamer_id, days.unwrap_or(30))
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn refresh_streamer_fans(
    state: State<'_, DesktopState>,
    streamer_id: i64,
) -> Result<FanHistory, String> {
    state
        .service
        .refresh_streamer_fans(streamer_id)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn live_session_for_schedule(
    state: State<'_, DesktopState>,
    schedule_id: i64,
) -> Result<Option<oshical_core::LiveSession>, String> {
    state
        .service
        .live_session_for_schedule(schedule_id)
        .await
        .map_err(map_error)
}
#[tauri::command]
async fn list_fertility_statuses(
    state: State<'_, DesktopState>,
    refresh: bool,
) -> Result<FertilitySnapshot, String> {
    state
        .service
        .list_fertility_statuses(refresh)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn check_live_now(
    state: State<'_, DesktopState>,
) -> Result<Vec<oshical_core::LiveStatus>, String> {
    state.service.check_live_all().await.map_err(map_error)
}

#[tauri::command]
async fn get_settings(state: State<'_, DesktopState>) -> Result<AppSettings, String> {
    load_settings(state.service.store())
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn save_settings(
    state: State<'_, DesktopState>,
    settings: AppSettings,
) -> Result<AppSettings, String> {
    save_settings_to_store(state.service.store(), &settings)
        .await
        .map_err(map_error)?;
    state
        .service
        .update_proxy_port(settings.proxy_port)
        .await
        .map_err(map_error)?;
    Ok(settings)
}

#[tauri::command]
async fn test_proxy(
    state: State<'_, DesktopState>,
    input: ProxyTestInput,
) -> Result<ProxyTestReport, String> {
    state
        .service
        .test_proxy(input.proxy_port)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn test_llm(
    state: State<'_, DesktopState>,
    config: LlmConfig,
) -> Result<LlmTestReport, String> {
    state.service.test_llm(config).await.map_err(map_error)
}

#[tauri::command]
async fn list_llm_models(
    state: State<'_, DesktopState>,
    config: LlmDiscoveryConfig,
) -> Result<Vec<String>, String> {
    state
        .service
        .list_llm_models(config)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn save_secrets(
    state: State<'_, DesktopState>,
    input: SecretInput,
) -> Result<SecretStatus, String> {
    if let Some(value) = input.llm_api_key {
        set_secret(KEY_LLM_API_KEY, Some(value))?;
    }
    if let Some(value) = input.bili_cookie {
        set_secret(KEY_BILI_COOKIE, Some(value))?;
    }
    if let Some(value) = input.server_token {
        set_secret(KEY_SERVER_TOKEN, Some(value))?;
    }
    state
        .service
        .update_secrets(load_runtime_secrets())
        .await
        .map_err(map_error)?;
    Ok(secret_status())
}

async fn persist_bili_cookie(service: &AppService, cookie: String) -> Result<SecretStatus, String> {
    set_secret(KEY_BILI_COOKIE, Some(cookie))?;
    service
        .update_secrets(load_runtime_secrets())
        .await
        .map_err(map_error)?;
    Ok(secret_status())
}

#[tauri::command]
fn get_secret_status() -> SecretStatus {
    secret_status()
}

#[tauri::command]
async fn start_bilibili_qr_login(
    state: State<'_, DesktopState>,
) -> Result<BiliQrLoginStartResponse, String> {
    let login = state
        .service
        .create_bili_qr_login()
        .await
        .map_err(map_error)?;
    Ok(BiliQrLoginStartResponse {
        qrcode_key: login.qrcode_key,
        url: login.url,
        expires_in_seconds: login.expires_in_seconds,
    })
}

#[tauri::command]
async fn poll_bilibili_qr_login(
    app: AppHandle,
    state: State<'_, DesktopState>,
    qrcode_key: String,
) -> Result<BiliQrLoginPollResponse, String> {
    let poll = state
        .service
        .poll_bili_qr_login(&qrcode_key)
        .await
        .map_err(map_error)?;
    let cookie_saved = if let Some(cookie) = poll.cookie {
        let status = persist_bili_cookie(&state.service, cookie).await?;
        let _ = app.emit("bili-cookie-updated", &status);
        true
    } else {
        false
    };
    Ok(BiliQrLoginPollResponse {
        status: poll.status.as_str().to_owned(),
        message: poll.message,
        cookie_saved,
    })
}

#[tauri::command]
async fn clear_bilibili_cookie(
    app: AppHandle,
    state: State<'_, DesktopState>,
) -> Result<SecretStatus, String> {
    set_secret(KEY_BILI_COOKIE, None)?;
    state
        .service
        .update_secrets(load_runtime_secrets())
        .await
        .map_err(map_error)?;
    let status = secret_status();
    let _ = app.emit("bili-cookie-updated", status.clone());
    Ok(status)
}

fn bili_login_url_allowed(url: &url::Url) -> bool {
    if url.scheme() == "about" {
        return true;
    }
    let host = url.host_str().unwrap_or_default();
    url.scheme() == "https"
        && (host == "bilibili.com"
            || host.ends_with(".bilibili.com")
            || host == "biligame.com"
            || host.ends_with(".biligame.com"))
}

fn cookie_header_from_webview(cookies: &[Cookie<'static>]) -> Option<String> {
    const COOKIE_NAMES: [&str; 5] = [
        "SESSDATA",
        "bili_jct",
        "DedeUserID",
        "DedeUserID__ckMd5",
        "sid",
    ];
    let mut values = BTreeMap::new();
    for cookie in cookies {
        let domain = cookie.domain().unwrap_or_default();
        if domain != "bilibili.com"
            && !domain.ends_with(".bilibili.com")
            && domain != "biligame.com"
            && !domain.ends_with(".biligame.com")
        {
            continue;
        }
        let name = cookie.name();
        let value = cookie.value();
        if COOKIE_NAMES.contains(&name)
            && !value.is_empty()
            && value.len() <= 4096
            && !value
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | ';'))
        {
            values.insert(name.to_owned(), value.to_owned());
        }
    }
    values.get("SESSDATA")?;
    Some(
        COOKIE_NAMES
            .iter()
            .filter_map(|name| values.get(*name).map(|value| format!("{name}={value}")))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

#[tauri::command]
async fn open_bilibili_browser_login(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("bilibili-login") {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }
    let login_url = url::Url::parse("https://passport.bilibili.com/login")
        .map_err(|error| error.to_string())?;
    let window = WebviewWindowBuilder::new(&app, "bilibili-login", WebviewUrl::External(login_url))
        .title("B 站登录 - 推历 OshiCal")
        .inner_size(560.0, 760.0)
        .min_inner_size(440.0, 600.0)
        .center()
        .on_navigation(bili_login_url_allowed)
        .build()
        .map_err(|error| format!("打开 B 站登录窗口失败: {error}"))?;

    let task_window = window.clone();
    let task_app = app.clone();
    tauri::async_runtime::spawn(async move {
        for _ in 0..180 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !task_window.is_visible().unwrap_or(false) {
                break;
            }
            let probe = task_window.clone();
            let result = tauri::async_runtime::spawn_blocking(move || probe.cookies()).await;
            let Ok(Ok(cookies)) = result else {
                continue;
            };
            let Some(cookie) = cookie_header_from_webview(&cookies) else {
                continue;
            };
            let state = task_app.state::<DesktopState>();
            match persist_bili_cookie(&state.service, cookie).await {
                Ok(status) => {
                    let _ = task_app.emit("bili-cookie-updated", status);
                    let _ = task_window.close();
                }
                Err(error) => {
                    let _ = task_app.emit("bili-cookie-login-error", error);
                }
            }
            break;
        }
    });
    Ok(())
}

#[tauri::command]
fn get_server_token() -> Option<String> {
    get_secret(KEY_SERVER_TOKEN)
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| format!("时间格式错误: {error}"))
}

fn spawn_local_monitor(app: AppHandle) {
    let monitor_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut first = true;
        let mut last_live = Instant::now() - Duration::from_secs(86_400);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let state = monitor_app.state::<DesktopState>();
            let settings = match load_settings(state.service.store()).await {
                Ok(settings) => settings,
                Err(error) => {
                    let _ = monitor_app.emit("monitor-error", error.to_string());
                    continue;
                }
            };
            if settings.data_mode != DataMode::Local {
                first = true;
                continue;
            }
            if last_live.elapsed() >= Duration::from_secs(settings.live_poll_seconds) {
                last_live = Instant::now();
                let result = if first {
                    state.service.prime_live_statuses().await
                } else {
                    state.service.check_live_all().await
                };
                first = false;
                if let Err(error) = result {
                    let _ = monitor_app.emit("monitor-error", error.to_string());
                }
            }
        }
    });
    let dynamic_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut previous = None;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let state = dynamic_app.state::<DesktopState>();
            let settings = match load_settings(state.service.store()).await {
                Ok(settings) => settings,
                Err(error) => {
                    let _ = dynamic_app.emit("monitor-error", error.to_string());
                    continue;
                }
            };
            if settings.data_mode != DataMode::Local {
                continue;
            }
            if previous.is_none_or(|started: Instant| {
                started.elapsed() >= Duration::from_secs(settings.dynamic_poll_seconds)
            }) {
                previous = Some(Instant::now());
                if let Err(error) = state.service.refresh_all().await {
                    let _ = dynamic_app.emit("monitor-error", error.to_string());
                }
            }
        }
    });

    let fan_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut previous = None;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let state = fan_app.state::<DesktopState>();
            let settings = match load_settings(state.service.store()).await {
                Ok(settings) => settings,
                Err(error) => {
                    let _ = fan_app.emit("monitor-error", error.to_string());
                    continue;
                }
            };
            if settings.data_mode != DataMode::Local {
                previous = None;
                continue;
            }
            if previous.is_none_or(|started: Instant| {
                started.elapsed() >= Duration::from_secs(FAN_POLL_INTERVAL_SECONDS)
            }) {
                previous = Some(Instant::now());
                match state.service.refresh_all_fans().await {
                    Ok(report) if report.refreshed_streamers > 0 => {
                        let _ = fan_app.emit("fan-count-updated", report);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = fan_app.emit("monitor-error", error.to_string());
                    }
                }
            }
        }
    });

    let pump_app = app;
    tauri::async_runtime::spawn(async move {
        let state = pump_app.state::<DesktopState>();
        let mut last_seq = state.service.store().latest_event_seq().await.unwrap_or(0);
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let state = pump_app.state::<DesktopState>();
            if let Ok(events) = state.service.store().events_after(last_seq, 200).await {
                for event in events {
                    if let Some(seq) = event.get("seq").and_then(Value::as_i64) {
                        last_seq = last_seq.max(seq);
                    }
                    let _ = pump_app.emit("server-event", event);
                }
            }
        }
    });
}

#[tauri::command]
async fn start_remote_monitor(
    app: AppHandle,
    state: State<'_, DesktopState>,
    server_url: String,
    token: String,
) -> Result<(), String> {
    url::Url::parse(&server_url).map_err(|error| format!("服务器地址无效: {error}"))?;
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let remote_task = state.remote_task.clone();
    let monitor_store = server_cache_store(
        sync_stores,
        cache_root,
        main_store.clone(),
        server_url.clone(),
    )
    .await?;
    let device_id = sync_client::device_id(&main_store).await?;
    let mut guard = remote_task.lock().await;
    if let Some(task) = guard.take() {
        task.abort();
    }
    let task = tauri::async_runtime::spawn(remote_monitor_loop(
        app,
        server_url,
        token,
        monitor_store,
        device_id,
    ));
    *guard = Some(task);
    Ok(())
}

#[tauri::command]
async fn rebuild_remote_sync(
    app: AppHandle,
    state: State<'_, DesktopState>,
    server_url: String,
    token: String,
) -> Result<i64, String> {
    url::Url::parse(&server_url).map_err(|error| format!("服务器地址无效: {error}"))?;
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let remote_task = state.remote_task.clone();
    let monitor_store = server_cache_store(
        sync_stores,
        cache_root,
        main_store.clone(),
        server_url.clone(),
    )
    .await?;
    let device_id = sync_client::device_id(&main_store).await?;
    {
        let mut guard = remote_task.lock().await;
        if let Some(task) = guard.take() {
            task.abort();
        }
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let previous_cursor = current_cursor(&monitor_store, &server_url)
        .await
        .unwrap_or_default();
    let cursor = bootstrap_remote(
        &app,
        &client,
        &monitor_store,
        &server_url,
        &token,
        &device_id,
        previous_cursor,
    )
    .await?;
    let task = tauri::async_runtime::spawn(remote_monitor_loop(
        app,
        server_url,
        token,
        monitor_store,
        device_id,
    ));
    *remote_task.lock().await = Some(task);
    Ok(cursor)
}

#[tauri::command]
async fn stop_remote_monitor(state: State<'_, DesktopState>) -> Result<(), String> {
    let mut guard = state.remote_task.lock().await;
    if let Some(task) = guard.take() {
        task.abort();
    }
    Ok(())
}

#[tauri::command]
async fn list_pending_mutations(
    state: State<'_, DesktopState>,
) -> Result<Vec<SyncMutation>, String> {
    state
        .service
        .store()
        .list_pending_mutations(500)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn enqueue_mutation(
    state: State<'_, DesktopState>,
    mutation: SyncMutation,
) -> Result<(), String> {
    state
        .service
        .store()
        .enqueue_mutation(&mutation)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn update_mutation_status(
    state: State<'_, DesktopState>,
    mutation_id: String,
    status: String,
    error: Option<String>,
) -> Result<(), String> {
    state
        .service
        .store()
        .update_mutation_status(&mutation_id, &status, error.as_deref())
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn cached_schedules(
    state: State<'_, DesktopState>,
    from: String,
    to: String,
) -> Result<Vec<Value>, String> {
    let main_store = state.service.store().clone();
    let service = state.service.clone();
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let settings = load_settings(&main_store).await.map_err(map_error)?;
    let from = parse_datetime(&from)?;
    let to = parse_datetime(&to)?;
    let server_mode =
        settings.data_mode == DataMode::Server && !settings.server_url.trim().is_empty();
    let store = if server_mode {
        server_cache_store(
            sync_stores,
            cache_root,
            main_store.clone(),
            settings.server_url.clone(),
        )
        .await?
    } else {
        main_store.clone()
    };
    let schedules = if server_mode {
        store
            .list_schedules(from, to, false)
            .await
            .map_err(map_error)?
    } else {
        service.list_schedules(from, to).await.map_err(map_error)?
    };
    let mut result = Vec::new();
    for schedule in schedules {
        let mapping = main_store
            .remote_entity(
                settings.server_url.trim_end_matches('/'),
                "schedule",
                schedule.id,
            )
            .await
            .map_err(map_error)?;
        let (remote_id, version) = mapping.unwrap_or((schedule.id, None));
        let mut value = serde_json::to_value(&schedule).map_err(|error| error.to_string())?;
        value["cachedLocalId"] = json!(schedule.id);
        value["remoteId"] = json!(remote_id);
        value["remoteUpdatedAt"] = json!(version);
        result.push(value);
    }
    Ok(result)
}

#[tauri::command]
async fn cached_schedule(
    state: State<'_, DesktopState>,
    remote_id: i64,
) -> Result<Option<Schedule>, String> {
    let main_store = state.service.store().clone();
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let settings = load_settings(&main_store).await.map_err(map_error)?;
    let server_mode =
        settings.data_mode == DataMode::Server && !settings.server_url.trim().is_empty();
    let store = if server_mode {
        server_cache_store(
            sync_stores,
            cache_root,
            main_store.clone(),
            settings.server_url.clone(),
        )
        .await?
    } else {
        main_store.clone()
    };
    let local_id = if server_mode {
        remote_id
    } else {
        match main_store
            .local_entity(
                settings.server_url.trim_end_matches('/'),
                "schedule",
                remote_id,
            )
            .await
            .map_err(map_error)?
        {
            Some(id) => id,
            None => remote_id,
        }
    };
    match store.get_schedule(local_id).await {
        Ok(schedule) => Ok(Some(schedule)),
        Err(oshical_core::AppError::NotFound(_)) => Ok(None),
        Err(error) => Err(map_error(error)),
    }
}
async fn current_server_cache_store(
    main_store: oshical_core::SqliteStore,
    sync_stores: Arc<std::sync::Mutex<HashMap<String, oshical_core::SqliteStore>>>,
    cache_root: PathBuf,
) -> Result<oshical_core::SqliteStore, String> {
    let settings = load_settings(&main_store).await.map_err(map_error)?;
    if settings.data_mode != DataMode::Server {
        return Err("当前不是服务器模式".to_owned());
    }
    let server_url = settings.server_url.trim().to_owned();
    if server_url.is_empty() {
        return Err("服务器模式未配置服务器地址".to_owned());
    }
    server_cache_store(sync_stores, cache_root, main_store, server_url).await
}

#[tauri::command]
async fn cache_create_schedule(
    state: State<'_, DesktopState>,
    input: NewSchedule,
) -> Result<Schedule, String> {
    let store = current_server_cache_store(
        state.service.store().clone(),
        state.sync_stores.clone(),
        state.server_cache_root.clone(),
    )
    .await?;
    sync_client::create_offline_schedule(&store, &input).await
}

#[tauri::command]
async fn cache_update_schedule(
    state: State<'_, DesktopState>,
    id: i64,
    update: UpdateSchedule,
) -> Result<Schedule, String> {
    let store = current_server_cache_store(
        state.service.store().clone(),
        state.sync_stores.clone(),
        state.server_cache_root.clone(),
    )
    .await?;
    sync_client::update_offline_schedule(&store, id, &update).await
}

#[tauri::command]
async fn cache_delete_schedule(state: State<'_, DesktopState>, id: i64) -> Result<(), String> {
    let store = current_server_cache_store(
        state.service.store().clone(),
        state.sync_stores.clone(),
        state.server_cache_root.clone(),
    )
    .await?;
    sync_client::delete_offline_schedule(&store, id).await
}

#[tauri::command]
async fn purge_offline_schedules(state: State<'_, DesktopState>) -> Result<u64, String> {
    let store = current_server_cache_store(
        state.service.store().clone(),
        state.sync_stores.clone(),
        state.server_cache_root.clone(),
    )
    .await?;
    sync_client::purge_offline_schedules(&store).await
}

#[tauri::command]
async fn remote_entity(
    state: State<'_, DesktopState>,
    entity: String,
    local_id: i64,
) -> Result<Option<i64>, String> {
    let settings = load_settings(state.service.store())
        .await
        .map_err(map_error)?;
    Ok(state
        .service
        .store()
        .remote_entity(settings.server_url.trim_end_matches('/'), &entity, local_id)
        .await
        .map_err(map_error)?
        .map(|value| value.0))
}

#[tauri::command]
async fn preview_server_snapshot(
    state: State<'_, DesktopState>,
    snapshot: oshical_core::store::ServerSnapshot,
) -> Result<oshical_core::store::ImportPreview, String> {
    state
        .service
        .store()
        .preview_snapshot(&snapshot)
        .await
        .map_err(map_error)
}

#[tauri::command]
async fn import_server_snapshot(
    state: State<'_, DesktopState>,
    snapshot: oshical_core::store::ServerSnapshot,
    strategy: String,
) -> Result<oshical_core::store::ImportReport, String> {
    let settings = load_settings(state.service.store())
        .await
        .map_err(map_error)?;
    state
        .service
        .store()
        .import_snapshot(
            &snapshot,
            &strategy,
            settings.server_url.trim_end_matches('/'),
        )
        .await
        .map_err(map_error)
}

#[tauri::command]
fn open_external_url(app: AppHandle, url: String) -> Result<(), String> {
    let parsed = validate_external_url(&url)?;
    tauri_plugin_opener::OpenerExt::opener(&app)
        .open_url(parsed.as_str(), None::<&str>)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn get_device_id(state: State<'_, DesktopState>) -> Result<String, String> {
    sync_client::device_id(state.service.store()).await
}

#[tauri::command]
async fn get_sync_meta(
    state: State<'_, DesktopState>,
    server_url: String,
) -> Result<sync_client::SyncMeta, String> {
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let store = server_cache_store(sync_stores, cache_root, main_store, server_url.clone()).await?;
    sync_client::get_sync_meta(&store, &server_url).await
}

#[tauri::command]
async fn list_sync_devices(
    server_url: String,
    token: String,
) -> Result<Vec<sync_client::DeviceInfo>, String> {
    url::Url::parse(&server_url).map_err(|error| format!("服务器地址无效: {error}"))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?;
    sync_client::list_devices(&client, &server_url, &token).await
}

#[tauri::command]
async fn get_sync_cursor(
    state: State<'_, DesktopState>,
    server_url: String,
) -> Result<i64, String> {
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let store = server_cache_store(sync_stores, cache_root, main_store, server_url.clone()).await?;
    Ok(sync_client::get_sync_meta(&store, &server_url)
        .await?
        .cursor_seq)
}

#[tauri::command]
async fn set_sync_ack(
    state: State<'_, DesktopState>,
    server_url: String,
    seq: i64,
) -> Result<sync_client::SyncMeta, String> {
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let store = server_cache_store(sync_stores, cache_root, main_store, server_url.clone()).await?;
    sync_client::set_sync_ack(&store, &server_url, seq).await
}

#[tauri::command]
async fn apply_sync_batch(
    state: State<'_, DesktopState>,
    server_url: String,
    batch: sync_client::SyncBatch,
) -> Result<sync_client::ApplySyncBatchResult, String> {
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let store = server_cache_store(sync_stores, cache_root, main_store, server_url.clone()).await?;
    sync_client::apply_batch(&store, &server_url, &batch).await
}

#[tauri::command]
async fn apply_sync_bootstrap(
    state: State<'_, DesktopState>,
    server_url: String,
    bootstrap: sync_client::SyncBootstrap,
) -> Result<sync_client::ApplySyncBootstrapResult, String> {
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let store = server_cache_store(sync_stores, cache_root, main_store, server_url.clone()).await?;
    sync_client::apply_bootstrap(&store, &server_url, &bootstrap).await
}

fn validate_external_url(value: &str) -> Result<url::Url, String> {
    let parsed = url::Url::parse(value).map_err(|error| error.to_string())?;
    let allowed = parsed.scheme() == "https"
        && matches!(
            parsed.host_str(),
            Some(
                "live.bilibili.com"
                    | "space.bilibili.com"
                    | "t.bilibili.com"
                    | "www.bilibili.com"
                    | "laplace.live"
            )
        );
    if !allowed {
        return Err(
            "只允许打开 B 站直播间、个人主页、动态、站内作品页面或 LAPLACE 魔法期页面，且必须使用 HTTPS"
                .to_owned(),
        );
    }
    Ok(parsed)
}

async fn remote_monitor_loop(
    app: AppHandle,
    server_url: String,
    token: String,
    store: oshical_core::SqliteStore,
    device_id: String,
) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            let _ = app.emit(
                "remote-status",
                json!({"connected": false, "connecting": false, "error": error.to_string()}),
            );
            return;
        }
    };
    let registration = sync_client::device_registration(device_id.clone());
    if let Err(error) =
        sync_client::register_device(&client, &server_url, &token, &registration).await
    {
        let _ = app.emit(
            "remote-status",
            json!({
                "connected": false,
                "connecting": true,
                "warning": format!("设备注册失败，继续尝试旧服务器兼容模式: {error}")
            }),
        );
    }

    let mut backoff = 1_u64;
    let mut legacy_mode = false;
    let mut legacy_seq = 0_i64;

    loop {
        let meta = match sync_client::get_sync_meta(&store, &server_url).await {
            Ok(meta) => meta,
            Err(error) => {
                let _ = app.emit(
                    "remote-status",
                    json!({"connected": false, "connecting": true, "error": error}),
                );
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
                continue;
            }
        };
        let after = if legacy_mode {
            legacy_seq
        } else {
            meta.cursor_seq
        };
        let _ = app.emit(
            "remote-status",
            json!({
                "connected": false,
                "connecting": true,
                "protocolVersion": 2,
                "deviceId": device_id,
                "lastSeq": after,
                "cursorSeq": meta.cursor_seq,
                "latestSeq": meta.latest_seq
            }),
        );

        let ws_url = match build_ws_url(&server_url, after, 2) {
            Ok(url) => url,
            Err(error) => {
                let _ = app.emit(
                    "remote-status",
                    json!({"connected": false, "connecting": false, "error": error}),
                );
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
                continue;
            }
        };

        match tokio_tungstenite::connect_async(ws_url.as_str()).await {
            Ok((mut socket, _)) => {
                backoff = 1;
                let auth = json!({
                    "type": "auth",
                    "token": token,
                    "deviceId": device_id,
                    "protocolVersion": 2
                });
                if socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        auth.to_string().into(),
                    ))
                    .await
                    .is_err()
                {
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(30);
                    continue;
                }

                let mut reconnect = false;
                let mut replay_through = 0_i64;
                loop {
                    match tokio::time::timeout(Duration::from_secs(45), socket.next()).await {
                        Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                            let Ok(mut event) = serde_json::from_str::<Value>(&text) else {
                                continue;
                            };
                            match event.get("type").and_then(Value::as_str) {
                                Some("auth_ok") => {
                                    let protocol = event
                                        .get("protocolVersion")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(1);
                                    replay_through =
                                        event.get("latestSeq").and_then(Value::as_i64).unwrap_or(0);
                                    if protocol >= 2 {
                                        legacy_mode = false;
                                        let current =
                                            match sync_client::get_sync_meta(&store, &server_url)
                                                .await
                                            {
                                                Ok(meta) => meta,
                                                Err(error) => {
                                                    let _ = app.emit(
                                                        "remote-status",
                                                        json!({
                                                            "connected": false,
                                                            "connecting": false,
                                                            "error": error
                                                        }),
                                                    );
                                                    break;
                                                }
                                            };
                                        let reset = event
                                            .get("resetRequired")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false)
                                            || !current.initialized;
                                        if reset {
                                            if let Err(error) = bootstrap_remote(
                                                &app,
                                                &client,
                                                &store,
                                                &server_url,
                                                &token,
                                                &device_id,
                                                current.cursor_seq,
                                            )
                                            .await
                                            {
                                                let _ = app.emit(
                                                    "remote-status",
                                                    json!({
                                                        "connected": false,
                                                        "connecting": false,
                                                        "error": error
                                                    }),
                                                );
                                            }
                                            reconnect = true;
                                            break;
                                        }
                                        emit_remote_connected(
                                            &app,
                                            &device_id,
                                            protocol,
                                            current.cursor_seq,
                                            replay_through,
                                        );
                                    } else {
                                        legacy_mode = true;
                                        legacy_seq = after;
                                        emit_remote_connected(
                                            &app,
                                            &device_id,
                                            1,
                                            after,
                                            replay_through,
                                        );
                                    }
                                }
                                Some("sync_batch") => {
                                    let batch = match serde_json::from_value::<sync_client::SyncBatch>(
                                        event.clone(),
                                    ) {
                                        Ok(batch) => batch,
                                        Err(error) => {
                                            let _ = app.emit(
                                                "remote-status",
                                                json!({
                                                    "connected": false,
                                                    "connecting": false,
                                                    "error": format!("sync_batch 无效: {error}")
                                                }),
                                            );
                                            break;
                                        }
                                    };
                                    if batch
                                        .changes
                                        .iter()
                                        .any(|change| change.entity_type == "sync_reset")
                                    {
                                        if let Err(error) = bootstrap_remote(
                                            &app,
                                            &client,
                                            &store,
                                            &server_url,
                                            &token,
                                            &device_id,
                                            current_cursor(&store, &server_url)
                                                .await
                                                .unwrap_or(after),
                                        )
                                        .await
                                        {
                                            let _ = app.emit(
                                                "remote-status",
                                                json!({
                                                    "connected": false,
                                                    "connecting": false,
                                                    "error": error
                                                }),
                                            );
                                            break;
                                        }
                                        reconnect = true;
                                        break;
                                    }
                                    let cursor = match apply_batch_with_catch_up(
                                        &app,
                                        &client,
                                        &store,
                                        &server_url,
                                        &token,
                                        &device_id,
                                        &batch,
                                        current_cursor(&store, &server_url).await.unwrap_or(after),
                                    )
                                    .await
                                    {
                                        Ok(cursor) => cursor,
                                        Err(error) => {
                                            if error.starts_with("SYNC_CACHE_INCONSISTENT:") {
                                                if let Err(rebuild_error) = bootstrap_remote(
                                                    &app,
                                                    &client,
                                                    &store,
                                                    &server_url,
                                                    &token,
                                                    &device_id,
                                                    current_cursor(&store, &server_url)
                                                        .await
                                                        .unwrap_or(after),
                                                )
                                                .await
                                                {
                                                    let _ = app.emit(
                                                        "remote-status",
                                                        json!({
                                                            "connected": false,
                                                            "connecting": false,
                                                            "error": rebuild_error
                                                        }),
                                                    );
                                                    break;
                                                }
                                                reconnect = true;
                                                break;
                                            }
                                            let _ = app.emit(
                                                "remote-status",
                                                json!({
                                                    "connected": false,
                                                    "connecting": false,
                                                    "error": error
                                                }),
                                            );
                                            break;
                                        }
                                    };
                                    let _ = socket
                                        .send(tokio_tungstenite::tungstenite::Message::Text(
                                            json!({"type": "ack", "seq": cursor})
                                                .to_string()
                                                .into(),
                                        ))
                                        .await;
                                    let _ = app.emit(
                                        "sync-applied",
                                        json!({
                                            "cursorSeq": cursor,
                                            "latestSeq": batch.latest_seq,
                                            "changeCount": batch.changes.len(),
                                            "changes": batch.changes
                                        }),
                                    );
                                    emit_remote_connected(
                                        &app,
                                        &device_id,
                                        2,
                                        cursor,
                                        batch.latest_seq,
                                    );
                                }
                                Some("reset_required") => {
                                    if let Err(error) = bootstrap_remote(
                                        &app,
                                        &client,
                                        &store,
                                        &server_url,
                                        &token,
                                        &device_id,
                                        current_cursor(&store, &server_url).await.unwrap_or(after),
                                    )
                                    .await
                                    {
                                        let _ = app.emit(
                                            "remote-status",
                                            json!({
                                                "connected": false,
                                                "connecting": false,
                                                "error": error
                                            }),
                                        );
                                    }
                                    reconnect = true;
                                    break;
                                }
                                Some("data_imported") => {
                                    if let Err(error) = bootstrap_remote(
                                        &app,
                                        &client,
                                        &store,
                                        &server_url,
                                        &token,
                                        &device_id,
                                        current_cursor(&store, &server_url).await.unwrap_or(after),
                                    )
                                    .await
                                    {
                                        let _ = app.emit(
                                            "remote-status",
                                            json!({
                                                "connected": false,
                                                "connecting": false,
                                                "error": error
                                            }),
                                        );
                                        break;
                                    }
                                    reconnect = true;
                                    break;
                                }
                                Some("job_progress") => emit_job_progress(&app, &event),
                                Some("ack") => {
                                    if let Some(seq) = event.get("seq").and_then(Value::as_i64) {
                                        let _ = sync_client::set_sync_ack(&store, &server_url, seq)
                                            .await;
                                    }
                                }
                                Some("error") => {
                                    let _ = app.emit(
                                        "remote-status",
                                        json!({
                                            "connected": false,
                                            "connecting": false,
                                            "error": "远程服务器鉴权失败"
                                        }),
                                    );
                                    return;
                                }
                                _ => {
                                    if let Some(seq) = event.get("seq").and_then(Value::as_i64) {
                                        if legacy_mode && seq <= legacy_seq {
                                            continue;
                                        }
                                        if legacy_mode {
                                            legacy_seq = seq;
                                        }
                                        event["replayed"] = json!(seq <= replay_through);
                                    }
                                    let _ = app.emit("server-event", event);
                                }
                            }
                        }
                        Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(payload)))) => {
                            let _ = socket
                                .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                                .await;
                        }
                        Ok(Some(Ok(_))) => {}
                        _ => break,
                    }
                }
                if reconnect {
                    continue;
                }
            }
            Err(error) => {
                let _ = app.emit(
                    "remote-status",
                    json!({
                        "connected": false,
                        "connecting": false,
                        "error": error.to_string(),
                        "lastSeq": after
                    }),
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn current_cursor(store: &oshical_core::SqliteStore, server_url: &str) -> Option<i64> {
    sync_client::get_sync_meta(store, server_url)
        .await
        .ok()
        .map(|meta| meta.cursor_seq)
}

fn emit_remote_connected(
    app: &AppHandle,
    device_id: &str,
    protocol: u64,
    cursor: i64,
    latest: i64,
) {
    let status = json!({
        "connected": true,
        "connecting": false,
        "protocolVersion": protocol,
        "deviceId": device_id,
        "cursorSeq": cursor,
        "lastSeq": cursor,
        "latestSeq": latest,
        "pending": latest.saturating_sub(cursor)
    });
    let _ = app.emit("remote-status", status.clone());
    let _ = app.emit("sync-status", status);
}

#[allow(clippy::too_many_arguments)]
async fn apply_batch_with_catch_up(
    app: &AppHandle,
    client: &reqwest::Client,
    store: &oshical_core::SqliteStore,
    server_url: &str,
    token: &str,
    device_id: &str,
    batch: &sync_client::SyncBatch,
    current_cursor: i64,
) -> Result<i64, String> {
    let needs_catch_up = batch
        .changes
        .iter()
        .find(|change| change.seq > current_cursor)
        .is_some_and(|change| change.seq != current_cursor + 1)
        || (batch.changes.is_empty() && batch.to_seq > current_cursor);
    if needs_catch_up {
        catch_up_changes(app, client, store, server_url, token, device_id).await?;
    }
    let result = match sync_client::apply_batch(store, server_url, batch).await {
        Ok(result) => result,
        Err(error) if error.starts_with("SYNC_GAP") => {
            catch_up_changes(app, client, store, server_url, token, device_id).await?;
            sync_client::apply_batch(store, server_url, batch).await?
        }
        Err(error) => return Err(error),
    };
    ack_remote(
        client,
        store,
        server_url,
        token,
        device_id,
        result.cursor_seq,
    )
    .await?;
    Ok(result.cursor_seq)
}

async fn catch_up_changes(
    app: &AppHandle,
    client: &reqwest::Client,
    store: &oshical_core::SqliteStore,
    server_url: &str,
    token: &str,
    device_id: &str,
) -> Result<i64, String> {
    loop {
        let meta = sync_client::get_sync_meta(store, server_url).await?;
        let page =
            sync_client::fetch_changes(client, server_url, token, meta.cursor_seq, 200).await?;
        if page.reset_required {
            return bootstrap_remote(
                app,
                client,
                store,
                server_url,
                token,
                device_id,
                meta.cursor_seq,
            )
            .await;
        }
        if page.changes.is_empty() {
            if page.has_more {
                return Err("服务器返回空变更页但仍有更多数据".to_owned());
            }
            ack_remote(client, store, server_url, token, device_id, meta.cursor_seq).await?;
            return Ok(meta.cursor_seq);
        }
        let batch = sync_client::SyncBatch {
            from_seq: page.from_seq,
            to_seq: page.to_seq,
            latest_seq: page.latest_seq,
            changes: page.changes,
        };
        let result = sync_client::apply_batch(store, server_url, &batch).await?;
        ack_remote(
            client,
            store,
            server_url,
            token,
            device_id,
            result.cursor_seq,
        )
        .await?;
        let _ = app.emit(
            "sync-applied",
            json!({
                "cursorSeq": result.cursor_seq,
                "latestSeq": result.latest_seq,
                "changeCount": result.applied,
                "recovered": true
            }),
        );
        if !page.has_more {
            return Ok(result.cursor_seq);
        }
    }
}

async fn bootstrap_remote(
    app: &AppHandle,
    client: &reqwest::Client,
    store: &oshical_core::SqliteStore,
    server_url: &str,
    token: &str,
    device_id: &str,
    previous_cursor: i64,
) -> Result<i64, String> {
    let _ = app.emit(
        "remote-status",
        json!({
            "connected": true,
            "connecting": false,
            "syncing": true,
            "cursorSeq": previous_cursor
        }),
    );
    let bootstrap = sync_client::fetch_bootstrap(client, server_url, token).await?;
    let result = sync_client::apply_bootstrap(store, server_url, &bootstrap).await?;
    ack_remote(
        client,
        store,
        server_url,
        token,
        device_id,
        result.cursor_seq,
    )
    .await?;
    let _ = app.emit(
        "sync-bootstrap-applied",
        json!({
            "cursorSeq": result.cursor_seq,
            "snapshotRevision": result.snapshot_revision,
            "entityCount": result.applied
        }),
    );
    emit_remote_connected(
        app,
        device_id,
        2,
        result.cursor_seq,
        bootstrap.snapshot_revision.max(previous_cursor),
    );
    Ok(result.cursor_seq)
}

async fn ack_remote(
    client: &reqwest::Client,
    store: &oshical_core::SqliteStore,
    server_url: &str,
    token: &str,
    device_id: &str,
    seq: i64,
) -> Result<(), String> {
    sync_client::post_ack(client, server_url, token, device_id, seq).await?;
    sync_client::set_sync_ack(store, server_url, seq).await?;
    Ok(())
}

fn emit_job_progress(app: &AppHandle, event: &Value) {
    let _ = app.emit("job-progress", event.clone());
    if let Some(streamer_id) = event.get("streamerId").and_then(Value::as_i64) {
        let progress = json!({
            "streamerId": streamer_id,
            "streamerName": event.get("streamerName").and_then(Value::as_str).unwrap_or(""),
            "stage": event.get("stage").and_then(Value::as_str).unwrap_or("刷新中"),
            "completed": event.get("completed").and_then(Value::as_u64).unwrap_or(0),
            "total": event.get("total").and_then(Value::as_u64).unwrap_or(0),
            "percent": event.get("percent").and_then(Value::as_u64).unwrap_or(0),
            "done": event.get("done").and_then(Value::as_bool).unwrap_or(false),
            "failed": event.get("failed").and_then(Value::as_bool).unwrap_or(false)
        });
        let _ = app.emit("refresh-progress", progress);
    }
}

fn build_ws_url(server_url: &str, after: i64, protocol: u32) -> Result<String, String> {
    let mut url = url::Url::parse(server_url).map_err(|error| error.to_string())?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => return Err(format!("不支持的服务器协议: {other}")),
    };
    url.set_scheme(scheme)
        .map_err(|_| "无法设置 WebSocket 协议".to_owned())?;
    let base_path = url.path().trim_end_matches('/');
    url.set_path(&format!("{base_path}/api/ws"));
    url.set_query(Some(&format!(
        "after={}&protocol={}",
        after.max(0),
        protocol
    )));
    Ok(url.to_string())
}
#[tauri::command]
async fn export_client_snapshot(
    state: State<'_, DesktopState>,
) -> Result<oshical_core::store::ServerSnapshot, String> {
    let store = state.service.store();
    Ok(oshical_core::store::ServerSnapshot {
        subscriptions: store.list_streamers().await.map_err(map_error)?,
        schedules: store.list_all_schedules().await.map_err(map_error)?,
        weekly_rules: store.list_weekly_rules(None).await.map_err(map_error)?,
        latest_event_seq: store.latest_event_seq().await.map_err(map_error)?,
        exported_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        app_update_endpoints, build_ws_url, cookie_header_from_webview, normalize_mirror_base,
        validate_external_url,
    };
    use tauri::webview::Cookie;

    #[test]
    fn websocket_url_preserves_server_path_prefix() {
        assert_eq!(
            build_ws_url("https://example.com/oshical", 12, 2).unwrap(),
            "wss://example.com/oshical/api/ws?after=12&protocol=2"
        );
        assert_eq!(
            build_ws_url("https://example.com/", 0, 2).unwrap(),
            "wss://example.com/api/ws?after=0&protocol=2"
        );
    }

    #[test]
    fn app_update_endpoints_prefer_mirror_then_direct() {
        let endpoints = app_update_endpoints(Some("https://gh-proxy.com")).expect("endpoints");
        assert_eq!(endpoints.len(), 2);
        assert_eq!(
            endpoints[0].as_str(),
            "https://gh-proxy.com/https://github.com/yueTc/OshiCal/releases/latest/download/latest.json"
        );
        assert_eq!(
            endpoints[1].as_str(),
            "https://github.com/yueTc/OshiCal/releases/latest/download/latest.json"
        );
        assert_eq!(normalize_mirror_base(Some("")).unwrap(), None);
        assert!(app_update_endpoints(Some("ftp://example.com")).is_err());
    }

    #[test]
    fn webview_cookie_header_only_uses_bilibili_cookies() {
        let cookies = vec![
            Cookie::build(("SESSDATA", "sess"))
                .domain(".bilibili.com")
                .build(),
            Cookie::build(("bili_jct", "csrf"))
                .domain(".bilibili.com")
                .build(),
            Cookie::build(("DedeUserID", "42"))
                .domain("passport.bilibili.com")
                .build(),
            Cookie::build(("SESSDATA", "wrong-domain"))
                .domain(".example.com")
                .build(),
        ];
        assert_eq!(
            cookie_header_from_webview(&cookies).as_deref(),
            Some("SESSDATA=sess; bili_jct=csrf; DedeUserID=42")
        );
        assert_eq!(
            cookie_header_from_webview(&[Cookie::build(("bili_jct", "csrf"))
                .domain(".bilibili.com")
                .build()]),
            None
        );
    }

    #[test]
    fn external_urls_allow_bilibili_sources_only() {
        assert!(validate_external_url("https://live.bilibili.com/123").is_ok());
        assert!(validate_external_url("https://space.bilibili.com/123").is_ok());
        assert!(validate_external_url("https://t.bilibili.com/123").is_ok());
        assert!(validate_external_url("https://www.bilibili.com/opus/123").is_ok());
        assert!(validate_external_url("http://space.bilibili.com/123").is_err());
        assert!(validate_external_url("http://t.bilibili.com/123").is_err());
        assert!(validate_external_url("https://example.com/123").is_err());
        assert!(validate_external_url("https://t.bilibili.com@example.com/123").is_err());
    }
}
