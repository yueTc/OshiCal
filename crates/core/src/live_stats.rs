use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{DateTime, Utc};
use flate2::read::ZlibDecoder;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};
use tokio::time::{interval, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    client_async_tls_with_config, connect_async_with_config, MaybeTlsStream, WebSocketStream,
};

use crate::bilibili::BilibiliClient;
use crate::error::{AppError, AppResult};
use crate::models::{LiveSession, LiveSessionStats};
#[cfg(test)]
use crate::models::Platform;
use crate::store::SqliteStore;

const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const COLLECTOR_STOP_TIMEOUT: Duration = Duration::from_secs(8);
const LIVE_AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const LIVE_SOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(95);
const LIVE_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PACKET_NESTING_DEPTH: usize = 4;
const MAX_DECOMPRESSED_BYTES: usize = 8 * 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_PROXY_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_WEBSOCKET_HOSTS: usize = 16;
const MAX_GIFT_V2_ITEMS: usize = 1024;
const MAX_TRACKED_USERS: usize = 100_000;
const MAX_DEDUP_EVENTS: usize = 50_000;
const USER_TRACKING_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const EVENT_DEDUP_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_PROTOBUF_FIELD_NUMBER: u64 = (1 << 29) - 1;

#[derive(Clone, Default)]
pub struct LiveStatsManager {
    tasks: Arc<Mutex<HashMap<i64, TaskHandle>>>,
    generations: Arc<Mutex<HashMap<i64, Arc<AtomicU64>>>>,
    lifecycles: Arc<Mutex<HashMap<i64, Arc<Mutex<()>>>>>,
}

struct TaskHandle {
    stop: Option<oneshot::Sender<DateTime<Utc>>>,
    handle: tokio::task::JoinHandle<()>,
    generation: Arc<AtomicU64>,
    expected_generation: u64,
}

struct CollectorToken {
    generation: Arc<AtomicU64>,
    expected: u64,
}

impl CollectorToken {
    fn is_current(&self) -> bool {
        self.generation.load(Ordering::Acquire) == self.expected
    }
}

struct BoundedSet {
    entries: HashMap<String, Instant>,
    order: VecDeque<(String, Instant)>,
    capacity: usize,
    ttl: Duration,
    degraded: bool,
}

impl BoundedSet {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
            ttl,
            degraded: false,
        }
    }

    fn purge_expired(&mut self, now: Instant) {
        while let Some((value, inserted_at)) = self.order.front() {
            if now.saturating_duration_since(*inserted_at) < self.ttl {
                break;
            }
            let value = value.clone();
            let inserted_at = *inserted_at;
            self.order.pop_front();
            if self.entries.get(&value).copied() == Some(inserted_at) {
                self.entries.remove(&value);
                // Once a dedup/user entry expires, a reconnect that replays an
                // older event can no longer be distinguished. Report partial
                // quality instead of claiming exact statistics.
                self.degraded = true;
            }
        }
    }

    fn insert(&mut self, value: String) -> bool {
        let now = Instant::now();
        self.purge_expired(now);
        if self.entries.contains_key(&value) {
            return false;
        }
        while self.entries.len() >= self.capacity {
            let Some((old_value, inserted_at)) = self.order.pop_front() else {
                break;
            };
            if self.entries.get(&old_value).copied() == Some(inserted_at) {
                self.entries.remove(&old_value);
                self.degraded = true;
            }
        }
        self.entries.insert(value.clone(), now);
        self.order.push_back((value, now));
        true
    }

    #[cfg(test)]
    fn contains(&self, value: &str) -> bool {
        self.entries.contains_key(value)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

struct CollectorStats {
    peak_high_energy: i64,
    // Latest fresh public observation since the last checkpoint; never restored from peak.
    online_observation: Option<(DateTime<Utc>, Instant, i64)>,
    danmaku_count: i64,
    gift_count: i64,
    super_chat_count: i64,
    guard_count: i64,
    revenue_cents: i64,
    gift_revenue_cents: i64,
    super_chat_revenue_cents: i64,
    guard_revenue_cents: i64,
    reconnect_count: i64,
    base_interactive_user_count: i64,
    base_paid_user_count: i64,
    interactive_user_count: i64,
    paid_user_count: i64,
    interactive_users: BoundedSet,
    paid_users: BoundedSet,
    dedup: BoundedSet,
    data_degraded: bool,
    connection_established: bool,
    has_connected_once: bool,
}

impl Default for CollectorStats {
    fn default() -> Self {
        Self {
            peak_high_energy: 0,
            online_observation: None,
            danmaku_count: 0,
            gift_count: 0,
            super_chat_count: 0,
            guard_count: 0,
            revenue_cents: 0,
            gift_revenue_cents: 0,
            super_chat_revenue_cents: 0,
            guard_revenue_cents: 0,
            reconnect_count: 0,
            base_interactive_user_count: 0,
            base_paid_user_count: 0,
            interactive_user_count: 0,
            paid_user_count: 0,
            interactive_users: BoundedSet::new(MAX_TRACKED_USERS, USER_TRACKING_TTL),
            paid_users: BoundedSet::new(MAX_TRACKED_USERS, USER_TRACKING_TTL),
            dedup: BoundedSet::new(MAX_DEDUP_EVENTS, EVENT_DEDUP_TTL),
            data_degraded: false,
            connection_established: false,
            has_connected_once: false,
        }
    }
}

impl CollectorStats {
    /// `resumed` 表示这条场次此前已经被采集过（换进程继续采，或断流重开后并回同一行）。
    /// 只有它才需要放弃“完整”的承诺：内存里的 UID/去重集合已经不存在了。
    fn from_session(session: &LiveSession, resumed: bool) -> Self {
        Self {
            peak_high_energy: i64::try_from(session.peak_high_energy).unwrap_or(i64::MAX),
            danmaku_count: session.danmaku_count,
            gift_count: session.gift_count,
            super_chat_count: session.super_chat_count,
            guard_count: session.guard_count,
            revenue_cents: session.revenue_cents,
            gift_revenue_cents: session.gift_revenue_cents,
            super_chat_revenue_cents: session.super_chat_revenue_cents,
            guard_revenue_cents: session.guard_revenue_cents,
            reconnect_count: session.reconnect_count,
            base_interactive_user_count: session.interactive_user_count,
            base_paid_user_count: session.paid_user_count,
            data_degraded: session.data_quality == "partial" || resumed,
            has_connected_once: resumed,
            ..Self::default()
        }
    }

    fn take_online_observation(
        &mut self,
        sampled_at: DateTime<Utc>,
    ) -> Option<(DateTime<Utc>, i64)> {
        let (observed_at, received, count) = self.online_observation.take()?;
        (self.connection_established
            && received.elapsed() <= LIVE_SOCKET_IDLE_TIMEOUT
            && observed_at <= sampled_at)
            .then_some((observed_at, count))
    }

    fn snapshot(&self, quality: &str, state: &str) -> LiveSessionStats {
        let quality = if !self.has_connected_once
            || self.data_degraded
            || self.interactive_users.degraded
            || self.paid_users.degraded
            || self.dedup.degraded
        {
            "partial"
        } else {
            quality
        };
        LiveSessionStats {
            peak_high_energy: self.peak_high_energy,
            revenue_cents: self.revenue_cents,
            gift_revenue_cents: self.gift_revenue_cents,
            super_chat_revenue_cents: self.super_chat_revenue_cents,
            guard_revenue_cents: self.guard_revenue_cents,
            paid_user_count: self
                .base_paid_user_count
                .saturating_add(self.paid_user_count),
            interactive_user_count: self
                .base_interactive_user_count
                .saturating_add(self.interactive_user_count),
            danmaku_count: self.danmaku_count,
            gift_count: self.gift_count,
            super_chat_count: self.super_chat_count,
            guard_count: self.guard_count,
            reconnect_count: self.reconnect_count,
            data_quality: quality.to_owned(),
            collector_state: state.to_owned(),
        }
    }
}

impl LiveStatsManager {
    async fn generation_counter(&self, streamer_id: i64) -> Arc<AtomicU64> {
        self.generations
            .lock()
            .await
            .entry(streamer_id)
            .or_default()
            .clone()
    }

    async fn lifecycle_lock(&self, streamer_id: i64) -> Arc<Mutex<()>> {
        self.lifecycles
            .lock()
            .await
            .entry(streamer_id)
            .or_default()
            .clone()
    }

    pub async fn start(&self, store: SqliteStore, bili: Arc<BilibiliClient>, session: LiveSession) {
        let streamer_id = session.streamer_id;
        let lifecycle = self.lifecycle_lock(streamer_id).await;
        let _lifecycle_guard = lifecycle.lock().await;
        let mut tasks = self.tasks.lock().await;
        if let Some(existing) = tasks.get(&streamer_id) {
            if !existing.handle.is_finished() {
                return;
            }
        }
        let generation = self.generation_counter(streamer_id).await;
        let expected_generation = generation.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        let token = CollectorToken {
            generation: generation.clone(),
            expected: expected_generation,
        };
        let (stop_tx, stop_rx) = oneshot::channel();
        // 这条场次已经被写过（检查点、断流重开合并都会推 updated_at），说明采集端在它身上
        // 已经跑过一轮：内存里的 UID 与去重集合都没了，不能承诺“完整”。
        let resumed = session.updated_at > session.created_at;
        let handle = tokio::spawn(run_collector(store, bili, session, stop_rx, token, resumed));
        tasks.insert(
            streamer_id,
            TaskHandle {
                stop: Some(stop_tx),
                handle,
                generation,
                expected_generation,
            },
        );
    }

    pub async fn stop(&self, streamer_id: i64, ended_at: DateTime<Utc>) {
        let lifecycle = self.lifecycle_lock(streamer_id).await;
        let _lifecycle_guard = lifecycle.lock().await;
        let entry = self.tasks.lock().await.remove(&streamer_id);
        let Some(mut entry) = entry else {
            return;
        };
        if let Some(stop) = entry.stop.take() {
            let _ = stop.send(ended_at);
        }
        if timeout(COLLECTOR_STOP_TIMEOUT, &mut entry.handle)
            .await
            .is_err()
        {
            let _ = entry.generation.compare_exchange(
                entry.expected_generation,
                entry.expected_generation.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            entry.handle.abort();
            let _ = entry.handle.await;
            tracing::warn!(streamer_id, "直播数据采集器停止超时，已中止旧任务");
        }
    }
}

async fn save_online_checkpoint(
    store: &SqliteStore,
    session_id: i64,
    stats: &mut CollectorStats,
    sampled_at: DateTime<Utc>,
    state: &str,
) {
    let observation = stats.take_online_observation(sampled_at);
    if let Err(error) = store
        .record_live_online_sample(session_id, sampled_at, observation, state)
        .await
    {
        stats.data_degraded = true;
        tracing::warn!(%error, session_id, "保存直播人数采样失败");
    }
}

async fn run_collector(
    store: SqliteStore,
    bili: Arc<BilibiliClient>,
    session: LiveSession,
    mut stop_rx: oneshot::Receiver<DateTime<Utc>>,
    token: CollectorToken,
    resumed: bool,
) {
    let mut stats = CollectorStats::from_session(&session, resumed);
    let mut retry_delay = RECONNECT_DELAY;
    loop {
        if !token.is_current() {
            return;
        }
        match collect_once(&store, &bili, &session, &mut stats, &mut stop_rx, &token).await {
            CollectResult::Stopped(ended_at) => {
                let quality = if stats.reconnect_count == 0 {
                    "complete"
                } else {
                    "partial"
                };
                if token.is_current() {
                    save_online_checkpoint(&store, session.id, &mut stats, ended_at, "finished")
                        .await;
                    if let Err(error) = store
                        .finish_live_session(
                            session.id,
                            ended_at,
                            &stats.snapshot(quality, "finished"),
                        )
                        .await
                    {
                        tracing::warn!(%error, session_id = session.id, "保存直播数据失败");
                    }
                }
                return;
            }
            CollectResult::Disconnected(error) => {
                if !token.is_current() {
                    return;
                }
                stats.reconnect_count = stats.reconnect_count.saturating_add(1);
                let current_delay = if stats.connection_established {
                    retry_delay = RECONNECT_DELAY;
                    RECONNECT_DELAY
                } else {
                    let delay = retry_delay;
                    retry_delay = (retry_delay * 2).min(MAX_RECONNECT_DELAY);
                    delay
                };
                stats.connection_established = false;
                stats.online_observation = None;
                if token.is_current() {
                    save_online_checkpoint(
                        &store,
                        session.id,
                        &mut stats,
                        Utc::now(),
                        "reconnecting",
                    )
                    .await;
                    if let Err(store_error) = store
                        .update_live_session_stats(
                            session.id,
                            &stats.snapshot("partial", "reconnecting"),
                        )
                        .await
                    {
                        tracing::warn!(%store_error, session_id = session.id, "保存直播采集断点失败");
                    }
                }
                tracing::warn!(
                    %error,
                    session_id = session.id,
                    retry_in_seconds = current_delay.as_secs(),
                    "直播数据连接中断，准备重连"
                );
                let stopped_at = tokio::select! {
                    value = &mut stop_rx => Some(value),
                    _ = tokio::time::sleep(current_delay) => None,
                };
                match stopped_at {
                    Some(Ok(ended_at)) => {
                        let quality = if stats.reconnect_count == 0 {
                            "complete"
                        } else {
                            "partial"
                        };
                        if token.is_current() {
                            save_online_checkpoint(
                                &store, session.id, &mut stats, ended_at, "finished",
                            )
                            .await;
                            let _ = store
                                .finish_live_session(
                                    session.id,
                                    ended_at,
                                    &stats.snapshot(quality, "finished"),
                                )
                                .await;
                        }
                        return;
                    }
                    Some(Err(_)) => return,
                    None => {}
                }
            }
        }
    }
}

enum CollectResult {
    Stopped(DateTime<Utc>),
    Disconnected(String),
}

async fn collect_once(
    store: &SqliteStore,
    bili: &BilibiliClient,
    session: &LiveSession,
    stats: &mut CollectorStats,
    stop_rx: &mut oneshot::Receiver<DateTime<Utc>>,
    collector_token: &CollectorToken,
) -> CollectResult {
    let websocket_proxy = bili.websocket_proxy().map(ToOwned::to_owned);
    let info = tokio::select! {
        value = &mut *stop_rx => {
            return match value {
                Ok(ended_at) => CollectResult::Stopped(ended_at),
                Err(_) => CollectResult::Disconnected("直播采集停止信号断开".to_owned()),
            };
        }
        result = bili.fetch_live_danmu_info(&session.room_id) => result,
    };
    let info = match info {
        Ok(value) => value,
        Err(error) => return CollectResult::Disconnected(error.to_string()),
    };
    let cookie_revision = info.cookie_revision;
    let mut auth = serde_json::json!({
        "uid": info.uid,
        "roomid": session.room_id.parse::<u64>().unwrap_or_default(),
        "protover": 2,
        "platform": "web",
        "type": 2,
    });
    if let Some(buvid) = info.buvid {
        auth["buvid"] = Value::String(buvid);
    }
    let info = info.response;
    let token = match info.pointer("/data/token").and_then(Value::as_str) {
        Some(value) => value,
        None => return CollectResult::Disconnected("弹幕服务器缺少 token".to_owned()),
    };
    let hosts = match info.pointer("/data/host_list").and_then(Value::as_array) {
        Some(value) => value,
        None => return CollectResult::Disconnected("弹幕服务器列表为空".to_owned()),
    };

    let mut last_error = None;
    'endpoints: for url in websocket_endpoints(hosts) {
        let connection = tokio::select! {
            value = &mut *stop_rx => {
                return match value {
                    Ok(ended_at) => CollectResult::Stopped(ended_at),
                    Err(_) => CollectResult::Disconnected("直播采集停止信号断开".to_owned()),
                };
            }
            result = timeout(
                Duration::from_secs(12),
                connect_live_socket(&url, websocket_proxy.as_deref()),
            ) => result,
        };
        let socket = match connection {
            Ok(Ok(socket)) => socket,
            Ok(Err(error)) => {
                tracing::debug!(%error, %url, "直播弹幕连接失败");
                continue;
            }
            Err(_) => {
                tracing::debug!(%url, "直播弹幕连接超时");
                continue;
            }
        };
        let (mut writer, mut reader) = socket.split();
        stats.connection_established = false;
        stats.online_observation = None;
        let mut last_received = Instant::now();
        if bili.cookie_revision() != cookie_revision {
            return CollectResult::Disconnected("B 站登录状态变化，重新认证直播连接".to_owned());
        }
        auth["key"] = Value::String(token.to_owned());
        let auth_result = tokio::select! {
            value = &mut *stop_rx => {
                return match value {
                    Ok(ended_at) => CollectResult::Stopped(ended_at),
                    Err(_) => CollectResult::Disconnected("直播采集停止信号断开".to_owned()),
                };
            }
            result = timeout(
                LIVE_SEND_TIMEOUT,
                writer.send(Message::Binary(
                    make_packet(7, serde_json::to_vec(&auth).unwrap_or_default()).into(),
                )),
            ) => result,
        };
        match auth_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                last_error = Some(error.to_string());
                continue 'endpoints;
            }
            Err(_) => {
                last_error = Some("发送弹幕认证消息超时".to_owned());
                continue 'endpoints;
            }
        }

        let mut heartbeat = interval(HEARTBEAT_INTERVAL);
        let mut checkpoint = interval(CHECKPOINT_INTERVAL);
        let auth_timeout = tokio::time::sleep(LIVE_AUTH_TIMEOUT);
        tokio::pin!(auth_timeout);
        heartbeat.tick().await;
        checkpoint.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        checkpoint.tick().await;

        loop {
            tokio::select! {
                value = &mut *stop_rx => {
                    return match value {
                        Ok(ended_at) => CollectResult::Stopped(ended_at),
                        Err(_) => CollectResult::Disconnected("直播采集停止信号断开".to_owned()),
                    };
                }
                _ = &mut auth_timeout, if !stats.connection_established => {
                    { last_error = Some("弹幕服务器认证超时".to_owned()); continue 'endpoints; }
                }
                _ = heartbeat.tick() => {
                    if !collector_token.is_current() {
                        return CollectResult::Disconnected("直播采集任务已过期".to_owned());
                    }
                    let send_result = tokio::select! {
                        value = &mut *stop_rx => {
                            return match value {
                                Ok(ended_at) => CollectResult::Stopped(ended_at),
                                Err(_) => CollectResult::Disconnected("直播采集停止信号断开".to_owned()),
                            };
                        }
                        result = timeout(
                            LIVE_SEND_TIMEOUT,
                            writer.send(Message::Binary(make_packet(2, Vec::new()).into())),
                        ) => result,
                    };
                    match send_result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => { last_error = Some(error.to_string()); continue 'endpoints; },
                        Err(_) => { last_error = Some("发送弹幕心跳超时".to_owned()); continue 'endpoints; },
                    }
                }
                _ = checkpoint.tick() => {
                    if bili.cookie_revision() != cookie_revision {
                        return CollectResult::Disconnected("B 站登录状态变化，重新认证直播连接".to_owned());
                    }
                    if last_received.elapsed() >= LIVE_SOCKET_IDLE_TIMEOUT {
                        { last_error = Some("弹幕服务器长时间无响应".to_owned()); continue 'endpoints; }
                    }
                    if !collector_token.is_current() {
                        return CollectResult::Disconnected("直播采集任务已过期".to_owned());
                    }
                    save_online_checkpoint(store, session.id, stats, Utc::now(), "active").await;
                    if let Err(error) = store.update_live_session_stats(
                        session.id,
                        &stats.snapshot("collecting", "active"),
                    ).await {
                        tracing::warn!(%error, session_id = session.id, "保存直播统计检查点失败");
                    }
                }
                message = reader.next() => {
                    last_received = Instant::now();
                    match message {
                        Some(Ok(Message::Binary(bytes))) => {
                            parse_live_packets(stats, &bytes, 0);
                        }
                        Some(Ok(Message::Text(text))) => {
                            if text.len() <= MAX_MESSAGE_BYTES {
                                match serde_json::from_str::<Value>(text.as_str()) {
                                    Ok(value) => parse_live_message(stats, &value),
                                    Err(error) => {
                                        stats.data_degraded = true;
                                        tracing::warn!(%error, session_id = session.id, "直播文本消息 JSON 解析失败");
                                    }
                                }
                            } else {
                                stats.data_degraded = true;
                                tracing::warn!(session_id = session.id, "直播文本消息超过大小限制");
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let pong_result = tokio::select! {
                                value = &mut *stop_rx => {
                                    return match value {
                                        Ok(ended_at) => CollectResult::Stopped(ended_at),
                                        Err(_) => CollectResult::Disconnected("直播采集停止信号断开".to_owned()),
                                    };
                                }
                                result = timeout(
                                    LIVE_SEND_TIMEOUT,
                                    writer.send(Message::Pong(payload)),
                                ) => result,
                            };
                            match pong_result {
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => { last_error = Some(error.to_string()); continue 'endpoints; },
                                Err(_) => { last_error = Some("发送弹幕 Pong 超时".to_owned()); continue 'endpoints; },
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            { last_error = Some("直播弹幕连接关闭".to_owned()); continue 'endpoints; }
                        }
                        Some(Err(error)) => {
                            { last_error = Some(error.to_string()); continue 'endpoints; };
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    CollectResult::Disconnected(last_error.unwrap_or_else(|| "所有弹幕服务器均连接失败".to_owned()))
}

async fn connect_live_socket(
    url: &str,
    proxy: Option<&str>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    let request = url
        .into_client_request()
        .map_err(|error| format!("创建弹幕 WebSocket 请求失败: {error}"))?;

    let Some(proxy) = proxy else {
        return connect_async_with_config(request, Some(live_socket_config()), false)
            .await
            .map(|(socket, _)| socket)
            .map_err(|error| error.to_string());
    };

    let proxy_display = crate::network::redact_proxy_url(proxy);
    let proxy_url = url::Url::parse(proxy)
        .map_err(|error| format!("弹幕代理地址无效（{proxy_display}）: {error}"))?;
    if proxy_url.scheme() != "http" {
        return Err(format!(
            "弹幕 WebSocket 当前仅支持 HTTP CONNECT 代理，收到 {}",
            proxy_url.scheme()
        ));
    }
    if !matches!(proxy_url.path(), "" | "/")
        || proxy_url.query().is_some()
        || proxy_url.fragment().is_some()
    {
        return Err(format!(
            "弹幕代理地址必须是无路径、查询和片段的 HTTP 地址: {proxy_display}"
        ));
    }
    let proxy_host = proxy_url
        .host_str()
        .ok_or_else(|| "弹幕代理地址缺少主机名".to_owned())?;
    let proxy_port = proxy_url.port_or_known_default().unwrap_or(8080);
    let target_url =
        url::Url::parse(url).map_err(|error| format!("弹幕服务器地址无效（{url}）: {error}"))?;
    if target_url.scheme() != "wss" {
        return Err("弹幕服务器必须使用 WSS".to_owned());
    }
    let target_host = normalized_websocket_host(
        target_url
            .host_str()
            .ok_or_else(|| "弹幕服务器地址缺少主机名".to_owned())?,
    )
    .ok_or_else(|| "弹幕服务器主机名不受信任".to_owned())?;
    let target_port = target_url.port_or_known_default().unwrap_or(443);

    let mut stream = TcpStream::connect((proxy_host, proxy_port))
        .await
        .map_err(|error| format!("连接弹幕代理 {proxy_host}:{proxy_port} 失败: {error}"))?;
    let mut connect_request = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\nProxy-Connection: Keep-Alive\r\n"
    );
    if !proxy_url.username().is_empty() {
        let password = proxy_url.password().unwrap_or_default();
        let credential = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{password}", proxy_url.username()));
        connect_request.push_str(&format!("Proxy-Authorization: Basic {credential}\r\n"));
    }
    connect_request.push_str("\r\n");
    stream
        .write_all(connect_request.as_bytes())
        .await
        .map_err(|error| format!("发送弹幕代理 CONNECT 请求失败: {error}"))?;

    let mut response = Vec::new();
    loop {
        let byte = stream
            .read_u8()
            .await
            .map_err(|error| format!("读取弹幕代理 CONNECT 响应失败: {error}"))?;
        response.push(byte);
        if response.ends_with(b"\r\n\r\n") {
            break;
        }
        if response.len() >= MAX_PROXY_RESPONSE_BYTES {
            return Err("弹幕代理 CONNECT 响应头过大".to_owned());
        }
    }
    let status_line = String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let status = status_line.split_whitespace().nth(1).unwrap_or_default();
    if status != "200" {
        return Err(format!("弹幕代理 CONNECT 被拒绝: {status_line}"));
    }

    client_async_tls_with_config(request, stream, Some(live_socket_config()), None)
        .await
        .map(|(socket, _)| socket)
        .map_err(|error| format!("经代理建立弹幕 TLS/WebSocket 失败: {error}"))
}

fn live_socket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_DECOMPRESSED_BYTES))
        .max_frame_size(Some(MAX_DECOMPRESSED_BYTES))
}

fn websocket_endpoints(hosts: &[Value]) -> Vec<String> {
    let mut endpoints = Vec::new();
    let mut seen = HashSet::new();
    for host in hosts.iter().take(MAX_WEBSOCKET_HOSTS) {
        let Some(host_name) = host.get("host").and_then(Value::as_str) else {
            continue;
        };
        let wss_port = host.get("wss_port").and_then(Value::as_u64).unwrap_or(2245);
        append_websocket_endpoint(&mut endpoints, &mut seen, host_name, 443);
        append_websocket_endpoint(&mut endpoints, &mut seen, host_name, wss_port);
    }
    // The regional host can be blocked while Bilibili's shared endpoint remains reachable.
    append_websocket_endpoint(
        &mut endpoints,
        &mut seen,
        "broadcastlv.chat.bilibili.com",
        443,
    );
    endpoints
}

fn append_websocket_endpoint(
    endpoints: &mut Vec<String>,
    seen: &mut HashSet<String>,
    host: &str,
    port: u64,
) {
    let Some(host) = normalized_websocket_host(host) else {
        tracing::warn!("忽略不合法的弹幕服务器主机名");
        return;
    };
    let Ok(port) = u16::try_from(port) else {
        return;
    };
    if port == 0 {
        return;
    }
    let key = format!("{host}:{port}");
    if !seen.insert(key) {
        return;
    }
    if port == 443 {
        endpoints.push(format!("wss://{host}/sub"));
    } else {
        endpoints.push(format!("wss://{host}:{port}/sub"));
    }
}

fn normalized_websocket_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 253
        || !host.contains('.')
        || host.parse::<std::net::IpAddr>().is_ok()
        || matches!(
            host.rsplit('.').next().unwrap_or_default(),
            "local" | "localhost" | "internal" | "lan" | "home"
        )
    {
        return None;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let tld = labels.last()?;
    if tld.len() < 2 || !tld.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return None;
    }
    if !labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    }) {
        return None;
    }
    Some(host)
}

fn make_packet(operation: u32, body: Vec<u8>) -> Vec<u8> {
    let mut packet = Vec::with_capacity(16 + body.len());
    packet.extend_from_slice(&((16 + body.len()) as u32).to_be_bytes());
    packet.extend_from_slice(&16u16.to_be_bytes());
    packet.extend_from_slice(&1u16.to_be_bytes());
    packet.extend_from_slice(&operation.to_be_bytes());
    packet.extend_from_slice(&1u32.to_be_bytes());
    packet.extend_from_slice(&body);
    packet
}

fn parse_live_packets(stats: &mut CollectorStats, bytes: &[u8], depth: usize) {
    let mut decompression_budget = MAX_DECOMPRESSED_BYTES;
    if let Err(error) = parse_live_packets_inner(stats, bytes, depth, &mut decompression_budget) {
        stats.data_degraded = true;
        tracing::warn!(%error, "直播数据包解析失败，已丢弃该包");
    }
}

fn parse_live_packets_inner(
    stats: &mut CollectorStats,
    bytes: &[u8],
    depth: usize,
    decompression_budget: &mut usize,
) -> Result<(), String> {
    if depth > MAX_PACKET_NESTING_DEPTH {
        return Err(format!("直播数据包嵌套超过 {MAX_PACKET_NESTING_DEPTH} 层"));
    }
    if bytes.is_empty() {
        return Err("直播数据包为空".to_owned());
    }
    if bytes.len() > MAX_DECOMPRESSED_BYTES {
        return Err("直播数据包超过大小限制".to_owned());
    }
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < 16 {
            return Err("直播数据包尾部不足 16 字节".to_owned());
        }
        let packet_len =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("packet header"))
                as usize;
        if packet_len < 16 {
            return Err("直播数据包长度小于头部长度".to_owned());
        }
        let packet_end = offset
            .checked_add(packet_len)
            .ok_or_else(|| "直播数据包长度溢出".to_owned())?;
        if packet_end > bytes.len() {
            return Err("直播数据包长度越界".to_owned());
        }
        let header_len = u16::from_be_bytes(
            bytes[offset + 4..offset + 6]
                .try_into()
                .expect("packet header"),
        ) as usize;
        if header_len < 16 || header_len > packet_len {
            return Err("直播数据包头部长度非法".to_owned());
        }
        let version = u16::from_be_bytes(
            bytes[offset + 6..offset + 8]
                .try_into()
                .expect("packet header"),
        );
        let operation = u32::from_be_bytes(
            bytes[offset + 8..offset + 12]
                .try_into()
                .expect("packet header"),
        );
        let body = &bytes[offset + header_len..packet_end];
        if operation == 8 {
            let value = serde_json::from_slice::<Value>(body)
                .map_err(|error| format!("认证响应 JSON 非法: {error}"))?;
            let auth_code = value
                .get("code")
                .and_then(value_to_string)
                .unwrap_or_default();
            if auth_code == "0" {
                stats.connection_established = true;
                stats.has_connected_once = true;
            } else {
                stats.data_degraded = true;
                tracing::warn!(%auth_code, "弹幕服务器拒绝认证");
            }
        } else if operation == 5 {
            match version {
                2 | 3 => {
                    if *decompression_budget == 0 || body.len() > MAX_DECOMPRESSED_BYTES {
                        return Err("直播压缩数据超过解压预算".to_owned());
                    }
                    let decoded = if version == 2 {
                        let decoder = ZlibDecoder::new(body);
                        read_decompressed_limited(decoder, *decompression_budget)?
                    } else {
                        let decoder = brotli::Decompressor::new(body, 4096);
                        read_decompressed_limited(decoder, *decompression_budget)?
                    };
                    if decoded.len() > *decompression_budget {
                        return Err("直播数据解压后超过大小限制".to_owned());
                    }
                    *decompression_budget -= decoded.len();
                    parse_live_packets_inner(stats, &decoded, depth + 1, decompression_budget)?;
                }
                _ => {
                    if body.len() > MAX_MESSAGE_BYTES {
                        return Err("直播 JSON 消息超过大小限制".to_owned());
                    }
                    let value = serde_json::from_slice::<Value>(body)
                        .map_err(|error| format!("直播 JSON 消息非法: {error}"))?;
                    parse_live_message(stats, &value);
                }
            }
        }
        offset = packet_end;
    }
    Ok(())
}

fn read_decompressed_limited<R: Read>(reader: R, limit: usize) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|error| format!("直播数据解压失败: {error}"))?;
    if decoded.len() > limit {
        return Err("直播数据解压后超过大小限制".to_owned());
    }
    Ok(decoded)
}
fn parse_live_message(stats: &mut CollectorStats, value: &Value) {
    let cmd = value
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    let data = value.get("data").unwrap_or(&Value::Null);
    match cmd {
        "ONLINE_RANK_COUNT" => {
            // B 站公开事件中的高能用户在线上限，不是直播间热度值。
            let raw = data.get("online_count").or_else(|| data.get("count"));
            let current = match raw.and_then(Value::as_i64) {
                Some(value) if value >= 0 => value,
                Some(_) => {
                    stats.data_degraded = true;
                    return;
                }
                None => {
                    if raw.is_some() {
                        stats.data_degraded = true;
                    }
                    return;
                }
            };
            stats.peak_high_energy = stats.peak_high_energy.max(current);
            stats.online_observation = Some((Utc::now(), Instant::now(), current));
        }
        "DANMU_MSG" => {
            stats.danmaku_count = stats.danmaku_count.saturating_add(1);
            if let Some(uid) = value.pointer("/info/2/0").and_then(value_to_string) {
                add_interactive_user(stats, &uid);
            }
        }
        "SEND_GIFT" => {
            add_gift(
                stats,
                event_sender_uid(data),
                data.get("giftId").and_then(Value::as_i64).unwrap_or(0),
                data.get("giftName")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                data.get("num").and_then(Value::as_i64).unwrap_or(1),
                data.get("price").and_then(Value::as_i64).unwrap_or(0),
                data.get("total_coin").and_then(Value::as_i64).unwrap_or(0),
                data.get("coin_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                data.get("tid").and_then(Value::as_str).unwrap_or_default(),
                data.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
            );
        }
        "SEND_GIFT_V2" => {
            let Some(pb) = data.get("pb").and_then(Value::as_str) else {
                stats.data_degraded = true;
                return;
            };
            let gifts = match decode_send_gift_v2(pb) {
                Ok(gifts) => gifts,
                Err(error) => {
                    stats.data_degraded = true;
                    tracing::warn!(%error, "SEND_GIFT_V2 protobuf 解析失败，已跳过该消息");
                    return;
                }
            };
            for gift in gifts {
                add_gift(
                    stats,
                    Some(gift.uid.to_string()),
                    gift.gift_id,
                    &gift.gift_name,
                    gift.num,
                    gift.price,
                    gift.total_coin,
                    &gift.coin_type,
                    &gift.tid,
                    gift.timestamp,
                );
            }
        }
        "SUPER_CHAT_MESSAGE" => {
            let uid = event_sender_uid(data);
            let event_id = data
                .get("id")
                .and_then(value_to_string)
                .filter(|value| !value.is_empty());
            let key = match event_id {
                Some(id) => format!("sc:{id}"),
                None => format!(
                    "sc:{}:{}:{}:{}",
                    uid.as_deref().unwrap_or("anonymous"),
                    data.get("price").and_then(Value::as_f64).unwrap_or(0.0),
                    data.get("time")
                        .and_then(value_to_string)
                        .unwrap_or_default(),
                    data.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ),
            };
            if !mark_unique_event(stats, key) {
                return;
            }
            stats.super_chat_count = stats.super_chat_count.saturating_add(1);
            if let Some(uid) = uid.as_deref() {
                add_interactive_user(stats, uid);
            }
            let amount_value = data.get("price");
            let amount = amount_value.and_then(Value::as_f64).unwrap_or(0.0);
            let cents = match yuan_to_cents(amount) {
                Some(cents) => cents,
                None if amount_value.is_some() => {
                    stats.data_degraded = true;
                    0
                }
                None => 0,
            };
            if cents > 0 {
                stats.revenue_cents = stats.revenue_cents.saturating_add(cents);
                stats.super_chat_revenue_cents =
                    stats.super_chat_revenue_cents.saturating_add(cents);
                if let Some(uid) = uid.as_deref() {
                    add_paid_user(stats, uid);
                } else {
                    stats.data_degraded = true;
                }
            }
        }
        "GUARD_BUY" => {
            let uid = event_sender_uid(data);
            let key = format!(
                "guard:{}:{}:{}:{}",
                uid.as_deref().unwrap_or("anonymous"),
                data.get("guard_level")
                    .and_then(value_to_string)
                    .unwrap_or_default(),
                data.get("start_time")
                    .or_else(|| data.get("timestamp"))
                    .and_then(value_to_string)
                    .unwrap_or_default(),
                data.get("price").and_then(Value::as_i64).unwrap_or(0)
            );
            if !mark_unique_event(stats, key) {
                return;
            }
            stats.guard_count = stats.guard_count.saturating_add(1);
            if let Some(uid) = uid.as_deref() {
                add_interactive_user(stats, uid);
            }
            let guard_price_value = data.get("price");
            let raw_guard_price = guard_price_value.and_then(Value::as_i64);
            let invalid_guard_price = match raw_guard_price {
                Some(value) => value < 0,
                None => true,
            };
            if guard_price_value.is_some() && invalid_guard_price {
                stats.data_degraded = true;
            }
            let cents = raw_guard_price.unwrap_or(0).max(0) / 10;
            if cents > 0 {
                stats.revenue_cents = stats.revenue_cents.saturating_add(cents);
                stats.guard_revenue_cents = stats.guard_revenue_cents.saturating_add(cents);
                if let Some(uid) = uid.as_deref() {
                    add_paid_user(stats, uid);
                } else {
                    stats.data_degraded = true;
                }
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn add_gift(
    stats: &mut CollectorStats,
    uid: Option<String>,
    gift_id: i64,
    gift_name: &str,
    num: i64,
    price: i64,
    total_coin: i64,
    coin_type: &str,
    tid: &str,
    timestamp: i64,
) {
    let uid = normalize_uid(uid.as_deref());
    let user_key = uid.as_deref().unwrap_or("anonymous");
    let key = if tid.trim().is_empty() {
        format!("gift:{user_key}:{gift_id}:{num}:{timestamp}:{price}:{gift_name}")
    } else {
        format!("gift:{}", tid.trim())
    };
    if !mark_unique_event(stats, key) {
        return;
    }

    stats.gift_count = stats.gift_count.saturating_add(1);
    if let Some(uid) = uid.as_deref() {
        add_interactive_user(stats, uid);
    }

    if gift_id <= 0 || price < 0 || total_coin < 0 {
        stats.data_degraded = true;
    }
    let num = if num > 0 {
        num
    } else {
        stats.data_degraded = true;
        1
    };
    let price = price.max(0);
    let total_coin = total_coin.max(0);
    let coins = if total_coin > 0 {
        total_coin
    } else {
        match price.checked_mul(num) {
            Some(value) => value,
            None => {
                stats.data_degraded = true;
                0
            }
        }
    };
    let cents = if coin_type.eq_ignore_ascii_case("silver") {
        0
    } else {
        coins / 10
    };
    if cents > 0 {
        stats.revenue_cents = stats.revenue_cents.saturating_add(cents);
        stats.gift_revenue_cents = stats.gift_revenue_cents.saturating_add(cents);
        if let Some(uid) = uid.as_deref() {
            add_paid_user(stats, uid);
        } else {
            stats.data_degraded = true;
        }
    }
}

fn event_sender_uid(data: &Value) -> Option<String> {
    // The top-level UID may be redacted while the sender profile still has it.
    // Never use receiver_uinfo: that identifies the gift recipient.
    [
        data.get("uid"),
        data.pointer("/sender_uinfo/uid"),
        data.pointer("/uinfo/uid"),
    ]
    .into_iter()
    .flatten()
    .filter_map(value_to_string)
    .find_map(|uid| normalize_uid(Some(&uid)))
}

fn normalize_uid(uid: Option<&str>) -> Option<String> {
    uid.map(str::trim)
        .filter(|value| !value.is_empty() && *value != "0")
        .map(ToOwned::to_owned)
}

fn add_interactive_user(stats: &mut CollectorStats, uid: &str) {
    let Some(uid) = normalize_uid(Some(uid)) else {
        return;
    };
    let key = bounded_tracking_key(stats, "user", &uid, 128);
    if stats.interactive_users.insert(key) {
        stats.interactive_user_count = stats.interactive_user_count.saturating_add(1);
    }
}

fn add_paid_user(stats: &mut CollectorStats, uid: &str) {
    let Some(uid) = normalize_uid(Some(uid)) else {
        return;
    };
    add_interactive_user(stats, &uid);
    let key = bounded_tracking_key(stats, "paid", &uid, 128);
    if stats.paid_users.insert(key) {
        stats.paid_user_count = stats.paid_user_count.saturating_add(1);
    }
}

fn mark_unique_event(stats: &mut CollectorStats, key: String) -> bool {
    let key = bounded_tracking_key(stats, "event", &key, 512);
    stats.dedup.insert(key)
}

fn bounded_tracking_key(
    stats: &mut CollectorStats,
    prefix: &str,
    value: &str,
    max_bytes: usize,
) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    stats.data_degraded = true;
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{prefix}-hash:{:016x}", hasher.finish())
}

fn yuan_to_cents(amount: f64) -> Option<i64> {
    if !amount.is_finite() || amount <= 0.0 || amount > i64::MAX as f64 / 100.0 {
        return None;
    }
    let cents = (amount * 100.0).round();
    if cents > i64::MAX as f64 {
        None
    } else {
        Some(cents as i64)
    }
}
fn value_to_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

struct GiftV2 {
    uid: u64,
    gift_id: i64,
    gift_name: String,
    num: i64,
    price: i64,
    total_coin: i64,
    coin_type: String,
    tid: String,
    timestamp: i64,
}

enum ProtoValue {
    Varint(u64),
    Bytes(Vec<u8>),
    Fixed32,
    Fixed64,
}

fn take_protobuf_bytes(bytes: &[u8], offset: &mut usize, len: usize) -> AppResult<Vec<u8>> {
    let end = (*offset)
        .checked_add(len)
        .ok_or_else(|| AppError::validation("protobuf 长度溢出"))?;
    if end > bytes.len() {
        return Err(AppError::validation("protobuf 长度越界"));
    }
    let value = bytes[*offset..end].to_vec();
    *offset = end;
    Ok(value)
}

fn decode_protobuf(bytes: &[u8]) -> AppResult<HashMap<u32, Vec<ProtoValue>>> {
    let mut fields: HashMap<u32, Vec<ProtoValue>> = HashMap::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = decode_varint(bytes, &mut offset)
            .ok_or_else(|| AppError::validation("非法 protobuf tag"))?;
        let field = tag >> 3;
        if field == 0 || field > MAX_PROTOBUF_FIELD_NUMBER {
            return Err(AppError::validation("protobuf 字段编号非法"));
        }
        let field = field as u32;
        let wire = (tag & 7) as u8;
        let value = match wire {
            0 => ProtoValue::Varint(
                decode_varint(bytes, &mut offset)
                    .ok_or_else(|| AppError::validation("非法 protobuf varint"))?,
            ),
            1 => {
                let _ = take_protobuf_bytes(bytes, &mut offset, 8)?;
                ProtoValue::Fixed64
            }
            2 => {
                let len = decode_varint(bytes, &mut offset)
                    .ok_or_else(|| AppError::validation("非法 protobuf 长度"))?;
                let len = usize::try_from(len)
                    .map_err(|_| AppError::validation("protobuf 长度不支持"))?;
                ProtoValue::Bytes(take_protobuf_bytes(bytes, &mut offset, len)?)
            }
            5 => {
                let _ = take_protobuf_bytes(bytes, &mut offset, 4)?;
                ProtoValue::Fixed32
            }
            _ => return Err(AppError::validation("不支持的 protobuf wire type")),
        };
        fields.entry(field).or_default().push(value);
    }
    Ok(fields)
}

fn decode_varint(bytes: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = *bytes.get(*offset)?;
        *offset += 1;
        if index == 9 && byte > 1 {
            return None;
        }
        value |= ((byte & 0x7f) as u64) << (index * 7);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn field_varint(fields: &HashMap<u32, Vec<ProtoValue>>, id: u32) -> AppResult<Option<u64>> {
    let Some(values) = fields.get(&id) else {
        return Ok(None);
    };
    let mut result = None;
    for value in values {
        match value {
            ProtoValue::Varint(value) => {
                if result.is_none() {
                    result = Some(*value);
                }
            }
            ProtoValue::Bytes(_) | ProtoValue::Fixed32 | ProtoValue::Fixed64 => {
                return Err(AppError::validation(format!(
                    "protobuf 字段 {id} 的 wire type 错误"
                )));
            }
        }
    }
    Ok(result)
}

fn field_string(fields: &HashMap<u32, Vec<ProtoValue>>, id: u32) -> AppResult<Option<String>> {
    let Some(values) = fields.get(&id) else {
        return Ok(None);
    };
    let mut result = None;
    for value in values {
        let ProtoValue::Bytes(value) = value else {
            return Err(AppError::validation(format!(
                "protobuf 字段 {id} 的 wire type 错误"
            )));
        };
        let text = std::str::from_utf8(value)
            .map_err(|_| AppError::validation(format!("protobuf 字段 {id} 不是有效 UTF-8")))?;
        if result.is_none() {
            result = Some(text.to_owned());
        }
    }
    Ok(result)
}

fn required_protobuf_i64(
    fields: &HashMap<u32, Vec<ProtoValue>>,
    id: u32,
    label: &str,
) -> AppResult<i64> {
    let value = field_varint(fields, id)?
        .ok_or_else(|| AppError::validation(format!("SEND_GIFT_V2 缺少{label}")))?;
    if value > i64::MAX as u64 {
        return Err(AppError::validation(format!(
            "SEND_GIFT_V2 {label}超出范围"
        )));
    }
    let value = value as i64;
    if value == 0 {
        return Err(AppError::validation(format!(
            "SEND_GIFT_V2 {label}必须大于 0"
        )));
    }
    Ok(value)
}

fn optional_protobuf_i64(
    fields: &HashMap<u32, Vec<ProtoValue>>,
    id: u32,
    label: &str,
) -> AppResult<i64> {
    let Some(value) = field_varint(fields, id)? else {
        return Ok(0);
    };
    if value > i64::MAX as u64 {
        return Err(AppError::validation(format!(
            "SEND_GIFT_V2 {label}超出范围"
        )));
    }
    Ok(value as i64)
}

fn decode_send_gift_v2(pb: &str) -> AppResult<Vec<GiftV2>> {
    if pb.is_empty() || pb.len() > MAX_MESSAGE_BYTES {
        return Err(AppError::validation("SEND_GIFT_V2 数据为空或过大"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(pb)
        .map_err(|_| AppError::validation("SEND_GIFT_V2 base64 格式错误"))?;
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE_BYTES {
        return Err(AppError::validation("SEND_GIFT_V2 解码后数据为空或过大"));
    }
    let root = decode_protobuf(&bytes)?;
    let uid = field_varint(&root, 1)?.unwrap_or(0);
    let items = root
        .get(&10)
        .ok_or_else(|| AppError::validation("SEND_GIFT_V2 缺少礼物流"))?;
    if items.is_empty() || items.len() > MAX_GIFT_V2_ITEMS {
        return Err(AppError::validation("SEND_GIFT_V2 礼物流数量非法"));
    }
    let mut gifts = Vec::with_capacity(items.len());
    for item in items {
        let ProtoValue::Bytes(item_bytes) = item else {
            return Err(AppError::validation("SEND_GIFT_V2 礼物流字段类型错误"));
        };
        let fields = decode_protobuf(item_bytes)?;
        let gift_id = required_protobuf_i64(&fields, 1, "礼物 ID")?;
        let num = required_protobuf_i64(&fields, 3, "礼物数量")?;
        let price = optional_protobuf_i64(&fields, 5, "礼物单价")?;
        let total_coin = optional_protobuf_i64(&fields, 7, "礼物总价")?;
        let timestamp = optional_protobuf_i64(&fields, 10, "礼物时间")?;
        if field_varint(&fields, 14)?.unwrap_or(0) > i64::MAX as u64 {
            return Err(AppError::validation("SEND_GIFT_V2 连击总价超出范围"));
        }
        gifts.push(GiftV2 {
            uid,
            gift_id,
            gift_name: field_string(&fields, 2)?.unwrap_or_default(),
            num,
            price,
            total_coin,
            coin_type: field_string(&fields, 8)?.unwrap_or_default(),
            tid: field_string(&fields, 9)?.unwrap_or_default(),
            timestamp,
        });
    }
    Ok(gifts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AddSubscription;
    use std::io::Write;

    fn write_varint(mut value: u64, output: &mut Vec<u8>) {
        while value >= 0x80 {
            output.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        output.push(value as u8);
    }

    fn write_varint_field(field: u32, value: u64, output: &mut Vec<u8>) {
        write_varint((field as u64) << 3, output);
        write_varint(value, output);
    }

    fn write_bytes_field(field: u32, value: &[u8], output: &mut Vec<u8>) {
        write_varint(((field as u64) << 3) | 2, output);
        write_varint(value.len() as u64, output);
        output.extend_from_slice(value);
    }

    fn gift_v2_payload(gift_fields: &[u8]) -> String {
        let mut root = Vec::new();
        write_varint_field(1, 42, &mut root);
        write_bytes_field(10, gift_fields, &mut root);
        base64::engine::general_purpose::STANDARD.encode(root)
    }

    #[test]
    fn websocket_endpoints_prefer_443_and_deduplicate() {
        let hosts = vec![
            serde_json::json!({"host": "regional.example", "wss_port": 2245}),
            serde_json::json!({"host": "regional.example", "wss_port": 2245}),
            serde_json::json!({"host": "port443.example", "wss_port": 443}),
        ];
        assert_eq!(
            websocket_endpoints(&hosts),
            vec![
                "wss://regional.example/sub",
                "wss://regional.example:2245/sub",
                "wss://port443.example/sub",
                "wss://broadcastlv.chat.bilibili.com/sub",
            ]
        );
    }

    #[test]
    fn websocket_hosts_reject_ips_and_invalid_dns_names() {
        for host in [
            "127.0.0.1",
            "[::1]",
            "localhost",
            "router.local",
            "user@evil.example",
            "evil.example/path",
            "bad_host.example",
        ] {
            assert!(normalized_websocket_host(host).is_none(), "{host}");
        }
        assert_eq!(
            normalized_websocket_host("BroadcastLV.Chat.Bilibili.com.").as_deref(),
            Some("broadcastlv.chat.bilibili.com")
        );
    }

    #[test]
    fn online_samples_use_current_count_not_peak_and_never_repeat_stale_values() {
        let mut stats = CollectorStats {
            connection_established: true,
            ..Default::default()
        };
        for count in [100, 70, 0] {
            parse_live_message(
                &mut stats,
                &serde_json::json!({
                    "cmd": "ONLINE_RANK_COUNT", "data": {"online_count": count}
                }),
            );
            assert_eq!(
                stats.take_online_observation(Utc::now()).map(|(_, n)| n),
                Some(count)
            );
            assert!(stats.take_online_observation(Utc::now()).is_none());
        }
        assert_eq!(stats.peak_high_energy, 100);
        parse_live_message(
            &mut stats,
            &serde_json::json!({
                "cmd": "ONLINE_RANK_COUNT", "data": {"count": 44}
            }),
        );
        stats.connection_established = false;
        assert!(stats.take_online_observation(Utc::now()).is_none());
        stats.connection_established = true;
        assert!(stats.take_online_observation(Utc::now()).is_none());
        stats.online_observation = Some((Utc::now(), Instant::now() - Duration::from_secs(96), 8));
        assert!(stats.take_online_observation(Utc::now()).is_none());
    }

    #[tokio::test]
    async fn online_checkpoint_persists_latest_window_then_final_point_and_gap() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&AddSubscription {
                    platform: Platform::Bilibili,
                    handle: None,
                    profile_url: None,
                    uploads_playlist_id: None,
                mid: "345".to_owned(),
                name: Some("测试".to_owned()),
                avatar_url: None,
                room_id: Some("678".to_owned()),
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: true,
            })
            .await
            .unwrap();
        let start = Utc::now() - chrono::Duration::minutes(1);
        let session = store
            .ensure_live_session(streamer.id, "678", "测试", "直播", None, start, start)
            .await
            .unwrap();
        let mut stats = CollectorStats {
            connection_established: true,
            has_connected_once: true,
            ..Default::default()
        };
        for count in [100, 70] {
            parse_live_message(
                &mut stats,
                &serde_json::json!({
                    "cmd": "ONLINE_RANK_COUNT", "data": {"online_count": count}
                }),
            );
        }
        let at = Utc::now();
        save_online_checkpoint(&store, session.id, &mut stats, at, "active").await;
        let final_at = at + chrono::Duration::seconds(30);
        parse_live_message(
            &mut stats,
            &serde_json::json!({"cmd":"ONLINE_RANK_COUNT", "data":{"online_count":0}}),
        );
        save_online_checkpoint(&store, session.id, &mut stats, final_at, "finished").await;
        store
            .finish_live_session(
                session.id,
                final_at,
                &stats.snapshot("complete", "finished"),
            )
            .await
            .unwrap();
        let page = store
            .list_live_online_samples(session.id, 0, 10)
            .await
            .unwrap();
        assert_eq!(page.samples.len(), 2);
        assert_eq!(page.samples[0].high_energy_count, Some(70));
        assert_eq!(page.samples[1].high_energy_count, Some(0));
        assert_eq!(page.samples[1].collector_state, "finished");
        assert_eq!(
            store
                .get_live_session(session.id)
                .await
                .unwrap()
                .peak_high_energy,
            100
        );
        // No synthetic samples are created for times between checkpoints.
        assert!(stats.take_online_observation(final_at).is_none());
    }

    #[test]
    fn invalid_online_messages_are_missing_not_zero() {
        for data in [
            serde_json::json!({}),
            serde_json::json!({"online_count": -1}),
            serde_json::json!({"online_count": "bad"}),
        ] {
            let mut stats = CollectorStats {
                connection_established: true,
                ..Default::default()
            };
            parse_live_message(
                &mut stats,
                &serde_json::json!({"cmd": "ONLINE_RANK_COUNT", "data": data}),
            );
            assert!(stats.take_online_observation(Utc::now()).is_none());
        }
    }

    #[test]
    fn tracks_peak_high_energy_from_online_rank_count() {
        let mut stats = CollectorStats::default();
        parse_live_message(
            &mut stats,
            &serde_json::json!({
                "cmd": "ONLINE_RANK_COUNT",
                "data": {"online_count": 1234, "count": 1200}
            }),
        );
        parse_live_message(
            &mut stats,
            &serde_json::json!({
                "cmd": "ONLINE_RANK_COUNT",
                "data": {"online_count": 1000, "count": 900}
            }),
        );
        assert_eq!(stats.peak_high_energy, 1234);
    }

    #[test]
    fn successful_auth_response_marks_connection_established() {
        let packet = make_packet(8, br#"{"code":0}"#.to_vec());
        let mut stats = CollectorStats::default();
        parse_live_packets(&mut stats, &packet, 0);
        assert!(stats.connection_established);
        assert!(stats.has_connected_once);
        assert_eq!(
            stats.snapshot("complete", "finished").data_quality,
            "complete"
        );
    }

    #[test]
    fn snapshot_without_successful_auth_is_partial() {
        let stats = CollectorStats::default();
        assert_eq!(
            stats.snapshot("complete", "finished").data_quality,
            "partial"
        );
    }

    #[test]
    fn parses_brotli_live_packets() {
        let message = serde_json::json!({
            "cmd": "DANMU_MSG",
            "info": [[], [], ["42"]]
        });
        let inner = make_packet(5, serde_json::to_vec(&message).expect("json"));
        let mut compressed = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            writer.write_all(&inner).expect("compress");
        }
        let mut outer = make_packet(5, compressed);
        outer[6..8].copy_from_slice(&3u16.to_be_bytes());
        let mut stats = CollectorStats::default();
        parse_live_packets(&mut stats, &outer, 0);
        assert_eq!(stats.danmaku_count, 1);
        assert!(stats.interactive_users.contains("42"));
    }

    #[test]
    fn anonymous_gifts_still_contribute_revenue() {
        let mut stats = CollectorStats::default();
        parse_live_message(
            &mut stats,
            &serde_json::json!({
                "cmd": "SEND_GIFT",
                "data": {
                    "uid": 0,
                    "giftId": 1,
                    "giftName": "anonymous gift",
                    "num": 1,
                    "price": 1000,
                    "total_coin": 1000,
                    "coin_type": "gold",
                    "tid": "anonymous-gift-1",
                    "timestamp": 1
                }
            }),
        );
        parse_live_message(
            &mut stats,
            &serde_json::json!({
                "cmd": "SEND_GIFT",
                "data": {
                    "giftId": 2,
                    "giftName": "missing uid gift",
                    "num": 1,
                    "price": 1000,
                    "total_coin": 1000,
                    "coin_type": "gold",
                    "tid": "anonymous-gift-2",
                    "timestamp": 2
                }
            }),
        );

        assert_eq!(stats.gift_count, 2);
        assert_eq!(stats.gift_revenue_cents, 200);
        assert_eq!(stats.revenue_cents, 200);
        assert!(stats.interactive_users.is_empty());
        assert!(stats.paid_users.is_empty());
    }

    #[test]
    fn nested_sender_uid_deduplicates_gifts_super_chats_and_guards() {
        let mut stats = CollectorStats::default();
        for tid in ["gift-1", "gift-2"] {
            parse_live_message(
                &mut stats,
                &serde_json::json!({
                    "cmd": "SEND_GIFT",
                    "data": {
                        "uid": 0, "sender_uinfo": {"uid": 42},
                        "receiver_uinfo": {"uid": 99},
                        "giftId": 1, "num": 1, "price": 1000,
                        "coin_type": "gold", "tid": tid
                    }
                }),
            );
        }
        parse_live_message(
            &mut stats,
            &serde_json::json!({
                "cmd": "SUPER_CHAT_MESSAGE",
                "data": {"id": 11, "uid": 0, "uinfo": {"uid": "42"}, "price": 30}
            }),
        );
        parse_live_message(
            &mut stats,
            &serde_json::json!({
                "cmd": "GUARD_BUY",
                "data": {"uid": "42", "guard_level": 3, "price": 1000, "start_time": 1}
            }),
        );
        assert_eq!(stats.paid_user_count, 1);
        assert_eq!(stats.interactive_user_count, 1);
        assert_eq!(stats.revenue_cents, 3300);
        assert!(!stats.data_degraded);
    }

    #[test]
    fn recipient_uid_never_counts_as_a_payer() {
        let mut stats = CollectorStats {
            has_connected_once: true,
            ..CollectorStats::default()
        };
        parse_live_message(
            &mut stats,
            &serde_json::json!({
                "cmd": "SEND_GIFT",
                "data": {
                    "uid": 0, "sender_uinfo": {"uid": 0}, "receiver_uinfo": {"uid": 99},
                    "giftId": 1, "num": 1, "price": 1000,
                    "coin_type": "gold", "tid": "redacted-sender"
                }
            }),
        );
        assert_eq!(stats.revenue_cents, 100);
        assert_eq!(stats.paid_user_count, 0);
        assert_eq!(
            stats.snapshot("complete", "finished").data_quality,
            "partial"
        );
    }

    #[test]
    fn replayed_gift_events_do_not_inflate_counts_or_revenue() {
        let mut stats = CollectorStats::default();
        let event = serde_json::json!({
            "cmd": "SEND_GIFT",
            "data": {
                "uid": 7,
                "giftId": 1,
                "giftName": "gift",
                "num": 2,
                "price": 500,
                "total_coin": 1000,
                "coin_type": "gold",
                "tid": "replayed-gift",
                "timestamp": 1
            }
        });
        parse_live_message(&mut stats, &event);
        parse_live_message(&mut stats, &event);

        assert_eq!(stats.gift_count, 1);
        assert_eq!(stats.gift_revenue_cents, 100);
        assert_eq!(stats.revenue_cents, 100);
        assert_eq!(stats.interactive_user_count, 1);
        assert_eq!(stats.paid_user_count, 1);
    }

    #[test]
    fn replayed_super_chat_and_guard_events_are_counted_once() {
        let mut stats = CollectorStats::default();
        let super_chat = serde_json::json!({
            "cmd": "SUPER_CHAT_MESSAGE",
            "data": {"id": 11, "uid": 8, "price": 30.0, "time": 1, "message": "sc"}
        });
        parse_live_message(&mut stats, &super_chat);
        parse_live_message(&mut stats, &super_chat);

        let guard = serde_json::json!({
            "cmd": "GUARD_BUY",
            "data": {"uid": 9, "guard_level": 3, "price": 1000, "start_time": 2}
        });
        parse_live_message(&mut stats, &guard);
        parse_live_message(&mut stats, &guard);

        assert_eq!(stats.super_chat_count, 1);
        assert_eq!(stats.super_chat_revenue_cents, 3000);
        assert_eq!(stats.guard_count, 1);
        assert_eq!(stats.guard_revenue_cents, 100);
        assert_eq!(stats.revenue_cents, 3100);
        assert_eq!(stats.interactive_user_count, 2);
        assert_eq!(stats.paid_user_count, 2);
    }

    #[test]
    fn protobuf_strictly_rejects_invalid_wire_types_utf8_and_lengths() {
        let wrong_varint = HashMap::from([(1u32, vec![ProtoValue::Fixed32])]);
        assert!(field_varint(&wrong_varint, 1).is_err());

        let wrong_string = HashMap::from([(2u32, vec![ProtoValue::Varint(1)])]);
        assert!(field_string(&wrong_string, 2).is_err());

        let invalid_utf8 = HashMap::from([(2u32, vec![ProtoValue::Bytes(vec![0xff])])]);
        assert!(field_string(&invalid_utf8, 2).is_err());

        assert!(decode_protobuf(&[0x00, 0x00]).is_err());
        assert!(decode_protobuf(&[0x0a, 0x05, 0x00]).is_err());
    }

    #[test]
    fn send_gift_v2_rejects_wrong_wire_type_invalid_utf8_and_zero_values() {
        let mut wrong_wire = Vec::new();
        write_bytes_field(1, b"not-a-varint", &mut wrong_wire);
        write_varint_field(3, 1, &mut wrong_wire);
        write_varint_field(5, 100, &mut wrong_wire);
        write_varint_field(7, 100, &mut wrong_wire);
        write_varint_field(10, 1, &mut wrong_wire);
        assert!(decode_send_gift_v2(&gift_v2_payload(&wrong_wire)).is_err());

        let mut invalid_utf8 = Vec::new();
        write_varint_field(1, 1, &mut invalid_utf8);
        write_bytes_field(2, &[0xff], &mut invalid_utf8);
        write_varint_field(3, 1, &mut invalid_utf8);
        write_varint_field(5, 100, &mut invalid_utf8);
        write_varint_field(7, 100, &mut invalid_utf8);
        write_varint_field(10, 1, &mut invalid_utf8);
        assert!(decode_send_gift_v2(&gift_v2_payload(&invalid_utf8)).is_err());

        let mut zero_gift = Vec::new();
        write_varint_field(1, 0, &mut zero_gift);
        write_varint_field(3, 1, &mut zero_gift);
        write_varint_field(5, 100, &mut zero_gift);
        write_varint_field(7, 100, &mut zero_gift);
        write_varint_field(10, 1, &mut zero_gift);
        assert!(decode_send_gift_v2(&gift_v2_payload(&zero_gift)).is_err());
    }

    #[test]
    fn send_gift_v2_accepts_a_valid_payload() {
        let mut gift = Vec::new();
        write_varint_field(1, 1, &mut gift);
        write_bytes_field(2, "小心心".as_bytes(), &mut gift);
        write_varint_field(3, 2, &mut gift);
        write_varint_field(5, 100, &mut gift);
        write_varint_field(7, 200, &mut gift);
        write_bytes_field(8, b"gold", &mut gift);
        write_bytes_field(9, b"gift-tid", &mut gift);
        write_varint_field(10, 1700000000, &mut gift);
        let gifts = decode_send_gift_v2(&gift_v2_payload(&gift)).expect("valid gift");
        assert_eq!(gifts.len(), 1);
        assert_eq!(gifts[0].uid, 42);
        assert_eq!(gifts[0].gift_id, 1);
        assert_eq!(gifts[0].num, 2);
        assert_eq!(gifts[0].total_coin, 200);
        assert_eq!(gifts[0].tid, "gift-tid");
    }

    #[test]
    fn bounded_dedup_sets_mark_quality_when_capacity_is_exceeded() {
        let mut set = BoundedSet::new(1, Duration::from_secs(60));
        assert!(set.insert("first".to_owned()));
        assert!(set.insert("second".to_owned()));
        assert!(set.degraded);
        assert!(!set.contains("first"));
        assert!(set.contains("second"));
    }

    #[test]
    fn malformed_live_packet_marks_data_quality_partial() {
        let packet = [0, 0, 0, 20, 0, 16, 0, 1, 0, 0, 0, 5, 0, 0, 0, 1];
        let mut stats = CollectorStats::default();
        parse_live_packets(&mut stats, &packet, 0);
        let snapshot = stats.snapshot("complete", "finished");
        assert_eq!(snapshot.data_quality, "partial");
    }

    #[tokio::test]
    async fn only_a_resumed_session_starts_out_degraded() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = store
            .add_subscription(&AddSubscription {
                    platform: Platform::Bilibili,
                    handle: None,
                    profile_url: None,
                    uploads_playlist_id: None,
                mid: "672328094".to_owned(),
                name: Some("直播完整度测试".to_owned()),
                avatar_url: None,
                room_id: Some("22637261".to_owned()),
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: false,
                live_stats_enabled: true,
            })
            .await
            .expect("streamer");
        let now = Utc::now();
        let fresh = store
            .ensure_live_session(
                streamer.id,
                "22637261",
                &streamer.name,
                "开播",
                None,
                now,
                now,
            )
            .await
            .expect("session");

        let mut stats = CollectorStats::from_session(&fresh, false);
        assert_eq!(
            stats.snapshot("complete", "active").data_quality,
            "partial",
            "还没连上之前不能声称完整"
        );
        stats.has_connected_once = true;
        stats.connection_established = true;
        assert_eq!(
            stats.snapshot("complete", "active").data_quality,
            "complete",
            "全新场次连上之后应当可以给出 complete"
        );

        store
            .finish_open_live_session(fresh.id, now + chrono::Duration::minutes(5))
            .await
            .expect("finish");
        let resumed = store
            .ensure_live_session(
                streamer.id,
                "22637261",
                &streamer.name,
                "开播",
                None,
                now,
                Utc::now(),
            )
            .await
            .expect("resume");
        assert!(
            resumed.updated_at > resumed.created_at,
            "续开的场次应当已经被写过"
        );
        let mut resumed_stats = CollectorStats::from_session(&resumed, true);
        assert!(resumed_stats.take_online_observation(Utc::now()).is_none());
        assert_eq!(
            resumed_stats.snapshot("complete", "finished").data_quality,
            "partial",
            "续开的场次丢了内存里的 UID/去重集合，必须降级"
        );
    }

    #[tokio::test]
    #[ignore = "requires a live Bilibili room and network"]
    async fn real_live_collector_smoke() {
        let room_id =
            std::env::var("OSHICAL_LIVE_SMOKE_ROOM").unwrap_or_else(|_| "22637261".to_owned());
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = store
            .add_subscription(&AddSubscription {
                    platform: Platform::Bilibili,
                    handle: None,
                    profile_url: None,
                    uploads_playlist_id: None,
                mid: "672342685".to_owned(),
                name: Some("直播统计冒烟测试".to_owned()),
                avatar_url: None,
                room_id: Some(room_id.clone()),
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: false,
                live_stats_enabled: true,
            })
            .await
            .expect("streamer");
        let now = Utc::now();
        let session = store
            .ensure_live_session(
                streamer.id,
                &room_id,
                &streamer.name,
                "直播统计冒烟测试",
                None,
                now,
                now,
            )
            .await
            .expect("session");
        let proxy_port = std::env::var("OSHICAL_LIVE_SMOKE_PROXY")
            .ok()
            .and_then(|value| value.parse().ok());
        let bili = Arc::new(BilibiliClient::new_with_proxy(None, proxy_port).expect("bili"));
        let manager = LiveStatsManager::default();
        manager.start(store.clone(), bili, session.clone()).await;
        tokio::time::sleep(Duration::from_secs(15)).await;
        manager.stop(streamer.id, Utc::now()).await;
        let saved = store.get_live_session(session.id).await.expect("saved");
        assert_eq!(saved.collector_state, "finished");
        assert_eq!(saved.reconnect_count, 0, "弹幕连接未成功建立");
        assert!(saved.danmaku_count >= 0);
    }
}
