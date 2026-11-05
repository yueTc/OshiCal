use std::collections::{HashMap, HashSet};
use std::io::Read;
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
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    client_async_tls_with_config, connect_async, MaybeTlsStream, WebSocketStream,
};

use crate::bilibili::BilibiliClient;
use crate::error::{AppError, AppResult};
use crate::models::{LiveSession, LiveSessionStats};
use crate::store::SqliteStore;

const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const MAX_DECOMPRESSED_BYTES: usize = 8 * 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_PROXY_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Clone, Default)]
pub struct LiveStatsManager {
    tasks: Arc<Mutex<HashMap<i64, TaskHandle>>>,
}

struct TaskHandle {
    stop: Option<oneshot::Sender<DateTime<Utc>>>,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct CollectorStats {
    peak_high_energy: i64,
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
    interactive_users: HashSet<String>,
    paid_users: HashSet<String>,
    dedup: HashSet<String>,
    connection_established: bool,
}

impl CollectorStats {
    fn from_session(session: &LiveSession) -> Self {
        Self {
            peak_high_energy: session.peak_high_energy as i64,
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
            ..Self::default()
        }
    }

    fn snapshot(&self, quality: &str, state: &str) -> LiveSessionStats {
        LiveSessionStats {
            peak_high_energy: self.peak_high_energy,
            revenue_cents: self.revenue_cents,
            gift_revenue_cents: self.gift_revenue_cents,
            super_chat_revenue_cents: self.super_chat_revenue_cents,
            guard_revenue_cents: self.guard_revenue_cents,
            paid_user_count: self.base_paid_user_count + self.paid_users.len() as i64,
            interactive_user_count: self.base_interactive_user_count
                + self.interactive_users.len() as i64,
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
    pub async fn start(&self, store: SqliteStore, bili: Arc<BilibiliClient>, session: LiveSession) {
        let streamer_id = session.streamer_id;
        let mut tasks = self.tasks.lock().await;
        if let Some(existing) = tasks.get(&streamer_id) {
            if !existing.handle.is_finished() {
                return;
            }
        }
        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = tokio::spawn(run_collector(store, bili, session, stop_rx));
        tasks.insert(
            streamer_id,
            TaskHandle {
                stop: Some(stop_tx),
                handle,
            },
        );
    }

    pub async fn stop(&self, streamer_id: i64, ended_at: DateTime<Utc>) {
        let entry = self.tasks.lock().await.remove(&streamer_id);
        let Some(mut entry) = entry else {
            return;
        };
        if let Some(stop) = entry.stop.take() {
            let _ = stop.send(ended_at);
        }
        if timeout(Duration::from_secs(8), &mut entry.handle)
            .await
            .is_err()
        {
            tracing::warn!(streamer_id, "直播数据采集器停止超时");
        }
    }
}

async fn run_collector(
    store: SqliteStore,
    bili: Arc<BilibiliClient>,
    session: LiveSession,
    mut stop_rx: oneshot::Receiver<DateTime<Utc>>,
) {
    let mut stats = CollectorStats::from_session(&session);
    let mut retry_delay = RECONNECT_DELAY;
    loop {
        match collect_once(&store, &bili, &session, &mut stats, &mut stop_rx).await {
            CollectResult::Stopped(ended_at) => {
                let quality = if stats.reconnect_count == 0 {
                    "complete"
                } else {
                    "partial"
                };
                if let Err(error) = store
                    .finish_live_session(session.id, ended_at, &stats.snapshot(quality, "finished"))
                    .await
                {
                    tracing::warn!(%error, session_id = session.id, "保存直播数据失败");
                }
                return;
            }
            CollectResult::Disconnected(error) => {
                stats.reconnect_count += 1;
                let current_delay = retry_delay;
                if stats.connection_established {
                    retry_delay = RECONNECT_DELAY;
                } else {
                    retry_delay = (retry_delay * 2).min(MAX_RECONNECT_DELAY);
                }
                stats.connection_established = false;
                if let Err(store_error) = store
                    .update_live_session_stats(
                        session.id,
                        &stats.snapshot("partial", "reconnecting"),
                    )
                    .await
                {
                    tracing::warn!(%store_error, session_id = session.id, "保存直播采集断点失败");
                }
                tracing::warn!(
                    %error,
                    session_id = session.id,
                    retry_in_seconds = current_delay.as_secs(),
                    "直播数据连接中断，准备重连"
                );
                tokio::select! {
                    _ = &mut stop_rx => {
                        let quality = if stats.reconnect_count == 0 { "complete" } else { "partial" };
                        let _ = store.finish_live_session(
                            session.id,
                            Utc::now(),
                            &stats.snapshot(quality, "finished"),
                        ).await;
                        return;
                    }
                    _ = tokio::time::sleep(current_delay) => {}
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
) -> CollectResult {
    let websocket_proxy = bili.websocket_proxy().map(ToOwned::to_owned);
    let info = match bili.fetch_live_danmu_info(&session.room_id).await {
        Ok(value) => value,
        Err(error) => return CollectResult::Disconnected(error.to_string()),
    };
    let token = match info.pointer("/data/token").and_then(Value::as_str) {
        Some(value) => value,
        None => return CollectResult::Disconnected("弹幕服务器缺少 token".to_owned()),
    };
    let hosts = match info.pointer("/data/host_list").and_then(Value::as_array) {
        Some(value) => value,
        None => return CollectResult::Disconnected("弹幕服务器列表为空".to_owned()),
    };

    for url in websocket_endpoints(hosts) {
        let socket = match timeout(
            Duration::from_secs(12),
            connect_live_socket(&url, websocket_proxy.as_deref()),
        )
        .await
        {
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
        let mut socket = socket;
        let buvid = bili.buvid().await.ok();
        let mut auth = serde_json::json!({
            "uid": 0,
            "roomid": session.room_id.parse::<u64>().unwrap_or_default(),
            "protover": 3,
            "platform": "web",
            "type": 2,
            "key": token,
        });
        if let Some(buvid) = buvid {
            auth["buvid"] = Value::String(buvid);
        }
        if let Err(error) = socket
            .send(Message::Binary(
                make_packet(7, serde_json::to_vec(&auth).unwrap_or_default()).into(),
            ))
            .await
        {
            return CollectResult::Disconnected(error.to_string());
        }

        let mut heartbeat = interval(HEARTBEAT_INTERVAL);
        let mut checkpoint = interval(CHECKPOINT_INTERVAL);
        heartbeat.tick().await;
        checkpoint.tick().await;
        let mut last_checkpoint = Instant::now();

        loop {
            tokio::select! {
                value = &mut *stop_rx => {
                    let ended_at = value.unwrap_or_else(|_| Utc::now());
                    let _ = store.update_live_session_stats(
                        session.id,
                        &stats.snapshot("collecting", "active"),
                    ).await;
                    return CollectResult::Stopped(ended_at);
                }
                _ = heartbeat.tick() => {
                    if let Err(error) = socket.send(Message::Binary(make_packet(2, Vec::new()).into())).await {
                        return CollectResult::Disconnected(error.to_string());
                    }
                }
                _ = checkpoint.tick() => {
                    if last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
                        last_checkpoint = Instant::now();
                        if let Err(error) = store.update_live_session_stats(
                            session.id,
                            &stats.snapshot("collecting", "active"),
                        ).await {
                            tracing::warn!(%error, session_id = session.id, "保存直播统计检查点失败");
                        }
                    }
                }
                message = socket.next() => {
                    match message {
                        Some(Ok(Message::Binary(bytes))) => {
                            let was_established = stats.connection_established;
                            parse_live_packets(stats, &bytes, 0);
                            if !was_established && stats.connection_established {
                                if let Err(error) = socket
                                    .send(Message::Binary(make_packet(2, Vec::new()).into()))
                                    .await
                                {
                                    return CollectResult::Disconnected(error.to_string());
                                }
                            }
                        }
                        Some(Ok(Message::Text(text))) => {
                            if text.len() <= MAX_MESSAGE_BYTES {
                                if let Ok(value) = serde_json::from_str::<Value>(text.as_str()) {
                                    parse_live_message(stats, &value);
                                }
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = socket.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            return CollectResult::Disconnected("直播弹幕连接关闭".to_owned());
                        }
                        Some(Err(error)) => {
                            return CollectResult::Disconnected(error.to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    CollectResult::Disconnected("所有弹幕服务器均连接失败".to_owned())
}

fn websocket_target(url: &str) -> Result<(String, u16), String> {
    let target =
        url::Url::parse(url).map_err(|error| format!("弹幕服务器地址无效（{url}）: {error}"))?;
    let host = target
        .host_str()
        .ok_or_else(|| "弹幕服务器地址缺少主机名".to_owned())?
        .to_owned();
    let port = target.port_or_known_default().unwrap_or(443);
    Ok((host, port))
}

async fn connect_proxy_tunnel(url: &str, proxy: &str) -> Result<TcpStream, String> {
    let proxy_url =
        url::Url::parse(proxy).map_err(|error| format!("弹幕代理地址无效（{proxy}）: {error}"))?;
    if proxy_url.scheme() != "http" {
        return Err(format!(
            "弹幕 WebSocket 当前仅支持 HTTP CONNECT 代理，收到 {}",
            proxy_url.scheme()
        ));
    }
    let proxy_host = proxy_url
        .host_str()
        .ok_or_else(|| "弹幕代理地址缺少主机名".to_owned())?;
    let proxy_port = proxy_url.port_or_known_default().unwrap_or(8080);
    let (target_host, target_port) = websocket_target(url)?;

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
    Ok(stream)
}

async fn connect_live_socket(
    url: &str,
    proxy: Option<&str>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    if let Some(proxy) = proxy {
        let request = url
            .into_client_request()
            .map_err(|error| format!("创建弹幕 WebSocket 请求失败: {error}"))?;
        let stream = connect_proxy_tunnel(url, proxy).await?;
        return client_async_tls_with_config(request, stream, None, None)
            .await
            .map(|(socket, _)| socket)
            .map_err(|error| format!("经代理建立弹幕 TLS/WebSocket 失败: {error}"));
    }
    connect_async(url)
        .await
        .map(|(socket, _)| socket)
        .map_err(|error| error.to_string())
}

fn websocket_endpoints(hosts: &[Value]) -> Vec<String> {
    let mut endpoints = Vec::new();
    let mut seen = HashSet::new();
    for host in hosts {
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
    let host = host.trim();
    if host.is_empty() || port == 0 {
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
    if depth > 4 || bytes.len() < 16 {
        return;
    }
    let mut offset = 0;
    while offset + 16 <= bytes.len() {
        let packet_len =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap_or([0; 4])) as usize;
        if packet_len < 16 || offset + packet_len > bytes.len() {
            return;
        }
        let header_len =
            u16::from_be_bytes(bytes[offset + 4..offset + 6].try_into().unwrap_or([0; 2])) as usize;
        let version =
            u16::from_be_bytes(bytes[offset + 6..offset + 8].try_into().unwrap_or([0; 2]));
        let operation =
            u32::from_be_bytes(bytes[offset + 8..offset + 12].try_into().unwrap_or([0; 4]));
        if header_len < 16 || offset + header_len > offset + packet_len {
            return;
        }
        let body = &bytes[offset + header_len..offset + packet_len];
        if operation == 8 {
            if serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|value| value.get("code").and_then(Value::as_i64))
                == Some(0)
            {
                stats.connection_established = true;
            }
        } else if operation == 5 {
            match version {
                2 => {
                    let decoder = ZlibDecoder::new(body);
                    let mut decoded = Vec::new();
                    if decoder
                        .take(MAX_DECOMPRESSED_BYTES as u64)
                        .read_to_end(&mut decoded)
                        .is_ok()
                    {
                        parse_live_packets(stats, &decoded, depth + 1);
                    }
                }
                3 => {
                    let decoder = brotli::Decompressor::new(body, 4096);
                    let mut decoded = Vec::new();
                    if decoder
                        .take(MAX_DECOMPRESSED_BYTES as u64)
                        .read_to_end(&mut decoded)
                        .is_ok()
                    {
                        parse_live_packets(stats, &decoded, depth + 1);
                    }
                }
                _ => {
                    if let Ok(value) = serde_json::from_slice::<Value>(body) {
                        parse_live_message(stats, &value);
                    }
                }
            }
        }
        offset += packet_len;
    }
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
            let current = data
                .get("online_count")
                .and_then(Value::as_i64)
                .or_else(|| data.get("count").and_then(Value::as_i64))
                .unwrap_or(0)
                .max(0);
            stats.peak_high_energy = stats.peak_high_energy.max(current);
        }
        "DANMU_MSG" => {
            stats.danmaku_count += 1;
            if let Some(uid) = value.pointer("/info/2/0").and_then(value_to_string) {
                stats.interactive_users.insert(uid);
            }
        }
        "SEND_GIFT" => {
            stats.gift_count += 1;
            add_gift(
                stats,
                data.get("uid").and_then(value_to_string),
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
                return;
            };
            for gift in decode_send_gift_v2(pb) {
                stats.gift_count += 1;
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
            stats.super_chat_count += 1;
            let uid = data.get("uid").and_then(value_to_string);
            if let Some(uid) = uid.as_ref() {
                stats.interactive_users.insert(uid.clone());
            }
            let amount = data.get("price").and_then(Value::as_f64).unwrap_or(0.0);
            let cents = (amount * 100.0).round() as i64;
            if cents > 0 {
                stats.revenue_cents += cents;
                stats.super_chat_revenue_cents += cents;
                if let Some(uid) = uid {
                    stats.paid_users.insert(uid);
                }
            }
        }
        "GUARD_BUY" => {
            stats.guard_count += 1;
            let uid = data.get("uid").and_then(value_to_string);
            if let Some(uid) = uid.as_ref() {
                stats.interactive_users.insert(uid.clone());
            }
            let cents = data.get("price").and_then(Value::as_i64).unwrap_or(0) / 10;
            if cents > 0 {
                stats.revenue_cents += cents;
                stats.guard_revenue_cents += cents;
                if let Some(uid) = uid {
                    stats.paid_users.insert(uid);
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
    let Some(uid) = uid else {
        return;
    };
    stats.interactive_users.insert(uid.clone());
    let coins = if total_coin > 0 {
        total_coin
    } else {
        price * num
    };
    let cents = if coin_type == "silver" { 0 } else { coins / 10 };
    let key = if tid.is_empty() {
        format!("{uid}:{gift_id}:{num}:{timestamp}:{price}:{gift_name}")
    } else {
        format!("gift:{tid}")
    };
    if cents > 0 && stats.dedup.insert(key) {
        stats.revenue_cents += cents;
        stats.gift_revenue_cents += cents;
        stats.paid_users.insert(uid);
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
}

fn decode_protobuf(bytes: &[u8]) -> AppResult<HashMap<u32, Vec<ProtoValue>>> {
    let mut fields = HashMap::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = decode_varint(bytes, &mut offset)
            .ok_or_else(|| AppError::validation("非法 protobuf tag"))?;
        let field = (tag >> 3) as u32;
        let wire = (tag & 7) as u8;
        let value = match wire {
            0 => ProtoValue::Varint(
                decode_varint(bytes, &mut offset)
                    .ok_or_else(|| AppError::validation("非法 protobuf varint"))?,
            ),
            1 => {
                let end = (offset + 8).min(bytes.len());
                let value = bytes[offset..end].to_vec();
                offset = end;
                ProtoValue::Bytes(value)
            }
            2 => {
                let len = decode_varint(bytes, &mut offset)
                    .ok_or_else(|| AppError::validation("非法 protobuf 长度"))?
                    as usize;
                let end = offset + len;
                if end > bytes.len() {
                    return Err(AppError::validation("protobuf 长度越界"));
                }
                let value = bytes[offset..end].to_vec();
                offset = end;
                ProtoValue::Bytes(value)
            }
            5 => {
                let end = (offset + 4).min(bytes.len());
                let value = bytes[offset..end].to_vec();
                offset = end;
                ProtoValue::Bytes(value)
            }
            _ => return Err(AppError::validation("不支持的 protobuf wire type")),
        };
        fields.entry(field).or_insert_with(Vec::new).push(value);
    }
    Ok(fields)
}

fn decode_varint(bytes: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    while *offset < bytes.len() && shift < 64 {
        let byte = bytes[*offset];
        *offset += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn field_varint(fields: &HashMap<u32, Vec<ProtoValue>>, id: u32) -> u64 {
    match fields.get(&id).and_then(|values| values.first()) {
        Some(ProtoValue::Varint(value)) => *value,
        _ => 0,
    }
}

fn field_string(fields: &HashMap<u32, Vec<ProtoValue>>, id: u32) -> String {
    match fields.get(&id).and_then(|values| values.first()) {
        Some(ProtoValue::Bytes(value)) => String::from_utf8_lossy(value).into_owned(),
        _ => String::new(),
    }
}

fn decode_send_gift_v2(pb: &str) -> Vec<GiftV2> {
    if pb.len() > MAX_MESSAGE_BYTES {
        return Vec::new();
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(pb) else {
        return Vec::new();
    };
    let Ok(root) = decode_protobuf(&bytes) else {
        return Vec::new();
    };
    let uid = field_varint(&root, 1);
    let mut gifts = Vec::new();
    let Some(items) = root.get(&10) else {
        return gifts;
    };
    for item in items {
        let ProtoValue::Bytes(item_bytes) = item else {
            continue;
        };
        let Ok(fields) = decode_protobuf(item_bytes) else {
            continue;
        };
        gifts.push(GiftV2 {
            uid,
            gift_id: field_varint(&fields, 1) as i64,
            gift_name: field_string(&fields, 2),
            num: field_varint(&fields, 3) as i64,
            price: field_varint(&fields, 5) as i64,
            total_coin: field_varint(&fields, 7) as i64,
            coin_type: field_string(&fields, 8),
            tid: field_string(&fields, 9),
            timestamp: field_varint(&fields, 10) as i64,
        });
    }
    gifts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AddSubscription;
    use std::io::Write;

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

    #[tokio::test]
    #[ignore = "requires a live Bilibili room and network"]
    async fn real_live_collector_smoke() {
        let room_id =
            std::env::var("OSHICAL_LIVE_SMOKE_ROOM").unwrap_or_else(|_| "22637261".to_owned());
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = store
            .add_subscription(&AddSubscription {
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
