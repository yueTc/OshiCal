use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
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
const MAX_TRACKED_EVENT_IDS: usize = 100_000;

type SharedCollectorResume = Arc<StdMutex<CollectorResumeState>>;

#[derive(Default)]
struct EventIdWindow {
    ids: HashSet<Arc<str>>,
    order: VecDeque<Arc<str>>,
}

impl EventIdWindow {
    fn remember(&mut self, id: &str) -> bool {
        let id = Arc::<str>::from(id);
        if !self.ids.insert(Arc::clone(&id)) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > MAX_TRACKED_EVENT_IDS {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(oldest.as_ref() as &str);
            }
        }
        true
    }

    fn forget(&mut self, id: &str) {
        self.ids.remove(id);
        self.order.retain(|candidate| candidate.as_ref() != id);
    }
}

struct CollectorResumeState {
    session_id: i64,
    video_id: Option<String>,
    chat_id: Option<String>,
    chat_page_token: Option<String>,
    super_chat_page_token: Option<String>,
    seen_chat_message_ids: EventIdWindow,
    seen_paid_event_ids: EventIdWindow,
    paid_users: HashSet<String>,
    interactive_users: HashSet<String>,
}

impl CollectorResumeState {
    fn new(session_id: i64, video_id: Option<String>) -> Self {
        Self {
            session_id,
            video_id,
            chat_id: None,
            chat_page_token: None,
            super_chat_page_token: None,
            seen_chat_message_ids: EventIdWindow::default(),
            seen_paid_event_ids: EventIdWindow::default(),
            paid_users: HashSet::new(),
            interactive_users: HashSet::new(),
        }
    }
}

fn lock_resume(resume: &SharedCollectorResume) -> std::sync::MutexGuard<'_, CollectorResumeState> {
    resume
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn resume_state_matches(
    state: &CollectorResumeState,
    session_id: i64,
    video_id: Option<&str>,
) -> bool {
    state.session_id == session_id && state.video_id.as_deref() == video_id
}

fn bind_chat_id(resume: &SharedCollectorResume, chat_id: &str) {
    let mut state = lock_resume(resume);
    if state.chat_id.as_deref() != Some(chat_id) {
        state.chat_id = Some(chat_id.to_owned());
        state.chat_page_token = None;
        state.super_chat_page_token = None;
    }
}

fn set_chat_page_token(resume: &SharedCollectorResume, chat_id: &str, page_token: Option<String>) {
    let mut state = lock_resume(resume);
    if state.chat_id.as_deref() == Some(chat_id) {
        state.chat_page_token = page_token;
    }
}

fn set_super_chat_page_token(
    resume: &SharedCollectorResume,
    chat_id: &str,
    page_token: Option<String>,
) {
    let mut state = lock_resume(resume);
    if state.chat_id.as_deref() == Some(chat_id) {
        state.super_chat_page_token = page_token;
    }
}

fn resume_page_tokens(
    resume: &SharedCollectorResume,
    chat_id: &str,
) -> (Option<String>, Option<String>) {
    let state = lock_resume(resume);
    if state.chat_id.as_deref() == Some(chat_id) {
        (
            state.chat_page_token.clone(),
            state.super_chat_page_token.clone(),
        )
    } else {
        (None, None)
    }
}

#[derive(Clone, Default)]
pub(crate) struct YouTubeStatsManager {
    tasks: Arc<Mutex<HashMap<i64, TaskHandle>>>,
    resume_states: Arc<Mutex<HashMap<i64, SharedCollectorResume>>>,
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
        let resume = {
            let mut resume_states = self.resume_states.lock().await;
            let reusable = resume_states
                .get(&streamer_id)
                .filter(|state| {
                    resume_state_matches(&lock_resume(state), session.id, video_id.as_deref())
                })
                .cloned();
            match reusable {
                Some(resume) => resume,
                None => {
                    let resume = Arc::new(StdMutex::new(CollectorResumeState::new(
                        session.id,
                        video_id.clone(),
                    )));
                    resume_states.insert(streamer_id, Arc::clone(&resume));
                    resume
                }
            }
        };
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
            resume,
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

struct CollectorMetrics {
    peak_online: u64,
    online_total: u64,
    online_samples: u64,
    danmaku_count: u64,
    super_chat_count: u64,
    membership_count: u64,
    resume: SharedCollectorResume,
}

impl CollectorMetrics {
    fn observe_viewers(&mut self, viewers: u64) {
        self.peak_online = self.peak_online.max(viewers);
        self.online_total = self.online_total.saturating_add(viewers);
        self.online_samples = self.online_samples.saturating_add(1);
    }

    fn remember_chat_message_id(&self, id: &str) -> bool {
        lock_resume(&self.resume).seen_chat_message_ids.remember(id)
    }

    fn remember_paid_event_id(&self, id: &str) -> bool {
        lock_resume(&self.resume).seen_paid_event_ids.remember(id)
    }

    fn forget_chat_message_id(&self, id: &str) {
        lock_resume(&self.resume).seen_chat_message_ids.forget(id);
    }

    fn forget_paid_event_id(&self, id: &str) {
        lock_resume(&self.resume).seen_paid_event_ids.forget(id);
    }

    fn remember_interactive_user(&self, user: &str) {
        let mut state = lock_resume(&self.resume);
        if state.interactive_users.len() < MAX_TRACKED_EVENT_IDS {
            state.interactive_users.insert(user.to_owned());
        }
    }

    fn remember_paid_user(&self, user: &str) {
        let mut state = lock_resume(&self.resume);
        if state.paid_users.len() < MAX_TRACKED_EVENT_IDS {
            state.paid_users.insert(user.to_owned());
        }
    }

    fn paid_user_count(&self) -> u64 {
        lock_resume(&self.resume).paid_users.len() as u64
    }

    fn interactive_user_count(&self) -> u64 {
        lock_resume(&self.resume).interactive_users.len() as u64
    }

    /// Returns false when a Super Chat contained in the chat message could not
    /// be persisted. The caller must then retain its previous chat cursor so the
    /// message can be retried instead of losing the paid event permanently.
    async fn observe_chat_message(
        &mut self,
        store: &SqliteStore,
        session_id: i64,
        message: YouTubeLiveChatMessage,
    ) -> bool {
        if !self.remember_chat_message_id(&message.id) {
            return true;
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
            if !self.observe_super_chat(store, session_id, input).await {
                self.forget_chat_message_id(&message.id);
                return false;
            }
        }
        if message.display_message.is_some() {
            self.danmaku_count = self.danmaku_count.saturating_add(1);
        }
        if let Some(author) = message.author_channel_id.as_ref() {
            self.remember_interactive_user(author);
        }
        if message.membership_event.is_some() {
            self.membership_count = self.membership_count.saturating_add(1);
        }
        true
    }

    /// Returns false only when persistence failed, so paginated callers can keep
    /// their previous cursor and retry the event instead of advancing past it.
    async fn observe_super_chat(
        &mut self,
        store: &SqliteStore,
        session_id: i64,
        event: SuperChatEventInput,
    ) -> bool {
        if !self.remember_paid_event_id(&event.id) {
            return true;
        }
        let user = event.user_channel_id.clone();
        match store
            .record_youtube_super_chat_event(session_id, &event)
            .await
        {
            Ok(true) => {
                self.super_chat_count = self.super_chat_count.saturating_add(1);
                if let Some(user) = user {
                    self.remember_paid_user(&user);
                }
                true
            }
            Ok(false) => true,
            Err(error) => {
                self.forget_paid_event_id(&event.id);
                tracing::debug!(%error, session_id, "保存 YouTube Super Chat 事件失败");
                false
            }
        }
    }

    async fn observe_super_chat_event(
        &mut self,
        store: &SqliteStore,
        session_id: i64,
        event: YouTubeSuperChatEvent,
    ) -> bool {
        self.observe_super_chat(
            store,
            session_id,
            SuperChatEventInput {
                id: event.id,
                kind: event.kind.clone(),
                live_chat_id: Some(event.live_chat_id).filter(|value| !value.trim().is_empty()),
                created_at: event.created_at,
                user_channel_id: event.user_channel_id,
                user_display_name: event.user_display_name,
                amount_micros: event.amount_micros,
                currency: event.currency,
                amount_display_string: event.amount_display_string,
                tier: event.tier,
            },
        )
        .await
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

fn chat_is_available(token: Option<&str>, live_chat_id: Option<&str>, chat_degraded: bool) -> bool {
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
    // The store's collector-state contract does not accept "paused"; using
    // "degraded" keeps the open session and preserves metrics before shutdown.
    persist_metrics(
        store,
        session,
        started_at,
        observed_at,
        stats_available,
        "degraded",
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
    resume: SharedCollectorResume,
    initial_viewers: u64,
    started_at: DateTime<Utc>,
    mut stop_rx: oneshot::Receiver<DateTime<Utc>>,
) {
    live_chat_id = live_chat_id
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if let Some(chat_id) = live_chat_id.as_deref() {
        bind_chat_id(&resume, chat_id);
    } else {
        // A collector resumed in the same process can keep the chat identity and
        // continuation tokens discovered by its previous task.
        live_chat_id = lock_resume(&resume).chat_id.clone();
    }
    let (mut next_chat_page_token, mut next_super_chat_page_token) = live_chat_id
        .as_deref()
        .map(|chat_id| resume_page_tokens(&resume, chat_id))
        .unwrap_or((None, None));
    let mut ticker = interval(Duration::from_secs(SAMPLE_INTERVAL_SECONDS));
    let count_from_db = |value: i64| u64::try_from(value).unwrap_or(0);
    let mut metrics = CollectorMetrics {
        peak_online: session.peak_online,
        online_total: count_from_db(session.online_total),
        online_samples: count_from_db(session.online_samples),
        danmaku_count: count_from_db(session.danmaku_count),
        super_chat_count: count_from_db(session.super_chat_count),
        membership_count: count_from_db(session.membership_count.unwrap_or(0)),
        resume: Arc::clone(&resume),
    };
    let mut observed_initial = false;
    let mut chat_degraded = false;
    let mut chat_ever_succeeded = session.youtube_stats_available.unwrap_or(false);
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
                    if let Some(chat_id) = live_chat_id.as_deref() {
                        set_chat_page_token(&resume, chat_id, None);
                    }
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
                                        bind_chat_id(&resume, &chat_id);
                                        live_chat_id = Some(chat_id);
                                        next_chat_page_token = None;
                                        next_super_chat_page_token = None;
                                    }
                                    chat_available = chat_is_available(
                                        current_token.as_deref(),
                                        live_chat_id.as_deref(),
                                        chat_degraded,
                                    );
                                }
                                if video.actual_end_at.is_some()
                                    || video.live_broadcast_content.is_known_not_live()
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
                                    let mut cursor_safe = true;
                                    for message in page.messages {
                                        if !metrics
                                            .observe_chat_message(&store, session.id, message)
                                            .await
                                        {
                                            cursor_safe = false;
                                            break;
                                        }
                                    }
                                    if !cursor_safe {
                                        break;
                                    }
                                    let Some(next) = next else {
                                        next_chat_page_token = None;
                                        set_chat_page_token(&resume, &chat_id, None);
                                        break;
                                    };
                                    if page_token.as_deref() == Some(next.as_str())
                                        || seen_page_tokens.contains(&next)
                                    {
                                        // Reset on a token cycle instead of retrying the
                                        // same cursor on every tick.
                                        next_chat_page_token = None;
                                        set_chat_page_token(&resume, &chat_id, None);
                                        break;
                                    }
                                    set_chat_page_token(&resume, &chat_id, Some(next.clone()));
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
                        // Keep the continuation cursor across ticks. Otherwise a
                        // stream with more than one page budget of paid events
                        // would rescan the same first pages forever.
                        let page_token = next_super_chat_page_token.clone();
                        let Some(batch_result) = super::run_youtube_request(
                            &store,
                            client.fetch_super_chat_batch(
                                chat_id,
                                &token,
                                page_token.as_deref(),
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
                        match batch_result {
                            Ok(batch) => {
                                let next_page_token = batch.next_page_token;
                                let mut cursor_safe = true;
                                for event in batch.events {
                                    if !metrics
                                        .observe_super_chat_event(&store, session.id, event)
                                        .await
                                    {
                                        cursor_safe = false;
                                    }
                                }
                                if cursor_safe {
                                    set_super_chat_page_token(
                                        &resume,
                                        chat_id,
                                        next_page_token.clone(),
                                    );
                                    next_super_chat_page_token = next_page_token;
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
    let avg_online = metrics
        .online_total
        .checked_div(metrics.online_samples)
        .unwrap_or(0);
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
            stats_available.then_some(metrics.paid_user_count()),
            stats_available.then_some(metrics.interactive_user_count()),
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

    fn empty_metrics(resume: SharedCollectorResume) -> CollectorMetrics {
        CollectorMetrics {
            peak_online: 0,
            online_total: 0,
            online_samples: 0,
            danmaku_count: 0,
            super_chat_count: 0,
            membership_count: 0,
            resume,
        }
    }

    #[test]
    fn collector_resume_state_keeps_cursors_and_dedup_window() {
        let resume = Arc::new(StdMutex::new(CollectorResumeState::new(
            42,
            Some("video-1".to_owned()),
        )));
        bind_chat_id(&resume, "chat-1");
        set_chat_page_token(&resume, "chat-1", Some("chat-page-2".to_owned()));
        set_super_chat_page_token(&resume, "chat-1", Some("super-chat-page-3".to_owned()));

        assert_eq!(
            resume_page_tokens(&resume, "chat-1"),
            (
                Some("chat-page-2".to_owned()),
                Some("super-chat-page-3".to_owned())
            )
        );

        let first = empty_metrics(Arc::clone(&resume));
        assert!(first.remember_chat_message_id("message-1"));
        first.remember_interactive_user("viewer-1");
        first.remember_paid_user("supporter-1");
        let resumed = empty_metrics(resume);
        assert!(!resumed.remember_chat_message_id("message-1"));
        assert_eq!(resumed.interactive_user_count(), 1);
        assert_eq!(resumed.paid_user_count(), 1);

        first.forget_chat_message_id("message-1");
        assert!(resumed.remember_chat_message_id("message-1"));
    }

    #[test]
    fn collector_resume_state_is_scoped_to_session_and_video() {
        let state = CollectorResumeState::new(42, Some("video-1".to_owned()));
        assert!(resume_state_matches(&state, 42, Some("video-1")));
        assert!(!resume_state_matches(&state, 43, Some("video-1")));
        assert!(!resume_state_matches(&state, 42, Some("video-2")));
    }

    #[tokio::test]
    async fn youtube_switch_off_blocks_the_next_chat_page_request() {
        let store = SqliteStore::in_memory().await.expect("db");
        let mut settings = AppSettings {
            youtube_enabled: true,
            ..AppSettings::default()
        };
        save_settings(&store, &settings)
            .await
            .expect("enable youtube");

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
        let settings = AppSettings {
            youtube_enabled: true,
            ..AppSettings::default()
        };
        save_settings(&store, &settings)
            .await
            .expect("enable youtube");

        let request_store = store.clone();
        let paginated_request = async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let settings = AppSettings {
                youtube_enabled: false,
                ..AppSettings::default()
            };
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
