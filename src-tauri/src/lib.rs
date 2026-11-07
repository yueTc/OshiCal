mod sync_client;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use oshical_core::{
    load_settings, save_settings as save_settings_to_store, AppError, AppService, AppSettings,
    DataMode, FanHistory, FertilitySnapshot, LlmConfig, LlmDiscoveryConfig, LlmTestReport,
    NewSchedule, NewWeeklyScheduleRule, ProxyTestInput, ProxyTestReport, RuntimeSecrets, Schedule,
    Streamer, SyncMutation, UpdateSchedule, UpdateSubscription, UpdateWeeklyScheduleRule,
    WeeklyScheduleRule, FAN_POLL_INTERVAL_SECONDS, FERTILITY_POLL_INTERVAL_SECONDS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
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
const APP_UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(45);
const APP_UPDATE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
struct RemoteMonitorState {
    generation: u64,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
    cancel_rebuild: Option<Arc<AtomicBool>>,
}

impl RemoteMonitorState {
    fn begin_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        if let Some(cancel) = self.cancel_rebuild.take() {
            cancel.store(true, AtomicOrdering::Release);
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation
    }

    fn set_rebuild_cancel(&mut self, generation: u64, cancel: Arc<AtomicBool>) -> bool {
        if !self.is_current(generation) {
            cancel.store(true, AtomicOrdering::Release);
            return false;
        }
        self.cancel_rebuild = Some(cancel);
        true
    }

    fn clear_rebuild_cancel(&mut self, generation: u64) {
        if self.is_current(generation) {
            self.cancel_rebuild = None;
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn stop_if_current(&mut self, expected_generation: u64) -> bool {
        if !self.is_current(expected_generation) {
            return false;
        }
        self.begin_generation();
        true
    }

    fn install_if_current(
        &mut self,
        generation: u64,
        task: tauri::async_runtime::JoinHandle<()>,
    ) -> bool {
        if !self.is_current(generation) {
            task.abort();
            return false;
        }
        if let Some(previous) = self.task.replace(task) {
            previous.abort();
        }
        true
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteMonitorSession {
    generation: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteRebuildResult {
    cursor_seq: i64,
    monitor_generation: u64,
}

struct DesktopState {
    service: Arc<AppService>,
    remote_monitor: Arc<Mutex<RemoteMonitorState>>,
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
                remote_monitor: Arc::new(Mutex::new(RemoteMonitorState::default())),
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

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开推历 OshiCal", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh", "立即刷新", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &refresh, &quit])?;
    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("推历 OshiCal · 直播日程")
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
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
    if url.scheme() != "https" {
        return Err("GitHub 镜像地址必须使用 https".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("GitHub 镜像地址不能包含用户名或密码".to_owned());
    }
    if url.query().is_some() {
        return Err("GitHub 镜像地址不能包含 query 参数".to_owned());
    }
    if url.fragment().is_some() {
        return Err("GitHub 镜像地址不能包含 fragment".to_owned());
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
    // Invalid mirror configuration must not disable the built-in direct path.
    let Some(base) = normalize_mirror_base(mirror_url).ok().flatten() else {
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
        .timeout(APP_UPDATE_CHECK_TIMEOUT)
        .build()
        .map_err(|error| format!("初始化更新检查失败: {error}"))?;
    let mut update = updater
        .check()
        .await
        .map_err(|error| format!("检查更新失败: {error}"))?;
    let mirror_base = normalize_mirror_base(mirror_url).ok().flatten();
    if let Some(update) = update.as_mut() {
        if let Some(base) = mirror_base.as_deref() {
            if update.download_url.scheme() == "https"
                && !update.download_url.as_str().starts_with(base)
            {
                update.download_url = url::Url::parse(&format!("{base}{}", update.download_url))
                    .map_err(|error| format!("镜像下载地址无效: {error}"))?;
            }
        }
        // The updater check timeout also applies to the entire package download. A slow
        // GitHub mirror can take several minutes to transfer a release installer.
        update.timeout = Some(APP_UPDATE_DOWNLOAD_TIMEOUT);
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
            let configured_mirror = normalize_mirror_base(mirror_url.as_deref())
                .ok()
                .flatten()
                .is_some();
            if !configured_mirror {
                return Err(first_error);
            }
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
async fn check_live_now(
    state: State<'_, DesktopState>,
) -> Result<Vec<oshical_core::LiveStatus>, String> {
    state.service.check_live_all().await.map_err(map_error)
}

/// 「寻血猎犬」开关关闭时只返回空快照，不读取缓存也不发起网络请求。
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

fn is_bilibili_cookie_domain(domain: &str) -> bool {
    domain == "bilibili.com"
        || domain.ends_with(".bilibili.com")
        || domain == "biligame.com"
        || domain.ends_with(".biligame.com")
}

async fn clear_bilibili_webview_cookies(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("bilibili-login") {
        let _ = window.close();
    }
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };
    tauri::async_runtime::spawn_blocking(move || {
        let cookies = window
            .cookies()
            .map_err(|error| format!("读取 B 站登录 Cookie 失败: {error}"))?;
        for cookie in cookies {
            if !is_bilibili_cookie_domain(cookie.domain().unwrap_or_default()) {
                continue;
            }
            window
                .delete_cookie(cookie)
                .map_err(|error| format!("清除 B 站登录 Cookie 失败: {error}"))?;
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|error| format!("清理 B 站登录 Cookie 任务失败: {error}"))?
}

#[tauri::command]
async fn clear_bilibili_cookie(
    app: AppHandle,
    state: State<'_, DesktopState>,
) -> Result<SecretStatus, String> {
    clear_bilibili_webview_cookies(&app).await?;
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
        if !is_bilibili_cookie_domain(cookie.domain().unwrap_or_default()) {
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

    let fertility_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut previous = None;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let state = fertility_app.state::<DesktopState>();
            let settings = match load_settings(state.service.store()).await {
                Ok(settings) => settings,
                Err(error) => {
                    let _ = fertility_app.emit("monitor-error", error.to_string());
                    continue;
                }
            };
            if settings.data_mode != DataMode::Local || !settings.fertility_enabled {
                // 关闭开关时重置计时，重新开启后立即触发一次采集。
                previous = None;
                continue;
            }
            if previous.is_none_or(|started: Instant| {
                started.elapsed() >= Duration::from_secs(FERTILITY_POLL_INTERVAL_SECONDS)
            }) {
                previous = Some(Instant::now());
                match state.service.refresh_fertility_if_enabled().await {
                    Ok(Some(_)) => {
                        let _ = fertility_app.emit("fertility-updated", Utc::now().to_rfc3339());
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = fertility_app.emit("monitor-error", error.to_string());
                    }
                }
            }
        }
    });

    let pump_app = app;
    tauri::async_runtime::spawn(async move {
        let mut last_seq = loop {
            let state = pump_app.state::<DesktopState>();
            match state.service.store().latest_event_seq().await {
                Ok(seq) => break seq,
                Err(error) => {
                    let _ = pump_app.emit(
                        "monitor-error",
                        format!("读取本地事件游标失败，稍后重试: {error}"),
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let state = pump_app.state::<DesktopState>();
            match state.service.store().events_after(last_seq, 200).await {
                Ok(events) => {
                    for event in events {
                        if let Some(seq) = event.get("seq").and_then(Value::as_i64) {
                            last_seq = last_seq.max(seq);
                        }
                        let _ = pump_app.emit("server-event", event);
                    }
                }
                Err(error) => {
                    let _ = pump_app.emit(
                        "monitor-error",
                        format!("读取本地事件失败，稍后重试: {error}"),
                    );
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
) -> Result<RemoteMonitorSession, String> {
    url::Url::parse(&server_url).map_err(|error| format!("服务器地址无效: {error}"))?;
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let remote_monitor = state.remote_monitor.clone();
    let generation = {
        let mut runtime = remote_monitor.lock().await;
        runtime.begin_generation()
    };
    let monitor_store = server_cache_store(
        sync_stores,
        cache_root,
        main_store.clone(),
        server_url.clone(),
    )
    .await?;
    let device_id = sync_client::device_id(&main_store).await?;
    let mut runtime = remote_monitor.lock().await;
    if !runtime.is_current(generation) {
        return Err("远端监控启动已被更新的操作取消".to_owned());
    }
    let task = tauri::async_runtime::spawn(remote_monitor_loop(
        app,
        server_url,
        token,
        monitor_store,
        device_id,
    ));
    if !runtime.install_if_current(generation, task) {
        return Err("远端监控启动已被更新的操作取消".to_owned());
    }
    Ok(RemoteMonitorSession { generation })
}

#[tauri::command]
async fn rebuild_remote_sync(
    app: AppHandle,
    state: State<'_, DesktopState>,
    server_url: String,
    token: String,
) -> Result<RemoteRebuildResult, String> {
    url::Url::parse(&server_url).map_err(|error| format!("服务器地址无效: {error}"))?;
    let sync_stores = state.sync_stores.clone();
    let cache_root = state.server_cache_root.clone();
    let main_store = state.service.store().clone();
    let remote_monitor = state.remote_monitor.clone();
    let rebuild_cancel = Arc::new(AtomicBool::new(false));
    let generation = {
        let mut runtime = remote_monitor.lock().await;
        let generation = runtime.begin_generation();
        runtime.set_rebuild_cancel(generation, rebuild_cancel.clone());
        generation
    };
    let monitor_store = server_cache_store(
        sync_stores,
        cache_root,
        main_store.clone(),
        server_url.clone(),
    )
    .await?;
    let device_id = sync_client::device_id(&main_store).await?;
    if !remote_monitor.lock().await.is_current(generation) {
        return Err("远端同步重建已被更新的操作取消".to_owned());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let previous_cursor = current_cursor(&monitor_store, &server_url)
        .await
        .unwrap_or_default();
    let cursor = bootstrap_remote_checked(
        &app,
        &client,
        &monitor_store,
        &server_url,
        &token,
        &device_id,
        previous_cursor,
        Some(rebuild_cancel.clone()),
    )
    .await?;
    let mut runtime = remote_monitor.lock().await;
    if !runtime.is_current(generation) {
        return Err("远端同步重建已被更新的操作取消".to_owned());
    }
    let task = tauri::async_runtime::spawn(remote_monitor_loop(
        app,
        server_url,
        token,
        monitor_store,
        device_id,
    ));
    runtime.clear_rebuild_cancel(generation);
    if !runtime.install_if_current(generation, task) {
        return Err("远端同步重建已被更新的操作取消".to_owned());
    }
    Ok(RemoteRebuildResult {
        cursor_seq: cursor,
        monitor_generation: generation,
    })
}

#[tauri::command]
async fn stop_remote_monitor(
    state: State<'_, DesktopState>,
    expected_generation: u64,
) -> Result<bool, String> {
    let mut runtime = state.remote_monitor.lock().await;
    Ok(runtime.stop_if_current(expected_generation))
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
        let (remote_id, version) = if server_mode {
            (schedule.id, Some(schedule.updated_at))
        } else {
            main_store
                .remote_entity(
                    settings.server_url.trim_end_matches('/'),
                    "schedule",
                    schedule.id,
                )
                .await
                .map_err(map_error)?
                .unwrap_or((schedule.id, None))
        };
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
            "只允许打开 B 站直播间、个人主页、动态、站内作品页面或 LAPLACE 生理期页面，且必须使用 HTTPS"
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
    let mut legacy_seq = -1_i64;

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
            legacy_seq.max(meta.legacy_cursor_seq).max(0)
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
                "feedId": &meta.feed_id,
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
                                    let reported_latest =
                                        event.get("latestSeq").and_then(Value::as_i64);
                                    replay_through = reported_latest.unwrap_or(0);
                                    if protocol >= 2 {
                                        let resumed_from_legacy = legacy_mode;
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
                                        let remote_feed_id = event
                                            .get("feedId")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .trim()
                                            .to_owned();
                                        let reset = event
                                            .get("resetRequired")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false)
                                            || !current.initialized
                                            || replay_through < current.latest_seq
                                            || sync_client::sync_feed_changed(
                                                &current.feed_id,
                                                current.cursor_seq,
                                                &remote_feed_id,
                                            );
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
                                        let current_acked = match ack_remote(
                                            &client,
                                            &store,
                                            &server_url,
                                            &token,
                                            &device_id,
                                            current.cursor_seq,
                                        )
                                        .await
                                        {
                                            Ok(meta) => meta.last_ack_seq,
                                            Err(error) => {
                                                let _ = app.emit(
                                                    "remote-status",
                                                    json!({
                                                        "connected": false,
                                                        "connecting": false,
                                                        "error": format!(
                                                            "同步 ACK 持久化失败: {error}"
                                                        )
                                                    }),
                                                );
                                                break;
                                            }
                                        };
                                        emit_remote_connected(
                                            &app,
                                            &device_id,
                                            protocol,
                                            current.cursor_seq,
                                            reported_latest
                                                .unwrap_or(current.latest_seq)
                                                .max(current.cursor_seq),
                                            current_acked,
                                            if remote_feed_id.is_empty() {
                                                current.feed_id.as_str()
                                            } else {
                                                remote_feed_id.as_str()
                                            },
                                        );
                                        // The V1 connection used an event-journal cursor
                                        // in the query string. Reconnect before accepting
                                        // V2 sync batches so they start from the sync cursor.
                                        if resumed_from_legacy && after != current.cursor_seq {
                                            reconnect = true;
                                            break;
                                        }
                                    } else {
                                        legacy_mode = true;
                                        let legacy_meta =
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
                                        let initialized = legacy_meta.legacy_cursor_seq >= 0;
                                        legacy_seq = legacy_meta.legacy_cursor_seq.max(0);
                                        let journal_regressed = initialized
                                            && reported_latest
                                                .is_some_and(|latest| latest < legacy_seq);
                                        if journal_regressed {
                                            if let Err(error) =
                                                sync_client::reset_legacy_event_cursor(
                                                    &store,
                                                    &server_url,
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
                                            legacy_seq = 0;
                                        } else if !initialized {
                                            if let Err(error) =
                                                sync_client::set_legacy_event_cursor(
                                                    &store,
                                                    &server_url,
                                                    0,
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
                                            legacy_seq = 0;
                                        }
                                        emit_remote_connected(
                                            &app,
                                            &device_id,
                                            1,
                                            legacy_seq,
                                            replay_through,
                                            legacy_meta.last_ack_seq,
                                            "",
                                        );
                                        // The first V1 connection may have used the V2
                                        // sync cursor in its query string. Reconnect with
                                        // the independent event-journal cursor.
                                        if journal_regressed || !initialized || after != legacy_seq
                                        {
                                            reconnect = true;
                                            break;
                                        }
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
                                    )
                                    .await
                                    {
                                        Ok(cursor) => cursor,
                                        Err(error) => {
                                            if error.starts_with("SYNC_CACHE_INCONSISTENT:")
                                                || error.starts_with("SYNC_FEED_CHANGED:")
                                            {
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
                                    // Catch-up may have applied changes beyond this
                                    // queued batch. Re-read local metadata so a late
                                    // batch cannot publish a regressed latestSeq.
                                    let (connected_latest, connected_acked, connected_feed_id) =
                                        match sync_client::get_sync_meta(&store, &server_url).await
                                        {
                                            Ok(meta) => (
                                                meta.latest_seq.max(cursor),
                                                meta.last_ack_seq,
                                                if meta.feed_id.trim().is_empty() {
                                                    batch.feed_id.clone()
                                                } else {
                                                    meta.feed_id
                                                },
                                            ),
                                            Err(_) => (
                                                batch.latest_seq.max(cursor),
                                                cursor,
                                                batch.feed_id.clone(),
                                            ),
                                        };
                                    emit_remote_connected(
                                        &app,
                                        &device_id,
                                        2,
                                        cursor,
                                        connected_latest,
                                        connected_acked,
                                        &connected_feed_id,
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
                                    if event.get("persisted").and_then(Value::as_bool) == Some(true)
                                    {
                                        if let Some(seq) = event.get("seq").and_then(Value::as_i64)
                                        {
                                            let _ =
                                                sync_client::set_sync_ack(&store, &server_url, seq)
                                                    .await;
                                        }
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
                                    if !prepare_forwarded_event(
                                        legacy_mode,
                                        &mut legacy_seq,
                                        replay_through,
                                        &mut event,
                                    ) {
                                        continue;
                                    }
                                    if legacy_mode {
                                        if let Some(seq) = event.get("seq").and_then(Value::as_i64)
                                        {
                                            if let Err(error) =
                                                sync_client::advance_legacy_event_cursor(
                                                    &store,
                                                    &server_url,
                                                    seq,
                                                )
                                                .await
                                            {
                                                let _ = app.emit(
                                                    "monitor-error",
                                                    format!("旧版服务器事件游标保存失败: {error}"),
                                                );
                                            }
                                        }
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
    acked_seq: i64,
    feed_id: &str,
) {
    let status = json!({
        "connected": true,
        "connecting": false,
        "protocolVersion": protocol,
        "deviceId": device_id,
        "feedId": feed_id,
        "cursorSeq": cursor,
        "lastSeq": cursor,
        "latestSeq": latest,
        "ackedSeq": acked_seq.min(cursor),
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
) -> Result<i64, String> {
    let meta = sync_client::get_sync_meta(store, server_url).await?;
    sync_client::ensure_sync_feed_compatible(&meta.feed_id, meta.cursor_seq, &batch.feed_id)?;
    if batch_is_fully_applied(batch, meta.cursor_seq) {
        if batch.latest_seq <= meta.latest_seq {
            return Ok(meta.cursor_seq);
        }
        return catch_up_changes(app, client, store, server_url, token, device_id).await;
    }
    let needs_catch_up = batch
        .changes
        .iter()
        .find(|change| change.seq > meta.cursor_seq)
        .is_some_and(|change| change.seq != meta.cursor_seq + 1)
        || (batch.changes.is_empty() && batch.to_seq > meta.cursor_seq);
    if needs_catch_up {
        catch_up_changes(app, client, store, server_url, token, device_id).await?;
        let recovered_meta = sync_client::get_sync_meta(store, server_url).await?;
        if batch_is_fully_applied(batch, recovered_meta.cursor_seq) {
            // catch_up_changes already emitted and ACKed every applied page.
            // This queued WebSocket batch is now stale, so do not re-emit,
            // ACK it again, or treat its older latestSeq as a regression.
            return Ok(recovered_meta.cursor_seq);
        }
    }
    let mut cursor_before_batch = sync_client::get_sync_meta(store, server_url)
        .await?
        .cursor_seq;
    let result = match sync_client::apply_batch(store, server_url, batch).await {
        Ok(result) => result,
        Err(error) if error.starts_with("SYNC_GAP") => {
            catch_up_changes(app, client, store, server_url, token, device_id).await?;
            let recovered_meta = sync_client::get_sync_meta(store, server_url).await?;
            if batch_is_fully_applied(batch, recovered_meta.cursor_seq) {
                return Ok(recovered_meta.cursor_seq);
            }
            cursor_before_batch = recovered_meta.cursor_seq;
            sync_client::apply_batch(store, server_url, batch).await?
        }
        Err(error) => return Err(error),
    };
    if result.applied > 0 {
        let applied_changes = changes_after_cursor(&batch.changes, cursor_before_batch);
        let _ = app.emit(
            "sync-applied",
            json!({
                "feedId": &batch.feed_id,
                "cursorSeq": result.cursor_seq,
                "latestSeq": result.latest_seq,
                "changeCount": result.applied,
                "changes": applied_changes,
                "recovered": false
            }),
        );
    }
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
        sync_client::ensure_sync_schema_version(page.schema_version)?;
        if sync_client::ensure_sync_feed_compatible(&meta.feed_id, meta.cursor_seq, &page.feed_id)
            .is_err()
        {
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
        if page.reset_required
            || page.latest_seq < meta.cursor_seq
            || page.latest_seq < meta.latest_seq
        {
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
        let expected_from = meta.cursor_seq.saturating_add(1);
        if page.changes.is_empty() {
            if page.has_more {
                return Err("服务器返回空变更页但仍有更多数据".to_owned());
            }
            if page.from_seq != expected_from
                || page.to_seq != meta.cursor_seq
                || empty_changes_page_requires_rebuild(
                    meta.cursor_seq,
                    meta.latest_seq,
                    page.latest_seq,
                )
            {
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
            ack_remote(client, store, server_url, token, device_id, meta.cursor_seq).await?;
            return Ok(meta.cursor_seq);
        }
        let expected_to = page
            .changes
            .last()
            .map(|change| change.seq)
            .unwrap_or(meta.cursor_seq);
        let contiguous = page
            .changes
            .first()
            .is_some_and(|change| change.seq == expected_from)
            && page
                .changes
                .windows(2)
                .all(|pair| pair[1].seq == pair[0].seq.saturating_add(1));
        if page.from_seq != expected_from
            || page.to_seq != expected_to
            || page.latest_seq < expected_to
            || !contiguous
        {
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
        let batch = sync_client::SyncBatch {
            from_seq: page.from_seq,
            to_seq: page.to_seq,
            latest_seq: page.latest_seq,
            feed_id: page.feed_id,
            changes: page.changes,
        };
        let result = sync_client::apply_batch(store, server_url, &batch).await?;
        if result.applied > 0 {
            let applied_changes = changes_after_cursor(&batch.changes, meta.cursor_seq);
            let _ = app.emit(
                "sync-applied",
                json!({
                    "feedId": &batch.feed_id,
                    "cursorSeq": result.cursor_seq,
                    "latestSeq": result.latest_seq,
                    "changeCount": result.applied,
                    "changes": applied_changes,
                    "recovered": true
                }),
            );
        }
        ack_remote(
            client,
            store,
            server_url,
            token,
            device_id,
            result.cursor_seq,
        )
        .await?;
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
    bootstrap_remote_checked(
        app,
        client,
        store,
        server_url,
        token,
        device_id,
        previous_cursor,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn bootstrap_remote_checked(
    app: &AppHandle,
    client: &reqwest::Client,
    store: &oshical_core::SqliteStore,
    server_url: &str,
    token: &str,
    device_id: &str,
    previous_cursor: i64,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<i64, String> {
    let cancelled = || {
        cancel
            .as_ref()
            .is_some_and(|flag| flag.load(AtomicOrdering::Acquire))
    };
    if cancelled() {
        return Err("远端同步重建已取消".to_owned());
    }
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
    if cancelled() {
        return Err("远端同步重建已取消".to_owned());
    }
    let result = sync_client::apply_bootstrap(store, server_url, &bootstrap).await?;
    if cancelled() {
        return Err("远端同步重建已取消".to_owned());
    }
    let _ = app.emit(
        "sync-bootstrap-applied",
        json!({
            "feedId": &bootstrap.feed_id,
            "cursorSeq": result.cursor_seq,
            "snapshotRevision": result.snapshot_revision,
            "entityCount": result.applied
        }),
    );
    if cancelled() {
        return Err("远端同步重建已取消".to_owned());
    }
    let sync_meta = ack_remote(
        client,
        store,
        server_url,
        token,
        device_id,
        result.cursor_seq,
    )
    .await?;
    if cancelled() {
        return Err("远端同步重建已取消".to_owned());
    }
    emit_remote_connected(
        app,
        device_id,
        2,
        result.cursor_seq,
        sync_meta.latest_seq.max(result.cursor_seq),
        sync_meta.last_ack_seq,
        &bootstrap.feed_id,
    );
    Ok(result.cursor_seq)
}

fn batch_is_fully_applied(batch: &sync_client::SyncBatch, cursor: i64) -> bool {
    batch.to_seq <= cursor && batch.changes.iter().all(|change| change.seq <= cursor)
}

fn empty_changes_page_requires_rebuild(cursor: i64, local_latest: i64, page_latest: i64) -> bool {
    page_latest < cursor || page_latest < local_latest || page_latest > cursor
}

fn changes_after_cursor(
    changes: &[sync_client::SyncChange],
    cursor: i64,
) -> Vec<&sync_client::SyncChange> {
    changes
        .iter()
        .filter(|change| change.seq > cursor)
        .collect()
}

async fn ack_remote(
    client: &reqwest::Client,
    store: &oshical_core::SqliteStore,
    server_url: &str,
    token: &str,
    device_id: &str,
    seq: i64,
) -> Result<sync_client::SyncMeta, String> {
    let response = sync_client::post_ack(client, server_url, token, device_id, seq).await?;
    let confirmed_device = response
        .get("deviceId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if confirmed_device != device_id.trim() {
        return Err("服务器 ACK 确认的设备不匹配".to_owned());
    }
    let confirmed_seq = response
        .get("lastAckSeq")
        .and_then(Value::as_i64)
        .ok_or_else(|| "服务器未确认 ACK 已持久化".to_owned())?;
    if confirmed_seq < seq {
        return Err(format!("服务器仅确认到 {confirmed_seq}，尚未持久化 {seq}"));
    }
    sync_client::set_sync_ack(store, server_url, seq).await
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

fn prepare_forwarded_event(
    legacy_mode: bool,
    legacy_seq: &mut i64,
    replay_through: i64,
    event: &mut Value,
) -> bool {
    // V2 event-journal sequence numbers are independent from the sync cursor.
    // Only legacy replay windows may mark an event as replayed.
    if !legacy_mode {
        return true;
    }
    let Some(seq) = event.get("seq").and_then(Value::as_i64) else {
        return true;
    };
    if seq <= *legacy_seq {
        return false;
    }
    *legacy_seq = seq;
    event["replayed"] = json!(seq <= replay_through);
    true
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
    let bootstrap = store.sync_bootstrap().await.map_err(map_error)?;
    Ok(oshical_core::store::ServerSnapshot {
        schema_version: oshical_core::store::SERVER_SNAPSHOT_SCHEMA_VERSION,
        subscriptions: bootstrap.subscriptions,
        schedules: bootstrap.schedules,
        weekly_rules: bootstrap.weekly_rules,
        weekly_exceptions: bootstrap.weekly_exceptions,
        schedule_sources: bootstrap.schedule_sources,
        source_suppressions: bootstrap.source_suppressions,
        latest_event_seq: store.latest_event_seq().await.map_err(map_error)?,
        exported_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        app_update_endpoints, batch_is_fully_applied, build_ws_url, changes_after_cursor,
        cookie_header_from_webview, empty_changes_page_requires_rebuild, normalize_mirror_base,
        prepare_forwarded_event, validate_external_url, RemoteMonitorState,
    };
    use crate::sync_client::SyncChange;
    use serde_json::json;
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
    fn stale_monitor_stop_cannot_stop_new_generation() {
        let mut state = RemoteMonitorState::default();
        let first = state.begin_generation();
        let second = state.begin_generation();
        assert!(!state.stop_if_current(first));
        assert!(state.is_current(second));
        assert!(state.stop_if_current(second));
        assert!(!state.is_current(second));
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
        assert_eq!(
            normalize_mirror_base(Some("https://gh-proxy.com")).unwrap(),
            Some("https://gh-proxy.com/".to_owned())
        );
        assert!(normalize_mirror_base(Some("http://gh-proxy.com")).is_err());
        assert!(normalize_mirror_base(Some("https://user@gh-proxy.com")).is_err());
        assert!(normalize_mirror_base(Some("https://user:pass@gh-proxy.com")).is_err());
        assert!(normalize_mirror_base(Some("https://gh-proxy.com?token=secret")).is_err());
        assert!(normalize_mirror_base(Some("https://gh-proxy.com#fragment")).is_err());

        let direct_only =
            app_update_endpoints(Some("http://gh-proxy.com")).expect("direct fallback");
        assert_eq!(direct_only.len(), 1);
        assert_eq!(
            direct_only[0].as_str(),
            "https://github.com/yueTc/OshiCal/releases/latest/download/latest.json"
        );
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
    fn catch_up_can_identify_an_already_applied_batch() {
        let batch = crate::sync_client::SyncBatch {
            from_seq: 10,
            to_seq: 12,
            latest_seq: 20,
            feed_id: "feed-test".to_owned(),
            changes: vec![
                SyncChange {
                    seq: 11,
                    entity_type: "schedule".to_owned(),
                    entity_id: "11".to_owned(),
                    operation: "upsert".to_owned(),
                    entity_revision: 1,
                    payload: None,
                    created_at: String::new(),
                },
                SyncChange {
                    seq: 12,
                    entity_type: "schedule".to_owned(),
                    entity_id: "12".to_owned(),
                    operation: "upsert".to_owned(),
                    entity_revision: 1,
                    payload: None,
                    created_at: String::new(),
                },
            ],
        };
        assert!(batch_is_fully_applied(&batch, 12));
        assert!(!batch_is_fully_applied(&batch, 11));
    }

    #[test]
    fn empty_changes_page_rebuilds_for_gaps_or_regressions() {
        assert!(!empty_changes_page_requires_rebuild(20, 20, 20));
        assert!(empty_changes_page_requires_rebuild(20, 0, 19));
        assert!(empty_changes_page_requires_rebuild(20, 30, 20));
        assert!(empty_changes_page_requires_rebuild(20, 20, 30));
        assert!(empty_changes_page_requires_rebuild(20, 30, 25));
    }

    #[test]
    fn sync_applied_payload_excludes_changes_already_recovered() {
        let changes = vec![
            SyncChange {
                seq: 10,
                entity_type: "schedule".to_owned(),
                entity_id: "10".to_owned(),
                operation: "upsert".to_owned(),
                entity_revision: 1,
                payload: None,
                created_at: String::new(),
            },
            SyncChange {
                seq: 11,
                entity_type: "schedule".to_owned(),
                entity_id: "11".to_owned(),
                operation: "upsert".to_owned(),
                entity_revision: 1,
                payload: None,
                created_at: String::new(),
            },
        ];
        let pending = changes_after_cursor(&changes, 10);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, 11);
    }

    #[test]
    fn websocket_v2_live_status_is_not_marked_replayed() {
        let mut event = json!({
            "seq": 1,
            "type": "live_status",
            "payload": {"previousIsLive": false, "isLive": true}
        });
        let mut legacy_seq = 0;
        assert!(prepare_forwarded_event(
            false,
            &mut legacy_seq,
            51_688,
            &mut event
        ));
        assert!(event.get("replayed").is_none());
        assert_eq!(legacy_seq, 0);
    }

    #[test]
    fn websocket_legacy_event_uses_event_journal_replay_window() {
        let mut event = json!({"seq": 8, "type": "live_status"});
        let mut legacy_seq = 7;
        assert!(prepare_forwarded_event(
            true,
            &mut legacy_seq,
            8,
            &mut event
        ));
        assert_eq!(event["replayed"], true);
        assert_eq!(legacy_seq, 8);

        let mut duplicate = json!({"seq": 8, "type": "live_status"});
        assert!(!prepare_forwarded_event(
            true,
            &mut legacy_seq,
            8,
            &mut duplicate
        ));
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
