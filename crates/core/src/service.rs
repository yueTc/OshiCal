use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};

use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, Timelike, Utc};
use futures_util::{stream::FuturesUnordered, StreamExt};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use tracing::Instrument;

use crate::analysis_retry;
use crate::bilibili::{BilibiliClient, QrLoginPoll, QrLoginStart};
use crate::error::{AppError, AppResult};
use crate::fertility::{
    parse_fertility_payload, FertilityClient, FertilityResponse, FERTILITY_CACHE_TTL_HOURS,
};
use crate::live_stats::LiveStatsManager;
use crate::llm::{
    events_from_raw, llm_base_urls_match, ContentAnalyzer, LlmClient, LlmConfig,
    LlmDiscoveryConfig, LlmEvent, LlmEventAction, LlmPool, LlmTestReport, PostAnalysisInput,
    POST_ANALYSIS_IMAGE_LIMIT,
};
use crate::models::{
    AddSubscription, DynamicPost, FanHistory, FanRefreshReport, FertilitySnapshot, FertilityStatus,
    LiveSnapshot, LiveStatus, NewSchedule, NewWeeklyScheduleRule, Platform, RefreshProgress,
    RefreshReport, Schedule, Streamer, UpdateSchedule, UpdateSubscription,
    UpdateWeeklyScheduleRule, WeeklyScheduleRule,
};
use crate::network::{test_google, ProxyTestReport};
use crate::refresh_coordinator::{ProgressSink, RefreshCoordinator, RefreshScope};
use crate::schedule::{parse_reserve_schedule, parse_text_schedule, ParsedSchedule};
use crate::settings::{load_settings, AppSettings, RuntimeSecrets};
use crate::store::{FertilityCache, SqliteStore};
use crate::timing::{measure, PhaseTimer};
use crate::youtube::{YouTubeChannel, YouTubeClient, YouTubeCommunityPost, YouTubeVideo};

#[path = "youtube_stats.rs"]
mod youtube_stats;
use youtube_stats::YouTubeStatsManager;

const A_SOUL_OFFICIAL_MID: &str = "703007996";
const A_SOUL_MEMBER_MIDS: &[&str] = &["672342685", "672328094", "672353429"];
pub const FAN_POLL_INTERVAL_SECONDS: u64 = 60 * 60;
const TITLE_REUSE_LOOKBACK_DAYS: i64 = 56;
const TITLE_REUSE_SLOT_TOLERANCE_MINUTES: i64 = 30;
/// 预约卡片与正文文字指向同一场直播时允许的时间差（分钟）。
/// 超过这个差值就当成两场不同的日程：预约只管卡片那一场，
/// 正文里另写的时间点（例如预约 06:00 的比赛、正文另说“今晚20:00来杂谈”）仍要入库。
const RESERVE_DUPLICATE_TOLERANCE_MINUTES: i64 = 10;
/// 同一条动态的文字通道与图片通道撞车时的容差：低于预约卡的 10 分钟，
/// 因为两条通道读的是同一份材料，不该出现“预约卡 06:00、正文另说 20:00”那种偏差。
const GROUP_DUPLICATE_TOLERANCE_MINUTES: i64 = 5;
/// 空结果的重试窗口（小时）。模型偶尔漏看一次，写进缓存后过一段时间再问；
/// 每轮都重问会把一次全量刷新拖成几十分钟。
const EMPTY_ANALYSIS_RETRY_HOURS: i64 = 12;
const FAN_SNAPSHOT_RETENTION_DAYS: i64 = 90;
// Space Bilibili request starts without holding a database or model permit.
const SOURCE_FETCH_START_INTERVAL_MS: u64 = 70;
/// 「立即刷新」最多等待的秒数：超过就回落到缓存，避免界面长时间停在「读取中」。
const FERTILITY_FORCE_REFRESH_TIMEOUT_SECONDS: u64 = 15;
/// In-flight YouTube requests poll the global switch at this interval. When the
/// switch turns off, dropping the request future prevents encapsulated paging
/// from starting another upstream call.
const YOUTUBE_SWITCH_RECHECK_INTERVAL_MS: u64 = 250;

async fn youtube_requests_enabled(store: &SqliteStore) -> bool {
    load_settings(store)
        .await
        .map(|settings| settings.youtube_enabled)
        .unwrap_or(false)
}

async fn wait_for_youtube_disabled(store: &SqliteStore) {
    loop {
        if !youtube_requests_enabled(store).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(
            YOUTUBE_SWITCH_RECHECK_INTERVAL_MS,
        ))
        .await;
    }
}

/// Runs one potentially paginated YouTube operation while watching the global
/// switch. `None` means the switch was disabled (or could not be read) and the
/// operation was cancelled before it could continue.
async fn run_youtube_request<T>(
    store: &SqliteStore,
    request: impl std::future::Future<Output = AppResult<T>>,
) -> Option<AppResult<T>> {
    tokio::select! {
        // Read the switch before polling the request. If it is already off,
        // the request future is dropped without ever starting its first API call.
        biased;
        _ = wait_for_youtube_disabled(store) => None,
        result = request => Some(result),
    }
}

fn youtube_feature_disabled_error() -> AppError {
    AppError::External {
        code: "YOUTUBE_FEATURE_DISABLED".to_owned(),
        message: "YouTube 功能未开启".to_owned(),
    }
}

async fn run_required_youtube_request<T>(
    store: &SqliteStore,
    request: impl std::future::Future<Output = AppResult<T>>,
) -> AppResult<T> {
    run_youtube_request(store, request)
        .await
        .unwrap_or_else(|| Err(youtube_feature_disabled_error()))
}

/// LAPLACE 抓取单飞标记：同一进程内只允许一个抓取在跑，避免重复请求把上游刷成风控。
static FERTILITY_REFRESH_RUNNING: AtomicBool = AtomicBool::new(false);

/// 单飞标记的 RAII 释放器：正常返回、提前 return 或 panic 都会释放。
struct FertilityRefreshGuard;

impl Drop for FertilityRefreshGuard {
    fn drop(&mut self) {
        FERTILITY_REFRESH_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// 抢到抓取权时返回守卫，已有抓取在跑时返回 `None`。
fn begin_fertility_refresh() -> Option<FertilityRefreshGuard> {
    if FERTILITY_REFRESH_RUNNING.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(FertilityRefreshGuard)
}

/// Immutable input snapshot shared by job registration and actual execution.
/// Credentials remain in memory; callers only receive an opaque request key.
#[derive(Clone)]
pub struct PreparedRefresh {
    settings: AppSettings,
    streamers: Vec<Streamer>,
    config: Option<LlmConfig>,
    scope: RefreshScope,
    streamer_id: Option<i64>,
    bili: Arc<BilibiliClient>,
    youtube: Arc<YouTubeClient>,
    llm: Arc<dyn ContentAnalyzer>,
    request_key: String,
}

impl PreparedRefresh {
    pub fn request_key(&self) -> &str {
        &self.request_key
    }
}

#[derive(Clone)]
pub struct AppService {
    store: SqliteStore,
    bili: Arc<StdRwLock<Arc<BilibiliClient>>>,
    youtube: Arc<StdRwLock<Arc<YouTubeClient>>>,
    llm: Arc<StdRwLock<Arc<dyn ContentAnalyzer>>>,
    fertility: Arc<StdRwLock<Arc<FertilityClient>>>,
    secrets: Arc<RwLock<RuntimeSecrets>>,
    llm_key_binding: Arc<RwLock<Option<(String, String)>>>,
    live_stats: LiveStatsManager,
    youtube_stats: YouTubeStatsManager,
    refresh_lock: Arc<Mutex<()>>,
    refresh_coordinator: RefreshCoordinator,
    // Protect the cross-streamer find/merge/insert sequence for joint streams.
    // This is never held during fetching, image preparation, or model requests.
    auto_schedule_write_lock: Arc<Mutex<()>>,
    live_check_locks: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
}

impl AppService {
    pub fn new(store: SqliteStore, secrets: RuntimeSecrets) -> AppResult<Self> {
        Self::new_with_proxy(store, secrets, None)
    }

    pub fn new_with_proxy(
        store: SqliteStore,
        secrets: RuntimeSecrets,
        proxy_port: Option<u16>,
    ) -> AppResult<Self> {
        let bili = Arc::new(BilibiliClient::new_with_proxy(
            secrets.bili_cookie.clone(),
            proxy_port,
        )?);
        let youtube = Arc::new(YouTubeClient::new_optional(
            secrets.youtube_api_key.clone(),
            proxy_port,
        )?);
        let llm = Arc::new(LlmClient::new_with_proxy(proxy_port)?);
        Ok(Self {
            store,
            bili: Arc::new(StdRwLock::new(bili)),
            youtube: Arc::new(StdRwLock::new(youtube)),
            llm: Arc::new(StdRwLock::new(llm)),
            fertility: Arc::new(StdRwLock::new(Arc::new(FertilityClient::new_with_proxy(
                proxy_port,
            )?))),
            secrets: Arc::new(RwLock::new(secrets)),
            llm_key_binding: Arc::new(RwLock::new(None)),
            live_stats: LiveStatsManager::default(),
            youtube_stats: YouTubeStatsManager::default(),
            refresh_lock: Arc::new(Mutex::new(())),
            refresh_coordinator: RefreshCoordinator::default(),
            auto_schedule_write_lock: Arc::new(Mutex::new(())),
            live_check_locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn with_analyzer(
        store: SqliteStore,
        secrets: RuntimeSecrets,
        analyzer: Arc<dyn ContentAnalyzer>,
    ) -> AppResult<Self> {
        let bili = Arc::new(BilibiliClient::new(secrets.bili_cookie.clone())?);
        let youtube = Arc::new(YouTubeClient::new_optional(
            secrets.youtube_api_key.clone(),
            None,
        )?);
        Ok(Self {
            store,
            bili: Arc::new(StdRwLock::new(bili)),
            youtube: Arc::new(StdRwLock::new(youtube)),
            llm: Arc::new(StdRwLock::new(analyzer)),
            fertility: Arc::new(StdRwLock::new(Arc::new(FertilityClient::new()?))),
            secrets: Arc::new(RwLock::new(secrets)),
            llm_key_binding: Arc::new(RwLock::new(None)),
            live_stats: LiveStatsManager::default(),
            youtube_stats: YouTubeStatsManager::default(),
            refresh_lock: Arc::new(Mutex::new(())),
            refresh_coordinator: RefreshCoordinator::default(),
            auto_schedule_write_lock: Arc::new(Mutex::new(())),
            live_check_locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    fn bili_client(&self) -> Arc<BilibiliClient> {
        self.bili
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn youtube_client(&self) -> Arc<YouTubeClient> {
        self.youtube
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn llm_analyzer(&self) -> Arc<dyn ContentAnalyzer> {
        self.llm
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn fertility_client(&self) -> Arc<FertilityClient> {
        self.fertility
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn update_proxy_port(&self, proxy_port: Option<u16>) -> AppResult<()> {
        let secrets = self.secrets.read().await.clone();
        let bili = Arc::new(BilibiliClient::new_with_proxy(
            secrets.bili_cookie,
            proxy_port,
        )?);
        let youtube = Arc::new(YouTubeClient::new_optional(
            secrets.youtube_api_key,
            proxy_port,
        )?);
        let llm: Arc<dyn ContentAnalyzer> = Arc::new(LlmClient::new_with_proxy(proxy_port)?);
        let fertility = Arc::new(FertilityClient::new_with_proxy(proxy_port)?);
        *self
            .bili
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = bili;
        *self
            .youtube
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = youtube;
        *self
            .llm
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = llm;
        *self
            .fertility
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fertility;
        Ok(())
    }

    pub async fn test_proxy(&self, proxy_port: u16) -> AppResult<ProxyTestReport> {
        test_google(proxy_port).await
    }

    async fn live_check_lock(&self, streamer_id: i64) -> Arc<Mutex<()>> {
        self.live_check_locks
            .lock()
            .await
            .entry(streamer_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn capture_avatar_data(&self, avatar_url: &str) -> Option<String> {
        let avatar_url = avatar_url.trim();
        if avatar_url.is_empty() || avatar_url.starts_with("data:image/") {
            return None;
        }
        match self.bili_client().fetch_avatar_data_url(avatar_url).await {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(%error, "capture streamer avatar failed");
                None
            }
        }
    }

    async fn backfill_avatar_captures(&self, streamers: &[Streamer]) {
        let mut seen = HashSet::new();
        for streamer in streamers {
            if streamer.platform != Platform::Bilibili
                || !seen.insert(streamer.id)
                || streamer.avatar_url.starts_with("data:image/")
            {
                continue;
            }
            let Some(avatar) = self.capture_avatar_data(&streamer.avatar_url).await else {
                continue;
            };
            if let Err(error) = self
                .store
                .update_streamer_avatar(streamer.id, &avatar)
                .await
            {
                tracing::warn!(%error, streamer_id = streamer.id, "store captured avatar failed");
            }
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        }
    }

    pub async fn update_secrets(&self, secrets: RuntimeSecrets) -> AppResult<()> {
        self.bili_client()
            .set_cookie(secrets.bili_cookie.clone())
            .await;
        self.youtube_client()
            .set_api_key(secrets.youtube_api_key.clone())
            .await;
        *self.secrets.write().await = secrets;
        Ok(())
    }

    pub async fn youtube_api_key_configured(&self) -> bool {
        self.youtube_client().api_key_configured().await
    }

    pub async fn update_youtube_api_key(&self, api_key: Option<String>) -> AppResult<()> {
        let api_key = api_key
            .map(|value| value.replace(['\r', '\n'], "").trim().to_owned())
            .filter(|value| !value.is_empty());
        self.youtube_client().set_api_key(api_key.clone()).await;
        self.secrets.write().await.youtube_api_key = api_key;
        Ok(())
    }

    pub async fn update_bili_cookie(&self, cookie: Option<String>) -> AppResult<()> {
        let cookie = cookie
            .map(|value| value.replace(['\r', '\n'], "").trim().to_owned())
            .filter(|value| !value.is_empty());
        self.bili_client().set_cookie(cookie.clone()).await;
        self.secrets.write().await.bili_cookie = cookie;
        Ok(())
    }

    pub async fn bili_cookie_configured(&self) -> bool {
        self.secrets.read().await.bili_cookie.is_some()
    }

    pub async fn llm_api_key(&self) -> Option<String> {
        self.secrets.read().await.llm_api_key.clone()
    }

    pub async fn update_llm_api_key(&self, api_key: String) -> AppResult<()> {
        *self.llm_key_binding.write().await = None;
        self.secrets.write().await.llm_api_key = Some(api_key);
        Ok(())
    }

    pub async fn update_bound_llm_api_key(
        &self,
        api_key: String,
        base_url: Option<String>,
    ) -> AppResult<()> {
        // Install the scope first: a concurrent reader can only withhold a key,
        // never send a new key to the previous endpoint during a two-step save.
        *self.llm_key_binding.write().await = Some((api_key.clone(), base_url.unwrap_or_default()));
        self.secrets.write().await.llm_api_key = Some(api_key);
        Ok(())
    }

    async fn llm_key_for_base_url(&self, base_url: &str) -> Option<String> {
        let key = self.secrets.read().await.llm_api_key.clone()?;
        if let Some((bound_key, bound_url)) = self.llm_key_binding.read().await.as_ref() {
            if bound_key != &key
                || bound_url.is_empty()
                || !llm_base_urls_match(base_url, bound_url).unwrap_or(false)
            {
                return None;
            }
        }
        Some(key)
    }

    pub async fn test_llm(&self, mut config: LlmConfig) -> AppResult<LlmTestReport> {
        if config.api_key.trim().is_empty() {
            let settings = load_settings(&self.store).await?;
            if !settings.llm_base_url.trim().is_empty()
                && llm_base_urls_match(&config.base_url, &settings.llm_base_url)?
            {
                config.api_key = self
                    .llm_key_for_base_url(&config.base_url)
                    .await
                    .unwrap_or_default();
            }
        }
        self.llm_analyzer().test_calendar_image(&config).await
    }

    pub async fn list_llm_models(&self, mut config: LlmDiscoveryConfig) -> AppResult<Vec<String>> {
        if config.api_key.trim().is_empty() {
            let settings = load_settings(&self.store).await?;
            if !settings.llm_base_url.trim().is_empty()
                && llm_base_urls_match(&config.base_url, &settings.llm_base_url)?
            {
                config.api_key = self
                    .llm_key_for_base_url(&config.base_url)
                    .await
                    .unwrap_or_default();
            }
        }
        self.llm_analyzer().list_models(&config).await
    }

    pub async fn search_accounts(
        &self,
        keyword: &str,
    ) -> AppResult<Vec<crate::models::AccountSearchResult>> {
        self.search_accounts_by_platform(keyword, Platform::Bilibili)
            .await
    }

    pub async fn search_accounts_by_platform(
        &self,
        keyword: &str,
        platform: Platform,
    ) -> AppResult<Vec<crate::models::AccountSearchResult>> {
        if platform == Platform::Youtube {
            self.require_youtube_enabled().await?;
        }
        let keyword = keyword.trim();
        if keyword.is_empty() || keyword.chars().count() > 100 {
            return Err(crate::error::AppError::validation(
                "搜索关键词长度必须为 1 到 100 字",
            ));
        }
        let subscriptions: Vec<Streamer> = self
            .store
            .list_streamers()
            .await?
            .into_iter()
            .filter(|streamer| streamer.platform == platform)
            .collect();
        let local_matches: Vec<crate::models::AccountSearchResult> = subscriptions
            .iter()
            .filter(|streamer| search_matches_subscription(keyword, streamer))
            .map(account_from_streamer)
            .collect();
        let remote = match platform {
            Platform::Bilibili => self.bili_client().search_accounts(keyword).await,
            Platform::Youtube => self.search_youtube_channels(keyword).await,
        };
        let mut results = match remote {
            Ok(results) => results,
            Err(error) if !local_matches.is_empty() => {
                tracing::warn!(%error, %platform, "account search failed; using local exact matches");
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        results.extend(local_matches);
        dedupe_and_rank_search_results(&mut results, keyword, &subscriptions);
        Ok(results)
    }

    async fn search_youtube_channels(
        &self,
        keyword: &str,
    ) -> AppResult<Vec<crate::models::AccountSearchResult>> {
        let client = self.youtube_client();
        let channels = match YouTubeClient::parse_account_input(keyword)? {
            crate::youtube::YouTubeAccountInput::Search(query) => {
                run_required_youtube_request(&self.store, client.search_channels(&query, 20))
                    .await?
            }
            _ => vec![
                run_required_youtube_request(&self.store, client.resolve_channel(keyword)).await?,
            ],
        };
        Ok(channels
            .into_iter()
            .map(youtube_account_search_result)
            .collect())
    }

    async fn require_youtube_enabled(&self) -> AppResult<()> {
        if load_settings(&self.store).await?.youtube_enabled {
            Ok(())
        } else {
            Err(youtube_feature_disabled_error())
        }
    }

    pub async fn create_bili_qr_login(&self) -> AppResult<QrLoginStart> {
        self.bili_client().create_qr_login().await
    }

    pub async fn poll_bili_qr_login(&self, qrcode_key: &str) -> AppResult<QrLoginPoll> {
        self.bili_client().poll_qr_login(qrcode_key).await
    }

    pub async fn add_subscription(&self, mut input: AddSubscription) -> AppResult<Streamer> {
        if input.platform == Platform::Youtube {
            self.require_youtube_enabled().await?;
            let client = self.youtube_client();
            let channel =
                run_required_youtube_request(&self.store, client.resolve_channel(&input.mid))
                    .await?;
            input.mid = channel.id;
            input.name = Some(channel.name);
            input.avatar_url = channel.avatar_url;
            input.handle = channel.handle;
            input.profile_url = Some(channel.profile_url);
            input.uploads_playlist_id = channel.uploads_playlist_id;
            input.room_id = None;
            return self.store.add_subscription(&input).await;
        }
        if input.name.as_deref().unwrap_or_default().trim().is_empty()
            || input
                .avatar_url
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            || input.room_id.is_none()
        {
            let account = self.bili_client().fetch_account(&input.mid).await?;
            input.name = input.name.or(Some(account.name));
            input.avatar_url = input.avatar_url.or(Some(account.avatar_url));
            input.room_id = input.room_id.or(account.room_id);
        }
        if let Some(avatar) = self
            .capture_avatar_data(input.avatar_url.as_deref().unwrap_or_default())
            .await
        {
            input.avatar_url = Some(avatar);
        }
        self.store.add_subscription(&input).await
    }

    pub async fn add_linked_account(
        &self,
        streamer_id: i64,
        mid: &str,
    ) -> AppResult<crate::models::LinkedAccount> {
        self.add_linked_account_for_platform(streamer_id, Platform::Bilibili, mid)
            .await
    }

    pub async fn add_linked_account_for_platform(
        &self,
        streamer_id: i64,
        platform: Platform,
        mid: &str,
    ) -> AppResult<crate::models::LinkedAccount> {
        let parent = self.store.get_streamer(streamer_id).await?;
        if parent.platform != platform {
            return Err(AppError::validation("关联账号必须与主播使用同一平台"));
        }
        if platform == Platform::Youtube {
            self.require_youtube_enabled().await?;
            let client = self.youtube_client();
            let channel =
                run_required_youtube_request(&self.store, client.resolve_channel(mid)).await?;
            return self
                .store
                .add_linked_account_for_platform(
                    streamer_id,
                    Platform::Youtube,
                    &channel.id,
                    &channel.name,
                    channel.avatar_url.as_deref().unwrap_or_default(),
                    channel.handle.as_deref(),
                    Some(channel.profile_url.as_str()),
                )
                .await;
        }
        let account = self.bili_client().fetch_account(mid).await?;
        let avatar_url = self
            .capture_avatar_data(&account.avatar_url)
            .await
            .unwrap_or(account.avatar_url);
        self.store
            .add_linked_account(streamer_id, mid, &account.name, &avatar_url)
            .await
    }

    pub async fn update_subscription(
        &self,
        id: i64,
        update: &UpdateSubscription,
    ) -> AppResult<Streamer> {
        let updated = self.store.update_subscription(id, update).await?;
        if update.live_stats_enabled == Some(false) {
            let now = Utc::now();
            let open_session = self.store.latest_open_live_session(id).await?;
            self.live_stats.stop(id, now).await;
            self.youtube_stats.stop(id, now).await;
            if updated.platform == Platform::Youtube {
                if let Some(session) = open_session {
                    self.store.finish_open_live_session(session.id, now).await?;
                }
            }
        }
        Ok(updated)
    }

    pub async fn reorder_subscriptions(&self, ids: &[i64]) -> AppResult<Vec<Streamer>> {
        self.store.reorder_subscriptions(ids).await
    }

    pub async fn delete_subscription(&self, id: i64) -> AppResult<()> {
        let stopped_at = Utc::now();
        self.live_stats.stop(id, stopped_at).await;
        self.youtube_stats.stop(id, stopped_at).await;
        self.store.delete_subscription(id).await?;
        if let Err(error) = self
            .store
            .append_event(
                "schedule_changed",
                &json!({"action": "subscription_deleted", "streamerId": id}),
            )
            .await
        {
            tracing::warn!(%error, streamer_id = id, "append subscription deletion event failed");
        }
        Ok(())
    }

    pub async fn list_subscriptions(&self) -> AppResult<Vec<Streamer>> {
        self.store.list_streamers().await
    }

    pub async fn list_schedules(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> AppResult<Vec<Schedule>> {
        self.materialize_weekly_rules(from, to).await?;
        self.store.list_schedules(from, to, false).await
    }

    pub async fn create_schedule(&self, schedule: &NewSchedule) -> AppResult<Schedule> {
        let (schedule, _) = self.store.upsert_schedule(schedule).await?;
        if let Err(error) = self
            .store
            .append_event(
                "schedule_changed",
                &json!({"action": "created", "scheduleId": schedule.id}),
            )
            .await
        {
            tracing::warn!(%error, "append schedule event failed");
        }
        Ok(schedule)
    }

    pub async fn update_schedule(&self, id: i64, update: &UpdateSchedule) -> AppResult<Schedule> {
        let schedule = self.store.update_schedule(id, update).await?;
        if let Err(error) = self
            .store
            .append_event(
                "schedule_changed",
                &json!({"action": "updated", "scheduleId": id}),
            )
            .await
        {
            tracing::warn!(%error, "append schedule event failed");
        }
        Ok(schedule)
    }

    pub async fn delete_schedule(&self, id: i64) -> AppResult<()> {
        self.store.delete_schedule(id).await?;
        if let Err(error) = self
            .store
            .append_event(
                "schedule_changed",
                &json!({"action": "deleted", "scheduleId": id}),
            )
            .await
        {
            tracing::warn!(%error, "append schedule event failed");
        }
        Ok(())
    }

    pub async fn list_weekly_rules(
        &self,
        streamer_id: Option<i64>,
    ) -> AppResult<Vec<WeeklyScheduleRule>> {
        self.store.list_weekly_rules(streamer_id).await
    }

    pub async fn create_weekly_rule(
        &self,
        input: &NewWeeklyScheduleRule,
    ) -> AppResult<WeeklyScheduleRule> {
        let (rule, created) = self.store.add_weekly_rule_idempotent(input).await?;
        let now = Utc::now();
        self.materialize_weekly_rules(now, now + Duration::days(84))
            .await?;
        if created {
            let _ = self
                .store
                .append_event(
                    "weekly_rule_changed",
                    &json!({"action": "created", "ruleId": rule.id, "streamerId": rule.streamer_id}),
                )
                .await;
        }
        Ok(rule)
    }

    pub async fn update_weekly_rule(
        &self,
        id: i64,
        update: &UpdateWeeklyScheduleRule,
    ) -> AppResult<WeeklyScheduleRule> {
        let rule = self.store.update_weekly_rule(id, update).await?;
        self.store
            .delete_future_weekly_occurrences(id, true)
            .await?;
        let now = Utc::now();
        self.materialize_weekly_rules(now, now + Duration::days(84))
            .await?;
        let _ = self
            .store
            .append_event(
                "weekly_rule_changed",
                &json!({"action": "updated", "ruleId": id, "streamerId": rule.streamer_id}),
            )
            .await;
        Ok(rule)
    }

    pub async fn delete_weekly_rule(&self, id: i64) -> AppResult<()> {
        let rule = self.store.get_weekly_rule(id).await?;
        self.store.delete_weekly_rule(id).await?;
        let _ = self
            .store
            .append_event(
                "weekly_rule_changed",
                &json!({"action": "deleted", "ruleId": id, "streamerId": rule.streamer_id}),
            )
            .await;
        Ok(())
    }

    pub async fn materialize_weekly_rules(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> AppResult<usize> {
        if to <= from {
            return Ok(0);
        }
        let now = Utc::now();
        let from_date = crate::weekly::china_date(from.max(now - Duration::hours(1)));
        let requested_to = crate::weekly::china_date(to);
        let to_date = requested_to.min(from_date + Duration::days(366));
        if to_date < from_date {
            return Ok(0);
        }
        let streamers: HashMap<i64, Streamer> = self
            .store
            .list_streamers()
            .await?
            .into_iter()
            .map(|streamer| (streamer.id, streamer))
            .collect();
        let mut created = 0;
        for rule in self.store.list_weekly_rules(None).await? {
            if !rule.enabled {
                continue;
            }
            let Some(streamer) = streamers.get(&rule.streamer_id) else {
                continue;
            };
            let Ok(effective_from) = NaiveDate::parse_from_str(&rule.effective_from, "%Y-%m-%d")
            else {
                continue;
            };
            let effective_until = rule
                .effective_until
                .as_deref()
                .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok());
            let mut date = from_date.max(effective_from);
            let end_date = effective_until.map_or(to_date, |until| to_date.min(until));
            while date <= end_date {
                if crate::weekly::occurs_on(rule.weekdays_mask, date) {
                    let occurrence_date = date.format("%Y-%m-%d").to_string();
                    if !self
                        .store
                        .weekly_exception_exists(rule.id, &occurrence_date)
                        .await?
                    {
                        if let Some((start_at, end_at)) = crate::weekly::occurrence_bounds(
                            &rule.start_time,
                            rule.end_time.as_deref(),
                            &occurrence_date,
                        ) {
                            if start_at >= now {
                                self.store
                                    .upsert_weekly_occurrence(
                                        &rule,
                                        streamer,
                                        &occurrence_date,
                                        start_at,
                                        end_at,
                                    )
                                    .await?;
                                created += 1;
                            }
                        }
                    }
                }
                date += Duration::days(1);
            }
        }
        Ok(created)
    }

    pub async fn list_live_sessions(
        &self,
        streamer_id: Option<i64>,
        limit: i64,
    ) -> AppResult<Vec<crate::models::LiveSession>> {
        self.store.list_live_sessions(streamer_id, limit).await
    }

    pub async fn live_session_for_schedule(
        &self,
        schedule_id: i64,
    ) -> AppResult<Option<crate::models::LiveSession>> {
        self.store.find_live_session_for_schedule(schedule_id).await
    }
    pub async fn list_live_statuses(&self) -> AppResult<Vec<LiveStatus>> {
        self.store.list_live_statuses().await
    }

    pub async fn get_fan_history(&self, streamer_id: i64, days: i64) -> AppResult<FanHistory> {
        if !(1..=FAN_SNAPSHOT_RETENTION_DAYS).contains(&days) {
            return Err(AppError::validation("粉丝历史范围必须在 1 到 90 天之间"));
        }
        let streamer = self.store.get_streamer(streamer_id).await?;
        let snapshots = self
            .store
            .list_fan_snapshots(streamer_id, Utc::now() - Duration::days(days))
            .await?;
        let checked_at = snapshots.last().map(|snapshot| snapshot.captured_at);
        Ok(FanHistory {
            streamer_id,
            current_fans: streamer.fans,
            checked_at,
            snapshots,
        })
    }

    async fn prune_fan_snapshots(&self) -> AppResult<u64> {
        let cutoff = (Utc::now() - Duration::days(FAN_SNAPSHOT_RETENTION_DAYS)).to_rfc3339();
        let result = sqlx::query("DELETE FROM fan_snapshots WHERE captured_at < ?")
            .bind(cutoff)
            .execute(self.store.pool())
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn refresh_streamer_fans(&self, streamer_id: i64) -> AppResult<FanHistory> {
        let streamer = self.store.get_streamer(streamer_id).await?;
        let fans = match streamer.platform {
            Platform::Bilibili => self.bili_client().fetch_fan_count(&streamer.mid).await?,
            Platform::Youtube => {
                let client = self.youtube_client();
                run_required_youtube_request(
                    &self.store,
                    client.fetch_channel_metadata(&streamer.mid),
                )
                .await?
                .subscriber_count
                .ok_or_else(|| AppError::External {
                    code: "YOUTUBE_SUBSCRIBER_COUNT_MISSING".to_owned(),
                    message: "YouTube 频道未返回订阅者数".to_owned(),
                })?
            }
        };
        self.store
            .record_fan_snapshot(streamer.id, fans, Utc::now())
            .await?;
        self.get_fan_history(streamer_id, 30).await
    }

    pub async fn refresh_all_fans(&self) -> AppResult<FanRefreshReport> {
        let streamers = self.store.list_streamers().await?;
        let mut report = FanRefreshReport {
            refreshed_streamers: 0,
            failed_streamers: Vec::new(),
        };
        let youtube_enabled = load_settings(&self.store).await?.youtube_enabled;
        let bili = self.bili_client();
        let youtube = self.youtube_client();
        for (index, streamer) in streamers.iter().enumerate() {
            let fans = match streamer.platform {
                Platform::Bilibili => bili.fetch_fan_count(&streamer.mid).await,
                Platform::Youtube if youtube_enabled => {
                    let Some(result) = run_youtube_request(
                        &self.store,
                        youtube.fetch_channel_metadata(&streamer.mid),
                    )
                    .await
                    else {
                        continue;
                    };
                    result.and_then(|channel| {
                        channel.subscriber_count.ok_or_else(|| AppError::External {
                            code: "YOUTUBE_SUBSCRIBER_COUNT_MISSING".to_owned(),
                            message: "YouTube 频道未返回订阅者数".to_owned(),
                        })
                    })
                }
                Platform::Youtube => continue,
            };
            match fans {
                Ok(fans) => {
                    match self
                        .store
                        .record_fan_snapshot(streamer.id, fans, Utc::now())
                        .await
                    {
                        Ok(_) => report.refreshed_streamers += 1,
                        Err(error) => report
                            .failed_streamers
                            .push(format!("{}: {error}", streamer.name)),
                    }
                }
                Err(error) => {
                    tracing::warn!(streamer_id = streamer.id, mid = %streamer.mid, %error, "粉丝数采集失败");
                    report
                        .failed_streamers
                        .push(format!("{}: {error}", streamer.name));
                }
            }
            if index % 5 == 4 && index + 1 < streamers.len() {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        if let Err(error) = self.prune_fan_snapshots().await {
            tracing::warn!(%error, "清理过期粉丝快照失败");
            report
                .failed_streamers
                .push(format!("粉丝快照清理: {error}"));
        }
        Ok(report)
    }

    /// 读取 LAPLACE 生理期数据，并与当前订阅主播匹配。
    ///
    /// 「寻血猎犬」关闭时直接返回 `enabled = false` 的空快照，**不读缓存也不发网络请求**。
    ///
    /// 命中 6 小时内的缓存时直接返回；缓存过期或为空时**不在请求路径上等网络**：立即返回
    /// 现有缓存（标记 `stale`）并触发一次后台单飞刷新，客户端不会长时间停在「读取中」。
    /// `force_refresh`（界面上的「立即刷新」）最多等待 15 秒，失败或超时同样回落到缓存。
    pub async fn list_fertility_statuses(
        &self,
        force_refresh: bool,
    ) -> AppResult<FertilitySnapshot> {
        let settings = load_settings(&self.store).await?;
        if !settings.fertility_enabled {
            return Ok(FertilitySnapshot::disabled());
        }
        let streamers = self.store.list_streamers().await?;
        if streamers.is_empty() {
            return Ok(FertilitySnapshot {
                enabled: true,
                source: "none".to_owned(),
                ..FertilitySnapshot::disabled()
            });
        }

        let forced = if force_refresh {
            self.try_force_fertility_refresh(&streamers).await
        } else {
            None
        };
        if let Some(snapshot) = forced {
            return Ok(snapshot);
        }

        let cached = self.store.get_fertility_cache().await?;
        let fresh = cached.as_ref().is_some_and(|cache| {
            Utc::now().signed_duration_since(cache.fetched_at)
                < Duration::hours(FERTILITY_CACHE_TTL_HOURS)
        });
        let reusable = if fresh && !force_refresh {
            Self::fertility_snapshot_from_cache(&streamers, cached.as_ref(), false, None)
        } else {
            None
        };
        if let Some(snapshot) = reusable {
            return Ok(snapshot);
        }

        let scheduled = self.spawn_fertility_refresh();
        let warning = if scheduled {
            "LAPLACE 数据正在后台刷新，先返回缓存数据"
        } else {
            "LAPLACE 数据刷新中，先返回缓存数据"
        };
        if let Some(snapshot) = Self::fertility_snapshot_from_cache(
            &streamers,
            cached.as_ref(),
            true,
            Some(warning.to_owned()),
        ) {
            return Ok(snapshot);
        }
        Ok(FertilitySnapshot {
            enabled: true,
            source: "none".to_owned(),
            stale: true,
            warning: Some(warning.to_owned()),
            ..FertilitySnapshot::disabled()
        })
    }

    /// 抓一次 LAPLACE 并写入缓存。只在后台任务或「立即刷新」里调用，会触网。
    async fn refresh_fertility_snapshot(
        &self,
        streamers: &[Streamer],
    ) -> AppResult<FertilitySnapshot> {
        let (payload, source, fetched_at) = self.fertility_client().fetch().await?;
        let serialized = serde_json::to_string(&payload)?;
        if let Err(error) = self
            .store
            .save_fertility_cache(&serialized, fetched_at, &source)
            .await
        {
            tracing::warn!(%error, "写入 LAPLACE 缓存失败");
        }
        Ok(Self::build_fertility_snapshot(
            streamers, &payload, fetched_at, source, false, None,
        ))
    }

    /// 「立即刷新」：抢到抓取权后同步抓一次，最多等 15 秒；抢不到或失败返回 `None`，
    /// 由调用方回落到缓存数据。
    async fn try_force_fertility_refresh(
        &self,
        streamers: &[Streamer],
    ) -> Option<FertilitySnapshot> {
        let _guard = begin_fertility_refresh()?;
        match tokio::time::timeout(
            std::time::Duration::from_secs(FERTILITY_FORCE_REFRESH_TIMEOUT_SECONDS),
            self.refresh_fertility_snapshot(streamers),
        )
        .await
        {
            Ok(Ok(snapshot)) => Some(snapshot),
            Ok(Err(error)) => {
                tracing::warn!(%error, "刷新 LAPLACE 生理期数据失败，回落到缓存数据");
                None
            }
            Err(_) => {
                tracing::warn!("刷新 LAPLACE 生理期数据超时，回落到缓存数据");
                None
            }
        }
    }

    /// 把缓存里的 LAPLACE 列表转成快照；缓存解析失败时返回 `None`。
    fn fertility_snapshot_from_cache(
        streamers: &[Streamer],
        cache: Option<&FertilityCache>,
        stale: bool,
        warning: Option<String>,
    ) -> Option<FertilitySnapshot> {
        let cache = cache?;
        let payload = match parse_fertility_payload(&cache.payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(%error, "缓存的 LAPLACE 数据无法解析，忽略该缓存");
                return None;
            }
        };
        Some(Self::build_fertility_snapshot(
            streamers,
            &payload,
            cache.fetched_at,
            cache.source.clone(),
            stale,
            warning,
        ))
    }

    fn build_fertility_snapshot(
        streamers: &[Streamer],
        payload: &FertilityResponse,
        fetched_at: DateTime<Utc>,
        source: String,
        stale: bool,
        warning: Option<String>,
    ) -> FertilitySnapshot {
        let mut statuses = match_fertility_statuses(streamers, payload);
        let matched = statuses.len();
        statuses.sort_by_key(|item| {
            let rank = match item.status.as_str() {
                "menstruating" => 0,
                "fertile" => 1,
                "ovulating" => 2,
                "normal" => 3,
                _ => 4,
            };
            (rank, item.streamer_name.clone())
        });
        FertilitySnapshot {
            enabled: true,
            fetched_at,
            source,
            stale,
            warning,
            total_available: payload.total.max(payload.data.len()),
            matched,
            statuses,
        }
    }

    /// 后台单飞刷新：同一进程内只跑一个 LAPLACE 抓取。
    fn spawn_fertility_refresh(&self) -> bool {
        let Some(guard) = begin_fertility_refresh() else {
            return false;
        };
        let service = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(error) = service.refresh_fertility_if_enabled_inner().await {
                tracing::warn!(%error, "后台刷新 LAPLACE 生理期数据失败");
            }
        });
        true
    }

    /// 采集循环入口：开关关闭时什么都不做，返回 `Ok(None)`。
    pub async fn refresh_fertility_if_enabled(&self) -> AppResult<Option<usize>> {
        let Some(_guard) = begin_fertility_refresh() else {
            return Ok(None);
        };
        self.refresh_fertility_if_enabled_inner().await
    }

    async fn refresh_fertility_if_enabled_inner(&self) -> AppResult<Option<usize>> {
        let settings = load_settings(&self.store).await?;
        if !settings.fertility_enabled {
            return Ok(None);
        }
        let streamers = self.store.list_streamers().await?;
        if streamers.is_empty() {
            return Ok(Some(0));
        }
        let snapshot = self.refresh_fertility_snapshot(&streamers).await?;
        Ok(Some(snapshot.matched))
    }

    pub async fn check_live_all(&self) -> AppResult<Vec<LiveStatus>> {
        self.check_live_all_inner(true).await
    }

    pub async fn prime_live_statuses(&self) -> AppResult<Vec<LiveStatus>> {
        let statuses = self.check_live_all_inner(false).await?;
        // Startup priming intentionally suppresses transition notifications, but
        // clients still need to replace cached live dots with the fresh state.
        self.store
            .append_event(
                "live_status_snapshot",
                &json!({
                    "checkedAt": Utc::now(),
                    "count": statuses.len()
                }),
            )
            .await?;
        Ok(statuses)
    }

    async fn check_live_all_inner(&self, emit_transitions: bool) -> AppResult<Vec<LiveStatus>> {
        let streamers = self.store.list_streamers().await?;
        let settings = load_settings(&self.store).await?;
        let live_stats_enabled = settings.live_stats_enabled;
        for (index, streamer) in streamers.iter().enumerate() {
            if streamer.platform == Platform::Youtube && !settings.youtube_enabled {
                let now = Utc::now();
                let open_session = self.store.latest_open_live_session(streamer.id).await?;
                self.youtube_stats.stop(streamer.id, now).await;
                if let Some(session) = open_session {
                    self.store.finish_open_live_session(session.id, now).await?;
                }
                continue;
            }
            let streamer_lock = self.live_check_lock(streamer.id).await;
            let _streamer_guard = streamer_lock.lock().await;
            let stats_enabled = live_stats_enabled && streamer.live_stats_enabled;
            if !stats_enabled {
                // Disabling statistics must stop an existing collector even when
                // the live-status request below fails or no room can be resolved.
                let disabled_at = Utc::now();
                let open_session = self.store.latest_open_live_session(streamer.id).await?;
                self.live_stats.stop(streamer.id, disabled_at).await;
                self.youtube_stats.stop(streamer.id, disabled_at).await;
                if let Some(session) = open_session {
                    self.store
                        .finish_open_live_session(session.id, disabled_at)
                        .await?;
                }
            }
            if index > 0 && index % 5 == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            let result = match streamer.platform {
                Platform::Bilibili => {
                    self.check_bilibili_live_streamer(streamer, stats_enabled)
                        .await?
                }
                Platform::Youtube => {
                    self.check_youtube_live_streamer(streamer, stats_enabled)
                        .await?
                }
            };
            let Some((previous, snapshot)) = result else {
                continue;
            };
            if emit_transitions
                && streamer.live_checked_at.is_some()
                && previous != snapshot.is_live
            {
                self.store
                    .append_event(
                        "live_status",
                        &json!({
                            "platform": streamer.platform,
                            "streamerId": streamer.id,
                            "mid": streamer.mid,
                            "streamerName": streamer.name,
                            "previousIsLive": previous,
                            "isLive": snapshot.is_live,
                            "title": snapshot.title,
                            "coverUrl": snapshot.cover_url,
                            "liveUrl": snapshot.live_url,
                            "liveVideoId": snapshot.live_video_id,
                            "online": snapshot.online,
                            "startedAt": snapshot.started_at,
                            "checkedAt": snapshot.checked_at,
                            "liveNotifyEnabled": streamer.live_notify_enabled
                        }),
                    )
                    .await?;
            }
        }
        self.store.list_live_statuses().await
    }

    async fn check_bilibili_live_streamer(
        &self,
        streamer: &Streamer,
        stats_enabled: bool,
    ) -> AppResult<Option<(bool, LiveSnapshot)>> {
        let room_id = match streamer.room_id.clone() {
            Some(room_id) => Some(room_id),
            None => match self.bili_client().resolve_room_id(&streamer.mid).await {
                Ok(room_id) => {
                    if let Some(room_id) = &room_id {
                        if let Err(error) = self.store.set_streamer_room(streamer.id, room_id).await
                        {
                            tracing::warn!(%error, streamer_id = streamer.id, "save room id failed");
                        }
                    }
                    room_id
                }
                Err(error) => {
                    tracing::warn!(%error, mid = %streamer.mid, "resolve room id failed");
                    self.store
                        .record_streamer_error(streamer.id, &error.to_string())
                        .await?;
                    return Ok(None);
                }
            },
        };
        let Some(room_id) = room_id else {
            return Ok(None);
        };
        let snapshot = match self.bili_client().fetch_live_status(&room_id).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.store
                    .record_streamer_error(streamer.id, &error.to_string())
                    .await?;
                return Ok(None);
            }
        };
        let previous = self.store.set_live_status(streamer.id, &snapshot).await?;
        if stats_enabled && snapshot.is_live {
            let old_session = self.store.latest_open_live_session(streamer.id).await?;
            match self
                .store
                .ensure_broadcast_live_session(
                    streamer.id,
                    &snapshot.title,
                    snapshot.cover_url.as_deref(),
                    snapshot.checked_at,
                )
                .await
            {
                Ok(session) => {
                    if let Some(old) = old_session.filter(|old| old.id != session.id) {
                        self.live_stats.stop(streamer.id, snapshot.checked_at).await;
                        self.store
                            .finish_open_live_session(old.id, snapshot.checked_at)
                            .await?;
                    }
                    self.live_stats
                        .start(self.store.clone(), self.bili_client(), session)
                        .await;
                }
                Err(error) => {
                    tracing::warn!(%error, streamer_id = streamer.id, "创建直播数据场次失败");
                }
            }
        } else if stats_enabled {
            let open_session = self.store.latest_open_live_session(streamer.id).await?;
            self.live_stats.stop(streamer.id, snapshot.checked_at).await;
            if let Some(session) = open_session {
                self.store
                    .finish_open_live_session(session.id, snapshot.checked_at)
                    .await?;
            }
        }
        Ok(Some((previous, snapshot)))
    }

    async fn check_youtube_live_streamer(
        &self,
        streamer: &Streamer,
        stats_enabled: bool,
    ) -> AppResult<Option<(bool, LiveSnapshot)>> {
        if !youtube_requests_enabled(&self.store).await {
            return Ok(None);
        }
        let client = self.youtube_client();
        if streamer.fans.is_none() {
            let Some(metadata) =
                run_youtube_request(&self.store, client.fetch_channel_metadata(&streamer.mid))
                    .await
            else {
                return Ok(None);
            };
            match metadata {
                Ok(channel) => {
                    if let Some(fans) = channel.subscriber_count {
                        if let Err(error) = self
                            .store
                            .record_fan_snapshot(streamer.id, fans, Utc::now())
                            .await
                        {
                            tracing::warn!(%error, streamer_id = streamer.id, "保存 YouTube 订阅者数失败");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, mid = %streamer.mid, "读取 YouTube 频道信息失败");
                }
            }
        }
        let Some(probe) =
            run_youtube_request(&self.store, client.probe_live_page(&streamer.mid)).await
        else {
            return Ok(None);
        };
        let probe = match probe {
            Ok(probe) => probe,
            Err(error) => {
                self.store
                    .record_streamer_error(streamer.id, &error.to_string())
                    .await?;
                return Ok(None);
            }
        };
        let mut snapshot = LiveSnapshot {
            platform: Platform::Youtube,
            is_live: probe.is_live,
            title: probe.title.clone(),
            cover_url: probe.cover_url.clone(),
            live_url: probe.live_url.clone(),
            live_video_id: probe.video_id.clone(),
            online: probe.concurrent_viewers.unwrap_or(0),
            started_at: probe.started_at.or(probe.scheduled_start_at),
            checked_at: probe.checked_at,
        };
        let mut active_live_chat_id = None;
        if probe.is_live {
            if let Some(video_id) = probe.video_id.as_deref() {
                let Some(videos) =
                    run_youtube_request(&self.store, client.fetch_videos(&[video_id.to_owned()]))
                        .await
                else {
                    return Ok(None);
                };
                match videos {
                    Ok(videos) => {
                        if let Some(video) = videos.into_iter().next() {
                            let live_url = video.live_url();
                            snapshot.title = video.title;
                            snapshot.cover_url = video.thumbnail_url.or(snapshot.cover_url);
                            snapshot.live_url = live_url.or(snapshot.live_url);
                            snapshot.started_at = video
                                .actual_start_at
                                .or(video.scheduled_start_at)
                                .or(snapshot.started_at);
                            snapshot.online = video.concurrent_viewers.unwrap_or(snapshot.online);
                            active_live_chat_id = video.active_live_chat_id;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, streamer_id = streamer.id, "读取 YouTube 直播详情失败");
                    }
                }
            }
        }
        let previous = self
            .store
            .set_youtube_live_status(streamer.id, &snapshot)
            .await?;
        if stats_enabled && snapshot.is_live {
            let old_session = self.store.latest_open_live_session(streamer.id).await?;
            let started_at = snapshot
                .started_at
                .or_else(|| old_session.as_ref().map(|session| session.started_at))
                .unwrap_or(snapshot.checked_at);
            let room_id = snapshot
                .live_video_id
                .as_deref()
                .unwrap_or(streamer.mid.as_str());
            match self
                .store
                .ensure_youtube_live_session(
                    streamer.id,
                    room_id,
                    &streamer.name,
                    &snapshot.title,
                    snapshot.cover_url.as_deref(),
                    started_at,
                    snapshot.checked_at,
                )
                .await
            {
                Ok(session) => {
                    if let Some(old) = old_session.filter(|old| old.id != session.id) {
                        self.youtube_stats
                            .stop(streamer.id, snapshot.checked_at)
                            .await;
                        self.store
                            .finish_open_live_session(old.id, snapshot.checked_at)
                            .await?;
                    }
                    self.youtube_stats
                        .start(
                            self.store.clone(),
                            self.youtube_client(),
                            Arc::clone(&self.secrets),
                            streamer.id,
                            session,
                            snapshot.live_video_id.clone(),
                            active_live_chat_id,
                            snapshot.online,
                            started_at,
                        )
                        .await;
                }
                Err(error) => {
                    tracing::warn!(%error, streamer_id = streamer.id, "创建 YouTube 直播数据场次失败");
                }
            }
        } else if stats_enabled {
            let open_session = self.store.latest_open_live_session(streamer.id).await?;
            self.youtube_stats
                .stop(streamer.id, snapshot.checked_at)
                .await;
            if let Some(session) = open_session {
                self.store
                    .finish_open_live_session(session.id, snapshot.checked_at)
                    .await?;
            }
        }
        Ok(Some((previous, snapshot)))
    }

    pub async fn refresh_all(&self) -> AppResult<RefreshReport> {
        self.refresh_all_with_progress(|_| {}).await
    }

    pub async fn refresh_all_with_progress<P>(&self, on_progress: P) -> AppResult<RefreshReport>
    where
        P: Fn(RefreshProgress) + Send + Sync,
    {
        self.coordinate_refresh(None, &on_progress).await
    }

    /// Opaque snapshot used by the HTTP job registry too. Runtime fields such
    /// as last_refresh_at must not invalidate a still-running refresh.
    pub async fn refresh_request_key(&self, streamer_id: Option<i64>) -> AppResult<String> {
        Ok(self.prepare_refresh_request(streamer_id).await?.request_key)
    }

    pub async fn prepare_refresh_request(
        &self,
        streamer_id: Option<i64>,
    ) -> AppResult<PreparedRefresh> {
        let settings = load_settings(&self.store).await?;
        let streamers = match streamer_id {
            Some(id) => vec![self.store.get_streamer(id).await?],
            None => self.store.list_streamers().await?,
        };
        let config = self.current_llm_config(&settings).await;
        let binding = self.llm_key_binding.read().await.clone();
        let secrets = self.secrets.read().await;
        let cookie_fingerprint = secrets.bili_cookie.as_deref().map(text_digest);
        let youtube_api_key = secrets.youtube_api_key.as_deref().map(text_digest);
        let youtube_oauth = secrets
            .youtube_oauth_access_token
            .as_deref()
            .map(text_digest);
        let youtube_cookie = secrets.youtube_web_cookie.as_deref().map(text_digest);
        drop(secrets);
        // Only hashes enter the scope. Do not retain or log raw credentials.
        let config_key = text_digest(&serde_json::to_string(&json!({
            "settings": &settings,
            "model_key": config.as_ref().map(|config| text_digest(&config.api_key)),
            "binding": binding.as_ref().map(|(key, url)| (text_digest(key), url)),
            "cookie": cookie_fingerprint,
            "youtube_api_key": youtube_api_key,
            "youtube_oauth": youtube_oauth,
            "youtube_cookie": youtube_cookie,
        }))?);
        let targets = streamers
            .iter()
            .map(|streamer| {
                let linked: Vec<_> = streamer
                    .linked_accounts
                    .iter()
                    .map(|account| (&account.mid, &account.name))
                    .collect();
                let fingerprint = text_digest(
                    &json!({
                        "mid": streamer.mid, "name": streamer.name, "room_id": streamer.room_id,
                        "live_url": streamer.live_url, "aliases": streamer.aliases,
                        "enabled": streamer.schedule_enabled, "linked": linked,
                    })
                    .to_string(),
                );
                (streamer.id, fingerprint)
            })
            .collect::<BTreeMap<_, _>>();
        let scope = RefreshScope {
            config_key,
            targets,
            all: streamer_id.is_none(),
        };
        let request_key = text_digest(
            &json!({ "config": scope.config_key, "targets": scope.targets, "all": scope.all })
                .to_string(),
        );
        Ok(PreparedRefresh {
            settings,
            streamers,
            config,
            scope,
            streamer_id,
            bili: self.bili_client(),
            youtube: self.youtube_client(),
            llm: self.llm_analyzer(),
            request_key,
        })
    }

    async fn coordinate_refresh(
        &self,
        streamer_id: Option<i64>,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
    ) -> AppResult<RefreshReport> {
        let prepared = self.prepare_refresh_request(streamer_id).await?;
        self.run_prepared_refresh(prepared, on_progress).await
    }

    pub async fn refresh_prepared_with_progress<P>(
        &self,
        prepared: PreparedRefresh,
        on_progress: P,
    ) -> AppResult<RefreshReport>
    where
        P: Fn(RefreshProgress) + Send + Sync,
    {
        self.run_prepared_refresh(prepared, &on_progress).await
    }

    async fn run_prepared_refresh(
        &self,
        prepared: PreparedRefresh,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
    ) -> AppResult<RefreshReport> {
        let PreparedRefresh {
            settings,
            streamers,
            config,
            scope,
            streamer_id,
            bili,
            youtube,
            llm,
            ..
        } = prepared;
        let target = match streamers.first().filter(|_| streamer_id.is_some()) {
            Some(streamer) => ProgressTarget::for_streamer(streamer),
            None => ProgressTarget::all(),
        };
        let mut service = self.clone();
        // Freeze the selected clients along with the request scope. A setting
        // change creates a different task instead of silently mutating this one.
        service.bili = Arc::new(StdRwLock::new(bili));
        service.youtube = Arc::new(StdRwLock::new(youtube));
        service.llm = Arc::new(StdRwLock::new(llm));
        self.refresh_coordinator.run(scope, on_progress, move |sink: ProgressSink| async move {
            let refresh_id = uuid::Uuid::new_v4().to_string();
            let span = tracing::info_span!("refresh_round", refresh_id = %refresh_id, scope = if streamer_id.is_none() { "all" } else { "streamer" });
            measure("refresh_round", async {
                let _guard = service.wait_for_refresh_lock(&target, sink.as_ref()).await;
                if streamer_id.is_none() { service.repair_reserve_room_urls().await; }
                service.backfill_avatar_captures(&streamers).await;
                let llm = config.map(|config| PostAnalyzer {
                    config, pool: LlmPool::new(settings.llm_parallelism),
                });
                let sources = sources_for_streamers(&streamers);
                let mut result = service
                    .refresh_sources(&sources, &settings, llm.as_ref(), Some(&target), sink.as_ref())
                    .await;
                // YouTube uses an independent fetch pipeline. Run it even when
                // the Bilibili dynamic pipeline fails, then preserve the Bili error
                // as the overall refresh result.
                let youtube_result = service
                    .refresh_youtube_sources(&streamers, &settings, llm.as_ref(), Some(&target), sink.as_ref())
                    .await;
                match (&mut result, youtube_result) {
                    (Ok(report), Ok(youtube)) => {
                        report.refreshed_streamers += youtube.refreshed_streamers;
                        report.failed_streamers.extend(youtube.failed_streamers);
                        report.new_schedules += youtube.new_schedules;
                        report.cancelled_schedules += youtube.cancelled_schedules;
                        report.llm_calls += youtube.llm_calls;
                        report.llm_cache_hits += youtube.llm_cache_hits;
                    }
                    (Ok(report), Err(error)) => report
                        .failed_streamers
                        .push(format!("YouTube 刷新: {error}")),
                    (Err(_), Err(error)) => tracing::warn!(%error, "YouTube 刷新在 Bilibili 刷新失败后也失败"),
                    (Err(_), Ok(_)) => {}
                }
                let failed = result.as_ref().map_or(true, |report| !report.failed_streamers.is_empty());
                let stage = match &result {
                    Ok(_) if failed => "刷新完成，但部分来源失败".to_owned(),
                    Ok(_) => "刷新完成".to_owned(),
                    Err(error) => format!("刷新失败: {error}"),
                };
                emit_progress(&target, &stage, 1, 1, 100, true, failed, sink.as_ref());
                result
            }).instrument(span).await
        }).await
    }

    /// 拿刷新锁。锁被后台轮询占着的时候（一次全量刷新可能十几分钟），
    /// 手工触发的刷新会一直排队；这里把等待状态播报出去，界面就不会
    /// 停在「准备全量刷新 0%」看起来像卡死。
    async fn wait_for_refresh_lock<'a>(
        &'a self,
        target: &ProgressTarget,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
    ) -> tokio::sync::MutexGuard<'a, ()> {
        let queue_timer = PhaseTimer::start("refresh_queue_wait");
        if let Ok(guard) = self.refresh_lock.try_lock() {
            queue_timer.finish(true);
            return guard;
        }
        let started = std::time::Instant::now();
        let acquisition = self.refresh_lock.lock();
        tokio::pin!(acquisition);
        loop {
            emit_progress(
                target,
                &format!(
                    "等待上一轮刷新结束（已等待 {} 秒）",
                    started.elapsed().as_secs()
                ),
                0,
                1,
                1,
                false,
                false,
                on_progress,
            );
            tokio::select! {
                guard = &mut acquisition => {
                    queue_timer.finish(true);
                    return guard;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {}
            }
        }
    }

    /// 历史数据里预约卡 `rid` 被当成房间号，地址会指向别人的直播间。
    /// 清掉这类地址后，双击卡片会回退到主播自己的直播间。
    pub async fn repair_reserve_room_urls(&self) {
        match self.store.repair_misattributed_reserve_room_urls().await {
            Ok(0) => {}
            Ok(repaired) => {
                tracing::info!(repaired, "已清空错位的预约日程直播间地址");
            }
            Err(error) => tracing::warn!(%error, "修复预约日程直播间地址失败"),
        }
    }

    pub async fn refresh_streamer(&self, streamer_id: i64) -> AppResult<RefreshReport> {
        self.refresh_streamer_with_progress(streamer_id, |_| {})
            .await
    }

    pub async fn refresh_streamer_with_progress<P>(
        &self,
        streamer_id: i64,
        on_progress: P,
    ) -> AppResult<RefreshReport>
    where
        P: Fn(RefreshProgress) + Send + Sync,
    {
        let streamer = self.store.get_streamer(streamer_id).await?;
        if streamer.platform == Platform::Youtube {
            self.require_youtube_enabled().await?;
        }
        self.coordinate_refresh(Some(streamer_id), &on_progress)
            .await
    }

    async fn current_llm_config(&self, settings: &AppSettings) -> Option<LlmConfig> {
        let api_key = self.llm_key_for_base_url(&settings.llm_base_url).await;
        if api_key.is_none() && self.llm_key_binding.read().await.is_some() {
            return None;
        }
        (settings.auto_analyze_images && !settings.llm_model.trim().is_empty()).then(|| LlmConfig {
            base_url: settings.llm_base_url.clone(),
            api_key: api_key.unwrap_or_default(),
            model: settings.llm_model.clone(),
            prompt_version: settings.llm_prompt_version.clone(),
            reasoning_effort: settings.llm_reasoning_effort,
        })
    }

    async fn refresh_sources(
        &self,
        sources: &[(String, Vec<Streamer>)],
        settings: &AppSettings,
        llm: Option<&PostAnalyzer>,
        progress: Option<&ProgressTarget>,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
    ) -> AppResult<RefreshReport> {
        let fetch = |mid: String| {
            let bili = self.bili_client();
            async move { bili.fetch_dynamics(&mid, 20).await }
        };
        measure("source_pipeline", async {
            let result = self
                .refresh_sources_with_fetcher(sources, settings, llm, progress, on_progress, &fetch)
                .await;
            if let Ok(report) = &result {
                tracing::info!(
                    sources = sources.len(),
                    parallelism = settings.llm_parallelism,
                    llm_calls = report.llm_calls,
                    cache_hits = report.llm_cache_hits,
                    image_limit = POST_ANALYSIS_IMAGE_LIMIT,
                    model = llm
                        .map(|analyzer| analyzer.config.model.as_str())
                        .unwrap_or("disabled"),
                    new_schedules = report.new_schedules,
                    failed_streamers = report.failed_streamers.len(),
                    "刷新耗时统计摘要"
                );
            }
            result
        })
        .await
    }

    async fn fetch_youtube_upcoming_live_gated(
        &self,
        client: &YouTubeClient,
        channel_id: &str,
        max_results: u32,
    ) -> AppResult<Option<Vec<YouTubeVideo>>> {
        if !youtube_requests_enabled(&self.store).await {
            return Ok(None);
        }
        let Some(channel_result) =
            run_youtube_request(&self.store, client.fetch_channel_metadata(channel_id)).await
        else {
            return Ok(None);
        };
        let channel = channel_result?;
        let uploads_playlist_id = channel
            .uploads_playlist_id
            .ok_or_else(|| AppError::not_found("YouTube 频道上传列表"))?;

        if !youtube_requests_enabled(&self.store).await {
            return Ok(None);
        }
        let Some(uploads_result) = run_youtube_request(
            &self.store,
            client.fetch_recent_uploads(&uploads_playlist_id, max_results),
        )
        .await
        else {
            return Ok(None);
        };
        let uploads = uploads_result?;
        let ids = uploads
            .into_iter()
            .map(|item| item.video_id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(Some(Vec::new()));
        }

        if !youtube_requests_enabled(&self.store).await {
            return Ok(None);
        }
        let Some(videos_result) = run_youtube_request(&self.store, client.fetch_videos(&ids)).await
        else {
            return Ok(None);
        };
        let mut videos = videos_result?;
        videos.retain(|video| video.is_live() || video.is_upcoming());
        videos.sort_by(|left, right| {
            left.scheduled_start_at
                .cmp(&right.scheduled_start_at)
                .then_with(|| left.actual_start_at.cmp(&right.actual_start_at))
        });
        Ok(Some(videos))
    }

    async fn refresh_youtube_sources(
        &self,
        streamers: &[Streamer],
        settings: &AppSettings,
        llm: Option<&PostAnalyzer>,
        progress: Option<&ProgressTarget>,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
    ) -> AppResult<RefreshReport> {
        let mut report = RefreshReport {
            refreshed_streamers: 0,
            failed_streamers: Vec::new(),
            new_schedules: 0,
            cancelled_schedules: 0,
            llm_calls: 0,
            llm_cache_hits: 0,
        };
        if !youtube_requests_enabled(&self.store).await {
            return Ok(report);
        }
        let targets: Vec<Streamer> = streamers
            .iter()
            .filter(|streamer| streamer.platform == Platform::Youtube && streamer.schedule_enabled)
            .cloned()
            .collect();
        if targets.is_empty() {
            return Ok(report);
        }
        let client = self.youtube_client();
        let web_cookie = self
            .secrets
            .read()
            .await
            .youtube_web_cookie
            .clone()
            .filter(|value| !value.trim().is_empty());
        let mut event_state = RefreshEventState::default();
        let total = targets.len();
        for (index, streamer) in targets.iter().enumerate() {
            // Re-read for every channel so a switch-off between channels cannot
            // start the next community, metadata, uploads, or video request.
            if !youtube_requests_enabled(&self.store).await {
                break;
            }
            if let Some(target) = progress {
                emit_progress(
                    target,
                    &format!("抓取 YouTube 社区帖（{}/{}）", index + 1, total),
                    index,
                    total,
                    10 + (index * 80 / total) as u8,
                    false,
                    false,
                    on_progress,
                );
            }
            let targets_slice = std::slice::from_ref(streamer);
            let mut errors = Vec::new();
            let mut community_posts = Vec::new();
            let Some(community_result) = run_youtube_request(
                &self.store,
                client.fetch_community_posts(&streamer.mid, web_cookie.as_deref()),
            )
            .await
            else {
                break;
            };
            match community_result {
                Ok(fetch) => {
                    let observed_at = Utc::now();
                    let can_read_membership = web_cookie.is_some()
                        && fetch.skipped_reason.as_deref()
                            != Some("membership_session_unavailable");
                    community_posts = fetch
                        .posts
                        .into_iter()
                        .filter(|post| !post.membership_only || can_read_membership)
                        .map(|post| {
                            dynamic_post_from_youtube_community(post, streamer, observed_at)
                        })
                        .collect();
                }
                Err(error) if error.code() == "YOUTUBE_FORBIDDEN" => {
                    tracing::debug!(streamer_id = streamer.id, "YouTube 会员社区帖不可用，跳过");
                }
                Err(error) => errors.push(error.to_string()),
            }
            let mut live_videos = Vec::new();
            match self
                .fetch_youtube_upcoming_live_gated(&client, &streamer.mid, 20)
                .await
            {
                Ok(Some(videos)) => live_videos = videos,
                Ok(None) => break,
                Err(error) => errors.push(error.to_string()),
            }
            let stats = self
                .process_source_posts_with_progress(
                    &streamer.mid,
                    targets_slice,
                    &community_posts,
                    settings,
                    llm,
                    &mut event_state,
                    progress,
                    on_progress,
                    index,
                    total,
                )
                .await?;
            report.llm_calls += stats.llm_calls;
            report.llm_cache_hits += stats.llm_cache_hits;
            report.new_schedules += stats.new_schedules;
            report.cancelled_schedules += stats.cancelled_schedules;
            if let Some(error) = stats.analysis_errors.get(&streamer.id) {
                errors.push(error.clone());
            }
            let mut seen_videos = HashSet::new();
            for video in live_videos {
                if !seen_videos.insert(video.id.clone()) {
                    continue;
                }
                if self.upsert_youtube_live_schedule(streamer, &video).await? {
                    report.new_schedules += 1;
                }
            }
            if errors.is_empty() {
                self.store.record_refresh_result(streamer.id, None).await?;
                report.refreshed_streamers += 1;
            } else {
                let error = errors.join("; ");
                self.store
                    .record_refresh_result(streamer.id, Some(&error))
                    .await?;
                report
                    .failed_streamers
                    .push(format!("{}: {error}", streamer.name));
            }
        }
        report.cancelled_schedules += self.apply_pending_cancellations(&mut event_state).await?;
        match self.dedupe_schedule_duplicates().await {
            Ok(removed) if removed > 0 => {
                tracing::info!(removed, "merged duplicate schedules after YouTube refresh");
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "deduplicate schedules after YouTube refresh failed")
            }
        }
        if report.new_schedules > 0 || report.cancelled_schedules > 0 {
            self.store
                .append_event(
                    "schedule_changed",
                    &json!({
                        "action": "refresh_youtube",
                        "newSchedules": report.new_schedules,
                        "cancelledSchedules": report.cancelled_schedules
                    }),
                )
                .await?;
        }
        Ok(report)
    }
    async fn upsert_youtube_live_schedule(
        &self,
        streamer: &Streamer,
        video: &YouTubeVideo,
    ) -> AppResult<bool> {
        let Some(start_at) = video.actual_start_at.or(video.scheduled_start_at) else {
            return Ok(false);
        };
        let post = dynamic_post_from_youtube_live(video, streamer);
        let title = if video.title.trim().is_empty() {
            "YouTube 直播".to_owned()
        } else {
            clean_schedule_title_value(video.title.clone(), streamer)
        };
        let schedule = ParsedSchedule {
            title,
            start_at,
            end_at: None,
            confidence: 1.0,
        };
        let identity = format!("youtube_live:{}:schedule", video.id);
        let fingerprint = source_fingerprint(&post, &schedule.title, "youtube_live");
        let participants = [streamer.name.clone()];
        let live_url = video.live_url();
        self.upsert_candidate(
            streamer,
            &schedule,
            "youtube_live",
            &post,
            video.thumbnail_url.as_deref().unwrap_or_default(),
            &identity,
            &fingerprint,
            live_url.as_deref(),
            Some(&video.channel_id),
            Some(streamer.name.as_str()),
            &participants,
        )
        .await
    }

    // Private transport seam: fixtures exercise the real refresh pipeline,
    // while production always uses BilibiliClient above.
    async fn refresh_sources_with_fetcher<Fetch, FetchFuture>(
        &self,
        sources: &[(String, Vec<Streamer>)],
        settings: &AppSettings,
        llm: Option<&PostAnalyzer>,
        progress: Option<&ProgressTarget>,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
        fetch: &Fetch,
    ) -> AppResult<RefreshReport>
    where
        Fetch: Fn(String) -> FetchFuture + Sync,
        FetchFuture: std::future::Future<Output = AppResult<Vec<DynamicPost>>> + Send,
    {
        let mut report = RefreshReport {
            refreshed_streamers: 0,
            failed_streamers: Vec::new(),
            new_schedules: 0,
            cancelled_schedules: 0,
            llm_calls: 0,
            llm_cache_hits: 0,
        };
        let mut event_state = RefreshEventState::default();
        let mut attempted_streamer_ids = HashSet::new();
        let mut successfully_analyzed_streamer_ids = HashSet::new();
        let mut streamer_errors: HashMap<i64, String> = HashMap::new();
        let mut deferred_analyses = 0usize;
        let outcomes = self
            .process_sources_concurrently(
                sources,
                settings,
                llm,
                &mut event_state,
                progress,
                on_progress,
                fetch,
            )
            .await?;
        // Aggregate in original source order, not completion order. Error
        // attribution and deferred cancellations retain their existing semantics.
        for (index, outcome) in outcomes {
            let (source_mid, targets) = &sources[index];
            match outcome {
                SourceRefreshResult::Processed(stats) => {
                    deferred_analyses += stats.deferred_analyses;
                    report.new_schedules += stats.new_schedules;
                    report.cancelled_schedules += stats.cancelled_schedules;
                    report.llm_calls += stats.llm_calls;
                    report.llm_cache_hits += stats.llm_cache_hits;
                    for target in targets {
                        let state_key = format!("analysis-errors:v1:{}", target.id);
                        let previous = self.store.get_client_sync_state(&state_key).await?;
                        let mut unresolved: std::collections::BTreeMap<String, String> = previous
                            .as_deref()
                            .map(serde_json::from_str)
                            .transpose()?
                            .unwrap_or_default();
                        // 旧的 text:/image: 前缀属于双通道时代，单次调用后不会再更新，
                        // 留着会让前端一直显示早就解决的历史报错，这里一次性清掉。
                        unresolved.retain(|identity, _| {
                            !identity.starts_with("text:") && !identity.starts_with("image:")
                        });
                        for (identity, outcome) in &stats.analysis_outcomes {
                            if let Some(message) = outcome {
                                unresolved.insert(identity.clone(), message.clone());
                            } else {
                                unresolved.remove(identity);
                            }
                        }
                        if !stats.analysis_outcomes.is_empty() {
                            self.store
                                .set_client_sync_state(
                                    &state_key,
                                    &serde_json::to_string(&unresolved)?,
                                )
                                .await?;
                        }
                        if let Some(error) = unresolved.values().next() {
                            streamer_errors
                                .entry(target.id)
                                .or_insert_with(|| error.clone());
                        }
                        attempted_streamer_ids.insert(target.id);
                        if let Some(error) = stats.analysis_errors.get(&target.id) {
                            streamer_errors
                                .entry(target.id)
                                .or_insert_with(|| error.clone());
                        } else if stats.analyzed_streamer_ids.contains(&target.id) {
                            successfully_analyzed_streamer_ids.insert(target.id);
                        }
                    }
                }
                SourceRefreshResult::FetchFailed(error) => {
                    let message = error.to_string();
                    for target in targets {
                        attempted_streamer_ids.insert(target.id);
                        streamer_errors
                            .entry(target.id)
                            .or_insert_with(|| message.clone());
                    }
                    report
                        .failed_streamers
                        .push(format!("{source_mid}: {message}"));
                }
            }
        }
        let mut refreshed_streamer_ids = HashSet::new();
        let mut recorded_streamer_ids = HashSet::new();
        for (_, targets) in sources {
            for target in targets {
                if !attempted_streamer_ids.contains(&target.id)
                    || !recorded_streamer_ids.insert(target.id)
                {
                    continue;
                }
                let error = streamer_errors.get(&target.id).map(String::as_str);
                match error {
                    Some(error) => {
                        self.store
                            .record_refresh_result(target.id, Some(error))
                            .await?;
                        report
                            .failed_streamers
                            .push(format!("{}: {error}", target.name));
                    }
                    None if successfully_analyzed_streamer_ids.contains(&target.id) => {
                        self.store.record_refresh_result(target.id, None).await?;
                        refreshed_streamer_ids.insert(target.id);
                    }
                    None => {
                        // A successful fetch with no analysable content must not erase
                        // an error from an earlier analysis attempt. Keep it visible
                        // until this streamer's content is actually analysed again.
                        self.store
                            .record_refresh_result_preserving_error(target.id)
                            .await?;
                        refreshed_streamer_ids.insert(target.id);
                    }
                }
            }
        }
        report.cancelled_schedules += self.apply_pending_cancellations(&mut event_state).await?;
        match self.dedupe_schedule_duplicates().await {
            Ok(removed) if removed > 0 => {
                tracing::info!(removed, "merged duplicate schedules");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "deduplicate schedules failed"),
        }
        report.refreshed_streamers = refreshed_streamer_ids.len();
        tracing::info!(deferred_analyses, "本轮动态失败退避汇总");
        if report.new_schedules > 0 || report.cancelled_schedules > 0 {
            self.store
                .append_event(
                    "schedule_changed",
                    &json!({
                        "action": "refresh",
                        "newSchedules": report.new_schedules,
                        "cancelledSchedules": report.cancelled_schedules
                    }),
                )
                .await?;
        }
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_sources_concurrently<Fetch, FetchFuture>(
        &self,
        sources: &[(String, Vec<Streamer>)],
        settings: &AppSettings,
        llm: Option<&PostAnalyzer>,
        event_state: &mut RefreshEventState,
        progress: Option<&ProgressTarget>,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
        fetch: &Fetch,
    ) -> AppResult<Vec<(usize, SourceRefreshResult)>>
    where
        Fetch: Fn(String) -> FetchFuture + Sync,
        FetchFuture: std::future::Future<Output = AppResult<Vec<DynamicPost>>> + Send,
    {
        let limit = usize::from(settings.llm_parallelism.clamp(1, 8));
        let mut pending: Vec<usize> = (0..sources.len()).collect();
        let mut active_targets = HashSet::new();
        // Declare shared state before the future set so cancellation drops all
        // borrowing futures before destroying their progress/pacing state.
        let source_progress = StdMutex::new(vec![0u8; sources.len()]);
        let next_fetch_start = Mutex::new(tokio::time::Instant::now());
        let mut running = FuturesUnordered::new();
        let mut outcomes: Vec<Option<SourceRefreshResult>> =
            (0..sources.len()).map(|_| None).collect();
        let mut cancellations: Vec<Vec<PendingCancellation>> =
            (0..sources.len()).map(|_| Vec::new()).collect();
        while !pending.is_empty() || !running.is_empty() {
            while running.len() < limit {
                let Some(position) = next_ready_source(sources, &pending, &active_targets) else {
                    break;
                };
                let index = pending.remove(position);
                let (source_mid, targets) = &sources[index];
                active_targets.extend(targets.iter().map(|target| target.id));
                let mut local_state = RefreshEventState {
                    cancellations: Vec::new(),
                    written_identities: event_state.written_identities.clone(),
                };
                let source_progress = &source_progress;
                let next_fetch_start = &next_fetch_start;
                let source_span = tracing::info_span!("refresh_source", source_mid = %source_mid);
                running.push(async move {
                    let source_timer = PhaseTimer::start("source_refresh");
                    let label = targets
                        .iter()
                        .map(|target| target.name.as_str())
                        .collect::<Vec<_>>()
                        .join("、");
                    let report = |mut update: RefreshProgress| {
                        let mut fractions = source_progress
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        fractions[index] = fractions[index].max(update.percent.min(100));
                        update.percent = (5 + fractions
                            .iter()
                            .map(|fraction| usize::from(*fraction))
                            .sum::<usize>()
                            * 80
                            / (sources.len().max(1) * 100))
                            as u8;
                        update.completed = fractions
                            .iter()
                            .filter(|fraction| **fraction == 100)
                            .count();
                        update.total = sources.len();
                        update.stage = format!("{label}：{}", update.stage);
                        // All callbacks for this coordinator are emitted in poll order.
                        drop(fractions);
                        on_progress(update);
                    };
                    if let Some(target) = progress {
                        emit_progress(target, "准备拉取动态", 0, 1, 0, false, false, &report);
                    }
                    {
                        let mut next = next_fetch_start.lock().await;
                        tokio::time::sleep_until(*next).await;
                        *next = tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SOURCE_FETCH_START_INTERVAL_MS);
                    }
                    let result = match measure("source_fetch", fetch(source_mid.clone())).await {
                        Ok(posts) => self
                            .process_source_posts_with_progress(
                                source_mid,
                                targets,
                                &posts,
                                settings,
                                llm,
                                &mut local_state,
                                progress,
                                &report,
                                0,
                                1,
                            )
                            .await
                            .map(SourceRefreshResult::Processed),
                        Err(error) => Ok(SourceRefreshResult::FetchFailed(error)),
                    };
                    if let (Some(target), Ok(outcome)) = (progress, &result) {
                        let failed = matches!(outcome, SourceRefreshResult::FetchFailed(_));
                        let stage = if failed {
                            "拉取动态失败"
                        } else {
                            "动态处理完成"
                        };
                        emit_progress(target, stage, 1, 1, 100, false, failed, &report);
                    }
                    source_timer.finish(matches!(&result, Ok(SourceRefreshResult::Processed(stats)) if stats.analysis_errors.is_empty()));
                    (index, local_state, result)
                }.instrument(source_span));
            }
            let Some((index, local_state, result)) = running.next().await else {
                break;
            };
            let outcome = result?;
            for target in &sources[index].1 {
                active_targets.remove(&target.id);
            }
            for (post_id, identities) in local_state.written_identities {
                event_state
                    .written_identities
                    .entry(post_id)
                    .or_default()
                    .extend(identities);
            }
            cancellations[index] = local_state.cancellations;
            outcomes[index] = Some(outcome);
        }
        // A late cancellation must see all schedules, even if another streamer
        // took longer to analyse. Preserve the original source traversal order.
        for pending in cancellations {
            event_state.cancellations.extend(pending);
        }
        outcomes
            .into_iter()
            .enumerate()
            .map(|(index, outcome)| {
                outcome
                    .map(|outcome| (index, outcome))
                    .ok_or_else(|| AppError::validation("动态刷新任务未完成"))
            })
            .collect()
    }

    #[cfg(test)]
    async fn process_source_posts(
        &self,
        source_mid: &str,
        targets: &[Streamer],
        posts: &[DynamicPost],
        settings: &AppSettings,
        llm: Option<&PostAnalyzer>,
    ) -> AppResult<ProcessStats> {
        let mut event_state = RefreshEventState::default();
        let mut stats = self
            .process_source_posts_with_progress(
                source_mid,
                targets,
                posts,
                settings,
                llm,
                &mut event_state,
                None,
                &|_| {},
                0,
                1,
            )
            .await?;
        stats.cancelled_schedules += self.apply_pending_cancellations(&mut event_state).await?;
        Ok(stats)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_source_posts_with_progress(
        &self,
        source_mid: &str,
        targets: &[Streamer],
        posts: &[DynamicPost],
        settings: &AppSettings,
        llm: Option<&PostAnalyzer>,
        event_state: &mut RefreshEventState,
        progress: Option<&ProgressTarget>,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
        source_index: usize,
        source_count: usize,
    ) -> AppResult<ProcessStats> {
        let now = Utc::now();
        let ordered_posts = ordered_posts_oldest_first(posts);
        let posts = ordered_posts.as_slice();
        let mut stats = ProcessStats::default();
        // 一条动态只调一次模型，进度也按动态条数计步：9 张图的动态以前要发
        // 10 次请求、占 10 步，现在只占 1 步。
        let total_steps = posts.len().max(1);
        let mut completed_steps = 0;
        let source_base = 10 + (source_index * 80 / source_count.max(1)) as u8;
        let source_span = (80 / source_count.max(1)).saturating_sub(5).max(1) as u8;
        let report_step =
            |stage: &str, completed_steps: usize, total_steps: usize, failed: bool| {
                let Some(target) = progress else {
                    return;
                };
                let fraction = completed_steps
                    .checked_mul(60)
                    .and_then(|value| value.checked_div(total_steps))
                    .unwrap_or(60) as u8;
                emit_progress(
                    target,
                    stage,
                    completed_steps,
                    total_steps,
                    (source_base as usize + source_span as usize * fraction as usize / 60) as u8,
                    false,
                    failed,
                    on_progress,
                );
            };
        for (post_index, post) in posts.iter().enumerate() {
            report_step(
                &format!("分析动态日程（{}/{}）", post_index + 1, posts.len()),
                completed_steps,
                total_steps,
                false,
            );
            tracing::info!(post_id = %post.id, source_mid, images_total = post.images.len(), images_selected = post.images.len().min(POST_ANALYSIS_IMAGE_LIMIT), "动态识别输入统计");
            measure("post_store", self.store.save_dynamic_post(post))
                .instrument(tracing::info_span!("post", post_id = %post.id))
                .await?;
            let reserve_schedule = post
                .reserve
                .as_ref()
                .and_then(|reserve| parse_reserve_schedule(reserve, post.published_at, now));
            let reserve_start = reserve_schedule.as_ref().map(|schedule| schedule.start_at);
            if reserve_schedule.is_none()
                && post
                    .reserve
                    .as_ref()
                    .is_some_and(|reserve| !reserve.is_live())
            {
                stats.cancelled_schedules +=
                    self.remove_stale_non_live_reserve_schedule(post).await?;
            }

            // 本轮这条动态涉及的全部 source_identity。清理只删「当前分析里已经没有
            // 第 N 条事件」的来源，这里登记过的无论尾号如何都不动：被去重、被跳过、
            // 或者换了个摘要的场次都还存在，只是这一轮没有重新写到而已。
            let mut keep_identities: HashSet<String> = event_state
                .written_identities
                .entry(post.id.clone())
                .or_default()
                .clone();
            // 这一轮每条事件对应的编号。数组序号即事件序号：清理时用它判断
            // 「第 N 条事件这一轮是不是被新编号重新覆盖了」。
            let mut slot_identities: HashMap<usize, String> = HashMap::new();
            // 纯文字动态的 identity 仍沿用「发布者 + 正文 + 预约卡」的旧摘要，
            // 升级后历史日程才能原地更新，而不是再造一条。
            let text_hash = text_digest(&text_analysis_context(post));
            let reserve = reserve_summary(post);
            let content_digest = content_digest_of(&text_hash, reserve.as_deref(), &post.images);
            if let Some(identity) = self
                .apply_deterministic_reservation_time_change(source_mid, targets, post)
                .await?
            {
                keep_identities.insert(identity);
            }

            // 带图动态与纯文字动态走同一条通道：正文（含预约卡）与前两图
            // 只在这一次请求里送出，任何一侧出现时间点都能被提取。
            let has_images = !post.images.is_empty();
            let analysis_source_kind = post_source_kind(post, has_images);
            // 卡片那一场现在也由模型给出：命中同槽就记下“已覆盖”，
            // 后面不再补正则兜底。模型给的取消同样采信。
            let mut card_slot_covered = false;
            // 模型偶尔把同一场说两遍（正文一次、图片一次），按时间槽去重。
            let mut applied_events: Vec<LlmEvent> = Vec::new();
            let mut analysis_succeeded = false;
            // 这一轮模型给出的有效事件条数，清理来源时按序号判断哪些已经不存在。
            let mut event_count = 0usize;
            // 既没有正文也没有图片的动态没有东西可送模型：不占一次调用，
            // 但仍然算“分析成功”，让上一版提示词留下的旧来源照常清理。
            let has_content = !post.text.trim().is_empty() || !post.images.is_empty();
            if llm.is_some() && !has_content {
                analysis_succeeded = true;
                stats
                    .analysis_outcomes
                    .insert(format!("post:{}", post.id), None);
            }
            if let Some(analyzer) = llm.filter(|_| has_content) {
                // v5 区分转发者与原作者，不复用缺少转发上下文的结果。
                let prompt_version = format!("{}-post-v5", settings.llm_prompt_version);
                // 取一个并发许可：同一时刻最多 llm_parallelism 个请求在跑。
                let config = LlmConfig {
                    base_url: analyzer.config.base_url.clone(),
                    api_key: analyzer.config.api_key.clone(),
                    model: analyzer.config.model.clone(),
                    prompt_version: prompt_version.clone(),
                    reasoning_effort: settings.llm_reasoning_effort,
                };
                let cache_key = format!(
                    "{}:{content_digest}:{}:{prompt_version}",
                    post.id, config.model
                );
                let analysis_input = PostAnalysisInput {
                    author: post.author_name.trim().to_owned(),
                    published_at: post.published_at,
                    text: post.text.clone(),
                    reserve,
                    image_urls: post
                        .images
                        .iter()
                        .take(POST_ANALYSIS_IMAGE_LIMIT)
                        .cloned()
                        .collect(),
                };
                let cached_events = match measure(
                    "cache_lookup",
                    self.store.load_post_analysis_entry(&cache_key),
                )
                .instrument(tracing::info_span!("post", post_id = %post.id))
                .await?
                {
                    Some((raw, created_at)) => match events_from_raw(&raw, now) {
                        Ok(events) if !events.is_empty() => {
                            stats.llm_cache_hits += 1;
                            Some(events)
                        }
                        // 空结果也进缓存，但只认一段时间：模型偶尔看走眼，
                        // 不能让它永久失去机会，也不能每轮都把全部动态重问一遍。
                        Ok(events) if empty_analysis_is_fresh(&created_at, now) => {
                            stats.llm_cache_hits += 1;
                            Some(events)
                        }
                        Ok(_) => {
                            tracing::debug!(
                                post_id = %post.id,
                                "cached empty post analysis expired, retry"
                            );
                            None
                        }
                        Err(error) => {
                            tracing::warn!(%error, post_id = %post.id, "cached post analysis invalid");
                            None
                        }
                    },
                    None => None,
                };
                let retry_content = text_digest(
                    &json!({
                        "digest": content_digest, "published": post.published_at,
                        "author": post.author_mid, "author_name": post.author_name,
                    })
                    .to_string(),
                );
                let retry_config = text_digest(
                    &json!({
                        "model": config.model, "base_url": config.base_url,
                        "prompt": config.prompt_version, "reasoning": config.reasoning_effort,
                        "credential": text_digest(&config.api_key),
                    })
                    .to_string(),
                );
                let retry_key =
                    analysis_retry::retry_state_key(&post.id, &retry_content, &retry_config);
                let events = match cached_events {
                    Some(events) => {
                        analysis_retry::clear(&self.store, &retry_key).await?;
                        Some(events)
                    }
                    None => {
                        let waiting = analysis_retry::load(&self.store, &retry_key)
                            .await?
                            .filter(|state| !state.is_due(Utc::now()));
                        if let Some(state) = waiting {
                            let message = format!("识别暂缓重试：{}", state.retry_message());
                            tracing::info!(post_id = %post.id, next_retry_at = %state.next_retry_at, "动态分析处于失败退避，未调用模型");
                            stats.deferred_analyses += 1;
                            self.record_post_analysis_error(targets, post, &message, &mut stats)
                                .await?;
                            None
                        } else {
                            let slot_timer = PhaseTimer::start("model_slot_wait");
                            let permit = analyzer.pool.acquire().await;
                            slot_timer.finish(true);
                            let analysis = self
                                .llm_analyzer()
                                .analyze_post(&config, &analysis_input)
                                .instrument(tracing::info_span!("post", post_id = %post.id))
                                .await;
                            drop(permit);
                            match analysis {
                                Ok(analysis) => {
                                    stats.llm_calls += 1;
                                    measure(
                                        "cache_store",
                                        self.store.save_post_analysis(
                                            &cache_key,
                                            &post.id,
                                            &content_digest,
                                            &config.model,
                                            &prompt_version,
                                            &analysis.raw,
                                        ),
                                    )
                                    .instrument(tracing::info_span!("post", post_id = %post.id))
                                    .await?;
                                    analysis_retry::clear(&self.store, &retry_key).await?;
                                    Some(analysis.events)
                                }
                                Err(error) => {
                                    // Never persist credential-bearing upstream messages.
                                    let detail = if config.api_key.is_empty() {
                                        error.to_string()
                                    } else {
                                        error.to_string().replace(&config.api_key, "[REDACTED]")
                                    };
                                    let state = analysis_retry::failure(
                                        &self.store,
                                        &retry_key,
                                        Utc::now(),
                                        &format!("动态日程分析失败: {detail}"),
                                    )
                                    .await?;
                                    let message = state.retry_message();
                                    tracing::warn!(post_id = %post.id, error_code = error.code(), next_retry_at = %state.next_retry_at, "动态日程分析失败，已安排退避重试");
                                    self.record_post_analysis_error(
                                        targets, post, &message, &mut stats,
                                    )
                                    .await?;
                                    None
                                }
                            }
                        }
                    }
                };
                event_count = events.as_ref().map_or(0, Vec::len);
                analysis_succeeded = events.is_some();
                if analysis_succeeded {
                    stats
                        .analysis_outcomes
                        .insert(format!("post:{}", post.id), None);
                    stats
                        .analyzed_streamer_ids
                        .extend(targets.iter().map(|target| target.id));
                }
                for (event_index, event) in events.unwrap_or_default().iter().enumerate() {
                    let event = canonicalize_event_participants(event, targets);
                    // 先把 identity 算出来：这一轮即使因为去重、或者找不到对应
                    // 主播而没写进库里，也不代表它已经不存在了，清理时不能删。
                    let identity = event_source_identity(
                        &post.id,
                        analysis_source_kind,
                        &text_hash,
                        &content_digest,
                        event_index,
                    );
                    if applied_events
                        .iter()
                        .any(|seen| llm_events_equivalent(seen, &event))
                    {
                        // 同一场直播模型说了两遍：只落一条。
                        continue;
                    }
                    applied_events.push(event.clone());
                    if let Some(reserve_start) = reserve_start {
                        if event.start_at.is_some_and(|start_at| {
                            same_schedule_slot(
                                start_at,
                                reserve_start,
                                RESERVE_DUPLICATE_TOLERANCE_MINUTES,
                            )
                        }) {
                            card_slot_covered = true;
                        }
                    }
                    let Some(target) =
                        match_event_target_for_post(&event, targets, &post.author_mid)
                    else {
                        continue;
                    };
                    if event.action == LlmEventAction::Cancel {
                        event_state.cancellations.push(PendingCancellation {
                            target,
                            event: event.clone(),
                            source_post_id: post.id.clone(),
                            published_at: post.published_at,
                        });
                        continue;
                    }
                    let source_kind = analysis_source_kind;
                    let fingerprint = format!(
                        "{}:{}:{}:{}",
                        source_fingerprint(post, &event.title, source_kind),
                        event_index,
                        event.performer,
                        event.participant_names().join("|")
                    );
                    // 多张图只发一次请求，日程卡片沿用第一张图当封面。
                    let image_url = post.images.first().map(String::as_str).unwrap_or_default();
                    let (inserted, cancelled) = self
                        .apply_llm_event(
                            &target,
                            targets,
                            &event,
                            source_kind,
                            post,
                            image_url,
                            &identity,
                            &fingerprint,
                        )
                        .await?;
                    // 真写进去了才算「这一序号已被新编号覆盖」，也才是绝不会被
                    // 清理的保留来源。没写进去的序号不登记，旧来源原样留着。
                    slot_identities.insert(event_index, identity.clone());
                    keep_identities.insert(identity);
                    if inserted {
                        stats.new_schedules += 1;
                    }
                    if cancelled {
                        stats.cancelled_schedules += 1;
                    }
                }
            }

            // 兜底：模型不可用、调用失败、返回空事件，或模型没覆盖卡片那一场时，
            // 用卡片正则补一条 reserve 日程，保证卡片场次不会因为模型漏掉而消失。
            if let Some(schedule) = reserve_schedule.as_ref().filter(|_| !card_slot_covered) {
                let context = reserve_context(post);
                if let Some(target) = choose_text_target(source_mid, targets, &context) {
                    let fingerprint = source_fingerprint(post, &schedule.title, "reserve");
                    let identity = format!("reserve:{}", post.id);
                    let participants = [target.name.clone()];
                    // 预约卡的 `rid` 是预约 id，不是房间号；只有 jump_url 里能确认的
                    // 房间号才可信，否则回退到主播自己的直播间。
                    let reserve_room_url = post
                        .reserve
                        .as_ref()
                        .and_then(|reserve| reserve.live_room_id())
                        .filter(|room_id| {
                            !room_id.is_empty()
                                && room_id.chars().all(|character| character.is_ascii_digit())
                        })
                        .map(|room_id| format!("https://live.bilibili.com/{room_id}"));
                    let inserted = self
                        .upsert_candidate(
                            &target,
                            schedule,
                            "reserve",
                            post,
                            "editor",
                            &identity,
                            &fingerprint,
                            reserve_room_url.as_deref().or(target.live_url.as_deref()),
                            Some(target.mid.as_str()),
                            Some(target.name.as_str()),
                            &participants,
                        )
                        .await?;
                    // 兜底卡片也是本轮真实写入的来源，不能被随后的清理删掉。
                    keep_identities.insert(identity);
                    if inserted {
                        stats.new_schedules += 1;
                    }
                }
            }

            // 清理这条动态的历史来源：只删「当前分析里已经没有第 N 条事件」的来源
            // 清理这条动态的历史来源，只认「事件序号」不认「这一轮写没写」：
            // 尾号超出当前事件条数、或者同一序号已经被本轮新编号覆盖的来源才清掉。
            //
            // 摘要一变就把旧编号当脏数据删掉是错误的：那会连带触发
            // `delete_orphan_schedules` 把整条日程删了，这正是同一场直播时有时无
            // 的原因（周表里思诺的单人场次就是这么丢的）。分析失败、或者一条事件
            // 都没识别出来时同样一条都不动。
            //
            // 同一条动态可能同时出现在主播本人和官号两个来源里，两条路径各自写入
            // 自己的 identity；保留集合按整轮并集维护，避免互相把对方删掉。
            if analysis_succeeded && event_count > 0 {
                event_state
                    .written_identities
                    .insert(post.id.clone(), keep_identities.clone());
                let keep: Vec<String> = keep_identities.iter().cloned().collect();
                let removed = self
                    .store
                    .prune_schedule_sources_beyond(&post.id, event_count, &keep, &slot_identities)
                    .await?;
                if !removed.is_empty() {
                    // 来源被清空的日程如果一条来源都不剩，就从库里删掉；
                    // 手动锁定或还有其它来源的场次由 store 侧跳过。
                    let mut orphan_schedule_ids: Vec<i64> = removed
                        .iter()
                        .map(|(_, schedule_id)| *schedule_id)
                        .collect();
                    orphan_schedule_ids.sort_unstable();
                    orphan_schedule_ids.dedup();
                    let deleted = self
                        .store
                        .delete_orphan_schedules(&orphan_schedule_ids)
                        .await?;
                    if !deleted.is_empty() {
                        tracing::info!(
                            post_id = %post.id,
                            deleted_schedules = ?deleted,
                            "removed orphan schedules left by stale sources"
                        );
                    }
                    tracing::info!(
                        post_id = %post.id,
                        removed = removed.len(),
                        orphan_schedules = orphan_schedule_ids.len(),
                        "pruned stale schedule sources"
                    );
                }
            }

            completed_steps += 1;
            report_step("写入日程结果", completed_steps, total_steps, false);
        }
        Ok(stats)
    }

    async fn record_post_analysis_error(
        &self,
        targets: &[Streamer],
        post: &DynamicPost,
        message: &str,
        stats: &mut ProcessStats,
    ) -> AppResult<()> {
        stats
            .analysis_outcomes
            .insert(format!("post:{}", post.id), Some(message.to_owned()));
        for target in targets {
            self.store.record_streamer_error(target.id, message).await?;
            stats
                .analysis_errors
                .entry(target.id)
                .or_insert_with(|| message.to_owned());
        }
        Ok(())
    }

    async fn dedupe_schedule_duplicates(&self) -> AppResult<usize> {
        let mut schedules = self.store.list_all_schedules().await?;
        schedules.sort_by_key(|schedule| (!schedule.manual_locked, schedule.id));
        let mut removed_ids = HashSet::new();
        let mut removed = 0;
        for index in 0..schedules.len() {
            if removed_ids.contains(&schedules[index].id) {
                continue;
            }
            let mut base = schedules[index].clone();
            for duplicate in schedules.iter().skip(index + 1) {
                if removed_ids.contains(&duplicate.id)
                    || (base.manual_locked && duplicate.manual_locked)
                    || !schedules_equivalent(&base, duplicate)
                {
                    continue;
                }
                let duplicate_input = schedule_to_new_input(duplicate);
                let merged =
                    (!base.manual_locked).then(|| merge_group_schedule(&base, &duplicate_input));
                if self
                    .store
                    .merge_and_delete_schedule_duplicate(
                        base.id,
                        base.revision,
                        duplicate.id,
                        duplicate.revision,
                        merged.as_ref(),
                    )
                    .await?
                {
                    removed_ids.insert(duplicate.id);
                    removed += 1;
                    base = self.store.get_schedule(base.id).await?;
                }
            }
        }
        Ok(removed)
    }

    async fn apply_pending_cancellations(
        &self,
        event_state: &mut RefreshEventState,
    ) -> AppResult<usize> {
        let mut cancelled = 0;
        for pending in std::mem::take(&mut event_state.cancellations) {
            cancelled += self
                .cancel_schedule_from_event(
                    &pending.target,
                    &pending.event,
                    &pending.source_post_id,
                    pending.published_at,
                )
                .await?;
        }
        Ok(cancelled)
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_llm_event(
        &self,
        target: &Streamer,
        targets: &[Streamer],
        event: &LlmEvent,
        source_kind: &str,
        post: &DynamicPost,
        image_url: &str,
        source_identity: &str,
        source_fingerprint: &str,
    ) -> AppResult<(bool, bool)> {
        if event.action == LlmEventAction::Cancel {
            let cancelled = self
                .cancel_schedule_from_event(target, event, &post.id, post.published_at)
                .await?
                > 0;
            return Ok((false, cancelled));
        }
        let start_at = event
            .start_at
            .ok_or_else(|| AppError::validation("日程事件缺少开始时间"))?;
        let schedule = ParsedSchedule {
            title: clean_schedule_title_value(event.title.clone(), target),
            start_at,
            end_at: event.end_at,
            confidence: event.confidence,
        };
        let participants = event.participant_names();
        let resolved_room = self
            .resolve_event_room(event, target, targets, &post.author_mid)
            .await;
        let inserted = self
            .upsert_candidate(
                target,
                &schedule,
                source_kind,
                post,
                image_url,
                source_identity,
                source_fingerprint,
                resolved_room.url.as_deref(),
                resolved_room.owner_mid.as_deref(),
                resolved_room.owner_name.as_deref(),
                &participants,
            )
            .await?;
        Ok((inserted, false))
    }

    async fn resolve_event_room(
        &self,
        event: &LlmEvent,
        target: &Streamer,
        targets: &[Streamer],
        source_mid: &str,
    ) -> ResolvedRoom {
        if target.platform == Platform::Youtube {
            let event_url = event
                .room_url
                .as_deref()
                .filter(|value| valid_youtube_room_url(value));
            let fallback = target
                .live_url
                .clone()
                .or_else(|| target.profile_url.clone())
                .unwrap_or_default();
            return ResolvedRoom::from_target(
                event_url.map_or(fallback, ToOwned::to_owned),
                target.clone(),
            );
        }
        let participants = event.participant_names();
        let owner_hint = event
            .room_owner
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let owner_mid_hint = event
            .room_owner_mid
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());

        if let Some(owner_mid) = owner_mid_hint {
            if let Some(owner) = targets.iter().find(|candidate| candidate.mid == owner_mid) {
                if let Some(room_url) = event
                    .room_url
                    .as_deref()
                    .filter(|value| valid_live_room_url(value))
                {
                    return ResolvedRoom::from_target(room_url.to_owned(), owner.clone());
                }
                return ResolvedRoom::from_target(
                    owner.live_url.clone().unwrap_or_default(),
                    owner.clone(),
                );
            }
            if event.room_url.is_none() {
                if let Ok(Some(room_id)) = self.bili_client().resolve_room_id(owner_mid).await {
                    let url = format!("https://live.bilibili.com/{room_id}");
                    if let Ok(Some(owner)) = self.bili_client().fetch_room_owner(&room_id).await {
                        return ResolvedRoom {
                            url: Some(url),
                            owner_mid: Some(owner.mid),
                            owner_name: Some(owner.name),
                        };
                    }
                    return ResolvedRoom {
                        url: Some(url),
                        owner_mid: Some(owner_mid.to_owned()),
                        owner_name: owner_hint.map(ToOwned::to_owned),
                    };
                }
            }
        }

        if let Some(room_url) = event
            .room_url
            .as_deref()
            .filter(|value| valid_live_room_url(value))
        {
            if let Some(room_id) = live_room_id(room_url) {
                if let Some(owner) = target_for_room_id(targets, &room_id) {
                    return ResolvedRoom::from_target(room_url.to_owned(), owner.clone());
                }
                if owner_hint.is_none() {
                    if let Ok(Some(owner)) = self.bili_client().fetch_room_owner(&room_id).await {
                        return ResolvedRoom {
                            url: Some(room_url.to_owned()),
                            owner_mid: Some(owner.mid),
                            owner_name: Some(owner.name),
                        };
                    }
                }
            }
            if let Some(hint) = owner_hint {
                let owner =
                    single_matching_target(&[normalize_alias(hint)], event.confidence, targets);
                return ResolvedRoom {
                    url: Some(room_url.to_owned()),
                    owner_mid: owner.as_ref().map(|value| value.mid.clone()),
                    owner_name: Some(owner.map_or_else(|| hint.to_owned(), |value| value.name)),
                };
            }
            return ResolvedRoom {
                url: Some(room_url.to_owned()),
                owner_mid: None,
                owner_name: None,
            };
        }

        if let Some(hint) = owner_hint {
            if let Some(owner) =
                single_matching_target(&[normalize_alias(hint)], event.confidence, targets)
            {
                return ResolvedRoom::from_target(
                    owner.live_url.clone().unwrap_or_default(),
                    owner,
                );
            }
        }

        if let Some(source_target) = targets.iter().find(|candidate| candidate.mid == source_mid) {
            if participants.iter().any(|participant| {
                normalize_alias(participant) == normalize_alias(&source_target.name)
            }) {
                return ResolvedRoom::from_target(
                    source_target.live_url.clone().unwrap_or_default(),
                    source_target.clone(),
                );
            }
        }

        if !event.is_group() {
            return ResolvedRoom::from_target(
                target.live_url.clone().unwrap_or_default(),
                target.clone(),
            );
        }
        ResolvedRoom {
            url: None,
            owner_mid: None,
            owner_name: owner_hint.map(ToOwned::to_owned),
        }
    }

    async fn remove_stale_non_live_reserve_schedule(&self, post: &DynamicPost) -> AppResult<usize> {
        let source_identity = format!("reserve:{}", post.id);
        let mut removed = 0;
        for schedule in self.store.list_all_schedules().await? {
            if schedule.manual_locked
                || schedule.source_kind != "reserve"
                || schedule.source_post_id.as_deref() != Some(post.id.as_str())
                || schedule.source_identity.as_deref() != Some(source_identity.as_str())
            {
                continue;
            }
            self.store.delete_schedule(schedule.id).await?;
            removed += 1;
        }
        Ok(removed)
    }

    async fn find_reservation_time_change_candidate(
        &self,
        target: &Streamer,
        start_at: DateTime<Utc>,
        post: &DynamicPost,
    ) -> AppResult<Option<Schedule>> {
        if !text_looks_like_schedule_time_change(&post.text) {
            return Ok(None);
        }
        let candidates: Vec<Schedule> = self
            .store
            .list_all_schedules()
            .await?
            .into_iter()
            .filter(|schedule| {
                schedule.streamer_id == target.id
                    && schedule.source_kind == "reserve"
                    && !schedule.cancelled
                    && !schedule.manual_locked
                    && schedule.source_author_mid.as_deref() == Some(post.author_mid.as_str())
                    && schedule
                        .source_published_at
                        .is_none_or(|published_at| published_at <= post.published_at)
                    && {
                        let minutes = (schedule.start_at - start_at).num_minutes().abs();
                        minutes > 0 && minutes <= 240
                    }
            })
            .collect();
        Ok((candidates.len() == 1).then(|| candidates[0].clone()))
    }

    async fn apply_deterministic_reservation_time_change(
        &self,
        source_mid: &str,
        targets: &[Streamer],
        post: &DynamicPost,
    ) -> AppResult<Option<String>> {
        if post.reserve.is_some() || !text_looks_like_schedule_time_change(&post.text) {
            return Ok(None);
        }
        let Some(target) = choose_text_target(source_mid, targets, &post.text) else {
            return Ok(None);
        };
        // Relative dates such as “tomorrow” refer to the post publication date, not
        // the later refresh time. Matching still requires a unique existing reservation.
        let Some(schedule) =
            parse_text_schedule(&post.text, post.published_at, post.published_at, "直播")
        else {
            return Ok(None);
        };
        let Some(existing) = self
            .find_reservation_time_change_candidate(&target, schedule.start_at, post)
            .await?
        else {
            return Ok(None);
        };
        let fingerprint = source_fingerprint(post, &schedule.title, "text");
        let digest = text_digest(post.text.trim());
        let identity = format!("text:{}:{digest}:0", post.id);
        let participants = [target.name.clone()];
        self.upsert_candidate(
            &target,
            &schedule,
            "text",
            post,
            "editor",
            &identity,
            &fingerprint,
            target.live_url.as_deref().or(existing.room_url.as_deref()),
            Some(post.author_mid.as_str()),
            Some(post.author_name.as_str()),
            &participants,
        )
        .await?;
        // 这条 identity 也属于本次真实写入，stale 清理必须保留它。
        Ok(Some(identity))
    }

    async fn cancel_schedule_from_event(
        &self,
        target: &Streamer,
        event: &LlmEvent,
        source_post_id: &str,
        published_at: DateTime<Utc>,
    ) -> AppResult<usize> {
        let mut candidates = if let Some(start_at) = event.start_at {
            self.store
                .list_schedules_near(target.id, start_at, 10)
                .await?
        } else {
            self.cancellation_window_candidates(target.id, published_at)
                .await?
        };
        candidates.retain(|schedule| {
            cancellation_candidate_is_eligible(schedule, source_post_id, published_at)
        });
        let mut selected = select_cancellation_candidates(&candidates, &event.title)
            .into_iter()
            .map(|schedule| schedule.id)
            .collect::<Vec<_>>();
        if selected.is_empty() && event.start_at.is_some() {
            let mut fallback = self
                .cancellation_window_candidates(target.id, published_at)
                .await?;
            fallback.retain(|schedule| {
                cancellation_candidate_is_eligible(schedule, source_post_id, published_at)
            });
            selected = select_cancellation_candidates(&fallback, &event.title)
                .into_iter()
                .map(|schedule| schedule.id)
                .collect();
        }
        for schedule_id in &selected {
            self.store.delete_schedule(*schedule_id).await?;
        }
        Ok(selected.len())
    }

    async fn cancellation_window_candidates(
        &self,
        streamer_id: i64,
        published_at: DateTime<Utc>,
    ) -> AppResult<Vec<Schedule>> {
        let from = published_at - Duration::hours(12);
        let to = published_at + Duration::days(14);
        Ok(self
            .store
            .list_all_schedules()
            .await?
            .into_iter()
            .filter(|schedule| {
                schedule.streamer_id == streamer_id
                    && !schedule.cancelled
                    && schedule.start_at >= from
                    && schedule.start_at <= to
            })
            .collect())
    }

    async fn resolve_auto_schedule_title(
        &self,
        streamer_id: i64,
        title: &str,
        start_at: DateTime<Utc>,
    ) -> AppResult<String> {
        if !schedule_title_is_placeholder(title) {
            return Ok(title.trim().to_owned());
        }
        Ok(self
            .previous_matching_schedule_title(streamer_id, start_at)
            .await?
            .unwrap_or_else(|| "直播".to_owned()))
    }

    async fn previous_matching_schedule_title(
        &self,
        streamer_id: i64,
        start_at: DateTime<Utc>,
    ) -> AppResult<Option<String>> {
        let earliest = start_at - Duration::days(TITLE_REUSE_LOOKBACK_DAYS);
        let schedule_title = self
            .store
            .list_all_schedules()
            .await?
            .into_iter()
            .filter(|candidate| candidate.streamer_id == streamer_id)
            .filter(|candidate| candidate.start_at >= earliest && candidate.start_at <= start_at)
            .filter(|candidate| same_recurring_slot(candidate.start_at, start_at))
            .filter(|candidate| !schedule_title_is_placeholder(&candidate.title))
            .max_by_key(|candidate| candidate.start_at)
            .map(|candidate| candidate.title.trim().to_owned());
        if schedule_title.is_some() {
            return Ok(schedule_title);
        }

        Ok(self
            .store
            .list_live_sessions(Some(streamer_id), 200)
            .await?
            .into_iter()
            .filter(|session| session.started_at >= earliest && session.started_at <= start_at)
            .filter(|session| same_recurring_slot(session.started_at, start_at))
            .filter(|session| !schedule_title_is_placeholder(&session.title))
            .max_by_key(|session| session.started_at)
            .map(|session| session.title.trim().to_owned()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_candidate(
        &self,
        target: &Streamer,
        schedule: &ParsedSchedule,
        source_kind: &str,
        post: &DynamicPost,
        image_url: &str,
        source_identity: &str,
        source_fingerprint: &str,
        room_url: Option<&str>,
        room_owner_mid: Option<&str>,
        room_owner_name: Option<&str>,
        participants: &[String],
    ) -> AppResult<bool> {
        measure("schedule_upsert", async {
            // Independent streamer jobs may report the same joint event. Keep the
            // existing lookup-and-merge decision atomic across those jobs, without
            // serialising their network/model work or holding a SQLite transaction.
            let _write_guard = self.auto_schedule_write_lock.lock().await;
            let mut schedule = schedule.clone();
            schedule.title = self
                .resolve_auto_schedule_title(target.id, &schedule.title, schedule.start_at)
                .await?;
            let schedule = &schedule;
            // 带预约卡片的动态不再在这里按卡片时间拦文字/图片候选：
            // 卡片那一场现在也由文字模型给出，模型结果就是主来源。
            // 卡片正则的兜底在 process_source_posts 里按“是否已被模型覆盖”决定，
            // 不会和模型事件重复落库。
            if self
                .store
                .is_source_suppressed(target.id, &post.id, source_fingerprint)
                .await?
            {
                return Ok(false);
            }
            let input = NewSchedule {
                streamer_id: target.id,
                participants: participants.to_vec(),
                title: schedule.title.clone(),
                start_at: schedule.start_at,
                end_at: schedule.end_at,
                room_url: room_url
                    .map(ToOwned::to_owned)
                    .or_else(|| target.live_url.clone()),
                room_owner_mid: room_owner_mid.map(ToOwned::to_owned),
                room_owner_name: room_owner_name.map(ToOwned::to_owned),
                source_kind: source_kind.to_owned(),
                source_post_id: Some(post.id.clone()),
                source_image_url: source_kind_has_image(source_kind)
                    .then(|| image_url.to_owned())
                    .filter(|value| !value.is_empty()),
                source_author_mid: Some(post.author_mid.clone()),
                source_fingerprint: Some(source_fingerprint.to_owned()),
                source_identity: Some(source_identity.to_owned()),
                source_published_at: Some(post.published_at),
                confidence: schedule.confidence,
            };
            if self.store.merge_late_live_schedule(&input).await? {
                return Ok(false);
            }
            if !participants.is_empty()
                || room_owner_mid.is_some()
                || room_url.is_some()
                || source_kind == "reserve"
            {
                let candidates = self
                    .store
                    .list_schedules_near_all(schedule.start_at, 10)
                    .await?;
                let mut duplicates = group_duplicates(
                    &candidates,
                    schedule,
                    participants,
                    room_owner_mid,
                    room_url,
                );
                if !duplicates.is_empty() {
                    duplicates.sort_by_key(|schedule| (!schedule.manual_locked, schedule.id));
                    let existing = duplicates.remove(0);
                    let mut base = existing.clone();
                    for duplicate in duplicates {
                        let duplicate_input = schedule_to_new_input(duplicate);
                        let merged = (!base.manual_locked)
                            .then(|| merge_group_schedule(&base, &duplicate_input));
                        if self
                            .store
                            .merge_and_delete_schedule_duplicate(
                                base.id,
                                base.revision,
                                duplicate.id,
                                duplicate.revision,
                                merged.as_ref(),
                            )
                            .await?
                        {
                            base = self.store.get_schedule(base.id).await?;
                        }
                    }
                    self.store.attach_schedule_source(base.id, &input).await?;
                    if !base.manual_locked {
                        let merged = merge_group_schedule(&base, &input);
                        self.store.merge_schedule_auto(base.id, &merged).await?;
                    }
                    return Ok(false);
                }
            }
            // 单次分析后图片来源与文字来源等价：带图动态的正文里常写“明天八点改到九点”，
            // 这类时间变更必须同样能改到预约卡对应的日程上。
            if is_post_analysis_source(source_kind) && post.reserve.is_none() {
                if let Some(existing) = self
                    .find_reservation_time_change_candidate(target, schedule.start_at, post)
                    .await?
                {
                    self.store
                        .attach_schedule_source(existing.id, &input)
                        .await?;
                    let mut merged = merge_input_for_existing(&existing, &input);
                    merged.source_published_at = Some(post.published_at);
                    merged.confidence = existing.confidence.max(input.confidence);
                    self.store.merge_schedule_auto(existing.id, &merged).await?;
                    return Ok(false);
                }
            }
            if let Some(existing) = self
                .store
                .find_schedule_near(target.id, schedule.start_at, 10)
                .await?
            {
                if existing.source_identity.as_deref() != Some(source_identity) {
                    // 预约卡与模型事件同槽但标题对不上时默认保留两条；
                    // 只有同一直播间且同槽时，才视为同一场合并。
                    let same_room = room_owner_mid
                        .is_some_and(|mid| existing.room_owner_mid.as_deref() == Some(mid))
                        || room_url.is_some_and(|url| existing.room_url.as_deref() == Some(url));
                    // 预约卡与同一直播间的图文识别经常给出不同标题；同房间且同槽
                    // 仍是同一场，不能因为标题措辞不同就再落一条。
                    let incompatible_reservation = (existing.source_kind == "reserve"
                        || input.source_kind == "reserve")
                        && !cancellation_titles_match(&existing.title, &input.title)
                        && !(same_room
                            && same_schedule_slot(
                                existing.start_at,
                                schedule.start_at,
                                RESERVE_DUPLICATE_TOLERANCE_MINUTES,
                            ));
                    if !incompatible_reservation {
                        self.store
                            .attach_schedule_source(existing.id, &input)
                            .await?;
                        if existing.weekly_rule_id.is_some()
                            && normalize_alias(&existing.title) != normalize_alias(&schedule.title)
                        {
                            self.store.mark_schedule_source_changed(existing.id).await?;
                        }
                        let incoming_wins = match existing.source_published_at {
                            Some(existing_published_at) => {
                                post.published_at >= existing_published_at
                            }
                            None => schedule.confidence >= existing.confidence,
                        };
                        if !existing.manual_locked && incoming_wins {
                            let merged = merge_input_for_existing(&existing, &input);
                            self.store.merge_schedule_auto(existing.id, &merged).await?;
                            return Ok(false);
                        }
                        return Ok(false);
                    }
                }
            }
            let (_, inserted) = self.store.upsert_schedule(&input).await?;
            Ok(inserted)
        })
        .instrument(
            tracing::info_span!("schedule_write", post_id = %post.id, streamer_id = target.id),
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
/// 进度播报对象。全量刷新没有“当前主播”，用一条合成条目代替，
/// 这样前端的任务卡也能看到真实百分比，而不是一直停在 0%。
#[derive(Debug, Clone)]
struct ProgressTarget {
    id: i64,
    name: String,
}

impl ProgressTarget {
    fn for_streamer(streamer: &Streamer) -> Self {
        Self {
            id: streamer.id,
            name: streamer.name.clone(),
        }
    }

    fn all() -> Self {
        Self {
            id: 0,
            name: "全部主播".to_owned(),
        }
    }
}

/// 一轮刷新共用的模型通道。主播级调度按 `llm_parallelism` 限制活跃任务，
/// 同一主播内部逐条分析；本许可池同时限制共享端点的模型请求数量。
struct PostAnalyzer {
    config: LlmConfig,
    pool: LlmPool,
}

// 进度播报的参数本来就多（阶段、步数、百分比、结束/失败标记），
// 打包成结构体只会让每个调用点更长。
#[allow(clippy::too_many_arguments)]
fn emit_progress(
    target: &ProgressTarget,
    stage: &str,
    completed: usize,
    total: usize,
    percent: u8,
    done: bool,
    failed: bool,
    on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
) {
    on_progress(RefreshProgress {
        streamer_id: target.id,
        streamer_name: target.name.clone(),
        stage: stage.to_owned(),
        completed,
        total,
        percent: percent.min(100),
        done,
        failed,
    });
}

#[derive(Debug, Clone)]
struct PendingCancellation {
    target: Streamer,
    event: LlmEvent,
    source_post_id: String,
    published_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct RefreshEventState {
    cancellations: Vec<PendingCancellation>,
    /// 本轮已经由某条来源写入过的 schedule source identity，按动态 id 汇总。
    /// 同一条动态可能走多遍分析（主播本人 + 官号），清理 stale source 时
    /// 要用整轮的并集，不能用单遍的结果。
    written_identities: HashMap<String, HashSet<String>>,
}

enum SourceRefreshResult {
    Processed(ProcessStats),
    FetchFailed(AppError),
}

// Earlier pending sources reserve their targets too: an official source must
// not be overtaken by a later personal/linked source for the same streamer.
// Unrelated targets can still run, rather than occupying a worker waiting for A.
fn next_ready_source(
    sources: &[(String, Vec<Streamer>)],
    pending: &[usize],
    active_targets: &HashSet<i64>,
) -> Option<usize> {
    let mut unavailable = active_targets.clone();
    for (position, index) in pending.iter().enumerate() {
        let targets = &sources[*index].1;
        if targets
            .iter()
            .all(|target| !unavailable.contains(&target.id))
        {
            return Some(position);
        }
        unavailable.extend(targets.iter().map(|target| target.id));
    }
    None
}

#[derive(Debug, Default)]
struct ProcessStats {
    deferred_analyses: usize,
    new_schedules: usize,
    cancelled_schedules: usize,
    llm_calls: usize,
    llm_cache_hits: usize,
    analysis_errors: HashMap<i64, String>,
    analysis_outcomes: HashMap<String, Option<String>>,
    analyzed_streamer_ids: HashSet<i64>,
}

fn is_asoul_member(mid: &str) -> bool {
    A_SOUL_MEMBER_MIDS.contains(&mid)
}

/// Group targets by source MID so a shared official account is fetched once per refresh.
fn sources_for_streamers(streamers: &[Streamer]) -> Vec<(String, Vec<Streamer>)> {
    let mut source_targets: HashMap<String, Vec<Streamer>> = HashMap::new();
    for streamer in streamers
        .iter()
        .filter(|streamer| streamer.platform == Platform::Bilibili && streamer.schedule_enabled)
    {
        push_unique_target(&mut source_targets, &streamer.mid, streamer);
        for linked in streamer
            .linked_accounts
            .iter()
            .filter(|linked| linked.platform == Platform::Bilibili)
        {
            push_unique_target(&mut source_targets, &linked.mid, streamer);
        }
        if is_asoul_member(&streamer.mid) {
            push_unique_target(&mut source_targets, A_SOUL_OFFICIAL_MID, streamer);
        }
    }
    let mut sources: Vec<(String, Vec<Streamer>)> = source_targets.into_iter().collect();
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    sources
}

fn push_unique_target(
    map: &mut HashMap<String, Vec<Streamer>>,
    source_mid: &str,
    streamer: &Streamer,
) {
    let entry = map.entry(source_mid.to_owned()).or_default();
    if !entry.iter().any(|existing| existing.id == streamer.id) {
        entry.push(streamer.clone());
    }
}

fn choose_text_target(source_mid: &str, targets: &[Streamer], context: &str) -> Option<Streamer> {
    if let Some(primary) = targets.iter().find(|target| target.mid == source_mid) {
        return Some(primary.clone());
    }
    if targets.len() == 1 {
        return targets.first().cloned();
    }
    match_text_streamer(context, targets)
}

fn reserve_context(post: &DynamicPost) -> String {
    let mut context = post.text.clone();
    if let Some(reserve) = &post.reserve {
        context.push('\n');
        context.push_str(&reserve.title);
        context.push('\n');
        context.push_str(&reserve.description);
    }
    context
}

/// 纯文字动态的分析上下文：发布者 + 正文 + 预约卡文本。
/// 单次调用后模型拿到的仍是同一段文字，摘要因此保持一致，
/// 历史日程的 `text:{post_id}:{digest}:{idx}` identity 才能原地更新。
fn text_analysis_context(post: &DynamicPost) -> String {
    format!(
        "发布者：{}\n{}",
        post.author_name.trim(),
        reserve_context(post)
    )
    .trim()
    .to_owned()
}

/// 动态分析摘要：正文、预约卡及原顺序前两图；未选中的广告尾图不影响缓存。
/// 任一组成变化都会得到新摘要，缓存条目与图片来源 identity 随之失效。
fn content_digest_of(text_hash: &str, reserve: Option<&str>, image_urls: &[String]) -> String {
    text_digest(&format!(
        "{text_hash}|{}|{}",
        reserve.unwrap_or_default(),
        image_urls
            .iter()
            .take(POST_ANALYSIS_IMAGE_LIMIT)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(",")
    ))
}

/// 预约卡只把标题与简介送进模型；卡片 id 之类的元数据与分析无关。
fn reserve_summary(post: &DynamicPost) -> Option<String> {
    let reserve = post.reserve.as_ref()?;
    let title = reserve.title.trim();
    let description = reserve.description.trim();
    let summary = match (title.is_empty(), description.is_empty()) {
        (true, true) => String::new(),
        (false, true) => title.to_owned(),
        (true, false) => description.to_owned(),
        (false, false) => format!("{title}\n{description}"),
    };
    (!summary.is_empty()).then_some(summary)
}

/// 单次分析的来源种类。文字与图片不再是两条通道，合并、预约时间变更、
/// 同槽回落这些判断都要同时认这两种 kind。
fn is_post_analysis_source(source_kind: &str) -> bool {
    matches!(
        source_kind,
        "text" | "image" | "youtube_community" | "youtube_live"
    )
}

fn post_source_kind(post: &DynamicPost, has_images: bool) -> &'static str {
    match post.raw_kind.trim() {
        "youtube_community" => "youtube_community",
        "youtube_live" => "youtube_live",
        _ if has_images => "image",
        _ => "text",
    }
}

fn source_kind_has_image(source_kind: &str) -> bool {
    matches!(source_kind, "image" | "youtube_community" | "youtube_live")
}

/// 缓存里的空结果还没过重试窗口：当作命中，不再问模型。
fn empty_analysis_is_fresh(created_at: &str, now: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(created_at) {
        Ok(value) => {
            now.signed_duration_since(value.with_timezone(&Utc))
                < Duration::hours(EMPTY_ANALYSIS_RETRY_HOURS)
        }
        // 时间戳坏了就当成过期，宁可多问一次也别把结果永久锁死。
        Err(_) => false,
    }
}

/// 两个开始时间是否指向同一场直播。日程合并与预约去重共用同一容差。
fn same_schedule_slot(left: DateTime<Utc>, right: DateTime<Utc>, tolerance_minutes: i64) -> bool {
    (left - right).num_minutes().abs() <= tolerance_minutes
}

fn schedule_title_is_placeholder(title: &str) -> bool {
    let normalized: String = title
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect();
    normalized.is_empty()
        || matches!(
            normalized.as_str(),
            "直播"
                | "未提供"
                | "未提供主题"
                | "主题未提供"
                | "直播未提供主题"
                | "未提及"
                | "未说明"
                | "暂无主题"
                | "无主题"
                | "未知主题"
                | "待定"
                | "未命名"
                | "tbd"
                | "untitled"
                | "unknown"
        )
}

fn same_recurring_slot(left: DateTime<Utc>, right: DateTime<Utc>) -> bool {
    let china = FixedOffset::east_opt(8 * 3600).expect("china offset");
    let left = left.with_timezone(&china);
    let right = right.with_timezone(&china);
    if left.weekday() != right.weekday() {
        return false;
    }
    let left_minute = i64::from(left.hour()) * 60 + i64::from(left.minute());
    let right_minute = i64::from(right.hour()) * 60 + i64::from(right.minute());
    (left_minute - right_minute).abs() <= TITLE_REUSE_SLOT_TOLERANCE_MINUTES
}

fn clean_schedule_title_value(mut title: String, target: &Streamer) -> String {
    let mut names: Vec<&str> = target
        .aliases
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(target.name.as_str()))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_by_key(|name| std::cmp::Reverse(name.chars().count()));
    for name in names {
        if let Some(rest) = title.trim_start().strip_prefix(name) {
            let cleaned = rest
                .trim_start_matches(|character: char| {
                    character.is_whitespace() || "：:，,、-—_/｜|".contains(character)
                })
                .trim();
            if !cleaned.is_empty() {
                title = cleaned.to_owned();
                break;
            }
        }
    }
    title.chars().take(200).collect()
}

fn normalize_search_value(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('@')
        .to_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

fn search_matches_subscription(keyword: &str, streamer: &Streamer) -> bool {
    let query = normalize_search_value(keyword);
    if query.is_empty() {
        return false;
    }
    normalize_search_value(&streamer.mid) == query
        || normalize_search_value(&streamer.name) == query
        || streamer
            .handle
            .as_deref()
            .is_some_and(|handle| normalize_search_value(handle) == query)
        || streamer
            .aliases
            .iter()
            .any(|alias| normalize_search_value(alias) == query)
}

fn account_from_streamer(streamer: &Streamer) -> crate::models::AccountSearchResult {
    crate::models::AccountSearchResult {
        platform: streamer.platform,
        mid: streamer.mid.clone(),
        name: streamer.name.clone(),
        avatar_url: streamer.avatar_url.clone(),
        sign: String::new(),
        handle: streamer.handle.clone(),
        profile_url: streamer.profile_url.clone(),
        room_id: streamer.room_id.clone(),
        fans: 0,
        is_live: streamer.is_live,
        live_url: streamer.live_url.clone(),
    }
}

fn youtube_account_search_result(channel: YouTubeChannel) -> crate::models::AccountSearchResult {
    crate::models::AccountSearchResult {
        platform: Platform::Youtube,
        mid: channel.id,
        name: channel.name,
        avatar_url: channel.avatar_url.unwrap_or_default(),
        sign: channel.description,
        handle: channel.handle,
        profile_url: Some(channel.profile_url),
        room_id: None,
        fans: channel.subscriber_count.unwrap_or(0),
        is_live: false,
        live_url: None,
    }
}

fn dynamic_post_from_youtube_community(
    post: YouTubeCommunityPost,
    streamer: &Streamer,
    observed_at: DateTime<Utc>,
) -> DynamicPost {
    // Relative labels such as "3 hours ago" and crawl-time estimates change between
    // refreshes. Keep the model-facing text stable so content_digest/source_fingerprint
    // do not turn the same post into a new source; the time anchor remains published_at.
    let time_context = match (post.published_at, post.published_at_estimated) {
        (Some(_), true) => {
            "[YouTube 原始发布时间：页面只提供了相对时间；按本次抓取时点估算，可能存在误差]"
                .to_owned()
        }
        (Some(value), false) => format!("[YouTube 原始发布时间：{}]", value.to_rfc3339()),
        (None, _) => "[YouTube 原始发布时间：无法可靠解析，不能视为原发布时间]".to_owned(),
    };
    let mut text = time_context;
    if !post.text.trim().is_empty() {
        text.push('\n');
        text.push_str(post.text.trim());
    }
    DynamicPost {
        id: format!("youtube:{}", post.id),
        author_mid: post.channel_id,
        author_name: post.author_name.unwrap_or_else(|| streamer.name.clone()),
        published_at: post.published_at.unwrap_or(observed_at),
        text,
        images: post.images,
        source_url: post.source_url,
        raw_kind: "youtube_community".to_owned(),
        reserve: None,
    }
}

fn dynamic_post_from_youtube_live(video: &YouTubeVideo, streamer: &Streamer) -> DynamicPost {
    let mut text = video.title.trim().to_owned();
    if !video.description.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(video.description.trim());
    }
    DynamicPost {
        id: format!("youtube_live:{}", video.id),
        author_mid: video.channel_id.clone(),
        author_name: if video.channel_title.trim().is_empty() {
            streamer.name.clone()
        } else {
            video.channel_title.clone()
        },
        published_at: video
            .published_at
            .or(video.actual_start_at)
            .or(video.scheduled_start_at)
            .unwrap_or_else(Utc::now),
        text,
        images: video.thumbnail_url.clone().into_iter().collect(),
        source_url: video
            .live_url()
            .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={}", video.id)),
        raw_kind: "youtube_live".to_owned(),
        reserve: None,
    }
}

fn search_result_rank(
    account: &crate::models::AccountSearchResult,
    keyword: &str,
    subscriptions: &[Streamer],
) -> (u8, u8, u64) {
    let query = normalize_search_value(keyword);
    let name = normalize_search_value(&account.name);
    let subscribed = subscriptions
        .iter()
        .any(|streamer| streamer.platform == account.platform && streamer.mid == account.mid);
    let alias_exact = subscriptions
        .iter()
        .filter(|streamer| streamer.platform == account.platform && streamer.mid == account.mid)
        .flat_map(|streamer| streamer.aliases.iter())
        .any(|alias| normalize_search_value(alias) == query);
    let match_rank = if account.mid == query || name == query {
        4
    } else if alias_exact {
        3
    } else if name.starts_with(&query) {
        2
    } else if name.contains(&query) {
        1
    } else {
        0
    };
    (match_rank, u8::from(subscribed), account.fans)
}

fn dedupe_and_rank_search_results(
    results: &mut Vec<crate::models::AccountSearchResult>,
    keyword: &str,
    subscriptions: &[Streamer],
) {
    let mut seen = std::collections::HashSet::new();
    results.retain(|account| seen.insert((account.platform, account.mid.clone())));
    results.sort_by(|left, right| {
        search_result_rank(right, keyword, subscriptions)
            .cmp(&search_result_rank(left, keyword, subscriptions))
            .then_with(|| right.fans.cmp(&left.fans))
            .then_with(|| left.name.cmp(&right.name))
    });
}

fn match_text_streamer(text: &str, targets: &[Streamer]) -> Option<Streamer> {
    let normalized_text = normalize_alias(text);
    let matches: Vec<&Streamer> = targets
        .iter()
        .filter(|target| {
            target
                .aliases
                .iter()
                .chain(std::iter::once(&target.name))
                .map(|alias| normalize_alias(alias))
                .filter(|alias| alias.chars().count() >= 2)
                .any(|alias| normalized_text.contains(&alias))
        })
        .collect();
    (matches.len() == 1).then(|| matches[0].clone())
}

fn canonicalize_event_participants(event: &LlmEvent, targets: &[Streamer]) -> LlmEvent {
    let mut canonical = event.clone();
    canonical.participants = event
        .participant_names()
        .into_iter()
        .map(|participant| {
            let normalized = normalize_alias(&participant);
            single_matching_target(&[normalized], event.confidence, targets)
                .map(|target| target.name)
                .unwrap_or(participant)
        })
        .fold(Vec::<String>::new(), |mut names, name| {
            let normalized = normalize_alias(&name);
            if !names
                .iter()
                .any(|existing| normalize_alias(existing) == normalized)
            {
                names.push(name);
            }
            names
        });
    canonical
}

fn match_event_target_for_post(
    event: &LlmEvent,
    targets: &[Streamer],
    author_mid: &str,
) -> Option<Streamer> {
    if let Some(target) = match_event_target(event, targets) {
        return Some(target);
    }
    if event.participant_names().len() > 1 || event.confidence < 0.6 {
        return None;
    }
    let author = targets.iter().find(|target| target.mid == author_mid)?;
    let event_names: Vec<String> = std::iter::once(&event.performer)
        .chain(event.aliases.iter())
        .map(|value| normalize_alias(value))
        .filter(|value| !value.is_empty())
        .collect();
    let participant_names: Vec<String> = event
        .participant_names()
        .iter()
        .map(|value| normalize_alias(value))
        .collect();
    if event_identity_matches_target(&event_names, author)
        && event_identity_matches_target(&participant_names, author)
    {
        return Some(author.clone());
    }
    // 发布或转发预告不等于参与直播。身份匹配失败时不能用作者兜底，
    // 即使模型置信度很高、直播间属于其他人，也必须保留“不匹配”的结果。
    None
}

fn event_identity_matches_target(event_names: &[String], target: &Streamer) -> bool {
    let target_names: Vec<String> = std::iter::once(&target.name)
        .chain(target.aliases.iter())
        .map(|value| normalize_alias(value))
        .filter(|value| !value.is_empty())
        .collect();
    event_names.iter().any(|event_name| {
        target_names.iter().any(|target_name| {
            event_name == target_name
                || (event_name.chars().count() >= 2
                    && target_name.chars().count() >= 2
                    && (event_name.contains(target_name) || target_name.contains(event_name)))
        })
    })
}

fn text_looks_like_schedule_time_change(text: &str) -> bool {
    [
        "就来", "改到", "改成", "提前", "推迟", "延后", "顺延", "调整",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}

fn match_event_target(event: &LlmEvent, targets: &[Streamer]) -> Option<Streamer> {
    let participants = event.participant_names();
    if participants.len() <= 1 {
        // 显式参与者比 performer/aliases 更具体；不能把未参与的发布者
        // 当作个人场的主播。没有参与者字段时仍兼容旧模型的别名输出。
        let event_names: Vec<String> = if event.participants.is_empty() {
            std::iter::once(&event.performer)
                .chain(event.aliases.iter())
                .map(|value| normalize_alias(value))
                .filter(|value| !value.is_empty())
                .collect()
        } else {
            participants
                .iter()
                .map(|value| normalize_alias(value))
                .collect()
        };
        return single_matching_target(&event_names, event.confidence, targets);
    }

    let mut matched = Vec::<Streamer>::new();
    for participant in participants {
        let normalized = normalize_alias(&participant);
        let Some(target) = single_matching_target(&[normalized], event.confidence, targets) else {
            continue;
        };
        if !matched.iter().any(|existing| existing.id == target.id) {
            matched.push(target);
        }
    }
    matched.sort_by_key(|target| target.id);
    matched.into_iter().next()
}

fn single_matching_target(
    event_names: &[String],
    confidence: f64,
    targets: &[Streamer],
) -> Option<Streamer> {
    let mut scores = Vec::new();
    for target in targets {
        let target_names: Vec<String> = std::iter::once(&target.name)
            .chain(target.aliases.iter())
            .map(|value| normalize_alias(value))
            .filter(|value| !value.is_empty())
            .collect();
        let lexical = event_names
            .iter()
            .flat_map(|event_name| {
                target_names.iter().map(move |target_name| {
                    if event_name == target_name {
                        1.0
                    } else if event_name.chars().count() >= 2
                        && target_name.chars().count() >= 2
                        && (event_name.contains(target_name) || target_name.contains(event_name))
                    {
                        0.85
                    } else {
                        0.0
                    }
                })
            })
            .fold(0.0_f64, f64::max);
        let score = confidence * 0.65 + lexical * 0.35;
        if score >= 0.85 {
            scores.push((target, score));
        }
    }
    scores.sort_by(|left, right| right.1.total_cmp(&left.1));
    if scores.is_empty() || (scores.len() > 1 && (scores[0].1 - scores[1].1).abs() < f64::EPSILON) {
        None
    } else {
        Some(scores[0].0.clone())
    }
}

fn valid_youtube_room_url(value: &str) -> bool {
    url::Url::parse(value.trim()).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|host| {
                host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com")
            })
    })
}

fn valid_live_room_url(value: &str) -> bool {
    url::Url::parse(value)
        .ok()
        .is_some_and(|url| url.scheme() == "https" && url.host_str() == Some("live.bilibili.com"))
}

fn live_room_id(value: &str) -> Option<String> {
    url::Url::parse(value)
        .ok()?
        .path_segments()?
        .find(|segment| !segment.is_empty() && segment.chars().all(|ch| ch.is_ascii_digit()))
        .map(ToOwned::to_owned)
}

fn target_for_room_id<'a>(targets: &'a [Streamer], room_id: &str) -> Option<&'a Streamer> {
    targets.iter().find(|target| {
        target.room_id.as_deref() == Some(room_id)
            || target.live_url.as_deref().and_then(live_room_id).as_deref() == Some(room_id)
    })
}

#[derive(Debug, Clone)]
struct ResolvedRoom {
    url: Option<String>,
    owner_mid: Option<String>,
    owner_name: Option<String>,
}

impl ResolvedRoom {
    fn from_target(url: String, target: Streamer) -> Self {
        Self {
            url: (!url.is_empty()).then_some(url),
            owner_mid: Some(target.mid),
            owner_name: Some(target.name),
        }
    }
}

/// 一条动态里第 `event_index` 条事件对应的来源编号。
///
/// 纯文字动态保持 `text:{post_id}:{text_hash}:{idx}`，历史日程能原地更新；
/// 带图动态用整条动态的 `content_digest`（正文 + 预约卡 + 图片地址），
/// 换图或改正文都会换编号。编号尾号就是事件序号，清理来源时按它判断
/// 「这条事件在最新一轮分析里还在不在」。
fn event_source_identity(
    post_id: &str,
    source_kind: &str,
    text_hash: &str,
    content_digest: &str,
    event_index: usize,
) -> String {
    match source_kind {
        "image" => format!("image:{post_id}:{content_digest}:{event_index}"),
        "youtube_community" => {
            format!("youtube_community:{post_id}:{content_digest}:{event_index}")
        }
        "youtube_live" => format!("youtube_live:{post_id}:{content_digest}:{event_index}"),
        _ => format!("text:{post_id}:{text_hash}:{event_index}"),
    }
}

/// 同一条动态里，文字通道与图片通道给出的两个事件是否指向同一场直播。
/// 优先看时间槽与直播间，缺少房间证据时再看参与者；标题措辞不同不代表是两场。
fn llm_events_equivalent(left: &LlmEvent, right: &LlmEvent) -> bool {
    if left.action != right.action {
        return false;
    }
    match (left.start_at, right.start_at) {
        (Some(left_start), Some(right_start)) => {
            if !same_schedule_slot(left_start, right_start, GROUP_DUPLICATE_TOLERANCE_MINUTES) {
                return false;
            }
        }
        (None, None) => {}
        _ => return false,
    }
    if let Some(same) = same_schedule_room(
        left.room_owner_mid.as_deref(),
        left.room_url.as_deref(),
        right.room_owner_mid.as_deref(),
        right.room_url.as_deref(),
    ) {
        return same;
    }
    let left_names: Vec<String> = left
        .participant_names()
        .iter()
        .map(|name| normalize_alias(name))
        .collect();
    let right_names: Vec<String> = right
        .participant_names()
        .iter()
        .map(|name| normalize_alias(name))
        .collect();
    if left_names == right_names {
        return true;
    }
    let shared = left_names
        .iter()
        .filter(|name| right_names.iter().any(|other| other == *name))
        .count();
    let min_participants = left_names.len().min(right_names.len());
    // 一场单人直播时 min_participants 为 1，要求同一个人重合才算同一场。
    shared >= 1 && shared * 2 >= min_participants
}

/// Room identity is stronger than participant spelling. Bilibili short/full room
/// URLs may differ, so a confirmed owner MID can also establish the same room.
/// Explicitly different rooms must not merge merely because the cast overlaps.
fn same_schedule_room(
    left_owner: Option<&str>,
    left_url: Option<&str>,
    right_owner: Option<&str>,
    right_url: Option<&str>,
) -> Option<bool> {
    let left_owner = left_owner.filter(|value| !value.trim().is_empty());
    let right_owner = right_owner.filter(|value| !value.trim().is_empty());
    if let (Some(left), Some(right)) = (left_owner, right_owner) {
        if left != right {
            return Some(false);
        }
    }
    let room_key = |value: &str| {
        let mut url = url::Url::parse(value).ok()?;
        if url.host_str() == Some("live.bilibili.com") {
            let room = url.path().trim_matches('/');
            if room.is_empty() || !room.chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }
            return Some(format!("bilibili:{room}"));
        }
        url.set_fragment(None);
        Some(url.to_string())
    };
    if let (Some(left), Some(right)) = (left_url.and_then(room_key), right_url.and_then(room_key)) {
        if left == right {
            return Some(true);
        }
        // Do not collapse different YouTube video URLs under the same channel.
        if !(left.starts_with("bilibili:") && right.starts_with("bilibili:")) {
            return Some(false);
        }
        return Some(left_owner.is_some() && left_owner == right_owner);
    }
    left_owner
        .zip(right_owner)
        .map(|(left, right)| left == right)
}

/// Same room and bounded start time identify a slot even with missing participants.
fn schedules_equivalent(left: &Schedule, right: &Schedule) -> bool {
    if (left.start_at - right.start_at).abs() > Duration::minutes(10) {
        return false;
    }
    if left.cancelled || right.cancelled {
        return false;
    }
    if let Some(same) = same_schedule_room(
        left.room_owner_mid.as_deref(),
        left.room_url.as_deref(),
        right.room_owner_mid.as_deref(),
        right.room_url.as_deref(),
    ) {
        return same;
    }
    let left_names: Vec<String> = left
        .participants
        .iter()
        .map(|name| normalize_alias(name))
        .collect();
    let right_names: Vec<String> = right
        .participants
        .iter()
        .map(|name| normalize_alias(name))
        .collect();
    let shared = left_names
        .iter()
        .filter(|name| right_names.iter().any(|other| other == *name))
        .count();
    let min_participants = left_names.len().min(right_names.len());
    shared >= 2 && shared * 2 >= min_participants
}

fn group_duplicates<'a>(
    candidates: &'a [Schedule],
    incoming: &ParsedSchedule,
    participants: &[String],
    room_owner_mid: Option<&str>,
    room_url: Option<&str>,
) -> Vec<&'a Schedule> {
    let incoming_names: Vec<String> = participants
        .iter()
        .map(|name| normalize_alias(name))
        .collect();
    candidates
        .iter()
        .filter(|candidate| {
            if candidate.cancelled
                || (candidate.start_at - incoming.start_at).abs() > Duration::minutes(10)
            {
                return false;
            }
            if let Some(same) = same_schedule_room(
                candidate.room_owner_mid.as_deref(),
                candidate.room_url.as_deref(),
                room_owner_mid,
                room_url,
            ) {
                return same;
            }
            let existing_names: Vec<String> = candidate
                .participants
                .iter()
                .map(|name| normalize_alias(name))
                .collect();
            let shared = incoming_names
                .iter()
                .filter(|name| existing_names.iter().any(|existing| existing == *name))
                .count();
            let min_participants = incoming_names.len().min(existing_names.len());
            shared >= 2 && shared * 2 >= min_participants
        })
        .collect()
}

fn schedule_to_new_input(schedule: &Schedule) -> NewSchedule {
    NewSchedule {
        streamer_id: schedule.streamer_id,
        participants: schedule.participants.clone(),
        title: schedule.title.clone(),
        start_at: schedule.start_at,
        end_at: schedule.end_at,
        room_url: schedule.room_url.clone(),
        room_owner_mid: schedule.room_owner_mid.clone(),
        room_owner_name: schedule.room_owner_name.clone(),
        source_kind: schedule.source_kind.clone(),
        source_post_id: schedule.source_post_id.clone(),
        source_image_url: schedule.source_image_url.clone(),
        source_author_mid: schedule.source_author_mid.clone(),
        source_fingerprint: schedule.source_fingerprint.clone(),
        source_identity: schedule.source_identity.clone(),
        source_published_at: schedule.source_published_at,
        confidence: schedule.confidence,
    }
}

fn merge_group_schedule(existing: &Schedule, incoming: &NewSchedule) -> NewSchedule {
    let mut participants = existing.participants.clone();
    for participant in &incoming.participants {
        let normalized = normalize_alias(participant);
        if !participants
            .iter()
            .any(|current| normalize_alias(current) == normalized)
        {
            participants.push(participant.clone());
        }
    }
    let keep_existing_source = (existing.source_kind == "reserve"
        && incoming.source_kind != "reserve")
        || (existing.source_kind != "live" && incoming.source_kind == "live");
    let incoming_reserve = incoming.source_kind == "reserve" && existing.source_kind != "reserve";
    let use_incoming_title = !keep_existing_source
        && !schedule_title_is_placeholder(&incoming.title)
        && (schedule_title_is_placeholder(&existing.title)
            || incoming_reserve
            || (existing.source_kind == "live" && incoming.source_kind != "live")
            || incoming.title.chars().count() > existing.title.chars().count());
    let title = if use_incoming_title {
        incoming.title.clone()
    } else {
        existing.title.clone()
    };
    let source_published_at = match (existing.source_published_at, incoming.source_published_at) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    let incoming_wins = match (existing.source_published_at, incoming.source_published_at) {
        (Some(existing_at), Some(incoming_at)) => incoming_at >= existing_at,
        (None, Some(_)) => true,
        _ => false,
    };
    NewSchedule {
        streamer_id: existing.streamer_id,
        participants,
        title,
        start_at: if incoming.source_kind == "live" && existing.source_kind != "live" {
            existing.start_at
        } else if incoming_wins {
            incoming.start_at
        } else {
            existing.start_at
        },
        end_at: if incoming_wins {
            incoming.end_at.or(existing.end_at)
        } else {
            existing.end_at.or(incoming.end_at)
        },
        room_url: if incoming_wins {
            incoming
                .room_url
                .clone()
                .or_else(|| existing.room_url.clone())
        } else {
            existing
                .room_url
                .clone()
                .or_else(|| incoming.room_url.clone())
        },
        room_owner_mid: if incoming_wins {
            incoming
                .room_owner_mid
                .clone()
                .or_else(|| existing.room_owner_mid.clone())
        } else {
            existing
                .room_owner_mid
                .clone()
                .or_else(|| incoming.room_owner_mid.clone())
        },
        room_owner_name: if incoming_wins {
            incoming
                .room_owner_name
                .clone()
                .or_else(|| existing.room_owner_name.clone())
        } else {
            existing
                .room_owner_name
                .clone()
                .or_else(|| incoming.room_owner_name.clone())
        },
        source_kind: if keep_existing_source {
            existing.source_kind.clone()
        } else {
            incoming.source_kind.clone()
        },
        source_post_id: if keep_existing_source {
            existing.source_post_id.clone()
        } else {
            incoming.source_post_id.clone()
        },
        source_image_url: if keep_existing_source {
            existing.source_image_url.clone()
        } else {
            incoming.source_image_url.clone()
        },
        source_author_mid: if keep_existing_source {
            existing.source_author_mid.clone()
        } else {
            incoming.source_author_mid.clone()
        },
        source_fingerprint: if keep_existing_source {
            existing.source_fingerprint.clone()
        } else {
            incoming.source_fingerprint.clone()
        },
        source_identity: if keep_existing_source {
            existing.source_identity.clone()
        } else {
            incoming.source_identity.clone()
        },
        source_published_at: if keep_existing_source {
            existing.source_published_at
        } else {
            source_published_at
        },
        confidence: existing.confidence.max(incoming.confidence),
    }
}

fn merge_input_for_existing(existing: &Schedule, incoming: &NewSchedule) -> NewSchedule {
    // 同槽时时间以 incoming（模型）为准，但标题与来源仍归卡片：
    // 卡片标题是官方文案，文字里的“明天下午三点就来”只是改期通知，
    // 不该把【突击】影之刃零DAY5！ 改成“和奶淇琳见面”。
    let mut merged = incoming.clone();
    if existing.source_kind == "reserve" && incoming.source_kind != "reserve" {
        merged.title = existing.title.clone();
        merged.source_kind = existing.source_kind.clone();
        merged.source_post_id = existing.source_post_id.clone();
        merged.source_image_url = existing.source_image_url.clone();
        merged.source_author_mid = existing.source_author_mid.clone();
        merged.source_fingerprint = existing.source_fingerprint.clone();
        merged.source_identity = existing.source_identity.clone();
        merged.source_published_at = existing.source_published_at;
    }
    if schedule_title_is_placeholder(&merged.title)
        && !schedule_title_is_placeholder(&existing.title)
    {
        merged.title = existing.title.clone();
    }
    merged
}

fn select_cancellation_candidates<'a>(
    candidates: &'a [Schedule],
    requested_title: &str,
) -> Vec<&'a Schedule> {
    let requested_title = normalize_alias(requested_title);
    let generic = requested_title.is_empty()
        || ["直播", "取消直播", "直播取消", "原定直播"].contains(&requested_title.as_str());
    if generic {
        return if candidates.len() == 1 {
            vec![&candidates[0]]
        } else {
            Vec::new()
        };
    }
    let parts = split_cancellation_title(&requested_title);
    if parts.len() >= 2 {
        let mut selected = Vec::new();
        let mut selected_ids = HashSet::new();
        for part in parts {
            let matches: Vec<&Schedule> = candidates
                .iter()
                .filter(|schedule| !selected_ids.contains(&schedule.id))
                .filter(|schedule| cancellation_titles_match(&schedule.title, &part))
                .collect();
            if matches.len() == 1 {
                selected.push(matches[0]);
                selected_ids.insert(matches[0].id);
            }
        }
        if !selected.is_empty() {
            return selected;
        }
    }
    let matches = matching_cancellation_titles(candidates, &requested_title);
    if matches.len() == 1 {
        matches
    } else {
        Vec::new()
    }
}

fn matching_cancellation_titles<'a>(
    candidates: &'a [Schedule],
    requested_title: &str,
) -> Vec<&'a Schedule> {
    candidates
        .iter()
        .filter(|schedule| cancellation_titles_match(&schedule.title, requested_title))
        .collect()
}

fn cancellation_titles_match(actual: &str, requested: &str) -> bool {
    let actual = normalize_alias(actual);
    let requested = normalize_alias(requested);
    let shorter = actual.chars().count().min(requested.chars().count());
    shorter >= 2 && (actual.contains(&requested) || requested.contains(&actual))
}

fn split_cancellation_title(title: &str) -> Vec<String> {
    let normalized = title.replace("以及", "和");
    normalized
        .split(['和', '与', '、', ',', '，', '/'])
        .map(normalize_alias)
        .filter(|part| part.chars().count() >= 2)
        .filter(|part| !["直播", "取消直播", "直播取消", "原定直播"].contains(&part.as_str()))
        .collect()
}

fn cancellation_candidate_is_eligible(
    schedule: &Schedule,
    source_post_id: &str,
    published_at: DateTime<Utc>,
) -> bool {
    !schedule.manual_locked
        && schedule.source_post_id.as_deref() != Some(source_post_id)
        && schedule
            .source_published_at
            .is_none_or(|source_published_at| source_published_at <= published_at)
}

fn normalize_alias(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|ch| {
            !ch.is_whitespace() && !"[]【】()（）<>《》_-·.,，。:：!！?？/\\|｜".contains(*ch)
        })
        .collect()
}

fn text_digest(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn ordered_posts_oldest_first(posts: &[DynamicPost]) -> Vec<DynamicPost> {
    let mut ordered = posts.to_vec();
    ordered.sort_by(|left, right| {
        left.published_at
            .cmp(&right.published_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    ordered
}

fn source_fingerprint(post: &DynamicPost, title: &str, source_kind: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(post.id.as_bytes());
    hasher.update(post.text.as_bytes());
    if post.raw_kind.trim() == "youtube_community" {
        hasher.update(b"\ncommunity-source\0");
        hasher.update(post.source_url.as_bytes());
        for image_url in &post.images {
            hasher.update(b"\ncommunity-image\0");
            hasher.update(image_url.as_bytes());
        }
    }
    hasher.update(title.as_bytes());
    hasher.update(source_kind.as_bytes());
    if let Some(reserve) = &post.reserve {
        hasher.update(reserve.title.as_bytes());
        hasher.update(reserve.description.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 用主播自己的 UID 匹配 LAPLACE 记录；未命中时再看关联官号。
fn match_fertility_statuses(
    streamers: &[Streamer],
    payload: &crate::fertility::FertilityResponse,
) -> Vec<FertilityStatus> {
    let remote_by_mid: HashMap<String, &crate::fertility::FertilityRecord> = payload
        .data
        .iter()
        .map(|record| (record.uid.to_string(), record))
        .collect();
    let mut statuses = Vec::new();
    for streamer in streamers {
        let mut hit: Option<(&crate::fertility::FertilityRecord, String)> = remote_by_mid
            .get(&streamer.mid)
            .copied()
            .map(|record| (record, streamer.mid.clone()));
        if hit.is_none() {
            for linked in &streamer.linked_accounts {
                if let Some(record) = remote_by_mid.get(&linked.mid).copied() {
                    hit = Some((record, linked.mid.clone()));
                    break;
                }
            }
        }
        let Some((record, matched_mid)) = hit else {
            continue;
        };
        statuses.push(FertilityStatus {
            streamer_id: streamer.id,
            mid: streamer.mid.clone(),
            streamer_name: streamer.name.clone(),
            streamer_avatar: if streamer.avatar_url.is_empty() {
                record.user.avatar.clone().unwrap_or_default()
            } else {
                streamer.avatar_url.clone()
            },
            status: record.status.to_ascii_lowercase(),
            day_in_cycle: record.day_in_cycle,
            ovulation_day: record.ovulation_day,
            next_period: record.next_period.clone(),
            cycle_length: record.effective_cycle_length,
            period_length: record.effective_period_length,
            data_points: record.data_points,
            matched_mid,
        });
    }
    statuses
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use chrono::{Duration, FixedOffset, TimeZone};
    use serde_json::Value;

    use crate::llm::LlmAnalysis;
    use crate::models::ReserveCard;
    use crate::settings::{save_settings, ReasoningEffort};

    /// 一次 `analyze_post` 调用收到的完整输入，供断言使用。
    struct CapturedCall {
        model: String,
        reasoning_effort: ReasoningEffort,
        input: PostAnalysisInput,
    }

    /// 模型桩：一条动态只该产生一次调用，输入全部留档。
    struct CountingAnalyzer {
        calls: AtomicUsize,
        events: Vec<LlmEvent>,
        text_events: Vec<LlmEvent>,
        captured: std::sync::Mutex<Vec<CapturedCall>>,
    }

    impl CountingAnalyzer {
        fn new(events: Vec<LlmEvent>, text_events: Vec<LlmEvent>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                events,
                text_events,
                captured: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn captured_calls(&self) -> std::sync::MutexGuard<'_, Vec<CapturedCall>> {
            self.captured.lock().expect("captured calls")
        }

        fn last_call(&self) -> CapturedCall {
            let captured = self.captured.lock().expect("captured calls");
            let last = captured.last().expect("analyzer was never called");
            CapturedCall {
                model: last.model.clone(),
                reasoning_effort: last.reasoning_effort,
                input: last.input.clone(),
            }
        }
    }

    #[async_trait]
    impl ContentAnalyzer for CountingAnalyzer {
        // 合并成单次调用后，正文事件排在前面：同槽不同标题时仍是
        // 「正文那一版先落库」，与旧双通道的行为一致。
        async fn analyze_post(
            &self,
            config: &LlmConfig,
            input: &PostAnalysisInput,
        ) -> AppResult<LlmAnalysis> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut events = self.text_events.clone();
            events.extend(self.events.clone());
            self.captured
                .lock()
                .expect("captured calls")
                .push(CapturedCall {
                    model: config.model.clone(),
                    reasoning_effort: config.reasoning_effort,
                    input: input.clone(),
                });
            let raw = events_to_model_json(&events);
            Ok(LlmAnalysis {
                events,
                raw,
                source_digest: "mock-post-digest".to_owned(),
            })
        }
    }

    /// 模型原始输出是 snake_case 的 JSON，缓存里存的也是这个形状。
    /// 直接 `to_value(&events)` 会得到 camelCase，再读回来就丢字段。
    fn events_to_model_json(events: &[LlmEvent]) -> serde_json::Value {
        serde_json::Value::Array(
            events
                .iter()
                .map(|event| {
                    json!({
                        "action": match event.action {
                            LlmEventAction::Schedule => "schedule",
                            LlmEventAction::Cancel => "cancel",
                        },
                        "performer": event.performer,
                        "aliases": event.aliases,
                        "participants": event.participants,
                        "room_url": event.room_url,
                        "room_owner": event.room_owner,
                        "room_owner_mid": event.room_owner_mid,
                        "title": event.title,
                        "start_at": event.start_at.map(|value| value.to_rfc3339()),
                        "end_at": event.end_at.map(|value| value.to_rfc3339()),
                        "confidence": event.confidence,
                    })
                })
                .collect(),
        )
    }

    /// 单并发的模型通道：与 `AppSettings::default()` 的 `llm_parallelism` 一致，
    /// 只把模型名换成用例关心的值。
    fn test_pool(model: &str) -> PostAnalyzer {
        PostAnalyzer {
            config: LlmConfig {
                base_url: "https://fixture.invalid/v1".to_owned(),
                api_key: String::new(),
                model: model.to_owned(),
                prompt_version: "fixture-v1".to_owned(),
                reasoning_effort: ReasoningEffort::Auto,
            },
            pool: LlmPool::new(1),
        }
    }

    fn subscription(mid: &str, name: &str) -> AddSubscription {
        AddSubscription {
            platform: Platform::Bilibili,
            handle: None,
            profile_url: None,
            uploads_playlist_id: None,
            mid: mid.to_owned(),
            name: Some(name.to_owned()),
            avatar_url: None,
            room_id: Some(format!("{}01", &mid[mid.len().saturating_sub(2)..])),
            aliases: None,
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
        }
    }

    #[tokio::test]
    async fn youtube_switch_off_stops_before_next_channel_request() {
        let store = SqliteStore::in_memory().await.expect("db");
        let mut settings = AppSettings {
            youtube_enabled: true,
            ..AppSettings::default()
        };
        save_settings(&store, &settings)
            .await
            .expect("enable youtube");

        let mut channels_started = 0;
        for _ in 0..2 {
            if !youtube_requests_enabled(&store).await {
                break;
            }
            channels_started += 1;
            settings.youtube_enabled = false;
            save_settings(&store, &settings)
                .await
                .expect("disable youtube");
        }

        assert_eq!(channels_started, 1);
    }

    #[tokio::test]
    async fn fertility_is_disabled_by_default_and_never_touches_the_network() {
        let store = SqliteStore::in_memory().await.expect("db");
        store
            .add_subscription(&subscription("700", "丁"))
            .await
            .expect("streamer");
        let service = AppService::new(store.clone(), RuntimeSecrets::default()).expect("service");

        let snapshot = service
            .list_fertility_statuses(true)
            .await
            .expect("disabled snapshot");
        assert!(!snapshot.enabled);
        assert_eq!(snapshot.matched, 0);
        assert!(snapshot.statuses.is_empty());
        assert_eq!(snapshot.source, "disabled");

        assert_eq!(
            service
                .refresh_fertility_if_enabled()
                .await
                .expect("noop refresh"),
            None
        );
        assert!(store.get_fertility_cache().await.expect("cache").is_none());
    }

    #[tokio::test]
    async fn fertility_matches_streamers_by_mid_then_linked_account() {
        let payload = crate::fertility::parse_fertility_payload(
            r#"{"data":[{"uid":900,"user":{"username":"直连","avatar":"https://example.com/a.jpg"},"status":"Menstruating","dayInCycle":2,"nextPeriod":"2026-12-04","effectiveCycleLength":28,"effectivePeriodLength":5,"dataPoints":3},{"uid":901,"user":{"username":"官号"},"status":"fertile","dayInCycle":11,"dataPoints":1}],"total":2}"#,
        )
        .expect("payload");

        let direct = streamer_stub(1, "900", "直连主播");
        let mut linked_owner = streamer_stub(2, "950", "挂靠主播");
        linked_owner
            .linked_accounts
            .push(crate::models::LinkedAccount {
                id: 1,
                streamer_id: 2,
                platform: Platform::Bilibili,
                mid: "901".to_owned(),
                name: "官号".to_owned(),
                avatar_url: String::new(),
                handle: None,
                profile_url: None,
                revision: 1,
                created_at: Utc::now(),
            });
        let unmatched = streamer_stub(3, "999", "未收录主播");

        let statuses = match_fertility_statuses(&[direct, linked_owner, unmatched], &payload);
        assert_eq!(statuses.len(), 2);

        let first = statuses
            .iter()
            .find(|item| item.streamer_id == 1)
            .expect("direct");
        assert_eq!(first.status, "menstruating");
        assert_eq!(first.matched_mid, "900");
        assert_eq!(first.cycle_length, Some(28));
        assert_eq!(first.period_length, Some(5));
        assert_eq!(first.day_in_cycle, Some(2));
        assert_eq!(first.next_period.as_deref(), Some("2026-12-04"));
        assert_eq!(first.streamer_avatar, "https://example.com/avatar-1.jpg");

        let second = statuses
            .iter()
            .find(|item| item.streamer_id == 2)
            .expect("linked");
        assert_eq!(second.mid, "950");
        assert_eq!(second.matched_mid, "901");
        assert_eq!(second.status, "fertile");
        assert_eq!(second.data_points, 1);
        // 官方记录没有头像时回落到空串，绝不借用其他主播的头像。
        assert_eq!(second.streamer_avatar, "https://example.com/avatar-2.jpg");
    }

    fn streamer_stub(id: i64, mid: &str, name: &str) -> Streamer {
        Streamer {
            platform: Platform::Bilibili,
            handle: None,
            profile_url: None,
            uploads_playlist_id: None,
            id,
            mid: mid.to_owned(),
            name: name.to_owned(),
            avatar_url: format!("https://example.com/avatar-{id}.jpg"),
            room_id: None,
            live_url: None,
            aliases: Vec::new(),
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
            fans: None,
            last_error: None,
            last_refresh_at: None,
            is_live: false,
            live_title: None,
            live_cover: None,
            live_video_id: None,
            live_started_at: None,
            live_checked_at: None,
            linked_accounts: Vec::new(),
            sort_order: 0,
            revision: 1,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn youtube_community_mapping_preserves_time_provenance() {
        let observed_at = DateTime::parse_from_rfc3339("2026-11-12T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let mut streamer = streamer_stub(1, "UC1234567890123456789012", "Example");
        streamer.platform = Platform::Youtube;

        let estimated = YouTubeCommunityPost {
            id: "estimated".to_owned(),
            channel_id: streamer.mid.clone(),
            author_name: Some(streamer.name.clone()),
            published_at: Some(observed_at - Duration::hours(3)),
            published_at_estimated: true,
            published_text: Some("3 hours ago".to_owned()),
            text: "明天 20:00 直播".to_owned(),
            images: Vec::new(),
            source_url: "https://www.youtube.com/post/estimated".to_owned(),
            renderer_kind: "backstagePostRenderer".to_owned(),
            membership_only: false,
        };
        let mapped = dynamic_post_from_youtube_community(estimated, &streamer, observed_at);
        assert_eq!(mapped.published_at, observed_at - Duration::hours(3));
        assert!(mapped.text.contains("相对时间"));
        assert!(mapped.text.contains("估算"));
        assert!(mapped.text.contains("明天 20:00 直播"));
        assert!(!mapped.text.contains("3 hours ago"));
        assert!(!mapped.text.contains("2026-11-12T09:00:00+00:00"));

        let unknown = YouTubeCommunityPost {
            id: "unknown".to_owned(),
            channel_id: streamer.mid.clone(),
            author_name: None,
            published_at: None,
            published_at_estimated: false,
            published_text: Some("recently-ish".to_owned()),
            text: "今晚直播".to_owned(),
            images: Vec::new(),
            source_url: "https://www.youtube.com/post/unknown".to_owned(),
            renderer_kind: "backstagePostRenderer".to_owned(),
            membership_only: false,
        };
        let mapped = dynamic_post_from_youtube_community(unknown, &streamer, observed_at);
        assert_eq!(mapped.published_at, observed_at);
        assert!(mapped.text.contains("无法可靠解析"));
        assert!(mapped.text.contains("不能视为原发布时间"));
        assert!(!mapped.text.contains("recently-ish"));
        assert!(!mapped.text.contains("2026-11-12T12:00:00+00:00"));
    }

    #[test]
    fn youtube_community_fingerprints_ignore_observation_time() {
        let first_observed = DateTime::parse_from_rfc3339("2026-11-12T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let second_observed = DateTime::parse_from_rfc3339("2026-11-13T12:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        let mut streamer = streamer_stub(1, "UC1234567890123456789012", "Example");
        streamer.platform = Platform::Youtube;

        let make_post = |observed_at: DateTime<Utc>, published_text: &str| YouTubeCommunityPost {
            id: "stable-post".to_owned(),
            channel_id: streamer.mid.clone(),
            author_name: Some(streamer.name.clone()),
            published_at: Some(observed_at - Duration::hours(3)),
            published_at_estimated: true,
            published_text: Some(published_text.to_owned()),
            text: "周五 20:00 直播".to_owned(),
            images: vec!["https://yt3.ggpht.com/poster.jpg".to_owned()],
            source_url: "https://www.youtube.com/post/stable-post".to_owned(),
            renderer_kind: "backstagePostRenderer".to_owned(),
            membership_only: false,
        };
        let first = dynamic_post_from_youtube_community(
            make_post(first_observed, "3 hours ago"),
            &streamer,
            first_observed,
        );
        let second = dynamic_post_from_youtube_community(
            make_post(second_observed, "1 day ago"),
            &streamer,
            second_observed,
        );

        let digest = |post: &DynamicPost| {
            let reserve = reserve_summary(post);
            content_digest_of(
                &text_digest(&text_analysis_context(post)),
                reserve.as_deref(),
                &post.images,
            )
        };
        assert_eq!(first.text, second.text);
        assert_eq!(digest(&first), digest(&second));
        assert_eq!(
            source_fingerprint(&first, "周五直播", "youtube_community"),
            source_fingerprint(&second, "周五直播", "youtube_community")
        );
        assert_ne!(
            source_fingerprint(&first, "周五直播", "youtube_community"),
            source_fingerprint(&first, "周六直播", "youtube_community")
        );

        let mut changed_images = second.clone();
        changed_images.images = vec!["https://yt3.ggpht.com/other.jpg".to_owned()];
        assert_ne!(digest(&first), digest(&changed_images));
        assert_ne!(
            source_fingerprint(&first, "周五直播", "youtube_community"),
            source_fingerprint(&changed_images, "周五直播", "youtube_community")
        );
    }

    #[tokio::test]
    async fn priming_live_statuses_publishes_snapshot_event() {
        let store = SqliteStore::in_memory().await.expect("db");
        let service = AppService::new(store.clone(), RuntimeSecrets::default()).expect("service");

        let statuses = service.prime_live_statuses().await.expect("prime");

        assert!(statuses.is_empty());
        let events = store.events_after(0, 10).await.expect("events");
        assert!(events
            .iter()
            .any(|event| event["type"] == "live_status_snapshot"));
    }

    #[tokio::test]
    async fn refresh_requests_queue_behind_an_active_refresh() {
        let store = SqliteStore::in_memory().await.expect("db");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = Arc::new(
            AppService::with_analyzer(store, RuntimeSecrets::default(), analyzer).expect("service"),
        );
        let first = service.refresh_lock.lock().await;
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queued_service = service.clone();
        let queued_acquired = acquired.clone();
        let queued = tokio::spawn(async move {
            let _second = queued_service.refresh_lock.lock().await;
            queued_acquired.store(true, Ordering::SeqCst);
        });

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!acquired.load(Ordering::SeqCst));
        drop(first);
        tokio::time::timeout(std::time::Duration::from_secs(1), queued)
            .await
            .expect("queued refresh should acquire the lock")
            .expect("queued task");
        assert!(acquired.load(Ordering::SeqCst));
    }

    #[test]
    fn event_matching_uses_threshold_and_requires_unique_target() {
        let now = Utc::now();
        let event = LlmEvent {
            action: LlmEventAction::Schedule,
            performer: "甲".to_owned(),
            aliases: vec!["甲酱".to_owned()],
            participants: Vec::new(),
            room_url: None,
            room_owner: None,
            room_owner_mid: None,
            title: "直播".to_owned(),
            start_at: Some(now + Duration::days(1)),
            end_at: None,
            confidence: 0.95,
        };
        let targets = vec![
            Streamer {
                platform: Platform::Bilibili,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
                id: 1,
                mid: "1".to_owned(),
                name: "甲".to_owned(),
                avatar_url: String::new(),
                room_id: None,
                live_url: None,
                aliases: vec!["甲".to_owned()],
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
                fans: None,
                last_error: None,
                last_refresh_at: None,
                is_live: false,
                live_title: None,
                live_cover: None,
                live_video_id: None,
                live_started_at: None,
                live_checked_at: None,
                linked_accounts: Vec::new(),
                sort_order: 0,
                revision: 1,
                created_at: now,
            },
            Streamer {
                platform: Platform::Bilibili,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
                id: 2,
                mid: "2".to_owned(),
                name: "乙".to_owned(),
                avatar_url: String::new(),
                room_id: None,
                live_url: None,
                aliases: vec!["乙".to_owned()],
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
                fans: None,
                last_error: None,
                last_refresh_at: None,
                is_live: false,
                live_title: None,
                live_cover: None,
                live_video_id: None,
                live_started_at: None,
                live_checked_at: None,
                linked_accounts: Vec::new(),
                sort_order: 1,
                revision: 1,
                created_at: now,
            },
        ];
        assert_eq!(match_event_target(&event, &targets).expect("target").id, 1);
        let ambiguous = LlmEvent {
            action: LlmEventAction::Schedule,
            performer: "甲乙".to_owned(),
            aliases: vec!["甲".to_owned(), "乙".to_owned()],
            ..event
        };
        assert!(match_event_target(&ambiguous, &targets).is_none());
    }

    #[tokio::test]
    async fn asoul_event_matches_all_three_subscribed_members() {
        let store = SqliteStore::in_memory().await.expect("db");
        let members = [
            store
                .add_subscription(&subscription("301", "乃琳"))
                .await
                .expect("乃琳"),
            store
                .add_subscription(&subscription("302", "嘉然"))
                .await
                .expect("嘉然"),
            store
                .add_subscription(&subscription("303", "贝拉"))
                .await
                .expect("贝拉"),
        ];
        let event = LlmEvent {
            action: LlmEventAction::Schedule,
            performer: "A-SOUL".to_owned(),
            aliases: Vec::new(),
            participants: vec!["A-SOUL".to_owned()],
            room_url: None,
            room_owner: Some("乃琳".to_owned()),
            room_owner_mid: None,
            title: "团体直播".to_owned(),
            start_at: Some(Utc::now() + Duration::days(1)),
            end_at: None,
            confidence: 0.98,
        };
        let canonical = canonicalize_event_participants(&event, &members);
        assert_eq!(canonical.participants, vec!["乃琳", "嘉然", "贝拉"]);
        let target = match_event_target(&canonical, &members).expect("A-SOUL target");
        assert_eq!(
            target.id,
            members[0].id.min(members[1].id).min(members[2].id)
        );
    }

    #[tokio::test]
    async fn image_and_text_group_events_in_distinct_rooms_remain_separate() {
        let store = SqliteStore::in_memory().await.expect("db");
        let xinyi = store
            .add_subscription(&subscription("401", "心宜不是心仪"))
            .await
            .expect("xinyi");
        let sinuo = store
            .add_subscription(&subscription("402", "思诺snow"))
            .await
            .expect("sinuo");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let start_at = now + Duration::days(3);
        let schedule = ParsedSchedule {
            title: "心宜思诺周年纪念直播·《执笔入梦》".to_owned(),
            start_at,
            end_at: None,
            confidence: 0.96,
        };
        let image_post = DynamicPost {
            id: "group-image".to_owned(),
            author_mid: "900".to_owned(),
            author_name: "官方周表".to_owned(),
            published_at: now,
            text: "10月31日心宜和思诺在思诺直播间".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/group-image".to_owned(),
            raw_kind: "draw".to_owned(),
            reserve: None,
        };
        let mut text_post = image_post.clone();
        text_post.id = "group-text".to_owned();
        text_post.published_at = now + Duration::hours(1);
        text_post.text = "书梦之旅".to_owned();
        let participants = vec!["心宜不是心仪".to_owned(), "思诺snow".to_owned()];
        assert!(service
            .upsert_candidate(
                &xinyi,
                &schedule,
                "image",
                &image_post,
                "https://i0.hdslb.com/group.jpg",
                "image:group-image",
                "image-fingerprint",
                Some("https://live.bilibili.com/555555"),
                Some("402"),
                Some("思诺snow"),
                &participants,
            )
            .await
            .expect("upsert candidate"));
        let short = ParsedSchedule {
            title: "书梦之旅".to_owned(),
            ..schedule
        };
        assert!(service
            .upsert_candidate(
                &sinuo,
                &short,
                "text",
                &text_post,
                "editor",
                "text:group-text",
                "text-fingerprint",
                Some("https://live.bilibili.com/444444"),
                Some("401"),
                Some("心宜不是心仪"),
                &participants,
            )
            .await
            .expect("upsert candidate"));
        let schedules = store.list_all_schedules().await.expect("schedules");
        // Explicitly different rooms are separate broadcasts even with the same cast.
        assert_eq!(schedules.len(), 2);
        for (room, owner, title) in [
            (
                "https://live.bilibili.com/555555",
                "402",
                "心宜思诺周年纪念直播·《执笔入梦》",
            ),
            ("https://live.bilibili.com/444444", "401", "书梦之旅"),
        ] {
            let card = schedules
                .iter()
                .find(|card| card.room_url.as_deref() == Some(room))
                .unwrap();
            assert_eq!(card.room_owner_mid.as_deref(), Some(owner));
            assert_eq!(card.title, title);
            assert_eq!(card.participants, participants);
        }
        assert_eq!(service.dedupe_schedule_duplicates().await.unwrap(), 0);
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn group_text_merges_into_reservation_from_the_same_room() {
        let store = SqliteStore::in_memory().await.expect("db");
        let xinyi = store
            .add_subscription(&subscription("401", "心宜不是心仪"))
            .await
            .expect("xinyi");
        let sinuo = store
            .add_subscription(&subscription("402", "思诺snow"))
            .await
            .expect("sinuo");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let now = Utc::now();
        let reserve_post = DynamicPost {
            id: "sinuo-anniversary-reserve".to_owned(),
            author_mid: sinuo.mid.clone(),
            author_name: sinuo.name.clone(),
            published_at: now,
            text: "三周年纪念直播预约".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/sinuo-anniversary-reserve".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：【3D出道周年纪念】执笔入梦".to_owned(),
                description: "明天 20:00 直播".to_owned(),
                room_id: Some("555555".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let reservation = parse_reserve_schedule(
            reserve_post.reserve.as_ref().expect("reserve"),
            reserve_post.published_at,
            now,
        )
        .expect("reservation schedule");
        let text_post = DynamicPost {
            id: "xinyi-sinuo-anniversary-text".to_owned(),
            author_mid: xinyi.mid.clone(),
            author_name: xinyi.name.clone(),
            published_at: now,
            text: "心宜和思诺三周年纪念直播".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/xinyi-sinuo-anniversary-text".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: None,
        };
        let text_schedule = ParsedSchedule {
            title: "心宜&思诺三周年纪念直播《执笔入梦》".to_owned(),
            start_at: reservation.start_at,
            end_at: None,
            confidence: 0.96,
        };
        let participants = vec!["心宜不是心仪".to_owned(), "思诺snow".to_owned()];
        assert!(service
            .upsert_candidate(
                &xinyi,
                &text_schedule,
                "text",
                &text_post,
                "editor",
                "text:xinyi-sinuo-anniversary-text:0",
                "text-fingerprint",
                Some("https://live.bilibili.com/555555"),
                Some("402"),
                Some("思诺snow"),
                &participants,
            )
            .await
            .expect("upsert candidate"));

        let stats = service
            .process_source_posts(
                "402",
                std::slice::from_ref(&sinuo),
                std::slice::from_ref(&reserve_post),
                &AppSettings::default(),
                None,
            )
            .await
            .expect("process reservation");

        assert_eq!(stats.llm_calls, 0);
        // 没有池子就完全不碰模型：卡片正则兜底独立完成。
        assert_eq!(analyzer.call_count(), 0);
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].source_kind, "reserve");
        assert_eq!(schedules[0].title, "【3D出道周年纪念】执笔入梦");
        assert_eq!(schedules[0].start_at, reservation.start_at);
        assert_eq!(schedules[0].participants, participants);
        assert_eq!(
            schedules[0].room_url.as_deref(),
            Some("https://live.bilibili.com/555555")
        );
    }

    #[tokio::test]
    async fn group_event_creates_one_shared_schedule_with_external_room() {
        let store = SqliteStore::in_memory().await.expect("db");
        let first = store
            .add_subscription(&subscription("100", "嘉然今天吃什么"))
            .await
            .expect("first");
        let second = store
            .add_subscription(&subscription("200", "贝拉kira"))
            .await
            .expect("second");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let event = LlmEvent {
            action: LlmEventAction::Schedule,
            performer: "嘉然".to_owned(),
            aliases: Vec::new(),
            participants: vec!["嘉然".to_owned(), "贝拉".to_owned(), "外部嘉宾".to_owned()],
            room_url: Some("https://live.bilibili.com/987654".to_owned()),
            room_owner: Some("嘉然今天吃什么".to_owned()),
            room_owner_mid: None,
            title: "联合歌回".to_owned(),
            start_at: Some(now + Duration::days(1)),
            end_at: None,
            confidence: 0.97,
        };
        let targets = [first.clone(), second.clone()];
        let event = canonicalize_event_participants(&event, &targets);
        assert_eq!(
            event.participants,
            vec!["嘉然今天吃什么", "贝拉kira", "外部嘉宾"]
        );
        let target = match_event_target(&event, &targets).expect("group target");
        assert_eq!(target.id, first.id.min(second.id));
        let post = DynamicPost {
            id: "group-post".to_owned(),
            author_mid: "100".to_owned(),
            author_name: "嘉然今天吃什么".to_owned(),
            published_at: now,
            text: "嘉然、贝拉和外部嘉宾联合直播".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/group-post".to_owned(),
            raw_kind: "dynamic".to_owned(),
            reserve: None,
        };
        let (inserted, cancelled) = service
            .apply_llm_event(
                &target,
                &targets,
                &event,
                "text",
                &post,
                "editor",
                "text:group-post:0",
                "group-fingerprint",
            )
            .await
            .expect("apply group event");
        assert!(inserted);
        assert!(!cancelled);
        let schedules = store
            .list_schedules(now, now + Duration::days(2), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(
            schedules[0].participants,
            vec!["嘉然今天吃什么", "贝拉kira", "外部嘉宾"]
        );
        assert_eq!(
            schedules[0].room_url.as_deref(),
            Some("https://live.bilibili.com/987654")
        );
        assert_eq!(
            schedules[0].room_owner_name.as_deref(),
            Some("嘉然今天吃什么")
        );
        assert_eq!(schedules[0].room_owner_mid.as_deref(), Some("100"));
        assert_eq!(schedules[0].source_published_at, Some(post.published_at));
    }

    #[test]
    fn asoul_member_sources_include_personal_and_official_accounts() {
        let streamer = Streamer {
            platform: Platform::Bilibili,
            handle: None,
            profile_url: None,
            uploads_playlist_id: None,
            id: 1,
            mid: "672342685".to_owned(),
            name: "乃琳Queen".to_owned(),
            avatar_url: String::new(),
            room_id: Some("22625025".to_owned()),
            live_url: Some("https://live.bilibili.com/22625025".to_owned()),
            aliases: vec!["乃琳".to_owned()],
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
            fans: None,
            last_error: None,
            last_refresh_at: None,
            is_live: false,
            live_title: None,
            live_cover: None,
            live_video_id: None,
            live_started_at: None,
            live_checked_at: None,
            linked_accounts: Vec::new(),
            sort_order: 0,
            revision: 1,
            created_at: Utc::now(),
        };
        let sources = sources_for_streamers(std::slice::from_ref(&streamer));
        assert_eq!(sources.len(), 2);
        let personal = sources
            .iter()
            .find(|(mid, _)| mid == &streamer.mid)
            .unwrap();
        let official = sources
            .iter()
            .find(|(mid, _)| mid == A_SOUL_OFFICIAL_MID)
            .unwrap();
        assert_eq!(personal.1[0].id, streamer.id);
        assert_eq!(official.1[0].id, streamer.id);
    }

    #[test]
    fn multiple_asoul_members_share_one_official_feed_source() {
        let now = Utc::now();
        let streamers = [
            (1, "672342685", "乃琳Queen", "乃琳"),
            (2, "672328094", "嘉然今天吃什么", "嘉然"),
            (3, "672353429", "贝拉kira", "贝拉"),
        ]
        .into_iter()
        .map(|(id, mid, name, alias)| Streamer {
            platform: Platform::Bilibili,
            handle: None,
            profile_url: None,
            uploads_playlist_id: None,
            id,
            mid: mid.to_owned(),
            name: name.to_owned(),
            avatar_url: String::new(),
            room_id: None,
            live_url: None,
            aliases: vec![alias.to_owned()],
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
            fans: None,
            last_error: None,
            last_refresh_at: None,
            is_live: false,
            live_title: None,
            live_cover: None,
            live_video_id: None,
            live_started_at: None,
            live_checked_at: None,
            linked_accounts: Vec::new(),
            sort_order: 0,
            revision: 1,
            created_at: now,
        })
        .collect::<Vec<_>>();

        let sources = sources_for_streamers(&streamers);
        assert_eq!(sources.len(), 4);
        let (_, official_targets) = sources
            .iter()
            .find(|(mid, _)| mid == A_SOUL_OFFICIAL_MID)
            .expect("official source");
        assert_eq!(official_targets.len(), 3);
        let unique_targets = official_targets
            .iter()
            .map(|target| target.id)
            .collect::<HashSet<_>>();
        assert_eq!(unique_targets.len(), 3);
    }

    #[test]
    fn search_ranks_exact_and_subscribed_accounts_and_deduplicates() {
        let now = Utc::now();
        let streamer = Streamer {
            platform: Platform::Bilibili,
            handle: None,
            profile_url: None,
            uploads_playlist_id: None,
            id: 1,
            mid: "100".to_owned(),
            name: "嘉然".to_owned(),
            avatar_url: String::new(),
            room_id: Some("123".to_owned()),
            live_url: Some("https://live.bilibili.com/123".to_owned()),
            aliases: vec!["然然".to_owned()],
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
            fans: None,
            last_error: None,
            last_refresh_at: None,
            is_live: false,
            live_title: None,
            live_cover: None,
            live_video_id: None,
            live_started_at: None,
            live_checked_at: None,
            linked_accounts: Vec::new(),
            sort_order: 0,
            revision: 1,
            created_at: now,
        };
        assert!(search_matches_subscription("@然然", &streamer));
        let account = |mid: &str, name: &str, fans: u64| crate::models::AccountSearchResult {
            platform: Platform::Bilibili,
            mid: mid.to_owned(),
            name: name.to_owned(),
            avatar_url: String::new(),
            sign: String::new(),
            handle: None,
            profile_url: None,
            room_id: None,
            fans,
            is_live: false,
            live_url: None,
        };
        let mut results = vec![
            account("200", "嘉然粉丝团", 999_999),
            account("100", "嘉然", 10),
            account("100", "嘉然", 10),
        ];
        dedupe_and_rank_search_results(&mut results, "嘉然", &[streamer]);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].mid, "100");
    }

    #[test]
    fn dynamic_posts_are_ordered_oldest_first() {
        let now = Utc::now();
        let newer = DynamicPost {
            id: "newer".to_owned(),
            author_mid: "100".to_owned(),
            author_name: "甲".to_owned(),
            published_at: now + Duration::hours(1),
            text: "新".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/newer".to_owned(),
            raw_kind: "dynamic".to_owned(),
            reserve: None,
        };
        let mut older = newer.clone();
        older.id = "older".to_owned();
        older.published_at = now;
        older.text = "旧".to_owned();
        let ordered = ordered_posts_oldest_first(&[newer, older]);
        assert_eq!(
            ordered
                .iter()
                .map(|post| post.id.as_str())
                .collect::<Vec<_>>(),
            vec!["older", "newer"]
        );
    }

    #[tokio::test]
    async fn newer_post_updates_older_schedule_but_old_post_cannot_overwrite_it() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let start_at = now + Duration::days(1);
        let schedule = ParsedSchedule {
            title: "联合歌回".to_owned(),
            start_at,
            end_at: None,
            confidence: 0.95,
        };
        let old_post = DynamicPost {
            id: "old-post".to_owned(),
            author_mid: "100".to_owned(),
            author_name: "甲".to_owned(),
            published_at: now,
            text: "旧公告".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/old-post".to_owned(),
            raw_kind: "dynamic".to_owned(),
            reserve: None,
        };
        let mut new_post = old_post.clone();
        new_post.id = "new-post".to_owned();
        new_post.published_at = now + Duration::hours(1);
        new_post.text = "新公告".to_owned();
        let mut even_older_post = old_post.clone();
        even_older_post.id = "even-older-post".to_owned();
        even_older_post.published_at = now - Duration::hours(1);

        assert!(service
            .upsert_candidate(
                &streamer,
                &schedule,
                "text",
                &old_post,
                "editor",
                "text:old-post",
                "old-fingerprint",
                Some("https://live.bilibili.com/111111"),
                Some("100"),
                Some("甲"),
                &["甲".to_owned(), "乙".to_owned()],
            )
            .await
            .expect("upsert candidate"));
        assert!(!service
            .upsert_candidate(
                &streamer,
                &schedule,
                "text",
                &new_post,
                "editor",
                "text:new-post",
                "new-fingerprint",
                Some("https://live.bilibili.com/222222"),
                Some("200"),
                Some("乙"),
                &["甲".to_owned(), "乙".to_owned(), "外部嘉宾".to_owned()],
            )
            .await
            .expect("upsert candidate"));
        let schedules = store
            .list_all_schedules()
            .await
            .expect("schedules after update");
        assert_eq!(schedules.len(), 1);
        assert_eq!(
            schedules[0].room_url.as_deref(),
            Some("https://live.bilibili.com/222222")
        );
        assert_eq!(schedules[0].room_owner_name.as_deref(), Some("乙"));
        assert_eq!(schedules[0].participants.len(), 3);
        assert_eq!(
            schedules[0].source_published_at,
            Some(new_post.published_at)
        );

        assert!(!service
            .upsert_candidate(
                &streamer,
                &schedule,
                "text",
                &even_older_post,
                "editor",
                "text:even-older-post",
                "even-older-fingerprint",
                Some("https://live.bilibili.com/333333"),
                Some("100"),
                Some("甲"),
                &["甲".to_owned()],
            )
            .await
            .expect("upsert candidate"));
        let schedules = store
            .list_all_schedules()
            .await
            .expect("schedules after stale update");
        assert_eq!(
            schedules[0].room_url.as_deref(),
            Some("https://live.bilibili.com/222222")
        );
        assert_eq!(
            schedules[0].source_published_at,
            Some(new_post.published_at)
        );
    }

    #[tokio::test]
    async fn authored_text_can_move_upcoming_reservation_time() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "乃琳Queen"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let old_start = now + Duration::hours(30);
        let new_start = old_start - Duration::hours(1);
        let reserve_post = DynamicPost {
            id: "reserve-post".to_owned(),
            author_mid: "100".to_owned(),
            author_name: "乃琳Queen".to_owned(),
            published_at: now - Duration::hours(2),
            text: "直播预约".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/reserve-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：【突击】影之刃零DAY5！".to_owned(),
                description: "明天 16:00 直播".to_owned(),
                room_id: Some("5850386".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let reserve_schedule = ParsedSchedule {
            title: "【突击】影之刃零DAY5！".to_owned(),
            start_at: old_start,
            end_at: None,
            confidence: 1.0,
        };
        assert!(service
            .upsert_candidate(
                &streamer,
                &reserve_schedule,
                "reserve",
                &reserve_post,
                "",
                "reserve:reserve-post",
                "reserve-fingerprint",
                Some("https://live.bilibili.com/5850386"),
                Some("100"),
                Some("乃琳Queen"),
                &["乃琳Queen".to_owned()],
            )
            .await
            .expect("upsert candidate"));

        let new_post = DynamicPost {
            id: "new-post".to_owned(),
            author_mid: "100".to_owned(),
            author_name: "乃琳Queen".to_owned(),
            published_at: now,
            text: "奶淇琳！明天下午3点就来！早点和奶淇琳见面捏".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/new-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        let event = LlmEvent {
            action: LlmEventAction::Schedule,
            performer: "乃琳".to_owned(),
            aliases: vec!["奶淇琳".to_owned(), "乃宝".to_owned()],
            participants: Vec::new(),
            room_url: None,
            room_owner: None,
            room_owner_mid: None,
            title: "和奶淇琳见面".to_owned(),
            start_at: Some(new_start),
            end_at: None,
            confidence: 0.7,
        };
        assert!(
            match_event_target_for_post(&event, std::slice::from_ref(&streamer), "100").is_some()
        );
        let changed = ParsedSchedule {
            title: "和奶淇琳见面".to_owned(),
            start_at: new_start,
            end_at: None,
            confidence: 0.7,
        };
        assert!(!service
            .upsert_candidate(
                &streamer,
                &changed,
                "text",
                &new_post,
                "editor",
                "text:new-post",
                "new-fingerprint",
                Some("https://live.bilibili.com/5850386"),
                Some("100"),
                Some("乃琳Queen"),
                &["乃琳".to_owned()],
            )
            .await
            .expect("upsert candidate"));
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].title, "【突击】影之刃零DAY5！");
        assert_eq!(schedules[0].start_at, new_start);
        assert_eq!(
            schedules[0].source_published_at,
            Some(new_post.published_at)
        );
    }

    #[tokio::test]
    async fn source_processing_reports_real_stage_progress() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service =
            AppService::with_analyzer(store, RuntimeSecrets::default(), analyzer).expect("service");
        let post = DynamicPost {
            id: "progress-post".to_owned(),
            author_mid: "100".to_owned(),
            author_name: "甲".to_owned(),
            published_at: Utc::now(),
            text: String::new(),
            images: vec!["https://i0.hdslb.com/test-calendar.png".to_owned()],
            source_url: "https://t.bilibili.com/progress-post".to_owned(),
            raw_kind: "dynamic".to_owned(),
            reserve: None,
        };
        let reports = Arc::new(std::sync::Mutex::new(Vec::<RefreshProgress>::new()));
        let captured = reports.clone();
        let mut event_state = RefreshEventState::default();
        let pool = test_pool("fixture-model");
        let target = ProgressTarget::for_streamer(&streamer);
        service
            .process_source_posts_with_progress(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
                &mut event_state,
                Some(&target),
                &move |progress| captured.lock().expect("progress").push(progress),
                0,
                1,
            )
            .await
            .expect("process");
        let reports = reports.lock().expect("reports");
        // 一条动态只报两步（开始分析、写入结果）：9 张图的动态以前要报 11 步。
        assert!(reports.len() >= 2);
        assert!(reports
            .windows(2)
            .all(|pair| pair[0].percent <= pair[1].percent));
        assert!(reports.iter().any(|report| report.stage == "写入日程结果"));
        // 图片与正文合并成一次分析后，进度按动态条数计步：
        // 单张图的动态只有一步「分析动态日程」，不再有逐图阶段。
        assert!(reports
            .iter()
            .any(|report| report.stage.starts_with("分析动态日程")));
        assert!(
            reports.iter().any(|report| report.percent >= 75),
            "{reports:?}"
        );
    }

    #[tokio::test]
    async fn model_cancel_event_deletes_matching_schedule() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let start_at = Utc::now() + Duration::days(1);
        let (schedule, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: streamer.id,
                participants: Vec::new(),
                title: "杂谈".to_owned(),
                start_at,
                end_at: None,
                room_url: streamer.live_url.clone(),
                room_owner_mid: None,
                room_owner_name: None,
                source_kind: "text".to_owned(),
                source_post_id: Some("old-post".to_owned()),
                source_image_url: None,
                source_author_mid: Some("100".to_owned()),
                source_fingerprint: Some("old-fingerprint".to_owned()),
                source_identity: Some("text:old-post:1".to_owned()),
                source_published_at: None,
                confidence: 0.95,
            })
            .await
            .expect("schedule");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: vec![LlmEvent {
                action: LlmEventAction::Cancel,
                performer: "甲".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "杂谈".to_owned(),
                start_at: Some(start_at),
                end_at: None,
                confidence: 0.97,
            }],
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let post = DynamicPost {
            id: "cancel-post".to_owned(),
            author_mid: "100".to_owned(),
            author_name: "甲".to_owned(),
            published_at: Utc::now(),
            text: "明天同一时间的杂谈取消".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/cancel-post".to_owned(),
            raw_kind: "dynamic".to_owned(),
            reserve: None,
        };
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");
        assert_eq!(stats.new_schedules, 0);
        assert_eq!(stats.cancelled_schedules, 1);
        assert!(store.get_schedule(schedule.id).await.is_err());
        assert!(store
            .is_source_suppressed(streamer.id, "old-post", "old-fingerprint")
            .await
            .expect("suppression"));
    }

    #[tokio::test]
    async fn reserved_post_cancellation_runs_through_the_model() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let published_at = Utc::now();
        let reserve = ReserveCard {
            title: "直播预约：【突击】电台电台！".to_owned(),
            description: "明天 19:00 直播".to_owned(),
            room_id: Some("22637261".to_owned()),
            jump_url: None,
            stype: Some(2),
        };
        let card_start = parse_reserve_schedule(&reserve, published_at, published_at)
            .expect("reservation schedule")
            .start_at;
        // Keep the cancelled shows distinct from the new reservation at every
        // wall-clock time. now+1day+1hour overlaps the 19:00 card around 18:00
        // Beijing time and exercises the unrelated 10-minute merge path.
        let old_start = card_start - Duration::hours(4);
        let song_start = card_start - Duration::hours(3);
        assert!(card_start.signed_duration_since(song_start) > Duration::minutes(10));
        let (old_schedule, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: streamer.id,
                participants: Vec::new(),
                title: "【突击】旧场".to_owned(),
                start_at: old_start,
                end_at: None,
                room_url: streamer.live_url.clone(),
                room_owner_mid: Some(streamer.mid.clone()),
                room_owner_name: Some(streamer.name.clone()),
                source_kind: "reserve".to_owned(),
                source_post_id: Some("old-surprise-post".to_owned()),
                source_image_url: None,
                source_author_mid: Some("100".to_owned()),
                source_fingerprint: Some("old-surprise-fingerprint".to_owned()),
                source_identity: Some("reserve:old-surprise-post".to_owned()),
                source_published_at: None,
                confidence: 1.0,
            })
            .await
            .expect("old schedule");
        let (old_song, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: streamer.id,
                participants: Vec::new(),
                title: "歌回".to_owned(),
                start_at: song_start,
                end_at: None,
                room_url: streamer.live_url.clone(),
                room_owner_mid: Some(streamer.mid.clone()),
                room_owner_name: Some(streamer.name.clone()),
                source_kind: "text".to_owned(),
                source_post_id: Some("old-song-post".to_owned()),
                source_image_url: None,
                source_author_mid: Some("100".to_owned()),
                source_fingerprint: Some("old-song-fingerprint".to_owned()),
                source_identity: Some("text:old-song-post:1".to_owned()),
                source_published_at: None,
                confidence: 0.95,
            })
            .await
            .expect("old song");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: vec![LlmEvent {
                action: LlmEventAction::Cancel,
                performer: "甲".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "歌回和突击".to_owned(),
                start_at: None,
                end_at: None,
                confidence: 0.97,
            }],
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = DynamicPost {
            id: "current-post".to_owned(),
            author_mid: "100".to_owned(),
            author_name: "甲".to_owned(),
            published_at,
            text: "周三的歌回和突击往后放一下，咱们先电台见面".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/current-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: Some(reserve),
        };
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 带卡片的动态照样送模型；模型给的是取消，就按取消处理。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.new_schedules, 1);
        // 事件标题是“歌回和突击”，两场都被点名取消。
        let remaining = store.list_all_schedules().await.expect("remaining");
        for cancelled_id in [old_schedule.id, old_song.id] {
            assert!(remaining
                .iter()
                .all(|schedule| schedule.id != cancelled_id || schedule.cancelled));
        }
        // 模型没给卡片那一场，正则兜底把预约场次补上。
        assert!(remaining.iter().any(|schedule| {
            schedule.source_post_id.as_deref() == Some("current-post")
                && schedule.source_kind == "reserve"
                && schedule.title == "【突击】电台电台！"
        }));
    }

    #[tokio::test]
    async fn group_dedupe_prefers_and_preserves_manual_schedule() {
        let store = SqliteStore::in_memory().await.expect("db");
        let first = store
            .add_subscription(&subscription("301", "团播甲"))
            .await
            .expect("first");
        let second = store
            .add_subscription(&subscription("302", "团播乙"))
            .await
            .expect("second");
        let service = AppService::new(store.clone(), RuntimeSecrets::default()).expect("service");
        let start = Utc::now() + Duration::days(1);
        let participants = vec!["团播甲".to_owned(), "团播乙".to_owned()];
        let (manual, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: first.id,
                participants: participants.clone(),
                title: "手工锁定团播".to_owned(),
                start_at: start,
                end_at: Some(start + Duration::hours(2)),
                room_url: Some("https://live.bilibili.com/12345".to_owned()),
                room_owner_mid: Some(first.mid.clone()),
                room_owner_name: Some(first.name.clone()),
                source_kind: "manual".to_owned(),
                source_post_id: None,
                source_image_url: None,
                source_author_mid: None,
                source_fingerprint: None,
                source_identity: None,
                source_published_at: None,
                confidence: 1.0,
            })
            .await
            .expect("manual schedule");
        store
            .update_schedule(
                manual.id,
                &UpdateSchedule {
                    title: Some("手工锁定团播".to_owned()),
                    start_at: None,
                    end_at: None,
                    room_url: None,
                },
            )
            .await
            .expect("lock manual schedule");
        let (automatic, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: second.id,
                participants,
                title: "自动识别团播".to_owned(),
                start_at: start + Duration::minutes(5),
                end_at: Some(start + Duration::hours(2)),
                room_url: Some("https://live.bilibili.com/12345".to_owned()),
                room_owner_mid: Some(first.mid.clone()),
                room_owner_name: Some(first.name.clone()),
                source_kind: "text".to_owned(),
                source_post_id: Some("group-dedupe-post".to_owned()),
                source_image_url: None,
                source_author_mid: Some(first.mid.clone()),
                source_fingerprint: Some("group-dedupe-fingerprint".to_owned()),
                source_identity: Some("text:group-dedupe-post".to_owned()),
                source_published_at: Some(Utc::now()),
                confidence: 0.95,
            })
            .await
            .expect("automatic schedule");

        assert_eq!(
            service.dedupe_schedule_duplicates().await.expect("dedupe"),
            1
        );
        assert!(
            store
                .get_schedule(manual.id)
                .await
                .expect("manual remains")
                .manual_locked
        );
        assert!(store.get_schedule(automatic.id).await.is_err());
    }

    #[tokio::test]
    async fn room_slot_dedupe_ignores_missing_cast_but_preserves_other_rooms_and_sessions() {
        let store = SqliteStore::in_memory().await.unwrap();
        let publisher = store
            .add_subscription(&subscription("1001", "公告账号"))
            .await
            .unwrap();
        let live_owner = store
            .add_subscription(&subscription("1002", "贝拉kira"))
            .await
            .unwrap();
        let start: DateTime<Utc> = "2026-11-13T11:50:00Z".parse().unwrap();
        let make = |streamer_id, title: &str, start_at, room: &str, participants| NewSchedule {
            streamer_id,
            title: title.into(),
            start_at,
            participants,
            end_at: None,
            room_url: Some(room.into()),
            room_owner_mid: None,
            room_owner_name: None,
            source_kind: "image".into(),
            source_post_id: None,
            source_image_url: None,
            source_author_mid: None,
            source_fingerprint: None,
            source_identity: None,
            source_published_at: None,
            confidence: 0.95,
        };
        let announced = store
            .upsert_schedule(&make(
                publisher.id,
                "默契加倍中",
                start + Duration::minutes(10),
                "https://live.bilibili.com/12345?from=post",
                vec!["嘉然".into(), "贝拉".into()],
            ))
            .await
            .unwrap()
            .0;
        let mut live_input = make(
            live_owner.id,
            "【3D】默契加倍中",
            start,
            "https://live.bilibili.com/12345/",
            Vec::new(),
        );
        live_input.source_kind = "live".into();
        let live_card = store.upsert_schedule(&live_input).await.unwrap().0;
        assert!(schedules_equivalent(&announced, &live_card));
        let candidates = vec![announced.clone()];
        let incoming = ParsedSchedule {
            title: live_input.title.clone(),
            start_at: start,
            end_at: None,
            confidence: 0.95,
        };
        assert_eq!(
            group_duplicates(
                &candidates,
                &incoming,
                &[],
                None,
                live_input.room_url.as_deref()
            )
            .len(),
            1
        );
        let mut other = live_card.clone();
        other.room_url = Some("https://live.bilibili.com/67890".into());
        other.participants = announced.participants.clone();
        assert!(!schedules_equivalent(&announced, &other));
        other = live_card.clone();
        other.start_at = announced.start_at + Duration::minutes(10) + Duration::milliseconds(1);
        assert!(!schedules_equivalent(&announced, &other));
        other = live_card.clone();
        other.cancelled = true;
        assert!(!schedules_equivalent(&announced, &other));
        let service = AppService::new(store.clone(), RuntimeSecrets::default()).unwrap();
        assert_eq!(service.dedupe_schedule_duplicates().await.unwrap(), 1);
        let remaining = store.list_all_schedules().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].title, "默契加倍中");
        assert_eq!(remaining[0].start_at, announced.start_at);
    }

    #[test]
    fn room_identity_handles_owner_aliases_and_distinct_youtube_videos() {
        assert_eq!(
            same_schedule_room(
                Some("100"),
                Some("https://live.bilibili.com/1"),
                Some("100"),
                Some("https://live.bilibili.com/12345")
            ),
            Some(true)
        );
        assert_eq!(
            same_schedule_room(Some("100"), None, Some("200"), None),
            Some(false)
        );
        assert_eq!(same_schedule_room(None, None, None, None), None);
        assert_eq!(
            same_schedule_room(
                Some("channel"),
                Some("https://www.youtube.com/watch?v=a"),
                Some("channel"),
                Some("https://www.youtube.com/watch?v=b")
            ),
            Some(false)
        );
    }

    #[tokio::test]
    async fn single_participant_duplicate_sources_dedupe_by_room_and_slot() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("401315430", "星瞳_Official"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let start = now + Duration::days(2);
        let end = start + Duration::hours(2) + Duration::minutes(30);
        let room_url = Some("https://live.bilibili.com/22886883".to_owned());
        let participants = vec![streamer.name.clone()];
        let variants = [
            (
                "reserve",
                "星瞳2026年生日会",
                None,
                "reserve:post-reserve",
                "post-reserve",
            ),
            (
                "image",
                "2026年生日会直播",
                Some(end),
                "image:post-image-1:digest:0",
                "post-image-1",
            ),
            (
                "image",
                "2026年生日会·偷星大盗",
                None,
                "image:post-image-2:digest:0",
                "post-image-2",
            ),
            (
                "image",
                "偷星大盗·2026生日会",
                None,
                "image:post-image-3:digest:0",
                "post-image-3",
            ),
        ];
        let mut canonical_id = 0;
        for (index, (kind, title, end_at, identity, post_id)) in variants.into_iter().enumerate() {
            let (schedule, _) = store
                .upsert_schedule(&NewSchedule {
                    streamer_id: streamer.id,
                    participants: participants.clone(),
                    title: title.to_owned(),
                    start_at: start,
                    end_at,
                    room_url: room_url.clone(),
                    room_owner_mid: Some(streamer.mid.clone()),
                    room_owner_name: Some(streamer.name.clone()),
                    source_kind: kind.to_owned(),
                    source_post_id: Some(post_id.to_owned()),
                    source_image_url: (kind == "image")
                        .then(|| format!("https://example.com/{index}.png")),
                    source_author_mid: Some(streamer.mid.clone()),
                    source_fingerprint: Some(format!("fingerprint-{index}")),
                    source_identity: Some(identity.to_owned()),
                    source_published_at: Some(now + Duration::minutes(index as i64)),
                    confidence: 0.95,
                })
                .await
                .expect("schedule");
            if index == 0 {
                canonical_id = schedule.id;
            }
        }

        let service = AppService::new(store.clone(), RuntimeSecrets::default()).expect("service");
        assert_eq!(
            service.dedupe_schedule_duplicates().await.expect("dedupe"),
            3
        );
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].id, canonical_id);
        assert_eq!(schedules[0].title, "星瞳2026年生日会");
        assert_eq!(schedules[0].source_kind, "reserve");
        assert_eq!(schedules[0].end_at, Some(end));
        let source_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM schedule_sources WHERE schedule_id = ?")
                .bind(canonical_id)
                .fetch_one(store.pool())
                .await
                .expect("source count");
        assert_eq!(source_count, 4);
    }

    #[tokio::test]
    async fn reservation_and_image_same_room_slot_merge_across_title_wording() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("401315430", "星瞳_Official"))
            .await
            .expect("streamer");
        let service = AppService::new(store.clone(), RuntimeSecrets::default()).expect("service");
        let now = Utc::now();
        let start = now + Duration::days(2);
        let room_url = "https://live.bilibili.com/22886883";
        let (reserved, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: streamer.id,
                participants: vec![streamer.name.clone()],
                title: "星瞳2026年生日会".to_owned(),
                start_at: start,
                end_at: None,
                room_url: Some(room_url.to_owned()),
                room_owner_mid: Some(streamer.mid.clone()),
                room_owner_name: Some(streamer.name.clone()),
                source_kind: "reserve".to_owned(),
                source_post_id: Some("reserve-post".to_owned()),
                source_image_url: None,
                source_author_mid: Some(streamer.mid.clone()),
                source_fingerprint: Some("reserve-fingerprint".to_owned()),
                source_identity: Some("reserve:reserve-post".to_owned()),
                source_published_at: Some(now),
                confidence: 0.95,
            })
            .await
            .expect("reserved schedule");
        let post = DynamicPost {
            id: "image-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now + Duration::minutes(1),
            text: "生日会海报".to_owned(),
            images: vec!["https://example.com/birthday.png".to_owned()],
            source_url: "https://t.bilibili.com/image-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        let candidate = ParsedSchedule {
            title: "偷星大盗·2026生日会".to_owned(),
            start_at: start,
            end_at: None,
            confidence: 0.98,
        };
        let participants = vec![streamer.name.clone()];
        let inserted = service
            .upsert_candidate(
                &streamer,
                &candidate,
                "image",
                &post,
                "https://example.com/birthday.png",
                "image:image-post:digest:0",
                "image-fingerprint",
                Some(room_url),
                Some(streamer.mid.as_str()),
                Some(streamer.name.as_str()),
                &participants,
            )
            .await
            .expect("upsert candidate");

        assert!(!inserted);
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].id, reserved.id);
        assert_eq!(schedules[0].title, "星瞳2026年生日会");
        let image_source_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM schedule_sources
             WHERE schedule_id = ? AND source_identity = ?",
        )
        .bind(reserved.id)
        .bind("image:image-post:digest:0")
        .fetch_one(store.pool())
        .await
        .expect("image source");
        assert_eq!(image_source_count, 1);
    }

    #[test]
    fn cancellation_protects_manual_and_newer_republished_schedules() {
        let now = Utc::now();
        let mut schedule = Schedule {
            id: 1,
            streamer_id: 1,
            streamer_name: "甲".to_owned(),
            streamer_avatar: String::new(),
            participants: Vec::new(),
            title: "突击".to_owned(),
            start_at: now + Duration::days(1),
            end_at: None,
            room_url: None,
            room_owner_mid: None,
            room_owner_name: None,
            source_kind: "text".to_owned(),
            source_post_id: Some("old-post".to_owned()),
            source_image_url: None,
            source_author_mid: Some("1".to_owned()),
            source_fingerprint: Some("fingerprint".to_owned()),
            source_identity: Some("text:old-post:1".to_owned()),
            source_published_at: Some(now - Duration::minutes(1)),
            weekly_rule_id: None,
            weekly_occurrence_date: None,
            confidence: 0.95,
            manual_locked: false,
            source_changed: false,
            cancelled: false,
            revision: 1,
            created_at: now,
            updated_at: now,
        };
        assert!(cancellation_candidate_is_eligible(
            &schedule,
            "cancel-post",
            now
        ));
        schedule.source_published_at = Some(now + Duration::minutes(1));
        assert!(!cancellation_candidate_is_eligible(
            &schedule,
            "cancel-post",
            now
        ));
        schedule.source_published_at = Some(now);
        schedule.source_post_id = Some("cancel-post".to_owned());
        assert!(!cancellation_candidate_is_eligible(
            &schedule,
            "cancel-post",
            now
        ));
        schedule.source_post_id = Some("old-post".to_owned());
        schedule.manual_locked = true;
        assert!(!cancellation_candidate_is_eligible(
            &schedule,
            "cancel-post",
            now
        ));
    }

    #[tokio::test]
    async fn shared_official_image_calls_model_once() {
        let store = SqliteStore::in_memory().await.expect("db");
        let first = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("first");
        let second = store
            .add_subscription(&subscription("200", "乙"))
            .await
            .expect("second");
        store
            .add_linked_account(first.id, "900", "官方周表", "")
            .await
            .expect("link first");
        store
            .add_linked_account(second.id, "900", "官方周表", "")
            .await
            .expect("link second");
        let targets = store.list_streamers().await.expect("targets");
        let now = Utc::now();
        let events = vec![
            LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "甲".to_owned(),
                aliases: vec![],
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "甲直播".to_owned(),
                start_at: Some(now + Duration::days(1)),
                end_at: None,
                confidence: 0.95,
            },
            LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "乙".to_owned(),
                aliases: vec![],
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "乙直播".to_owned(),
                start_at: Some(now + Duration::days(1) + Duration::hours(1)),
                end_at: None,
                confidence: 0.95,
            },
        ];
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events,
            text_events: Vec::new(),
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = DynamicPost {
            id: "post-shared".to_owned(),
            author_mid: "900".to_owned(),
            author_name: "官方周表".to_owned(),
            published_at: now,
            text: "本周日程".to_owned(),
            images: vec!["https://i0.hdslb.com/bfs/test.jpg".to_owned()],
            source_url: "https://t.bilibili.com/post-shared".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        let settings = AppSettings {
            llm_model: "mock-vision".to_owned(),
            ..Default::default()
        };
        let pool = test_pool("mock-vision");
        let stats = service
            .process_source_posts(
                "900",
                &targets,
                std::slice::from_ref(&post),
                &settings,
                Some(&pool),
            )
            .await
            .expect("process");
        // 正文与图片在同一次请求里送出：一条动态只调一次模型，llm_calls 也只加一。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.llm_calls, 1);
        assert_eq!(stats.new_schedules, 2);
        let schedules = store
            .list_schedules(now - Duration::hours(1), now + Duration::days(3), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 2);
        let raw_event: Value = serde_json::json!({"ok": true});
        assert!(raw_event.is_object());
    }

    #[tokio::test]
    async fn official_text_post_splits_multiple_performers() {
        let store = SqliteStore::in_memory().await.expect("db");
        let first = store
            .add_subscription(&subscription("100", "甲酱"))
            .await
            .expect("first");
        let second = store
            .add_subscription(&subscription("200", "乙酱"))
            .await
            .expect("second");
        store
            .add_linked_account(first.id, "900", "官方周表", "")
            .await
            .expect("link first");
        store
            .add_linked_account(second.id, "900", "官方周表", "")
            .await
            .expect("link second");
        let targets = store.list_streamers().await.expect("targets");
        let now = Utc::now();
        let text_events = vec![
            LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "甲酱".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "杂谈".to_owned(),
                start_at: Some(now + Duration::days(1)),
                end_at: None,
                confidence: 0.95,
            },
            LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "乙酱".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "游戏".to_owned(),
                start_at: Some(now + Duration::days(2)),
                end_at: None,
                confidence: 0.95,
            },
        ];
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events,
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let offset = FixedOffset::east_opt(8 * 3600).expect("offset");
        let today = now.with_timezone(&offset).date_naive();
        let text = format!(
            "本周日程\n{}\n甲酱 20:00 直播杂谈\n{}\n乙酱 21:00 直播游戏",
            (today + Duration::days(1)).format("%m月%d日"),
            (today + Duration::days(2)).format("%m月%d日")
        );
        let post = DynamicPost {
            id: "post-official-text".to_owned(),
            author_mid: "900".to_owned(),
            author_name: "官方周表".to_owned(),
            published_at: now,
            text,
            images: Vec::new(),
            source_url: "https://t.bilibili.com/post-official-text".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: None,
        };
        let settings = AppSettings {
            llm_model: "mock-text".to_owned(),
            ..Default::default()
        };
        let pool = test_pool("mock-text");
        let stats = service
            .process_source_posts(
                "900",
                &targets,
                std::slice::from_ref(&post),
                &settings,
                Some(&pool),
            )
            .await
            .expect("process");
        assert_eq!(stats.new_schedules, 2);
        assert_eq!(analyzer.call_count(), 1);
        let schedules = store
            .list_schedules(now - Duration::hours(1), now + Duration::days(4), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 2);
        assert_ne!(schedules[0].streamer_id, schedules[1].streamer_id);
        assert!(schedules
            .iter()
            .any(|schedule| schedule.streamer_name == "甲酱"));
        assert!(schedules
            .iter()
            .any(|schedule| schedule.streamer_name == "乙酱"));
        assert!(schedules.iter().any(|schedule| schedule.title == "杂谈"));
        assert!(schedules.iter().any(|schedule| schedule.title == "游戏"));
        // 第二轮命中 post_analyses：同一条动态、同一个模型与提示词版本，
        // 缓存键（含 content_digest）完全一致，不再发请求。
        let second_stats = service
            .process_source_posts(
                "900",
                &targets,
                std::slice::from_ref(&post),
                &settings,
                Some(&pool),
            )
            .await
            .expect("cached process");
        assert_eq!(second_stats.llm_calls, 0);
        assert!(second_stats.llm_cache_hits >= 1);
    }

    #[tokio::test]
    async fn schedules_within_ten_minutes_merge_sources() {
        let store = SqliteStore::in_memory().await.expect("db");
        store
            .add_subscription(&subscription("300", "丙"))
            .await
            .expect("streamer");
        let targets = store.list_streamers().await.expect("targets");
        let target = targets.first().expect("target");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let start = now + Duration::days(1);
        let post_a = DynamicPost {
            id: "post-a".to_owned(),
            author_mid: "300".to_owned(),
            author_name: "丙".to_owned(),
            published_at: now,
            text: "10月30日 20:00 直播".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/post-a".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: None,
        };
        let post_b = DynamicPost {
            id: "post-b".to_owned(),
            published_at: now + Duration::minutes(1),
            ..post_a.clone()
        };
        let schedule_a = ParsedSchedule {
            title: "周五杂谈".to_owned(),
            start_at: start,
            end_at: None,
            confidence: 0.9,
        };
        let schedule_b = ParsedSchedule {
            title: "周五杂谈回".to_owned(),
            start_at: start + Duration::minutes(5),
            end_at: None,
            confidence: 0.9,
        };
        assert!(service
            .upsert_candidate(
                target,
                &schedule_a,
                "text",
                &post_a,
                "",
                "text:post-a",
                "fp-a",
                None,
                None,
                None,
                &[],
            )
            .await
            .expect("upsert candidate"));
        assert!(!service
            .upsert_candidate(
                target,
                &schedule_b,
                "text",
                &post_b,
                "",
                "text:post-b",
                "fp-b",
                None,
                None,
                None,
                &[],
            )
            .await
            .expect("upsert candidate"));
        let schedules = store
            .list_schedules(now, now + Duration::days(3), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].title, "周五杂谈回");
    }

    #[test]
    fn title_reuse_requires_same_weekday_and_close_start_time() {
        let china = FixedOffset::east_opt(8 * 3600).expect("offset");
        let base = china
            .with_ymd_and_hms(2026, 11, 5, 21, 0, 0)
            .single()
            .expect("date")
            .with_timezone(&Utc);
        assert!(same_recurring_slot(base + Duration::days(7), base));
        assert!(same_recurring_slot(
            base + Duration::days(7) + Duration::minutes(20),
            base
        ));
        assert!(!same_recurring_slot(
            base + Duration::days(7) + Duration::hours(2),
            base
        ));
        assert!(!same_recurring_slot(base + Duration::days(1), base));
        assert!(schedule_title_is_placeholder("（未提供主题）"));
        assert!(!schedule_title_is_placeholder("周五杂谈"));
    }

    #[tokio::test]
    async fn missing_title_reuses_previous_matching_schedule() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("500", "戊"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let target_start = now + Duration::days(9);
        let previous_start = target_start - Duration::days(7);
        let previous_post = DynamicPost {
            id: "previous-theme".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "下周同一时间".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/previous-theme".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: None,
        };
        let previous = ParsedSchedule {
            title: "固定歌回".to_owned(),
            start_at: previous_start,
            end_at: None,
            confidence: 0.9,
        };
        assert!(service
            .upsert_candidate(
                &streamer,
                &previous,
                "text",
                &previous_post,
                "",
                "text:previous-theme",
                "previous-theme-fingerprint",
                None,
                None,
                None,
                &[],
            )
            .await
            .expect("upsert candidate"));

        let current_post = DynamicPost {
            id: "missing-theme".to_owned(),
            published_at: now + Duration::minutes(1),
            ..previous_post.clone()
        };
        let current = ParsedSchedule {
            title: "未提供主题".to_owned(),
            start_at: target_start,
            end_at: None,
            confidence: 0.9,
        };
        assert!(service
            .upsert_candidate(
                &streamer,
                &current,
                "text",
                &current_post,
                "",
                "text:missing-theme",
                "missing-theme-fingerprint",
                None,
                None,
                None,
                &[],
            )
            .await
            .expect("upsert candidate"));

        let schedules = store.list_all_schedules().await.expect("schedules");
        let reused = schedules
            .iter()
            .find(|schedule| schedule.start_at == target_start)
            .expect("reused schedule");
        assert_eq!(reused.title, "固定歌回");
    }

    #[tokio::test]
    async fn missing_title_reuses_previous_matching_live_session() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("600", "己"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let target_start = now + Duration::days(2);
        let previous_start = target_start - Duration::days(7);
        store
            .ensure_live_session(
                streamer.id,
                "600",
                &streamer.name,
                "上周歌回",
                None,
                previous_start,
                now,
            )
            .await
            .expect("live session");

        let post = DynamicPost {
            id: "live-session-theme".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "21:00 直播".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/live-session-theme".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: None,
        };
        let schedule = ParsedSchedule {
            title: "直播".to_owned(),
            start_at: target_start,
            end_at: None,
            confidence: 0.9,
        };
        assert!(service
            .upsert_candidate(
                &streamer,
                &schedule,
                "text",
                &post,
                "",
                "text:live-session-theme",
                "live-session-theme-fingerprint",
                None,
                None,
                None,
                &[],
            )
            .await
            .expect("upsert candidate"));

        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].title, "上周歌回");
    }

    #[tokio::test]
    async fn video_reservation_is_skipped_and_removes_stale_reserve_schedule() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let post = DynamicPost {
            id: "video-reserve-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "视频更新预告".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/video-reserve-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: Some(ReserveCard {
                title: "视频预约：雨爱".to_owned(),
                description: "预计明天 17:20发布".to_owned(),
                room_id: Some("5848306".to_owned()),
                jump_url: Some("https://www.bilibili.com/video/BV1dBxA6PEcw/".to_owned()),
                stype: Some(1),
            }),
        };
        let stale_schedule = ParsedSchedule {
            title: post.reserve.as_ref().expect("reserve").title.clone(),
            start_at: now + Duration::days(1) + Duration::hours(9),
            end_at: None,
            confidence: 1.0,
        };
        assert!(service
            .upsert_candidate(
                &streamer,
                &stale_schedule,
                "reserve",
                &post,
                "editor",
                "reserve:video-reserve-post",
                "stale-video-fingerprint",
                None,
                None,
                None,
                &[],
            )
            .await
            .expect("upsert candidate"));

        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                None,
            )
            .await
            .expect("process");

        assert_eq!(stats.new_schedules, 0);
        assert_eq!(stats.cancelled_schedules, 1);
        assert!(store
            .list_all_schedules()
            .await
            .expect("schedules")
            .is_empty());
    }

    #[tokio::test]
    async fn unrelated_solo_event_does_not_fall_back_to_the_post_author() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("3537115310721781", "思诺snow"))
            .await
            .expect("streamer");
        let event = LlmEvent {
            action: LlmEventAction::Schedule,
            performer: "心宜".to_owned(),
            aliases: Vec::new(),
            participants: vec!["心宜".to_owned()],
            room_url: None,
            room_owner: None,
            room_owner_mid: None,
            title: "2D鸣潮 2.6下".to_owned(),
            start_at: Some(Utc::now() + Duration::hours(2)),
            end_at: None,
            confidence: 0.78,
        };
        let matched = match_event_target_for_post(
            &event,
            std::slice::from_ref(&streamer),
            "3537115310721781",
        );
        assert!(
            matched.is_none(),
            "unrelated performer must not be credited to the author"
        );
    }

    #[tokio::test]
    async fn explicit_solo_participant_overrides_author_performer_and_aliases() {
        let store = SqliteStore::in_memory().await.expect("db");
        let author = store
            .add_subscription(&subscription("9501", "栞栞Shiori"))
            .await
            .expect("author");
        let guest = store
            .add_subscription(&subscription("9502", "罗己sui"))
            .await
            .expect("guest");
        let mut event = schedule_event(
            "新衣回",
            Some(Utc::now() + Duration::hours(2)),
            &author.name,
        );
        event.aliases = vec![author.name.clone()];
        event.participants = vec![guest.name.clone()];
        let author_only = std::slice::from_ref(&author);
        assert!(match_event_target_for_post(&event, author_only, &author.mid).is_none());
        let targets = [author.clone(), guest.clone()];
        let canonical = canonicalize_event_participants(&event, &targets);
        let matched =
            match_event_target_for_post(&canonical, &targets, &author.mid).expect("guest");
        assert_eq!(matched.id, guest.id);
        event.participants = vec![author.name.clone(), guest.name.clone()];
        assert!(match_event_target_for_post(&event, author_only, &author.mid).is_some());
    }

    #[tokio::test]
    async fn reposted_solo_schedule_requires_author_participation_before_writing() {
        for participates in [false, true] {
            let store = SqliteStore::in_memory().await.expect("db");
            let author = store
                .add_subscription(&subscription("9501", "栞栞Shiori"))
                .await
                .expect("author");
            let now = Utc::now();
            let mut event = schedule_event("新衣回", Some(now + Duration::hours(2)), "罗己sui");
            event.room_url = Some("https://live.bilibili.com/9502".to_owned());
            event.room_owner = Some("罗己sui".to_owned());
            event.room_owner_mid = Some("9502".to_owned());
            if participates {
                event.participants.push(author.name.clone());
            }
            let analyzer = Arc::new(CountingAnalyzer::new(vec![event], Vec::new()));
            let service = AppService::with_analyzer(
                store.clone(),
                RuntimeSecrets::default(),
                analyzer.clone(),
            )
            .expect("service");
            let post = dynamic_post(
                "reposted-solo",
                &author.mid,
                &author.name,
                now,
                if participates {
                    "转发附言：我也参加新衣回联动"
                } else {
                    "转发附言：支持一下新衣回"
                },
                Vec::new(),
            );
            // 第二轮复用模型缓存，也必须走相同的参与者归属校验。
            for _ in 0..2 {
                service
                    .process_source_posts(
                        &author.mid,
                        std::slice::from_ref(&author),
                        std::slice::from_ref(&post),
                        &AppSettings::default(),
                        Some(&test_pool("fixture")),
                    )
                    .await
                    .expect("process");
                let schedules = store.list_all_schedules().await.expect("schedules");
                if participates {
                    assert_eq!(schedules.len(), 1);
                    assert_eq!(schedules[0].streamer_id, author.id);
                    assert_eq!(schedules[0].room_owner_name.as_deref(), Some("罗己sui"));
                    assert!(schedules[0].participants.contains(&author.name));
                } else {
                    assert!(
                        schedules.is_empty(),
                        "a pure repost must not create a schedule"
                    );
                    assert!(source_identities(&store, &post.id).await.is_empty());
                }
            }
            assert_eq!(
                analyzer.call_count(),
                1,
                "second refresh must use the cache"
            );
        }
    }

    #[tokio::test]
    async fn reserved_card_event_is_credited_to_the_post_author() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("3537115310721781", "思诺snow"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let reserve = ReserveCard {
            title: "直播预约：【2D鸣潮】2.6下！".to_owned(),
            description: "明天 19:00 直播".to_owned(),
            room_id: None,
            jump_url: None,
            stype: Some(2),
        };
        let post = DynamicPost {
            id: "sinuo-card-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "继续2.6下哦！".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/sinuo-card-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: Some(reserve.clone()),
        };
        let card_start = parse_reserve_schedule(&reserve, post.published_at, now)
            .expect("card schedule")
            .start_at;
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: vec![LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "心宜".to_owned(),
                aliases: Vec::new(),
                participants: vec!["心宜".to_owned()],
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "2D鸣潮 2.6下".to_owned(),
                start_at: Some(card_start),
                end_at: None,
                confidence: 0.78,
            }],
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.new_schedules, 1);
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].streamer_id, streamer.id);
        assert_eq!(schedules[0].title, "2D鸣潮 2.6下");
        assert_eq!(schedules[0].start_at, card_start);
        assert_eq!(schedules[0].source_kind, "text");
    }

    #[tokio::test]
    async fn reserved_dynamics_analyze_card_with_the_text_model() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let post = DynamicPost {
            id: "reserved-text-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "明晚电台见".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/reserved-text-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：【突击】电台电台！".to_owned(),
                description: "明天 19:00 直播".to_owned(),
                room_id: Some("22637261".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let card_start = parse_reserve_schedule(
            post.reserve.as_ref().expect("reserve"),
            post.published_at,
            now,
        )
        .expect("reservation schedule")
        .start_at;
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: vec![LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "甲".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "电台".to_owned(),
                start_at: Some(card_start),
                end_at: None,
                confidence: 0.95,
            }],
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 带预约卡片的动态也要跑文字模型，卡片那一场由模型给出。
        assert_eq!(stats.llm_calls, 1);
        assert_eq!(analyzer.call_count(), 1);
        let schedules = store
            .list_schedules(now - Duration::hours(1), now + Duration::days(3), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].source_kind, "text");
        assert_eq!(schedules[0].start_at, card_start);
    }
    #[tokio::test]
    async fn reserved_card_falls_back_to_regex_without_llm() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let post = DynamicPost {
            id: "reserve-fallback-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "明晚电台见".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/reserve-fallback-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：【突击】电台电台！".to_owned(),
                description: "明天 19:00 直播".to_owned(),
                room_id: Some("22637261".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let card_start = parse_reserve_schedule(
            post.reserve.as_ref().expect("reserve"),
            post.published_at,
            now,
        )
        .expect("reservation schedule")
        .start_at;
        let service = AppService::with_analyzer(
            store.clone(),
            RuntimeSecrets::default(),
            Arc::new(CountingAnalyzer {
                calls: AtomicUsize::new(0),
                captured: std::sync::Mutex::new(Vec::new()),
                events: Vec::new(),
                text_events: Vec::new(),
            }),
        )
        .expect("service");
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                None,
            )
            .await
            .expect("process");

        // 没有模型可用时，卡片正则兜底仍然要落一条预约日程。
        assert_eq!(stats.llm_calls, 0);
        assert_eq!(stats.new_schedules, 1);
        let schedules = store
            .list_schedules(now - Duration::hours(1), now + Duration::days(3), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].source_kind, "reserve");
        assert_eq!(schedules[0].start_at, card_start);
        assert_eq!(schedules[0].title, "【突击】电台电台！");
    }

    #[tokio::test]
    async fn reserved_card_falls_back_when_model_returns_nothing() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let post = DynamicPost {
            id: "reserve-empty-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "明晚电台见".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/reserve-empty-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：【突击】电台电台！".to_owned(),
                description: "明天 19:00 直播".to_owned(),
                room_id: Some("22637261".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let card_start = parse_reserve_schedule(
            post.reserve.as_ref().expect("reserve"),
            post.published_at,
            now,
        )
        .expect("reservation schedule")
        .start_at;
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.new_schedules, 1);
        let schedules = store
            .list_schedules(now - Duration::hours(1), now + Duration::days(3), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].source_kind, "reserve");
        assert_eq!(schedules[0].start_at, card_start);
    }

    #[tokio::test]
    async fn reserved_card_is_added_when_model_misses_the_card_slot() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let post = DynamicPost {
            id: "reserve-miss-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "明晚电台，后天杂谈".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/reserve-miss-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：【突击】电台电台！".to_owned(),
                description: "明天 19:00 直播".to_owned(),
                room_id: Some("22637261".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let card_start = parse_reserve_schedule(
            post.reserve.as_ref().expect("reserve"),
            post.published_at,
            now,
        )
        .expect("reservation schedule")
        .start_at;
        let other_start = now + Duration::days(2);
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: vec![LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "甲".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "杂谈".to_owned(),
                start_at: Some(other_start),
                end_at: None,
                confidence: 0.95,
            }],
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 模型只给了正文那一场，卡片场次由正则补进来。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.new_schedules, 2);
        let schedules = store
            .list_schedules(now - Duration::hours(1), now + Duration::days(3), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 2);
        assert!(schedules.iter().any(|schedule| {
            schedule.source_kind == "reserve" && schedule.start_at == card_start
        }));
        assert!(schedules
            .iter()
            .any(|schedule| schedule.source_kind == "text"
                && schedule.title == "杂谈"
                && schedule.start_at == other_start));
    }

    #[tokio::test]
    async fn reserved_llm_event_updates_the_card_slot() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let post = DynamicPost {
            id: "reserve-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "明晚电台见".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/reserve-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：【突击】电台电台！".to_owned(),
                description: "明天 19:00 直播".to_owned(),
                room_id: Some("22637261".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let reservation = parse_reserve_schedule(
            post.reserve.as_ref().expect("reserve"),
            post.published_at,
            now,
        )
        .expect("reservation schedule");
        assert!(service
            .upsert_candidate(
                &streamer,
                &reservation,
                "reserve",
                &post,
                "editor",
                "reserve:reserve-post",
                "reserve-fingerprint",
                None,
                None,
                None,
                &[],
            )
            .await
            .expect("upsert candidate"));

        let llm_schedule = ParsedSchedule {
            title: "电台".to_owned(),
            // 模型给的时间比卡片晚 5 分钟：同槽内以模型为准，标题仍归卡片。
            start_at: reservation.start_at + Duration::minutes(5),
            end_at: None,
            confidence: 0.95,
        };
        assert!(!service
            .upsert_candidate(
                &streamer,
                &llm_schedule,
                "text",
                &post,
                "editor",
                "text:reserve-post:0",
                "text-fingerprint",
                None,
                None,
                None,
                &[],
            )
            .await
            .expect("upsert candidate"));

        let schedules = store
            .list_schedules(now, now + Duration::days(2), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 1);
        // 同一场只留一条：时间换成模型给的，标题与来源保持卡片。
        assert_eq!(schedules[0].title, "【突击】电台电台！");
        assert_eq!(schedules[0].source_kind, "reserve");
        assert_eq!(
            schedules[0].start_at,
            reservation.start_at + Duration::minutes(5)
        );
    }

    #[tokio::test]
    async fn reserved_post_schedules_both_model_events() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let post = DynamicPost {
            id: "reserved-extra-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "明天6:00来看IG VS geng四强战！\n从现在开始做吧😈\n*后天20:00来杂谈🥳".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/reserved-extra-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：小鼠,做法中: IG给我赢啊啊啊啊".to_owned(),
                description: "明天 06:00 直播".to_owned(),
                room_id: Some("22637261".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let reservation = parse_reserve_schedule(
            post.reserve.as_ref().expect("reserve"),
            post.published_at,
            now,
        )
        .expect("reservation schedule");
        let extra_start = now + Duration::days(3);
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: vec![
                LlmEvent {
                    action: LlmEventAction::Schedule,
                    performer: "甲".to_owned(),
                    aliases: Vec::new(),
                    participants: Vec::new(),
                    room_url: None,
                    room_owner: None,
                    room_owner_mid: None,
                    title: "IG四强战转播".to_owned(),
                    start_at: Some(reservation.start_at),
                    end_at: None,
                    confidence: 0.95,
                },
                LlmEvent {
                    action: LlmEventAction::Schedule,
                    performer: "甲".to_owned(),
                    aliases: Vec::new(),
                    participants: Vec::new(),
                    room_url: None,
                    room_owner: None,
                    room_owner_mid: None,
                    title: "杂谈".to_owned(),
                    start_at: Some(extra_start),
                    end_at: None,
                    confidence: 0.95,
                },
            ],
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 卡片那一场和正文第二场都由模型给出：同槽事件不再丢弃，也不补正则兜底。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.new_schedules, 2);
        let schedules = store
            .list_schedules(now - Duration::hours(1), now + Duration::days(4), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 2);
        assert!(schedules
            .iter()
            .all(|schedule| schedule.source_kind == "text"));
        assert!(schedules
            .iter()
            .any(|schedule| schedule.title == "IG四强战转播"
                && schedule.start_at == reservation.start_at));
        assert!(schedules
            .iter()
            .any(|schedule| schedule.title == "杂谈" && schedule.start_at == extra_start));
    }

    #[tokio::test]
    async fn video_reserved_post_still_analyzes_live_text() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let post = DynamicPost {
            id: "video-reserve-text-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "明天20:00来杂谈".to_owned(),
            images: Vec::new(),
            source_url: "https://t.bilibili.com/video-reserve-text-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_WORD".to_owned(),
            reserve: Some(ReserveCard {
                title: "视频预约：雨爱".to_owned(),
                description: "预计明天 17:20发布".to_owned(),
                room_id: Some("5848306".to_owned()),
                jump_url: Some("https://www.bilibili.com/video/BV1dBxA6PEcw/".to_owned()),
                stype: Some(1),
            }),
        };
        let start_at = now + Duration::days(1) + Duration::hours(2);
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: vec![LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "甲".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "杂谈".to_owned(),
                start_at: Some(start_at),
                end_at: None,
                confidence: 0.95,
            }],
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 视频预约不进直播日程，正文里的直播时间必须照常识别。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.new_schedules, 1);
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].source_kind, "text");
        assert_eq!(schedules[0].title, "杂谈");
        assert_eq!(schedules[0].start_at, start_at);
    }

    #[tokio::test]
    async fn weekly_rules_materialize_idempotently_and_respect_skipped_occurrence() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer)
            .expect("service");
        let now = Utc::now();
        let start_date = crate::weekly::china_date(now) + Duration::days(1);
        let rule = service
            .create_weekly_rule(&NewWeeklyScheduleRule {
                rule_key: Some("weekly-test-rule".to_owned()),
                streamer_id: streamer.id,
                title: "固定杂谈".to_owned(),
                weekdays_mask: 0b0111_1111,
                start_time: "20:00".to_owned(),
                end_time: Some("21:00".to_owned()),
                timezone: "Asia/Shanghai".to_owned(),
                room_url: streamer.live_url.clone(),
                effective_from: start_date.format("%Y-%m-%d").to_string(),
                effective_until: None,
                enabled: true,
            })
            .await
            .expect("weekly rule");
        let from = now;
        let to = now + Duration::days(8);
        let schedules = service.list_schedules(from, to).await.expect("schedules");
        let expected_count = schedules.len();
        assert!(expected_count >= 7);
        assert!(schedules
            .iter()
            .all(|schedule| schedule.source_kind == "weekly"));
        assert!(schedules
            .iter()
            .all(|schedule| schedule.weekly_rule_id == Some(rule.id)));

        service
            .list_schedules(from, to)
            .await
            .expect("second materialization");
        assert_eq!(
            service
                .list_schedules(from, to)
                .await
                .expect("stable schedules")
                .len(),
            expected_count
        );

        let skipped = schedules[0].id;
        service
            .delete_schedule(skipped)
            .await
            .expect("skip occurrence");
        let after_skip = service.list_schedules(from, to).await.expect("after skip");
        assert_eq!(after_skip.len(), expected_count - 1);
        assert!(after_skip.iter().all(|schedule| schedule.id != skipped));

        service
            .update_weekly_rule(
                rule.id,
                &UpdateWeeklyScheduleRule {
                    title: Some("新的固定主题".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .expect("update rule");
        let updated = service
            .list_schedules(from, to)
            .await
            .expect("updated schedules");
        assert_eq!(updated.len(), expected_count - 1);
        assert!(updated
            .iter()
            .all(|schedule| schedule.title == "新的固定主题"));
    }

    /// 回归：明前奶绿动态 1268658714237206552（“八点来啵一下电台噢…八点见”）
    /// 同时带一张表情包。图片模型对这类图返回空数组，旧逻辑把带图动态
    /// 整条短路出文字通道，正文里的时间点因此丢失。
    #[tokio::test]
    async fn image_dynamic_still_runs_the_text_channel() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("2132180406", "明前奶绿"))
            .await
            .expect("streamer");
        let china = FixedOffset::east_opt(8 * 3600).expect("offset");
        let now = Utc::now();
        let start_at = (now.with_timezone(&china).date_naive() + Duration::days(1))
            .and_hms_opt(20, 0, 0)
            .expect("eight pm")
            .and_local_timezone(china)
            .single()
            .expect("local eight pm")
            .with_timezone(&Utc);
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            // 图片模型照旧什么都不给，模型只从正文里读出“八点”。
            events: Vec::new(),
            text_events: vec![LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "明前奶绿".to_owned(),
                aliases: Vec::new(),
                participants: vec!["明前奶绿".to_owned()],
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "电台".to_owned(),
                start_at: Some(start_at),
                end_at: None,
                confidence: 0.92,
            }],
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = DynamicPost {
            id: "1268658714237206552".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "八点来啵一下电台噢  ᖰ⌯'▾'⌯ᖳ\n八点见八点见[怪怪小黄豆_祈祷]".to_owned(),
            images: vec![
                "https://i0.hdslb.com/bfs/new_dyn/a3e6bab19c88da4d855ab7ff6ea7fa182132180406.jpg"
                    .to_owned(),
            ],
            source_url: "https://t.bilibili.com/1268658714237206552".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "2132180406",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 合并成一次调用后，正文和图片一起送进模型：正文里的“八点见”
        // 这次真的到了模型手里，一条请求就把这场电台排进日程。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(
            stats.analysis_outcomes.get("post:1268658714237206552"),
            Some(&None)
        );
        assert_eq!(stats.new_schedules, 1);
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        // 带图动态的 identity 走 image: 前缀，来源类型也跟着变成 image。
        assert_eq!(schedules[0].source_kind, "image");
        assert_eq!(schedules[0].start_at, start_at);
        assert!(schedules[0].title.contains("电台"), "{schedules:?}");
    }

    /// 同一条动态里文字和图片都识别出同一场时只落一条。
    #[tokio::test]
    async fn image_event_duplicating_the_text_event_is_dropped() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("2132180406", "明前奶绿"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let start_at = now + Duration::days(1);
        let event = LlmEvent {
            action: LlmEventAction::Schedule,
            performer: "明前奶绿".to_owned(),
            aliases: Vec::new(),
            participants: vec!["明前奶绿".to_owned()],
            room_url: None,
            room_owner: None,
            room_owner_mid: None,
            title: "电台杂谈".to_owned(),
            start_at: Some(start_at),
            end_at: None,
            confidence: 0.95,
        };
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            captured: std::sync::Mutex::new(Vec::new()),
            // 图片通道在同一个时间槽给出同一场，只是标题措辞不同。
            events: vec![LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "明前奶绿".to_owned(),
                aliases: Vec::new(),
                participants: vec!["明前奶绿".to_owned()],
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "电台".to_owned(),
                start_at: Some(start_at),
                end_at: None,
                confidence: 0.9,
            }],
            text_events: vec![event],
        });
        let post = DynamicPost {
            id: "dedupe-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "明天晚上电台杂谈，八点见".to_owned(),
            images: vec!["https://i0.hdslb.com/bfs/new_dyn/dedupe.jpg".to_owned()],
            source_url: "https://t.bilibili.com/dedupe-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "2132180406",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 一次调用同时返回文字与图片两版描述：同槽等价事件只落一条，
        // 桩里正文事件排在前面，所以最终标题来自正文那版。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.new_schedules, 1);
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        // 文字通道先跑，落库的是它给出的标题。
        assert_eq!(schedules[0].title, "电台杂谈");
    }

    #[tokio::test]
    async fn historical_schedule_reanalysis_ignores_v2_cache_and_is_idempotent() {
        for images in [
            Vec::new(),
            vec!["https://i0.hdslb.com/bfs/new_dyn/history.png"],
        ] {
            let store = SqliteStore::in_memory().await.expect("db");
            let streamer = store
                .add_subscription(&subscription("1203217682", "泽音Melody"))
                .await
                .unwrap();
            let now = Utc::now();
            let start = now - Duration::days(7);
            let post = dynamic_post(
                "historical-post",
                &streamer.mid,
                &streamer.name,
                now - Duration::days(8),
                "周表",
                images,
            );
            let settings = AppSettings {
                llm_prompt_version: "custom-history".to_owned(),
                ..AppSettings::default()
            };
            let digest = content_digest_of(
                &text_digest(&text_analysis_context(&post)),
                None,
                &post.images,
            );
            let old_version = format!("{}-post-v2", settings.llm_prompt_version);
            store
                .save_post_analysis(
                    &format!("{}:{digest}:fixture-model:{old_version}", post.id),
                    &post.id,
                    &digest,
                    "fixture-model",
                    &old_version,
                    &json!([]),
                )
                .await
                .unwrap();
            let analyzer = Arc::new(CountingAnalyzer::new(
                vec![schedule_event("历史歌回", Some(start), &streamer.name)],
                Vec::new(),
            ));
            let service = AppService::with_analyzer(
                store.clone(),
                RuntimeSecrets::default(),
                analyzer.clone(),
            )
            .unwrap();
            let pool = test_pool("fixture-model");
            for expected_calls in [1, 0] {
                let stats = service
                    .process_source_posts(
                        &streamer.mid,
                        std::slice::from_ref(&streamer),
                        std::slice::from_ref(&post),
                        &settings,
                        Some(&pool),
                    )
                    .await
                    .unwrap();
                assert_eq!(stats.llm_calls, expected_calls);
                assert_eq!(stats.llm_cache_hits, 1 - expected_calls);
                let schedules = store.list_all_schedules().await.unwrap();
                assert_eq!(schedules.len(), 1);
                assert_eq!(schedules[0].start_at.timestamp(), start.timestamp());
                assert_eq!(schedules[0].source_published_at, Some(post.published_at));
            }
            assert_eq!(analyzer.call_count(), 1);
        }
    }

    #[tokio::test]
    async fn publication_week_revision_ignores_old_post_cache_in_both_channels() {
        for images in [
            Vec::new(),
            vec!["https://i0.hdslb.com/bfs/new_dyn/week.png"],
        ] {
            let store = SqliteStore::in_memory().await.expect("db");
            let streamer = store
                .add_subscription(&subscription("1203217682", "泽音Melody"))
                .await
                .expect("streamer");
            let now = Utc::now();
            let post = dynamic_post(
                "old-weekly-post",
                &streamer.mid,
                &streamer.name,
                now - Duration::days(8),
                "呵呵哈哈是周表诶……",
                images,
            );
            // 自定义提示词版本也必须失效，不只针对默认 schedule-v1。
            let settings = AppSettings {
                llm_prompt_version: "custom-schedule-v7".to_owned(),
                ..AppSettings::default()
            };
            let text_hash = text_digest(&text_analysis_context(&post));
            let content_digest = content_digest_of(&text_hash, None, &post.images);
            let old_version = format!("{}-post-v1", settings.llm_prompt_version);
            let old_key = format!("{}:{content_digest}:fixture-model:{old_version}", post.id);
            let wrong_events = vec![schedule_event(
                "不应复用的本周日程",
                Some(now + Duration::days(2)),
                &streamer.name,
            )];
            store
                .save_post_analysis(
                    &old_key,
                    &post.id,
                    &content_digest,
                    "fixture-model",
                    &old_version,
                    &events_to_model_json(&wrong_events),
                )
                .await
                .expect("seed old cache");
            let analyzer = Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new()));
            let service = AppService::with_analyzer(
                store.clone(),
                RuntimeSecrets::default(),
                analyzer.clone(),
            )
            .expect("service");
            let pool = test_pool("fixture-model");
            for expected_calls in [1, 0] {
                let stats = service
                    .process_source_posts(
                        &streamer.mid,
                        std::slice::from_ref(&streamer),
                        std::slice::from_ref(&post),
                        &settings,
                        Some(&pool),
                    )
                    .await
                    .expect("process old weekly post");
                assert_eq!(stats.llm_calls, expected_calls);
                assert_eq!(stats.llm_cache_hits, 1 - expected_calls);
                assert_eq!(stats.new_schedules, 0);
                assert!(store
                    .list_all_schedules()
                    .await
                    .expect("schedules")
                    .is_empty());
            }
            assert_eq!(analyzer.call_count(), 1);
            assert_eq!(analyzer.last_call().input.published_at, post.published_at);
            let new_key = format!(
                "{}:{content_digest}:fixture-model:{}-post-v5",
                post.id, settings.llm_prompt_version
            );
            assert_eq!(
                store.load_post_analysis(&new_key).await.expect("new cache"),
                Some(serde_json::json!([]))
            );
        }
    }

    /// 空结果从不写缓存；万一库里留着旧的空记录，也要当没分析过重跑一次。
    ///
    /// 旧用例校验的是 `image_analyses` 里的空数组（那时一条动态会逐图缓存）。
    /// 合并成单次调用后缓存只剩 `post_analyses` 一层：空结果进缓存，但只在
    /// 重试窗口内算命中，过期后必须重新问模型。
    #[tokio::test]
    async fn empty_cached_post_analysis_is_retried() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("2132180406", "明前奶绿"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let post = DynamicPost {
            id: "cached-empty-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "八点见".to_owned(),
            images: vec!["https://i0.hdslb.com/bfs/new_dyn/cached.jpg".to_owned()],
            source_url: "https://t.bilibili.com/cached-empty-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        let settings = AppSettings::default();
        let model = "fixture-model";
        let pool = test_pool(model);
        // 手工塞一条空事件的缓存记录，键与流水线里算出来的完全一致。
        let text_hash = text_digest(&text_analysis_context(&post));
        let reserve = reserve_summary(&post);
        let content_digest = content_digest_of(&text_hash, reserve.as_deref(), &post.images);
        let prompt_version = format!("{}-post-v5", settings.llm_prompt_version);
        let cache_key = format!("{}:{content_digest}:{model}:{prompt_version}", post.id);
        store
            .save_post_analysis(
                &cache_key,
                &post.id,
                &content_digest,
                model,
                &prompt_version,
                &serde_json::json!([]),
            )
            .await
            .expect("seed empty analysis");
        // 把这条空结果改成过期，模拟“窗口已过”。
        let stale = (Utc::now() - Duration::hours(EMPTY_ANALYSIS_RETRY_HOURS + 1)).to_rfc3339();
        sqlx::query("UPDATE post_analyses SET created_at = ? WHERE cache_key = ?")
            .bind(&stale)
            .bind(&cache_key)
            .execute(store.pool())
            .await
            .expect("age the cache row");

        let analyzer = Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new()));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let stats = service
            .process_source_posts(
                "2132180406",
                std::slice::from_ref(&streamer),
                &[post],
                &settings,
                Some(&pool),
            )
            .await
            .expect("process");

        // 过期的空结果不算缓存命中，真跑一次模型。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.llm_calls, 1);
        assert_eq!(stats.llm_cache_hits, 0);
    }

    /// 动图这类不支持的图片按“跳过”处理，不等于分析失败。
    #[tokio::test]
    async fn unsupported_image_skip_is_not_an_error() {
        struct SkipAllImages {
            calls: AtomicUsize,
        }

        #[async_trait]
        impl ContentAnalyzer for SkipAllImages {
            // 动图这类图片解码不出来时模型什么也读不到。单次调用没有“跳过”这种
            // 返回值，空事件集本身就等于“这条动态没有日程”，不是失败。
            async fn analyze_post(
                &self,
                _config: &LlmConfig,
                _input: &PostAnalysisInput,
            ) -> AppResult<LlmAnalysis> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(LlmAnalysis {
                    events: Vec::new(),
                    raw: serde_json::json!([]),
                    source_digest: "mock-post-digest".to_owned(),
                })
            }
        }

        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("2132180406", "明前奶绿"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(SkipAllImages {
            calls: AtomicUsize::new(0),
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = DynamicPost {
            id: "gif-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: Utc::now(),
            text: String::new(),
            images: vec!["https://i0.hdslb.com/bfs/new_dyn/skip.gif".to_owned()],
            source_url: "https://t.bilibili.com/gif-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "2132180406",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        assert_eq!(analyzer.calls.load(Ordering::SeqCst), 1);
        assert!(stats.analysis_errors.is_empty(), "{stats:?}");
        // 空事件是正常结果：记一条“已分析，无日程”，不写错误。
        assert_eq!(stats.analysis_outcomes.get("post:gif-post"), Some(&None));
    }

    #[test]
    fn model_event_dedupe_prefers_room_over_participant_spelling() {
        let time = Utc::now();
        let mut left = schedule_event("团播", Some(time), "贝拉");
        left.room_url = Some("https://live.bilibili.com/12345?from=post".into());
        let mut right = schedule_event("团播", Some(time), "贝拉kira");
        right.room_url = Some("https://live.bilibili.com/12345/".into());
        assert!(llm_events_equivalent(&left, &right));
        right.room_url = Some("https://live.bilibili.com/67890".into());
        right.participants = left.participants.clone();
        right.performer = left.performer.clone();
        assert!(!llm_events_equivalent(&left, &right));
    }

    #[test]
    fn llm_events_equivalent_matches_slot_and_participants() {
        let base = Utc::now() + Duration::days(2);
        let make = |title: &str, start_at: DateTime<Utc>, participants: Vec<String>| LlmEvent {
            action: LlmEventAction::Schedule,
            performer: participants.first().cloned().unwrap_or_default(),
            aliases: Vec::new(),
            participants,
            room_url: None,
            room_owner: None,
            room_owner_mid: None,
            title: title.to_owned(),
            start_at: Some(start_at),
            end_at: None,
            confidence: 0.9,
        };

        // 同一时间槽、同一批人，标题措辞不同也算同一场。
        assert!(llm_events_equivalent(
            &make("电台杂谈", base, vec!["明前奶绿".to_owned()]),
            &make(
                "电台",
                base + Duration::minutes(2),
                vec!["明前奶绿".to_owned()]
            ),
        ));
        // 时间槽差太多就是两场。
        assert!(!llm_events_equivalent(
            &make("电台", base, vec!["明前奶绿".to_owned()]),
            &make(
                "电台",
                base + Duration::hours(2),
                vec!["明前奶绿".to_owned()]
            ),
        ));
        // 人对不上就不是同一场。
        assert!(!llm_events_equivalent(
            &make("电台", base, vec!["明前奶绿".to_owned()]),
            &make("电台", base, vec!["呜米".to_owned()]),
        ));
        // 取消和新增永远不是同一件事。
        let mut cancel = make("电台", base, vec!["明前奶绿".to_owned()]);
        cancel.action = LlmEventAction::Cancel;
        assert!(!llm_events_equivalent(
            &make("电台", base, vec!["明前奶绿".to_owned()]),
            &cancel,
        ));
    }
    fn dynamic_post(
        id: &str,
        author_mid: &str,
        author_name: &str,
        published_at: DateTime<Utc>,
        text: &str,
        images: Vec<&str>,
    ) -> DynamicPost {
        let has_images = !images.is_empty();
        DynamicPost {
            id: id.to_owned(),
            author_mid: author_mid.to_owned(),
            author_name: author_name.to_owned(),
            published_at,
            text: text.to_owned(),
            images: images.into_iter().map(str::to_owned).collect(),
            source_url: format!("https://t.bilibili.com/{id}"),
            raw_kind: if has_images {
                "DYNAMIC_TYPE_DRAW"
            } else {
                "DYNAMIC_TYPE_WORD"
            }
            .to_owned(),
            reserve: None,
        }
    }

    fn schedule_event(title: &str, start_at: Option<DateTime<Utc>>, performer: &str) -> LlmEvent {
        LlmEvent {
            action: LlmEventAction::Schedule,
            performer: performer.to_owned(),
            aliases: Vec::new(),
            participants: vec![performer.to_owned()],
            room_url: None,
            room_owner: None,
            room_owner_mid: None,
            title: title.to_owned(),
            start_at,
            end_at: None,
            confidence: 0.95,
        }
    }

    async fn source_identities(store: &SqliteStore, post_id: &str) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT source_identity FROM schedule_sources WHERE source_post_id = ? ORDER BY id",
        )
        .bind(post_id)
        .fetch_all(store.pool())
        .await
        .expect("source identities")
    }

    #[tokio::test]
    async fn repost_attribution_reaches_analyzer_and_invalidates_cache_when_original_changes() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&subscription("9501", "转发者"))
            .await
            .unwrap();
        let analyzer = Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new()));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .unwrap();
        let mut post = dynamic_post("repost-context", &streamer.mid, &streamer.name, Utc::now(),
            "转发附言：支持一下\n原动态发起人：原主播（MID：200）\n原动态发布时间：2026-11-12 20:00:00（北京时间）\n原动态正文：明晚八点见", Vec::new());
        let settings = AppSettings::default();
        for changed in [false, true] {
            if changed {
                post.text = post.text.replace("MID：200", "MID：201");
            }
            service
                .process_source_posts(
                    &streamer.mid,
                    std::slice::from_ref(&streamer),
                    std::slice::from_ref(&post),
                    &settings,
                    Some(&test_pool("fixture")),
                )
                .await
                .unwrap();
            let call = analyzer.last_call();
            assert_eq!(call.input.author, "转发者");
            assert_eq!(call.input.text, post.text);
            assert!(call.input.text.contains("原动态发起人"));
        }
        assert_eq!(analyzer.call_count(), 2);
    }

    #[tokio::test]
    async fn service_sends_only_first_two_images_but_preserves_original_post() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&subscription("9501", "测试主播"))
            .await
            .unwrap();
        let analyzer = Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new()));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .unwrap();
        let images: Vec<String> = (0..9)
            .map(|i| format!("https://i0.hdslb.com/bfs/new_dyn/raw-{i}.jpg"))
            .collect();
        let post = DynamicPost {
            id: "two-image-cap".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: Utc::now(),
            text: "周表".to_owned(),
            images: images.clone(),
            source_url: "https://www.bilibili.com/opus/two-image-cap".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&test_pool("fixture")),
            )
            .await
            .unwrap();
        assert_eq!(analyzer.last_call().input.image_urls, images[..2]);
        let stored: String = sqlx::query_scalar("SELECT images_json FROM dynamic_posts WHERE id=?")
            .bind(&post.id)
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&stored).unwrap(),
            images
        );
        let settings = AppSettings::default();
        let mut changed = post.clone();
        changed.images[2] = "https://i0.hdslb.com/bfs/new_dyn/changed-ad.jpg".to_owned();
        let cached = service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&changed),
                &settings,
                Some(&test_pool("fixture")),
            )
            .await
            .unwrap();
        assert_eq!(
            cached.llm_calls, 0,
            "ignored tail changes must not invalidate analysis"
        );
        assert_eq!(cached.llm_cache_hits, 1);
        changed.images[0] = "https://i0.hdslb.com/bfs/new_dyn/changed-schedule.jpg".to_owned();
        let fresh = service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&changed),
                &settings,
                Some(&test_pool("fixture")),
            )
            .await
            .unwrap();
        assert_eq!(
            fresh.llm_calls, 1,
            "selected image changes must be analysed"
        );
        assert_eq!(analyzer.call_count(), 2);
    }

    /// 九图动态只选前两张，仍然只发一次模型请求。
    #[tokio::test]
    async fn nine_images_go_in_one_request() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("500", "戊"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let images: Vec<String> = (0..9)
            .map(|index| format!("https://i0.hdslb.com/bfs/new_dyn/nine-{index}.jpg"))
            .collect();
        let analyzer = Arc::new(CountingAnalyzer::new(
            Vec::new(),
            vec![schedule_event("杂谈", Some(now + Duration::days(1)), "戊")],
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = DynamicPost {
            id: "nine-image-post".to_owned(),
            author_mid: "500".to_owned(),
            author_name: "戊".to_owned(),
            published_at: now,
            text: "明天20:00杂谈，图都在这条动态里".to_owned(),
            images: images.clone(),
            source_url: "https://t.bilibili.com/nine-image-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: None,
        };
        let pool = test_pool("mock-vision");
        let stats = service
            .process_source_posts(
                "500",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 旧实现是「每张图一次 + 正文一次」= 10 次请求，现在只剩 1 次。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(analyzer.captured_calls().len(), 1);
        assert_eq!(stats.llm_calls, 1);
        assert_eq!(analyzer.last_call().input.image_urls, images[..2]);
        assert_eq!(stats.new_schedules, 1);
    }

    /// 单图动态同样只发一次请求。
    #[tokio::test]
    async fn one_image_goes_in_one_request() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("501", "己"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let analyzer = Arc::new(CountingAnalyzer::new(
            Vec::new(),
            vec![schedule_event("歌回", Some(now + Duration::days(1)), "己")],
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post(
            "one-image-post",
            "501",
            "己",
            now,
            "明晚八点歌回",
            vec!["https://i0.hdslb.com/bfs/new_dyn/one.jpg"],
        );
        let pool = test_pool("mock-vision");
        let stats = service
            .process_source_posts(
                "501",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.llm_calls, 1);
        assert_eq!(analyzer.last_call().input.image_urls.len(), 1);
        assert_eq!(stats.new_schedules, 1);
    }

    /// 纯文字动态不带任何图片片段，模型输入里 `image_urls` 为空。
    #[tokio::test]
    async fn text_only_post_has_no_image_parts() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("502", "庚"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let analyzer = Arc::new(CountingAnalyzer::new(
            Vec::new(),
            vec![schedule_event("杂谈", Some(now + Duration::days(1)), "庚")],
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post(
            "text-only-post",
            "502",
            "庚",
            now,
            "明天20:00杂谈",
            Vec::new(),
        );
        let pool = test_pool("fixture-model");
        service
            .process_source_posts(
                "502",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        let call = analyzer.last_call();
        assert!(call.input.image_urls.is_empty());
        assert_eq!(call.input.text, "明天20:00杂谈");
        // 纯文字动态的 identity 仍是 `text:{post_id}:{digest}:{idx}`。
        let identities = source_identities(&store, "text-only-post").await;
        assert_eq!(identities.len(), 1);
        assert!(
            identities[0].starts_with("text:text-only-post:"),
            "{identities:?}"
        );
    }

    /// 作者与发布时间随请求送出：模型要靠它们解析“明天”“下周”这类相对日期。
    #[tokio::test]
    async fn prompt_carries_author_and_publish_time() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("503", "辛"))
            .await
            .expect("streamer");
        let published_at = Utc::now() - Duration::hours(3);
        let analyzer = Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new()));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post(
            "author-post",
            "503",
            "  辛  ",
            published_at,
            "明天20:00杂谈",
            Vec::new(),
        );
        let pool = test_pool("fixture-model");
        service
            .process_source_posts(
                "503",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        let call = analyzer.last_call();
        assert_eq!(call.input.author, "辛");
        assert_eq!(call.input.published_at, published_at);
    }

    /// 取消事件与新增事件来自同一次调用，两者都要生效。
    #[tokio::test]
    async fn cancel_event_survives_single_call() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("504", "壬"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let cancelled_start = now + Duration::days(1);
        let (cancelled, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: streamer.id,
                participants: Vec::new(),
                title: "杂谈".to_owned(),
                start_at: cancelled_start,
                end_at: None,
                room_url: streamer.live_url.clone(),
                room_owner_mid: None,
                room_owner_name: None,
                source_kind: "text".to_owned(),
                source_post_id: Some("older-post".to_owned()),
                source_image_url: None,
                source_author_mid: Some("504".to_owned()),
                source_fingerprint: Some("older-fingerprint".to_owned()),
                source_identity: Some("text:older-post:1".to_owned()),
                source_published_at: None,
                confidence: 0.9,
            })
            .await
            .expect("schedule");
        let mut cancel = schedule_event("杂谈", Some(cancelled_start), "壬");
        cancel.action = LlmEventAction::Cancel;
        let analyzer = Arc::new(CountingAnalyzer::new(
            Vec::new(),
            vec![
                cancel,
                schedule_event("歌回", Some(now + Duration::days(2)), "壬"),
            ],
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post(
            "cancel-single-post",
            "504",
            "壬",
            now,
            "明天的杂谈取消，改成后天歌回",
            Vec::new(),
        );
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "504",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        // 一次调用同时给出取消和新日程：取消走延迟队列，新增立即落库。
        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.cancelled_schedules, 1);
        assert_eq!(stats.new_schedules, 1);
        assert!(store.get_schedule(cancelled.id).await.is_err());
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].title, "歌回");
    }

    /// 同一场直播被模型用不同标题说两遍，也只落一条日程。
    #[tokio::test]
    async fn varying_titles_merge_into_one_slot() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("505", "癸"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let start_at = now + Duration::days(1);
        let analyzer = Arc::new(CountingAnalyzer::new(
            Vec::new(),
            vec![
                schedule_event("电台", Some(start_at), "癸"),
                schedule_event("电台杂谈", Some(start_at), "癸"),
            ],
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post(
            "varying-title-post",
            "505",
            "癸",
            now,
            "明天20:00电台杂谈",
            Vec::new(),
        );
        let pool = test_pool("fixture-model");
        let stats = service
            .process_source_posts(
                "505",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        assert_eq!(analyzer.call_count(), 1);
        assert_eq!(stats.new_schedules, 1);
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        // 先出现的那条留在日程里，后一条按时间槽判为同一场。
        assert_eq!(schedules[0].title, "电台");
    }

    /// 纯文字动态的 identity 在重复分析后保持稳定，历史日程才能原地更新。
    #[tokio::test]
    async fn text_identity_is_stable_for_text_posts() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("506", "子"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let analyzer = Arc::new(CountingAnalyzer::new(
            Vec::new(),
            vec![schedule_event("杂谈", Some(now + Duration::days(1)), "子")],
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post(
            "stable-identity-post",
            "506",
            "子",
            now,
            "明天20:00杂谈",
            Vec::new(),
        );
        let pool = test_pool("fixture-model");
        service
            .process_source_posts(
                "506",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("first process");
        let first = source_identities(&store, "stable-identity-post").await;

        // 第二轮命中缓存，事件重新应用一次：identity 必须一字不差。
        let stats = service
            .process_source_posts(
                "506",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("cached process");
        let second = source_identities(&store, "stable-identity-post").await;

        assert_eq!(stats.llm_cache_hits, 1);
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert!(first[0].starts_with("text:stable-identity-post:"));
        assert!(first[0].ends_with(":0"), "{first:?}");
    }

    /// 同一动态的旧来源在分析成功后清掉：旧提示词、旧图集留下的 identity 不再挂着。
    #[tokio::test]
    async fn stale_identities_are_pruned() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("507", "丑"))
            .await
            .expect("streamer");
        let now = Utc::now();
        // 先造一条同 post 的历史日程来源，identity 形如旧版图片通道。
        let (stale, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: streamer.id,
                participants: Vec::new(),
                title: "旧日程".to_owned(),
                start_at: now + Duration::days(5),
                end_at: None,
                room_url: streamer.live_url.clone(),
                room_owner_mid: None,
                room_owner_name: None,
                source_kind: "image".to_owned(),
                source_post_id: Some("stale-post".to_owned()),
                source_image_url: Some("https://i0.hdslb.com/bfs/new_dyn/stale.jpg".to_owned()),
                source_author_mid: Some("507".to_owned()),
                source_fingerprint: Some("stale-fingerprint".to_owned()),
                source_identity: Some("image:stale-post:legacy-digest:0".to_owned()),
                source_published_at: None,
                confidence: 0.9,
            })
            .await
            .expect("stale schedule");
        let analyzer = Arc::new(CountingAnalyzer::new(
            Vec::new(),
            vec![schedule_event("杂谈", Some(now + Duration::days(1)), "丑")],
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post("stale-post", "507", "丑", now, "明天20:00杂谈", Vec::new());
        let pool = test_pool("fixture-model");
        service
            .process_source_posts(
                "507",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("process");

        let identities = source_identities(&store, "stale-post").await;
        assert_eq!(identities.len(), 1, "{identities:?}");
        assert!(identities[0].starts_with("text:stale-post:"));
        // 旧日程的来源被清空后没有剩下任何 source，孤儿场次一并删掉。
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1, "{schedules:?}");
        assert!(schedules.iter().all(|schedule| schedule.id != stale.id));
        assert_eq!(
            stale.source_identity.as_deref(),
            Some("image:stale-post:legacy-digest:0")
        );
    }

    /// 空结果也进缓存，只在过了重试窗口后才重新问模型。
    #[tokio::test]
    async fn empty_result_is_cached_and_retried_after_the_window() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("508", "寅"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let analyzer = Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new()));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post("empty-post", "508", "寅", now, "随手一发", Vec::new());
        let pool = test_pool("fixture-model");
        let first = service
            .process_source_posts(
                "508",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("first process");
        let second = service
            .process_source_posts(
                "508",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("second process");

        assert_eq!(first.llm_calls, 1);
        assert_eq!(second.llm_calls, 0, "空结果命中缓存，不该每轮都重问");
        assert_eq!(second.llm_cache_hits, 1);
        assert_eq!(analyzer.call_count(), 1);

        let settings = AppSettings::default();
        let text_hash = text_digest(&text_analysis_context(&post));
        let content_digest = content_digest_of(&text_hash, None, &post.images);
        let prompt_version = format!("{}-post-v5", settings.llm_prompt_version);
        let cache_key = format!(
            "{}:{content_digest}:fixture-model:{prompt_version}",
            post.id
        );
        assert!(store
            .load_post_analysis(&cache_key)
            .await
            .expect("cache lookup")
            .is_some());

        // 把写入时间改成过期，下一轮必须重新问模型。
        let stale = (Utc::now() - Duration::hours(EMPTY_ANALYSIS_RETRY_HOURS + 1)).to_rfc3339();
        sqlx::query("UPDATE post_analyses SET created_at = ? WHERE cache_key = ?")
            .bind(&stale)
            .bind(&cache_key)
            .execute(store.pool())
            .await
            .expect("age the cache row");
        let third = service
            .process_source_posts(
                "508",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("third process");
        assert_eq!(third.llm_calls, 1, "过了重试窗口要重新分析");
        assert_eq!(third.llm_cache_hits, 0);
        assert_eq!(analyzer.call_count(), 2);
    }

    /// 全量刷新也要有真实进度：合成条目让前端不再一直停在 0%。
    #[test]
    fn all_refresh_progress_uses_synthetic_target() {
        let reports = std::sync::Mutex::new(Vec::new());
        let target = ProgressTarget::all();
        emit_progress(
            &target,
            "拉取动态 3/16",
            2,
            16,
            20,
            false,
            false,
            &|progress| reports.lock().expect("progress").push(progress),
        );
        let reports = reports.lock().expect("reports");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].streamer_id, 0);
        assert_eq!(reports[0].streamer_name, "全部主播");
        assert_eq!(reports[0].stage, "拉取动态 3/16");
        assert_eq!(reports[0].percent, 20);
        assert!(!reports[0].done);
    }

    /// 手工刷新撞上后台轮询时要播报排队状态，界面不能停在「准备刷新 0%」。
    #[tokio::test]
    async fn waiting_for_the_refresh_lock_reports_progress() {
        let store = SqliteStore::in_memory().await.expect("db");
        let analyzer = Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new()));
        let service =
            AppService::with_analyzer(store, RuntimeSecrets::default(), analyzer).expect("service");
        let _held = service.refresh_lock.lock().await;
        let target = ProgressTarget::all();
        let reports: std::sync::Mutex<Vec<RefreshProgress>> = std::sync::Mutex::new(Vec::new());
        let waiting = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            service.wait_for_refresh_lock(&target, &|progress| {
                reports.lock().expect("reports").push(progress)
            }),
        )
        .await;
        assert!(waiting.is_err(), "锁被占着时不应拿到刷新锁");
        let reports = reports.lock().expect("reports");
        assert_eq!(reports[0].streamer_id, 0);
        assert!(
            reports[0].stage.contains("等待上一轮刷新结束"),
            "{:?}",
            reports[0].stage
        );
    }

    /// `ReasoningEffort::Auto` 表示不带该字段；显式指定时才随请求发送。
    #[tokio::test]
    async fn reasoning_effort_auto_omits_field() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("509", "卯"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let analyzer = Arc::new(CountingAnalyzer::new(
            Vec::new(),
            vec![schedule_event("杂谈", Some(now + Duration::days(1)), "卯")],
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let auto_post = dynamic_post("auto-post", "509", "卯", now, "明天20:00杂谈", Vec::new());
        let default_settings = AppSettings::default();
        assert_eq!(default_settings.llm_reasoning_effort, ReasoningEffort::Auto);
        let pool = test_pool("fixture-model");
        service
            .process_source_posts(
                "509",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&auto_post),
                &default_settings,
                Some(&pool),
            )
            .await
            .expect("auto process");
        let auto_call = analyzer.last_call();
        assert_eq!(auto_call.reasoning_effort, ReasoningEffort::Auto);
        assert!(auto_call.reasoning_effort.as_request_value().is_none());

        // 换一个 post id，避开缓存，验证显式档位会被带上。
        let high_post = dynamic_post("high-post", "509", "卯", now, "明天20:00杂谈", Vec::new());
        let high_settings = AppSettings {
            llm_reasoning_effort: ReasoningEffort::High,
            ..Default::default()
        };
        service
            .process_source_posts(
                "509",
                std::slice::from_ref(&streamer),
                &[high_post],
                &high_settings,
                Some(&pool),
            )
            .await
            .expect("high process");
        let high_call = analyzer.last_call();
        assert_eq!(high_call.reasoning_effort, ReasoningEffort::High);
        assert_eq!(high_call.reasoning_effort.as_request_value(), Some("high"));
    }

    #[test]
    fn shared_sources_preserve_target_order_without_blocking_unrelated_streamers() {
        let a = streamer_stub(1, "1", "甲");
        let b = streamer_stub(2, "2", "乙");
        let c = streamer_stub(3, "3", "丙");
        let sources = vec![
            ("a-main".to_owned(), vec![a.clone()]),
            ("shared".to_owned(), vec![a.clone(), b.clone()]),
            ("b-main".to_owned(), vec![b]),
            ("a-linked".to_owned(), vec![a]),
            ("c-main".to_owned(), vec![c]),
        ];
        // Shared(A,B) waits for A; later B must not overtake it. C can start.
        assert_eq!(
            next_ready_source(&sources, &[1, 2, 3, 4], &HashSet::from([1])),
            Some(3)
        );
        assert_eq!(
            next_ready_source(&sources, &[1, 2, 3], &HashSet::new()),
            Some(0)
        );
        assert_eq!(
            next_ready_source(&sources, &[2, 3], &HashSet::from([1, 2])),
            None
        );
        assert_eq!(
            next_ready_source(&sources, &[2, 3], &HashSet::new()),
            Some(0)
        );
    }

    #[tokio::test]
    async fn shared_source_runs_once_and_linked_sources_do_not_overtake_it() {
        let store = SqliteStore::in_memory().await.unwrap();
        let a = store
            .add_subscription(&subscription("991", "甲"))
            .await
            .unwrap();
        let b = store
            .add_subscription(&subscription("992", "乙"))
            .await
            .unwrap();
        let c = store
            .add_subscription(&subscription("993", "丙"))
            .await
            .unwrap();
        let sources = vec![
            ("a-main".to_owned(), vec![a.clone()]),
            ("shared".to_owned(), vec![a.clone(), b.clone()]),
            ("b-main".to_owned(), vec![b]),
            ("a-linked".to_owned(), vec![a]),
            ("c-main".to_owned(), vec![c]),
        ];
        let analyzer = Arc::new(StreamerConcurrencyAnalyzer::new(false));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .unwrap();
        let fetched = StdMutex::new(Vec::new());
        let now = Utc::now();
        let fetch = |mid: String| {
            fetched.lock().unwrap().push(mid.clone());
            let post = dynamic_post(&format!("{mid}-post"), &mid, &mid, now, &mid, Vec::new());
            async move { Ok(vec![post]) }
        };
        let settings = AppSettings {
            llm_parallelism: 2,
            ..AppSettings::default()
        };
        let pool = PostAnalyzer {
            config: test_pool("fixture-model").config,
            pool: LlmPool::new(2),
        };
        let report = service
            .refresh_sources_with_fetcher(&sources, &settings, Some(&pool), None, &|_| {}, &fetch)
            .await
            .unwrap();
        assert!(report.failed_streamers.is_empty());
        assert_eq!(report.refreshed_streamers, 3);
        assert_eq!(report.llm_calls, 5);
        let fetched = fetched.lock().unwrap();
        let position = |mid: &str| fetched.iter().position(|value| value == mid).unwrap();
        assert_eq!(fetched.iter().filter(|mid| *mid == "shared").count(), 1);
        assert!(position("a-main") < position("shared"));
        assert!(position("shared") < position("b-main"));
        assert!(position("shared") < position("a-linked"));
    }

    #[tokio::test]
    async fn failed_source_does_not_discard_other_streamers_results() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut sources = Vec::new();
        for index in 0..3 {
            let mid = format!("998{index}");
            sources.push((
                mid.clone(),
                vec![store
                    .add_subscription(&subscription(&mid, &mid))
                    .await
                    .unwrap()],
            ));
        }
        let analyzer = Arc::new(StreamerConcurrencyAnalyzer::new(false));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer).unwrap();
        let now = Utc::now();
        let fetch = |mid: String| async move {
            if mid == "9981" {
                return Err(AppError::validation("fixture fetch failed"));
            }
            Ok(vec![dynamic_post(
                &format!("{mid}-post"),
                &mid,
                &mid,
                now,
                "first",
                Vec::new(),
            )])
        };
        let settings = AppSettings {
            llm_parallelism: 2,
            ..AppSettings::default()
        };
        let pool = PostAnalyzer {
            config: test_pool("fixture-model").config,
            pool: LlmPool::new(2),
        };
        let report = service
            .refresh_sources_with_fetcher(&sources, &settings, Some(&pool), None, &|_| {}, &fetch)
            .await
            .unwrap();
        assert_eq!(report.refreshed_streamers, 2);
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 2);
        assert!(report
            .failed_streamers
            .iter()
            .any(|error| error.contains("fixture fetch failed")));
        assert!(store
            .get_streamer(sources[1].1[0].id)
            .await
            .unwrap()
            .last_error
            .is_some());
        assert!(store
            .get_streamer(sources[0].1[0].id)
            .await
            .unwrap()
            .last_error
            .is_none());
    }

    struct StreamerConcurrencyAnalyzer {
        active: AtomicUsize,
        peak: AtomicUsize,
        active_by_author: std::sync::Mutex<HashSet<String>>,
        order: std::sync::Mutex<HashMap<String, Vec<String>>>,
        barrier: Option<tokio::sync::Barrier>,
        joint_participants: Vec<String>,
    }

    impl StreamerConcurrencyAnalyzer {
        fn new(parallel: bool) -> Self {
            Self {
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                active_by_author: std::sync::Mutex::new(HashSet::new()),
                order: std::sync::Mutex::new(HashMap::new()),
                barrier: parallel.then(|| tokio::sync::Barrier::new(2)),
                joint_participants: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl ContentAnalyzer for StreamerConcurrencyAnalyzer {
        async fn analyze_post(
            &self,
            _config: &LlmConfig,
            input: &PostAnalysisInput,
        ) -> AppResult<LlmAnalysis> {
            assert!(
                self.active_by_author
                    .lock()
                    .unwrap()
                    .insert(input.author.clone()),
                "same streamer must never overlap"
            );
            self.order
                .lock()
                .unwrap()
                .entry(input.author.clone())
                .or_default()
                .push(input.text.clone());
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let paired = match &self.barrier {
                Some(barrier) => {
                    tokio::time::timeout(std::time::Duration::from_millis(900), barrier.wait())
                        .await
                        .is_ok()
                }
                None => true,
            };
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.active_by_author.lock().unwrap().remove(&input.author);
            if !paired {
                return Err(AppError::validation(
                    "fixture: another streamer never started",
                ));
            }
            let offset = if input.text == "second" { 2 } else { 1 };
            let mut events = vec![schedule_event(
                &input.text,
                Some(input.published_at + Duration::days(2) + Duration::hours(offset)),
                &input.author,
            )];
            if !self.joint_participants.is_empty() {
                events[0].participants = self.joint_participants.clone();
                events[0].room_url = Some("https://live.bilibili.com/12345".to_owned());
            }
            Ok(LlmAnalysis {
                raw: events_to_model_json(&events),
                events,
                source_digest: "fixture".to_owned(),
            })
        }
    }

    async fn assert_streamer_refresh_parallelism(parallelism: u8) {
        let store = SqliteStore::in_memory().await.unwrap();
        let analyzer = Arc::new(StreamerConcurrencyAnalyzer::new(parallelism > 1));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .unwrap();
        let mut sources = Vec::new();
        let mut posts_by_mid = HashMap::new();
        let now = Utc::now();
        for index in 0..4 {
            let mid = format!("900{index}");
            let name = format!("主播{index}");
            let streamer = store
                .add_subscription(&subscription(&mid, &name))
                .await
                .unwrap();
            // Reverse API order: each streamer's processing must still be oldest-first.
            posts_by_mid.insert(
                mid.clone(),
                vec![
                    dynamic_post(
                        &format!("{mid}-second"),
                        &mid,
                        &name,
                        now + Duration::minutes(1),
                        "second",
                        Vec::new(),
                    ),
                    dynamic_post(
                        &format!("{mid}-first"),
                        &mid,
                        &name,
                        now,
                        "first",
                        Vec::new(),
                    ),
                ],
            );
            sources.push((mid, vec![streamer]));
        }
        let fetch = |mid: String| {
            let posts = posts_by_mid[&mid].clone();
            async move { Ok(posts) }
        };
        let settings = AppSettings {
            llm_parallelism: parallelism,
            ..AppSettings::default()
        };
        let pool = PostAnalyzer {
            config: test_pool("fixture-model").config,
            pool: LlmPool::new(parallelism),
        };
        let reports = std::sync::Mutex::new(Vec::new());
        let report = service
            .refresh_sources_with_fetcher(
                &sources,
                &settings,
                Some(&pool),
                Some(&ProgressTarget::all()),
                &|p| reports.lock().unwrap().push(p),
                &fetch,
            )
            .await
            .unwrap();
        assert!(
            report.failed_streamers.is_empty(),
            "{:?}",
            report.failed_streamers
        );
        assert_eq!(report.refreshed_streamers, 4);
        assert_eq!(report.llm_calls, 8);
        assert_eq!(
            analyzer.peak.load(Ordering::SeqCst),
            usize::from(parallelism)
        );
        for order in analyzer.order.lock().unwrap().values() {
            assert_eq!(order, &["first", "second"]);
        }
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 8);
        let reports = reports.lock().unwrap();
        assert!(
            reports
                .windows(2)
                .all(|pair| pair[0].percent <= pair[1].percent),
            "global progress must not go backwards"
        );
    }

    #[tokio::test]
    async fn simultaneous_streamer_reports_create_only_one_joint_schedule() {
        let store = SqliteStore::in_memory().await.unwrap();
        let a = store
            .add_subscription(&subscription("9961", "甲"))
            .await
            .unwrap();
        let b = store
            .add_subscription(&subscription("9962", "乙"))
            .await
            .unwrap();
        let mut analyzer = StreamerConcurrencyAnalyzer::new(true);
        analyzer.joint_participants = vec!["甲".to_owned(), "乙".to_owned()];
        let analyzer = Arc::new(analyzer);
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .unwrap();
        let sources = vec![(a.mid.clone(), vec![a]), (b.mid.clone(), vec![b])];
        let now = Utc::now();
        let fetch = |mid: String| {
            let name = if mid == "9961" { "甲" } else { "乙" };
            let post = dynamic_post(
                &format!("{mid}-joint"),
                &mid,
                name,
                now,
                "联动歌回",
                Vec::new(),
            );
            async move { Ok(vec![post]) }
        };
        let settings = AppSettings {
            llm_parallelism: 2,
            ..AppSettings::default()
        };
        let pool = PostAnalyzer {
            config: test_pool("fixture-model").config,
            pool: LlmPool::new(2),
        };
        let report = service
            .refresh_sources_with_fetcher(&sources, &settings, Some(&pool), None, &|_| {}, &fetch)
            .await
            .unwrap();
        assert!(report.failed_streamers.is_empty());
        assert_eq!(
            analyzer.peak.load(Ordering::SeqCst),
            2,
            "model calls remain concurrent"
        );
        assert_eq!(
            report.new_schedules, 1,
            "must not create two rows and hide the race with final deduplication"
        );
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn independent_streamers_actually_refresh_in_parallel() {
        assert_streamer_refresh_parallelism(2).await;
    }

    #[tokio::test]
    async fn one_streamer_at_a_time_when_parallelism_is_one() {
        assert_streamer_refresh_parallelism(1).await;
    }

    /// 并发许可数就是并行个数：许可用尽后新的请求必须排队。
    #[tokio::test]
    async fn parallelism_permits_bound_the_in_flight_requests() {
        let pool = LlmPool::new(2);
        let first = pool.acquire().await;
        let second = pool.acquire().await;
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(120), pool.acquire()).await;
        assert!(blocked.is_err(), "并行个数用完时第三个请求必须等待");
        drop(first);
        let after_release =
            tokio::time::timeout(std::time::Duration::from_millis(500), pool.acquire()).await;
        assert!(after_release.is_ok(), "归还后应立即拿到许可");
        drop(second);
        assert_eq!(pool.effective_parallelism(), 2);
        // 0 会让第一个请求永远等下去，构造时兜底成 1。
        assert_eq!(LlmPool::new(0).effective_parallelism(), 1);
    }

    /// 只清「当前分析里已经没有第 N 条事件」的来源，序号还在的一条都不动。
    #[tokio::test]
    async fn pruning_only_drops_events_beyond_the_current_analysis() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("701", "甲"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let first_at = now + Duration::days(1);
        let second_at = now + Duration::days(2);
        let analyzer = Arc::new(CountingAnalyzer::new(
            vec![
                schedule_event("第一场", Some(first_at), "甲"),
                schedule_event("第二场", Some(second_at), "甲"),
            ],
            Vec::new(),
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let post = dynamic_post(
            "prune-post",
            "701",
            "甲",
            now,
            "明天八点第一场，后天八点第二场",
            vec!["https://i0.hdslb.com/bfs/new_dyn/prune.jpg"],
        );
        let pool = test_pool("fixture-model");
        service
            .process_source_posts(
                "701",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("first process");
        let identities = source_identities(&store, "prune-post").await;
        assert_eq!(identities.len(), 2, "两条事件各挂一条来源: {identities:?}");
        let kept_identity = identities[0].clone();
        // 造一条上一版提示词留下的第 7 条事件来源，指向第一场的日程。
        let schedule_id: i64 = sqlx::query_scalar(
            "SELECT schedule_id FROM schedule_sources WHERE source_identity = ?",
        )
        .bind(&kept_identity)
        .fetch_one(store.pool())
        .await
        .expect("schedule id");
        sqlx::query(
            "INSERT INTO schedule_sources
                 (schedule_id, source_kind, source_post_id, source_fingerprint,
                  source_identity, confidence, created_at, revision)
             VALUES (?, 'image', 'prune-post', 'legacy', ?, 0.9, ?, 1)",
        )
        .bind(schedule_id)
        .bind("image:prune-post:legacy:7".to_owned())
        .bind(now.to_rfc3339())
        .execute(store.pool())
        .await
        .expect("insert legacy source");

        // 第二轮命中缓存，结果仍是两条事件：第 7 条要清掉，前两条必须留着。
        service
            .process_source_posts(
                "701",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("second process");
        let identities = source_identities(&store, "prune-post").await;
        assert!(
            !identities.iter().any(|identity| identity.ends_with(":7")),
            "第 7 条事件已经不存在，来源要清掉: {identities:?}"
        );
        assert!(
            identities.contains(&kept_identity),
            "第 0 条事件还在，来源不能删: {identities:?}"
        );
        assert_eq!(
            store.list_all_schedules().await.expect("schedules").len(),
            2,
            "清理来源不该连带删掉日程"
        );
    }

    /// 摘要变化会换掉全部 identity：同一序号被新编号覆盖时，旧编号可以清掉，
    /// 但日程必须原地存活（线上思诺周表就是在这里被连带删掉的）。
    #[tokio::test]
    async fn digest_change_keeps_the_schedule_and_replaces_the_source() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("702", "乙"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let start_at = now + Duration::days(1);
        let analyzer = Arc::new(CountingAnalyzer::new(
            vec![schedule_event("电台", Some(start_at), "乙")],
            Vec::new(),
        ));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let pool = test_pool("fixture-model");
        let before = dynamic_post(
            "digest-post",
            "702",
            "乙",
            now,
            "明天八点电台",
            vec!["https://i0.hdslb.com/bfs/new_dyn/digest-a.jpg"],
        );
        service
            .process_source_posts(
                "702",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&before),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("first process");
        let old_identities = source_identities(&store, "digest-post").await;
        assert_eq!(old_identities.len(), 1);

        // 同一条动态多挂了一张图：content_digest 变了，identity 全换新。
        let mut after = before.clone();
        after
            .images
            .push("https://i0.hdslb.com/bfs/new_dyn/digest-b.jpg".to_owned());
        service
            .process_source_posts(
                "702",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&after),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("second process");

        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1, "同槽不该因为换摘要变成两条日程");
        let identities = source_identities(&store, "digest-post").await;
        assert_eq!(
            identities.len(),
            1,
            "旧摘要那条已被新编号覆盖，只留一条来源: {identities:?}"
        );
        assert_ne!(identities[0], old_identities[0], "留下的必须是新摘要的编号");
    }

    /// 分析失败时一条来源都不许动：模型偶尔看走眼，不能把还能用的场次删掉。
    #[tokio::test]
    async fn pruning_skips_everything_when_analysis_failed() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("703", "丙"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let start_at = now + Duration::days(1);
        let ok_analyzer = Arc::new(CountingAnalyzer::new(
            vec![schedule_event("电台", Some(start_at), "丙")],
            Vec::new(),
        ));
        let service = AppService::with_analyzer(
            store.clone(),
            RuntimeSecrets::default(),
            ok_analyzer.clone(),
        )
        .expect("service");
        let pool = test_pool("fixture-model");
        let post = dynamic_post("fail-post", "703", "丙", now, "明天八点电台", Vec::new());
        service
            .process_source_posts(
                "703",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("first process");
        let before = source_identities(&store, "fail-post").await;
        assert_eq!(before.len(), 1);

        // 第二轮换成会报错的模型：清理必须整体跳过。
        let failing = Arc::new(FailingAnalyzer);
        let failing_service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), failing)
                .expect("failing service");
        let mut changed = post.clone();
        changed.text = "明天八点电台，别错过".to_owned();
        failing_service
            .process_source_posts(
                "703",
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&changed),
                &AppSettings::default(),
                Some(&pool),
            )
            .await
            .expect("failing process");

        assert_eq!(
            source_identities(&store, "fail-post").await,
            before,
            "分析失败时来源必须原样保留"
        );
        assert_eq!(
            store.list_all_schedules().await.expect("schedules").len(),
            1
        );
    }

    #[tokio::test]
    async fn public_refresh_joins_background_progress_and_survives_caller_disconnect() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut input = subscription("99414", "共享主播");
        input.schedule_enabled = false; // No external API requests in this integration fixture.
        input.avatar_url = Some(String::new());
        let streamer = store.add_subscription(&input).await.unwrap();
        let service = AppService::with_analyzer(
            store,
            RuntimeSecrets::default(),
            Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new())),
        )
        .unwrap();
        let held = service.refresh_lock.lock().await;
        let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
        let first_service = service.clone();
        let first = tokio::spawn(async move {
            first_service
                .refresh_all_with_progress(move |progress| {
                    let _ = first_tx.send(progress);
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(progress) = first_rx.recv().await {
                if progress.stage.contains("等待上一轮刷新") {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let (second_tx, mut second_rx) = tokio::sync::mpsc::unbounded_channel();
        let second_service = service.clone();
        let second = tokio::spawn(async move {
            second_service
                .refresh_streamer_with_progress(streamer.id, move |progress| {
                    let _ = second_tx.send(progress);
                })
                .await
        });
        let joined = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let progress = second_rx.recv().await.expect("progress stream open");
                if progress.stage.contains("已加入") {
                    break progress;
                }
            }
        })
        .await
        .unwrap();
        assert!(joined.stage.contains("全量"));
        first.abort();
        let _ = first.await;
        drop(held);
        let report = tokio::time::timeout(std::time::Duration::from_secs(3), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(report.failed_streamers.is_empty());
        let mut saw_completion = false;
        while let Ok(progress) = second_rx.try_recv() {
            saw_completion |= progress.done;
        }
        assert!(
            saw_completion,
            "remaining waiter receives actual completion after leader disconnects"
        );
    }

    #[tokio::test]
    async fn prepared_job_executes_the_same_snapshot_used_for_its_key() {
        let store = SqliteStore::in_memory().await.unwrap();
        let service = AppService::with_analyzer(
            store.clone(),
            RuntimeSecrets::default(),
            Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new())),
        )
        .unwrap();
        let prepared = service.prepare_refresh_request(None).await.unwrap();
        let key = prepared.request_key().to_owned();
        // A changed (here deliberately invalid) setting after job registration
        // must not be read again by this already prepared operation.
        sqlx::query("INSERT INTO settings(key,value,updated_at) VALUES('llm_model','invalid-json','2026-11-11T00:00:00Z') ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .execute(store.pool()).await.unwrap();
        assert!(service.refresh_request_key(None).await.is_err());
        assert_eq!(prepared.request_key(), key);
        let result = service
            .refresh_prepared_with_progress(prepared, |_| {})
            .await
            .unwrap();
        assert_eq!(result.refreshed_streamers, 0);
        assert!(result.failed_streamers.is_empty());
    }

    #[tokio::test]
    async fn refresh_request_fingerprint_ignores_runtime_state_but_tracks_inputs() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&subscription("99412", "指纹主播"))
            .await
            .unwrap();
        let service = AppService::with_analyzer(
            store.clone(),
            RuntimeSecrets::default(),
            Arc::new(CountingAnalyzer::new(Vec::new(), Vec::new())),
        )
        .unwrap();
        let key = service.refresh_request_key(None).await.unwrap();
        store
            .record_refresh_result(streamer.id, None)
            .await
            .unwrap();
        assert_eq!(service.refresh_request_key(None).await.unwrap(), key);
        sqlx::query("UPDATE streamers SET name='新名字' WHERE id=?")
            .bind(streamer.id)
            .execute(store.pool())
            .await
            .unwrap();
        let renamed = service.refresh_request_key(None).await.unwrap();
        assert_ne!(renamed, key);
        service
            .llm_key_binding
            .write()
            .await
            .replace(("new-secret".to_owned(), "https://example.com/v1".to_owned()));
        let changed_key = service.refresh_request_key(None).await.unwrap();
        assert_ne!(changed_key, renamed);
        assert!(!changed_key.contains("new-secret"));
    }

    #[tokio::test]
    async fn retry_is_persistent_and_success_clears_it_without_losing_old_sources() {
        struct RecoveringAnalyzer(AtomicBool);
        #[async_trait]
        impl ContentAnalyzer for RecoveringAnalyzer {
            async fn analyze_post(
                &self,
                _config: &LlmConfig,
                input: &PostAnalysisInput,
            ) -> AppResult<LlmAnalysis> {
                if self.0.load(Ordering::SeqCst) {
                    return Err(AppError::External {
                        code: "LLM_503".to_owned(),
                        message: "temporary fixture-key failure".to_owned(),
                    });
                }
                let events = vec![schedule_event(
                    "恢复歌回",
                    Some(input.published_at + Duration::days(1)),
                    &input.author,
                )];
                Ok(LlmAnalysis {
                    raw: events_to_model_json(&events),
                    events,
                    source_digest: "fixture".to_owned(),
                })
            }
        }
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&subscription("99413", "恢复主播"))
            .await
            .unwrap();
        let analyzer = Arc::new(RecoveringAnalyzer(AtomicBool::new(true)));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .unwrap();
        let post = dynamic_post(
            "recover-post",
            &streamer.mid,
            &streamer.name,
            Utc::now(),
            "明天歌回",
            Vec::new(),
        );
        let mut pool = test_pool("recover-model");
        pool.config.api_key = "fixture-key".to_owned();
        let settings = AppSettings::default();
        let first = service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &settings,
                Some(&pool),
            )
            .await
            .unwrap();
        assert!(!first.analysis_errors.is_empty());
        let (key, raw): (String, String) = sqlx::query_as(
            "SELECT key,value FROM client_sync_state WHERE key LIKE 'analysis_retry:%'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(
            !raw.contains("fixture-key"),
            "credentials must be redacted from persisted errors"
        );
        analyzer.0.store(false, Ordering::SeqCst);
        // A fresh service object still observes the persisted deadline.
        let second_service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer).unwrap();
        let waiting = second_service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &settings,
                Some(&pool),
            )
            .await
            .unwrap();
        assert_eq!(waiting.deferred_analyses, 1);
        assert_eq!(waiting.llm_calls, 0);
        assert!(waiting
            .analysis_errors
            .values()
            .all(|message| message.contains("暂缓重试")));
        assert_eq!(
            store.get_client_sync_state(&key).await.unwrap().unwrap(),
            raw,
            "skip does not extend deadline"
        );
        let mut state: serde_json::Value = serde_json::from_str(&raw).unwrap();
        state["next_retry_at"] = json!((Utc::now() - Duration::seconds(1)).to_rfc3339());
        store
            .set_client_sync_state(&key, &state.to_string())
            .await
            .unwrap();
        let recovered = second_service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &settings,
                Some(&pool),
            )
            .await
            .unwrap();
        assert_eq!(recovered.llm_calls, 1);
        assert!(recovered.analysis_errors.is_empty());
        assert!(store.get_client_sync_state(&key).await.unwrap().is_none());
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn repeated_analysis_failure_is_deferred_but_content_changes_retry() {
        struct CountFailures(AtomicUsize);
        #[async_trait]
        impl ContentAnalyzer for CountFailures {
            async fn analyze_post(
                &self,
                _config: &LlmConfig,
                _input: &PostAnalysisInput,
            ) -> AppResult<LlmAnalysis> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(AppError::External {
                    code: "LLM_503".to_owned(),
                    message: "fixture unavailable".to_owned(),
                })
            }
        }
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&subscription("99411", "测试主播"))
            .await
            .unwrap();
        let analyzer = Arc::new(CountFailures(AtomicUsize::new(0)));
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .unwrap();
        let mut post = dynamic_post(
            "retry-post",
            &streamer.mid,
            &streamer.name,
            Utc::now(),
            "明晚歌回",
            Vec::new(),
        );
        let pool = test_pool("retry-model");
        let settings = AppSettings::default();
        for _ in 0..2 {
            let stats = service
                .process_source_posts(
                    &streamer.mid,
                    std::slice::from_ref(&streamer),
                    std::slice::from_ref(&post),
                    &settings,
                    Some(&pool),
                )
                .await
                .unwrap();
            assert!(!stats.analysis_errors.is_empty());
            assert_eq!(
                stats.llm_cache_hits, 0,
                "failure is not successful cached emptiness"
            );
        }
        assert_eq!(
            analyzer.0.load(Ordering::SeqCst),
            1,
            "same failure must not call the model every refresh"
        );
        post.text.push_str("改为九点");
        service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &settings,
                Some(&pool),
            )
            .await
            .unwrap();
        assert_eq!(
            analyzer.0.load(Ordering::SeqCst),
            2,
            "changed content bypasses backoff"
        );
        let mut changed_pool = test_pool("other-model");
        service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &settings,
                Some(&changed_pool),
            )
            .await
            .unwrap();
        assert_eq!(
            analyzer.0.load(Ordering::SeqCst),
            3,
            "model change bypasses backoff"
        );
        changed_pool.config.api_key = "changed-key".to_owned();
        service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &settings,
                Some(&changed_pool),
            )
            .await
            .unwrap();
        assert_eq!(
            analyzer.0.load(Ordering::SeqCst),
            4,
            "credential change bypasses backoff"
        );
        let changed_settings = AppSettings {
            llm_reasoning_effort: ReasoningEffort::High,
            ..settings.clone()
        };
        service
            .process_source_posts(
                &streamer.mid,
                std::slice::from_ref(&streamer),
                std::slice::from_ref(&post),
                &changed_settings,
                Some(&changed_pool),
            )
            .await
            .unwrap();
        assert_eq!(
            analyzer.0.load(Ordering::SeqCst),
            5,
            "reasoning change bypasses backoff"
        );
        assert!(store.list_all_schedules().await.unwrap().is_empty());
        let successes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM post_analyses")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(successes, 0);
    }

    struct FailingAnalyzer;

    #[async_trait]
    impl ContentAnalyzer for FailingAnalyzer {
        async fn analyze_post(
            &self,
            _config: &LlmConfig,
            _input: &PostAnalysisInput,
        ) -> AppResult<LlmAnalysis> {
            Err(AppError::External {
                code: "LLM_TEST".to_owned(),
                message: "日程模型调用失败".to_owned(),
            })
        }
    }
}
