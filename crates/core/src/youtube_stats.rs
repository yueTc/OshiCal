use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio::time::{interval, timeout};

use crate::models::{LiveSession, SuperChatEventInput};
use crate::settings::RuntimeSecrets;
use crate::store::SqliteStore;
use crate::youtube::{YouTubeClient, YouTubeLiveChatMessage, YouTubeSuperChatEvent};

const SAMPLE_INTERVAL_SECONDS: u64 = 30;
const STOP_TIMEOUT_SECONDS: u64 = 8;
const LIVE_CHAT_PAGE_SIZE: u32 = 200;
const MAX_LIVE_CHAT_PAGES_PER_TICK: usize = 20;
const SUPER_CHAT_PAGE_SIZE: u32 = 50;
const MAX_SUPER_CHAT_PAGES_PER_TICK: u32 = 10;

#[derive(Clone, Default)]
pub(crate) struct YouTubeStatsManager {
    tasks: Arc<Mutex<HashMap<i64, TaskHandle>>>,
}

struct TaskHandle {
    stop: Option<oneshot::Sender<DateTime<Utc>>>,
    handle: tokio::task::JoinHandle<()>,
    session_id: i64,
    video_id: Option<String>,
}

impl YouTubeStatsManager {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        &self,
        store: SqliteStore,
        client: Arc<YouTubeClient>,
        secrets: Arc<RwLock<RuntimeSecrets>>,
        streamer_id: i64,
        session: LiveSession,
        video_id: Option<String>,
        live_chat_id: Option<String>,
        initial_viewers: u64,
        started_at: DateTime<Utc>,
    ) {
        let mut tasks = self.tasks.lock().await;
        if let Some(existing) = tasks.get(&streamer_id) {
            if existing.session_id == session.id
                && existing.video_id == video_id
                && !existing.handle.is_finished()
            {
                return;
            }
        }
        if let Some(mut previous) = tasks.remove(&streamer_id) {
            if let Some(stop) = previous.stop.take() {
                let _ = stop.send(Utc::now());
            }
            previous.handle.abort();
        }
        let session_id = session.id;
        let task_video_id = video_id.clone();
        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = tokio::spawn(run_collector(
            store,
            client,
            secrets,
            session,
            video_id,
            live_chat_id,
            initial_viewers,
            started_at,
            stop_rx,
        ));
        tasks.insert(
            streamer_id,
            TaskHandle {
                stop: Some(stop_tx),
                handle,
                session_id,
                video_id: task_video_id,
            },
        );
    }

    pub(crate) async fn stop(&self, streamer_id: i64, ended_at: DateTime<Utc>) {
        let task = self.tasks.lock().await.remove(&streamer_id);
        let Some(mut task) = task else {
            return;
        };
        if let Some(stop) = task.stop.take() {
            let _ = stop.send(ended_at);
        }
        if timeout(Duration::from_secs(STOP_TIMEOUT_SECONDS), &mut task.handle)
            .await
            .is_err()
        {
            task.handle.abort();
        }
    }
}

#[derive(Default)]
struct CollectorMetrics {
    peak_online: u64,
    online_total: u64,
    online_samples: u64,
    danmaku_count: u64,
    super_chat_count: u64,
    membership_count: u64,
    paid_users: HashSet<String>,
    interactive_users: HashSet<String>,
    seen_chat_message_ids: HashSet<String>,
    seen_paid_event_ids: HashSet<String>,
}

impl CollectorMetrics {
    fn observe_viewers(&mut self, viewers: u64) {
        self.peak_online = self.peak_online.max(viewers);
        self.online_total = self.online_total.saturating_add(viewers);
        self.online_samples = self.online_samples.saturating_add(1);
    }

    async fn observe_chat_message(
        &mut self,
        store: &SqliteStore,
        session_id: i64,
        message: YouTubeLiveChatMessage,
    ) {
        if self.seen_chat_message_ids.len() >= 100_000
            || !self.seen_chat_message_ids.insert(message.id.clone())
        {
            return;
        }
        if message.display_message.is_some() {
            self.danmaku_count = self.danmaku_count.saturating_add(1);
        }
        if let Some(author) = message.author_channel_id.as_ref() {
            if self.interactive_users.len() < 100_000 {
                self.interactive_users.insert(author.clone());
            }
        }
        if let Some(event) = message.monetary_event.as_ref() {
            let input = SuperChatEventInput {
                id: message.id.clone(),
                kind: event.kind.clone(),
                live_chat_id: Some(message.live_chat_id.clone())
                    .filter(|value| !value.trim().is_empty()),
                created_at: message.published_at,
                user_channel_id: message.author_channel_id.clone(),
                user_display_name: message.author_display_name.clone(),
                amount_micros: event.amount_micros,
                currency: event.currency.clone(),
                amount_display_string: event.amount_display_string.clone(),
                tier: event.tier,
            };
            self.observe_super_chat(store, session_id, input).await;
        }
        if message.membership_event.is_some() {
            self.membership_count = self.membership_count.saturating_add(1);
        }
    }

    async fn observe_super_chat(
        &mut self,
        store: &SqliteStore,
        session_id: i64,
        event: SuperChatEventInput,
    ) {
        if self.seen_paid_event_ids.len() >= 100_000
            || !self.seen_paid_event_ids.insert(event.id.clone())
        {
            return;
        }
        let user = event.user_channel_id.clone();
        match store
            .record_youtube_super_chat_event(session_id, &event)
            .await
        {
            Ok(true) => {
                self.super_chat_count = self.super_chat_count.saturating_add(1);
                if let Some(user) = user {
                    if self.paid_users.len() < 100_000 {
                        self.paid_users.insert(user);
                    }
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(%error, session_id, "保存 YouTube Super Chat 事件失败");
            }
        }
    }

    async fn observe_super_chat_event(
        &mut self,
        store: &SqliteStore,
        session_id: i64,
        event: YouTubeSuperChatEvent,
    ) {
        self.observe_super_chat(
            store,
            session_id,
            SuperChatEventInput {
                id: event.id,
                kind: event.kind.clone(),
                live_chat_id: Some(event.live_chat_id)
                    .filter(|value| !value.trim().is_empty()),
                created_at: event.created_at,
                user_channel_id: event.user_channel_id,
                user_display_name: event.user_display_name,
                amount_micros: event.amount_micros,
                currency: event.currency,
                amount_display_string: event.amount_display_string,
                tier: event.tier,
            },
        )
        .await;
    }
}

async fn current_access_token(secrets: &RwLock<RuntimeSecrets>) -> Option<String> {
    secrets
        .read()
        .await
        .youtube_oauth_access_token
        .clone()
        .map(|value| value.replace(['\r', '\n'], "").trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn auth_failed(error: &crate::error::AppError) -> bool {
    matches!(error.code(), "YOUTUBE_UNAUTHORIZED" | "YOUTUBE_FORBIDDEN")
}

fn degrade_chat(
    chat_degraded: &mut bool,
    last_failed_token: &mut Option<String>,
    token: Option<&str>,
) {
    *chat_degraded = true;
    *last_failed_token = token.map(str::to_owned);
}

fn chat_is_available(
    token: Option<&str>,
    live_chat_id: Option<&str>,
    chat_degraded: bool,
) -> bool {
    token.is_some() && live_chat_id.is_some() && !chat_degraded
}

fn effective_stats_available(
    token: Option<&str>,
    chat_ever_succeeded: bool,
    chat_degraded: bool,
) -> bool {
    token.is_some() && chat_ever_succeeded && !chat_degraded
}

async fn finish_session(store: &SqliteStore, session: &LiveSession, ended_at: DateTime<Utc>) {
    if let Err(error) = store.finish_open_live_session(session.id, ended_at).await {
        tracing::debug!(%error, session_id = session.id, "结束 YouTube 直播场次失败");
    }
}

/// Persist the observations collected before the switch was turned off, then
/// stop this collector without claiming the live broadcast ended. The open
/// session can resume in place when YouTube is enabled again. OAuth failures
/// remain represented by `stats_available = false`, never fabricated zeroes.
#[allow(clippy::too_many_arguments)]
async fn stop_collector(
    store: &SqliteStore,
    session: &LiveSession,
    secrets: &Arc<RwLock<RuntimeSecrets>>,
    started_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    metrics: &CollectorMetrics,
    chat_ever_succeeded: bool,
    chat_degraded: bool,
) {
    let token = current_access_token(secrets.as_ref()).await;
    let stats_available =
        effective_stats_available(token.as_deref(), chat_ever_succeeded, chat_degraded);
    persist_metrics(
        store,
        session,
        started_at,
        observed_at,
        stats_available,
        "paused",
        metrics,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn stop_collector_if_youtube_disabled(
    store: &SqliteStore,
    session: &LiveSession,
    secrets: &Arc<RwLock<RuntimeSecrets>>,
    started_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    metrics: &CollectorMetrics,
    chat_ever_succeeded: bool,
    chat_degraded: bool,
) -> bool {
    if super::youtube_requests_enabled(store).await {
        return false;
    }
    stop_collector(
        store,
        session,
        secrets,
        started_at,
        observed_at,
        metrics,
        chat_ever_succeeded,
        chat_degraded,
    )
    .await;
    true
}

#[allow(clippy::too_many_arguments)]
async fn run_collector(
    store: SqliteStore,
    client: Arc<YouTubeClient>,
    secrets: Arc<RwLock<RuntimeSecrets>>,
    session: LiveSession,
    video_id: Option<String>,
    mut live_chat_id: Option<String>,
    initial_viewers: u64,
    started_at: DateTime<Utc>,
    mut stop_rx: oneshot::Receiver<DateTime<Utc>>,
) {
    let mut ticker = interval(Duration::from_secs(SAMPLE_INTERVAL_SECONDS));
    let mut metrics = CollectorMetrics::default();
    let mut next_chat_page_token: Option<String> = None;
    let mut observed_initial = false;
    let mut chat_degraded = false;
    let mut chat_ever_succeeded = false;
    let mut last_failed_token: Option<String> = None;

    loop {
        tokio::select! {
            ended_at = &mut stop_rx => {
                let ended_at = ended_at.unwrap_or_else(|_| Utc::now());
                let token = current_access_token(&secrets).await;
                let stats_available = effective_stats_available(
                    token.as_deref(),
                    chat_ever_succeeded,
                    chat_degraded,
                );
                persist_metrics(
                    &store,
                    &session,
                    started_at,
                    ended_at,
                    stats_available,
                    if stats_available { "active" } else { "degraded" },
                    &metrics,
                ).await;
                finish_session(&store, &session, ended_at).await;
                return;
            }
            _ = ticker.tick() => {
                let now = Utc::now();
                // Fail closed: a task started while enabled must stop making
                // YouTube requests as soon as the global switch is turned off.
                if stop_collector_if_youtube_disabled(
                    &store,
                    &session,
                    &secrets,
                    started_at,
                    now,
                    &metrics,
                    chat_ever_succeeded,
                    chat_degraded,
                )
                .await
                {
                    return;
                }

                let current_token = current_access_token(&secrets).await;
                if chat_degraded && current_token != last_failed_token {
                    chat_degraded = false;
                    last_failed_token = None;
                    next_chat_page_token = None;
                }
                let mut chat_available = chat_is_available(
                    current_token.as_deref(),
                    live_chat_id.as_deref(),
                    chat_degraded,
                );

                let mut current_viewers = None;
                let mut went_offline = false;
                if let Some(video_id) = video_id.as_deref() {
                    if stop_collector_if_youtube_disabled(
                        &store,
                        &session,
                        &secrets,
                        started_at,
                        now,
                        &metrics,
                        chat_ever_succeeded,
                        chat_degraded,
                    )
                    .await
                    {
                        return;
                    }
                    let Some(videos_result) = super::run_youtube_request(
                        &store,
                        client.fetch_videos(&[video_id.to_owned()]),
                    )
                    .await
                    else {
                        stop_collector(
                            &store,
                            &session,
                            &secrets,
                            started_at,
                            Utc::now(),
                            &metrics,
                            chat_ever_succeeded,
                            chat_degraded,
                        )
                        .await;
                        return;
                    };
                    match videos_result {
                        Ok(videos) => {
                            if let Some(video) = videos.into_iter().next() {
                                current_viewers = video.concurrent_viewers;
                                if let Some(chat_id) = video.active_live_chat_id.clone() {
                                    if live_chat_id.as_deref() != Some(chat_id.as_str()) {
                                        live_chat_id = Some(chat_id);
                                        next_chat_page_token = None;
                                    }
                                    chat_available = chat_is_available(
                                        current_token.as_deref(),
                                        live_chat_id.as_deref(),
                                        chat_degraded,
                                    );
                                }
                                if video.actual_end_at.is_some()
                                    || (!video.is_live() && !video.is_upcoming())
                                {
                                    went_offline = true;
                                }
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, session_id = session.id, "YouTube 直播观看人数采样失败");
                        }
                    }
                } else if !observed_initial {
                    current_viewers = Some(initial_viewers);
                }
                if current_viewers.is_none() && !observed_initial {
                    current_viewers = Some(initial_viewers);
                }
                if let Some(value) = current_viewers {
                    metrics.observe_viewers(value);
                    if let Ok(value) = i64::try_from(value) {
                        store
                            .record_live_online_sample(
                                session.id,
                                now,
                                Some((now, value)),
                                "active",
                            )
                            .await
                            .ok();
                    }
                }
                observed_initial = true;
                if stop_collector_if_youtube_disabled(
                    &store,
                    &session,
                    &secrets,
                    started_at,
                    now,
                    &metrics,
                    chat_ever_succeeded,
                    chat_degraded,
                )
                .await
                {
                    return;
                }

                if chat_available {
                    if let Some(chat_id) = live_chat_id.clone() {
                        // Keep the last usable cursor when a token refresh or a
                        // transient request error interrupts this tick. Each tick
                        // has a hard page budget and a per-tick token-cycle guard,
                        // so a malformed continuation cannot spin forever.
                        let mut page_token = next_chat_page_token.clone();
                        let mut seen_page_tokens = HashSet::new();
                        for _ in 0..MAX_LIVE_CHAT_PAGES_PER_TICK {
                            if stop_collector_if_youtube_disabled(
                                &store,
                                &session,
                                &secrets,
                                started_at,
                                now,
                                &metrics,
                                chat_ever_succeeded,
                                chat_degraded,
                            )
                            .await
                            {
                                return;
                            }
                            let Some(token) = current_access_token(&secrets).await else {
                                chat_available = false;
                                break;
                            };
                            if let Some(value) = page_token.as_deref() {
                                if !seen_page_tokens.insert(value.to_owned()) {
                                    break;
                                }
                            }
                            let Some(page_result) = super::run_youtube_request(
                                &store,
                                client.fetch_live_chat_messages(
                                    &chat_id,
                                    &token,
                                    page_token.as_deref(),
                                    LIVE_CHAT_PAGE_SIZE,
                                ),
                            )
                            .await
                            else {
                                stop_collector(
                                    &store,
                                    &session,
                                    &secrets,
                                    started_at,
                                    Utc::now(),
                                    &metrics,
                                    chat_ever_succeeded,
                                    chat_degraded,
                                )
                                .await;
                                return;
                            };
                            match page_result {
                                Ok(page) => {
                                    chat_ever_succeeded = true;
                                    chat_degraded = false;
                                    last_failed_token = None;
                                    let next = page.next_page_token;
                                    for message in page.messages {
                                        metrics
                                            .observe_chat_message(&store, session.id, message)
                                            .await;
                                    }
                                    let Some(next) = next else {
                                        next_chat_page_token = None;
                                        break;
                                    };
                                    if page_token.as_deref() == Some(next.as_str())
                                        || seen_page_tokens.contains(&next)
                                    {
                                        next_chat_page_token = Some(next);
                                        break;
                                    }
                                    next_chat_page_token = Some(next.clone());
                                    page_token = Some(next);
                                }
                                Err(error) => {
                                    tracing::debug!(
                                        %error,
                                        session_id = session.id,
                                        "YouTube 直播聊天采样失败"
                                    );
                                    if auth_failed(&error) {
                                        degrade_chat(
                                            &mut chat_degraded,
                                            &mut last_failed_token,
                                            Some(token.as_str()),
                                        );
                                    }
                                    break;
                                }
                            }
                        }
                    } else {
                        chat_available = false;
                    }
                }

                if stop_collector_if_youtube_disabled(
                    &store,
                    &session,
                    &secrets,
                    started_at,
                    now,
                    &metrics,
                    chat_ever_succeeded,
                    chat_degraded,
                )
                .await
                {
                    return;
                }
                if let (Some(chat_id), Some(token)) = (
                    live_chat_id.as_deref(),
                    current_access_token(&secrets).await,
                ) {
                    if !chat_degraded {
                        // `fetch_all_super_chat_events` follows continuation
                        // tokens. The surrounding request guard drops this future
                        // if the global switch turns off, so no later page starts.
                        let Some(events_result) = super::run_youtube_request(
                            &store,
                            client.fetch_all_super_chat_events(
                                chat_id,
                                &token,
                                None,
                                SUPER_CHAT_PAGE_SIZE,
                                MAX_SUPER_CHAT_PAGES_PER_TICK,
                            ),
                        )
                        .await
                        else {
                            stop_collector(
                                &store,
                                &session,
                                &secrets,
                                started_at,
                                Utc::now(),
                                &metrics,
                                chat_ever_succeeded,
                                chat_degraded,
                            )
                            .await;
                            return;
                        };
                        match events_result {
                            Ok(events) => {
                                for event in events {
                                    metrics
                                        .observe_super_chat_event(&store, session.id, event)
                                        .await;
                                }
                            }
                            Err(error) => {
                                tracing::debug!(
                                    %error,
                                    session_id = session.id,
                                    "YouTube Super Chat 补充采样失败"
                                );
                                if auth_failed(&error) {
                                    degrade_chat(
                                        &mut chat_degraded,
                                        &mut last_failed_token,
                                        Some(token.as_str()),
                                    );
                                }
                            }
                        }
                    }
                }

                let token = current_access_token(&secrets).await;
                let stats_available = effective_stats_available(
                    token.as_deref(),
                    chat_ever_succeeded,
                    chat_degraded,
                );
                persist_metrics(
                    &store,
                    &session,
                    started_at,
                    now,
                    stats_available,
                    if stats_available { "active" } else { "degraded" },
                    &metrics,
                ).await;
                if went_offline {
                    finish_session(&store, &session, now).await;
                    return;
                }
            }
        }
    }
}

async fn persist_metrics(
    store: &SqliteStore,
    session: &LiveSession,
    started_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    stats_available: bool,
    collector_state: &str,
    metrics: &CollectorMetrics,
) {
    let avg_online = if metrics.online_samples == 0 {
        0
    } else {
        metrics.online_total / metrics.online_samples
    };
    let duration_seconds = observed_at
        .signed_duration_since(started_at)
        .num_seconds()
        .max(0) as u64;
    if let Err(error) = store
        .update_youtube_live_metrics(
            session.id,
            metrics.peak_online,
            avg_online,
            metrics.online_samples,
            metrics.online_total,
            duration_seconds,
            stats_available.then_some(metrics.danmaku_count),
            stats_available.then_some(metrics.super_chat_count),
            stats_available.then_some(metrics.membership_count),
            stats_available.then_some(metrics.paid_users.len() as u64),
            stats_available.then_some(metrics.interactive_users.len() as u64),
            stats_available,
            "partial",
            collector_state,
        )
        .await
    {
        tracing::debug!(%error, session_id = session.id, "保存 YouTube 直播统计失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{save_settings, AppSettings};

    #[tokio::test]
    async fn youtube_switch_off_blocks_the_next_chat_page_request() {
        let store = SqliteStore::in_memory().await.expect("db");
        let mut settings = AppSettings::default();
        settings.youtube_enabled = true;
        save_settings(&store, &settings).await.expect("enable youtube");

        let mut pages_started = 0;
        for _ in 0..2 {
            if !super::super::youtube_requests_enabled(&store).await {
                break;
            }
            pages_started += 1;
            settings.youtube_enabled = false;
            save_settings(&store, &settings)
                .await
                .expect("disable youtube");
        }

        assert_eq!(pages_started, 1);
    }

    #[tokio::test]
    async fn youtube_switch_off_cancels_an_in_flight_paginated_request() {
        let store = SqliteStore::in_memory().await.expect("db");
        let mut settings = AppSettings::default();
        settings.youtube_enabled = true;
        save_settings(&store, &settings).await.expect("enable youtube");

        let request_store = store.clone();
        let paginated_request = async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut settings = AppSettings::default();
            settings.youtube_enabled = false;
            save_settings(&request_store, &settings)
                .await
                .expect("disable youtube");
            std::future::pending::<crate::error::AppResult<()>>().await
        };
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            super::super::run_youtube_request(&store, paginated_request),
        )
        .await
        .expect("switch-off should cancel the in-flight request");

        assert!(result.is_none());
    }
}
