mod import;
mod live_broadcasts;
mod live_samples;
pub use import::{
    ImportConflict, ImportPreview, ImportReport, MigrationApplyOutcome, ServerSnapshot,
    SERVER_SNAPSHOT_SCHEMA_VERSION,
};
pub use live_broadcasts::LiveBroadcast;
mod sync;
mod sync_feed;
use sync_feed::SyncActor;
pub use sync_feed::{SyncPrunePolicy, SYNC_SCHEMA_VERSION};

use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::models::{
    AddSubscription, ClientSyncState, DynamicPost, FanSnapshot, HealthState, LinkedAccount,
    LiveSession, LiveSessionStats, LiveSnapshot, LiveStatus, NewSchedule, NewWeeklyScheduleRule,
    Schedule, SourceSuppression, Streamer, SyncMutation, UpdateSchedule, UpdateSubscription,
    UpdateWeeklyScheduleRule, WeeklyScheduleRule,
};
use crate::settings::SETTING_KEYS;

/// 断流重开的合并窗口：同一房间在这个间隔内重新开播，算作同一场直播的一段，而不是新的一场。
const LIVE_SESSION_MERGE_MINUTES: i64 = live_broadcasts::LIVE_RECONNECT_MINUTES;

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS streamers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    mid TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    avatar_url TEXT NOT NULL DEFAULT '',
    room_id TEXT,
    live_url TEXT,
    schedule_enabled INTEGER NOT NULL DEFAULT 1,
    live_notify_enabled INTEGER NOT NULL DEFAULT 1,
    live_stats_enabled INTEGER NOT NULL DEFAULT 0,
    fans INTEGER,
    aliases_json TEXT NOT NULL DEFAULT '[]',
    last_error TEXT,
    last_refresh_at TEXT,
    live_is_live INTEGER NOT NULL DEFAULT 0,
    live_title TEXT,
    live_cover TEXT,
    live_started_at TEXT,
    live_checked_at TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    live_revision INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS live_broadcasts (
    id TEXT PRIMARY KEY,
    streamer_id INTEGER NOT NULL REFERENCES streamers(id) ON DELETE CASCADE,
    room_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    last_segment_started_at TEXT,
    last_seen_at TEXT NOT NULL,
    offline_since TEXT,
    schedule_id INTEGER REFERENCES schedules(id) ON DELETE SET NULL,
    schedule_decided INTEGER NOT NULL DEFAULT 0,
    segment_count INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_live_broadcasts_streamer ON live_broadcasts(streamer_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_live_broadcasts_schedule ON live_broadcasts(schedule_id) WHERE schedule_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS fan_snapshots (
    streamer_id INTEGER NOT NULL REFERENCES streamers(id) ON DELETE CASCADE,
    captured_at TEXT NOT NULL,
    fans INTEGER NOT NULL,
    PRIMARY KEY(streamer_id, captured_at)
);

CREATE INDEX IF NOT EXISTS idx_fan_snapshots_streamer_captured
ON fan_snapshots(streamer_id, captured_at DESC);

CREATE TABLE IF NOT EXISTS live_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    streamer_id INTEGER NOT NULL REFERENCES streamers(id) ON DELETE CASCADE,
    streamer_name TEXT NOT NULL,
    room_id TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    cover_url TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    duration_seconds INTEGER NOT NULL DEFAULT 0,
    peak_online INTEGER NOT NULL DEFAULT 0,
    peak_high_energy INTEGER NOT NULL DEFAULT 0,
    avg_online INTEGER NOT NULL DEFAULT 0,
    online_samples INTEGER NOT NULL DEFAULT 0,
    online_total INTEGER NOT NULL DEFAULT 0,
    revenue_cents INTEGER NOT NULL DEFAULT 0,
    gift_revenue_cents INTEGER NOT NULL DEFAULT 0,
    super_chat_revenue_cents INTEGER NOT NULL DEFAULT 0,
    guard_revenue_cents INTEGER NOT NULL DEFAULT 0,
    paid_user_count INTEGER NOT NULL DEFAULT 0,
    interactive_user_count INTEGER NOT NULL DEFAULT 0,
    danmaku_count INTEGER NOT NULL DEFAULT 0,
    gift_count INTEGER NOT NULL DEFAULT 0,
    super_chat_count INTEGER NOT NULL DEFAULT 0,
    guard_count INTEGER NOT NULL DEFAULT 0,
    reconnect_count INTEGER NOT NULL DEFAULT 0,
    segment_count INTEGER NOT NULL DEFAULT 1,
    last_segment_started_at TEXT NOT NULL DEFAULT '',
    data_quality TEXT NOT NULL DEFAULT 'collecting',
    collector_state TEXT NOT NULL DEFAULT 'active',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    UNIQUE(streamer_id, started_at)
);

CREATE INDEX IF NOT EXISTS idx_live_sessions_streamer_started
ON live_sessions(streamer_id, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_live_sessions_room_started
ON live_sessions(room_id, started_at DESC);
-- Independent history: never embedded in repeated sync payloads.
CREATE TABLE IF NOT EXISTS live_online_samples (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES live_sessions(id) ON DELETE CASCADE,
    sampled_at TEXT NOT NULL,
    observed_at TEXT,
    high_energy_count INTEGER CHECK (high_energy_count >= 0),
    collector_state TEXT NOT NULL,
    CHECK ((observed_at IS NULL) = (high_energy_count IS NULL)),
    UNIQUE(session_id, sampled_at)
);
CREATE INDEX IF NOT EXISTS idx_live_online_samples_session_id
ON live_online_samples(session_id, id);

CREATE TABLE IF NOT EXISTS linked_accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    streamer_id INTEGER NOT NULL REFERENCES streamers(id) ON DELETE CASCADE,
    mid TEXT NOT NULL,
    name TEXT NOT NULL,
    avatar_url TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    UNIQUE(streamer_id, mid)
);

CREATE TABLE IF NOT EXISTS dynamic_posts (
    id TEXT PRIMARY KEY,
    author_mid TEXT NOT NULL,
    author_name TEXT NOT NULL,
    published_at TEXT NOT NULL,
    text TEXT NOT NULL,
    images_json TEXT NOT NULL,
    source_url TEXT NOT NULL,
    raw_kind TEXT NOT NULL,
    reserve_json TEXT,
    fetched_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS image_analyses (
    cache_key TEXT PRIMARY KEY,
    post_id TEXT NOT NULL,
    image_url TEXT NOT NULL,
    model TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    result_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS image_analysis_aliases (
    post_id TEXT NOT NULL,
    image_url TEXT NOT NULL,
    model TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    cache_key TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY(post_id, image_url, model, prompt_version)
);

CREATE TABLE IF NOT EXISTS text_analyses (
    cache_key TEXT PRIMARY KEY,
    post_id TEXT NOT NULL,
    text_digest TEXT NOT NULL,
    model TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    result_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS post_analyses (
    cache_key TEXT PRIMARY KEY,
    post_id TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    model TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    result_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_post_analyses_post ON post_analyses(post_id);

CREATE TABLE IF NOT EXISTS schedules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    streamer_id INTEGER NOT NULL REFERENCES streamers(id) ON DELETE CASCADE,
    participants_json TEXT NOT NULL DEFAULT '[]',
    title TEXT NOT NULL,
    start_at TEXT NOT NULL,
    end_at TEXT,
    room_url TEXT,
    room_owner_mid TEXT,
    room_owner_name TEXT,
    source_kind TEXT NOT NULL,
    source_post_id TEXT,
    source_image_url TEXT,
    source_author_mid TEXT,
    source_fingerprint TEXT,
    source_identity TEXT,
    source_published_at TEXT,
    weekly_rule_id INTEGER REFERENCES weekly_schedule_rules(id) ON DELETE SET NULL,
    weekly_occurrence_date TEXT,
    confidence REAL NOT NULL DEFAULT 1.0,
    manual_locked INTEGER NOT NULL DEFAULT 0,
    source_changed INTEGER NOT NULL DEFAULT 0,
    cancelled INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    UNIQUE(streamer_id, start_at, title)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_schedules_source_identity
ON schedules(streamer_id, source_identity)
WHERE source_identity IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_schedules_time ON schedules(start_at, end_at);

CREATE TABLE IF NOT EXISTS weekly_schedule_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_key TEXT NOT NULL UNIQUE,
    streamer_id INTEGER NOT NULL REFERENCES streamers(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    weekdays_mask INTEGER NOT NULL,
    start_time TEXT NOT NULL,
    end_time TEXT,
    timezone TEXT NOT NULL DEFAULT 'Asia/Shanghai',
    room_url TEXT,
    effective_from TEXT NOT NULL,
    effective_until TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_weekly_rules_streamer ON weekly_schedule_rules(streamer_id, enabled);

CREATE TABLE IF NOT EXISTS weekly_schedule_exceptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_id INTEGER NOT NULL REFERENCES weekly_schedule_rules(id) ON DELETE CASCADE,
    occurrence_date TEXT NOT NULL,
    exception_kind TEXT NOT NULL,
    schedule_id INTEGER,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    UNIQUE(rule_id, occurrence_date)
);

CREATE TABLE IF NOT EXISTS schedule_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    schedule_id INTEGER NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    source_kind TEXT NOT NULL,
    source_post_id TEXT,
    source_image_url TEXT,
    source_fingerprint TEXT,
    source_identity TEXT NOT NULL,
    confidence REAL NOT NULL,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    UNIQUE(schedule_id, source_identity)
);

CREATE TABLE IF NOT EXISTS source_suppressions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    streamer_id INTEGER NOT NULL REFERENCES streamers(id) ON DELETE CASCADE,
    source_post_id TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    UNIQUE(streamer_id, source_post_id, source_fingerprint)
);

CREATE TABLE IF NOT EXISTS sync_queue (
    mutation_id TEXT PRIMARY KEY,
    operation TEXT NOT NULL,
    entity TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    base_updated_at TEXT,
    device_id TEXT,
    base_revision INTEGER,
    status TEXT NOT NULL DEFAULT 'pending',
    last_error TEXT,
    created_at TEXT NOT NULL,
    applied_at TEXT,
    result_json TEXT
);

CREATE INDEX IF NOT EXISTS idx_sync_queue_status ON sync_queue(status, created_at);

CREATE TABLE IF NOT EXISTS client_sync_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS fertility_cache (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    payload_json TEXT NOT NULL,
    fetched_at TEXT NOT NULL,
    source TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS event_journal (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_event_journal_type ON event_journal(type, seq);
"#;

#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

/// LAPLACE 抓取结果的单行缓存，`id` 恒为 1。
#[derive(Debug, Clone)]
pub struct FertilityCache {
    pub payload_json: String,
    pub fetched_at: DateTime<Utc>,
    pub source: String,
}

#[derive(Debug, sqlx::FromRow)]
struct StreamerRow {
    id: i64,
    mid: String,
    name: String,
    avatar_url: String,
    room_id: Option<String>,
    live_url: Option<String>,
    schedule_enabled: i64,
    live_notify_enabled: i64,
    live_stats_enabled: i64,
    fans: Option<i64>,
    aliases_json: String,
    last_error: Option<String>,
    last_refresh_at: Option<String>,
    live_is_live: i64,
    live_title: Option<String>,
    live_cover: Option<String>,
    live_started_at: Option<String>,
    live_checked_at: Option<String>,
    sort_order: i64,
    created_at: String,
    revision: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct LinkedAccountRow {
    id: i64,
    streamer_id: i64,
    mid: String,
    name: String,
    avatar_url: String,
    revision: i64,
    created_at: String,
}

#[derive(Debug, sqlx::FromRow)]
struct ScheduleRow {
    id: i64,
    streamer_id: i64,
    streamer_name: String,
    streamer_avatar: String,
    participants_json: String,
    title: String,
    start_at: String,
    end_at: Option<String>,
    room_url: Option<String>,
    room_owner_mid: Option<String>,
    room_owner_name: Option<String>,
    source_kind: String,
    source_post_id: Option<String>,
    source_image_url: Option<String>,
    source_author_mid: Option<String>,
    source_fingerprint: Option<String>,
    source_identity: Option<String>,
    source_published_at: Option<String>,
    weekly_rule_id: Option<i64>,
    weekly_occurrence_date: Option<String>,
    confidence: f64,
    manual_locked: i64,
    source_changed: i64,
    cancelled: i64,
    created_at: String,
    updated_at: String,
    revision: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct WeeklyScheduleRuleRow {
    id: i64,
    rule_key: String,
    streamer_id: i64,
    title: String,
    weekdays_mask: i64,
    start_time: String,
    end_time: Option<String>,
    timezone: String,
    room_url: Option<String>,
    effective_from: String,
    effective_until: Option<String>,
    enabled: i64,
    created_at: String,
    updated_at: String,
    revision: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct LiveStatusRow {
    live_schedule_id: Option<i64>,
    streamer_id: i64,
    mid: String,
    streamer_name: String,
    avatar_url: String,
    live_is_live: i64,
    live_title: Option<String>,
    live_cover: Option<String>,
    live_url: Option<String>,
    live_checked_at: Option<String>,
    live_started_at: Option<String>,
    live_notify_enabled: i64,
    revision: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct LiveSessionRow {
    schedule_id: Option<i64>,
    id: i64,
    streamer_id: i64,
    streamer_name: String,
    room_id: String,
    title: String,
    cover_url: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    duration_seconds: i64,
    peak_online: i64,
    peak_high_energy: i64,
    avg_online: i64,
    online_samples: i64,
    online_total: i64,
    revenue_cents: i64,
    gift_revenue_cents: i64,
    super_chat_revenue_cents: i64,
    guard_revenue_cents: i64,
    paid_user_count: i64,
    interactive_user_count: i64,
    danmaku_count: i64,
    gift_count: i64,
    super_chat_count: i64,
    guard_count: i64,
    reconnect_count: i64,
    segment_count: i64,
    data_quality: String,
    collector_state: String,
    created_at: String,
    updated_at: String,
    revision: i64,
}
#[derive(Debug, sqlx::FromRow)]
struct SyncMutationRow {
    mutation_id: String,
    operation: String,
    entity: String,
    payload_json: String,
    base_updated_at: Option<String>,
    device_id: Option<String>,
    base_revision: Option<i64>,
    status: String,
    last_error: Option<String>,
    created_at: String,
    applied_at: Option<String>,
    result_json: Option<String>,
}

impl SqliteStore {
    pub async fn connect(path: impl AsRef<Path>) -> AppResult<Self> {
        Self::connect_path(path.as_ref().to_path_buf()).await
    }

    pub async fn connect_path(path: PathBuf) -> AppResult<Self> {
        let path = path.as_path();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        Self::initialize(pool).await
    }

    pub async fn in_memory() -> AppResult<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        Self::initialize(pool).await
    }

    async fn initialize(pool: SqlitePool) -> AppResult<Self> {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::raw_sql(SCHEMA).execute(&mut *tx).await?;
        sync_feed::initialize_sync_feed(&mut tx).await?;
        for (table, column, definition) in [
            ("streamers", "live_schedule_id", "ALTER TABLE streamers ADD COLUMN live_schedule_id INTEGER"),
            ("live_sessions", "schedule_id", "ALTER TABLE live_sessions ADD COLUMN schedule_id INTEGER REFERENCES schedules(id) ON DELETE SET NULL"),
        ] {
            let columns = sqlx::query(&format!("PRAGMA table_info({table})")).fetch_all(&mut *tx).await?;
            if !columns.iter().any(|row| row.get::<String, _>("name") == column) {
                sqlx::query(definition).execute(&mut *tx).await?;
            }
        }
        let columns = sqlx::query("PRAGMA table_info(sync_queue)")
            .fetch_all(&mut *tx)
            .await?;
        if !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "result_json")
        {
            sqlx::query("ALTER TABLE sync_queue ADD COLUMN result_json TEXT")
                .execute(&mut *tx)
                .await?;
        }
        for (name, definition) in [
            (
                "device_id",
                "ALTER TABLE sync_queue ADD COLUMN device_id TEXT",
            ),
            (
                "base_revision",
                "ALTER TABLE sync_queue ADD COLUMN base_revision INTEGER",
            ),
        ] {
            if !columns
                .iter()
                .any(|row| row.get::<String, _>("name") == name)
            {
                sqlx::query(definition).execute(&mut *tx).await?;
            }
        }
        let streamer_columns = sqlx::query("PRAGMA table_info(streamers)")
            .fetch_all(&mut *tx)
            .await?;
        if !streamer_columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "live_stats_enabled")
        {
            sqlx::query(
                "ALTER TABLE streamers ADD COLUMN live_stats_enabled INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&mut *tx)
            .await?;
        }
        if !streamer_columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "fans")
        {
            sqlx::query("ALTER TABLE streamers ADD COLUMN fans INTEGER")
                .execute(&mut *tx)
                .await?;
        }
        if !streamer_columns
            .iter()
            .any(|row| row.get::<String, _>("name") == "sort_order")
        {
            sqlx::query("ALTER TABLE streamers ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0")
                .execute(&mut *tx)
                .await?;
            let streamer_ids: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM streamers ORDER BY name COLLATE NOCASE, id")
                    .fetch_all(&mut *tx)
                    .await?;
            for (index, streamer_id) in streamer_ids.into_iter().enumerate() {
                sqlx::query("UPDATE streamers SET sort_order = ? WHERE id = ?")
                    .bind(index as i64)
                    .bind(streamer_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let schedule_columns = sqlx::query("PRAGMA table_info(schedules)")
            .fetch_all(&mut *tx)
            .await?;
        let schedule_column_names: Vec<String> = schedule_columns
            .iter()
            .map(|row| row.get::<String, _>("name"))
            .collect();
        for (name, definition) in [
            (
                "participants_json",
                "ALTER TABLE schedules ADD COLUMN participants_json TEXT NOT NULL DEFAULT '[]'",
            ),
            (
                "room_owner_mid",
                "ALTER TABLE schedules ADD COLUMN room_owner_mid TEXT",
            ),
            (
                "room_owner_name",
                "ALTER TABLE schedules ADD COLUMN room_owner_name TEXT",
            ),
            (
                "source_published_at",
                "ALTER TABLE schedules ADD COLUMN source_published_at TEXT",
            ),
            (
                "weekly_rule_id",
                "ALTER TABLE schedules ADD COLUMN weekly_rule_id INTEGER",
            ),
            (
                "weekly_occurrence_date",
                "ALTER TABLE schedules ADD COLUMN weekly_occurrence_date TEXT",
            ),
        ] {
            if !schedule_column_names.iter().any(|column| column == name) {
                sqlx::query(definition).execute(&mut *tx).await?;
            }
        }
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_schedules_weekly_occurrence
             ON schedules(weekly_rule_id, weekly_occurrence_date)
             WHERE weekly_rule_id IS NOT NULL AND weekly_occurrence_date IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mutation_receipts (
            mutation_id TEXT PRIMARY KEY, request_json TEXT NOT NULL,
            result_json TEXT NOT NULL, applied_at TEXT NOT NULL)",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS remote_entity_map (
            server_key TEXT NOT NULL, entity TEXT NOT NULL, remote_id INTEGER NOT NULL,
            local_id INTEGER NOT NULL, remote_updated_at TEXT,
            PRIMARY KEY(server_key,entity,remote_id))",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn backup_to(&self, path: impl AsRef<Path>) -> AppResult<()> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() {
            return Err(AppError::validation("备份路径不能为空"));
        }
        if tokio::fs::try_exists(path).await? {
            return Err(AppError::validation("备份目标已存在，拒绝覆盖"));
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let backup_path = path
            .to_str()
            .ok_or_else(|| AppError::validation("备份路径必须是有效 UTF-8"))?;
        sqlx::query("VACUUM INTO ?1")
            .bind(backup_path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_streamers(&self) -> AppResult<Vec<Streamer>> {
        let rows = sqlx::query_as::<_, StreamerRow>(
            "SELECT id, mid, name, avatar_url, room_id, live_url, schedule_enabled, live_notify_enabled, live_stats_enabled, fans,
                    aliases_json, last_error, last_refresh_at, live_is_live, live_title, live_cover,
                    live_started_at, live_checked_at, sort_order, created_at, revision
             FROM streamers ORDER BY sort_order, name COLLATE NOCASE, id",
        )
        .fetch_all(&self.pool)
        .await?;
        self.map_streamers(rows).await
    }

    pub async fn get_streamer(&self, id: i64) -> AppResult<Streamer> {
        let row = sqlx::query_as::<_, StreamerRow>(
            "SELECT id, mid, name, avatar_url, room_id, live_url, schedule_enabled, live_notify_enabled, live_stats_enabled, fans,
                    aliases_json, last_error, last_refresh_at, live_is_live, live_title, live_cover,
                    live_started_at, live_checked_at, sort_order, created_at, revision
             FROM streamers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found(format!("主播 {id}")))?;
        let mut streamers = self.map_streamers(vec![row]).await?;
        streamers
            .pop()
            .ok_or_else(|| AppError::not_found(format!("主播 {id}")))
    }

    pub async fn get_streamer_by_mid(&self, mid: &str) -> AppResult<Option<Streamer>> {
        let row = sqlx::query_as::<_, StreamerRow>(
            "SELECT id, mid, name, avatar_url, room_id, live_url, schedule_enabled, live_notify_enabled, live_stats_enabled, fans,
                    aliases_json, last_error, last_refresh_at, live_is_live, live_title, live_cover,
                    live_started_at, live_checked_at, sort_order, created_at, revision
             FROM streamers WHERE mid = ?",
        )
        .bind(mid)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(self.map_streamers(vec![row]).await?.pop()),
            None => Ok(None),
        }
    }

    async fn map_streamers(&self, rows: Vec<StreamerRow>) -> AppResult<Vec<Streamer>> {
        let link_rows = sqlx::query(
            "SELECT id, streamer_id, mid, name, avatar_url, revision, created_at
             FROM linked_accounts ORDER BY name COLLATE NOCASE",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut by_streamer: std::collections::HashMap<i64, Vec<LinkedAccount>> =
            std::collections::HashMap::new();
        for row in link_rows {
            let streamer_id: i64 = row.try_get("streamer_id")?;
            by_streamer
                .entry(streamer_id)
                .or_default()
                .push(LinkedAccount {
                    id: row.try_get("id")?,
                    streamer_id,
                    mid: row.try_get("mid")?,
                    name: row.try_get("name")?,
                    avatar_url: row.try_get("avatar_url")?,
                    revision: row.try_get("revision")?,
                    created_at: parse_ts(row.try_get::<String, _>("created_at")?.as_str())?,
                });
        }
        rows.into_iter()
            .map(|row| {
                let aliases = serde_json::from_str::<Vec<String>>(&row.aliases_json)
                    .map_err(|_| AppError::validation("主播别名数据损坏"))?;
                Ok(Streamer {
                    id: row.id,
                    mid: row.mid,
                    name: row.name,
                    avatar_url: row.avatar_url,
                    room_id: row.room_id,
                    live_url: row.live_url,
                    aliases,
                    schedule_enabled: row.schedule_enabled != 0,
                    live_notify_enabled: row.live_notify_enabled != 0,
                    live_stats_enabled: row.live_stats_enabled != 0,
                    fans: row.fans.and_then(|value| u64::try_from(value).ok()),
                    last_error: row.last_error,
                    last_refresh_at: parse_opt_ts(row.last_refresh_at.as_deref())?,
                    is_live: row.live_is_live != 0,
                    live_title: row.live_title,
                    live_cover: row.live_cover,
                    live_started_at: parse_opt_ts(row.live_started_at.as_deref())?,
                    live_checked_at: parse_opt_ts(row.live_checked_at.as_deref())?,
                    linked_accounts: by_streamer.remove(&row.id).unwrap_or_default(),
                    sort_order: row.sort_order,
                    revision: row.revision,
                    created_at: parse_ts(&row.created_at)?,
                })
            })
            .collect()
    }

    pub async fn add_subscription(&self, input: &AddSubscription) -> AppResult<Streamer> {
        validate_mid(&input.mid)?;
        let name = input
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AppError::validation("主播昵称不能为空"))?;
        if name.chars().count() > 80 {
            return Err(AppError::validation("主播昵称不能超过 80 字"));
        }
        let avatar = input.avatar_url.as_deref().unwrap_or_default();
        let room_id = input
            .room_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let live_url = room_id.map(live_url_for_room);
        let mut alias_pool = self
            .get_streamer_by_mid(&input.mid)
            .await?
            .map(|streamer| streamer.aliases)
            .unwrap_or_default();
        alias_pool.extend(input.aliases.clone().unwrap_or_default());
        let aliases = normalize_aliases(alias_pool, name);
        let aliases_json = serde_json::to_string(&aliases)?;
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let next_sort_order: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(sort_order), -1) + 1 FROM streamers")
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query(
            "INSERT INTO streamers
                (mid, name, avatar_url, room_id, live_url, schedule_enabled, live_notify_enabled, live_stats_enabled,
                 aliases_json, sort_order, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(mid) DO UPDATE SET
                name = excluded.name,
                avatar_url = CASE
                    WHEN excluded.avatar_url LIKE 'data:image/%' THEN excluded.avatar_url
                    WHEN streamers.avatar_url LIKE 'data:image/%' THEN streamers.avatar_url
                    WHEN excluded.avatar_url <> '' THEN excluded.avatar_url
                    ELSE streamers.avatar_url END,
                room_id = COALESCE(excluded.room_id, streamers.room_id),
                live_url = COALESCE(excluded.live_url, streamers.live_url),
                aliases_json = excluded.aliases_json,
                updated_at = excluded.updated_at,
                revision = streamers.revision + 1",
        )
        .bind(&input.mid)
        .bind(name)
        .bind(avatar)
        .bind(room_id)
        .bind(&live_url)
        .bind(input.schedule_enabled)
        .bind(input.live_notify_enabled)
        .bind(input.live_stats_enabled)
        .bind(aliases_json)
        .bind(next_sort_order)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        let id: i64 = sqlx::query_scalar("SELECT id FROM streamers WHERE mid = ?")
            .bind(&input.mid)
            .fetch_one(&mut *tx)
            .await?;
        self.emit_streamer_change(&mut tx, id, "upsert", SyncActor::system())
            .await?;
        tx.commit().await?;
        self.get_streamer_by_mid(&input.mid)
            .await?
            .ok_or_else(|| AppError::not_found(format!("主播 {}", input.mid)))
    }

    pub async fn update_streamer_avatar(&self, id: i64, avatar_url: &str) -> AppResult<Streamer> {
        let avatar_url = avatar_url.trim();
        if avatar_url.is_empty() {
            return Err(AppError::validation("头像数据不能为空"));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated =
            sqlx::query("UPDATE streamers SET avatar_url = ?, updated_at = ?, revision = revision + 1 WHERE id = ?")
                .bind(avatar_url)
                .bind(Utc::now().to_rfc3339())
                .bind(id)
                .execute(&mut *tx)
                .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::not_found(format!("主播 {id}")));
        }
        self.emit_streamer_change(&mut tx, id, "upsert", SyncActor::system())
            .await?;
        tx.commit().await?;
        self.get_streamer(id).await
    }

    pub async fn record_fan_snapshot(
        &self,
        streamer_id: i64,
        fans: u64,
        captured_at: DateTime<Utc>,
    ) -> AppResult<FanSnapshot> {
        let fans_value =
            i64::try_from(fans).map_err(|_| AppError::validation("粉丝数超出可存储范围"))?;
        let captured_at = captured_at.to_rfc3339();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM streamers WHERE id = ?")
            .bind(streamer_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::not_found(format!("主播 {streamer_id}")));
        }
        sqlx::query(
            "INSERT INTO fan_snapshots(streamer_id, captured_at, fans)
             VALUES(?, ?, ?)
             ON CONFLICT(streamer_id, captured_at) DO UPDATE SET fans = excluded.fans",
        )
        .bind(streamer_id)
        .bind(&captured_at)
        .bind(fans_value)
        .execute(&mut *tx)
        .await?;
        let updated = sqlx::query(
            "UPDATE streamers SET fans = ?, updated_at = ?, revision = revision + 1
             WHERE id = ? AND (fans IS NULL OR fans <> ?)
               AND ? = (
                   SELECT captured_at FROM fan_snapshots
                   WHERE streamer_id = ? ORDER BY captured_at DESC LIMIT 1
               )",
        )
        .bind(fans_value)
        .bind(Utc::now().to_rfc3339())
        .bind(streamer_id)
        .bind(fans_value)
        .bind(&captured_at)
        .bind(streamer_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() > 0 {
            self.emit_streamer_change(&mut tx, streamer_id, "upsert", SyncActor::system())
                .await?;
        }
        tx.commit().await?;
        Ok(FanSnapshot {
            captured_at: parse_ts(&captured_at)?,
            fans,
        })
    }

    pub async fn list_fan_snapshots(
        &self,
        streamer_id: i64,
        since: DateTime<Utc>,
    ) -> AppResult<Vec<FanSnapshot>> {
        let rows = sqlx::query(
            "SELECT captured_at, fans FROM fan_snapshots
             WHERE streamer_id = ? AND captured_at >= ?
             ORDER BY captured_at",
        )
        .bind(streamer_id)
        .bind(since.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let fans: i64 = row.try_get("fans")?;
                Ok(FanSnapshot {
                    captured_at: parse_ts(row.try_get::<String, _>("captured_at")?.as_str())?,
                    fans: u64::try_from(fans)
                        .map_err(|_| AppError::validation("粉丝快照数据损坏"))?,
                })
            })
            .collect()
    }

    pub async fn update_subscription(
        &self,
        id: i64,
        update: &UpdateSubscription,
    ) -> AppResult<Streamer> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = sqlx::query("SELECT name, revision FROM streamers WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::not_found(format!("主播 {id}")))?;
        let current_name: String = current.try_get("name")?;
        let current_revision: i64 = current.try_get("revision")?;
        let changed = update.schedule_enabled.is_some()
            || update.live_notify_enabled.is_some()
            || update.live_stats_enabled.is_some()
            || update.aliases.is_some();
        if !changed {
            tx.commit().await?;
            return self.get_streamer(id).await;
        }
        let aliases_json = update
            .aliases
            .as_ref()
            .map(|aliases| normalize_aliases(aliases.clone(), &current_name))
            .map(|aliases| serde_json::to_string(&aliases))
            .transpose()?;
        let updated = sqlx::query(
            "UPDATE streamers SET
                    schedule_enabled = COALESCE(?, schedule_enabled),
                    live_notify_enabled = COALESCE(?, live_notify_enabled),
                    live_stats_enabled = COALESCE(?, live_stats_enabled),
                    aliases_json = COALESCE(?, aliases_json),
                    updated_at = ?, revision = revision + 1
             WHERE id = ? AND revision = ?",
        )
        .bind(update.schedule_enabled)
        .bind(update.live_notify_enabled)
        .bind(update.live_stats_enabled)
        .bind(aliases_json.as_deref())
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .bind(current_revision)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::validation("主播设置在更新期间已被修改"));
        }
        self.emit_streamer_change(&mut tx, id, "upsert", SyncActor::system())
            .await?;
        tx.commit().await?;
        self.get_streamer(id).await
    }

    pub async fn reorder_subscriptions(&self, ids: &[i64]) -> AppResult<Vec<Streamer>> {
        let mut requested = ids.to_vec();
        requested.sort_unstable();
        if requested.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AppError::validation("主播排序中存在重复项"));
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut existing: Vec<i64> = sqlx::query_scalar("SELECT id FROM streamers")
            .fetch_all(&mut *tx)
            .await?;
        existing.sort_unstable();
        if requested != existing {
            return Err(AppError::validation("主播排序必须包含当前全部主播"));
        }

        let now = Utc::now().to_rfc3339();
        for (index, streamer_id) in ids.iter().enumerate() {
            sqlx::query(
                "UPDATE streamers SET sort_order = ?, updated_at = ?, revision = revision + 1 WHERE id = ?",
            )
            .bind(index as i64)
            .bind(&now)
            .bind(streamer_id)
            .execute(&mut *tx)
            .await?;
        }
        for streamer_id in ids {
            self.emit_streamer_change(&mut tx, *streamer_id, "upsert", SyncActor::system())
                .await?;
        }
        tx.commit().await?;
        self.list_streamers().await
    }

    pub async fn delete_subscription(&self, id: i64) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM streamers WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::not_found(format!("主播 {id}")));
        }
        let linked_ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM linked_accounts WHERE streamer_id = ? ORDER BY id")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        let schedule_ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM schedules WHERE streamer_id = ? ORDER BY id")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        for schedule_id in &schedule_ids {
            let source_ids: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM schedule_sources WHERE schedule_id = ? ORDER BY id",
            )
            .bind(schedule_id)
            .fetch_all(&mut *tx)
            .await?;
            for source_id in source_ids {
                self.emit_schedule_source_change(&mut tx, source_id, "delete", SyncActor::system())
                    .await?;
            }
            self.emit_schedule_change(&mut tx, *schedule_id, "delete", SyncActor::system())
                .await?;
        }
        for linked_id in linked_ids {
            self.emit_linked_account_change(&mut tx, linked_id, "delete", SyncActor::system())
                .await?;
        }
        let suppression_ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM source_suppressions WHERE streamer_id = ? ORDER BY id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        for suppression_id in suppression_ids {
            self.emit_source_suppression_change(
                &mut tx,
                suppression_id,
                "delete",
                SyncActor::system(),
            )
            .await?;
        }
        let rule_ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM weekly_schedule_rules WHERE streamer_id = ? ORDER BY id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        for rule_id in rule_ids {
            let exception_ids: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM weekly_schedule_exceptions WHERE rule_id = ? ORDER BY id",
            )
            .bind(rule_id)
            .fetch_all(&mut *tx)
            .await?;
            for exception_id in exception_ids {
                self.emit_weekly_exception_change(
                    &mut tx,
                    exception_id,
                    "delete",
                    SyncActor::system(),
                )
                .await?;
            }
            self.emit_weekly_rule_change(&mut tx, rule_id, "delete", SyncActor::system())
                .await?;
        }
        let live_session_ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM live_sessions WHERE streamer_id = ? ORDER BY id")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        for live_session_id in live_session_ids {
            self.emit_live_session_delete(&mut tx, live_session_id, SyncActor::system())
                .await?;
        }
        self.emit_live_status_delete(&mut tx, id, SyncActor::system())
            .await?;
        self.emit_streamer_change(&mut tx, id, "delete", SyncActor::system())
            .await?;
        sqlx::query("DELETE FROM source_suppressions WHERE streamer_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM linked_accounts WHERE streamer_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM schedules WHERE streamer_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM streamers WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn add_linked_account(
        &self,
        streamer_id: i64,
        mid: &str,
        name: &str,
        avatar_url: &str,
    ) -> AppResult<LinkedAccount> {
        validate_mid(mid)?;
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err(AppError::validation("官号昵称长度必须为 1 到 80 字"));
        }
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO linked_accounts (streamer_id, mid, name, avatar_url, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(streamer_id, mid) DO UPDATE SET
                name = excluded.name,
                avatar_url = CASE
                    WHEN excluded.avatar_url LIKE 'data:image/%' THEN excluded.avatar_url
                    WHEN linked_accounts.avatar_url LIKE 'data:image/%' THEN linked_accounts.avatar_url
                    WHEN excluded.avatar_url <> '' THEN excluded.avatar_url
                    ELSE linked_accounts.avatar_url END,
                revision = linked_accounts.revision + 1",
        )
        .bind(streamer_id)
        .bind(mid)
        .bind(name)
        .bind(avatar_url)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query_as::<_, LinkedAccountRow>(
            "SELECT id, streamer_id, mid, name, avatar_url, revision, created_at
             FROM linked_accounts WHERE streamer_id = ? AND mid = ?",
        )
        .bind(streamer_id)
        .bind(mid)
        .fetch_one(&mut *tx)
        .await?;
        self.emit_linked_account_change(&mut tx, row.id, "upsert", SyncActor::system())
            .await?;
        tx.commit().await?;
        Ok(LinkedAccount {
            id: row.id,
            streamer_id: row.streamer_id,
            mid: row.mid,
            name: row.name,
            avatar_url: row.avatar_url,
            revision: row.revision,
            created_at: parse_ts(&row.created_at)?,
        })
    }

    pub async fn remove_linked_account(&self, streamer_id: i64, mid: &str) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM linked_accounts WHERE streamer_id = ? AND mid = ?")
                .bind(streamer_id)
                .bind(mid)
                .fetch_optional(&mut *tx)
                .await?;
        let id = id.ok_or_else(|| AppError::not_found(format!("官号 {mid}")))?;
        self.emit_linked_account_change(&mut tx, id, "delete", SyncActor::system())
            .await?;
        sqlx::query("DELETE FROM linked_accounts WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn save_dynamic_post(&self, post: &DynamicPost) -> AppResult<()> {
        let images = serde_json::to_string(&post.images)?;
        let reserve = post
            .reserve
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        sqlx::query(
            "INSERT INTO dynamic_posts
                (id, author_mid, author_name, published_at, text, images_json, source_url, raw_kind,
                 reserve_json, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                author_name = excluded.author_name,
                text = excluded.text,
                images_json = excluded.images_json,
                reserve_json = excluded.reserve_json,
                fetched_at = excluded.fetched_at",
        )
        .bind(&post.id)
        .bind(&post.author_mid)
        .bind(&post.author_name)
        .bind(post.published_at.to_rfc3339())
        .bind(truncate(&post.text, 65_536))
        .bind(images)
        .bind(&post.source_url)
        .bind(&post.raw_kind)
        .bind(reserve)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_image_analysis(&self, cache_key: &str) -> AppResult<Option<Value>> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT result_json FROM image_analyses WHERE cache_key = ?")
                .bind(cache_key)
                .fetch_optional(&self.pool)
                .await?;
        value
            .map(|text| serde_json::from_str(&text).map_err(AppError::from))
            .transpose()
    }

    pub async fn get_text_analysis(&self, cache_key: &str) -> AppResult<Option<Value>> {
        let value: Option<String> =
            sqlx::query_scalar("SELECT result_json FROM text_analyses WHERE cache_key = ?")
                .bind(cache_key)
                .fetch_optional(&self.pool)
                .await?;
        value
            .map(|text| serde_json::from_str(&text).map_err(AppError::from))
            .transpose()
    }

    pub async fn save_text_analysis(
        &self,
        cache_key: &str,
        post_id: &str,
        text_digest: &str,
        model: &str,
        prompt_version: &str,
        result: &Value,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO text_analyses
                (cache_key, post_id, text_digest, model, prompt_version, result_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(cache_key) DO UPDATE SET result_json = excluded.result_json",
        )
        .bind(cache_key)
        .bind(post_id)
        .bind(text_digest)
        .bind(model)
        .bind(prompt_version)
        .bind(serde_json::to_string(result)?)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn save_image_analysis(
        &self,
        cache_key: &str,
        post_id: &str,
        image_url: &str,
        model: &str,
        prompt_version: &str,
        result: &Value,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO image_analyses
                (cache_key, post_id, image_url, model, prompt_version, result_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(cache_key) DO UPDATE SET result_json = excluded.result_json",
        )
        .bind(cache_key)
        .bind(post_id)
        .bind(image_url)
        .bind(model)
        .bind(prompt_version)
        .bind(serde_json::to_string(result)?)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 一条动态一次调用后的整体结果缓存，按 `post_id:content_digest:model:prompt_version` 去重。
    pub async fn save_post_analysis(
        &self,
        cache_key: &str,
        post_id: &str,
        content_digest: &str,
        model: &str,
        prompt_version: &str,
        result: &Value,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO post_analyses
                (cache_key, post_id, content_digest, model, prompt_version, result_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(cache_key)
        .bind(post_id)
        .bind(content_digest)
        .bind(model)
        .bind(prompt_version)
        .bind(serde_json::to_string(result)?)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_post_analysis(&self, cache_key: &str) -> AppResult<Option<Value>> {
        Ok(self
            .load_post_analysis_entry(cache_key)
            .await?
            .map(|(result, _)| result))
    }

    /// 连同写入时间一起取回来：空结果要按“多久之后重试”判断是否还算命中。
    pub async fn load_post_analysis_entry(
        &self,
        cache_key: &str,
    ) -> AppResult<Option<(Value, String)>> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT result_json, created_at FROM post_analyses WHERE cache_key = ?")
                .bind(cache_key)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(text, created_at)| {
            serde_json::from_str(&text)
                .map(|value| (value, created_at))
                .map_err(AppError::from)
        })
        .transpose()
    }

    pub async fn get_image_analysis_cache_key(
        &self,
        post_id: &str,
        image_url: &str,
        model: &str,
        prompt_version: &str,
    ) -> AppResult<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT cache_key FROM image_analysis_aliases
             WHERE post_id = ? AND image_url = ? AND model = ? AND prompt_version = ?",
        )
        .bind(post_id)
        .bind(image_url)
        .bind(model)
        .bind(prompt_version)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn save_image_analysis_cache_key(
        &self,
        post_id: &str,
        image_url: &str,
        model: &str,
        prompt_version: &str,
        cache_key: &str,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO image_analysis_aliases
                (post_id, image_url, model, prompt_version, cache_key, created_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(post_id, image_url, model, prompt_version)
             DO UPDATE SET cache_key = excluded.cache_key, created_at = excluded.created_at",
        )
        .bind(post_id)
        .bind(image_url)
        .bind(model)
        .bind(prompt_version)
        .bind(cache_key)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn upsert_schedule(&self, input: &NewSchedule) -> AppResult<(Schedule, bool)> {
        validate_schedule_input(input)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (id, inserted) = self.upsert_schedule_on(&mut tx, input).await?;
        tx.commit().await?;
        Ok((self.get_schedule(id).await?, inserted))
    }

    async fn upsert_schedule_on(
        &self,
        tx: &mut SqliteConnection,
        input: &NewSchedule,
    ) -> AppResult<(i64, bool)> {
        validate_schedule_input(input)?;
        let existing_id: Option<i64> = if let Some(identity) = input.source_identity.as_deref() {
            // 先按来源 identity 找同一份来源写入过的场次；找不到再退回自然键。
            // 直接 INSERT 会撞 UNIQUE(streamer_id, start_at, title)：同一个场次
            // 可能先由预约卡或另一条动态以别的 identity 落库，这次是正常写入，
            // 不该把整批刷新打断。
            let by_identity = sqlx::query_scalar(
                "SELECT id FROM schedules WHERE streamer_id = ? AND source_identity = ?",
            )
            .bind(input.streamer_id)
            .bind(identity)
            .fetch_optional(&mut *tx)
            .await?;
            match by_identity {
                Some(id) => Some(id),
                None => sqlx::query_scalar(
                    "SELECT id FROM schedules WHERE streamer_id = ? AND start_at = ? AND title = ?",
                )
                .bind(input.streamer_id)
                .bind(input.start_at.to_rfc3339())
                .bind(&input.title)
                .fetch_optional(&mut *tx)
                .await?,
            }
        } else {
            sqlx::query_scalar(
                "SELECT id FROM schedules WHERE streamer_id = ? AND start_at = ? AND title = ?",
            )
            .bind(input.streamer_id)
            .bind(input.start_at.to_rfc3339())
            .bind(&input.title)
            .fetch_optional(&mut *tx)
            .await?
        };

        if let Some(existing_id) = existing_id {
            let current = sync_feed::load_schedule(tx, existing_id).await?;
            let unchanged = if current.manual_locked {
                locked_schedule_source_matches(&current, input)
            } else {
                !current.cancelled && schedule_matches_input(&current, input)
            };
            if unchanged {
                self.attach_schedule_source_on(tx, existing_id, input, SyncActor::collector())
                    .await?;
                tx.commit().await?;
                return Ok((self.get_schedule(existing_id).await?, false));
            }
            let now = Utc::now().to_rfc3339();
            let manual_locked: i64 =
                sqlx::query_scalar("SELECT manual_locked FROM schedules WHERE id = ?")
                    .bind(existing_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if manual_locked != 0 {
                sqlx::query(
                    "UPDATE schedules SET
                        source_changed = CASE
                            WHEN source_fingerprint IS NOT NULL AND ? IS NOT NULL AND source_fingerprint <> ?
                            THEN 1 ELSE source_changed END,
                        source_fingerprint = COALESCE(?, source_fingerprint),
                        source_image_url = COALESCE(?, source_image_url),
                        source_author_mid = COALESCE(?, source_author_mid),
                        updated_at = ?, revision = revision + 1
                     WHERE id = ?",
                )
                .bind(&input.source_fingerprint)
                .bind(&input.source_fingerprint)
                .bind(&input.source_fingerprint)
                .bind(&input.source_image_url)
                .bind(&input.source_author_mid)
                .bind(now)
                .bind(existing_id)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query(
                    "UPDATE schedules SET participants_json = ?, title = ?, start_at = ?, end_at = ?,
                        room_url = ?, room_owner_mid = ?, room_owner_name = ?, confidence = ?,
                        source_kind = ?, source_post_id = ?, source_image_url = ?,
                        source_author_mid = ?, source_fingerprint = ?, source_published_at = ?,
                        cancelled = 0, updated_at = ?, revision = revision + 1
                     WHERE id = ?",
                )
                .bind(serde_json::to_string(&input.participants)?)
                .bind(&input.title)
                .bind(input.start_at.to_rfc3339())
                .bind(input.end_at.map(|value| value.to_rfc3339()))
                .bind(&input.room_url)
                .bind(&input.room_owner_mid)
                .bind(&input.room_owner_name)
                .bind(input.confidence)
                .bind(&input.source_kind)
                .bind(&input.source_post_id)
                .bind(&input.source_image_url)
                .bind(&input.source_author_mid)
                .bind(&input.source_fingerprint)
                .bind(input.source_published_at.map(|value| value.to_rfc3339()))
                .bind(now)
                .bind(existing_id)
                .execute(&mut *tx)
                .await?;
            }
            self.emit_schedule_change(tx, existing_id, "upsert", SyncActor::collector())
                .await?;
            self.attach_schedule_source_on(tx, existing_id, input, SyncActor::collector())
                .await?;
            return Ok((existing_id, false));
        }

        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "INSERT INTO schedules
                (streamer_id, participants_json, title, start_at, end_at, room_url,
                 room_owner_mid, room_owner_name, source_kind, source_post_id,
                 source_image_url, source_author_mid, source_fingerprint, source_identity,
                 source_published_at, weekly_rule_id, weekly_occurrence_date, confidence, manual_locked, source_changed, cancelled,
                 created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, 0, 0, 0, ?, ?)",
        )
        .bind(input.streamer_id)
        .bind(serde_json::to_string(&input.participants)?)
        .bind(&input.title)
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
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        let id = result.last_insert_rowid();
        self.emit_schedule_change(tx, id, "upsert", SyncActor::collector())
            .await?;
        self.attach_schedule_source_on(tx, id, input, SyncActor::collector())
            .await?;
        Ok((id, true))
    }

    pub async fn find_schedule_near(
        &self,
        streamer_id: i64,
        start_at: DateTime<Utc>,
        window_minutes: i64,
    ) -> AppResult<Option<Schedule>> {
        let window = chrono::Duration::minutes(window_minutes.clamp(1, 60));
        let id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM schedules
             WHERE streamer_id = ? AND start_at >= ? AND start_at <= ? AND cancelled = 0
             ORDER BY ABS(strftime('%s', start_at) - strftime('%s', ?)), id LIMIT 1",
        )
        .bind(streamer_id)
        .bind((start_at - window).to_rfc3339())
        .bind((start_at + window).to_rfc3339())
        .bind(start_at.to_rfc3339())
        .fetch_optional(&self.pool)
        .await?;
        match id {
            Some(id) => Ok(Some(self.get_schedule(id).await?)),
            None => Ok(None),
        }
    }

    pub async fn list_schedules_near_all(
        &self,
        start_at: DateTime<Utc>,
        window_minutes: i64,
    ) -> AppResult<Vec<Schedule>> {
        let window = chrono::Duration::minutes(window_minutes.clamp(1, 60));
        let rows = sqlx::query_as::<_, ScheduleRow>(
            "SELECT s.id, s.streamer_id, st.name AS streamer_name, st.avatar_url AS streamer_avatar,
                    s.participants_json, s.title, s.start_at, s.end_at, s.room_url,
                    s.room_owner_mid, s.room_owner_name, s.source_kind, s.source_post_id,
                    s.source_image_url, s.source_author_mid, s.source_fingerprint, s.source_identity,
                    s.source_published_at, s.weekly_rule_id, s.weekly_occurrence_date, s.confidence, s.manual_locked, s.source_changed,
                    s.cancelled, s.created_at, s.updated_at, s.revision
             FROM schedules s JOIN streamers st ON st.id = s.streamer_id
             WHERE s.cancelled = 0 AND s.start_at >= ? AND s.start_at <= ?
             ORDER BY ABS(strftime('%s', s.start_at) - strftime('%s', ?)), s.id",
        )
        .bind((start_at - window).to_rfc3339())
        .bind((start_at + window).to_rfc3339())
        .bind(start_at.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_schedule).collect()
    }

    pub async fn list_schedules_near(
        &self,
        streamer_id: i64,
        start_at: DateTime<Utc>,
        window_minutes: i64,
    ) -> AppResult<Vec<Schedule>> {
        let window = chrono::Duration::minutes(window_minutes.clamp(1, 60));
        let rows = sqlx::query_as::<_, ScheduleRow>(
            "SELECT s.id, s.streamer_id, st.name AS streamer_name, st.avatar_url AS streamer_avatar,
                    s.participants_json, s.title, s.start_at, s.end_at, s.room_url,
                    s.room_owner_mid, s.room_owner_name, s.source_kind, s.source_post_id,
                    s.source_image_url, s.source_author_mid, s.source_fingerprint, s.source_identity,
                    s.source_published_at, s.weekly_rule_id, s.weekly_occurrence_date, s.confidence, s.manual_locked, s.source_changed,
                    s.cancelled, s.created_at, s.updated_at, s.revision
             FROM schedules s JOIN streamers st ON st.id = s.streamer_id
             WHERE s.streamer_id = ? AND s.cancelled = 0
               AND s.start_at >= ? AND s.start_at <= ?
             ORDER BY ABS(strftime('%s', s.start_at) - strftime('%s', ?)), s.id",
        )
        .bind(streamer_id)
        .bind((start_at - window).to_rfc3339())
        .bind((start_at + window).to_rfc3339())
        .bind(start_at.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_schedule).collect()
    }

    pub async fn attach_schedule_source(
        &self,
        schedule_id: i64,
        input: &NewSchedule,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.attach_schedule_source_on(&mut tx, schedule_id, input, SyncActor::collector())
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn merge_schedule_auto(
        &self,
        schedule_id: i64,
        input: &NewSchedule,
    ) -> AppResult<Schedule> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = sync_feed::load_schedule(&mut tx, schedule_id).await?;
        if current.manual_locked || schedule_matches_input(&current, input) {
            self.attach_schedule_source_on(&mut tx, schedule_id, input, SyncActor::collector())
                .await?;
            tx.commit().await?;
            return self.get_schedule(schedule_id).await;
        }
        sqlx::query(
            "UPDATE schedules SET participants_json = ?, title = ?, start_at = ?, end_at = ?,
                    room_url = ?, room_owner_mid = ?, room_owner_name = ?, confidence = ?,
                    source_kind = ?, source_post_id = ?, source_image_url = ?,
                    source_author_mid = ?, source_fingerprint = ?, source_published_at = ?,
                    updated_at = ?, revision = revision + 1
             WHERE id = ? AND manual_locked = 0",
        )
        .bind(serde_json::to_string(&input.participants)?)
        .bind(&input.title)
        .bind(input.start_at.to_rfc3339())
        .bind(input.end_at.map(|value| value.to_rfc3339()))
        .bind(&input.room_url)
        .bind(&input.room_owner_mid)
        .bind(&input.room_owner_name)
        .bind(input.confidence)
        .bind(&input.source_kind)
        .bind(&input.source_post_id)
        .bind(&input.source_image_url)
        .bind(&input.source_author_mid)
        .bind(&input.source_fingerprint)
        .bind(input.source_published_at.map(|value| value.to_rfc3339()))
        .bind(Utc::now().to_rfc3339())
        .bind(schedule_id)
        .execute(&mut *tx)
        .await?;
        self.emit_schedule_change(&mut tx, schedule_id, "upsert", SyncActor::collector())
            .await?;
        self.attach_schedule_source_on(&mut tx, schedule_id, input, SyncActor::collector())
            .await?;
        tx.commit().await?;
        self.get_schedule(schedule_id).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn merge_and_delete_schedule_duplicate(
        &self,
        canonical_id: i64,
        canonical_revision: i64,
        duplicate_id: i64,
        duplicate_revision: i64,
        merge_input: Option<&NewSchedule>,
    ) -> AppResult<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let canonical = sqlx::query("SELECT manual_locked, revision FROM schedules WHERE id = ?")
            .bind(canonical_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::not_found(format!("日程 {canonical_id}")))?;
        let canonical_locked: i64 = canonical.try_get("manual_locked")?;
        let current_canonical_revision: i64 = canonical.try_get("revision")?;

        let duplicate = sqlx::query("SELECT manual_locked, revision FROM schedules WHERE id = ?")
            .bind(duplicate_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::not_found(format!("日程 {duplicate_id}")))?;
        let duplicate_locked: i64 = duplicate.try_get("manual_locked")?;
        let current_duplicate_revision: i64 = duplicate.try_get("revision")?;

        if duplicate_locked != 0
            || current_duplicate_revision != duplicate_revision
            || current_canonical_revision != canonical_revision
            || (merge_input.is_some() && canonical_locked != 0)
        {
            tx.commit().await?;
            return Ok(false);
        }

        if let Some(merge_input) = merge_input {
            let updated = sqlx::query(
                "UPDATE schedules SET participants_json = ?, title = ?, start_at = ?, end_at = ?,
                        room_url = ?, room_owner_mid = ?, room_owner_name = ?, confidence = ?,
                        source_kind = ?, source_post_id = ?, source_image_url = ?,
                        source_author_mid = ?, source_fingerprint = ?, source_published_at = ?,
                        updated_at = ?, revision = revision + 1
                 WHERE id = ? AND manual_locked = 0 AND revision = ?",
            )
            .bind(serde_json::to_string(&merge_input.participants)?)
            .bind(&merge_input.title)
            .bind(merge_input.start_at.to_rfc3339())
            .bind(merge_input.end_at.map(|value| value.to_rfc3339()))
            .bind(&merge_input.room_url)
            .bind(&merge_input.room_owner_mid)
            .bind(&merge_input.room_owner_name)
            .bind(merge_input.confidence)
            .bind(&merge_input.source_kind)
            .bind(&merge_input.source_post_id)
            .bind(&merge_input.source_image_url)
            .bind(&merge_input.source_author_mid)
            .bind(&merge_input.source_fingerprint)
            .bind(
                merge_input
                    .source_published_at
                    .map(|value| value.to_rfc3339()),
            )
            .bind(Utc::now().to_rfc3339())
            .bind(canonical_id)
            .bind(canonical_revision)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(AppError::validation("日程在合并期间已被修改"));
            }
            self.emit_schedule_change(&mut tx, canonical_id, "upsert", SyncActor::collector())
                .await?;
        }

        let source_identities: Vec<String> = sqlx::query_scalar(
            "SELECT source_identity FROM schedule_sources WHERE schedule_id = ? ORDER BY id",
        )
        .bind(duplicate_id)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO schedule_sources
                (schedule_id, source_kind, source_post_id, source_image_url,
                 source_fingerprint, source_identity, confidence, created_at, revision)
             SELECT ?, source_kind, source_post_id, source_image_url,
                    source_fingerprint, source_identity, confidence, created_at, revision
             FROM schedule_sources WHERE schedule_id = ?
             ON CONFLICT(schedule_id, source_identity) DO UPDATE SET
                confidence = MAX(schedule_sources.confidence, excluded.confidence),
                source_kind = CASE WHEN excluded.confidence > schedule_sources.confidence
                    THEN excluded.source_kind ELSE schedule_sources.source_kind END,
                source_post_id = CASE WHEN excluded.confidence > schedule_sources.confidence
                    THEN excluded.source_post_id ELSE schedule_sources.source_post_id END,
                source_image_url = COALESCE(excluded.source_image_url, schedule_sources.source_image_url),
                source_fingerprint = COALESCE(excluded.source_fingerprint, schedule_sources.source_fingerprint),
                revision = schedule_sources.revision + 1",
        )
        .bind(canonical_id)
        .bind(duplicate_id)
        .execute(&mut *tx)
        .await?;
        for identity in source_identities {
            let source_id: i64 = sqlx::query_scalar(
                "SELECT id FROM schedule_sources WHERE schedule_id = ? AND source_identity = ?",
            )
            .bind(canonical_id)
            .bind(identity)
            .fetch_one(&mut *tx)
            .await?;
            self.emit_schedule_source_change(&mut tx, source_id, "upsert", SyncActor::collector())
                .await?;
        }

        if !self
            .delete_schedule_on(
                &mut tx,
                duplicate_id,
                true,
                Some(duplicate_revision),
                SyncActor::collector(),
            )
            .await?
        {
            return Err(AppError::validation("日程在删除期间已被修改"));
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn get_schedule(&self, id: i64) -> AppResult<Schedule> {
        let row = sqlx::query_as::<_, ScheduleRow>(
            "SELECT s.id, s.streamer_id, st.name AS streamer_name, st.avatar_url AS streamer_avatar,
                    s.participants_json, s.title, s.start_at, s.end_at, s.room_url,
                    s.room_owner_mid, s.room_owner_name, s.source_kind, s.source_post_id,
                    s.source_image_url, s.source_author_mid, s.source_fingerprint, s.source_identity,
                    s.source_published_at, s.weekly_rule_id, s.weekly_occurrence_date, s.confidence, s.manual_locked, s.source_changed,
                    s.cancelled, s.created_at, s.updated_at, s.revision
             FROM schedules s JOIN streamers st ON st.id = s.streamer_id WHERE s.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found(format!("日程 {id}")))?;
        map_schedule(row)
    }

    pub async fn list_schedules(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        include_cancelled: bool,
    ) -> AppResult<Vec<Schedule>> {
        if to <= from {
            return Err(AppError::validation("结束时间必须晚于开始时间"));
        }
        let rows = sqlx::query_as::<_, ScheduleRow>(
            "SELECT s.id, s.streamer_id, st.name AS streamer_name, st.avatar_url AS streamer_avatar,
                    s.participants_json, s.title, s.start_at, s.end_at, s.room_url,
                    s.room_owner_mid, s.room_owner_name, s.source_kind, s.source_post_id,
                    s.source_image_url, s.source_author_mid, s.source_fingerprint, s.source_identity,
                    s.source_published_at, s.weekly_rule_id, s.weekly_occurrence_date, s.confidence, s.manual_locked, s.source_changed,
                    s.cancelled, s.created_at, s.updated_at, s.revision
             FROM schedules s JOIN streamers st ON st.id = s.streamer_id
             WHERE s.start_at >= ? AND s.start_at < ?
               AND (? = 1 OR s.cancelled = 0)
               AND (st.schedule_enabled = 1 OR s.source_kind = 'manual')
             ORDER BY s.start_at, s.id",
        )
        .bind(from.to_rfc3339())
        .bind(to.to_rfc3339())
        .bind(include_cancelled)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_schedule).collect()
    }

    pub async fn list_all_schedules(&self) -> AppResult<Vec<Schedule>> {
        let rows = sqlx::query_as::<_, ScheduleRow>(
            "SELECT s.id, s.streamer_id, st.name AS streamer_name, st.avatar_url AS streamer_avatar,
                    s.participants_json, s.title, s.start_at, s.end_at, s.room_url,
                    s.room_owner_mid, s.room_owner_name, s.source_kind, s.source_post_id,
                    s.source_image_url, s.source_author_mid, s.source_fingerprint, s.source_identity,
                    s.source_published_at, s.weekly_rule_id, s.weekly_occurrence_date, s.confidence, s.manual_locked, s.source_changed,
                    s.cancelled, s.created_at, s.updated_at, s.revision
             FROM schedules s JOIN streamers st ON st.id = s.streamer_id
             ORDER BY s.start_at, s.id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_schedule).collect()
    }
    async fn get_schedule_on(
        &self,
        conn: &mut sqlx::SqliteConnection,
        id: i64,
    ) -> AppResult<Schedule> {
        let row = sqlx::query_as::<_, ScheduleRow>(
            "SELECT s.id, s.streamer_id, st.name AS streamer_name, st.avatar_url AS streamer_avatar,
                    s.participants_json, s.title, s.start_at, s.end_at, s.room_url,
                    s.room_owner_mid, s.room_owner_name, s.source_kind, s.source_post_id,
                    s.source_image_url, s.source_author_mid, s.source_fingerprint, s.source_identity,
                    s.source_published_at, s.weekly_rule_id, s.weekly_occurrence_date, s.confidence,
                    s.manual_locked, s.source_changed, s.cancelled, s.created_at, s.updated_at,
                    s.revision
             FROM schedules s JOIN streamers st ON st.id = s.streamer_id WHERE s.id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| AppError::not_found(format!("日程 {id}")))?;
        map_schedule(row)
    }

    async fn weekly_rule_exists_on(
        &self,
        conn: &mut sqlx::SqliteConnection,
        rule_id: i64,
    ) -> AppResult<bool> {
        let id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM weekly_schedule_rules WHERE id = ?")
                .bind(rule_id)
                .fetch_optional(&mut *conn)
                .await?;
        Ok(id.is_some())
    }

    async fn clear_orphan_weekly_schedule_links_on(
        &self,
        conn: &mut sqlx::SqliteConnection,
    ) -> AppResult<()> {
        let schedule_ids: Vec<i64> = sqlx::query_scalar(
            "SELECT s.id FROM schedules s
             WHERE (s.weekly_rule_id IS NOT NULL
                    AND NOT EXISTS (
                        SELECT 1 FROM weekly_schedule_rules r WHERE r.id = s.weekly_rule_id
                    ))
                OR (s.weekly_rule_id IS NULL AND s.weekly_occurrence_date IS NOT NULL)
             ORDER BY s.id",
        )
        .fetch_all(&mut *conn)
        .await?;
        for schedule_id in schedule_ids {
            sqlx::query(
                "UPDATE schedules SET weekly_rule_id = NULL, weekly_occurrence_date = NULL,
                        updated_at = ?, revision = revision + 1 WHERE id = ?",
            )
            .bind(Utc::now().to_rfc3339())
            .bind(schedule_id)
            .execute(&mut *conn)
            .await?;
            self.emit_schedule_change(conn, schedule_id, "upsert", SyncActor::system())
                .await?;
        }
        Ok(())
    }

    pub async fn update_schedule(&self, id: i64, update: &UpdateSchedule) -> AppResult<Schedule> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = self.get_schedule_on(&mut tx, id).await?;
        let current_revision = current.revision;
        let weekly_scope = if let (Some(rule_id), Some(occurrence_date)) = (
            current.weekly_rule_id,
            current.weekly_occurrence_date.clone(),
        ) {
            if self.weekly_rule_exists_on(&mut tx, rule_id).await? {
                Some((rule_id, occurrence_date))
            } else {
                None
            }
        } else {
            None
        };
        let clear_orphan_weekly_link = match current.weekly_rule_id {
            Some(_) => weekly_scope.is_none(),
            None => current.weekly_occurrence_date.is_some(),
        };
        let title = update
            .title
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or(current.title);
        if title.chars().count() > 200 {
            return Err(AppError::validation("日程标题不能超过 200 字"));
        }
        let start_at = update.start_at.unwrap_or(current.start_at);
        let end_at = update.end_at.or(current.end_at);
        if let Some(end) = end_at {
            if end <= start_at {
                return Err(AppError::validation("结束时间必须晚于开始时间"));
            }
        }
        let room_url = update.room_url.clone().or(current.room_url);
        if let Some(url) = &room_url {
            validate_live_url(url)?;
        }
        if let Some((rule_id, occurrence_date)) = weekly_scope {
            self.save_weekly_exception_on(
                &mut tx,
                rule_id,
                &occurrence_date,
                "override",
                Some(id),
                SyncActor::system(),
            )
            .await?;
        }
        let updated = sqlx::query(
            "UPDATE schedules SET title = ?, start_at = ?, end_at = ?, room_url = ?,
                    room_owner_mid = NULL, room_owner_name = NULL,
                    weekly_rule_id = CASE WHEN ? THEN NULL ELSE weekly_rule_id END,
                    weekly_occurrence_date = CASE WHEN ? THEN NULL ELSE weekly_occurrence_date END,
                    manual_locked = 1, source_changed = 0, updated_at = ?, revision = revision + 1
             WHERE id = ? AND revision = ?",
        )
        .bind(title)
        .bind(start_at.to_rfc3339())
        .bind(end_at.map(|value| value.to_rfc3339()))
        .bind(room_url)
        .bind(clear_orphan_weekly_link)
        .bind(clear_orphan_weekly_link)
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .bind(current_revision)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::validation("日程在更新期间已被修改"));
        }
        self.emit_schedule_change(&mut tx, id, "upsert", SyncActor::system())
            .await?;
        tx.commit().await?;
        self.get_schedule(id).await
    }

    pub async fn cancel_schedule(&self, id: i64) -> AppResult<Schedule> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE schedules SET cancelled = 1, updated_at = ?, revision = revision + 1 WHERE id = ?",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::not_found(format!("日程 {id}")));
        }
        self.emit_schedule_change(&mut tx, id, "upsert", SyncActor::collector())
            .await?;
        self.clear_live_schedule_status_on(&mut tx, id, SyncActor::collector())
            .await?;
        tx.commit().await?;
        self.get_schedule(id).await
    }

    /// 老版本把 B 站直播预约卡的 `rid`（预约 id）当成房间号写进 `room_url`，
    /// 双击日程就会打开别人的直播间。预约日程的房主就是所属主播时，地址只
    /// 可能是该主播自己的直播间；对不上的清空，播放路径会回退到主播直播间。
    pub async fn repair_misattributed_reserve_room_urls(&self) -> AppResult<u64> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT s.id FROM schedules s
             JOIN streamers st ON st.id = s.streamer_id
             WHERE s.source_kind = 'reserve'
               AND s.room_url IS NOT NULL
               AND s.room_owner_mid IS NOT NULL
               AND s.room_owner_mid = st.mid
               AND COALESCE(st.live_url, '') <> s.room_url
               AND ('https://live.bilibili.com/' || COALESCE(st.room_id, '')) <> s.room_url",
        )
        .fetch_all(&self.pool)
        .await?;
        if ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut repaired: u64 = 0;
        for id in ids {
            let updated = sqlx::query(
                "UPDATE schedules SET room_url = NULL, updated_at = ?, revision = revision + 1
                 WHERE id = ? AND room_url IS NOT NULL",
            )
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                continue;
            }
            self.emit_schedule_change(&mut tx, id, "upsert", SyncActor::collector())
                .await?;
            repaired += 1;
        }
        tx.commit().await?;
        Ok(repaired)
    }

    pub async fn mark_schedule_source_changed(&self, id: i64) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE schedules SET source_changed = 1, updated_at = ?, revision = revision + 1
             WHERE id = ? AND source_changed = 0",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() > 0 {
            self.emit_schedule_change(&mut tx, id, "upsert", SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_weekly_rules(
        &self,
        streamer_id: Option<i64>,
    ) -> AppResult<Vec<WeeklyScheduleRule>> {
        let rows = if let Some(streamer_id) = streamer_id {
            sqlx::query_as::<_, WeeklyScheduleRuleRow>(
                "SELECT id, rule_key, streamer_id, title, weekdays_mask, start_time, end_time,
                        timezone, room_url, effective_from, effective_until, enabled,
                        created_at, updated_at, revision
                 FROM weekly_schedule_rules WHERE streamer_id = ? ORDER BY start_time, id",
            )
            .bind(streamer_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, WeeklyScheduleRuleRow>(
                "SELECT id, rule_key, streamer_id, title, weekdays_mask, start_time, end_time,
                        timezone, room_url, effective_from, effective_until, enabled,
                        created_at, updated_at, revision
                 FROM weekly_schedule_rules ORDER BY streamer_id, start_time, id",
            )
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter().map(map_weekly_rule).collect()
    }

    pub async fn get_weekly_rule(&self, id: i64) -> AppResult<WeeklyScheduleRule> {
        let row = sqlx::query_as::<_, WeeklyScheduleRuleRow>(
            "SELECT id, rule_key, streamer_id, title, weekdays_mask, start_time, end_time,
                    timezone, room_url, effective_from, effective_until, enabled,
                    created_at, updated_at, revision
             FROM weekly_schedule_rules WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found(format!("固定周表 {id}")))?;
        map_weekly_rule(row)
    }

    pub async fn get_weekly_rule_by_key(
        &self,
        rule_key: &str,
    ) -> AppResult<Option<WeeklyScheduleRule>> {
        let row = sqlx::query_as::<_, WeeklyScheduleRuleRow>(
            "SELECT id, rule_key, streamer_id, title, weekdays_mask, start_time, end_time,
                    timezone, room_url, effective_from, effective_until, enabled,
                    created_at, updated_at, revision
             FROM weekly_schedule_rules WHERE rule_key = ?",
        )
        .bind(rule_key)
        .fetch_optional(&self.pool)
        .await?;
        row.map(map_weekly_rule).transpose()
    }

    pub async fn add_weekly_rule(
        &self,
        input: &NewWeeklyScheduleRule,
    ) -> AppResult<WeeklyScheduleRule> {
        self.add_weekly_rule_idempotent(input)
            .await
            .map(|(rule, _)| rule)
    }

    pub async fn add_weekly_rule_idempotent(
        &self,
        input: &NewWeeklyScheduleRule,
    ) -> AppResult<(WeeklyScheduleRule, bool)> {
        crate::weekly::validate_rule_fields(
            &input.title,
            input.weekdays_mask,
            &input.start_time,
            input.end_time.as_deref(),
            &input.timezone,
            input.room_url.as_deref(),
            &input.effective_from,
            input.effective_until.as_deref(),
        )?;
        let rule_key = input
            .rule_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing =
            sqlx::query("SELECT id, streamer_id FROM weekly_schedule_rules WHERE rule_key = ?")
                .bind(&rule_key)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(row) = existing {
            let id: i64 = row.try_get("id")?;
            let existing_streamer_id: i64 = row.try_get("streamer_id")?;
            if existing_streamer_id != input.streamer_id {
                return Err(AppError::validation(format!(
                    "周表 ruleKey 已被主播 {existing_streamer_id} 使用"
                )));
            }
            tx.commit().await?;
            return Ok((self.get_weekly_rule(id).await?, false));
        }
        let id = sqlx::query(
            "INSERT INTO weekly_schedule_rules
             (rule_key, streamer_id, title, weekdays_mask, start_time, end_time, timezone,
              room_url, effective_from, effective_until, enabled, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&rule_key)
        .bind(input.streamer_id)
        .bind(input.title.trim())
        .bind(input.weekdays_mask)
        .bind(input.start_time.trim())
        .bind(input.end_time.as_deref().map(str::trim))
        .bind(&input.timezone)
        .bind(input.room_url.as_deref())
        .bind(&input.effective_from)
        .bind(input.effective_until.as_deref())
        .bind(input.enabled)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        self.emit_weekly_rule_change(&mut tx, id, "upsert", SyncActor::system())
            .await?;
        tx.commit().await?;
        Ok((self.get_weekly_rule(id).await?, true))
    }

    pub async fn update_weekly_rule(
        &self,
        id: i64,
        update: &UpdateWeeklyScheduleRule,
    ) -> AppResult<WeeklyScheduleRule> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current_row = sqlx::query_as::<_, WeeklyScheduleRuleRow>(
            "SELECT id, rule_key, streamer_id, title, weekdays_mask, start_time, end_time,
                    timezone, room_url, effective_from, effective_until, enabled,
                    created_at, updated_at, revision
             FROM weekly_schedule_rules WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::not_found(format!("固定周表 {id}")))?;
        let current = map_weekly_rule(current_row)?;
        let current_revision = current.revision;
        let title = update
            .title
            .clone()
            .unwrap_or_else(|| current.title.clone());
        let weekdays_mask = update.weekdays_mask.unwrap_or(current.weekdays_mask);
        let start_time = update
            .start_time
            .clone()
            .unwrap_or_else(|| current.start_time.clone());
        let end_time = if update.clear_end_time {
            None
        } else {
            update.end_time.clone().or_else(|| current.end_time.clone())
        };
        let room_url = if update.clear_room_url {
            None
        } else {
            update.room_url.clone().or_else(|| current.room_url.clone())
        };
        let effective_from = update
            .effective_from
            .clone()
            .unwrap_or_else(|| current.effective_from.clone());
        let effective_until = if update.clear_effective_until {
            None
        } else {
            update
                .effective_until
                .clone()
                .or_else(|| current.effective_until.clone())
        };
        crate::weekly::validate_rule_fields(
            &title,
            weekdays_mask,
            &start_time,
            end_time.as_deref(),
            &current.timezone,
            room_url.as_deref(),
            &effective_from,
            effective_until.as_deref(),
        )?;
        let changed = update.title.is_some()
            || update.weekdays_mask.is_some()
            || update.start_time.is_some()
            || update.end_time.is_some()
            || update.clear_end_time
            || update.room_url.is_some()
            || update.clear_room_url
            || update.effective_from.is_some()
            || update.effective_until.is_some()
            || update.clear_effective_until
            || update.enabled.is_some();
        if !changed {
            tx.commit().await?;
            return Ok(current);
        }
        let updated = sqlx::query(
            "UPDATE weekly_schedule_rules SET
                    title = COALESCE(?, title),
                    weekdays_mask = COALESCE(?, weekdays_mask),
                    start_time = COALESCE(?, start_time),
                    end_time = CASE WHEN ? THEN NULL ELSE COALESCE(?, end_time) END,
                    room_url = CASE WHEN ? THEN NULL ELSE COALESCE(?, room_url) END,
                    effective_from = COALESCE(?, effective_from),
                    effective_until = CASE WHEN ? THEN NULL ELSE COALESCE(?, effective_until) END,
                    enabled = COALESCE(?, enabled),
                    updated_at = ?, revision = revision + 1
             WHERE id = ? AND revision = ?",
        )
        .bind(update.title.as_deref().map(str::trim))
        .bind(update.weekdays_mask)
        .bind(update.start_time.as_deref().map(str::trim))
        .bind(update.clear_end_time)
        .bind(update.end_time.as_deref().map(str::trim))
        .bind(update.clear_room_url)
        .bind(update.room_url.as_deref())
        .bind(update.effective_from.as_deref())
        .bind(update.clear_effective_until)
        .bind(update.effective_until.as_deref())
        .bind(update.enabled)
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .bind(current_revision)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::validation("固定周表在更新期间已被修改"));
        }
        self.emit_weekly_rule_change(&mut tx, id, "upsert", SyncActor::system())
            .await?;
        tx.commit().await?;
        self.get_weekly_rule(id).await
    }

    pub async fn delete_weekly_rule(&self, id: i64) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.clear_orphan_weekly_schedule_links_on(&mut tx).await?;
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT id FROM weekly_schedule_rules WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return Err(AppError::not_found(format!("固定周表 {id}")));
        }
        let schedule_ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM schedules WHERE weekly_rule_id = ? AND start_at >= ? ORDER BY id",
        )
        .bind(id)
        .bind(Utc::now().to_rfc3339())
        .fetch_all(&mut *tx)
        .await?;
        for schedule_id in &schedule_ids {
            let source_ids: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM schedule_sources WHERE schedule_id = ? ORDER BY id",
            )
            .bind(schedule_id)
            .fetch_all(&mut *tx)
            .await?;
            for source_id in source_ids {
                self.emit_schedule_source_change(&mut tx, source_id, "delete", SyncActor::system())
                    .await?;
            }
            self.emit_schedule_change(&mut tx, *schedule_id, "delete", SyncActor::system())
                .await?;
        }
        let exception_ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM weekly_schedule_exceptions WHERE rule_id = ? ORDER BY id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        for exception_id in exception_ids {
            self.emit_weekly_exception_change(&mut tx, exception_id, "delete", SyncActor::system())
                .await?;
        }
        self.emit_weekly_rule_change(&mut tx, id, "delete", SyncActor::system())
            .await?;
        sqlx::query("DELETE FROM schedules WHERE weekly_rule_id = ? AND start_at >= ?")
            .bind(id)
            .bind(Utc::now().to_rfc3339())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM weekly_schedule_exceptions WHERE rule_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM weekly_schedule_rules WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        self.clear_orphan_weekly_schedule_links_on(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn weekly_exception_exists(
        &self,
        rule_id: i64,
        occurrence_date: &str,
    ) -> AppResult<bool> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM weekly_schedule_exceptions WHERE rule_id = ? AND occurrence_date = ?",
        )
        .bind(rule_id)
        .bind(occurrence_date)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn save_weekly_exception(
        &self,
        rule_id: i64,
        occurrence_date: &str,
        exception_kind: &str,
        schedule_id: Option<i64>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.save_weekly_exception_on(
            &mut tx,
            rule_id,
            occurrence_date,
            exception_kind,
            schedule_id,
            SyncActor::system(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_future_weekly_occurrences(
        &self,
        rule_id: i64,
        keep_overrides: bool,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let sql = if keep_overrides {
            "SELECT id FROM schedules WHERE weekly_rule_id = ? AND start_at >= ?
             AND id NOT IN (SELECT schedule_id FROM weekly_schedule_exceptions
                            WHERE rule_id = ? AND exception_kind = 'override' AND schedule_id IS NOT NULL)
             ORDER BY id"
        } else {
            "SELECT id FROM schedules WHERE weekly_rule_id = ? AND start_at >= ? ORDER BY id"
        };
        let now = Utc::now().to_rfc3339();
        let mut query = sqlx::query_scalar::<_, i64>(sql).bind(rule_id).bind(&now);
        if keep_overrides {
            query = query.bind(rule_id);
        }
        let schedule_ids = query.fetch_all(&mut *tx).await?;
        for schedule_id in schedule_ids {
            let source_ids: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM schedule_sources WHERE schedule_id = ? ORDER BY id",
            )
            .bind(schedule_id)
            .fetch_all(&mut *tx)
            .await?;
            for source_id in source_ids {
                self.emit_schedule_source_change(&mut tx, source_id, "delete", SyncActor::system())
                    .await?;
            }
            self.emit_schedule_change(&mut tx, schedule_id, "delete", SyncActor::system())
                .await?;
        }
        let delete_sql = if keep_overrides {
            "DELETE FROM schedules WHERE weekly_rule_id = ? AND start_at >= ?
             AND id NOT IN (SELECT schedule_id FROM weekly_schedule_exceptions
                            WHERE rule_id = ? AND exception_kind = 'override' AND schedule_id IS NOT NULL)"
        } else {
            "DELETE FROM schedules WHERE weekly_rule_id = ? AND start_at >= ?"
        };
        let mut delete = sqlx::query(delete_sql).bind(rule_id).bind(&now);
        if keep_overrides {
            delete = delete.bind(rule_id);
        }
        delete.execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn upsert_weekly_occurrence(
        &self,
        rule: &WeeklyScheduleRule,
        streamer: &Streamer,
        occurrence_date: &str,
        start_at: DateTime<Utc>,
        end_at: Option<DateTime<Utc>>,
    ) -> AppResult<i64> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM schedules WHERE weekly_rule_id = ? AND weekly_occurrence_date = ?",
        )
        .bind(rule.id)
        .bind(occurrence_date)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = existing {
            tx.commit().await?;
            return Ok(id);
        }
        let now = Utc::now().to_rfc3339();
        let source_identity = format!("weekly:{}:{occurrence_date}", rule.rule_key);
        let id = sqlx::query(
            "INSERT INTO schedules
             (streamer_id, participants_json, title, start_at, end_at, room_url,
              room_owner_mid, room_owner_name, source_kind, source_author_mid,
              source_fingerprint, source_identity, weekly_rule_id, weekly_occurrence_date,
              confidence, manual_locked, created_at, updated_at)
             VALUES (?, '[]', ?, ?, ?, ?, ?, ?, 'weekly', ?, ?, ?, ?, ?, 1.0, 1, ?, ?)",
        )
        .bind(streamer.id)
        .bind(&rule.title)
        .bind(start_at.to_rfc3339())
        .bind(end_at.map(|value| value.to_rfc3339()))
        .bind(rule.room_url.as_deref().or(streamer.live_url.as_deref()))
        .bind(&streamer.mid)
        .bind(&streamer.name)
        .bind(&streamer.mid)
        .bind(format!("weekly:{}", rule.rule_key))
        .bind(source_identity)
        .bind(rule.id)
        .bind(occurrence_date)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        self.emit_schedule_change(&mut tx, id, "upsert", SyncActor::collector())
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn delete_schedule_on(
        &self,
        tx: &mut sqlx::SqliteConnection,
        id: i64,
        require_unlocked: bool,
        expected_revision: Option<i64>,
        actor: SyncActor<'_>,
    ) -> AppResult<bool> {
        let state = sqlx::query(
            "SELECT streamer_id, source_kind, source_post_id, source_fingerprint,
                    weekly_rule_id, weekly_occurrence_date, manual_locked, revision
             FROM schedules WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::not_found(format!("日程 {id}")))?;
        let streamer_id: i64 = state.try_get("streamer_id")?;
        let source_kind: String = state.try_get("source_kind")?;
        let source_post_id: Option<String> = state.try_get("source_post_id")?;
        let source_fingerprint: Option<String> = state.try_get("source_fingerprint")?;
        let weekly_rule_id: Option<i64> = state.try_get("weekly_rule_id")?;
        let weekly_occurrence_date: Option<String> = state.try_get("weekly_occurrence_date")?;
        let manual_locked: i64 = state.try_get("manual_locked")?;
        let revision: i64 = state.try_get("revision")?;
        if (require_unlocked && manual_locked != 0)
            || expected_revision.is_some_and(|expected| expected != revision)
        {
            return Ok(false);
        }

        if let (Some(rule_id), Some(occurrence_date)) =
            (weekly_rule_id, weekly_occurrence_date.as_deref())
        {
            if self.weekly_rule_exists_on(tx, rule_id).await? {
                self.save_weekly_exception_on(
                    tx,
                    rule_id,
                    occurrence_date,
                    "skip",
                    None,
                    SyncActor::system(),
                )
                .await?;
            }
        }
        self.clear_live_schedule_status_on(tx, id, actor).await?;
        if source_kind != "manual" {
            let mut suppressions = Vec::new();
            if let (Some(post_id), Some(fingerprint)) =
                (source_post_id.as_deref(), source_fingerprint.as_deref())
            {
                suppressions.push((post_id.to_owned(), fingerprint.to_owned()));
            }
            let source_rows = sqlx::query(
                "SELECT source_post_id, source_fingerprint FROM schedule_sources
                 WHERE schedule_id = ? AND source_post_id IS NOT NULL
                   AND source_fingerprint IS NOT NULL",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            for row in source_rows {
                suppressions.push((
                    row.try_get("source_post_id")?,
                    row.try_get("source_fingerprint")?,
                ));
            }
            suppressions.sort();
            suppressions.dedup();
            for (post_id, fingerprint) in suppressions {
                self.add_source_suppression_on(
                    tx,
                    streamer_id,
                    &post_id,
                    &fingerprint,
                    SyncActor::collector(),
                )
                .await?;
            }
        }
        let source_ids: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM schedule_sources WHERE schedule_id = ? ORDER BY id")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        for source_id in source_ids {
            self.emit_schedule_source_change(tx, source_id, "delete", SyncActor::system())
                .await?;
        }
        self.emit_schedule_change(tx, id, "delete", actor).await?;
        let deleted = match (require_unlocked, expected_revision) {
            (true, Some(expected_revision)) => {
                sqlx::query(
                    "DELETE FROM schedules
                     WHERE id = ? AND manual_locked = 0 AND revision = ?",
                )
                .bind(id)
                .bind(expected_revision)
                .execute(&mut *tx)
                .await?
            }
            (true, None) => {
                sqlx::query("DELETE FROM schedules WHERE id = ? AND manual_locked = 0")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
            }
            (false, Some(expected_revision)) => {
                sqlx::query("DELETE FROM schedules WHERE id = ? AND revision = ?")
                    .bind(id)
                    .bind(expected_revision)
                    .execute(&mut *tx)
                    .await?
            }
            (false, None) => {
                sqlx::query("DELETE FROM schedules WHERE id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
            }
        };
        Ok(deleted.rows_affected() == 1)
    }

    pub async fn delete_schedule(&self, id: i64) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !self
            .delete_schedule_on(&mut tx, id, false, None, SyncActor::system())
            .await?
        {
            return Err(AppError::not_found(format!("日程 {id}")));
        }
        tx.commit().await?;
        Ok(())
    }

    /// 删除已经没有任何 `schedule_sources` 的孤儿场次。
    /// 手动锁定、仍有来源、或已被删掉的场次都跳过；返回真正删掉的行 id。
    pub async fn delete_orphan_schedules(&self, candidates: &[i64]) -> AppResult<Vec<i64>> {
        let mut deleted = Vec::new();
        for id in candidates {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            let manual_locked: Option<i64> =
                sqlx::query_scalar("SELECT manual_locked FROM schedules WHERE id = ?")
                    .bind(*id)
                    .fetch_optional(&mut *tx)
                    .await?;
            let Some(manual_locked) = manual_locked else {
                continue;
            };
            if manual_locked != 0 {
                continue;
            }
            let remaining: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM schedule_sources WHERE schedule_id = ?")
                    .bind(*id)
                    .fetch_one(&mut *tx)
                    .await?;
            if remaining > 0 {
                continue;
            }
            let removed = self
                .delete_schedule_on(&mut tx, *id, true, None, SyncActor::system())
                .await?;
            tx.commit().await?;
            if removed {
                deleted.push(*id);
            }
        }
        Ok(deleted)
    }

    /// 删除该动态下 identity 不在 `keep` 里的 `schedule_sources`，返回 (被删 identity, 所属 schedule_id)。
    /// 只删 source 行，不删 schedule 行；孤儿场次由 service 层调用 `delete_schedule` 处理。
    pub async fn delete_schedule_sources_except(
        &self,
        post_id: &str,
        keep: &[String],
    ) -> AppResult<Vec<(String, i64)>> {
        let keep_set: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(
            "SELECT id, source_identity, schedule_id FROM schedule_sources
             WHERE source_post_id = ?
             ORDER BY id",
        )
        .bind(post_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut removed = Vec::new();
        for row in rows {
            let source_id: i64 = row.try_get("id")?;
            let identity: String = row.try_get("source_identity")?;
            if keep_set.contains(identity.as_str()) {
                continue;
            }
            let schedule_id: i64 = row.try_get("schedule_id")?;
            self.emit_schedule_source_change(&mut tx, source_id, "delete", SyncActor::system())
                .await?;
            sqlx::query("DELETE FROM schedule_sources WHERE id = ?")
                .bind(source_id)
                .execute(&mut *tx)
                .await?;
            removed.push((identity, schedule_id));
        }
        tx.commit().await?;
        Ok(removed)
    }

    /// 只清理「当前分析里已经不存在的来源」：来源编号尾号是事件序号。
    ///
    /// - 尾号 `>= event_count`：这一版结果里没有第 N 条事件了（旧提示词、
    ///   旧的逐图缓存、被合并掉的重复场次留下的编号），清掉。
    /// - 尾号在范围内、且 `replaced` 里这个序号已经换了新编号：同一条事件
    ///   被新一轮结果重新覆盖了（摘要变了），旧编号属于同一场，清掉不会丢
    ///   日程——新编号已经挂在这条日程上。
    /// - 其它一律保留。摘要变了但这一轮没重新写到的，不许删：那正是同一场
    ///   直播时有时无的根源（删了来源会连带触发 `delete_orphan_schedules`）。
    ///
    /// 与 [`Self::delete_schedule_sources_except`] 的区别：那个按「本轮写没写」
    /// 判断，摘要一变就把上一轮写入的来源全删掉；这个按事件序号判断。
    ///
    /// `keep` 里的编号无论尾号如何都保留；`event_count == 0`（一条事件都
    /// 没识别出来）时一条都不动——模型偶尔看走眼，不能因此删掉还能用的场次。
    pub async fn prune_schedule_sources_beyond(
        &self,
        post_id: &str,
        event_count: usize,
        keep: &[String],
        replaced: &std::collections::HashMap<usize, String>,
    ) -> AppResult<Vec<(String, i64)>> {
        if event_count == 0 {
            return Ok(Vec::new());
        }
        let keep_set: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // 只看这条动态的 text/image 来源：预约卡来源（`reserve:{post}`）尾号是
        // 动态 id，按序号解析会把它误判成过期。
        let rows = sqlx::query(
            "SELECT id, source_identity, schedule_id FROM schedule_sources
             WHERE source_post_id = ? AND source_kind IN ('text', 'image')
             ORDER BY id",
        )
        .bind(post_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut removed = Vec::new();
        for row in rows {
            let source_id: i64 = row.try_get("id")?;
            let identity: String = row.try_get("source_identity")?;
            if keep_set.contains(identity.as_str()) {
                continue;
            }
            let Some(event_index) = identity
                .rsplit(':')
                .next()
                .and_then(|tail| tail.parse::<usize>().ok())
            else {
                continue;
            };
            let stale = event_index >= event_count
                || replaced
                    .get(&event_index)
                    .is_some_and(|fresh| fresh != &identity);
            if !stale {
                continue;
            }
            let schedule_id: i64 = row.try_get("schedule_id")?;
            self.emit_schedule_source_change(&mut tx, source_id, "delete", SyncActor::system())
                .await?;
            sqlx::query("DELETE FROM schedule_sources WHERE id = ?")
                .bind(source_id)
                .execute(&mut *tx)
                .await?;
            removed.push((identity, schedule_id));
        }
        tx.commit().await?;
        Ok(removed)
    }

    pub async fn add_source_suppression(
        &self,
        streamer_id: i64,
        source_post_id: &str,
        source_fingerprint: &str,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.add_source_suppression_on(
            &mut tx,
            streamer_id,
            source_post_id,
            source_fingerprint,
            SyncActor::collector(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn is_source_suppressed(
        &self,
        streamer_id: i64,
        source_post_id: &str,
        source_fingerprint: &str,
    ) -> AppResult<bool> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM source_suppressions
             WHERE streamer_id = ? AND source_post_id = ? AND source_fingerprint = ?",
        )
        .bind(streamer_id)
        .bind(source_post_id)
        .bind(source_fingerprint)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn list_source_suppressions(&self) -> AppResult<Vec<SourceSuppression>> {
        let rows = sqlx::query(
            "SELECT id, streamer_id, source_post_id, source_fingerprint, revision, created_at
             FROM source_suppressions ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(SourceSuppression {
                    id: row.try_get("id")?,
                    streamer_id: row.try_get("streamer_id")?,
                    source_post_id: row.try_get("source_post_id")?,
                    source_fingerprint: row.try_get("source_fingerprint")?,
                    revision: row.try_get("revision")?,
                    created_at: parse_ts(row.try_get::<String, _>("created_at")?.as_str())?,
                })
            })
            .collect()
    }

    pub async fn set_streamer_room(&self, streamer_id: i64, room_id: &str) -> AppResult<()> {
        if room_id.trim().is_empty() || !room_id.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(AppError::validation("直播间号必须是数字"));
        }
        let live_url = live_url_for_room(room_id);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE streamers SET room_id = ?, live_url = ?, updated_at = ?,
                    revision = revision + 1 WHERE id = ?",
        )
        .bind(room_id)
        .bind(live_url)
        .bind(Utc::now().to_rfc3339())
        .bind(streamer_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::not_found(format!("主播 {streamer_id}")));
        }
        self.emit_streamer_change(&mut tx, streamer_id, "upsert", SyncActor::collector())
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_live_status(
        &self,
        streamer_id: i64,
        snapshot: &LiveSnapshot,
    ) -> AppResult<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous = sqlx::query(
            "SELECT live_is_live, live_title, live_cover, live_url, live_started_at, live_checked_at, live_schedule_id
             FROM streamers WHERE id = ?",
        )
        .bind(streamer_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::not_found(format!("主播 {streamer_id}")))?;
        let previous_is_live: i64 = previous.try_get("live_is_live")?;
        let previous_title: Option<String> = previous.try_get("live_title")?;
        let previous_cover: Option<String> = previous.try_get("live_cover")?;
        let previous_live_url: Option<String> = previous.try_get("live_url")?;
        let previous_started_at: Option<String> = previous.try_get("live_started_at")?;
        let previous_checked: Option<String> = previous.try_get("live_checked_at")?;
        if parse_opt_ts(previous_checked.as_deref())?.is_some_and(|time| time > snapshot.checked_at)
        {
            return Ok(previous_is_live != 0);
        }
        let broadcast = self
            .observe_live_broadcast_on(&mut tx, streamer_id, snapshot)
            .await?;
        let bound_id = broadcast
            .as_ref()
            .and_then(|broadcast| broadcast.schedule_id);
        let schedule_id: Option<i64> = if let Some(id) = bound_id {
            sqlx::query_scalar("SELECT id FROM schedules WHERE id=? AND cancelled=0")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
        } else {
            None
        };
        let previous_schedule_id: Option<i64> = previous.try_get("live_schedule_id")?;
        let snapshot_started_at = snapshot.started_at.map(|value| value.to_rfc3339());
        let changed = previous_is_live != (snapshot.is_live as i64)
            || previous_title.as_deref().unwrap_or_default() != snapshot.title
            || previous_cover.as_deref() != snapshot.cover_url.as_deref()
            || snapshot
                .live_url
                .as_deref()
                .is_some_and(|url| previous_live_url.as_deref() != Some(url))
            || previous_started_at.as_deref() != snapshot_started_at.as_deref()
            || previous_schedule_id != schedule_id;
        sqlx::query(
            "UPDATE streamers SET live_is_live = ?, live_title = ?, live_cover = ?,
                    live_url = COALESCE(?, live_url), live_started_at = ?, live_checked_at = ?, live_schedule_id = ?,
                    updated_at = ?, live_revision = live_revision + ? WHERE id = ?",
        )
        .bind(snapshot.is_live)
        .bind(&snapshot.title)
        .bind(&snapshot.cover_url)
        .bind(&snapshot.live_url)
        .bind(snapshot_started_at)
        .bind(snapshot.checked_at.to_rfc3339())
        .bind(schedule_id)
        .bind(Utc::now().to_rfc3339())
        .bind(if changed { 1_i64 } else { 0_i64 })
        .bind(streamer_id)
        .execute(&mut *tx)
        .await?;
        if changed {
            self.emit_live_status_change(&mut tx, streamer_id, SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        Ok(previous_is_live != 0)
    }

    pub async fn record_streamer_error(&self, streamer_id: i64, error: &str) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = sqlx::query("SELECT last_error FROM streamers WHERE id = ?")
            .bind(streamer_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::not_found(format!("主播 {streamer_id}")))?;
        let previous_error: Option<String> = current.try_get("last_error")?;
        if previous_error.as_deref() != Some(error) {
            sqlx::query(
                "UPDATE streamers SET last_error = ?, updated_at = ?,
                        revision = revision + 1 WHERE id = ?",
            )
            .bind(error)
            .bind(Utc::now().to_rfc3339())
            .bind(streamer_id)
            .execute(&mut *tx)
            .await?;
            self.emit_streamer_change(&mut tx, streamer_id, "upsert", SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_refresh_result(
        &self,
        streamer_id: i64,
        error: Option<&str>,
    ) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM streamers WHERE id = ?")
            .bind(streamer_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::not_found(format!("主播 {streamer_id}")));
        }
        sqlx::query(
            "UPDATE streamers SET last_refresh_at = ?, last_error = ?, updated_at = ?,
                    revision = revision + 1 WHERE id = ?",
        )
        .bind(&now)
        .bind(error)
        .bind(&now)
        .bind(streamer_id)
        .execute(&mut *tx)
        .await?;
        self.emit_streamer_change(&mut tx, streamer_id, "upsert", SyncActor::collector())
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_refresh_result_preserving_error(&self, streamer_id: i64) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM streamers WHERE id = ?")
            .bind(streamer_id)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AppError::not_found(format!("主播 {streamer_id}")));
        }
        sqlx::query(
            "UPDATE streamers SET last_refresh_at = ?, updated_at = ?,
                    last_error = CASE WHEN last_error LIKE '动态文字分析失败:%'
                        OR last_error LIKE '图片日程分析失败:%' THEN last_error ELSE NULL END,
                    revision = revision + 1 WHERE id = ?",
        )
        .bind(&now)
        .bind(&now)
        .bind(streamer_id)
        .execute(&mut *tx)
        .await?;
        self.emit_streamer_change(&mut tx, streamer_id, "upsert", SyncActor::collector())
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_live_statuses(&self) -> AppResult<Vec<LiveStatus>> {
        let rows = sqlx::query_as::<_, LiveStatusRow>(
            "SELECT id AS streamer_id, mid, name AS streamer_name, avatar_url, live_is_live,
                    live_title, live_cover, live_url, live_checked_at, live_started_at, live_schedule_id,
                    live_notify_enabled, live_revision AS revision
             FROM streamers ORDER BY name COLLATE NOCASE",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(LiveStatus {
                    schedule_id: row.live_schedule_id,
                    streamer_id: row.streamer_id,
                    mid: row.mid,
                    streamer_name: row.streamer_name,
                    avatar_url: row.avatar_url,
                    is_live: row.live_is_live != 0,
                    title: row.live_title.unwrap_or_default(),
                    cover_url: row.live_cover,
                    live_url: row.live_url,
                    online: 0,
                    started_at: parse_opt_ts(row.live_started_at.as_deref())?,
                    checked_at: parse_opt_ts(row.live_checked_at.as_deref())?
                        .unwrap_or_else(Utc::now),
                    live_notify_enabled: row.live_notify_enabled != 0,
                    revision: row.revision,
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_live_session(
        &self,
        streamer_id: i64,
        room_id: &str,
        streamer_name: &str,
        title: &str,
        cover_url: Option<&str>,
        started_at: DateTime<Utc>,
        checked_at: DateTime<Utc>,
    ) -> AppResult<LiveSession> {
        let started = started_at.to_rfc3339();
        let now = checked_at.to_rfc3339();
        let room_id = room_id.trim();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing = sqlx::query(
            "SELECT id, streamer_name, room_id, title, cover_url, collector_state, ended_at
             FROM live_sessions WHERE streamer_id = ? AND started_at = ?",
        )
        .bind(streamer_id)
        .bind(&started)
        .fetch_optional(&mut *tx)
        .await?;
        // 同一条 (streamer_id, started_at) 只可能是同一场直播：B 站 live_time 没变，说明主播
        // 只是断了一下又推流回来。这时必须续上原来的计数器，而不是清零重来。
        let (id, changed) = if let Some(row) = existing {
            let id: i64 = row.try_get("id")?;
            let existing_room_id: String = row.try_get("room_id")?;
            let existing_streamer_name: String = row.try_get("streamer_name")?;
            let existing_title: String = row.try_get("title")?;
            let existing_cover: Option<String> = row.try_get("cover_url")?;
            let existing_state: String = row.try_get("collector_state")?;
            let existing_ended_at: Option<String> = row.try_get("ended_at")?;
            let resumed = existing_ended_at.is_some() || existing_state != "active";
            let changed = resumed
                || existing_room_id != room_id
                || existing_streamer_name != streamer_name
                || existing_title != title
                || (cover_url.is_some() && existing_cover.as_deref() != cover_url);
            if resumed {
                self.reopen_live_session_on(
                    &mut tx,
                    id,
                    streamer_name,
                    room_id,
                    title,
                    cover_url,
                    &started,
                    &now,
                )
                .await?;
            } else if changed {
                self.touch_live_session_metadata_on(
                    &mut tx,
                    id,
                    streamer_name,
                    room_id,
                    title,
                    cover_url,
                    &now,
                )
                .await?;
            }
            (id, changed)
        } else {
            // B 站把断流后的重开当成了新一场（live_time 变了），所以这里没有同键记录。
            // 先看这次重开是否已经并进来过，再看能不能并回上一段。
            let absorbed = sqlx::query(
                "SELECT id, ended_at
                 FROM live_sessions
                 WHERE streamer_id = ? AND room_id = ? AND last_segment_started_at = ?
                 ORDER BY started_at DESC, id DESC LIMIT 1",
            )
            .bind(streamer_id)
            .bind(room_id)
            .bind(&started)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(row) = absorbed {
                let id: i64 = row.try_get("id")?;
                let ended_at: Option<String> = row.try_get("ended_at")?;
                if ended_at.is_some() {
                    self.reopen_live_session_on(
                        &mut tx,
                        id,
                        streamer_name,
                        room_id,
                        title,
                        cover_url,
                        &started,
                        &now,
                    )
                    .await?;
                    (id, true)
                } else {
                    // 轮询会拿同一个 live_time 反复调用，这里不能每轮都再并一次。
                    (id, false)
                }
            } else {
                let merge_floor = (started_at
                    - chrono::Duration::minutes(LIVE_SESSION_MERGE_MINUTES))
                .to_rfc3339();
                let resumable = sqlx::query(
                    "SELECT id FROM live_sessions
                     WHERE streamer_id = ? AND room_id = ? AND started_at < ?
                       AND (ended_at >= ? OR (ended_at IS NULL AND updated_at >= ?))
                     ORDER BY started_at DESC, id DESC LIMIT 1",
                )
                .bind(streamer_id)
                .bind(room_id)
                .bind(&started)
                .bind(&merge_floor)
                .bind(&merge_floor)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(row) = resumable {
                    let id: i64 = row.try_get("id")?;
                    self.reopen_live_session_on(
                        &mut tx,
                        id,
                        streamer_name,
                        room_id,
                        title,
                        cover_url,
                        &started,
                        &now,
                    )
                    .await?;
                    (id, true)
                } else {
                    let id = sqlx::query(
                        "INSERT INTO live_sessions
                         (streamer_id, streamer_name, room_id, title, cover_url, started_at,
                          data_quality, collector_state, segment_count, last_segment_started_at,
                          created_at, updated_at)
                         VALUES (?, ?, ?, ?, ?, ?, 'collecting', 'active', 1, ?, ?, ?)",
                    )
                    .bind(streamer_id)
                    .bind(streamer_name)
                    .bind(room_id.trim())
                    .bind(title)
                    .bind(cover_url)
                    .bind(&started)
                    .bind(&started)
                    .bind(&now)
                    .bind(&now)
                    .execute(&mut *tx)
                    .await?
                    .last_insert_rowid();
                    (id, true)
                }
            }
        };
        if changed {
            self.emit_live_session_change(&mut tx, id, "upsert", SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        self.get_live_session(id).await
    }

    /// 把一条已结束（或非 active）的场次重新打开：保留计数器与整场的开始时间，只清掉结束时间。
    /// 断流窗口内的事件确实丢了，所以完整度降级为 partial，段数 +1。
    #[allow(clippy::too_many_arguments)]
    async fn reopen_live_session_on(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        streamer_name: &str,
        room_id: &str,
        title: &str,
        cover_url: Option<&str>,
        segment_started: &str,
        now: &str,
    ) -> AppResult<()> {
        sqlx::query(
            "UPDATE live_sessions
             SET streamer_name = ?, room_id = ?, title = ?,
                 cover_url = COALESCE(?, cover_url), ended_at = NULL, duration_seconds = 0,
                 collector_state = 'active',
                 data_quality = CASE WHEN data_quality = 'collecting' THEN 'collecting' ELSE 'partial' END,
                 segment_count = MAX(segment_count + 1, 2),
                 last_segment_started_at = ?, updated_at = ?, revision = revision + 1
             WHERE id = ?",
        )
        .bind(streamer_name)
        .bind(room_id)
        .bind(title)
        .bind(cover_url)
        .bind(segment_started)
        .bind(now)
        .bind(id)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn touch_live_session_metadata_on(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        streamer_name: &str,
        room_id: &str,
        title: &str,
        cover_url: Option<&str>,
        now: &str,
    ) -> AppResult<()> {
        sqlx::query(
            "UPDATE live_sessions
             SET streamer_name = ?, room_id = ?, title = ?,
                 cover_url = COALESCE(?, cover_url), collector_state = 'active',
                 updated_at = ?, revision = revision + 1
             WHERE id = ?",
        )
        .bind(streamer_name)
        .bind(room_id)
        .bind(title)
        .bind(cover_url)
        .bind(now)
        .bind(id)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn get_live_session(&self, id: i64) -> AppResult<LiveSession> {
        let row = sqlx::query_as::<_, LiveSessionRow>(
            "SELECT id, schedule_id, streamer_id, streamer_name, room_id, title, cover_url, started_at, ended_at,
                    duration_seconds, peak_online, peak_high_energy, avg_online, online_samples, online_total,
                    revenue_cents, gift_revenue_cents, super_chat_revenue_cents, guard_revenue_cents,
                    paid_user_count, interactive_user_count, danmaku_count, gift_count,
                    super_chat_count, guard_count, reconnect_count, segment_count, data_quality, collector_state,
                    created_at, updated_at, revision
             FROM live_sessions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found(format!("直播数据 {id}")))?;
        map_live_session(row)
    }

    pub async fn list_live_sessions(
        &self,
        streamer_id: Option<i64>,
        limit: i64,
    ) -> AppResult<Vec<LiveSession>> {
        let limit = limit.clamp(1, 200);
        let rows = if let Some(streamer_id) = streamer_id {
            sqlx::query_as::<_, LiveSessionRow>(
                "SELECT id, schedule_id, streamer_id, streamer_name, room_id, title, cover_url, started_at, ended_at,
                        duration_seconds, peak_online, peak_high_energy, avg_online, online_samples, online_total,
                        revenue_cents, gift_revenue_cents, super_chat_revenue_cents, guard_revenue_cents,
                        paid_user_count, interactive_user_count, danmaku_count, gift_count,
                        super_chat_count, guard_count, reconnect_count, segment_count, data_quality, collector_state,
                        created_at, updated_at, revision
                 FROM live_sessions WHERE streamer_id = ?
                 ORDER BY started_at DESC LIMIT ?",
            )
            .bind(streamer_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, LiveSessionRow>(
                "SELECT id, schedule_id, streamer_id, streamer_name, room_id, title, cover_url, started_at, ended_at,
                        duration_seconds, peak_online, peak_high_energy, avg_online, online_samples, online_total,
                        revenue_cents, gift_revenue_cents, super_chat_revenue_cents, guard_revenue_cents,
                        paid_user_count, interactive_user_count, danmaku_count, gift_count,
                        super_chat_count, guard_count, reconnect_count, segment_count, data_quality, collector_state,
                        created_at, updated_at, revision
                 FROM live_sessions ORDER BY started_at DESC LIMIT ?",
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter().map(map_live_session).collect()
    }

    pub async fn find_live_session_for_schedule(
        &self,
        schedule_id: i64,
    ) -> AppResult<Option<LiveSession>> {
        let row = sqlx::query_as::<_, LiveSessionRow>(
            "SELECT ls.id, ls.schedule_id, ls.streamer_id, ls.streamer_name, ls.room_id, ls.title, ls.cover_url,
                    ls.started_at, ls.ended_at, ls.duration_seconds, ls.peak_online, ls.peak_high_energy, ls.avg_online,
                    ls.online_samples, ls.online_total, ls.revenue_cents, ls.gift_revenue_cents,
                    ls.super_chat_revenue_cents, ls.guard_revenue_cents, ls.paid_user_count,
                    ls.interactive_user_count, ls.danmaku_count, ls.gift_count, ls.super_chat_count,
                    ls.guard_count, ls.reconnect_count, ls.segment_count, ls.data_quality, ls.collector_state,
                    ls.created_at, ls.updated_at, ls.revision
             FROM live_sessions ls
             JOIN schedules s ON s.streamer_id = ls.streamer_id
             WHERE s.id = ?
               AND (ls.schedule_id = s.id OR (
                    ls.schedule_id IS NULL
                    AND NOT EXISTS (SELECT 1 FROM live_broadcasts b
                        WHERE b.streamer_id=ls.streamer_id AND b.started_at=ls.started_at)
                    AND ABS(unixepoch(ls.started_at) - unixepoch(s.start_at)) <= 7200))
             ORDER BY (ls.schedule_id = s.id) DESC,
                      ABS(unixepoch(ls.started_at) - unixepoch(s.start_at)) ASC,
                      ls.started_at DESC
             LIMIT 1",
        )
        .bind(schedule_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(map_live_session).transpose()
    }

    pub async fn update_live_session_stats(
        &self,
        id: i64,
        stats: &LiveSessionStats,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE live_sessions
             SET peak_high_energy = ?, revenue_cents = ?, gift_revenue_cents = ?, super_chat_revenue_cents = ?,
                 guard_revenue_cents = ?, paid_user_count = ?, interactive_user_count = ?,
                 danmaku_count = ?, gift_count = ?, super_chat_count = ?, guard_count = ?,
                 reconnect_count = ?, data_quality = ?, collector_state = ?, updated_at = ?,
                 revision = revision + 1
             WHERE id = ? AND ended_at IS NULL
               AND (peak_high_energy IS NOT ? OR revenue_cents IS NOT ?
                    OR gift_revenue_cents IS NOT ? OR super_chat_revenue_cents IS NOT ?
                    OR guard_revenue_cents IS NOT ? OR paid_user_count IS NOT ?
                    OR interactive_user_count IS NOT ? OR danmaku_count IS NOT ?
                    OR gift_count IS NOT ? OR super_chat_count IS NOT ?
                    OR guard_count IS NOT ? OR reconnect_count IS NOT ?
                    OR data_quality IS NOT ? OR collector_state IS NOT ?)",
        )
        .bind(stats.peak_high_energy)
        .bind(stats.revenue_cents)
        .bind(stats.gift_revenue_cents)
        .bind(stats.super_chat_revenue_cents)
        .bind(stats.guard_revenue_cents)
        .bind(stats.paid_user_count)
        .bind(stats.interactive_user_count)
        .bind(stats.danmaku_count)
        .bind(stats.gift_count)
        .bind(stats.super_chat_count)
        .bind(stats.guard_count)
        .bind(stats.reconnect_count)
        .bind(&stats.data_quality)
        .bind(&stats.collector_state)
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .bind(stats.peak_high_energy)
        .bind(stats.revenue_cents)
        .bind(stats.gift_revenue_cents)
        .bind(stats.super_chat_revenue_cents)
        .bind(stats.guard_revenue_cents)
        .bind(stats.paid_user_count)
        .bind(stats.interactive_user_count)
        .bind(stats.danmaku_count)
        .bind(stats.gift_count)
        .bind(stats.super_chat_count)
        .bind(stats.guard_count)
        .bind(stats.reconnect_count)
        .bind(&stats.data_quality)
        .bind(&stats.collector_state)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() > 0 {
            self.emit_live_session_change(&mut tx, id, "upsert", SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn finish_live_session(
        &self,
        id: i64,
        ended_at: DateTime<Utc>,
        stats: &LiveSessionStats,
    ) -> AppResult<()> {
        let ended = ended_at.to_rfc3339();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE live_sessions
             SET ended_at = ?,
                 duration_seconds = CAST(MAX(0.0, (julianday(?) - julianday(started_at)) * 86400.0) AS INTEGER),
                 peak_high_energy = ?, revenue_cents = ?, gift_revenue_cents = ?, super_chat_revenue_cents = ?,
                 guard_revenue_cents = ?, paid_user_count = ?, interactive_user_count = ?,
                 danmaku_count = ?, gift_count = ?, super_chat_count = ?, guard_count = ?,
                 reconnect_count = ?, data_quality = ?, collector_state = 'finished', updated_at = ?,
                 revision = revision + 1
             WHERE id = ? AND ended_at IS NULL",
        )
        .bind(&ended)
        .bind(&ended)
        .bind(stats.peak_high_energy)
        .bind(stats.revenue_cents)
        .bind(stats.gift_revenue_cents)
        .bind(stats.super_chat_revenue_cents)
        .bind(stats.guard_revenue_cents)
        .bind(stats.paid_user_count)
        .bind(stats.interactive_user_count)
        .bind(stats.danmaku_count)
        .bind(stats.gift_count)
        .bind(stats.super_chat_count)
        .bind(stats.guard_count)
        .bind(stats.reconnect_count)
        .bind(&stats.data_quality)
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() > 0 {
            self.emit_live_session_change(&mut tx, id, "upsert", SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn latest_open_live_session(
        &self,
        streamer_id: i64,
    ) -> AppResult<Option<LiveSession>> {
        let row = sqlx::query_as::<_, LiveSessionRow>(
            "SELECT id, schedule_id, streamer_id, streamer_name, room_id, title, cover_url, started_at, ended_at,
                    duration_seconds, peak_online, peak_high_energy, avg_online, online_samples, online_total,
                    revenue_cents, gift_revenue_cents, super_chat_revenue_cents, guard_revenue_cents,
                    paid_user_count, interactive_user_count, danmaku_count, gift_count,
                    super_chat_count, guard_count, reconnect_count, segment_count, data_quality, collector_state,
                    created_at, updated_at, revision
             FROM live_sessions
             WHERE streamer_id = ? AND ended_at IS NULL
             ORDER BY started_at DESC LIMIT 1",
        )
        .bind(streamer_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(map_live_session).transpose()
    }

    pub async fn finish_open_live_session(
        &self,
        id: i64,
        ended_at: DateTime<Utc>,
    ) -> AppResult<()> {
        let ended = ended_at.to_rfc3339();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE live_sessions
             SET ended_at = ?,
                 duration_seconds = CAST(MAX(0.0, (julianday(?) - julianday(started_at)) * 86400.0) AS INTEGER),
                 data_quality = CASE WHEN data_quality = 'collecting' THEN 'partial' ELSE data_quality END,
                 collector_state = 'finished', updated_at = ?, revision = revision + 1
             WHERE id = ? AND ended_at IS NULL",
        )
        .bind(&ended)
        .bind(&ended)
        .bind(&ended)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() > 0 {
            self.emit_live_session_change(&mut tx, id, "upsert", SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn append_event(&self, event_type: &str, payload: &Value) -> AppResult<i64> {
        let result = sqlx::query(
            "INSERT INTO event_journal (type, payload_json, created_at) VALUES (?, ?, ?)",
        )
        .bind(event_type)
        .bind(serde_json::to_string(payload)?)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    pub async fn latest_event_seq(&self) -> AppResult<i64> {
        Ok(
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM event_journal")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn events_after(&self, after: i64, limit: i64) -> AppResult<Vec<Value>> {
        let limit = limit.clamp(1, 500);
        let rows = sqlx::query(
            "SELECT seq, type, payload_json, created_at FROM event_journal
             WHERE seq > ? ORDER BY seq LIMIT ?",
        )
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                let payload: Value =
                    serde_json::from_str(row.try_get::<String, _>("payload_json")?.as_str())?;
                Ok(json!({
                    "seq": row.try_get::<i64, _>("seq")?,
                    "type": row.try_get::<String, _>("type")?,
                    "createdAt": row.try_get::<String, _>("created_at")?,
                    "payload": payload
                }))
            })
            .collect()
    }

    pub async fn health(&self) -> AppResult<HealthState> {
        let subscription_count = sqlx::query_scalar("SELECT COUNT(*) FROM streamers")
            .fetch_one(&self.pool)
            .await?;
        let schedule_count = sqlx::query_scalar("SELECT COUNT(*) FROM schedules")
            .fetch_one(&self.pool)
            .await?;
        let last: Option<String> = sqlx::query_scalar("SELECT MAX(live_checked_at) FROM streamers")
            .fetch_one(&self.pool)
            .await?;
        Ok(HealthState {
            database_ok: true,
            subscription_count,
            schedule_count,
            last_live_check_at: parse_opt_ts(last.as_deref())?,
        })
    }

    pub async fn get_setting(&self, key: &str) -> AppResult<Option<String>> {
        ensure_setting_key_allowed(key)?;
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn set_settings(&self, values: &[(&str, String)]) -> AppResult<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed_at = Utc::now().to_rfc3339();
        let mut server_settings_changed = false;
        for (key, value) in values {
            ensure_setting_key_allowed(key)?;
            let server_key = is_server_setting_key(key);
            let revision = if server_key {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COALESCE(MAX(revision), 0) + 1 FROM settings
                     WHERE key IN (
                        'live_poll_seconds', 'dynamic_poll_seconds', 'auto_analyze_images',
                        'live_stats_enabled', 'fertility_enabled', 'llm_base_url',
                        'llm_model', 'llm_prompt_version', 'llm_reasoning_effort',
                        'llm_parallelism', 'proxy_port'
                     )",
                )
                .fetch_one(&mut *tx)
                .await?
            } else {
                1
            };
            let changed = sqlx::query(
                "INSERT INTO settings (key, value, updated_at, revision) VALUES (?, ?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET
                    value = excluded.value,
                    updated_at = excluded.updated_at,
                    revision = MAX(settings.revision + 1, excluded.revision)
                 WHERE settings.value <> excluded.value",
            )
            .bind(*key)
            .bind(value)
            .bind(&changed_at)
            .bind(revision)
            .execute(&mut *tx)
            .await?;
            if changed.rows_affected() > 0 && server_key {
                server_settings_changed = true;
            }
        }
        if server_settings_changed {
            self.emit_server_settings_change(&mut tx, SyncActor::system())
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_fertility_cache(&self) -> AppResult<Option<FertilityCache>> {
        let row = sqlx::query(
            "SELECT payload_json, fetched_at, source FROM fertility_cache WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let fetched_at: String = row.try_get("fetched_at")?;
        let fetched_at = DateTime::parse_from_rfc3339(&fetched_at)
            .map_err(|error| AppError::validation(format!("生理期缓存时间无效: {error}")))?
            .with_timezone(&Utc);
        Ok(Some(FertilityCache {
            payload_json: row.try_get("payload_json")?,
            fetched_at,
            source: row.try_get("source")?,
        }))
    }

    pub async fn save_fertility_cache(
        &self,
        payload_json: &str,
        fetched_at: DateTime<Utc>,
        source: &str,
    ) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO fertility_cache (id, payload_json, fetched_at, source) VALUES (1, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET payload_json = excluded.payload_json, fetched_at = excluded.fetched_at, source = excluded.source",
        )
        .bind(payload_json)
        .bind(fetched_at.to_rfc3339())
        .bind(source)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn clear_fertility_cache(&self) -> AppResult<u64> {
        let result = sqlx::query("DELETE FROM fertility_cache WHERE id = 1")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn enqueue_mutation(&self, mutation: &SyncMutation) -> AppResult<()> {
        let operation = mutation.operation.trim();
        let entity = mutation.entity.trim();
        if !matches!(operation, "create" | "update" | "delete") {
            return Err(AppError::validation(
                "同步操作必须是 create、update 或 delete",
            ));
        }
        if !matches!(entity, "subscription" | "schedule" | "link") {
            return Err(AppError::validation("同步实体不受支持"));
        }
        if mutation.mutation_id.trim().is_empty() {
            return Err(AppError::validation("同步 mutation_id 不能为空"));
        }
        let existing: Option<String> =
            sqlx::query_scalar("SELECT status FROM sync_queue WHERE mutation_id = ?")
                .bind(&mutation.mutation_id)
                .fetch_optional(&self.pool)
                .await?;
        if existing.is_some() {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO sync_queue
                (mutation_id, operation, entity, payload_json, base_updated_at, device_id, base_revision, status, last_error, created_at, result_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', NULL, ?, NULL)
             ON CONFLICT(mutation_id) DO NOTHING",
        )
        .bind(&mutation.mutation_id)
        .bind(operation)
        .bind(entity)
        .bind(serde_json::to_string(&mutation.payload)?)
        .bind(mutation.base_updated_at.map(|value| value.to_rfc3339()))
        .bind(mutation.device_id.as_deref())
        .bind(mutation.base_revision)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_pending_mutations(&self, limit: i64) -> AppResult<Vec<SyncMutation>> {
        let rows = sqlx::query_as::<_, SyncMutationRow>(
            "SELECT mutation_id, operation, entity, payload_json, base_updated_at, device_id,
                    base_revision, status, last_error, created_at, applied_at, result_json
             FROM sync_queue WHERE status IN ('pending', 'syncing')
             ORDER BY created_at LIMIT ?",
        )
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_sync_mutation).collect()
    }

    pub async fn list_blocked_mutations(&self, limit: i64) -> AppResult<Vec<SyncMutation>> {
        let rows = sqlx::query_as::<_, SyncMutationRow>(
            "SELECT mutation_id, operation, entity, payload_json, base_updated_at, device_id,
                    base_revision, status, last_error, created_at, applied_at, result_json
             FROM sync_queue WHERE status = 'blocked'
             ORDER BY created_at LIMIT ?",
        )
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_sync_mutation).collect()
    }
    pub async fn update_mutation_status(
        &self,
        mutation_id: &str,
        status: &str,
        last_error: Option<&str>,
    ) -> AppResult<()> {
        self.update_mutation_result(mutation_id, status, last_error, None)
            .await
    }

    pub async fn update_mutation_result(
        &self,
        mutation_id: &str,
        status: &str,
        last_error: Option<&str>,
        result: Option<&Value>,
    ) -> AppResult<()> {
        if !matches!(status, "pending" | "syncing" | "blocked" | "applied") {
            return Err(AppError::validation("非法同步状态"));
        }
        let applied_at = (status == "applied").then(|| Utc::now().to_rfc3339());
        sqlx::query(
            "UPDATE sync_queue SET status = ?, last_error = ?, applied_at = COALESCE(?, applied_at), result_json = COALESCE(?, result_json)
             WHERE mutation_id = ?",
        )
        .bind(status)
        .bind(last_error)
        .bind(applied_at)
        .bind(result.map(serde_json::to_string).transpose()?)
        .bind(mutation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_client_sync_state(&self, key: &str) -> AppResult<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT value FROM client_sync_state WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn set_client_sync_state(&self, key: &str, value: &str) -> AppResult<()> {
        if key.trim().is_empty() {
            return Err(AppError::validation("同步状态键不能为空"));
        }
        sqlx::query(
            "INSERT INTO client_sync_state (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_client_sync_state(&self, key: &str) -> AppResult<bool> {
        if key.trim().is_empty() {
            return Err(AppError::validation("同步状态键不能为空"));
        }
        let result = sqlx::query("DELETE FROM client_sync_state WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn client_sync_states(&self) -> AppResult<Vec<ClientSyncState>> {
        let rows = sqlx::query("SELECT key, value, updated_at FROM client_sync_state ORDER BY key")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ClientSyncState {
                    key: row.try_get("key")?,
                    value: row.try_get("value")?,
                    updated_at: parse_ts(row.try_get::<String, _>("updated_at")?.as_str())?,
                })
            })
            .collect()
    }
}

// Compare the fields the automatic UPDATE actually writes, not timestamps or
// revision metadata. Do not suppress user-visible changes such as cancellation.
fn schedule_matches_input(current: &Schedule, input: &NewSchedule) -> bool {
    current.participants == input.participants
        && current.title == input.title
        && current.start_at == input.start_at
        && current.end_at == input.end_at
        && current.room_url == input.room_url
        && current.room_owner_mid == input.room_owner_mid
        && current.room_owner_name == input.room_owner_name
        && current.confidence == input.confidence
        && current.source_kind == input.source_kind
        && current.source_post_id == input.source_post_id
        && current.source_image_url == input.source_image_url
        && current.source_author_mid == input.source_author_mid
        && current.source_fingerprint == input.source_fingerprint
        && current.source_published_at == input.source_published_at
}

fn locked_schedule_source_matches(current: &Schedule, input: &NewSchedule) -> bool {
    input
        .source_fingerprint
        .as_ref()
        .is_none_or(|v| current.source_fingerprint.as_ref() == Some(v))
        && input
            .source_image_url
            .as_ref()
            .is_none_or(|v| current.source_image_url.as_ref() == Some(v))
        && input
            .source_author_mid
            .as_ref()
            .is_none_or(|v| current.source_author_mid.as_ref() == Some(v))
}

fn map_live_session(row: LiveSessionRow) -> AppResult<LiveSession> {
    let started_at = parse_ts(&row.started_at)?;
    let ended_at = parse_opt_ts(row.ended_at.as_deref())?;
    let duration_seconds = if ended_at.is_none() {
        Utc::now()
            .signed_duration_since(started_at)
            .num_seconds()
            .max(0)
    } else {
        row.duration_seconds
    };
    Ok(LiveSession {
        schedule_id: row.schedule_id,
        id: row.id,
        streamer_id: row.streamer_id,
        streamer_name: row.streamer_name,
        room_id: row.room_id,
        title: row.title,
        cover_url: row.cover_url,
        started_at,
        ended_at,
        duration_seconds,
        peak_online: row.peak_online.max(0) as u64,
        peak_high_energy: row.peak_high_energy.max(0) as u64,
        avg_online: row.avg_online.max(0) as u64,
        online_samples: row.online_samples,
        online_total: row.online_total,
        revenue_cents: row.revenue_cents,
        gift_revenue_cents: row.gift_revenue_cents,
        super_chat_revenue_cents: row.super_chat_revenue_cents,
        guard_revenue_cents: row.guard_revenue_cents,
        paid_user_count: row.paid_user_count,
        interactive_user_count: row.interactive_user_count,
        danmaku_count: row.danmaku_count,
        gift_count: row.gift_count,
        super_chat_count: row.super_chat_count,
        guard_count: row.guard_count,
        reconnect_count: row.reconnect_count,
        segment_count: row.segment_count.max(0) as u64,
        data_quality: row.data_quality,
        collector_state: row.collector_state,
        created_at: parse_ts(&row.created_at)?,
        updated_at: parse_ts(&row.updated_at)?,
        revision: row.revision,
    })
}
fn map_schedule(row: ScheduleRow) -> AppResult<Schedule> {
    Ok(Schedule {
        id: row.id,
        streamer_id: row.streamer_id,
        streamer_name: row.streamer_name,
        streamer_avatar: row.streamer_avatar,
        participants: serde_json::from_str(&row.participants_json).unwrap_or_default(),
        title: row.title,
        start_at: parse_ts(&row.start_at)?,
        end_at: parse_opt_ts(row.end_at.as_deref())?,
        room_url: row.room_url,
        room_owner_mid: row.room_owner_mid,
        room_owner_name: row.room_owner_name,
        source_kind: row.source_kind,
        source_post_id: row.source_post_id,
        source_image_url: row.source_image_url,
        source_author_mid: row.source_author_mid,
        source_fingerprint: row.source_fingerprint,
        source_identity: row.source_identity,
        source_published_at: parse_opt_ts(row.source_published_at.as_deref())?,
        weekly_rule_id: row.weekly_rule_id,
        weekly_occurrence_date: row.weekly_occurrence_date,
        confidence: row.confidence,
        manual_locked: row.manual_locked != 0,
        source_changed: row.source_changed != 0,
        cancelled: row.cancelled != 0,
        revision: row.revision,
        created_at: parse_ts(&row.created_at)?,
        updated_at: parse_ts(&row.updated_at)?,
    })
}

fn map_weekly_rule(row: WeeklyScheduleRuleRow) -> AppResult<WeeklyScheduleRule> {
    Ok(WeeklyScheduleRule {
        id: row.id,
        rule_key: row.rule_key,
        streamer_id: row.streamer_id,
        title: row.title,
        weekdays_mask: u8::try_from(row.weekdays_mask)
            .map_err(|_| AppError::validation("固定周表星期数据损坏"))?,
        start_time: row.start_time,
        end_time: row.end_time,
        timezone: row.timezone,
        room_url: row.room_url,
        effective_from: row.effective_from,
        effective_until: row.effective_until,
        enabled: row.enabled != 0,
        revision: row.revision,
        created_at: parse_ts(&row.created_at)?,
        updated_at: parse_ts(&row.updated_at)?,
    })
}

fn map_sync_mutation(row: SyncMutationRow) -> AppResult<SyncMutation> {
    Ok(SyncMutation {
        mutation_id: row.mutation_id,
        operation: row.operation,
        entity: row.entity,
        payload: serde_json::from_str(&row.payload_json)?,
        base_updated_at: parse_opt_ts(row.base_updated_at.as_deref())?,
        device_id: row.device_id,
        base_revision: row.base_revision,
        status: row.status,
        last_error: row.last_error,
        created_at: parse_ts(&row.created_at)?,
        applied_at: parse_opt_ts(row.applied_at.as_deref())?,
        result_json: row
            .result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
    })
}

fn parse_ts(value: &str) -> AppResult<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .map_err(|error| AppError::validation(format!("时间格式错误: {error}")))?
        .with_timezone(&Utc))
}

fn parse_opt_ts(value: Option<&str>) -> AppResult<Option<DateTime<Utc>>> {
    value.map(parse_ts).transpose()
}

fn validate_mid(mid: &str) -> AppResult<()> {
    let mid = mid.trim();
    if mid.is_empty() || mid.len() > 32 || !mid.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(AppError::validation("B 站 MID 必须是数字"));
    }
    Ok(())
}

fn normalize_aliases(mut aliases: Vec<String>, name: &str) -> Vec<String> {
    aliases.push(name.to_owned());
    let mut seen = std::collections::HashSet::new();
    aliases
        .into_iter()
        .map(|alias| alias.trim().to_owned())
        .filter(|alias| !alias.is_empty() && alias.chars().count() <= 80)
        .filter(|alias| seen.insert(alias.to_lowercase()))
        .take(30)
        .collect()
}

fn live_url_for_room(room_id: &str) -> String {
    format!("https://live.bilibili.com/{}", room_id.trim())
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn validate_schedule_input(input: &NewSchedule) -> AppResult<()> {
    let title = input.title.trim();
    if title.is_empty() {
        return Err(AppError::validation("日程标题不能为空"));
    }
    if title.chars().count() > 200 {
        return Err(AppError::validation("日程标题不能超过 200 字"));
    }
    if !(0.0..=1.0).contains(&input.confidence) {
        return Err(AppError::validation("置信度必须在 0 到 1 之间"));
    }
    if input.participants.len() > 20
        || input.participants.iter().any(|participant| {
            let name = participant.trim();
            name.is_empty() || name.chars().count() > 80
        })
    {
        return Err(AppError::validation(
            "团播参与者最多 20 人，每人名称不能为空且不能超过 80 字",
        ));
    }
    if let Some(end) = input.end_at {
        if end <= input.start_at {
            return Err(AppError::validation("结束时间必须晚于开始时间"));
        }
    }
    if let Some(url) = &input.room_url {
        validate_live_url(url)?;
    }
    if input
        .room_owner_name
        .as_deref()
        .is_some_and(|name| name.trim().is_empty() || name.chars().count() > 80)
    {
        return Err(AppError::validation(
            "直播间所属账号名称长度必须为 1 到 80 字",
        ));
    }
    if input
        .room_owner_mid
        .as_deref()
        .is_some_and(|mid| mid.trim().is_empty() || mid.chars().count() > 32)
    {
        return Err(AppError::validation("直播间所属账号 MID 无效"));
    }
    Ok(())
}

fn validate_live_url(url: &str) -> AppResult<()> {
    let parsed = url::Url::parse(url)?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("live.bilibili.com") {
        return Err(AppError::validation(
            "直播间地址只允许 https://live.bilibili.com/",
        ));
    }
    Ok(())
}

fn is_server_setting_key(key: &str) -> bool {
    matches!(
        key,
        "live_poll_seconds"
            | "dynamic_poll_seconds"
            | "auto_analyze_images"
            | "live_stats_enabled"
            | "fertility_enabled"
            | "llm_base_url"
            | "llm_model"
            | "llm_prompt_version"
            | "llm_reasoning_effort"
            | "llm_parallelism"
            | "proxy_port"
    )
}

fn ensure_setting_key_allowed(key: &str) -> AppResult<()> {
    let lower = key.to_ascii_lowercase();
    if lower.contains("cookie")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("api_key")
        || lower.contains("apikey")
    {
        return Err(AppError::validation("敏感配置禁止写入 SQLite"));
    }
    if !SETTING_KEYS.contains(&key) {
        return Err(AppError::validation(format!("未知设置键: {key}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{load_settings, save_settings, AppSettings, DataMode, ReasoningEffort};
    use chrono::Duration;

    fn add_input(mid: &str, name: &str) -> AddSubscription {
        AddSubscription {
            mid: mid.to_owned(),
            name: Some(name.to_owned()),
            avatar_url: None,
            room_id: Some("12345".to_owned()),
            aliases: Some(vec!["别名".to_owned()]),
            schedule_enabled: true,
            live_notify_enabled: true,
            live_stats_enabled: false,
        }
    }

    fn new_schedule(streamer_id: i64, start_at: DateTime<Utc>) -> NewSchedule {
        NewSchedule {
            streamer_id,
            participants: Vec::new(),
            title: "周五杂谈".to_owned(),
            start_at,
            end_at: Some(start_at + Duration::hours(2)),
            room_url: Some("https://live.bilibili.com/12345".to_owned()),
            room_owner_mid: None,
            room_owner_name: None,
            source_kind: "text".to_owned(),
            source_post_id: Some("post-1".to_owned()),
            source_image_url: None,
            source_author_mid: Some("100".to_owned()),
            source_fingerprint: Some("fp-1".to_owned()),
            source_identity: Some("text:post-1".to_owned()),
            source_published_at: None,
            confidence: 1.0,
        }
    }

    async fn remove_test_directory_when_released(path: &Path) {
        let mut last_error = None;
        for _ in 0..20 {
            match tokio::fs::remove_dir_all(path).await {
                Ok(()) => return,
                Err(error) => {
                    last_error = Some(error);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
        panic!("temporary test directory stayed locked: {last_error:?}");
    }

    #[tokio::test]
    async fn subscription_is_idempotent() {
        let store = SqliteStore::in_memory().await.expect("db");
        let first = store
            .add_subscription(&add_input("100", "主播A"))
            .await
            .expect("first");
        let second = store
            .add_subscription(&add_input("100", "主播A改名"))
            .await
            .expect("second");
        let list = store.list_streamers().await.expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(first.id, second.id);
        assert_eq!(list[0].name, "主播A改名");
        assert!(list[0].aliases.iter().any(|value| value == "主播A改名"));
    }

    #[tokio::test]
    async fn fan_snapshots_update_current_count_and_do_not_regress_on_old_samples() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("300", "粉丝主播"))
            .await
            .expect("streamer");
        assert_eq!(streamer.fans, None);
        let start = Utc::now() - Duration::hours(2);
        store
            .record_fan_snapshot(streamer.id, 1_000, start)
            .await
            .expect("first");
        store
            .record_fan_snapshot(streamer.id, 1_100, start + Duration::hours(1))
            .await
            .expect("second");
        store
            .record_fan_snapshot(streamer.id, 900, start + Duration::minutes(30))
            .await
            .expect("older");
        let current = store.get_streamer(streamer.id).await.expect("current");
        assert_eq!(current.fans, Some(1_100));
        let snapshots = store
            .list_fan_snapshots(streamer.id, start)
            .await
            .expect("snapshots");
        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0].fans, 1_000);
        assert_eq!(snapshots[2].fans, 1_100);
    }

    #[tokio::test]
    async fn repair_clears_reservation_room_urls_that_belong_to_someone_else() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("672342685", "乃琳Queen"))
            .await
            .expect("streamer");
        sqlx::query("UPDATE streamers SET room_id = ?, live_url = ? WHERE id = ?")
            .bind("22625027")
            .bind("https://live.bilibili.com/22625027")
            .bind(streamer.id)
            .execute(store.pool())
            .await
            .expect("streamer room");
        let start = Utc::now() + Duration::days(1);
        // 老版本把预约 id 当房间号写进来的记录。
        let mut wrong = new_schedule(streamer.id, start);
        wrong.source_kind = "reserve".to_owned();
        wrong.room_url = Some("https://live.bilibili.com/5866502".to_owned());
        wrong.room_owner_mid = Some("672342685".to_owned());
        wrong.room_owner_name = Some("乃琳Queen".to_owned());
        let (wrong_schedule, _) = store.upsert_schedule(&wrong).await.expect("wrong");
        // 指向主播本人直播间的记录不能被清掉。
        let mut right = new_schedule(streamer.id, start + Duration::hours(4));
        right.source_kind = "reserve".to_owned();
        right.room_url = Some("https://live.bilibili.com/22625027".to_owned());
        right.room_owner_mid = Some("672342685".to_owned());
        right.room_owner_name = Some("乃琳Queen".to_owned());
        right.source_post_id = Some("post-2".to_owned());
        right.source_identity = Some("reserve:post-2".to_owned());
        let (right_schedule, _) = store.upsert_schedule(&right).await.expect("right");

        let repaired = store
            .repair_misattributed_reserve_room_urls()
            .await
            .expect("repair");
        assert_eq!(repaired, 1);
        let cleared = store
            .get_schedule(wrong_schedule.id)
            .await
            .expect("cleared");
        assert!(cleared.room_url.is_none());
        let kept = store.get_schedule(right_schedule.id).await.expect("kept");
        assert_eq!(
            kept.room_url.as_deref(),
            Some("https://live.bilibili.com/22625027")
        );
        // 幂等：第二轮没有可清空的记录。
        assert_eq!(
            store
                .repair_misattributed_reserve_room_urls()
                .await
                .expect("second repair"),
            0
        );
    }

    #[tokio::test]
    async fn consistent_backup_restores_subscriptions_and_schedules() {
        let directory =
            std::env::temp_dir().join(format!("oshical-backup-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create temp directory");
        let database = directory.join("oshical.db");
        let backup = directory.join("oshical-backup.db");
        let existing_target = directory.join("existing.db");

        let store = SqliteStore::connect(&database).await.expect("db");
        let streamer = store
            .add_subscription(&add_input("104", "备份主播"))
            .await
            .expect("add");
        let start = Utc::now() + Duration::days(1);
        let (schedule, inserted) = store
            .upsert_schedule(&new_schedule(streamer.id, start))
            .await
            .expect("schedule");
        assert!(inserted);
        tokio::fs::write(&existing_target, b"keep")
            .await
            .expect("existing target");
        assert!(store.backup_to(&existing_target).await.is_err());
        assert_eq!(
            tokio::fs::read(&existing_target)
                .await
                .expect("read target"),
            b"keep"
        );
        store.backup_to(&backup).await.expect("consistent backup");
        store.pool.close().await;
        drop(store);

        let restored_path = directory.join("restored.db");
        tokio::fs::copy(&backup, &restored_path)
            .await
            .expect("restore copied backup");

        let restored = SqliteStore::connect(&restored_path)
            .await
            .expect("reopen restored backup");
        let restored_streamers = restored.list_streamers().await.expect("streamers");
        assert_eq!(restored_streamers.len(), 1);
        assert_eq!(restored_streamers[0].name, "备份主播");
        let restored_schedule = restored
            .get_schedule(schedule.id)
            .await
            .expect("restored schedule");
        assert_eq!(restored_schedule.title, "周五杂谈");
        restored.pool.close().await;
        drop(restored);

        remove_test_directory_when_released(&directory).await;
    }

    #[tokio::test]
    async fn subscription_toggles_and_links_round_trip() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("101", "主播B"))
            .await
            .expect("add");
        store
            .update_subscription(
                streamer.id,
                &UpdateSubscription {
                    schedule_enabled: Some(false),
                    live_notify_enabled: Some(false),
                    live_stats_enabled: None,
                    aliases: Some(vec!["B酱".to_owned()]),
                },
            )
            .await
            .expect("update");
        store
            .add_linked_account(streamer.id, "200", "官号", "")
            .await
            .expect("link");
        let loaded = store.get_streamer(streamer.id).await.expect("get");
        assert!(!loaded.schedule_enabled);
        assert!(!loaded.live_notify_enabled);
        assert!(loaded.aliases.iter().any(|value| value == "B酱"));
        assert_eq!(loaded.linked_accounts.len(), 1);

        let partially_updated = store
            .update_subscription(
                streamer.id,
                &UpdateSubscription {
                    live_stats_enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .expect("partial update");
        assert!(!partially_updated.schedule_enabled);
        assert!(!partially_updated.live_notify_enabled);
        assert!(partially_updated.live_stats_enabled);
        assert!(partially_updated.aliases.iter().any(|value| value == "B酱"));
    }

    #[tokio::test]
    async fn streamer_reorder_persists_and_requires_the_complete_set() {
        let store = SqliteStore::in_memory().await.expect("db");
        let first = store
            .add_subscription(&add_input("107", "主播甲"))
            .await
            .expect("first");
        let second = store
            .add_subscription(&add_input("108", "主播乙"))
            .await
            .expect("second");
        let third = store
            .add_subscription(&add_input("109", "主播丙"))
            .await
            .expect("third");

        let ordered = store
            .reorder_subscriptions(&[third.id, first.id, second.id])
            .await
            .expect("reorder");
        assert_eq!(
            ordered
                .iter()
                .map(|streamer| streamer.id)
                .collect::<Vec<_>>(),
            vec![third.id, first.id, second.id]
        );
        assert_eq!(
            ordered
                .iter()
                .map(|streamer| streamer.sort_order)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(store
            .reorder_subscriptions(&[first.id, second.id])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn deleting_streamer_removes_her_schedules_links_and_suppressions() {
        let store = SqliteStore::in_memory().await.expect("db");
        let first = store
            .add_subscription(&add_input("105", "删除主播"))
            .await
            .expect("first");
        let second = store
            .add_subscription(&add_input("106", "保留主播"))
            .await
            .expect("second");
        store
            .add_linked_account(first.id, "900", "删除主播官号", "")
            .await
            .expect("link");
        let start = Utc::now() + Duration::days(1);
        let first_schedule = store
            .upsert_schedule(&new_schedule(first.id, start))
            .await
            .expect("first schedule")
            .0;
        let second_schedule = store
            .upsert_schedule(&new_schedule(second.id, start))
            .await
            .expect("second schedule")
            .0;
        store
            .add_source_suppression(first.id, "deleted-post", "deleted-fingerprint")
            .await
            .expect("suppression");

        store.delete_subscription(first.id).await.expect("delete");

        let streamers = store.list_streamers().await.expect("streamers");
        assert_eq!(streamers.len(), 1);
        assert_eq!(streamers[0].id, second.id);
        let schedules = store.list_all_schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].id, second_schedule.id);
        assert!(store.get_schedule(first_schedule.id).await.is_err());
        assert!(!store
            .is_source_suppressed(first.id, "deleted-post", "deleted-fingerprint")
            .await
            .expect("suppression lookup"));
    }

    #[tokio::test]
    async fn schedule_manual_lock_and_delete_suppression() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("102", "主播C"))
            .await
            .expect("add");
        let start = Utc::now() + Duration::days(1);
        let (schedule, inserted) = store
            .upsert_schedule(&new_schedule(streamer.id, start))
            .await
            .expect("schedule");
        assert!(inserted);
        let updated = store
            .update_schedule(
                schedule.id,
                &UpdateSchedule {
                    title: Some("用户改过的标题".to_owned()),
                    start_at: Some(start + Duration::minutes(30)),
                    end_at: None,
                    room_url: None,
                },
            )
            .await
            .expect("update");
        assert!(updated.manual_locked);

        let mut changed_source = new_schedule(streamer.id, start);
        changed_source.title = "自动新标题".to_owned();
        changed_source.source_fingerprint = Some("fp-2".to_owned());
        let (locked, inserted) = store
            .upsert_schedule(&changed_source)
            .await
            .expect("upsert locked");
        assert!(!inserted);
        assert_eq!(locked.title, "用户改过的标题");
        assert!(locked.source_changed);

        store.delete_schedule(locked.id).await.expect("delete");
        assert!(store
            .is_source_suppressed(streamer.id, "post-1", "fp-2")
            .await
            .expect("suppressed"));
    }

    #[tokio::test]
    async fn schedule_range_hides_disabled_and_cancelled() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("103", "主播D"))
            .await
            .expect("add");
        let start = Utc::now() + Duration::days(1);
        let (schedule, _) = store
            .upsert_schedule(&new_schedule(streamer.id, start))
            .await
            .expect("schedule");
        store.cancel_schedule(schedule.id).await.expect("cancel");
        let visible = store
            .list_schedules(
                start - Duration::hours(1),
                start + Duration::hours(1),
                false,
            )
            .await
            .expect("list");
        assert!(visible.is_empty());
        let history = store
            .list_schedules(start - Duration::hours(1), start + Duration::hours(1), true)
            .await
            .expect("history");
        assert_eq!(history.len(), 1);
        assert!(history[0].cancelled);

        store
            .update_subscription(
                streamer.id,
                &UpdateSubscription {
                    schedule_enabled: Some(false),
                    live_notify_enabled: None,
                    live_stats_enabled: None,
                    aliases: None,
                },
            )
            .await
            .expect("disable");
        let hidden = store
            .list_schedules(start - Duration::hours(1), start + Duration::hours(1), true)
            .await
            .expect("hidden");
        assert!(hidden.is_empty());
    }

    #[tokio::test]
    async fn settings_round_trip_and_secret_rejection() {
        let store = SqliteStore::in_memory().await.expect("db");
        let settings = AppSettings {
            server_url: "http://127.0.0.1:8787".to_owned(),
            data_mode: DataMode::Server,
            proxy_port: Some(7890),
            llm_reasoning_effort: ReasoningEffort::High,
            llm_parallelism: 3,
            ..Default::default()
        };
        save_settings(&store, &settings).await.expect("save");
        let loaded = load_settings(&store).await.expect("load");
        assert_eq!(loaded, settings);
        let denied = store
            .set_settings(&[("llm_api_key", "\"secret\"".to_owned())])
            .await;
        assert!(denied.is_err());
    }

    #[tokio::test]
    async fn dirty_llm_settings_fall_back_to_defaults() {
        let store = SqliteStore::in_memory().await.expect("db");
        // 直接写库模拟「历史遗留 / 手工改坏」的脏值：set_settings 会校验并
        // 在同步变更里解析失败，这里要绕过它。
        for (key, value) in [
            ("llm_parallelism", "0"),
            ("llm_reasoning_effort", "\"turbo\""),
        ] {
            sqlx::query(
                "INSERT INTO settings (key, value, updated_at, revision) VALUES (?, ?, ?, 1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(value)
            .bind(Utc::now().to_rfc3339())
            .execute(store.pool())
            .await
            .expect("seed dirty settings");
        }
        let loaded = load_settings(&store).await.expect("load");
        assert_eq!(loaded.llm_parallelism, 1);
        assert_eq!(loaded.llm_reasoning_effort, ReasoningEffort::Auto);
    }

    #[test]
    fn omitted_llm_settings_deserialize_with_defaults() {
        let raw = r#"{
            "dataMode": "local",
            "serverUrl": "",
            "livePollSeconds": 20,
            "dynamicPollSeconds": 300,
            "autoAnalyzeImages": true,
            "notificationsEnabled": true,
            "llmBaseUrl": "https://api.example.com/v1",
            "llmModel": "cn:test",
            "llmPromptVersion": "schedule-v1"
        }"#;
        let settings: AppSettings = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(settings.llm_reasoning_effort, ReasoningEffort::Auto);
        assert_eq!(settings.llm_parallelism, 1);
        settings.validate().expect("validate");
    }

    #[tokio::test]
    async fn llm_parallelism_rejects_out_of_range() {
        let store = SqliteStore::in_memory().await.expect("db");
        let settings = AppSettings {
            llm_parallelism: 9,
            ..Default::default()
        };
        assert!(save_settings(&store, &settings).await.is_err());
    }

    #[tokio::test]
    async fn post_analysis_cache_round_trip() {
        let store = SqliteStore::in_memory().await.expect("db");
        let cache_key = "post-1:digest:model:prompt-v1-post-v1";
        assert!(store
            .load_post_analysis(cache_key)
            .await
            .expect("miss")
            .is_none());
        let first = serde_json::json!({"events": [{"title": "周五杂谈"}]});
        store
            .save_post_analysis(cache_key, "post-1", "digest", "model", "prompt", &first)
            .await
            .expect("save");
        assert_eq!(
            store.load_post_analysis(cache_key).await.expect("hit"),
            Some(first)
        );
        let replacement = serde_json::json!({"events": []});
        store
            .save_post_analysis(
                cache_key,
                "post-1",
                "digest",
                "model",
                "prompt",
                &replacement,
            )
            .await
            .expect("overwrite");
        assert_eq!(
            store
                .load_post_analysis(cache_key)
                .await
                .expect("hit again"),
            Some(replacement)
        );
    }

    #[tokio::test]
    async fn deleting_stale_schedule_sources_keeps_kept_identities() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("301", "主播D"))
            .await
            .expect("streamer");
        let start_at = Utc::now() + Duration::hours(3);
        let mut input = new_schedule(streamer.id, start_at);
        input.source_identity = Some("image:post-9:digest:0".to_owned());
        input.source_post_id = Some("post-9".to_owned());
        let (schedule, created) = store.upsert_schedule(&input).await.expect("upsert");
        assert!(created);

        let mut extra = input.clone();
        extra.source_identity = Some("image:post-9:digest:1".to_owned());
        extra.start_at = start_at + Duration::hours(1);
        store
            .attach_schedule_source(schedule.id, &extra)
            .await
            .expect("attach");

        let removed = store
            .delete_schedule_sources_except("post-9", &["image:post-9:digest:1".to_owned()])
            .await
            .expect("prune");
        assert_eq!(
            removed,
            vec![("image:post-9:digest:0".to_owned(), schedule.id)]
        );

        let remaining: Vec<String> = sqlx::query_scalar(
            "SELECT source_identity FROM schedule_sources
             WHERE source_post_id = ? ORDER BY id",
        )
        .bind("post-9")
        .fetch_all(store.pool())
        .await
        .expect("remaining sources");
        assert_eq!(remaining, vec!["image:post-9:digest:1".to_owned()]);
        // 本层只删来源行，场次本身留给 service 层决定。
        store
            .get_schedule(schedule.id)
            .await
            .expect("schedule kept");
    }

    /// 同一个场次先由预约卡（或另一条动态）以别的 identity 落库后，再来一份
    /// 来源写入同一个 (streamer, start_at, title)：应当复用它，而不是撞
    /// UNIQUE 约束把整批刷新打断。
    #[tokio::test]
    async fn upserting_the_same_slot_with_a_new_identity_reuses_the_row() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("302", "主播E"))
            .await
            .expect("streamer");
        let start_at = Utc::now() + Duration::hours(6);
        let mut reservation = new_schedule(streamer.id, start_at);
        reservation.source_kind = "reserve".to_owned();
        reservation.source_identity = Some("reserve:post-77".to_owned());
        reservation.source_post_id = Some("post-77".to_owned());
        let (reserved, created) = store
            .upsert_schedule(&reservation)
            .await
            .expect("reservation");
        assert!(created);

        let mut image = new_schedule(streamer.id, start_at);
        image.source_kind = "image".to_owned();
        image.source_identity = Some("image:post-77:digest:0".to_owned());
        image.source_post_id = Some("post-77".to_owned());
        let (same, created) = store.upsert_schedule(&image).await.expect("image source");

        assert!(!created, "同一场次不该再插入一条");
        assert_eq!(same.id, reserved.id);
        assert_eq!(
            store.list_all_schedules().await.expect("schedules").len(),
            1
        );
        let identities: Vec<String> = sqlx::query_scalar(
            "SELECT source_identity FROM schedule_sources
             WHERE schedule_id = ? ORDER BY id",
        )
        .bind(reserved.id)
        .fetch_all(store.pool())
        .await
        .expect("identities");
        assert!(identities.contains(&"image:post-77:digest:0".to_owned()));
        assert!(identities.contains(&"reserve:post-77".to_owned()));
    }

    #[tokio::test]
    async fn streamer_refresh_metadata_emits_sync_changes() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("204", "同步元数据主播"))
            .await
            .expect("streamer");
        let before = store
            .sync_changes_after(0, 1000)
            .await
            .expect("initial feed");
        store
            .record_streamer_error(streamer.id, "network failed")
            .await
            .expect("record error");
        let after_error = store
            .sync_changes_after(before.latest_seq, 10)
            .await
            .expect("error feed");
        let change = after_error.changes.last().expect("error change");
        assert_eq!(change.entity_type, "subscription");
        assert_eq!(change.entity_id, streamer.id.to_string());
        assert_eq!(
            change
                .payload
                .as_ref()
                .and_then(|value| value.get("lastError"))
                .and_then(Value::as_str),
            Some("network failed")
        );

        store
            .record_streamer_error(streamer.id, "network failed")
            .await
            .expect("unchanged error");
        let unchanged = store
            .sync_changes_after(after_error.latest_seq, 10)
            .await
            .expect("unchanged feed");
        assert!(unchanged.changes.is_empty());

        store
            .record_refresh_result(streamer.id, None)
            .await
            .expect("refresh result");
        let refreshed = store
            .sync_changes_after(unchanged.latest_seq, 10)
            .await
            .expect("refresh feed");
        let change = refreshed.changes.last().expect("refresh change");
        assert!(change.payload.as_ref().is_some_and(|value| {
            value
                .get("lastRefreshAt")
                .is_some_and(|value| !value.is_null())
        }));
    }

    #[tokio::test]
    async fn sync_queue_is_idempotent() {
        let store = SqliteStore::in_memory().await.expect("db");
        let mutation = SyncMutation {
            mutation_id: "m-1".to_owned(),
            operation: "create".to_owned(),
            entity: "schedule".to_owned(),
            payload: json!({"title": "测试"}),
            base_updated_at: None,
            device_id: None,
            base_revision: None,
            status: "pending".to_owned(),
            last_error: None,
            created_at: Utc::now(),
            applied_at: None,
            result_json: None,
        };
        store.enqueue_mutation(&mutation).await.expect("first");
        store.enqueue_mutation(&mutation).await.expect("second");
        let pending = store.list_pending_mutations(10).await.expect("pending");
        assert_eq!(pending.len(), 1);
        store
            .update_mutation_status("m-1", "applied", None)
            .await
            .expect("applied");
        assert!(store
            .list_pending_mutations(10)
            .await
            .expect("empty")
            .is_empty());
    }
    #[tokio::test]
    async fn weekly_rule_creation_is_idempotent_by_caller_key() {
        let store = SqliteStore::in_memory().await.expect("db");
        let first_streamer = store
            .add_subscription(&add_input("201", "周表甲"))
            .await
            .expect("first streamer");
        let second_streamer = store
            .add_subscription(&add_input("202", "周表乙"))
            .await
            .expect("second streamer");
        let input = NewWeeklyScheduleRule {
            rule_key: Some("client-weekly-key".to_owned()),
            streamer_id: first_streamer.id,
            title: "固定周表".to_owned(),
            weekdays_mask: 0b0000_0001,
            start_time: "20:00".to_owned(),
            end_time: Some("22:00".to_owned()),
            timezone: "Asia/Shanghai".to_owned(),
            room_url: None,
            effective_from: "2026-01-01".to_owned(),
            effective_until: None,
            enabled: true,
        };
        let (first, created) = store
            .add_weekly_rule_idempotent(&input)
            .await
            .expect("first rule");
        assert!(created);
        let (retry, created_again) = store
            .add_weekly_rule_idempotent(&input)
            .await
            .expect("retry rule");
        assert!(!created_again);
        assert_eq!(first.id, retry.id);
        let updated = store
            .update_weekly_rule(
                first.id,
                &UpdateWeeklyScheduleRule {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect("partial weekly update");
        assert_eq!(updated.title, first.title);
        assert_eq!(updated.weekdays_mask, first.weekdays_mask);
        assert_eq!(updated.start_time, first.start_time);
        assert!(!updated.enabled);
        assert_eq!(updated.revision, first.revision + 1);
        let conflicting = NewWeeklyScheduleRule {
            streamer_id: second_streamer.id,
            ..input
        };
        assert!(store
            .add_weekly_rule_idempotent(&conflicting)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn update_schedule_clears_dangling_weekly_reference() {
        let store = SqliteStore::in_memory().await.expect("db");
        let streamer = store
            .add_subscription(&add_input("205", "悬空周表主播"))
            .await
            .expect("streamer");
        let (schedule, _) = store
            .upsert_schedule(&new_schedule(streamer.id, Utc::now() + Duration::days(1)))
            .await
            .expect("schedule");
        {
            let mut conn = store.pool.acquire().await.expect("connection");
            sqlx::query("PRAGMA foreign_keys = OFF")
                .execute(&mut *conn)
                .await
                .expect("disable foreign keys");
            sqlx::query(
                "UPDATE schedules SET weekly_rule_id = 999999,
                        weekly_occurrence_date = '2026-11-07' WHERE id = ?",
            )
            .bind(schedule.id)
            .execute(&mut *conn)
            .await
            .expect("inject dangling reference");
        }
        let updated = store
            .update_schedule(
                schedule.id,
                &UpdateSchedule {
                    title: Some("清理悬空引用".to_owned()),
                    start_at: None,
                    end_at: None,
                    room_url: None,
                },
            )
            .await
            .expect("update schedule");
        assert_eq!(updated.title, "清理悬空引用");
        assert!(updated.weekly_rule_id.is_none());
        assert!(updated.weekly_occurrence_date.is_none());
    }

    #[tokio::test]
    async fn identical_automatic_schedules_and_sources_do_not_emit_sync_changes() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&add_input("300", "测试"))
            .await
            .unwrap();
        let mut input = new_schedule(streamer.id, Utc::now() + Duration::days(1));
        input.source_identity = Some("image:post:0".to_owned());
        input.source_fingerprint = Some("digest".to_owned());
        let (original, _) = store.upsert_schedule(&input).await.unwrap();
        let seq = store.latest_sync_seq().await.unwrap();
        let (same, created) = store.upsert_schedule(&input).await.unwrap();
        assert!(!created);
        assert_eq!(same.revision, original.revision);
        assert_eq!(same.updated_at, original.updated_at);
        store
            .merge_schedule_auto(original.id, &input)
            .await
            .unwrap();
        store
            .attach_schedule_source(original.id, &input)
            .await
            .unwrap();
        assert_eq!(store.latest_sync_seq().await.unwrap(), seq);
        input.title = "真实变更".to_owned();
        let (changed, _) = store.upsert_schedule(&input).await.unwrap();
        assert_eq!(changed.title, "真实变更");
        assert!(changed.revision > original.revision);
        assert!(store.latest_sync_seq().await.unwrap() > seq);
        store
            .update_schedule(
                original.id,
                &UpdateSchedule {
                    title: Some("人工标题".to_owned()),
                    start_at: None,
                    end_at: None,
                    room_url: None,
                },
            )
            .await
            .unwrap();
        let locked = store.get_schedule(original.id).await.unwrap();
        let seq = store.latest_sync_seq().await.unwrap();
        store.upsert_schedule(&input).await.unwrap();
        assert_eq!(store.latest_sync_seq().await.unwrap(), seq);
        assert_eq!(
            store.get_schedule(original.id).await.unwrap().title,
            locked.title
        );
        input.source_fingerprint = Some("changed-digest".to_owned());
        let (changed, _) = store.upsert_schedule(&input).await.unwrap();
        assert!(changed.source_changed);
        assert_eq!(changed.title, locked.title);
        assert!(store.latest_sync_seq().await.unwrap() > seq);
    }

    #[tokio::test]
    async fn identical_live_summaries_do_not_emit_but_state_changes_do() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&add_input("301", "测试"))
            .await
            .unwrap();
        let now = Utc::now();
        let session = store
            .ensure_live_session(streamer.id, "99", "测试", "直播", None, now, now)
            .await
            .unwrap();
        let mut stats = LiveSessionStats {
            data_quality: "collecting".to_owned(),
            collector_state: "active".to_owned(),
            ..Default::default()
        };
        store
            .update_live_session_stats(session.id, &stats)
            .await
            .unwrap();
        let seq = store.latest_sync_seq().await.unwrap();
        store
            .update_live_session_stats(session.id, &stats)
            .await
            .unwrap();
        assert_eq!(store.latest_sync_seq().await.unwrap(), seq);
        stats.collector_state = "reconnecting".to_owned();
        store
            .update_live_session_stats(session.id, &stats)
            .await
            .unwrap();
        assert!(store.latest_sync_seq().await.unwrap() > seq);
        let seq = store.latest_sync_seq().await.unwrap();
        store
            .finish_live_session(session.id, now + Duration::seconds(60), &stats)
            .await
            .unwrap();
        assert!(store.latest_sync_seq().await.unwrap() > seq);
    }

    #[tokio::test]
    async fn live_session_and_schedule_lookup() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = store
            .add_subscription(&add_input("200", "乃琳"))
            .await
            .expect("streamer");
        let start = Utc::now() - Duration::minutes(5);
        let session = store
            .ensure_live_session(
                streamer.id,
                "12345",
                &streamer.name,
                "测试直播",
                None,
                start,
                Utc::now(),
            )
            .await
            .expect("session");
        let active = store.get_live_session(session.id).await.expect("active");
        assert!(active.ended_at.is_none());
        assert!(active.duration_seconds >= 299);
        let stats = LiveSessionStats {
            peak_high_energy: 987,
            danmaku_count: 25_000,
            interactive_user_count: 4_014,
            paid_user_count: 977,
            revenue_cents: 353_350,
            data_quality: "complete".to_owned(),
            collector_state: "finished".to_owned(),
            ..Default::default()
        };
        store
            .finish_live_session(session.id, start + Duration::hours(1), &stats)
            .await
            .expect("finish");

        let (schedule, _) = store
            .upsert_schedule(&new_schedule(streamer.id, start))
            .await
            .expect("schedule");
        let found = store
            .find_live_session_for_schedule(schedule.id)
            .await
            .expect("lookup")
            .expect("matched session");
        assert_eq!(found.id, session.id);
        assert_eq!(found.peak_high_energy, 987);
        assert_eq!(found.revenue_cents, 353_350);
        assert_eq!(found.danmaku_count, 25_000);
        assert_eq!(found.data_quality, "complete");
    }

    #[tokio::test]
    async fn reopening_finished_live_session_keeps_counters() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = store
            .add_subscription(&add_input("203", "重开主播"))
            .await
            .expect("streamer");
        let start = Utc::now() - Duration::minutes(10);
        let session = store
            .ensure_live_session(
                streamer.id,
                "12345",
                &streamer.name,
                "第一场",
                None,
                start,
                Utc::now(),
            )
            .await
            .expect("session");
        store
            .update_live_session_stats(
                session.id,
                &LiveSessionStats {
                    danmaku_count: 42,
                    revenue_cents: 500,
                    paid_user_count: 3,
                    interactive_user_count: 7,
                    data_quality: "partial".to_owned(),
                    collector_state: "active".to_owned(),
                    ..Default::default()
                },
            )
            .await
            .expect("stats");
        store
            .finish_open_live_session(session.id, start + Duration::minutes(5))
            .await
            .expect("finish");
        let reopened = store
            .ensure_live_session(
                streamer.id,
                "12345",
                &streamer.name,
                "重新开播",
                None,
                start,
                Utc::now(),
            )
            .await
            .expect("reopen");
        assert_eq!(reopened.id, session.id);
        assert!(reopened.ended_at.is_none());
        let stored_duration: i64 =
            sqlx::query_scalar("SELECT duration_seconds FROM live_sessions WHERE id = ?")
                .bind(session.id)
                .fetch_one(&store.pool)
                .await
                .expect("stored duration");
        assert_eq!(stored_duration, 0);
        // 断流重开是同一场：计数继续累加，不能清零。
        assert_eq!(reopened.danmaku_count, 42);
        assert_eq!(reopened.revenue_cents, 500);
        assert_eq!(reopened.interactive_user_count, 7);
        assert_eq!(reopened.segment_count, 2);
        assert_eq!(reopened.data_quality, "partial");
        assert_eq!(reopened.collector_state, "active");
    }

    #[tokio::test]
    async fn stream_restart_with_new_start_time_merges_into_previous_session() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = store
            .add_subscription(&add_input("205", "断流主播"))
            .await
            .expect("streamer");
        let first_start = Utc::now() - Duration::minutes(30);
        let first = store
            .ensure_live_session(
                streamer.id,
                "30849777",
                &streamer.name,
                "【突击】周末愉快～",
                None,
                first_start,
                Utc::now(),
            )
            .await
            .expect("first");
        store
            .update_live_session_stats(
                first.id,
                &LiveSessionStats {
                    danmaku_count: 786,
                    revenue_cents: 21_670,
                    interactive_user_count: 255,
                    paid_user_count: 96,
                    peak_high_energy: 789,
                    data_quality: "partial".to_owned(),
                    collector_state: "active".to_owned(),
                    ..Default::default()
                },
            )
            .await
            .expect("stats");
        store
            .finish_open_live_session(first.id, first_start + Duration::minutes(5))
            .await
            .expect("finish");
        assert!(store
            .latest_open_live_session(streamer.id)
            .await
            .expect("open")
            .is_none());

        // 46 秒后主播重新推流，B 站把 live_time 更新成了新的开播时间。
        let second_start = first_start + Duration::minutes(5) + Duration::seconds(46);
        let merged = store
            .ensure_live_session(
                streamer.id,
                "30849777",
                &streamer.name,
                "【突击】周末愉快～",
                None,
                second_start,
                Utc::now(),
            )
            .await
            .expect("merge");
        assert_eq!(merged.id, first.id, "重开应该并回原来那一场");
        assert_eq!(merged.started_at, first_start);
        assert!(merged.ended_at.is_none());
        assert_eq!(merged.segment_count, 2);
        assert_eq!(merged.danmaku_count, 786);
        assert_eq!(merged.revenue_cents, 21_670);
        assert_eq!(merged.peak_high_energy, 789);

        // 轮询会拿同一个 live_time 反复调用：不能每轮都再并一次。
        let again = store
            .ensure_live_session(
                streamer.id,
                "30849777",
                &streamer.name,
                "【突击】周末愉快～",
                None,
                second_start,
                Utc::now(),
            )
            .await
            .expect("idempotent merge");
        assert_eq!(again.id, first.id);
        assert_eq!(again.segment_count, 2, "同一次重开只能算一段");
        let sessions = store
            .list_live_sessions(Some(streamer.id), 10)
            .await
            .expect("list");
        assert_eq!(sessions.len(), 1, "同一场直播不应该留两条记录");
    }

    #[tokio::test]
    async fn stream_restart_after_merge_window_stays_a_new_session() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = store
            .add_subscription(&add_input("206", "换场主播"))
            .await
            .expect("streamer");
        let first_start = Utc::now() - Duration::minutes(60);
        let first = store
            .ensure_live_session(
                streamer.id,
                "30849777",
                &streamer.name,
                "上午场",
                None,
                first_start,
                Utc::now(),
            )
            .await
            .expect("first");
        store
            .finish_open_live_session(first.id, first_start + Duration::minutes(5))
            .await
            .expect("finish");
        // 间隔超过合并窗口，属于真正的新一场。
        let second = store
            .ensure_live_session(
                streamer.id,
                "30849777",
                &streamer.name,
                "下午场",
                None,
                first_start + Duration::minutes(36),
                Utc::now(),
            )
            .await
            .expect("second");
        assert_ne!(second.id, first.id);
        assert_eq!(second.segment_count, 1);
        let sessions = store
            .list_live_sessions(Some(streamer.id), 10)
            .await
            .expect("list");
        assert_eq!(sessions.len(), 2);
    }
}
