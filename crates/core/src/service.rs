use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock as StdRwLock};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

use crate::bilibili::{BilibiliClient, QrLoginPoll, QrLoginStart};
use crate::error::{AppError, AppResult};
use crate::fertility::{parse_fertility_payload, FertilityClient};
use crate::live_stats::LiveStatsManager;
use crate::llm::{
    analysis_cache_key, events_from_raw, ContentAnalyzer, LlmClient, LlmConfig, LlmDiscoveryConfig,
    LlmEvent, LlmEventAction, LlmTestReport,
};
use crate::models::{
    AddSubscription, DynamicPost, FertilitySnapshot, FertilityStatus, LiveStatus, NewSchedule,
    NewWeeklyScheduleRule, RefreshProgress, RefreshReport, Schedule, Streamer, UpdateSchedule,
    UpdateSubscription, UpdateWeeklyScheduleRule, WeeklyScheduleRule,
};
use crate::network::{test_google, ProxyTestReport};
use crate::schedule::{parse_reserve_schedule, ParsedSchedule};
use crate::settings::{load_settings, AppSettings, RuntimeSecrets};
use crate::store::SqliteStore;

const A_SOUL_OFFICIAL_MID: &str = "703007996";
const A_SOUL_MEMBER_MIDS: &[&str] = &["672342685", "672328094", "672353429"];

#[derive(Clone)]
pub struct AppService {
    store: SqliteStore,
    bili: Arc<StdRwLock<Arc<BilibiliClient>>>,
    llm: Arc<StdRwLock<Arc<dyn ContentAnalyzer>>>,
    fertility: Arc<StdRwLock<Arc<FertilityClient>>>,
    secrets: Arc<RwLock<RuntimeSecrets>>,
    live_stats: LiveStatsManager,
    refresh_lock: Arc<Mutex<()>>,
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
        let llm = Arc::new(LlmClient::new_with_proxy(proxy_port)?);
        Ok(Self {
            store,
            bili: Arc::new(StdRwLock::new(bili)),
            llm: Arc::new(StdRwLock::new(llm)),
            fertility: Arc::new(StdRwLock::new(Arc::new(FertilityClient::new_with_proxy(
                proxy_port,
            )?))),
            secrets: Arc::new(RwLock::new(secrets)),
            live_stats: LiveStatsManager::default(),
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn with_analyzer(
        store: SqliteStore,
        secrets: RuntimeSecrets,
        analyzer: Arc<dyn ContentAnalyzer>,
    ) -> AppResult<Self> {
        let bili = Arc::new(BilibiliClient::new(secrets.bili_cookie.clone())?);
        Ok(Self {
            store,
            bili: Arc::new(StdRwLock::new(bili)),
            llm: Arc::new(StdRwLock::new(analyzer)),
            fertility: Arc::new(StdRwLock::new(Arc::new(FertilityClient::new()?))),
            secrets: Arc::new(RwLock::new(secrets)),
            live_stats: LiveStatsManager::default(),
            refresh_lock: Arc::new(Mutex::new(())),
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
        let bili_cookie = self.secrets.read().await.bili_cookie.clone();
        let bili = Arc::new(BilibiliClient::new_with_proxy(bili_cookie, proxy_port)?);
        let llm: Arc<dyn ContentAnalyzer> = Arc::new(LlmClient::new_with_proxy(proxy_port)?);
        let fertility = Arc::new(FertilityClient::new_with_proxy(proxy_port)?);
        *self
            .bili
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = bili;
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

    async fn acquire_refresh_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.refresh_lock.lock().await
    }

    async fn capture_avatar_data(&self, avatar_url: &str) -> Option<String> {
        let avatar_url = avatar_url.trim();
        if avatar_url.is_empty() || avatar_url.starts_with("data:image/") {
            return None;
        }
        match self.bili_client().fetch_avatar_data_url(avatar_url).await {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(%error, %avatar_url, "capture streamer avatar failed");
                None
            }
        }
    }

    async fn backfill_avatar_captures(&self, streamers: &[Streamer]) {
        let mut seen = HashSet::new();
        for streamer in streamers {
            if !seen.insert(streamer.id) || streamer.avatar_url.starts_with("data:image/") {
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
        *self.secrets.write().await = secrets;
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
        self.secrets.write().await.llm_api_key = Some(api_key);
        Ok(())
    }

    pub async fn test_llm(&self, mut config: LlmConfig) -> AppResult<LlmTestReport> {
        if config.api_key.trim().is_empty() {
            config.api_key = self
                .secrets
                .read()
                .await
                .llm_api_key
                .clone()
                .unwrap_or_default();
        }
        self.llm_analyzer().test_calendar_image(&config).await
    }

    pub async fn list_llm_models(&self, mut config: LlmDiscoveryConfig) -> AppResult<Vec<String>> {
        if config.api_key.trim().is_empty() {
            config.api_key = self
                .secrets
                .read()
                .await
                .llm_api_key
                .clone()
                .unwrap_or_default();
        }
        self.llm_analyzer().list_models(&config).await
    }

    pub async fn search_accounts(
        &self,
        keyword: &str,
    ) -> AppResult<Vec<crate::models::AccountSearchResult>> {
        let keyword = keyword.trim();
        if keyword.is_empty() || keyword.chars().count() > 100 {
            return Err(crate::error::AppError::validation(
                "搜索关键词长度必须为 1 到 100 字",
            ));
        }
        let subscriptions = self.store.list_streamers().await.unwrap_or_default();
        let local_matches: Vec<crate::models::AccountSearchResult> = subscriptions
            .iter()
            .filter(|streamer| search_matches_subscription(keyword, streamer))
            .map(account_from_streamer)
            .collect();
        let mut results = match self.bili_client().search_accounts(keyword).await {
            Ok(results) => results,
            Err(error) if !local_matches.is_empty() => {
                tracing::warn!(%error, "Bilibili account search failed; using local exact matches");
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        results.extend(local_matches);
        dedupe_and_rank_search_results(&mut results, keyword, &subscriptions);
        Ok(results)
    }

    pub async fn create_bili_qr_login(&self) -> AppResult<QrLoginStart> {
        self.bili_client().create_qr_login().await
    }

    pub async fn poll_bili_qr_login(&self, qrcode_key: &str) -> AppResult<QrLoginPoll> {
        self.bili_client().poll_qr_login(qrcode_key).await
    }

    pub async fn add_subscription(&self, mut input: AddSubscription) -> AppResult<Streamer> {
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
        self.store.update_subscription(id, update).await
    }

    pub async fn reorder_subscriptions(&self, ids: &[i64]) -> AppResult<Vec<Streamer>> {
        self.store.reorder_subscriptions(ids).await
    }

    pub async fn delete_subscription(&self, id: i64) -> AppResult<()> {
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
        let rule = self.store.add_weekly_rule(input).await?;
        let now = Utc::now();
        self.materialize_weekly_rules(now, now + Duration::days(84))
            .await?;
        let _ = self
            .store
            .append_event(
                "weekly_rule_changed",
                &json!({"action": "created", "ruleId": rule.id, "streamerId": rule.streamer_id}),
            )
            .await;
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

    pub async fn list_fertility_statuses(
        &self,
        force_refresh: bool,
    ) -> AppResult<FertilitySnapshot> {
        let streamers = self.store.list_streamers().await?;
        if streamers.is_empty() {
            return Ok(FertilitySnapshot {
                fetched_at: Utc::now(),
                source: "none".to_owned(),
                stale: false,
                warning: None,
                total_available: 0,
                matched: 0,
                statuses: Vec::new(),
            });
        }

        let cached = self.store.get_fertility_cache().await?;
        let now = Utc::now();
        let mut payload = None;
        let mut meta = None;

        if !force_refresh {
            if let Some(cache) = &cached {
                if now.signed_duration_since(cache.fetched_at) < Duration::hours(6) {
                    match parse_fertility_payload(&cache.payload_json) {
                        Ok(parsed) => {
                            payload = Some(parsed);
                            meta = Some((cache.fetched_at, cache.source.clone(), false, None));
                        }
                        Err(error) => {
                            tracing::warn!(%error, "cached fertility payload is invalid");
                        }
                    }
                }
            }
        }

        if payload.is_none() {
            match self.fertility_client().fetch().await {
                Ok((fetched, source, fetched_at)) => {
                    let serialized = serde_json::to_string(&fetched)?;
                    self.store
                        .save_fertility_cache(&serialized, fetched_at, &source)
                        .await?;
                    payload = Some(fetched);
                    meta = Some((fetched_at, source, false, None));
                }
                Err(error) => {
                    let fallback = cached.as_ref().and_then(|cache| {
                        parse_fertility_payload(&cache.payload_json)
                            .ok()
                            .map(|payload| (cache, payload))
                    });
                    if let Some((cache, parsed)) = fallback {
                        payload = Some(parsed);
                        meta = Some((
                            cache.fetched_at,
                            cache.source.clone(),
                            true,
                            Some(format!("魔法期数据刷新失败，已使用缓存：{error}")),
                        ));
                    } else {
                        return Err(error);
                    }
                }
            }
        }

        let payload = payload.expect("fertility payload initialized");
        let (fetched_at, source, stale, warning) = meta.expect("fertility metadata initialized");
        let remote_by_mid: HashMap<String, &crate::fertility::FertilityRecord> = payload
            .data
            .iter()
            .map(|record| (record.uid.to_string(), record))
            .collect();
        let mut statuses = Vec::new();
        for streamer in &streamers {
            let record = remote_by_mid.get(&streamer.mid).copied().or_else(|| {
                streamer
                    .linked_accounts
                    .iter()
                    .find_map(|linked| remote_by_mid.get(&linked.mid).copied())
            });
            let Some(record) = record else {
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
            });
        }
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
        let matched = statuses.len();
        Ok(FertilitySnapshot {
            fetched_at,
            source,
            stale,
            warning,
            total_available: payload.total.max(payload.data.len()),
            matched,
            statuses,
        })
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
        let live_stats_enabled = load_settings(&self.store).await?.live_stats_enabled;
        for (index, streamer) in streamers.iter().enumerate() {
            if index > 0 && index % 5 == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            let room_id = match streamer.room_id.clone() {
                Some(room_id) => Some(room_id),
                None => match self.bili_client().resolve_room_id(&streamer.mid).await {
                    Ok(room_id) => {
                        if let Some(room_id) = &room_id {
                            if let Err(error) =
                                self.store.set_streamer_room(streamer.id, room_id).await
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
                        continue;
                    }
                },
            };
            let Some(room_id) = room_id else {
                continue;
            };
            let snapshot = match self.bili_client().fetch_live_status(&room_id).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.store
                        .record_streamer_error(streamer.id, &error.to_string())
                        .await?;
                    continue;
                }
            };
            let previous = self.store.set_live_status(streamer.id, &snapshot).await?;
            let stats_enabled = live_stats_enabled && streamer.live_stats_enabled;
            if stats_enabled && snapshot.is_live {
                let started_at = snapshot.started_at.unwrap_or(snapshot.checked_at);
                match self
                    .store
                    .ensure_live_session(
                        streamer.id,
                        &room_id,
                        &streamer.name,
                        &snapshot.title,
                        snapshot.cover_url.as_deref(),
                        started_at,
                        snapshot.checked_at,
                    )
                    .await
                {
                    Ok(session) => {
                        if let Err(error) = self
                            .store
                            .record_live_online(streamer.id, snapshot.online, snapshot.checked_at)
                            .await
                        {
                            tracing::warn!(%error, streamer_id = streamer.id, "保存直播人气失败");
                        }
                        self.live_stats
                            .start(self.store.clone(), self.bili_client(), session)
                            .await;
                    }
                    Err(error) => {
                        tracing::warn!(%error, streamer_id = streamer.id, "创建直播数据场次失败");
                    }
                }
            } else if previous && (!snapshot.is_live || !stats_enabled) {
                self.live_stats.stop(streamer.id, snapshot.checked_at).await;
                if let Err(error) = self
                    .store
                    .finish_open_live_session(streamer.id, snapshot.checked_at)
                    .await
                {
                    tracing::warn!(%error, streamer_id = streamer.id, "结束直播数据场次失败");
                }
            }
            if emit_transitions
                && streamer.live_checked_at.is_some()
                && previous != snapshot.is_live
            {
                self.store
                    .append_event(
                        "live_status",
                        &json!({
                            "streamerId": streamer.id,
                            "mid": streamer.mid,
                            "streamerName": streamer.name,
                            "previousIsLive": previous,
                            "isLive": snapshot.is_live,
                            "title": snapshot.title,
                            "coverUrl": snapshot.cover_url,
                            "liveUrl": snapshot.live_url,
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

    pub async fn refresh_all(&self) -> AppResult<RefreshReport> {
        // A manual refresh may arrive while the periodic monitor is running.
        // Queue it instead of failing and discarding the caller's progress UI.
        let _guard = self.acquire_refresh_lock().await;
        let settings = load_settings(&self.store).await?;
        let llm_config = self.current_llm_config(&settings).await;
        let streamers = self.store.list_streamers().await?;
        self.backfill_avatar_captures(&streamers).await;
        let sources = sources_for_streamers(&streamers);
        self.refresh_sources(&sources, &settings, llm_config.as_ref(), None, &|_| {})
            .await
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
        // Keep one refresh in flight, but let a requested refresh wait for the
        // current background run instead of returning an immediate validation error.
        let _guard = self.acquire_refresh_lock().await;
        let streamer = self.store.get_streamer(streamer_id).await?;
        self.backfill_avatar_captures(std::slice::from_ref(&streamer))
            .await;
        emit_progress(
            &streamer,
            "准备抓取主播与官号动态",
            0,
            0,
            3,
            false,
            false,
            &on_progress,
        );
        let result = async {
            let settings = load_settings(&self.store).await?;
            let llm_config = self.current_llm_config(&settings).await;
            let sources = sources_for_streamers(std::slice::from_ref(&streamer));
            self.refresh_sources(
                &sources,
                &settings,
                llm_config.as_ref(),
                Some(&streamer),
                &on_progress,
            )
            .await
        }
        .await;
        match &result {
            Ok(report) => {
                let failed = !report.failed_streamers.is_empty();
                emit_progress(
                    &streamer,
                    if failed {
                        "刷新完成，但部分来源失败"
                    } else {
                        "刷新完成"
                    },
                    1,
                    1,
                    100,
                    true,
                    failed,
                    &on_progress,
                );
            }
            Err(error) => {
                emit_progress(
                    &streamer,
                    &format!("刷新失败: {error}"),
                    1,
                    1,
                    100,
                    true,
                    true,
                    &on_progress,
                );
            }
        }
        result
    }

    async fn current_llm_config(&self, settings: &AppSettings) -> Option<LlmConfig> {
        let secrets = self.secrets.read().await.clone();
        (settings.auto_analyze_images && !settings.llm_model.trim().is_empty()).then(|| LlmConfig {
            base_url: settings.llm_base_url.clone(),
            api_key: secrets.llm_api_key.clone().unwrap_or_default(),
            model: settings.llm_model.clone(),
            prompt_version: settings.llm_prompt_version.clone(),
        })
    }

    async fn refresh_sources(
        &self,
        sources: &[(String, Vec<Streamer>)],
        settings: &AppSettings,
        llm_config: Option<&LlmConfig>,
        progress_streamer: Option<&Streamer>,
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
        let mut analysis_cache: HashMap<String, CachedImageAnalysis> = HashMap::new();
        let mut event_state = RefreshEventState::default();
        let mut refreshed_streamer_ids = HashSet::new();
        for (index, (source_mid, targets)) in sources.iter().enumerate() {
            if index > 0 && index % 5 == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(350)).await;
            }
            if let Some(streamer) = progress_streamer {
                emit_progress(
                    streamer,
                    &format!("拉取动态 {}/{}", index + 1, sources.len()),
                    index,
                    sources.len(),
                    5 + (index * 80 / sources.len().max(1)) as u8,
                    false,
                    false,
                    on_progress,
                );
            }
            match self.bili_client().fetch_dynamics(source_mid, 20).await {
                Ok(posts) => {
                    let stats = self
                        .process_source_posts_with_progress(
                            source_mid,
                            targets,
                            &posts,
                            settings,
                            llm_config,
                            &mut analysis_cache,
                            &mut event_state,
                            progress_streamer,
                            on_progress,
                            index,
                            sources.len(),
                        )
                        .await?;
                    report.new_schedules += stats.new_schedules;
                    report.cancelled_schedules += stats.cancelled_schedules;
                    report.llm_calls += stats.llm_calls;
                    report.llm_cache_hits += stats.llm_cache_hits;
                    for target in targets {
                        self.store.record_refresh_result(target.id, None).await?;
                        refreshed_streamer_ids.insert(target.id);
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    for target in targets {
                        self.store
                            .record_refresh_result(target.id, Some(&message))
                            .await?;
                    }
                    if let Some(streamer) = progress_streamer {
                        emit_progress(
                            streamer,
                            "拉取动态失败",
                            index,
                            sources.len(),
                            5 + ((index + 1) * 80 / sources.len().max(1)) as u8,
                            false,
                            true,
                            on_progress,
                        );
                    }
                    report
                        .failed_streamers
                        .push(format!("{source_mid}: {message}"));
                }
            }
        }
        report.cancelled_schedules += self.apply_pending_cancellations(&mut event_state).await?;
        match self.dedupe_group_schedules().await {
            Ok(removed) if removed > 0 => {
                tracing::info!(removed, "merged duplicate group schedules");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "deduplicate group schedules failed"),
        }
        report.refreshed_streamers = refreshed_streamer_ids.len();
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

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn process_source_posts(
        &self,
        source_mid: &str,
        targets: &[Streamer],
        posts: &[DynamicPost],
        settings: &AppSettings,
        llm_config: Option<&LlmConfig>,
        analysis_cache: &mut HashMap<String, CachedImageAnalysis>,
    ) -> AppResult<ProcessStats> {
        let mut event_state = RefreshEventState::default();
        let mut stats = self
            .process_source_posts_with_progress(
                source_mid,
                targets,
                posts,
                settings,
                llm_config,
                analysis_cache,
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
        llm_config: Option<&LlmConfig>,
        analysis_cache: &mut HashMap<String, CachedImageAnalysis>,
        event_state: &mut RefreshEventState,
        progress_streamer: Option<&Streamer>,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
        source_index: usize,
        source_count: usize,
    ) -> AppResult<ProcessStats> {
        let now = Utc::now();
        let ordered_posts = ordered_posts_oldest_first(posts);
        let posts = ordered_posts.as_slice();
        let mut stats = ProcessStats::default();
        let analyze_images = llm_config.is_some() && settings.auto_analyze_images;
        let total_steps = posts.len()
            + if analyze_images {
                posts
                    .iter()
                    .filter(|post| post.reserve.is_none())
                    .map(|post| post.images.len())
                    .sum::<usize>()
            } else {
                0
            };
        let mut completed_steps = 0;
        let source_base = 10 + (source_index * 80 / source_count.max(1)) as u8;
        let source_span = (80 / source_count.max(1)).saturating_sub(5).max(1) as u8;
        let report_step =
            |stage: &str, completed_steps: usize, total_steps: usize, failed: bool| {
                let Some(streamer) = progress_streamer else {
                    return;
                };
                let fraction = completed_steps
                    .checked_mul(60)
                    .and_then(|value| value.checked_div(total_steps))
                    .unwrap_or(60) as u8;
                emit_progress(
                    streamer,
                    stage,
                    completed_steps,
                    total_steps,
                    (source_base as usize + source_span as usize * fraction as usize / 60) as u8,
                    false,
                    failed,
                    on_progress,
                );
            };
        for post in posts {
            report_step(
                &format!("识别动态日程（{} 条）", posts.len()),
                completed_steps,
                total_steps,
                false,
            );
            self.store.save_dynamic_post(post).await?;
            if post.reserve.is_some() {
                stats.cancelled_schedules += self
                    .remove_stale_model_schedules_for_reserved_post(post, targets)
                    .await?;
            }
            let reserve_schedule = post
                .reserve
                .as_ref()
                .and_then(|reserve| parse_reserve_schedule(reserve, post.published_at, now));
            if let Some(schedule) = reserve_schedule {
                let context = reserve_context(post);
                if let Some(target) = choose_text_target(source_mid, targets, &context) {
                    let fingerprint = source_fingerprint(post, &schedule.title, "reserve");
                    let identity = format!("reserve:{}", post.id);
                    let participants = [target.name.clone()];
                    let reserve_room_url = post
                        .reserve
                        .as_ref()
                        .and_then(|reserve| reserve.room_id.as_deref())
                        .filter(|room_id| {
                            !room_id.is_empty()
                                && room_id.chars().all(|character| character.is_ascii_digit())
                        })
                        .map(|room_id| format!("https://live.bilibili.com/{room_id}"));
                    if self
                        .upsert_candidate(
                            &target,
                            &schedule,
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
                        .await
                    {
                        stats.new_schedules += 1;
                    }
                }
            } else if post
                .reserve
                .as_ref()
                .is_some_and(|reserve| !reserve.is_live())
            {
                stats.cancelled_schedules +=
                    self.remove_stale_non_live_reserve_schedule(post).await?;
            }

            let has_combined_images = post.reserve.is_none()
                && llm_config.is_some()
                && settings.auto_analyze_images
                && !post.images.is_empty();
            if let Some(config) = llm_config.filter(|_| post.reserve.is_none()) {
                let text = post.text.trim();
                if !has_combined_images && !text.is_empty() {
                    let text_digest = text_digest(text);
                    let prompt_version = format!("{}-text-v7", config.prompt_version);
                    let cache_key =
                        analysis_cache_key(&post.id, &text_digest, &config.model, &prompt_version);
                    let cached_events = match self.store.get_text_analysis(&cache_key).await? {
                        Some(raw) => match events_from_raw(&raw, now) {
                            Ok(events) => {
                                stats.llm_cache_hits += 1;
                                Some(events)
                            }
                            Err(error) => {
                                tracing::warn!(%error, post_id = %post.id, "cached text analysis invalid");
                                None
                            }
                        },
                        None => None,
                    };
                    let events = match cached_events {
                        Some(events) => Some(events),
                        None => match self
                            .llm_analyzer()
                            .analyze_text(config, text, post.published_at, now)
                            .await
                        {
                            Ok(analysis) => {
                                stats.llm_calls += 1;
                                self.store
                                    .save_text_analysis(
                                        &cache_key,
                                        &post.id,
                                        &text_digest,
                                        &config.model,
                                        &prompt_version,
                                        &analysis.raw,
                                    )
                                    .await?;
                                Some(analysis.events)
                            }
                            Err(error) => {
                                tracing::warn!(%error, post_id = %post.id, "text analysis failed");
                                for target in targets {
                                    let _ = self
                                        .store
                                        .record_streamer_error(
                                            target.id,
                                            &format!("动态文字分析失败: {error}"),
                                        )
                                        .await;
                                }
                                None
                            }
                        },
                    };
                    for (event_index, event) in events.unwrap_or_default().iter().enumerate() {
                        let event = canonicalize_event_participants(event, targets);
                        if let Some(target) = match_event_target(&event, targets) {
                            if event.action == LlmEventAction::Cancel {
                                event_state.cancellations.push(PendingCancellation {
                                    target,
                                    event: event.clone(),
                                    source_post_id: post.id.clone(),
                                    published_at: post.published_at,
                                });
                                continue;
                            }
                            let fingerprint = format!(
                                "{}:{}:{}:{}",
                                source_fingerprint(post, &event.title, "text"),
                                event_index,
                                event.performer,
                                event.participant_names().join("|")
                            );
                            let identity = format!("text:{}:{text_digest}:{event_index}", post.id);
                            let (inserted, cancelled) = self
                                .apply_llm_event(
                                    &target,
                                    targets,
                                    &event,
                                    "text",
                                    post,
                                    "editor",
                                    &identity,
                                    &fingerprint,
                                )
                                .await?;
                            if inserted {
                                stats.new_schedules += 1;
                            }
                            if cancelled {
                                stats.cancelled_schedules += 1;
                            }
                        }
                    }
                }
            }

            if let Some(config) = llm_config.filter(|_| post.reserve.is_none()) {
                if settings.auto_analyze_images && !post.images.is_empty() {
                    let context_digest = text_digest(post.text.trim());
                    let image_prompt_version = format!(
                        "{}-image-context-v4-{}",
                        config.prompt_version,
                        &context_digest[..12]
                    );
                    for (image_index, image_url) in post.images.iter().enumerate() {
                        let image_cache_key = format!(
                            "{}:{}:{image_url}:{image_prompt_version}",
                            post.id,
                            post.published_at.timestamp()
                        );
                        report_step(
                            &format!("识别图片日程（{}/{}）", image_index + 1, post.images.len()),
                            completed_steps,
                            total_steps,
                            false,
                        );
                        let analysis = if let Some(cached) =
                            analysis_cache.get(&image_cache_key).cloned()
                        {
                            stats.llm_cache_hits += 1;
                            Some(cached)
                        } else if let Some(cache_key) = self
                            .store
                            .get_image_analysis_cache_key(
                                &post.id,
                                image_url,
                                &config.model,
                                &image_prompt_version,
                            )
                            .await?
                        {
                            match self.store.get_image_analysis(&cache_key).await? {
                                Some(raw) => match events_from_raw(&raw, now) {
                                    Ok(events) => {
                                        stats.llm_cache_hits += 1;
                                        let _ = raw;
                                        Some(CachedImageAnalysis {
                                            events,
                                            source_digest: cache_key_digest(&cache_key),
                                        })
                                    }
                                    Err(error) => {
                                        tracing::warn!(%error, post_id = %post.id, "cached llm result invalid");
                                        None
                                    }
                                },
                                None => None,
                            }
                        } else {
                            None
                        };
                        let analysis = match analysis {
                            Some(value) => value,
                            None => {
                                match self
                                    .llm_analyzer()
                                    .analyze_image_url(
                                        config,
                                        image_url,
                                        &post.text,
                                        post.published_at,
                                        now,
                                    )
                                    .await
                                {
                                    Ok(result) => {
                                        stats.llm_calls += 1;
                                        let cache_key = analysis_cache_key(
                                            &post.id,
                                            &result.source_digest,
                                            &config.model,
                                            &image_prompt_version,
                                        );
                                        self.store
                                            .save_image_analysis(
                                                &cache_key,
                                                &post.id,
                                                image_url,
                                                &config.model,
                                                &image_prompt_version,
                                                &result.raw,
                                            )
                                            .await?;
                                        self.store
                                            .save_image_analysis_cache_key(
                                                &post.id,
                                                image_url,
                                                &config.model,
                                                &image_prompt_version,
                                                &cache_key,
                                            )
                                            .await?;
                                        CachedImageAnalysis {
                                            events: result.events,
                                            source_digest: result.source_digest,
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(%error, post_id = %post.id, "image analysis failed");
                                        for target in targets {
                                            let _ = self
                                                .store
                                                .record_streamer_error(
                                                    target.id,
                                                    &format!("图片日程分析失败: {error}"),
                                                )
                                                .await;
                                        }
                                        completed_steps += 1;
                                        continue;
                                    }
                                }
                            }
                        };
                        let digest = analysis.source_digest.clone();
                        let events = analysis.events.clone();
                        analysis_cache.insert(image_cache_key, analysis);
                        for (event_index, event) in events.iter().enumerate() {
                            let event = canonicalize_event_participants(event, targets);
                            if let Some(target) = match_event_target(&event, targets) {
                                if event.action == LlmEventAction::Cancel {
                                    event_state.cancellations.push(PendingCancellation {
                                        target,
                                        event: event.clone(),
                                        source_post_id: post.id.clone(),
                                        published_at: post.published_at,
                                    });
                                    continue;
                                }
                                let fingerprint = format!(
                                    "{}:{}:{}:{}",
                                    source_fingerprint(post, &event.title, "image"),
                                    event_index,
                                    event.performer,
                                    event.participant_names().join("|")
                                );
                                let identity = format!("image:{}:{digest}:{event_index}", post.id);
                                let (inserted, cancelled) = self
                                    .apply_llm_event(
                                        &target,
                                        targets,
                                        &event,
                                        "image",
                                        post,
                                        image_url,
                                        &identity,
                                        &fingerprint,
                                    )
                                    .await?;
                                if inserted {
                                    stats.new_schedules += 1;
                                }
                                if cancelled {
                                    stats.cancelled_schedules += 1;
                                }
                            }
                        }
                        completed_steps += 1;
                    }
                }
            }
            completed_steps += 1;
            report_step("写入日程结果", completed_steps, total_steps, false);
        }
        Ok(stats)
    }

    async fn dedupe_group_schedules(&self) -> AppResult<usize> {
        let mut schedules = self.store.list_all_schedules().await?;
        schedules.sort_by_key(|schedule| (!schedule.manual_locked, schedule.id));
        let mut removed_ids = HashSet::new();
        let mut removed = 0;
        for index in 0..schedules.len() {
            if removed_ids.contains(&schedules[index].id) || schedules[index].participants.len() < 2
            {
                continue;
            }
            let mut base = schedules[index].clone();
            for duplicate in schedules.iter().skip(index + 1) {
                if removed_ids.contains(&duplicate.id)
                    || duplicate.participants.len() < 2
                    || (base.manual_locked && duplicate.manual_locked)
                    || !group_schedules_equivalent(&base, duplicate)
                {
                    continue;
                }
                let duplicate_input = schedule_to_new_input(duplicate);
                if let Err(error) = self
                    .store
                    .attach_schedule_source(base.id, &duplicate_input)
                    .await
                {
                    tracing::warn!(%error, "attach duplicate group source failed");
                }
                if !base.manual_locked {
                    let merged = merge_group_schedule(&base, &duplicate_input);
                    if let Err(error) = self.store.merge_schedule_auto(base.id, &merged).await {
                        tracing::warn!(%error, "merge duplicate group schedule failed");
                    } else if let Ok(updated) = self.store.get_schedule(base.id).await {
                        base = updated;
                    }
                }
                if let Err(error) = self.store.delete_schedule(duplicate.id).await {
                    tracing::warn!(%error, "delete duplicate group schedule failed");
                } else {
                    removed_ids.insert(duplicate.id);
                    removed += 1;
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
            .await;
        Ok((inserted, false))
    }

    async fn resolve_event_room(
        &self,
        event: &LlmEvent,
        target: &Streamer,
        targets: &[Streamer],
        source_mid: &str,
    ) -> ResolvedRoom {
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

    async fn remove_stale_model_schedules_for_reserved_post(
        &self,
        post: &DynamicPost,
        targets: &[Streamer],
    ) -> AppResult<usize> {
        let target_ids = targets
            .iter()
            .map(|target| target.id)
            .collect::<HashSet<_>>();
        let mut removed = 0;
        for schedule in self.store.list_all_schedules().await? {
            if schedule.manual_locked
                || !target_ids.contains(&schedule.streamer_id)
                || schedule.source_post_id.as_deref() != Some(post.id.as_str())
                || !matches!(schedule.source_kind.as_str(), "text" | "image")
            {
                continue;
            }
            self.store.delete_schedule(schedule.id).await?;
            removed += 1;
        }
        Ok(removed)
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
    ) -> bool {
        if post.reserve.is_some() && matches!(source_kind, "text" | "image") {
            return false;
        }
        if self
            .store
            .is_source_suppressed(target.id, &post.id, source_fingerprint)
            .await
            .unwrap_or(false)
        {
            return false;
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
            source_image_url: (source_kind == "image").then(|| image_url.to_owned()),
            source_author_mid: Some(post.author_mid.clone()),
            source_fingerprint: Some(source_fingerprint.to_owned()),
            source_identity: Some(source_identity.to_owned()),
            source_published_at: Some(post.published_at),
            confidence: schedule.confidence,
        };
        if participants.len() > 1 || source_kind == "reserve" {
            if let Ok(candidates) = self
                .store
                .list_schedules_near_all(schedule.start_at, 10)
                .await
            {
                let mut duplicates = group_duplicates(
                    &candidates,
                    schedule,
                    participants,
                    room_owner_mid,
                    room_url,
                    source_kind,
                );
                if !duplicates.is_empty() {
                    duplicates.sort_by_key(|schedule| (!schedule.manual_locked, schedule.id));
                    let existing = duplicates.remove(0);
                    let mut base = existing.clone();
                    for duplicate in duplicates {
                        let duplicate_input = schedule_to_new_input(duplicate);
                        if let Err(error) = self
                            .store
                            .attach_schedule_source(base.id, &duplicate_input)
                            .await
                        {
                            tracing::warn!(%error, streamer_id = target.id, "attach duplicate group source failed");
                        }
                        if !base.manual_locked {
                            let merged = merge_group_schedule(&base, &duplicate_input);
                            if let Err(error) =
                                self.store.merge_schedule_auto(base.id, &merged).await
                            {
                                tracing::warn!(%error, streamer_id = target.id, "merge duplicate group schedule failed");
                            } else if let Ok(updated) = self.store.get_schedule(base.id).await {
                                base = updated;
                            }
                        }
                        if let Err(error) = self.store.delete_schedule(duplicate.id).await {
                            tracing::warn!(%error, streamer_id = target.id, "delete duplicate group schedule failed");
                        }
                    }
                    if let Err(error) = self.store.attach_schedule_source(base.id, &input).await {
                        tracing::warn!(%error, streamer_id = target.id, "attach group source failed");
                    }
                    if !base.manual_locked {
                        let merged = merge_group_schedule(&base, &input);
                        if let Err(error) = self.store.merge_schedule_auto(base.id, &merged).await {
                            tracing::warn!(%error, streamer_id = target.id, "merge group schedule failed");
                        }
                    }
                    return false;
                }
            }
        }
        if let Ok(Some(existing)) = self
            .store
            .find_schedule_near(target.id, schedule.start_at, 10)
            .await
        {
            if existing.source_identity.as_deref() != Some(source_identity) {
                let incompatible_reservation = (existing.source_kind == "reserve"
                    || input.source_kind == "reserve")
                    && !cancellation_titles_match(&existing.title, &input.title);
                if !incompatible_reservation {
                    if let Err(error) = self.store.attach_schedule_source(existing.id, &input).await
                    {
                        tracing::warn!(%error, streamer_id = target.id, "attach schedule source failed");
                    }
                    if existing.weekly_rule_id.is_some()
                        && normalize_alias(&existing.title) != normalize_alias(&schedule.title)
                    {
                        let _ = self.store.mark_schedule_source_changed(existing.id).await;
                    }
                    let incoming_wins = match existing.source_published_at {
                        Some(existing_published_at) => post.published_at >= existing_published_at,
                        None => schedule.confidence >= existing.confidence,
                    };
                    if !existing.manual_locked && incoming_wins {
                        let merged = merge_input_for_existing(&existing, &input);
                        if let Err(error) =
                            self.store.merge_schedule_auto(existing.id, &merged).await
                        {
                            tracing::warn!(%error, streamer_id = target.id, "merge newer schedule failed");
                        }
                        return false;
                    }
                    return false;
                }
            }
        }
        match self.store.upsert_schedule(&input).await {
            Ok((_, inserted)) => inserted,
            Err(error) => {
                tracing::warn!(%error, streamer_id = target.id, "upsert schedule failed");
                false
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_progress(
    streamer: &Streamer,
    stage: &str,
    completed: usize,
    total: usize,
    percent: u8,
    done: bool,
    failed: bool,
    on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
) {
    on_progress(RefreshProgress {
        streamer_id: streamer.id,
        streamer_name: streamer.name.clone(),
        stage: stage.to_owned(),
        completed,
        total,
        percent: percent.min(100),
        done,
        failed,
    });
}

#[derive(Debug, Clone)]
struct CachedImageAnalysis {
    events: Vec<LlmEvent>,
    source_digest: String,
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
}

#[derive(Debug, Default)]
struct ProcessStats {
    new_schedules: usize,
    cancelled_schedules: usize,
    llm_calls: usize,
    llm_cache_hits: usize,
}

fn is_asoul_member(mid: &str) -> bool {
    A_SOUL_MEMBER_MIDS.contains(&mid)
}

/// Group targets by source MID so a shared official account is fetched once per refresh.
fn sources_for_streamers(streamers: &[Streamer]) -> Vec<(String, Vec<Streamer>)> {
    let mut source_targets: HashMap<String, Vec<Streamer>> = HashMap::new();
    for streamer in streamers
        .iter()
        .filter(|streamer| streamer.schedule_enabled)
    {
        push_unique_target(&mut source_targets, &streamer.mid, streamer);
        for linked in &streamer.linked_accounts {
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
    streamer.mid == query
        || normalize_search_value(&streamer.name) == query
        || streamer
            .aliases
            .iter()
            .any(|alias| normalize_search_value(alias) == query)
}

fn account_from_streamer(streamer: &Streamer) -> crate::models::AccountSearchResult {
    crate::models::AccountSearchResult {
        mid: streamer.mid.clone(),
        name: streamer.name.clone(),
        avatar_url: streamer.avatar_url.clone(),
        sign: String::new(),
        room_id: streamer.room_id.clone(),
        fans: 0,
        is_live: streamer.is_live,
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
        .any(|streamer| streamer.mid == account.mid);
    let alias_exact = subscriptions
        .iter()
        .filter(|streamer| streamer.mid == account.mid)
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
    results.retain(|account| seen.insert(account.mid.clone()));
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

fn match_event_target(event: &LlmEvent, targets: &[Streamer]) -> Option<Streamer> {
    let participants = event.participant_names();
    if participants.len() <= 1 {
        let event_names: Vec<String> = std::iter::once(&event.performer)
            .chain(event.aliases.iter())
            .map(|value| normalize_alias(value))
            .filter(|value| !value.is_empty())
            .collect();
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

fn group_schedules_equivalent(left: &Schedule, right: &Schedule) -> bool {
    let minutes = (left.start_at - right.start_at).num_minutes().abs();
    if minutes > 10 {
        return false;
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
    let strong_overlap = shared >= 2 && shared * 2 >= min_participants;
    let same_room_owner =
        left.room_owner_mid.is_some() && left.room_owner_mid == right.room_owner_mid;
    let same_room_url = left.room_url.is_some() && left.room_url == right.room_url;
    strong_overlap || ((same_room_owner || same_room_url) && shared >= 1)
}

fn group_duplicates<'a>(
    candidates: &'a [Schedule],
    incoming: &ParsedSchedule,
    participants: &[String],
    room_owner_mid: Option<&str>,
    room_url: Option<&str>,
    incoming_source_kind: &str,
) -> Vec<&'a Schedule> {
    let incoming_names: Vec<String> = participants
        .iter()
        .map(|name| normalize_alias(name))
        .collect();
    candidates
        .iter()
        .filter(|candidate| {
            let minutes = (candidate.start_at - incoming.start_at).num_minutes().abs();
            if minutes > 10 {
                return false;
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
            let strong_overlap = shared >= 2 && shared * 2 >= min_participants;
            let same_room_owner =
                room_owner_mid.is_some_and(|mid| candidate.room_owner_mid.as_deref() == Some(mid));
            let same_room_url =
                room_url.is_some_and(|url| candidate.room_url.as_deref() == Some(url));
            let same_room = same_room_owner || same_room_url;
            // A reservation often has no participant list, while the same live can
            // later be announced as a group text by another account. Same room and
            // an almost identical start time are the strong signal in that case.
            let reservation_same_room = (candidate.source_kind == "reserve"
                || incoming_source_kind == "reserve")
                && same_room
                && minutes <= 1;
            strong_overlap || (same_room && shared >= 1) || reservation_same_room
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
    let keep_existing_reserve =
        existing.source_kind == "reserve" && incoming.source_kind != "reserve";
    let incoming_reserve = incoming.source_kind == "reserve" && existing.source_kind != "reserve";
    let title = if keep_existing_reserve {
        existing.title.clone()
    } else if incoming_reserve || incoming.title.chars().count() > existing.title.chars().count() {
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
        start_at: if incoming_wins {
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
        source_kind: if keep_existing_reserve {
            existing.source_kind.clone()
        } else {
            incoming.source_kind.clone()
        },
        source_post_id: if keep_existing_reserve {
            existing.source_post_id.clone()
        } else {
            incoming.source_post_id.clone()
        },
        source_image_url: if keep_existing_reserve {
            existing.source_image_url.clone()
        } else {
            incoming.source_image_url.clone()
        },
        source_author_mid: if keep_existing_reserve {
            existing.source_author_mid.clone()
        } else {
            incoming.source_author_mid.clone()
        },
        source_fingerprint: if keep_existing_reserve {
            existing.source_fingerprint.clone()
        } else {
            incoming.source_fingerprint.clone()
        },
        source_identity: if keep_existing_reserve {
            existing.source_identity.clone()
        } else {
            incoming.source_identity.clone()
        },
        source_published_at: if keep_existing_reserve {
            existing.source_published_at
        } else {
            source_published_at
        },
        confidence: existing.confidence.max(incoming.confidence),
    }
}

fn merge_input_for_existing(existing: &Schedule, incoming: &NewSchedule) -> NewSchedule {
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

fn cache_key_digest(cache_key: &str) -> String {
    cache_key.split(':').nth(1).unwrap_or("unknown").to_owned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use chrono::{Duration, FixedOffset};
    use serde_json::Value;

    use crate::llm::LlmAnalysis;
    use crate::models::ReserveCard;

    struct CountingAnalyzer {
        calls: AtomicUsize,
        text_calls: AtomicUsize,
        events: Vec<LlmEvent>,
        text_events: Vec<LlmEvent>,
    }

    #[async_trait]
    impl ContentAnalyzer for CountingAnalyzer {
        async fn analyze_image_url(
            &self,
            _config: &LlmConfig,
            _image_url: &str,
            _context_text: &str,
            _published_at: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> AppResult<LlmAnalysis> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let raw = serde_json::to_value(&self.events)?;
            Ok(LlmAnalysis {
                events: self.events.clone(),
                raw,
                source_digest: "mock-digest".to_owned(),
            })
        }

        async fn analyze_text(
            &self,
            _config: &LlmConfig,
            _text: &str,
            _published_at: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> AppResult<LlmAnalysis> {
            self.text_calls.fetch_add(1, Ordering::SeqCst);
            let raw = serde_json::to_value(&self.text_events)?;
            Ok(LlmAnalysis {
                events: self.text_events.clone(),
                raw,
                source_digest: "mock-text-digest".to_owned(),
            })
        }
    }

    fn subscription(mid: &str, name: &str) -> AddSubscription {
        AddSubscription {
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
            text_calls: AtomicUsize::new(0),
            events: Vec::new(),
            text_events: Vec::new(),
        });
        let service = Arc::new(
            AppService::with_analyzer(store, RuntimeSecrets::default(), analyzer).expect("service"),
        );
        let first = service.acquire_refresh_lock().await;
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queued_service = service.clone();
        let queued_acquired = acquired.clone();
        let queued = tokio::spawn(async move {
            let _second = queued_service.acquire_refresh_lock().await;
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
                last_error: None,
                last_refresh_at: None,
                is_live: false,
                live_title: None,
                live_cover: None,
                live_started_at: None,
                live_checked_at: None,
                linked_accounts: Vec::new(),
                sort_order: 0,
                revision: 1,
                created_at: now,
            },
            Streamer {
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
                last_error: None,
                last_refresh_at: None,
                is_live: false,
                live_title: None,
                live_cover: None,
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
    async fn image_and_text_group_events_merge_across_owner_accounts() {
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
            text_calls: AtomicUsize::new(0),
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
        assert!(
            service
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
        );
        let short = ParsedSchedule {
            title: "书梦之旅".to_owned(),
            ..schedule
        };
        assert!(
            !service
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
        );
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].title, "心宜思诺周年纪念直播·《执笔入梦》");
        assert_eq!(
            schedules[0].room_owner_name.as_deref(),
            Some("心宜不是心仪")
        );
        assert_eq!(schedules[0].participants, participants);
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
            text_calls: AtomicUsize::new(0),
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
        assert!(
            service
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
        );

        let mut cache = HashMap::new();
        let stats = service
            .process_source_posts(
                "402",
                std::slice::from_ref(&sinuo),
                std::slice::from_ref(&reserve_post),
                &AppSettings::default(),
                None,
                &mut cache,
            )
            .await
            .expect("process reservation");

        assert_eq!(stats.llm_calls, 0);
        assert_eq!(analyzer.calls.load(Ordering::SeqCst), 0);
        assert_eq!(analyzer.text_calls.load(Ordering::SeqCst), 0);
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
            text_calls: AtomicUsize::new(0),
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
            last_error: None,
            last_refresh_at: None,
            is_live: false,
            live_title: None,
            live_cover: None,
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
            last_error: None,
            last_refresh_at: None,
            is_live: false,
            live_title: None,
            live_cover: None,
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
            last_error: None,
            last_refresh_at: None,
            is_live: false,
            live_title: None,
            live_cover: None,
            live_started_at: None,
            live_checked_at: None,
            linked_accounts: Vec::new(),
            sort_order: 0,
            revision: 1,
            created_at: now,
        };
        assert!(search_matches_subscription("@然然", &streamer));
        let account = |mid: &str, name: &str, fans: u64| crate::models::AccountSearchResult {
            mid: mid.to_owned(),
            name: name.to_owned(),
            avatar_url: String::new(),
            sign: String::new(),
            room_id: None,
            fans,
            is_live: false,
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
            text_calls: AtomicUsize::new(0),
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

        assert!(
            service
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
        );
        assert!(
            !service
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
        );
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

        assert!(
            !service
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
        );
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
    async fn source_processing_reports_real_stage_progress() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            text_calls: AtomicUsize::new(0),
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
        let mut cache = HashMap::new();
        let mut event_state = RefreshEventState::default();
        let llm_config = LlmConfig {
            base_url: "https://fixture.invalid/v1".to_owned(),
            api_key: String::new(),
            model: "fixture-model".to_owned(),
            prompt_version: "fixture-v1".to_owned(),
        };
        service
            .process_source_posts_with_progress(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&llm_config),
                &mut cache,
                &mut event_state,
                Some(&streamer),
                &move |progress| captured.lock().expect("progress").push(progress),
                0,
                1,
            )
            .await
            .expect("process");
        let reports = reports.lock().expect("reports");
        assert!(reports.len() >= 3);
        assert!(reports
            .windows(2)
            .all(|pair| pair[0].percent <= pair[1].percent));
        assert!(reports.iter().any(|report| report.stage == "写入日程结果"));
        assert!(reports
            .iter()
            .any(|report| report.stage.starts_with("识别图片日程")));
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
            text_calls: AtomicUsize::new(0),
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
        let config = LlmConfig {
            base_url: "https://fixture.invalid/v1".to_owned(),
            api_key: String::new(),
            model: "fixture-model".to_owned(),
            prompt_version: "fixture-v1".to_owned(),
        };
        let mut cache = HashMap::new();
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&config),
                &mut cache,
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
    async fn reserved_post_does_not_run_model_for_cancellation_text() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let published_at = Utc::now();
        let (old_schedule, _) = store
            .upsert_schedule(&NewSchedule {
                streamer_id: streamer.id,
                participants: Vec::new(),
                title: "【突击】旧场".to_owned(),
                start_at: published_at + Duration::days(1),
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
                start_at: published_at + Duration::days(1) + Duration::hours(1),
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
            text_calls: AtomicUsize::new(0),
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
            reserve: Some(ReserveCard {
                title: "直播预约：【突击】电台电台！".to_owned(),
                description: "明天 19:00 直播".to_owned(),
                room_id: Some("22637261".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let config = LlmConfig {
            base_url: "https://fixture.invalid/v1".to_owned(),
            api_key: String::new(),
            model: "fixture-model".to_owned(),
            prompt_version: "fixture-v1".to_owned(),
        };
        let mut cache = HashMap::new();
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                Some(&config),
                &mut cache,
            )
            .await
            .expect("process");

        assert_eq!(stats.new_schedules, 1);
        assert_eq!(stats.cancelled_schedules, 0);
        assert_eq!(analyzer.text_calls.load(Ordering::SeqCst), 0);
        assert!(store.get_schedule(old_schedule.id).await.is_ok());
        assert!(store.get_schedule(old_song.id).await.is_ok());
        let remaining = store.list_all_schedules().await.expect("remaining");
        assert_eq!(remaining.len(), 3);
        assert!(remaining.iter().any(|schedule| {
            schedule.source_post_id.as_deref() == Some("current-post")
                && schedule.source_kind == "reserve"
                && schedule.title == "【突击】电台电台！"
        }));
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
            text_calls: AtomicUsize::new(0),
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
        let config = LlmConfig {
            base_url: "https://example.com/v1".to_owned(),
            api_key: String::new(),
            model: "mock-vision".to_owned(),
            prompt_version: "v1".to_owned(),
        };
        let mut cache = HashMap::new();
        let stats = service
            .process_source_posts(
                "900",
                &targets,
                std::slice::from_ref(&post),
                &settings,
                Some(&config),
                &mut cache,
            )
            .await
            .expect("process");
        assert_eq!(analyzer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(analyzer.text_calls.load(Ordering::SeqCst), 0);
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
            text_calls: AtomicUsize::new(0),
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
        let mut cache = HashMap::new();
        let settings = AppSettings {
            llm_model: "mock-text".to_owned(),
            ..Default::default()
        };
        let config = LlmConfig {
            base_url: "https://example.com/v1".to_owned(),
            api_key: String::new(),
            model: "mock-text".to_owned(),
            prompt_version: "v1".to_owned(),
        };
        let stats = service
            .process_source_posts(
                "900",
                &targets,
                std::slice::from_ref(&post),
                &settings,
                Some(&config),
                &mut cache,
            )
            .await
            .expect("process");
        assert_eq!(stats.new_schedules, 2);
        assert_eq!(analyzer.text_calls.load(Ordering::SeqCst), 1);
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
        let mut second_cache = HashMap::new();
        let second_stats = service
            .process_source_posts(
                "900",
                &targets,
                std::slice::from_ref(&post),
                &settings,
                Some(&config),
                &mut second_cache,
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
            text_calls: AtomicUsize::new(0),
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
        assert!(
            service
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
        );
        assert!(
            !service
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
        );
        let schedules = store
            .list_schedules(now, now + Duration::days(3), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].title, "周五杂谈回");
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
            text_calls: AtomicUsize::new(0),
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
        assert!(
            service
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
        );

        let mut cache = HashMap::new();
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[post],
                &AppSettings::default(),
                None,
                &mut cache,
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
    async fn reserved_dynamics_skip_models_and_remove_stale_model_schedules() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let now = Utc::now();
        let text_post = DynamicPost {
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
        let image_post = DynamicPost {
            id: "reserved-image-post".to_owned(),
            author_mid: streamer.mid.clone(),
            author_name: streamer.name.clone(),
            published_at: now,
            text: "图片里也有安排".to_owned(),
            images: vec!["https://i0.hdslb.com/test-reserved-calendar.png".to_owned()],
            source_url: "https://t.bilibili.com/reserved-image-post".to_owned(),
            raw_kind: "DYNAMIC_TYPE_DRAW".to_owned(),
            reserve: Some(ReserveCard {
                title: "直播预约：周末杂谈".to_owned(),
                description: "后天 20:00 直播".to_owned(),
                room_id: Some("22637261".to_owned()),
                jump_url: None,
                stype: Some(2),
            }),
        };
        let first_reservation = parse_reserve_schedule(
            text_post.reserve.as_ref().expect("reserve"),
            text_post.published_at,
            now,
        )
        .expect("reservation schedule");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            text_calls: AtomicUsize::new(0),
            events: vec![LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "甲".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "图片日程".to_owned(),
                start_at: Some(now + Duration::days(2)),
                end_at: None,
                confidence: 0.95,
            }],
            text_events: vec![LlmEvent {
                action: LlmEventAction::Schedule,
                performer: "甲".to_owned(),
                aliases: Vec::new(),
                participants: Vec::new(),
                room_url: None,
                room_owner: None,
                room_owner_mid: None,
                title: "文字日程".to_owned(),
                start_at: Some(first_reservation.start_at),
                end_at: None,
                confidence: 0.95,
            }],
        });
        let service =
            AppService::with_analyzer(store.clone(), RuntimeSecrets::default(), analyzer.clone())
                .expect("service");
        let stale_post = DynamicPost {
            reserve: None,
            ..text_post.clone()
        };
        let stale_text = ParsedSchedule {
            title: "旧文字日程".to_owned(),
            start_at: first_reservation.start_at,
            end_at: None,
            confidence: 0.95,
        };
        assert!(
            service
                .upsert_candidate(
                    &streamer,
                    &stale_text,
                    "text",
                    &stale_post,
                    "editor",
                    "text:reserved-text-post:0",
                    "old-text-fingerprint",
                    None,
                    None,
                    None,
                    &[],
                )
                .await
        );

        let config = LlmConfig {
            base_url: "https://fixture.invalid/v1".to_owned(),
            api_key: String::new(),
            model: "fixture-model".to_owned(),
            prompt_version: "fixture-v1".to_owned(),
        };
        let mut cache = HashMap::new();
        let stats = service
            .process_source_posts(
                "100",
                std::slice::from_ref(&streamer),
                &[text_post, image_post],
                &AppSettings::default(),
                Some(&config),
                &mut cache,
            )
            .await
            .expect("process");

        assert_eq!(stats.llm_calls, 0);
        assert_eq!(analyzer.calls.load(Ordering::SeqCst), 0);
        assert_eq!(analyzer.text_calls.load(Ordering::SeqCst), 0);
        assert_eq!(stats.new_schedules, 2);
        assert_eq!(stats.cancelled_schedules, 1);
        let schedules = store
            .list_schedules(now - Duration::hours(1), now + Duration::days(4), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 2);
        assert!(schedules
            .iter()
            .all(|schedule| schedule.source_kind == "reserve"));
        assert!(schedules
            .iter()
            .all(|schedule| schedule.title != "旧文字日程"));
    }

    #[tokio::test]
    async fn llm_schedule_does_not_override_reservation_title() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&subscription("100", "甲"))
            .await
            .expect("streamer");
        let analyzer = Arc::new(CountingAnalyzer {
            calls: AtomicUsize::new(0),
            text_calls: AtomicUsize::new(0),
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
        assert!(
            service
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
        );

        let llm_schedule = ParsedSchedule {
            title: "电台".to_owned(),
            start_at: reservation.start_at,
            end_at: None,
            confidence: 0.95,
        };
        assert!(
            !service
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
        );

        let schedules = store
            .list_schedules(now, now + Duration::days(2), false)
            .await
            .expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].title, "【突击】电台电台！");
        assert_eq!(schedules[0].source_kind, "reserve");
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
            text_calls: AtomicUsize::new(0),
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
}
