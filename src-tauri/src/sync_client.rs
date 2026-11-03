use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use oshical_core::{
    LiveSession, LiveStatus, NewSchedule, Schedule, SourceSuppression, SqliteStore, Streamer,
    UpdateSchedule, WeeklyScheduleRule,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{sqlite::Sqlite, Row, SqlitePool, Transaction};

const SYNC_SCHEMA_VERSION: u32 = 2;

const SYNC_SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS device_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    device_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_meta (
    server_key TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    cursor_seq INTEGER NOT NULL DEFAULT 0,
    latest_seq INTEGER NOT NULL DEFAULT 0,
    last_snapshot_revision INTEGER NOT NULL DEFAULT 0,
    last_ack_seq INTEGER NOT NULL DEFAULT 0,
    last_sync_at TEXT,
    status TEXT NOT NULL DEFAULT 'idle',
    initialized INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS sync_entity_cache (
    server_key TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    entity_revision INTEGER NOT NULL DEFAULT 0,
    operation TEXT NOT NULL,
    payload_json TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (server_key, entity_type, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_sync_entity_cache_type
ON sync_entity_cache(server_key, entity_type, entity_revision);
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncChange {
    pub seq: i64,
    pub entity_type: String,
    pub entity_id: String,
    pub operation: String,
    #[serde(default)]
    pub entity_revision: i64,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncBatch {
    #[serde(default)]
    pub from_seq: i64,
    #[serde(default)]
    pub to_seq: i64,
    #[serde(default)]
    pub latest_seq: i64,
    #[serde(default)]
    pub changes: Vec<SyncChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncChangesPage {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub from_seq: i64,
    #[serde(default)]
    pub to_seq: i64,
    #[serde(default)]
    pub latest_seq: i64,
    #[serde(default)]
    pub min_retained_seq: i64,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub reset_required: bool,
    #[serde(default)]
    pub changes: Vec<SyncChange>,
}

const fn default_schema_version() -> u32 {
    SYNC_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncBootstrap {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub snapshot_revision: i64,
    #[serde(default)]
    pub min_retained_seq: i64,
    pub server_time: Option<String>,
    #[serde(default)]
    pub subscriptions: Vec<Value>,
    #[serde(default)]
    pub schedules: Vec<Value>,
    #[serde(default)]
    pub weekly_rules: Vec<Value>,
    #[serde(default)]
    pub weekly_exceptions: Vec<Value>,
    #[serde(default)]
    pub schedule_sources: Vec<Value>,
    #[serde(default)]
    pub source_suppressions: Vec<Value>,
    #[serde(default)]
    pub server_settings: Value,
    #[serde(default)]
    pub live_statuses: Vec<Value>,
    #[serde(default)]
    pub live_sessions: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncMeta {
    pub server_key: String,
    pub device_id: String,
    pub cursor_seq: i64,
    pub latest_seq: i64,
    pub last_snapshot_revision: i64,
    pub last_ack_seq: i64,
    pub last_sync_at: Option<String>,
    pub status: String,
    pub initialized: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplySyncBatchResult {
    pub applied: usize,
    pub cursor_seq: i64,
    pub latest_seq: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplySyncBootstrapResult {
    pub applied: usize,
    pub cursor_seq: i64,
    pub snapshot_revision: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRegistration {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub last_ack_seq: i64,
    #[serde(default)]
    pub online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshJobAccepted {
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    ok: bool,
    data: Option<T>,
    error: Option<EnvelopeError>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeError {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkPayload {
    streamer_id: i64,
    mid: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    avatar_url: String,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WeeklyExceptionPayload {
    rule_id: i64,
    occurrence_date: String,
    exception_kind: String,
    schedule_id: Option<i64>,
    #[serde(default)]
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleSourcePayload {
    schedule_id: i64,
    source_kind: String,
    source_post_id: Option<String>,
    source_image_url: Option<String>,
    source_fingerprint: Option<String>,
    source_identity: String,
    #[serde(default = "default_confidence")]
    confidence: f64,
    #[serde(default)]
    created_at: Option<String>,
}

const fn default_confidence() -> f64 {
    1.0
}

pub async fn ensure_schema(store: &SqliteStore) -> Result<(), String> {
    sqlx::raw_sql(SYNC_SCHEMA)
        .execute(store.pool())
        .await
        .map_err(db_error)?;
    ensure_column(
        store.pool(),
        "sync_meta",
        "latest_seq",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    ensure_column(
        store.pool(),
        "sync_meta",
        "initialized",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    Ok(())
}

async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), String> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .map_err(db_error)?;
    if rows
        .iter()
        .any(|row| row.get::<String, _>("name") == column)
    {
        return Ok(());
    }
    sqlx::query(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
    ))
    .execute(pool)
    .await
    .map_err(db_error)?;
    Ok(())
}

pub fn normalize_server_key(server_url: &str) -> Result<String, String> {
    let value = server_url.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err("服务器地址不能为空".to_owned());
    }
    let parsed = url::Url::parse(value).map_err(|error| format!("服务器地址无效: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("服务器地址必须使用 http 或 https".to_owned());
    }
    Ok(value.to_owned())
}

pub async fn device_id(store: &SqliteStore) -> Result<String, String> {
    ensure_schema(store).await?;
    if let Some(value) =
        sqlx::query_scalar::<_, String>("SELECT device_id FROM device_identity WHERE singleton = 1")
            .fetch_optional(store.pool())
            .await
            .map_err(db_error)?
    {
        return Ok(value);
    }

    let generated = generate_device_id();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO device_identity(singleton, device_id, created_at) VALUES(1, ?, ?)
         ON CONFLICT(singleton) DO NOTHING",
    )
    .bind(&generated)
    .bind(&now)
    .execute(store.pool())
    .await
    .map_err(db_error)?;
    sqlx::query_scalar::<_, String>("SELECT device_id FROM device_identity WHERE singleton = 1")
        .fetch_one(store.pool())
        .await
        .map_err(db_error)
}

pub async fn set_device_id(store: &SqliteStore, device_id_value: &str) -> Result<(), String> {
    ensure_schema(store).await?;
    let device_id_value = device_id_value.trim();
    if device_id_value.is_empty() {
        return Err("设备 ID 不能为空".to_owned());
    }
    sqlx::query(
        "INSERT INTO device_identity(singleton, device_id, created_at) VALUES(1, ?, ?)
         ON CONFLICT(singleton) DO UPDATE SET device_id = excluded.device_id",
    )
    .bind(device_id_value)
    .bind(Utc::now().to_rfc3339())
    .execute(store.pool())
    .await
    .map_err(db_error)?;
    Ok(())
}

pub async fn get_sync_meta(store: &SqliteStore, server_url: &str) -> Result<SyncMeta, String> {
    ensure_schema(store).await?;
    let server_key = normalize_server_key(server_url)?;
    let device_id = device_id(store).await?;
    sqlx::query(
        "INSERT INTO sync_meta(server_key, device_id) VALUES(?, ?)
         ON CONFLICT(server_key) DO UPDATE SET device_id = excluded.device_id",
    )
    .bind(&server_key)
    .bind(&device_id)
    .execute(store.pool())
    .await
    .map_err(db_error)?;
    read_meta(store.pool(), &server_key).await
}

pub async fn set_sync_ack(
    store: &SqliteStore,
    server_url: &str,
    seq: i64,
) -> Result<SyncMeta, String> {
    ensure_schema(store).await?;
    if seq < 0 {
        return Err("同步 ACK 序号不能为负数".to_owned());
    }
    let server_key = normalize_server_key(server_url)?;
    let device_id = device_id(store).await?;
    sqlx::query(
        "INSERT INTO sync_meta(server_key, device_id, last_ack_seq) VALUES(?, ?, ?)
         ON CONFLICT(server_key) DO UPDATE SET
            device_id = excluded.device_id,
            last_ack_seq = MAX(sync_meta.last_ack_seq, excluded.last_ack_seq)",
    )
    .bind(&server_key)
    .bind(&device_id)
    .bind(seq)
    .execute(store.pool())
    .await
    .map_err(db_error)?;
    read_meta(store.pool(), &server_key).await
}

pub async fn apply_batch(
    store: &SqliteStore,
    server_url: &str,
    batch: &SyncBatch,
) -> Result<ApplySyncBatchResult, String> {
    ensure_schema(store).await?;
    let server_key = normalize_server_key(server_url)?;
    let device_id = device_id(store).await?;
    if batch.changes.iter().any(|change| change.seq <= 0) {
        return Err("同步批次包含非法序号".to_owned());
    }

    let mut tx = store
        .pool()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(db_error)?;
    ensure_meta_row(&mut tx, &server_key, &device_id).await?;
    let meta = read_meta_tx(&mut tx, &server_key).await?;
    let mut cursor = meta.cursor_seq;
    let mut applied = 0_usize;

    for change in &batch.changes {
        if change.seq <= cursor {
            continue;
        }
        if change.seq != cursor + 1 {
            return Err(format!(
                "SYNC_GAP: expected {}, received {}",
                cursor + 1,
                change.seq
            ));
        }
        apply_change(&mut tx, &server_key, change).await?;
        cursor = change.seq;
        applied += 1;
    }

    let now = Utc::now().to_rfc3339();
    let latest = batch.latest_seq.max(meta.latest_seq).max(cursor);
    sqlx::query(
        "UPDATE sync_meta SET cursor_seq = ?, latest_seq = ?, last_sync_at = ?,
            status = ?, initialized = CASE WHEN ? > 0 OR initialized = 1 THEN 1 ELSE 0 END
         WHERE server_key = ?",
    )
    .bind(cursor)
    .bind(latest)
    .bind(&now)
    .bind(if applied > 0 { "synced" } else { "idle" })
    .bind(cursor)
    .bind(&server_key)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;

    Ok(ApplySyncBatchResult {
        applied,
        cursor_seq: cursor,
        latest_seq: latest,
    })
}

pub async fn apply_bootstrap(
    store: &SqliteStore,
    server_url: &str,
    bootstrap: &SyncBootstrap,
) -> Result<ApplySyncBootstrapResult, String> {
    ensure_schema(store).await?;
    if bootstrap.snapshot_revision < 0 {
        return Err("快照 revision 不能为负数".to_owned());
    }
    let server_key = normalize_server_key(server_url)?;
    let device_id = device_id(store).await?;
    let mut tx = store
        .pool()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(db_error)?;
    ensure_meta_row(&mut tx, &server_key, &device_id).await?;

    // Server mode treats this desktop database as a rebuildable mirror. Entity IDs
    // match the server so sync changes can be applied directly after bootstrap.
    for statement in [
        "DELETE FROM schedules",
        "DELETE FROM weekly_schedule_exceptions",
        "DELETE FROM weekly_schedule_rules",
        "DELETE FROM live_sessions",
        "DELETE FROM source_suppressions",
        "DELETE FROM linked_accounts",
        "DELETE FROM streamers",
    ] {
        sqlx::query(statement)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
    }
    sqlx::query("DELETE FROM sync_entity_cache WHERE server_key = ?")
        .bind(&server_key)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    sqlx::query("DELETE FROM remote_entity_map WHERE server_key = ?")
        .bind(&server_key)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;

    let mut applied = 0_usize;
    for payload in &bootstrap.subscriptions {
        apply_upsert(&mut tx, &server_key, "subscription", payload, 0).await?;
        applied += 1;
    }
    for payload in &bootstrap.weekly_rules {
        apply_upsert(&mut tx, &server_key, "weekly_rule", payload, 0).await?;
        applied += 1;
    }
    for payload in &bootstrap.weekly_exceptions {
        apply_upsert(&mut tx, &server_key, "weekly_exception", payload, 0).await?;
        applied += 1;
    }
    for payload in &bootstrap.schedules {
        apply_upsert(&mut tx, &server_key, "schedule", payload, 0).await?;
        applied += 1;
    }
    for payload in &bootstrap.schedule_sources {
        apply_upsert(&mut tx, &server_key, "schedule_source", payload, 0).await?;
        applied += 1;
    }
    for payload in &bootstrap.source_suppressions {
        apply_upsert(&mut tx, &server_key, "source_suppression", payload, 0).await?;
        applied += 1;
    }
    for payload in &bootstrap.live_statuses {
        apply_upsert(&mut tx, &server_key, "live_status", payload, 0).await?;
        applied += 1;
    }
    for payload in &bootstrap.live_sessions {
        apply_upsert(&mut tx, &server_key, "live_session", payload, 0).await?;
        applied += 1;
    }
    if !bootstrap.server_settings.is_null() {
        apply_upsert(
            &mut tx,
            &server_key,
            "server_settings",
            &bootstrap.server_settings,
            0,
        )
        .await?;
        applied += 1;
    }

    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE sync_meta SET cursor_seq = ?, latest_seq = MAX(latest_seq, ?),
            last_snapshot_revision = ?, last_sync_at = ?, status = 'synced', initialized = 1
         WHERE server_key = ?",
    )
    .bind(bootstrap.snapshot_revision)
    .bind(bootstrap.snapshot_revision)
    .bind(bootstrap.snapshot_revision)
    .bind(&now)
    .bind(&server_key)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;

    Ok(ApplySyncBootstrapResult {
        applied,
        cursor_seq: bootstrap.snapshot_revision,
        snapshot_revision: bootstrap.snapshot_revision,
    })
}

async fn apply_change(
    tx: &mut Transaction<'_, Sqlite>,
    server_key: &str,
    change: &SyncChange,
) -> Result<(), String> {
    match change.operation.as_str() {
        "upsert" => {
            let payload = change
                .payload
                .as_ref()
                .ok_or_else(|| format!("{} 的 upsert 缺少 payload", change.entity_type))?;
            apply_upsert(
                tx,
                server_key,
                &change.entity_type,
                payload,
                change.entity_revision,
            )
            .await?;
        }
        "delete" => {
            apply_delete(
                tx,
                server_key,
                &change.entity_type,
                &change.entity_id,
                change.payload.as_ref(),
            )
            .await?;
        }
        other => return Err(format!("不支持的同步操作: {other}")),
    }
    Ok(())
}

async fn apply_upsert(
    tx: &mut Transaction<'_, Sqlite>,
    server_key: &str,
    entity_type: &str,
    payload: &Value,
    revision: i64,
) -> Result<(), String> {
    let entity_id = entity_id_from_payload(entity_type, payload)?;
    match entity_type {
        "subscription" => apply_subscription(tx, payload).await?,
        "link" | "linked_account" => apply_link(tx, payload).await?,
        "schedule" => apply_schedule(tx, payload).await?,
        "weekly_rule" => apply_weekly_rule(tx, payload).await?,
        "weekly_exception" => apply_weekly_exception(tx, payload).await?,
        "schedule_source" => apply_schedule_source(tx, payload).await?,
        "source_suppression" => apply_source_suppression(tx, payload).await?,
        "live_status" => apply_live_status(tx, payload).await?,
        "live_session" => apply_live_session(tx, payload).await?,
        "server_settings" => apply_server_settings(tx, payload).await?,
        "server_secret" | "sync_reset" => {}
        other => return Err(format!("不支持的同步实体类型: {other}")),
    }
    upsert_cache(
        tx,
        server_key,
        entity_type,
        &entity_id,
        revision,
        "upsert",
        Some(payload),
    )
    .await
}

async fn apply_delete(
    tx: &mut Transaction<'_, Sqlite>,
    server_key: &str,
    entity_type: &str,
    entity_id: &str,
    payload: Option<&Value>,
) -> Result<(), String> {
    match entity_type {
        "subscription" => {
            let id = parse_entity_id(entity_id, "subscription")?;
            sqlx::query("DELETE FROM streamers WHERE id = ?")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(db_error)?;
        }
        "link" | "linked_account" => {
            if let Some(payload) = payload {
                if let Ok(link) = serde_json::from_value::<LinkPayload>(payload.clone()) {
                    sqlx::query("DELETE FROM linked_accounts WHERE streamer_id = ? AND mid = ?")
                        .bind(link.streamer_id)
                        .bind(&link.mid)
                        .execute(&mut **tx)
                        .await
                        .map_err(db_error)?;
                }
            } else if let Some((streamer_id, mid)) = entity_id.split_once(':') {
                let streamer_id = streamer_id
                    .parse::<i64>()
                    .map_err(|_| "link tombstone 的 streamerId 无效".to_owned())?;
                sqlx::query("DELETE FROM linked_accounts WHERE streamer_id = ? AND mid = ?")
                    .bind(streamer_id)
                    .bind(mid)
                    .execute(&mut **tx)
                    .await
                    .map_err(db_error)?;
            }
        }
        "schedule" => {
            let id = parse_entity_id(entity_id, "schedule")?;
            sqlx::query("DELETE FROM schedules WHERE id = ?")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(db_error)?;
        }
        "weekly_rule" => {
            let id = parse_entity_id(entity_id, "weekly_rule")?;
            sqlx::query("DELETE FROM weekly_schedule_rules WHERE id = ?")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(db_error)?;
        }
        "weekly_exception" => {
            let id = parse_entity_id(entity_id, "weekly_exception")?;
            sqlx::query("DELETE FROM weekly_schedule_exceptions WHERE id = ?")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(db_error)?;
        }
        "schedule_source" => {
            let id = parse_entity_id(entity_id, "schedule_source")?;
            sqlx::query("DELETE FROM schedule_sources WHERE id = ?")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(db_error)?;
        }
        "source_suppression" => {
            let id = parse_entity_id(entity_id, "source_suppression")?;
            sqlx::query("DELETE FROM source_suppressions WHERE id = ?")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(db_error)?;
        }
        "live_status" => {
            let id = parse_entity_id(entity_id, "live_status")?;
            sqlx::query("UPDATE streamers SET live_is_live = 0 WHERE id = ?")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(db_error)?;
        }
        "live_session" => {
            let id = parse_entity_id(entity_id, "live_session")?;
            sqlx::query("DELETE FROM live_sessions WHERE id = ?")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(db_error)?;
        }
        "server_settings" | "server_secret" | "sync_reset" => {}
        other => return Err(format!("不支持的同步实体类型: {other}")),
    }
    sqlx::query(
        "DELETE FROM sync_entity_cache
         WHERE server_key = ? AND entity_type = ? AND entity_id = ?",
    )
    .bind(server_key)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_subscription(
    tx: &mut Transaction<'_, Sqlite>,
    payload: &Value,
) -> Result<(), String> {
    let value = serde_json::from_value::<Streamer>(payload.clone())
        .map_err(|error| format!("subscription payload 无效: {error}"))?;
    let aliases = serde_json::to_string(&value.aliases).map_err(|error| error.to_string())?;
    let created = value.created_at.to_rfc3339();
    sqlx::query(
        "INSERT INTO streamers(id, mid, name, avatar_url, room_id, live_url,
            schedule_enabled, live_notify_enabled, live_stats_enabled, aliases_json,
            last_error, last_refresh_at, live_is_live, live_title, live_cover,
            live_started_at, live_checked_at, sort_order, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            mid = excluded.mid, name = excluded.name, avatar_url = excluded.avatar_url,
            room_id = excluded.room_id, live_url = excluded.live_url,
            schedule_enabled = excluded.schedule_enabled,
            live_notify_enabled = excluded.live_notify_enabled,
            live_stats_enabled = excluded.live_stats_enabled,
            aliases_json = excluded.aliases_json, last_error = excluded.last_error,
            last_refresh_at = excluded.last_refresh_at, live_is_live = excluded.live_is_live,
            live_title = excluded.live_title, live_cover = excluded.live_cover,
            live_started_at = excluded.live_started_at,
            live_checked_at = excluded.live_checked_at,
            sort_order = excluded.sort_order, updated_at = excluded.updated_at",
    )
    .bind(value.id)
    .bind(&value.mid)
    .bind(&value.name)
    .bind(&value.avatar_url)
    .bind(&value.room_id)
    .bind(&value.live_url)
    .bind(value.schedule_enabled)
    .bind(value.live_notify_enabled)
    .bind(value.live_stats_enabled)
    .bind(aliases)
    .bind(&value.last_error)
    .bind(value.last_refresh_at.map(|time| time.to_rfc3339()))
    .bind(value.is_live)
    .bind(&value.live_title)
    .bind(&value.live_cover)
    .bind(value.live_started_at.map(|time| time.to_rfc3339()))
    .bind(value.live_checked_at.map(|time| time.to_rfc3339()))
    .bind(value.sort_order)
    .bind(&created)
    .bind(&created)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;

    sqlx::query("DELETE FROM linked_accounts WHERE streamer_id = ?")
        .bind(value.id)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    for link in value.linked_accounts {
        sqlx::query(
            "INSERT INTO linked_accounts(streamer_id, mid, name, avatar_url, created_at)
             VALUES(?, ?, ?, ?, ?)
             ON CONFLICT(streamer_id, mid) DO UPDATE SET
                name = excluded.name, avatar_url = excluded.avatar_url",
        )
        .bind(value.id)
        .bind(&link.mid)
        .bind(&link.name)
        .bind(&link.avatar_url)
        .bind(link.created_at.to_rfc3339())
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    }
    Ok(())
}

async fn apply_link(tx: &mut Transaction<'_, Sqlite>, payload: &Value) -> Result<(), String> {
    let value = serde_json::from_value::<LinkPayload>(payload.clone())
        .map_err(|error| format!("link payload 无效: {error}"))?;
    let created = value
        .created_at
        .clone()
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    sqlx::query(
        "INSERT INTO linked_accounts(streamer_id, mid, name, avatar_url, created_at)
         VALUES(?, ?, ?, ?, ?)
         ON CONFLICT(streamer_id, mid) DO UPDATE SET
            name = excluded.name, avatar_url = excluded.avatar_url",
    )
    .bind(value.streamer_id)
    .bind(&value.mid)
    .bind(&value.name)
    .bind(&value.avatar_url)
    .bind(created)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_schedule(tx: &mut Transaction<'_, Sqlite>, payload: &Value) -> Result<(), String> {
    let value = serde_json::from_value::<Schedule>(payload.clone())
        .map_err(|error| format!("schedule payload 无效: {error}"))?;
    let participants =
        serde_json::to_string(&value.participants).map_err(|error| error.to_string())?;
    sqlx::query(
        "INSERT INTO schedules(id, streamer_id, participants_json, title, start_at, end_at,
            room_url, room_owner_mid, room_owner_name, source_kind, source_post_id,
            source_image_url, source_author_mid, source_fingerprint, source_identity,
            source_published_at, weekly_rule_id, weekly_occurrence_date, confidence,
            manual_locked, source_changed, cancelled, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            streamer_id = excluded.streamer_id, participants_json = excluded.participants_json,
            title = excluded.title, start_at = excluded.start_at, end_at = excluded.end_at,
            room_url = excluded.room_url, room_owner_mid = excluded.room_owner_mid,
            room_owner_name = excluded.room_owner_name, source_kind = excluded.source_kind,
            source_post_id = excluded.source_post_id,
            source_image_url = excluded.source_image_url,
            source_author_mid = excluded.source_author_mid,
            source_fingerprint = excluded.source_fingerprint,
            source_identity = excluded.source_identity,
            source_published_at = excluded.source_published_at,
            weekly_rule_id = excluded.weekly_rule_id,
            weekly_occurrence_date = excluded.weekly_occurrence_date,
            confidence = excluded.confidence, manual_locked = excluded.manual_locked,
            source_changed = excluded.source_changed, cancelled = excluded.cancelled,
            updated_at = excluded.updated_at",
    )
    .bind(value.id)
    .bind(value.streamer_id)
    .bind(participants)
    .bind(&value.title)
    .bind(value.start_at.to_rfc3339())
    .bind(value.end_at.map(|time| time.to_rfc3339()))
    .bind(&value.room_url)
    .bind(&value.room_owner_mid)
    .bind(&value.room_owner_name)
    .bind(&value.source_kind)
    .bind(&value.source_post_id)
    .bind(&value.source_image_url)
    .bind(&value.source_author_mid)
    .bind(&value.source_fingerprint)
    .bind(&value.source_identity)
    .bind(value.source_published_at.map(|time| time.to_rfc3339()))
    .bind(value.weekly_rule_id)
    .bind(&value.weekly_occurrence_date)
    .bind(value.confidence)
    .bind(value.manual_locked)
    .bind(value.source_changed)
    .bind(value.cancelled)
    .bind(value.created_at.to_rfc3339())
    .bind(value.updated_at.to_rfc3339())
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_weekly_rule(
    tx: &mut Transaction<'_, Sqlite>,
    payload: &Value,
) -> Result<(), String> {
    let value = serde_json::from_value::<WeeklyScheduleRule>(payload.clone())
        .map_err(|error| format!("weekly_rule payload 无效: {error}"))?;
    sqlx::query(
        "INSERT INTO weekly_schedule_rules(id, rule_key, streamer_id, title, weekdays_mask,
            start_time, end_time, timezone, room_url, effective_from, effective_until,
            enabled, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            rule_key = excluded.rule_key, streamer_id = excluded.streamer_id,
            title = excluded.title, weekdays_mask = excluded.weekdays_mask,
            start_time = excluded.start_time, end_time = excluded.end_time,
            timezone = excluded.timezone, room_url = excluded.room_url,
            effective_from = excluded.effective_from,
            effective_until = excluded.effective_until, enabled = excluded.enabled,
            updated_at = excluded.updated_at",
    )
    .bind(value.id)
    .bind(&value.rule_key)
    .bind(value.streamer_id)
    .bind(&value.title)
    .bind(i64::from(value.weekdays_mask))
    .bind(&value.start_time)
    .bind(&value.end_time)
    .bind(&value.timezone)
    .bind(&value.room_url)
    .bind(&value.effective_from)
    .bind(&value.effective_until)
    .bind(value.enabled)
    .bind(value.created_at.to_rfc3339())
    .bind(value.updated_at.to_rfc3339())
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_weekly_exception(
    tx: &mut Transaction<'_, Sqlite>,
    payload: &Value,
) -> Result<(), String> {
    let value = serde_json::from_value::<WeeklyExceptionPayload>(payload.clone())
        .map_err(|error| format!("weekly_exception payload 无效: {error}"))?;
    sqlx::query(
        "INSERT INTO weekly_schedule_exceptions(rule_id, occurrence_date, exception_kind,
            schedule_id, created_at)
         VALUES(?, ?, ?, ?, ?)
         ON CONFLICT(rule_id, occurrence_date) DO UPDATE SET
            exception_kind = excluded.exception_kind, schedule_id = excluded.schedule_id",
    )
    .bind(value.rule_id)
    .bind(&value.occurrence_date)
    .bind(&value.exception_kind)
    .bind(value.schedule_id)
    .bind(value.created_at.unwrap_or_else(|| Utc::now().to_rfc3339()))
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_schedule_source(
    tx: &mut Transaction<'_, Sqlite>,
    payload: &Value,
) -> Result<(), String> {
    let value = serde_json::from_value::<ScheduleSourcePayload>(payload.clone())
        .map_err(|error| format!("schedule_source payload 无效: {error}"))?;
    sqlx::query(
        "INSERT INTO schedule_sources(schedule_id, source_kind, source_post_id,
            source_image_url, source_fingerprint, source_identity, confidence, created_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(schedule_id, source_identity) DO UPDATE SET
            source_kind = excluded.source_kind, source_post_id = excluded.source_post_id,
            source_image_url = excluded.source_image_url,
            source_fingerprint = excluded.source_fingerprint,
            confidence = excluded.confidence",
    )
    .bind(value.schedule_id)
    .bind(&value.source_kind)
    .bind(&value.source_post_id)
    .bind(&value.source_image_url)
    .bind(&value.source_fingerprint)
    .bind(&value.source_identity)
    .bind(value.confidence)
    .bind(value.created_at.unwrap_or_else(|| Utc::now().to_rfc3339()))
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_source_suppression(
    tx: &mut Transaction<'_, Sqlite>,
    payload: &Value,
) -> Result<(), String> {
    let value = serde_json::from_value::<SourceSuppression>(payload.clone())
        .map_err(|error| format!("source_suppression payload 无效: {error}"))?;
    sqlx::query(
        "INSERT INTO source_suppressions(id, streamer_id, source_post_id,
            source_fingerprint, created_at)
         VALUES(?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            streamer_id = excluded.streamer_id,
            source_post_id = excluded.source_post_id,
            source_fingerprint = excluded.source_fingerprint",
    )
    .bind(value.id)
    .bind(value.streamer_id)
    .bind(&value.source_post_id)
    .bind(&value.source_fingerprint)
    .bind(value.created_at.to_rfc3339())
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_live_status(
    tx: &mut Transaction<'_, Sqlite>,
    payload: &Value,
) -> Result<(), String> {
    let value = serde_json::from_value::<LiveStatus>(payload.clone())
        .map_err(|error| format!("live_status payload 无效: {error}"))?;
    sqlx::query(
        "UPDATE streamers SET live_is_live = ?, live_title = ?, live_cover = ?,
            live_url = COALESCE(?, live_url), live_started_at = ?, live_checked_at = ?
         WHERE id = ?",
    )
    .bind(value.is_live)
    .bind(&value.title)
    .bind(&value.cover_url)
    .bind(&value.live_url)
    .bind(value.started_at.map(|time| time.to_rfc3339()))
    .bind(value.checked_at.to_rfc3339())
    .bind(value.streamer_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_live_session(
    tx: &mut Transaction<'_, Sqlite>,
    payload: &Value,
) -> Result<(), String> {
    let value = serde_json::from_value::<LiveSession>(payload.clone())
        .map_err(|error| format!("live_session payload 无效: {error}"))?;
    sqlx::query(
        "INSERT INTO live_sessions(id, streamer_id, streamer_name, room_id, title, cover_url,
            started_at, ended_at, duration_seconds, peak_online, avg_online, online_samples,
            online_total, revenue_cents, gift_revenue_cents, super_chat_revenue_cents,
            guard_revenue_cents, paid_user_count, interactive_user_count, danmaku_count,
            gift_count, super_chat_count, guard_count, reconnect_count, data_quality,
            collector_state, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            streamer_id = excluded.streamer_id, streamer_name = excluded.streamer_name,
            room_id = excluded.room_id, title = excluded.title,
            cover_url = excluded.cover_url, started_at = excluded.started_at,
            ended_at = excluded.ended_at, duration_seconds = excluded.duration_seconds,
            peak_online = excluded.peak_online, avg_online = excluded.avg_online,
            online_samples = excluded.online_samples, online_total = excluded.online_total,
            revenue_cents = excluded.revenue_cents,
            gift_revenue_cents = excluded.gift_revenue_cents,
            super_chat_revenue_cents = excluded.super_chat_revenue_cents,
            guard_revenue_cents = excluded.guard_revenue_cents,
            paid_user_count = excluded.paid_user_count,
            interactive_user_count = excluded.interactive_user_count,
            danmaku_count = excluded.danmaku_count, gift_count = excluded.gift_count,
            super_chat_count = excluded.super_chat_count,
            guard_count = excluded.guard_count,
            reconnect_count = excluded.reconnect_count,
            data_quality = excluded.data_quality,
            collector_state = excluded.collector_state, updated_at = excluded.updated_at",
    )
    .bind(value.id)
    .bind(value.streamer_id)
    .bind(&value.streamer_name)
    .bind(&value.room_id)
    .bind(&value.title)
    .bind(&value.cover_url)
    .bind(value.started_at.to_rfc3339())
    .bind(value.ended_at.map(|time| time.to_rfc3339()))
    .bind(value.duration_seconds)
    .bind(value.peak_online as i64)
    .bind(value.avg_online as i64)
    .bind(value.online_samples)
    .bind(value.online_total)
    .bind(value.revenue_cents)
    .bind(value.gift_revenue_cents)
    .bind(value.super_chat_revenue_cents)
    .bind(value.guard_revenue_cents)
    .bind(value.paid_user_count)
    .bind(value.interactive_user_count)
    .bind(value.danmaku_count)
    .bind(value.gift_count)
    .bind(value.super_chat_count)
    .bind(value.guard_count)
    .bind(value.reconnect_count)
    .bind(&value.data_quality)
    .bind(&value.collector_state)
    .bind(value.created_at.to_rfc3339())
    .bind(value.updated_at.to_rfc3339())
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn apply_server_settings(
    tx: &mut Transaction<'_, Sqlite>,
    payload: &Value,
) -> Result<(), String> {
    let object = payload
        .as_object()
        .ok_or_else(|| "server_settings payload 必须是对象".to_owned())?;
    let now = Utc::now().to_rfc3339();
    for (json_key, db_key) in [
        ("livePollSeconds", "live_poll_seconds"),
        ("dynamicPollSeconds", "dynamic_poll_seconds"),
        ("autoAnalyzeImages", "auto_analyze_images"),
        ("liveStatsEnabled", "live_stats_enabled"),
        ("llmBaseUrl", "llm_base_url"),
        ("llmModel", "llm_model"),
        ("llmPromptVersion", "llm_prompt_version"),
        ("proxyPort", "proxy_port"),
    ] {
        let Some(value) = object.get(json_key) else {
            continue;
        };
        let encoded = serde_json::to_string(value).map_err(|error| error.to_string())?;
        sqlx::query(
            "INSERT INTO settings(key, value, updated_at) VALUES(?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                updated_at = excluded.updated_at",
        )
        .bind(db_key)
        .bind(encoded)
        .bind(&now)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    }
    Ok(())
}

async fn upsert_cache(
    tx: &mut Transaction<'_, Sqlite>,
    server_key: &str,
    entity_type: &str,
    entity_id: &str,
    revision: i64,
    operation: &str,
    payload: Option<&Value>,
) -> Result<(), String> {
    let payload_json = payload
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| error.to_string())?;
    sqlx::query(
        "INSERT INTO sync_entity_cache(server_key, entity_type, entity_id, entity_revision,
            operation, payload_json, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(server_key, entity_type, entity_id) DO UPDATE SET
            entity_revision = excluded.entity_revision,
            operation = excluded.operation,
            payload_json = excluded.payload_json,
            updated_at = excluded.updated_at",
    )
    .bind(server_key)
    .bind(entity_type)
    .bind(entity_id)
    .bind(revision)
    .bind(operation)
    .bind(payload_json)
    .bind(Utc::now().to_rfc3339())
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

fn entity_id_from_payload(entity_type: &str, payload: &Value) -> Result<String, String> {
    let value = match entity_type {
        "link" | "linked_account" => payload
            .get("streamerId")
            .and_then(Value::as_i64)
            .zip(payload.get("mid").and_then(Value::as_str))
            .map(|(streamer_id, mid)| format!("{streamer_id}:{mid}")),
        "live_status" => payload
            .get("streamerId")
            .and_then(Value::as_i64)
            .map(|id| id.to_string()),
        "server_secret" | "sync_reset" => {
            payload.get("id").and_then(Value::as_str).map(str::to_owned)
        }
        "server_settings" => Some("server".to_owned()),
        _ => payload
            .get("id")
            .and_then(Value::as_i64)
            .map(|id| id.to_string()),
    };
    value.ok_or_else(|| format!("{entity_type} payload 缺少实体 ID"))
}

fn parse_entity_id(value: &str, entity_type: &str) -> Result<i64, String> {
    value
        .parse::<i64>()
        .map_err(|_| format!("{entity_type} tombstone 的 entityId 无效: {value}"))
}

async fn ensure_meta_row(
    tx: &mut Transaction<'_, Sqlite>,
    server_key: &str,
    device_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO sync_meta(server_key, device_id) VALUES(?, ?)
         ON CONFLICT(server_key) DO UPDATE SET device_id = excluded.device_id",
    )
    .bind(server_key)
    .bind(device_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn read_meta(pool: &SqlitePool, server_key: &str) -> Result<SyncMeta, String> {
    let row = sqlx::query(
        "SELECT server_key, device_id, cursor_seq, latest_seq, last_snapshot_revision,
            last_ack_seq, last_sync_at, status, initialized
         FROM sync_meta WHERE server_key = ?",
    )
    .bind(server_key)
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    Ok(meta_from_row(&row))
}

async fn read_meta_tx(
    tx: &mut Transaction<'_, Sqlite>,
    server_key: &str,
) -> Result<SyncMeta, String> {
    let row = sqlx::query(
        "SELECT server_key, device_id, cursor_seq, latest_seq, last_snapshot_revision,
            last_ack_seq, last_sync_at, status, initialized
         FROM sync_meta WHERE server_key = ?",
    )
    .bind(server_key)
    .fetch_one(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(meta_from_row(&row))
}

fn meta_from_row(row: &sqlx::sqlite::SqliteRow) -> SyncMeta {
    SyncMeta {
        server_key: row.get("server_key"),
        device_id: row.get("device_id"),
        cursor_seq: row.get("cursor_seq"),
        latest_seq: row.get("latest_seq"),
        last_snapshot_revision: row.get("last_snapshot_revision"),
        last_ack_seq: row.get("last_ack_seq"),
        last_sync_at: row.get("last_sync_at"),
        status: row.get("status"),
        initialized: row.get::<i64, _>("initialized") != 0,
    }
}

fn db_error(error: sqlx::Error) -> String {
    let message = error.to_string();
    if message.contains("FOREIGN KEY constraint failed") {
        return format!("SYNC_CACHE_INCONSISTENT: 本地同步缓存外键不一致: {error}");
    }
    format!("本地同步数据库错误: {error}")
}

fn generate_device_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut first = std::collections::hash_map::RandomState::new().build_hasher();
    first.write_u128(now);
    first.write_u32(std::process::id());
    first.write_u64(0x9e37_79b9_7f4a_7c15);
    let high = first.finish();
    let mut second = std::collections::hash_map::RandomState::new().build_hasher();
    second.write_u64(high);
    second.write_u128(now.rotate_left(41));
    second.write_u64(0xbf58_476d_1ce4_e5b9);
    let low = second.finish();
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

pub fn device_registration(device_id: String) -> DeviceRegistration {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "未命名设备".to_owned());
    DeviceRegistration {
        device_id,
        name,
        platform: std::env::consts::OS.to_owned(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

pub async fn register_device(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    registration: &DeviceRegistration,
) -> Result<DeviceInfo, String> {
    post_json(
        client,
        server_url,
        token,
        "/api/devices/register",
        registration,
    )
    .await
}

pub async fn fetch_changes(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    after: i64,
    limit: usize,
) -> Result<SyncChangesPage, String> {
    get_json(
        client,
        server_url,
        token,
        &format!(
            "/api/sync/changes?after={}&limit={}",
            after.max(0),
            limit.clamp(1, 500)
        ),
    )
    .await
}

pub async fn fetch_bootstrap(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
) -> Result<SyncBootstrap, String> {
    get_json(client, server_url, token, "/api/sync/bootstrap").await
}

pub async fn post_ack(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    device_id: &str,
    seq: i64,
) -> Result<Value, String> {
    post_json(
        client,
        server_url,
        token,
        "/api/sync/ack",
        &json!({"deviceId": device_id, "seq": seq}),
    )
    .await
}

pub async fn request_refresh_job(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    streamer_id: Option<i64>,
) -> Result<RefreshJobAccepted, String> {
    let path = match streamer_id {
        Some(id) => format!("/api/jobs/refresh/{id}"),
        None => "/api/jobs/refresh".to_owned(),
    };
    post_json(client, server_url, token, &path, &json!({})).await
}

pub async fn list_devices(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
) -> Result<Vec<DeviceInfo>, String> {
    get_json(client, server_url, token, "/api/devices").await
}

async fn get_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    path: &str,
) -> Result<T, String> {
    let url = join_url(server_url, path)?;
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| format!("服务器请求失败: {error}"))?;
    decode_response(response).await
}

async fn post_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    server_url: &str,
    token: &str,
    path: &str,
    body: &impl Serialize,
) -> Result<T, String> {
    let url = join_url(server_url, path)?;
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|error| format!("服务器请求失败: {error}"))?;
    decode_response(response).await
}

async fn decode_response<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, String> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取服务器响应失败: {error}"))?;
    let envelope = serde_json::from_str::<Envelope<T>>(&text)
        .map_err(|error| format!("服务器返回了无效 JSON: {error}"))?;
    if !status.is_success() || !envelope.ok {
        return Err(match envelope.error {
            Some(error) => format!("{}: {}", error.code, error.message),
            None => format!("服务器请求失败: HTTP {status}"),
        });
    }
    envelope
        .data
        .ok_or_else(|| "服务器响应缺少 data".to_owned())
}

fn join_url(server_url: &str, path: &str) -> Result<String, String> {
    let server_key = normalize_server_key(server_url)?;
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    Ok(format!("{server_key}{path}"))
}

fn schedule_error(error: oshical_core::AppError) -> String {
    format!("本地日程缓存错误: {error}")
}

fn validate_offline_schedule_times(
    start_at: chrono::DateTime<Utc>,
    end_at: Option<chrono::DateTime<Utc>>,
) -> Result<(), String> {
    if let Some(end_at) = end_at {
        if end_at <= start_at {
            return Err("结束时间必须晚于开始时间".to_owned());
        }
    }
    Ok(())
}

pub async fn create_offline_schedule(
    store: &SqliteStore,
    input: &NewSchedule,
) -> Result<Schedule, String> {
    ensure_schema(store).await?;
    let title = input.title.trim();
    if title.is_empty() {
        return Err("日程标题不能为空".to_owned());
    }
    if title.chars().count() > 200 {
        return Err("日程标题不能超过 200 字".to_owned());
    }
    validate_offline_schedule_times(input.start_at, input.end_at)?;

    let now = Utc::now();
    let seed_id = -now.timestamp_millis().max(1);
    let participants =
        serde_json::to_string(&input.participants).map_err(|error| error.to_string())?;
    let mut tx = store
        .pool()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(db_error)?;
    let id: i64 = sqlx::query_scalar(
        "SELECT CASE WHEN MIN(id) IS NULL THEN ? ELSE MIN(id) - 1 END
         FROM schedules WHERE id < 0",
    )
    .bind(seed_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    sqlx::query(
        "INSERT INTO schedules
         (id, streamer_id, participants_json, title, start_at, end_at, room_url,
          room_owner_mid, room_owner_name, source_kind, source_post_id, source_image_url,
          source_author_mid, source_fingerprint, source_identity, source_published_at,
          weekly_rule_id, weekly_occurrence_date, confidence, manual_locked, source_changed,
          cancelled, created_at, updated_at, revision)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, 1, 0, 0, ?, ?, 1)",
    )
    .bind(id)
    .bind(input.streamer_id)
    .bind(participants)
    .bind(title)
    .bind(input.start_at.to_rfc3339())
    .bind(input.end_at.map(|value| value.to_rfc3339()))
    .bind(&input.room_url)
    .bind(&input.room_owner_mid)
    .bind(&input.room_owner_name)
    .bind(&input.source_kind)
    .bind(&input.source_post_id)
    .bind(&input.source_image_url)
    .bind(&input.source_author_mid)
    .bind(&input.source_fingerprint)
    .bind(&input.source_identity)
    .bind(input.source_published_at.map(|value| value.to_rfc3339()))
    .bind(input.confidence)
    .bind(now.to_rfc3339())
    .bind(now.to_rfc3339())
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;

    store.get_schedule(id).await.map_err(schedule_error)
}

pub async fn update_offline_schedule(
    store: &SqliteStore,
    id: i64,
    update: &UpdateSchedule,
) -> Result<Schedule, String> {
    ensure_schema(store).await?;
    let current = store.get_schedule(id).await.map_err(schedule_error)?;
    let title = update
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or(current.title);
    if title.chars().count() > 200 {
        return Err("日程标题不能超过 200 字".to_owned());
    }
    let start_at = update.start_at.unwrap_or(current.start_at);
    let end_at = update.end_at.or(current.end_at);
    validate_offline_schedule_times(start_at, end_at)?;
    let room_url = update.room_url.clone().or(current.room_url);

    let result = sqlx::query(
        "UPDATE schedules SET title = ?, start_at = ?, end_at = ?, room_url = ?,
            room_owner_mid = NULL, room_owner_name = NULL,
            manual_locked = 1, source_changed = 0, updated_at = ?, revision = revision + 1
         WHERE id = ?",
    )
    .bind(title)
    .bind(start_at.to_rfc3339())
    .bind(end_at.map(|value| value.to_rfc3339()))
    .bind(room_url)
    .bind(Utc::now().to_rfc3339())
    .bind(id)
    .execute(store.pool())
    .await
    .map_err(db_error)?;
    if result.rows_affected() == 0 {
        return Err(format!("未找到日程 {id}"));
    }

    store.get_schedule(id).await.map_err(schedule_error)
}

pub async fn delete_offline_schedule(store: &SqliteStore, id: i64) -> Result<(), String> {
    ensure_schema(store).await?;
    let result = sqlx::query("DELETE FROM schedules WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .map_err(db_error)?;
    if result.rows_affected() == 0 {
        return Err(format!("未找到日程 {id}"));
    }
    Ok(())
}

pub async fn purge_offline_schedules(store: &SqliteStore) -> Result<u64, String> {
    ensure_schema(store).await?;
    let result = sqlx::query("DELETE FROM schedules WHERE id < 0")
        .execute(store.pool())
        .await
        .map_err(db_error)?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> SqliteStore {
        let store = SqliteStore::in_memory().await.unwrap();
        ensure_schema(&store).await.unwrap();
        store
    }

    #[tokio::test]
    async fn foreign_key_failure_requests_cache_rebuild() {
        let store = store().await;
        let server = "https://sync-cache-rebuild.example.com";
        get_sync_meta(&store, server).await.unwrap();
        sqlx::query("UPDATE sync_meta SET cursor_seq = 19, latest_seq = 19, initialized = 1 WHERE server_key = ?")
            .bind(server)
            .execute(store.pool())
            .await
            .unwrap();
        let batch = SyncBatch {
            from_seq: 20,
            to_seq: 20,
            latest_seq: 20,
            changes: vec![SyncChange {
                seq: 20,
                entity_type: "source_suppression".to_owned(),
                entity_id: "1".to_owned(),
                operation: "upsert".to_owned(),
                entity_revision: 1,
                payload: Some(json!({
                    "id": 1,
                    "streamerId": 999,
                    "sourcePostId": "post-1",
                    "sourceFingerprint": "fingerprint-1",
                    "createdAt": "2026-10-30T00:00:00Z"
                })),
                created_at: String::new(),
            }],
        };
        let error = apply_batch(&store, server, &batch).await.unwrap_err();
        assert!(error.starts_with("SYNC_CACHE_INCONSISTENT:"), "{error}");
        assert_eq!(get_sync_meta(&store, server).await.unwrap().cursor_seq, 19);
    }

    #[tokio::test]
    async fn device_id_is_stable() {
        let store = store().await;
        let first = device_id(&store).await.unwrap();
        let second = device_id(&store).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(first.as_bytes()[14], b'4');
    }

    #[tokio::test]
    async fn batch_failure_does_not_advance_cursor() {
        let store = store().await;
        let server = "https://example.com";
        get_sync_meta(&store, server).await.unwrap();
        let batch = SyncBatch {
            from_seq: 1,
            to_seq: 2,
            latest_seq: 2,
            changes: vec![
                SyncChange {
                    seq: 1,
                    entity_type: "subscription".to_owned(),
                    entity_id: "1".to_owned(),
                    operation: "upsert".to_owned(),
                    entity_revision: 1,
                    payload: Some(json!({
                        "id": 1,
                        "mid": "100",
                        "name": "测试主播",
                        "avatarUrl": "",
                        "roomId": null,
                        "liveUrl": null,
                        "aliases": [],
                        "scheduleEnabled": true,
                        "liveNotifyEnabled": true,
                        "liveStatsEnabled": false,
                        "lastError": null,
                        "lastRefreshAt": null,
                        "isLive": false,
                        "liveTitle": null,
                        "liveCover": null,
                        "liveStartedAt": null,
                        "liveCheckedAt": null,
                        "linkedAccounts": [],
                        "createdAt": "2026-10-29T00:00:00Z"
                    })),
                    created_at: String::new(),
                },
                SyncChange {
                    seq: 2,
                    entity_type: "schedule".to_owned(),
                    entity_id: "1".to_owned(),
                    operation: "upsert".to_owned(),
                    entity_revision: 1,
                    payload: Some(json!({"id": 1})),
                    created_at: String::new(),
                },
            ],
        };
        assert!(apply_batch(&store, server, &batch).await.is_err());
        let meta = get_sync_meta(&store, server).await.unwrap();
        assert_eq!(meta.cursor_seq, 0);
        assert!(store.list_streamers().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gap_is_rejected_without_cursor_change() {
        let store = store().await;
        let server = "https://example.com";
        let batch = SyncBatch {
            from_seq: 2,
            to_seq: 2,
            latest_seq: 2,
            changes: vec![SyncChange {
                seq: 2,
                entity_type: "schedule".to_owned(),
                entity_id: "1".to_owned(),
                operation: "delete".to_owned(),
                entity_revision: 1,
                payload: None,
                created_at: String::new(),
            }],
        };
        let error = apply_batch(&store, server, &batch).await.unwrap_err();
        assert!(error.contains("SYNC_GAP"));
        assert_eq!(get_sync_meta(&store, server).await.unwrap().cursor_seq, 0);
    }

    #[tokio::test]
    async fn bootstrap_applies_live_status_and_legacy_linked_account_changes() {
        let store = store().await;
        let server = "https://sync-entity.example.com";
        let bootstrap = SyncBootstrap {
            schema_version: SYNC_SCHEMA_VERSION,
            snapshot_revision: 10,
            min_retained_seq: 0,
            server_time: None,
            subscriptions: vec![json!({
                "id": 1,
                "mid": "100",
                "name": "实体兼容测试主播",
                "avatarUrl": "",
                "roomId": "123",
                "liveUrl": "https://live.bilibili.com/123",
                "aliases": [],
                "scheduleEnabled": true,
                "liveNotifyEnabled": true,
                "liveStatsEnabled": false,
                "lastError": null,
                "lastRefreshAt": null,
                "isLive": false,
                "liveTitle": null,
                "liveCover": null,
                "liveStartedAt": null,
                "liveCheckedAt": null,
                "linkedAccounts": [],
                "createdAt": "2026-10-29T00:00:00Z"
            })],
            schedules: Vec::new(),
            weekly_rules: Vec::new(),
            weekly_exceptions: Vec::new(),
            schedule_sources: Vec::new(),
            source_suppressions: Vec::new(),
            server_settings: Value::Null,
            live_statuses: vec![json!({
                "streamerId": 1,
                "mid": "100",
                "streamerName": "实体兼容测试主播",
                "avatarUrl": "",
                "isLive": true,
                "title": "直播中",
                "coverUrl": null,
                "liveUrl": "https://live.bilibili.com/123",
                "online": 42,
                "startedAt": "2026-10-29T00:00:00Z",
                "checkedAt": "2026-10-29T00:01:00Z",
                "liveNotifyEnabled": true,
                "revision": 1
            })],
            live_sessions: Vec::new(),
        };
        apply_bootstrap(&store, server, &bootstrap).await.unwrap();
        let is_live: i64 = sqlx::query_scalar("SELECT live_is_live FROM streamers WHERE id = 1")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(is_live, 1);

        let batch = SyncBatch {
            from_seq: 11,
            to_seq: 12,
            latest_seq: 12,
            changes: vec![
                SyncChange {
                    seq: 11,
                    entity_type: "linked_account".to_owned(),
                    entity_id: "7".to_owned(),
                    operation: "upsert".to_owned(),
                    entity_revision: 1,
                    payload: Some(json!({
                        "id": 7,
                        "streamerId": 1,
                        "mid": "200",
                        "name": "兼容官号",
                        "avatarUrl": "",
                        "revision": 1,
                        "createdAt": "2026-10-29T00:00:00Z"
                    })),
                    created_at: String::new(),
                },
                SyncChange {
                    seq: 12,
                    entity_type: "server_secret".to_owned(),
                    entity_id: "llm_api_key".to_owned(),
                    operation: "upsert".to_owned(),
                    entity_revision: 1,
                    payload: Some(json!({"id": "llm_api_key", "configured": true})),
                    created_at: String::new(),
                },
            ],
        };
        apply_batch(&store, server, &batch).await.unwrap();
        let links: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM linked_accounts WHERE streamer_id = 1 AND mid = '200'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(links, 1);
        assert_eq!(get_sync_meta(&store, server).await.unwrap().cursor_seq, 12);
    }

    #[tokio::test]
    async fn offline_schedule_crud_stays_in_cache_and_purges_only_temporary_rows() {
        let store = store().await;
        let server = "https://sync.example.com";
        let start_at = Utc::now() + chrono::Duration::hours(1);
        let bootstrap = SyncBootstrap {
            schema_version: SYNC_SCHEMA_VERSION,
            snapshot_revision: 10,
            min_retained_seq: 0,
            server_time: None,
            subscriptions: vec![json!({
                "id": 1,
                "mid": "100",
                "name": "缓存测试主播",
                "avatarUrl": "",
                "roomId": null,
                "liveUrl": null,
                "aliases": [],
                "scheduleEnabled": true,
                "liveNotifyEnabled": true,
                "liveStatsEnabled": false,
                "lastError": null,
                "lastRefreshAt": null,
                "isLive": false,
                "liveTitle": null,
                "liveCover": null,
                "liveStartedAt": null,
                "liveCheckedAt": null,
                "linkedAccounts": [],
                "createdAt": "2026-10-29T00:00:00Z"
            })],
            schedules: Vec::new(),
            weekly_rules: Vec::new(),
            weekly_exceptions: Vec::new(),
            schedule_sources: Vec::new(),
            source_suppressions: Vec::new(),
            server_settings: Value::Null,
            live_statuses: Vec::new(),
            live_sessions: Vec::new(),
        };
        apply_bootstrap(&store, server, &bootstrap).await.unwrap();

        let input = NewSchedule {
            streamer_id: 1,
            participants: vec!["嘉宾".to_owned()],
            title: "离线新增日程".to_owned(),
            start_at,
            end_at: Some(start_at + chrono::Duration::hours(1)),
            room_url: Some("https://live.bilibili.com/123".to_owned()),
            room_owner_mid: Some("100".to_owned()),
            room_owner_name: Some("缓存测试主播".to_owned()),
            source_kind: "manual".to_owned(),
            source_post_id: None,
            source_image_url: None,
            source_author_mid: None,
            source_fingerprint: None,
            source_identity: None,
            source_published_at: None,
            confidence: 1.0,
        };
        let created = create_offline_schedule(&store, &input).await.unwrap();
        assert!(created.id < 0);
        assert_eq!(created.title, input.title);
        assert_eq!(store.get_schedule(created.id).await.unwrap().id, created.id);
        let listed = store
            .list_schedules(
                start_at - chrono::Duration::hours(1),
                start_at + chrono::Duration::hours(2),
                false,
            )
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);

        let updated = update_offline_schedule(
            &store,
            created.id,
            &UpdateSchedule {
                title: Some("离线修改日程".to_owned()),
                start_at: None,
                end_at: None,
                room_url: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.title, "离线修改日程");
        assert_eq!(updated.revision, 2);

        let positive_candidate = create_offline_schedule(
            &store,
            &NewSchedule {
                title: "保留的正数日程".to_owned(),
                start_at: start_at + chrono::Duration::hours(3),
                end_at: Some(start_at + chrono::Duration::hours(4)),
                ..input.clone()
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE schedules SET id = 1 WHERE id = ?")
            .bind(positive_candidate.id)
            .execute(store.pool())
            .await
            .unwrap();

        assert_eq!(purge_offline_schedules(&store).await.unwrap(), 1);
        assert!(store.get_schedule(created.id).await.is_err());
        assert_eq!(store.get_schedule(1).await.unwrap().title, "保留的正数日程");

        let removable = create_offline_schedule(
            &store,
            &NewSchedule {
                title: "待删除日程".to_owned(),
                start_at: start_at + chrono::Duration::hours(5),
                end_at: Some(start_at + chrono::Duration::hours(6)),
                ..input
            },
        )
        .await
        .unwrap();
        delete_offline_schedule(&store, removable.id).await.unwrap();
        assert!(store.get_schedule(removable.id).await.is_err());
    }
}
