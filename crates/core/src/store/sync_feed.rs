use super::*;
use crate::models::{
    DeviceInfo, LiveSession, ScheduleSource, ServerSettings, Streamer, SyncBootstrap, SyncChange,
    SyncChangesPage, WeeklyScheduleException,
};
use sqlx::{Row, SqliteConnection};

pub const SYNC_SCHEMA_VERSION: u32 = 2;
const SYNC_PAGE_MAX: i64 = 500;
const DEVICE_ONLINE_WINDOW_SECONDS: i64 = 90;
const META_LATEST_SEQ: &str = "latest_seq";
const META_MIN_RETAINED_SEQ: &str = "min_retained_seq";
const SYNC_PRUNE_BATCH_ROWS: i64 = 256;
// MAX(seq) otherwise encourages a rowid scan over large payload pages. Keep
// julianday comparisons for historical timestamps with different UTC offsets.
const SYNC_PRUNE_BEFORE_BOUNDARY_SQL: &str =
    "SELECT MAX(seq) FROM sync_changes INDEXED BY idx_sync_changes_created
     WHERE julianday(created_at) < julianday(?)";

const SYNC_FEED_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sync_changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('upsert', 'delete')),
    entity_revision INTEGER NOT NULL,
    payload_json TEXT,
    actor_type TEXT NOT NULL,
    actor_id TEXT,
    mutation_id TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sync_changes_entity
ON sync_changes(entity_type, entity_id, seq);

CREATE INDEX IF NOT EXISTS idx_sync_changes_created
ON sync_changes(created_at, seq);

CREATE TABLE IF NOT EXISTS sync_feed_meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_feed_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    feed_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS devices (
    device_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    platform TEXT NOT NULL,
    app_version TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    last_ack_seq INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    revoked_at TEXT
);
"#;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SyncActor<'a> {
    actor_type: &'a str,
    actor_id: Option<&'a str>,
    mutation_id: Option<&'a str>,
}

impl SyncActor<'static> {
    pub(crate) const fn system() -> Self {
        Self {
            actor_type: "system",
            actor_id: None,
            mutation_id: None,
        }
    }

    pub(crate) const fn collector() -> Self {
        Self {
            actor_type: "collector",
            actor_id: None,
            mutation_id: None,
        }
    }
}

impl<'a> SyncActor<'a> {
    #[allow(dead_code)]
    pub(crate) const fn device(device_id: &'a str) -> Self {
        Self {
            actor_type: "device",
            actor_id: Some(device_id),
            mutation_id: None,
        }
    }

    pub(crate) const fn with_mutation(mut self, mutation_id: &'a str) -> Self {
        self.mutation_id = Some(mutation_id);
        self
    }
}

pub(crate) async fn initialize_sync_feed(conn: &mut SqliteConnection) -> AppResult<()> {
    sqlx::raw_sql(SYNC_FEED_SCHEMA).execute(&mut *conn).await?;
    sync_feed_id_on(conn).await?;

    for (table, column, definition) in [
        (
            "streamers",
            "revision",
            "ALTER TABLE streamers ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "streamers",
            "live_revision",
            "ALTER TABLE streamers ADD COLUMN live_revision INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "linked_accounts",
            "revision",
            "ALTER TABLE linked_accounts ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "schedules",
            "revision",
            "ALTER TABLE schedules ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "weekly_schedule_rules",
            "revision",
            "ALTER TABLE weekly_schedule_rules ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "weekly_schedule_exceptions",
            "revision",
            "ALTER TABLE weekly_schedule_exceptions ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "schedule_sources",
            "revision",
            "ALTER TABLE schedule_sources ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "source_suppressions",
            "revision",
            "ALTER TABLE source_suppressions ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "settings",
            "revision",
            "ALTER TABLE settings ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "live_sessions",
            "peak_high_energy",
            "ALTER TABLE live_sessions ADD COLUMN peak_high_energy INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "live_sessions",
            "revision",
            "ALTER TABLE live_sessions ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
        ),
        // 主播断流重开后，同一房间、间隔在合并窗口内的场次会并回上一行；这两列记录并了多少段、
        // 最后并进来的是哪一段，避免同一次重开被反复合并。
        (
            "live_sessions",
            "segment_count",
            "ALTER TABLE live_sessions ADD COLUMN segment_count INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "live_sessions",
            "last_segment_started_at",
            "ALTER TABLE live_sessions ADD COLUMN last_segment_started_at TEXT NOT NULL DEFAULT ''",
        ),
    ] {
        add_column_if_missing(conn, table, column, definition).await?;
    }

    for statement in [
        "UPDATE streamers SET revision = 1 WHERE revision IS NULL OR revision < 1",
        "UPDATE linked_accounts SET revision = 1 WHERE revision IS NULL OR revision < 1",
        "UPDATE schedules SET revision = 1 WHERE revision IS NULL OR revision < 1",
        "UPDATE weekly_schedule_rules SET revision = 1 WHERE revision IS NULL OR revision < 1",
        "UPDATE weekly_schedule_exceptions SET revision = 1 WHERE revision IS NULL OR revision < 1",
        "UPDATE schedule_sources SET revision = 1 WHERE revision IS NULL OR revision < 1",
        "UPDATE source_suppressions SET revision = 1 WHERE revision IS NULL OR revision < 1",
        "UPDATE settings SET revision = 1 WHERE revision IS NULL OR revision < 1",
        "UPDATE live_sessions SET revision = 1 WHERE revision IS NULL OR revision < 1",
    ] {
        sqlx::query(statement).execute(&mut *conn).await?;
    }
    let table_latest: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM sync_changes")
        .fetch_one(&mut *conn)
        .await?;
    let table_min: i64 = sqlx::query_scalar("SELECT COALESCE(MIN(seq), 0) FROM sync_changes")
        .fetch_one(&mut *conn)
        .await?;
    advance_feed_meta_on(conn, META_LATEST_SEQ, table_latest).await?;
    advance_feed_meta_on(conn, META_MIN_RETAINED_SEQ, table_min).await?;
    Ok(())
}

/// Controls which rows of `sync_changes` are removed.
///
/// Pruning always removes a contiguous prefix so clients can safely retain a
/// single cursor. `Before` removes changes older than the timestamp, while
/// `KeepLatest` retains at most the requested number of newest changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPrunePolicy {
    Before(DateTime<Utc>),
    KeepLatest(i64),
}

async fn add_column_if_missing(
    conn: &mut SqliteConnection,
    table: &str,
    column: &str,
    definition: &str,
) -> AppResult<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let columns = sqlx::query(&pragma).fetch_all(&mut *conn).await?;
    if !columns
        .iter()
        .any(|row| row.get::<String, _>("name") == column)
    {
        sqlx::query(definition).execute(&mut *conn).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct SyncStreamerRow {
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

#[derive(Debug, Clone, sqlx::FromRow)]
struct SyncLinkedAccountRow {
    id: i64,
    streamer_id: i64,
    mid: String,
    name: String,
    avatar_url: String,
    revision: i64,
    created_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct SyncWeeklyExceptionRow {
    id: i64,
    rule_id: i64,
    occurrence_date: String,
    exception_kind: String,
    schedule_id: Option<i64>,
    revision: i64,
    created_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct SyncScheduleSourceRow {
    id: i64,
    schedule_id: i64,
    source_kind: String,
    source_post_id: Option<String>,
    source_image_url: Option<String>,
    source_fingerprint: Option<String>,
    source_identity: String,
    confidence: f64,
    revision: i64,
    created_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct SyncSuppressionRow {
    id: i64,
    streamer_id: i64,
    source_post_id: String,
    source_fingerprint: String,
    revision: i64,
    created_at: String,
}
impl SqliteStore {
    pub async fn sync_feed_id(&self) -> AppResult<String> {
        let mut conn = self.pool.acquire().await?;
        sync_feed_id_on(&mut conn).await
    }

    pub async fn append_sync_change(
        &self,
        entity_type: &str,
        entity_id: &str,
        operation: &str,
        entity_revision: i64,
        payload: Option<Value>,
    ) -> AppResult<i64> {
        self.append_sync_change_with_actor(
            entity_type,
            entity_id,
            operation,
            entity_revision,
            payload,
            "system",
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn append_sync_change_with_actor(
        &self,
        entity_type: &str,
        entity_id: &str,
        operation: &str,
        entity_revision: i64,
        payload: Option<Value>,
        actor_type: &str,
        actor_id: Option<&str>,
        mutation_id: Option<&str>,
    ) -> AppResult<i64> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let seq = append_sync_change_on(
            &mut tx,
            entity_type,
            entity_id,
            operation,
            entity_revision,
            payload,
            actor_type,
            actor_id,
            mutation_id,
            Utc::now(),
        )
        .await?;
        tx.commit().await?;
        Ok(seq)
    }

    pub async fn latest_sync_seq(&self) -> AppResult<i64> {
        latest_sync_seq_on(&mut *self.pool.acquire().await?).await
    }

    pub async fn min_retained_sync_seq(&self) -> AppResult<i64> {
        min_retained_sync_seq_on(&mut *self.pool.acquire().await?).await
    }

    /// Removes a contiguous prefix of the sync change log and advances the
    /// persistent retention boundary. Clients whose cursor is older than the
    /// new boundary receive `resetRequired` on their next change request.
    pub async fn prune_sync_changes(&self, policy: SyncPrunePolicy) -> AppResult<u64> {
        let started = std::time::Instant::now();
        // Boundary discovery is read-only: even a long scan must not hold the
        // single SQLite writer slot needed by schedules and live checkpoints.
        let boundary: Option<i64> = match policy {
            SyncPrunePolicy::Before(before_created_at) => {
                sqlx::query_scalar(SYNC_PRUNE_BEFORE_BOUNDARY_SQL)
                    .bind(before_created_at.to_rfc3339())
                    .fetch_one(&self.pool)
                    .await?
            }
            SyncPrunePolicy::KeepLatest(max_rows) => {
                if max_rows < 0 {
                    return Err(AppError::validation("同步日志保留条数不能为负数"));
                }
                sqlx::query_scalar(
                    "SELECT seq FROM sync_changes ORDER BY seq DESC LIMIT 1 OFFSET ?",
                )
                .bind(max_rows)
                .fetch_optional(&self.pool)
                .await?
            }
        };
        let boundary_ms = started.elapsed().as_millis() as u64;
        if boundary_ms >= 500 {
            tracing::warn!(?policy, boundary_ms, "同步日志清理边界查询较慢（未持写锁）");
        }
        let Some(boundary) = boundary else {
            tracing::info!(?policy, boundary_ms, deleted = 0, "同步日志无需清理");
            return Ok(0);
        };
        let mut deleted = 0u64;
        let mut batches = 0u64;
        loop {
            let waiting = std::time::Instant::now();
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            let lock_wait_ms = waiting.elapsed().as_millis() as u64;
            let transaction_started = std::time::Instant::now();
            let batch_boundary: Option<i64> = sqlx::query_scalar(
                "SELECT MAX(seq) FROM (
                     SELECT seq FROM sync_changes WHERE seq <= ? ORDER BY seq LIMIT ?
                 )",
            )
            .bind(boundary)
            .bind(SYNC_PRUNE_BATCH_ROWS)
            .fetch_one(&mut *tx)
            .await?;
            let Some(batch_boundary) = batch_boundary else {
                tx.commit().await?;
                break;
            };
            let batch_deleted = sqlx::query("DELETE FROM sync_changes WHERE seq <= ?")
                .bind(batch_boundary)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            // A client can observe between batches. The removed prefix and its
            // reset boundary must therefore be committed in the SAME transaction.
            advance_feed_meta_on(
                &mut tx,
                META_MIN_RETAINED_SEQ,
                batch_boundary.saturating_add(1),
            )
            .await?;
            let latest = latest_sync_seq_on(&mut tx).await?;
            advance_feed_meta_on(&mut tx, META_LATEST_SEQ, latest).await?;
            tx.commit().await?;
            deleted += batch_deleted;
            batches += 1;
            let transaction_ms = transaction_started.elapsed().as_millis() as u64;
            if lock_wait_ms >= 500 || transaction_ms >= 500 {
                tracing::warn!(
                    lock_wait_ms,
                    transaction_ms,
                    batch_deleted,
                    "同步日志清理批次较慢"
                );
            }
            if batch_boundary >= boundary {
                break;
            }
            // Give pending application writes a chance to acquire the lock.
            // The target boundary is fixed, so concurrent appends are retained.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tracing::info!(
            ?policy,
            deleted,
            batches,
            boundary_ms,
            total_ms = started.elapsed().as_millis() as u64,
            "同步日志清理完成"
        );
        Ok(deleted)
    }

    pub async fn sync_changes_after(&self, after: i64, limit: i64) -> AppResult<SyncChangesPage> {
        if after < 0 {
            return Err(AppError::validation("同步游标不能为负数"));
        }
        let limit = limit.clamp(1, SYNC_PAGE_MAX);
        let mut tx = self.pool.begin().await?;
        let feed_id = sync_feed_id_on(&mut tx).await?;
        let latest_seq = latest_sync_seq_on(&mut tx).await?;
        let min_retained_seq = min_retained_sync_seq_on(&mut tx).await?;
        if after < min_retained_seq.saturating_sub(1) || after > latest_seq {
            tx.commit().await?;
            return Ok(SyncChangesPage {
                schema_version: SYNC_SCHEMA_VERSION,
                feed_id,
                from_seq: after.saturating_add(1),
                to_seq: after,
                latest_seq,
                min_retained_seq,
                has_more: false,
                reset_required: true,
                changes: Vec::new(),
            });
        }
        let rows = sqlx::query(
            "SELECT seq, entity_type, entity_id, operation, entity_revision,
                    payload_json, created_at
             FROM sync_changes WHERE seq > ? ORDER BY seq LIMIT ?",
        )
        .bind(after)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let changes = rows
            .into_iter()
            .map(map_sync_change)
            .collect::<AppResult<Vec<_>>>()?;
        // Seq values are a contiguous log contract. A missing row in the
        // requested range means a client must rebuild from a snapshot rather
        // than silently skip data. This also covers a meta/latest cursor that
        // is ahead of the physical table after an interrupted/manual repair.
        let has_gap = changes
            .windows(2)
            .any(|pair| pair[1].seq != pair[0].seq.saturating_add(1));
        let starts_after_cursor = changes
            .first()
            .is_none_or(|change| change.seq == after.saturating_add(1));
        let empty_before_latest = changes.is_empty() && after < latest_seq;
        if has_gap || !starts_after_cursor || empty_before_latest {
            tx.commit().await?;
            return Ok(SyncChangesPage {
                schema_version: SYNC_SCHEMA_VERSION,
                feed_id,
                from_seq: after.saturating_add(1),
                to_seq: after,
                latest_seq,
                min_retained_seq,
                has_more: false,
                reset_required: true,
                changes: Vec::new(),
            });
        }

        let to_seq = changes.last().map_or(after, |change| change.seq);
        tx.commit().await?;
        Ok(SyncChangesPage {
            schema_version: SYNC_SCHEMA_VERSION,
            feed_id,
            from_seq: after.saturating_add(1),
            to_seq,
            latest_seq,
            min_retained_seq,
            has_more: to_seq < latest_seq,
            reset_required: false,
            changes,
        })
    }

    pub async fn sync_bootstrap(&self) -> AppResult<SyncBootstrap> {
        let mut tx = self.pool.begin().await?;
        let feed_id = sync_feed_id_on(&mut tx).await?;
        let snapshot_revision = latest_sync_seq_on(&mut tx).await?;
        let min_retained_seq = min_retained_sync_seq_on(&mut tx).await?;
        let subscriptions = load_streamers(&mut tx).await?;
        let schedules = load_schedules(&mut tx).await?;
        let weekly_rules = load_weekly_rules(&mut tx).await?;
        let weekly_exceptions = load_weekly_exceptions(&mut tx).await?;
        let schedule_sources = load_schedule_sources(&mut tx).await?;
        let source_suppressions = load_source_suppressions(&mut tx).await?;
        let server_settings = load_server_settings(&mut tx).await?;
        let live_statuses = load_live_statuses(&mut tx).await?;
        let live_sessions = load_live_sessions(&mut tx).await?;
        tx.commit().await?;
        Ok(SyncBootstrap {
            schema_version: SYNC_SCHEMA_VERSION,
            feed_id,
            snapshot_revision,
            min_retained_seq,
            server_time: Utc::now(),
            subscriptions,
            schedules,
            weekly_rules,
            weekly_exceptions,
            schedule_sources,
            source_suppressions,
            server_settings,
            live_statuses,
            live_sessions,
        })
    }

    pub async fn register_device(
        &self,
        device_id: &str,
        name: &str,
        platform: &str,
        app_version: &str,
    ) -> AppResult<DeviceInfo> {
        validate_device_input(device_id, name, platform, app_version)?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO devices
                (device_id, name, platform, app_version, last_seen_at, last_ack_seq, created_at)
             VALUES (?, ?, ?, ?, ?, 0, ?)
             ON CONFLICT(device_id) DO UPDATE SET
                name = excluded.name,
                platform = excluded.platform,
                app_version = excluded.app_version,
                last_seen_at = excluded.last_seen_at",
        )
        .bind(device_id)
        .bind(name.trim())
        .bind(platform.trim())
        .bind(app_version.trim())
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        self.get_device(device_id).await
    }

    pub async fn upsert_device(
        &self,
        device_id: &str,
        name: &str,
        platform: &str,
        app_version: &str,
    ) -> AppResult<DeviceInfo> {
        self.register_device(device_id, name, platform, app_version)
            .await
    }

    pub async fn get_device(&self, device_id: &str) -> AppResult<DeviceInfo> {
        let row = sqlx::query(
            "SELECT device_id, name, platform, app_version, last_seen_at, last_ack_seq
             FROM devices WHERE device_id = ? AND revoked_at IS NULL",
        )
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::not_found(format!("设备 {device_id}")))?;
        map_device(row)
    }

    pub async fn list_devices(&self) -> AppResult<Vec<DeviceInfo>> {
        let rows = sqlx::query(
            "SELECT device_id, name, platform, app_version, last_seen_at, last_ack_seq
             FROM devices WHERE revoked_at IS NULL ORDER BY last_seen_at DESC, device_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_device).collect()
    }

    pub async fn delete_device_log(&self, device_id: &str) -> AppResult<()> {
        let cutoff =
            (Utc::now() - chrono::Duration::seconds(DEVICE_ONLINE_WINDOW_SECONDS)).to_rfc3339();
        let deleted = sqlx::query(
            "DELETE FROM devices
             WHERE device_id = ? AND revoked_at IS NULL
               AND julianday(last_seen_at) <= julianday(?)",
        )
        .bind(device_id)
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        if deleted.rows_affected() > 0 {
            return Ok(());
        }

        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM devices WHERE device_id = ? AND revoked_at IS NULL",
        )
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        if exists {
            return Err(AppError::validation("仅可删除离线设备的日志"));
        }
        Err(AppError::not_found(format!("设备 {device_id}")))
    }

    pub async fn ack_device(&self, device_id: &str, seq: i64) -> AppResult<DeviceInfo> {
        let device_id = device_id.trim();
        if device_id.is_empty() || device_id.chars().count() > 128 {
            return Err(AppError::validation("设备 ID 长度必须为 1 到 128 字"));
        }
        if seq < 0 {
            return Err(AppError::validation("设备确认序号不能为负数"));
        }

        // Return the same snapshot that persisted the ACK. Reading through the
        // pool after the UPDATE allowed a concurrent ACK/re-registration to
        // change the response between write and read.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = Utc::now().to_rfc3339();
        let updated = sqlx::query(
            "UPDATE devices SET last_seen_at = ?, last_ack_seq = MAX(last_ack_seq, ?)
             WHERE device_id = ? AND revoked_at IS NULL",
        )
        .bind(&now)
        .bind(seq)
        .bind(device_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::not_found(format!("设备 {device_id}")));
        }
        let row = sqlx::query(
            "SELECT device_id, name, platform, app_version, last_seen_at, last_ack_seq
             FROM devices WHERE device_id = ? AND revoked_at IS NULL",
        )
        .bind(device_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::not_found(format!("设备 {device_id}")))?;
        let device = map_device(row)?;
        tx.commit().await?;
        Ok(device)
    }
    pub(crate) async fn emit_streamer_change(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        operation: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let revision: i64 = sqlx::query_scalar("SELECT revision FROM streamers WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
        let payload = if operation == "upsert" {
            Some(serde_json::to_value(load_streamer(conn, id).await?)?)
        } else {
            None
        };
        append_sync_change_on(
            conn,
            "subscription",
            &id.to_string(),
            operation,
            revision,
            payload,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_linked_account_change(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        operation: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let row = load_linked_account_by_id(conn, id).await?;
        let payload = if operation == "upsert" {
            Some(linked_account_payload(&row)?)
        } else {
            None
        };
        // Keep linked-account entity IDs stable across mirror rebuilds.
        let entity_id = format!("{}:{}", row.streamer_id, row.mid);
        append_sync_change_on(
            conn,
            "linked_account",
            &entity_id,
            operation,
            row.revision,
            payload,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_schedule_change(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        operation: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let revision: i64 = sqlx::query_scalar("SELECT revision FROM schedules WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
        let payload = if operation == "upsert" {
            Some(serde_json::to_value(load_schedule(conn, id).await?)?)
        } else {
            None
        };
        append_sync_change_on(
            conn,
            "schedule",
            &id.to_string(),
            operation,
            revision,
            payload,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_weekly_rule_change(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        operation: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let revision: i64 =
            sqlx::query_scalar("SELECT revision FROM weekly_schedule_rules WHERE id = ?")
                .bind(id)
                .fetch_one(&mut *conn)
                .await?;
        let payload = if operation == "upsert" {
            Some(serde_json::to_value(load_weekly_rule(conn, id).await?)?)
        } else {
            None
        };
        append_sync_change_on(
            conn,
            "weekly_rule",
            &id.to_string(),
            operation,
            revision,
            payload,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_weekly_exception_change(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        operation: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let row = load_weekly_exception_by_id(conn, id).await?;
        let payload = if operation == "upsert" {
            Some(serde_json::to_value(map_weekly_exception_row(
                row.clone(),
            )?)?)
        } else {
            None
        };
        append_sync_change_on(
            conn,
            "weekly_exception",
            &id.to_string(),
            operation,
            row.revision,
            payload,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_schedule_source_change(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        operation: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let row = load_schedule_source_by_id(conn, id).await?;
        let payload = if operation == "upsert" {
            Some(serde_json::to_value(map_schedule_source_row(row.clone())?)?)
        } else {
            None
        };
        append_sync_change_on(
            conn,
            "schedule_source",
            &id.to_string(),
            operation,
            row.revision,
            payload,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_source_suppression_change(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        operation: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let row = load_source_suppression_by_id(conn, id).await?;
        let payload = if operation == "upsert" {
            Some(serde_json::to_value(map_source_suppression_row(
                row.clone(),
            )?)?)
        } else {
            None
        };
        append_sync_change_on(
            conn,
            "source_suppression",
            &id.to_string(),
            operation,
            row.revision,
            payload,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_live_status_change(
        &self,
        conn: &mut SqliteConnection,
        streamer_id: i64,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let status = load_live_status(conn, streamer_id).await?;
        let revision = status.revision;
        append_sync_change_on(
            conn,
            "live_status",
            &streamer_id.to_string(),
            "upsert",
            revision,
            Some(serde_json::to_value(status)?),
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_live_status_delete(
        &self,
        conn: &mut SqliteConnection,
        streamer_id: i64,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let revision: i64 = sqlx::query_scalar("SELECT live_revision FROM streamers WHERE id = ?")
            .bind(streamer_id)
            .fetch_one(&mut *conn)
            .await?;
        append_sync_change_on(
            conn,
            "live_status",
            &streamer_id.to_string(),
            "delete",
            revision,
            None,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_live_session_change(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        operation: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let session = load_live_session_by_id(conn, id).await?;
        let revision = session.revision;
        let payload = if operation == "upsert" {
            Some(serde_json::to_value(session)?)
        } else {
            None
        };
        append_sync_change_on(
            conn,
            "live_session",
            &id.to_string(),
            operation,
            revision,
            payload,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_live_session_delete(
        &self,
        conn: &mut SqliteConnection,
        id: i64,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let revision: i64 = sqlx::query_scalar("SELECT revision FROM live_sessions WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
        append_sync_change_on(
            conn,
            "live_session",
            &id.to_string(),
            "delete",
            revision,
            None,
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }

    pub(crate) async fn emit_server_settings_change(
        &self,
        conn: &mut SqliteConnection,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        let settings = load_server_settings(conn).await?;
        let revision = settings.revision;
        append_sync_change_on(
            conn,
            "server_settings",
            "server_settings",
            "upsert",
            revision,
            Some(serde_json::to_value(settings)?),
            actor.actor_type,
            actor.actor_id,
            actor.mutation_id,
            Utc::now(),
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn append_sync_change_on(
    conn: &mut SqliteConnection,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    entity_revision: i64,
    payload: Option<Value>,
    actor_type: &str,
    actor_id: Option<&str>,
    mutation_id: Option<&str>,
    created_at: DateTime<Utc>,
) -> AppResult<i64> {
    if entity_type.trim().is_empty() || entity_id.trim().is_empty() {
        return Err(AppError::validation("同步实体类型和 ID 不能为空"));
    }
    if !matches!(operation, "upsert" | "delete") {
        return Err(AppError::validation("同步操作必须是 upsert 或 delete"));
    }
    if entity_revision < 0 {
        return Err(AppError::validation("同步实体版本不能为负数"));
    }
    if operation == "upsert" && payload.is_none() {
        return Err(AppError::validation("同步 upsert 必须包含完整实体数据"));
    }
    if actor_type.trim().is_empty() {
        return Err(AppError::validation("同步操作者类型不能为空"));
    }
    let payload_json = payload.as_ref().map(serde_json::to_string).transpose()?;
    let result = sqlx::query(
        "INSERT INTO sync_changes
            (entity_type, entity_id, operation, entity_revision, payload_json,
             actor_type, actor_id, mutation_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(entity_type.trim())
    .bind(entity_id.trim())
    .bind(operation)
    .bind(entity_revision)
    .bind(payload_json)
    .bind(actor_type.trim())
    .bind(actor_id)
    .bind(mutation_id)
    .bind(created_at.to_rfc3339())
    .execute(&mut *conn)
    .await?;
    let seq = result.last_insert_rowid();
    advance_feed_meta_on(conn, META_LATEST_SEQ, seq).await?;
    Ok(seq)
}

async fn latest_sync_seq_on(conn: &mut SqliteConnection) -> AppResult<i64> {
    let table_latest: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM sync_changes")
        .fetch_one(&mut *conn)
        .await?;
    let meta_latest = feed_meta_on(conn, META_LATEST_SEQ).await?.unwrap_or(0);
    Ok(table_latest.max(meta_latest))
}

async fn min_retained_sync_seq_on(conn: &mut SqliteConnection) -> AppResult<i64> {
    let table_min: Option<i64> = sqlx::query_scalar("SELECT MIN(seq) FROM sync_changes")
        .fetch_one(&mut *conn)
        .await?;
    let meta_min = feed_meta_on(conn, META_MIN_RETAINED_SEQ)
        .await?
        .unwrap_or(0);
    Ok(match table_min {
        Some(table_min) => table_min.max(meta_min),
        None => meta_min,
    })
}

async fn sync_feed_id_on(conn: &mut SqliteConnection) -> AppResult<String> {
    if let Some(feed_id) = sqlx::query_scalar::<_, String>(
        "SELECT feed_id FROM sync_feed_identity WHERE singleton = 1",
    )
    .fetch_optional(&mut *conn)
    .await?
    .filter(|value| !value.trim().is_empty())
    {
        return Ok(feed_id);
    }

    let feed_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO sync_feed_identity(singleton, feed_id, created_at) VALUES(1, ?, ?)
         ON CONFLICT(singleton) DO UPDATE SET
             feed_id = CASE
                 WHEN TRIM(sync_feed_identity.feed_id) = '' THEN excluded.feed_id
                 ELSE sync_feed_identity.feed_id
             END",
    )
    .bind(&feed_id)
    .bind(Utc::now().to_rfc3339())
    .execute(&mut *conn)
    .await?;
    sqlx::query_scalar("SELECT feed_id FROM sync_feed_identity WHERE singleton = 1")
        .fetch_one(&mut *conn)
        .await
        .map_err(AppError::from)
}

async fn feed_meta_on(conn: &mut SqliteConnection, key: &str) -> AppResult<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT value FROM sync_feed_meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

async fn advance_feed_meta_on(conn: &mut SqliteConnection, key: &str, value: i64) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO sync_feed_meta (key, value, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET
             value = MAX(sync_feed_meta.value, excluded.value),
             updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(Utc::now().to_rfc3339())
    .execute(&mut *conn)
    .await?;
    Ok(())
}
fn map_sync_change(row: sqlx::sqlite::SqliteRow) -> AppResult<SyncChange> {
    let payload_json: Option<String> = row.try_get("payload_json")?;
    Ok(SyncChange {
        seq: row.try_get("seq")?,
        entity_type: row.try_get("entity_type")?,
        entity_id: row.try_get("entity_id")?,
        operation: row.try_get("operation")?,
        entity_revision: row.try_get("entity_revision")?,
        payload: payload_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        created_at: parse_ts(row.try_get::<String, _>("created_at")?.as_str())?,
    })
}

fn validate_device_input(
    device_id: &str,
    name: &str,
    platform: &str,
    app_version: &str,
) -> AppResult<()> {
    if device_id.trim().is_empty() || device_id.chars().count() > 128 {
        return Err(AppError::validation("设备 ID 长度必须为 1 到 128 字"));
    }
    if name.trim().is_empty() || name.chars().count() > 80 {
        return Err(AppError::validation("设备名称长度必须为 1 到 80 字"));
    }
    if platform.trim().is_empty() || platform.chars().count() > 32 {
        return Err(AppError::validation("设备平台长度必须为 1 到 32 字"));
    }
    if app_version.trim().is_empty() || app_version.chars().count() > 64 {
        return Err(AppError::validation("应用版本长度必须为 1 到 64 字"));
    }
    Ok(())
}

fn map_device(row: sqlx::sqlite::SqliteRow) -> AppResult<DeviceInfo> {
    let last_seen_at = parse_ts(row.try_get::<String, _>("last_seen_at")?.as_str())?;
    Ok(DeviceInfo {
        device_id: row.try_get("device_id")?,
        name: row.try_get("name")?,
        platform: row.try_get("platform")?,
        app_version: row.try_get("app_version")?,
        last_seen_at,
        last_ack_seq: row.try_get("last_ack_seq")?,
        online: Utc::now().signed_duration_since(last_seen_at).num_seconds()
            <= DEVICE_ONLINE_WINDOW_SECONDS,
    })
}

async fn load_streamers(conn: &mut SqliteConnection) -> AppResult<Vec<Streamer>> {
    let rows = sqlx::query_as::<_, SyncStreamerRow>(
        "SELECT id, mid, name, avatar_url, room_id, live_url, schedule_enabled,
                live_notify_enabled, live_stats_enabled, fans, aliases_json, last_error,
                last_refresh_at, live_is_live, live_title, live_cover, live_started_at,
                live_checked_at, sort_order, created_at, revision
         FROM streamers ORDER BY sort_order, name COLLATE NOCASE, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let links = sqlx::query_as::<_, SyncLinkedAccountRow>(
        "SELECT id, streamer_id, mid, name, avatar_url, revision, created_at
         FROM linked_accounts ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut by_streamer: std::collections::HashMap<i64, Vec<LinkedAccount>> =
        std::collections::HashMap::new();
    for link in links {
        let account = map_linked_account_row(link)?;
        by_streamer
            .entry(account.streamer_id)
            .or_default()
            .push(account);
    }
    rows.into_iter()
        .map(|row| {
            let linked_accounts = by_streamer.remove(&row.id).unwrap_or_default();
            map_streamer_row(row, linked_accounts)
        })
        .collect()
}

async fn load_streamer(conn: &mut SqliteConnection, id: i64) -> AppResult<Streamer> {
    let row = sqlx::query_as::<_, SyncStreamerRow>(
        "SELECT id, mid, name, avatar_url, room_id, live_url, schedule_enabled,
                live_notify_enabled, live_stats_enabled, fans, aliases_json, last_error,
                last_refresh_at, live_is_live, live_title, live_cover, live_started_at,
                live_checked_at, sort_order, created_at, revision
         FROM streamers WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found(format!("主播 {id}")))?;
    let links = sqlx::query_as::<_, SyncLinkedAccountRow>(
        "SELECT id, streamer_id, mid, name, avatar_url, revision, created_at
         FROM linked_accounts WHERE streamer_id = ? ORDER BY name COLLATE NOCASE",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let linked_accounts = links
        .into_iter()
        .map(map_linked_account_row)
        .collect::<AppResult<Vec<_>>>()?;
    map_streamer_row(row, linked_accounts)
}

fn map_streamer_row(
    row: SyncStreamerRow,
    linked_accounts: Vec<LinkedAccount>,
) -> AppResult<Streamer> {
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
        linked_accounts,
        sort_order: row.sort_order,
        revision: row.revision,
        created_at: parse_ts(&row.created_at)?,
    })
}

async fn load_linked_account_by_id(
    conn: &mut SqliteConnection,
    id: i64,
) -> AppResult<SyncLinkedAccountRow> {
    sqlx::query_as::<_, SyncLinkedAccountRow>(
        "SELECT id, streamer_id, mid, name, avatar_url, revision, created_at
         FROM linked_accounts WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found(format!("官号关联 {id}")))
}

fn map_linked_account_row(row: SyncLinkedAccountRow) -> AppResult<LinkedAccount> {
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

fn linked_account_payload(row: &SyncLinkedAccountRow) -> AppResult<Value> {
    Ok(json!({
        "id": row.id,
        "streamerId": row.streamer_id,
        "mid": row.mid,
        "name": row.name,
        "avatarUrl": row.avatar_url,
        "revision": row.revision,
        "createdAt": parse_ts(&row.created_at)?,
    }))
}
async fn load_schedules(conn: &mut SqliteConnection) -> AppResult<Vec<Schedule>> {
    let rows = sqlx::query_as::<_, ScheduleRow>(
        "SELECT s.id, s.streamer_id, st.name AS streamer_name, st.avatar_url AS streamer_avatar,
                s.participants_json, s.title, s.start_at, s.end_at, s.room_url,
                s.room_owner_mid, s.room_owner_name, s.source_kind, s.source_post_id,
                s.source_image_url, s.source_author_mid, s.source_fingerprint, s.source_identity,
                s.source_published_at, s.weekly_rule_id, s.weekly_occurrence_date, s.confidence,
                s.manual_locked, s.source_changed, s.cancelled, s.created_at, s.updated_at,
                s.revision
         FROM schedules s JOIN streamers st ON st.id = s.streamer_id
         ORDER BY s.start_at, s.id",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(map_schedule).collect()
}

async fn load_schedule(conn: &mut SqliteConnection, id: i64) -> AppResult<Schedule> {
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

async fn load_weekly_rules(conn: &mut SqliteConnection) -> AppResult<Vec<WeeklyScheduleRule>> {
    let rows = sqlx::query_as::<_, WeeklyScheduleRuleRow>(
        "SELECT id, rule_key, streamer_id, title, weekdays_mask, start_time, end_time,
                timezone, room_url, effective_from, effective_until, enabled, created_at,
                updated_at, revision
         FROM weekly_schedule_rules ORDER BY streamer_id, start_time, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(map_weekly_rule).collect()
}

async fn load_weekly_rule(conn: &mut SqliteConnection, id: i64) -> AppResult<WeeklyScheduleRule> {
    let row = sqlx::query_as::<_, WeeklyScheduleRuleRow>(
        "SELECT id, rule_key, streamer_id, title, weekdays_mask, start_time, end_time,
                timezone, room_url, effective_from, effective_until, enabled, created_at,
                updated_at, revision
         FROM weekly_schedule_rules WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found(format!("固定周表 {id}")))?;
    map_weekly_rule(row)
}

async fn load_weekly_exceptions(
    conn: &mut SqliteConnection,
) -> AppResult<Vec<WeeklyScheduleException>> {
    let rows = sqlx::query_as::<_, SyncWeeklyExceptionRow>(
        "SELECT id, rule_id, occurrence_date, exception_kind, schedule_id, revision, created_at
         FROM weekly_schedule_exceptions ORDER BY rule_id, occurrence_date",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(map_weekly_exception_row).collect()
}

async fn load_weekly_exception_by_id(
    conn: &mut SqliteConnection,
    id: i64,
) -> AppResult<SyncWeeklyExceptionRow> {
    sqlx::query_as::<_, SyncWeeklyExceptionRow>(
        "SELECT id, rule_id, occurrence_date, exception_kind, schedule_id, revision, created_at
         FROM weekly_schedule_exceptions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found(format!("周表例外 {id}")))
}

fn map_weekly_exception_row(row: SyncWeeklyExceptionRow) -> AppResult<WeeklyScheduleException> {
    Ok(WeeklyScheduleException {
        id: row.id,
        rule_id: row.rule_id,
        occurrence_date: row.occurrence_date,
        exception_kind: row.exception_kind,
        schedule_id: row.schedule_id,
        revision: row.revision,
        created_at: parse_ts(&row.created_at)?,
    })
}

async fn load_schedule_sources(conn: &mut SqliteConnection) -> AppResult<Vec<ScheduleSource>> {
    let rows = sqlx::query_as::<_, SyncScheduleSourceRow>(
        "SELECT id, schedule_id, source_kind, source_post_id, source_image_url,
                source_fingerprint, source_identity, confidence, revision, created_at
         FROM schedule_sources ORDER BY schedule_id, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(map_schedule_source_row).collect()
}

async fn load_schedule_source_by_id(
    conn: &mut SqliteConnection,
    id: i64,
) -> AppResult<SyncScheduleSourceRow> {
    sqlx::query_as::<_, SyncScheduleSourceRow>(
        "SELECT id, schedule_id, source_kind, source_post_id, source_image_url,
                source_fingerprint, source_identity, confidence, revision, created_at
         FROM schedule_sources WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found(format!("日程来源 {id}")))
}

fn map_schedule_source_row(row: SyncScheduleSourceRow) -> AppResult<ScheduleSource> {
    Ok(ScheduleSource {
        id: row.id,
        schedule_id: row.schedule_id,
        source_kind: row.source_kind,
        source_post_id: row.source_post_id,
        source_image_url: row.source_image_url,
        source_fingerprint: row.source_fingerprint,
        source_identity: row.source_identity,
        confidence: row.confidence,
        revision: row.revision,
        created_at: parse_ts(&row.created_at)?,
    })
}
async fn load_source_suppressions(
    conn: &mut SqliteConnection,
) -> AppResult<Vec<SourceSuppression>> {
    let rows = sqlx::query_as::<_, SyncSuppressionRow>(
        "SELECT id, streamer_id, source_post_id, source_fingerprint, revision, created_at
         FROM source_suppressions ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(map_source_suppression_row).collect()
}

async fn load_source_suppression_by_id(
    conn: &mut SqliteConnection,
    id: i64,
) -> AppResult<SyncSuppressionRow> {
    sqlx::query_as::<_, SyncSuppressionRow>(
        "SELECT id, streamer_id, source_post_id, source_fingerprint, revision, created_at
         FROM source_suppressions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found(format!("来源抑制 {id}")))
}

fn map_source_suppression_row(row: SyncSuppressionRow) -> AppResult<SourceSuppression> {
    Ok(SourceSuppression {
        id: row.id,
        streamer_id: row.streamer_id,
        source_post_id: row.source_post_id,
        source_fingerprint: row.source_fingerprint,
        revision: row.revision,
        created_at: parse_ts(&row.created_at)?,
    })
}

async fn load_server_settings(conn: &mut SqliteConnection) -> AppResult<ServerSettings> {
    let rows = sqlx::query("SELECT key, value, revision FROM settings")
        .fetch_all(&mut *conn)
        .await?;
    let mut settings = ServerSettings::default();
    let mut revision = 1_i64;
    for row in rows {
        let key: String = row.try_get("key")?;
        if !matches!(
            key.as_str(),
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
        ) {
            continue;
        }
        let value: String = row.try_get("value")?;
        revision = revision.max(row.try_get::<i64, _>("revision")?);
        match key.as_str() {
            "live_poll_seconds" => settings.live_poll_seconds = serde_json::from_str(&value)?,
            "dynamic_poll_seconds" => settings.dynamic_poll_seconds = serde_json::from_str(&value)?,
            "auto_analyze_images" => settings.auto_analyze_images = serde_json::from_str(&value)?,
            "live_stats_enabled" => settings.live_stats_enabled = serde_json::from_str(&value)?,
            "fertility_enabled" => settings.fertility_enabled = serde_json::from_str(&value)?,
            "llm_base_url" => settings.llm_base_url = serde_json::from_str(&value)?,
            "llm_model" => settings.llm_model = serde_json::from_str(&value)?,
            "llm_prompt_version" => settings.llm_prompt_version = serde_json::from_str(&value)?,
            "llm_reasoning_effort" => settings.llm_reasoning_effort = serde_json::from_str(&value)?,
            "llm_parallelism" => settings.llm_parallelism = serde_json::from_str(&value)?,
            "proxy_port" => settings.proxy_port = serde_json::from_str(&value)?,
            _ => {}
        }
    }
    settings.revision = revision;
    Ok(settings)
}

async fn load_live_statuses(conn: &mut SqliteConnection) -> AppResult<Vec<LiveStatus>> {
    let rows = sqlx::query(
        "SELECT id, mid, name, avatar_url, live_is_live, live_title, live_cover, live_url,
                live_checked_at, live_started_at, live_notify_enabled, live_revision
         FROM streamers ORDER BY name COLLATE NOCASE",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(map_live_status_row).collect()
}

async fn load_live_status(conn: &mut SqliteConnection, streamer_id: i64) -> AppResult<LiveStatus> {
    let row = sqlx::query(
        "SELECT id, mid, name, avatar_url, live_is_live, live_title, live_cover, live_url,
                live_checked_at, live_started_at, live_notify_enabled, live_revision
         FROM streamers WHERE id = ?",
    )
    .bind(streamer_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found(format!("主播 {streamer_id}")))?;
    map_live_status_row(row)
}

fn map_live_status_row(row: sqlx::sqlite::SqliteRow) -> AppResult<LiveStatus> {
    Ok(LiveStatus {
        streamer_id: row.try_get("id")?,
        mid: row.try_get("mid")?,
        streamer_name: row.try_get("name")?,
        avatar_url: row.try_get("avatar_url")?,
        is_live: row.try_get::<i64, _>("live_is_live")? != 0,
        title: row
            .try_get::<Option<String>, _>("live_title")?
            .unwrap_or_default(),
        cover_url: row.try_get("live_cover")?,
        live_url: row.try_get("live_url")?,
        online: 0,
        started_at: parse_opt_ts(
            row.try_get::<Option<String>, _>("live_started_at")?
                .as_deref(),
        )?,
        checked_at: parse_opt_ts(
            row.try_get::<Option<String>, _>("live_checked_at")?
                .as_deref(),
        )?
        .unwrap_or_else(Utc::now),
        live_notify_enabled: row.try_get::<i64, _>("live_notify_enabled")? != 0,
        revision: row.try_get("live_revision")?,
    })
}

async fn load_live_sessions(conn: &mut SqliteConnection) -> AppResult<Vec<LiveSession>> {
    let rows = sqlx::query_as::<_, LiveSessionRow>(
        "SELECT id, streamer_id, streamer_name, room_id, title, cover_url, started_at, ended_at,
                duration_seconds, peak_online, peak_high_energy, avg_online, online_samples, online_total,
                revenue_cents, gift_revenue_cents, super_chat_revenue_cents, guard_revenue_cents,
                paid_user_count, interactive_user_count, danmaku_count, gift_count,
                super_chat_count, guard_count, reconnect_count, segment_count, data_quality, collector_state,
                created_at, updated_at, revision
         FROM live_sessions ORDER BY started_at DESC, id DESC",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(map_live_session).collect()
}

async fn load_live_session_by_id(conn: &mut SqliteConnection, id: i64) -> AppResult<LiveSession> {
    let row = sqlx::query_as::<_, LiveSessionRow>(
        "SELECT id, streamer_id, streamer_name, room_id, title, cover_url, started_at, ended_at,
                duration_seconds, peak_online, peak_high_energy, avg_online, online_samples, online_total,
                revenue_cents, gift_revenue_cents, super_chat_revenue_cents, guard_revenue_cents,
                paid_user_count, interactive_user_count, danmaku_count, gift_count,
                super_chat_count, guard_count, reconnect_count, segment_count, data_quality, collector_state,
                created_at, updated_at, revision
         FROM live_sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::not_found(format!("直播数据 {id}")))?;
    map_live_session(row)
}
impl SqliteStore {
    pub(crate) async fn attach_schedule_source_on(
        &self,
        conn: &mut SqliteConnection,
        schedule_id: i64,
        input: &NewSchedule,
        actor: SyncActor<'_>,
    ) -> AppResult<Option<i64>> {
        let Some(identity) = input.source_identity.as_deref() else {
            return Ok(None);
        };
        sqlx::query(
            "INSERT INTO schedule_sources
                (schedule_id, source_kind, source_post_id, source_image_url,
                 source_fingerprint, source_identity, confidence, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(schedule_id, source_identity) DO UPDATE SET
                confidence = MAX(schedule_sources.confidence, excluded.confidence),
                source_fingerprint = COALESCE(excluded.source_fingerprint, schedule_sources.source_fingerprint),
                revision = schedule_sources.revision + 1",
        )
        .bind(schedule_id)
        .bind(&input.source_kind)
        .bind(&input.source_post_id)
        .bind(&input.source_image_url)
        .bind(&input.source_fingerprint)
        .bind(identity)
        .bind(input.confidence)
        .bind(Utc::now().to_rfc3339())
        .execute(&mut *conn)
        .await?;
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM schedule_sources WHERE schedule_id = ? AND source_identity = ?",
        )
        .bind(schedule_id)
        .bind(identity)
        .fetch_one(&mut *conn)
        .await?;
        self.emit_schedule_source_change(conn, id, "upsert", actor)
            .await?;
        Ok(Some(id))
    }

    pub(crate) async fn save_weekly_exception_on(
        &self,
        conn: &mut SqliteConnection,
        rule_id: i64,
        occurrence_date: &str,
        exception_kind: &str,
        schedule_id: Option<i64>,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        sqlx::query(
            "INSERT INTO weekly_schedule_exceptions
                (rule_id, occurrence_date, exception_kind, schedule_id, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(rule_id, occurrence_date) DO UPDATE SET
                exception_kind = excluded.exception_kind,
                schedule_id = excluded.schedule_id,
                revision = weekly_schedule_exceptions.revision + 1",
        )
        .bind(rule_id)
        .bind(occurrence_date)
        .bind(exception_kind)
        .bind(schedule_id)
        .bind(Utc::now().to_rfc3339())
        .execute(&mut *conn)
        .await?;
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM weekly_schedule_exceptions WHERE rule_id = ? AND occurrence_date = ?",
        )
        .bind(rule_id)
        .bind(occurrence_date)
        .fetch_one(&mut *conn)
        .await?;
        self.emit_weekly_exception_change(conn, id, "upsert", actor)
            .await?;
        Ok(id)
    }

    pub(crate) async fn add_source_suppression_on(
        &self,
        conn: &mut SqliteConnection,
        streamer_id: i64,
        source_post_id: &str,
        source_fingerprint: &str,
        actor: SyncActor<'_>,
    ) -> AppResult<i64> {
        sqlx::query(
            "INSERT INTO source_suppressions
                (streamer_id, source_post_id, source_fingerprint, created_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(streamer_id, source_post_id, source_fingerprint) DO UPDATE SET
                revision = source_suppressions.revision + 1",
        )
        .bind(streamer_id)
        .bind(source_post_id)
        .bind(source_fingerprint)
        .bind(Utc::now().to_rfc3339())
        .execute(&mut *conn)
        .await?;
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM source_suppressions
             WHERE streamer_id = ? AND source_post_id = ? AND source_fingerprint = ?",
        )
        .bind(streamer_id)
        .bind(source_post_id)
        .bind(source_fingerprint)
        .fetch_one(&mut *conn)
        .await?;
        self.emit_source_suppression_change(conn, id, "upsert", actor)
            .await?;
        Ok(id)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AddSubscription, LiveSessionStats, LiveSnapshot, NewSchedule, NewWeeklyScheduleRule,
        UpdateSchedule, UpdateSubscription,
    };
    use chrono::Duration;

    async fn add_streamer(store: &SqliteStore, mid: &str, name: &str) -> Streamer {
        store
            .add_subscription(&AddSubscription {
                mid: mid.to_owned(),
                name: Some(name.to_owned()),
                avatar_url: None,
                room_id: None,
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
            })
            .await
            .expect("streamer")
    }

    async fn add_schedule(store: &SqliteStore, streamer_id: i64) -> Schedule {
        store
            .upsert_schedule(&NewSchedule {
                streamer_id,
                participants: Vec::new(),
                title: "同步测试日程".to_owned(),
                start_at: Utc::now() + Duration::days(1),
                end_at: None,
                room_url: None,
                room_owner_mid: None,
                room_owner_name: None,
                source_kind: "manual".to_owned(),
                source_post_id: None,
                source_image_url: None,
                source_author_mid: None,
                source_fingerprint: None,
                source_identity: Some("manual:sync-test".to_owned()),
                source_published_at: None,
                confidence: 1.0,
            })
            .await
            .expect("schedule")
            .0
    }

    #[tokio::test]
    async fn sync_change_sequences_are_contiguous() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = add_streamer(&store, "100", "同步主播").await;
        let schedule = add_schedule(&store, streamer.id).await;
        store
            .add_linked_account(streamer.id, "200", "官号", "")
            .await
            .expect("link");
        store
            .add_weekly_rule(&NewWeeklyScheduleRule {
                rule_key: Some("sync-rule".to_owned()),
                streamer_id: streamer.id,
                title: "固定周表".to_owned(),
                weekdays_mask: 1,
                start_time: "20:00".to_owned(),
                end_time: None,
                timezone: "Asia/Shanghai".to_owned(),
                room_url: None,
                effective_from: "2026-01-01".to_owned(),
                effective_until: None,
                enabled: true,
            })
            .await
            .expect("weekly rule");
        store
            .update_subscription(
                streamer.id,
                &UpdateSubscription {
                    schedule_enabled: Some(true),
                    live_notify_enabled: Some(false),
                    live_stats_enabled: None,
                    aliases: Some(vec!["别名".to_owned()]),
                },
            )
            .await
            .expect("subscription update");
        store
            .update_schedule(
                schedule.id,
                &UpdateSchedule {
                    title: Some("同步后标题".to_owned()),
                    start_at: None,
                    end_at: None,
                    room_url: None,
                },
            )
            .await
            .expect("schedule update");
        store.cancel_schedule(schedule.id).await.expect("cancel");
        store.delete_schedule(schedule.id).await.expect("delete");

        let page = store
            .sync_changes_after(0, SYNC_PAGE_MAX)
            .await
            .expect("changes");
        assert!(!page.reset_required);
        assert_eq!(page.changes.len() as i64, page.latest_seq);
        for (index, change) in page.changes.iter().enumerate() {
            assert_eq!(change.seq, index as i64 + 1);
        }
    }

    #[tokio::test]
    async fn sync_feed_identity_is_stable_and_included_in_protocol_payloads() {
        let store = SqliteStore::in_memory().await.expect("store");
        let first = store.sync_feed_id().await.expect("feed id");
        let second = store.sync_feed_id().await.expect("feed id");
        assert_eq!(first, second);
        assert_eq!(first.len(), 36);

        let page = store.sync_changes_after(0, 10).await.expect("changes");
        let bootstrap = store.sync_bootstrap().await.expect("bootstrap");
        assert_eq!(page.feed_id, first);
        assert_eq!(bootstrap.feed_id, first);
    }

    #[tokio::test]
    async fn linked_account_changes_use_stable_entity_ids() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = add_streamer(&store, "100", "官号同步主播").await;
        store
            .add_linked_account(streamer.id, "200", "官号", "")
            .await
            .expect("link");
        store
            .remove_linked_account(streamer.id, "200")
            .await
            .expect("delete link");

        let page = store.sync_changes_after(0, 100).await.expect("changes");
        let changes: Vec<_> = page
            .changes
            .iter()
            .filter(|change| change.entity_type == "linked_account")
            .collect();
        assert_eq!(changes.len(), 2);
        let expected = format!("{}:200", streamer.id);
        assert_eq!(changes[0].entity_id, expected);
        assert_eq!(changes[0].operation, "upsert");
        assert!(changes[0].payload.is_some());
        assert_eq!(changes[1].entity_id, expected);
        assert_eq!(changes[1].operation, "delete");
        assert!(changes[1].payload.is_none());
    }

    #[tokio::test]
    async fn deleting_schedule_writes_a_tombstone() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = add_streamer(&store, "101", "删除主播").await;
        let schedule = add_schedule(&store, streamer.id).await;
        store.delete_schedule(schedule.id).await.expect("delete");

        let page = store.sync_changes_after(0, 100).await.expect("changes");
        let tombstone = page
            .changes
            .iter()
            .rev()
            .find(|change| {
                change.entity_type == "schedule" && change.entity_id == schedule.id.to_string()
            })
            .expect("schedule change");
        assert_eq!(tombstone.operation, "delete");
        assert!(tombstone.payload.is_none());
        assert!(tombstone.entity_revision >= 1);
    }

    #[tokio::test]
    async fn bootstrap_revision_connects_to_later_changes() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = add_streamer(&store, "102", "快照主播").await;
        let schedule = add_schedule(&store, streamer.id).await;
        let bootstrap = store.sync_bootstrap().await.expect("bootstrap");
        assert_eq!(
            bootstrap.snapshot_revision,
            store.latest_sync_seq().await.unwrap()
        );
        assert_eq!(bootstrap.subscriptions.len(), 1);
        assert_eq!(bootstrap.schedules.len(), 1);

        store
            .update_schedule(
                schedule.id,
                &UpdateSchedule {
                    title: Some("快照后的修改".to_owned()),
                    start_at: None,
                    end_at: None,
                    room_url: None,
                },
            )
            .await
            .expect("update");
        let page = store
            .sync_changes_after(bootstrap.snapshot_revision, 100)
            .await
            .expect("changes");
        assert!(!page.reset_required);
        assert_eq!(page.from_seq, bootstrap.snapshot_revision + 1);
        assert_eq!(page.changes.len(), 1);
        assert_eq!(page.changes[0].entity_type, "schedule");
        assert_eq!(page.changes[0].entity_id, schedule.id.to_string());
        assert_eq!(
            page.changes[0].payload.as_ref().unwrap()["title"],
            "快照后的修改"
        );
    }

    #[tokio::test]
    async fn transaction_failure_leaves_entity_and_change_log_unchanged() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = add_streamer(&store, "103", "事务主播").await;
        let latest = store.latest_sync_seq().await.expect("latest");
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.expect("tx");
        sqlx::query("UPDATE streamers SET name = '不应提交' WHERE id = ?")
            .bind(streamer.id)
            .execute(&mut *tx)
            .await
            .expect("update");
        append_sync_change_on(
            &mut tx,
            "subscription",
            &streamer.id.to_string(),
            "upsert",
            2,
            Some(json!({"id": streamer.id, "name": "不应提交"})),
            "system",
            None,
            None,
            Utc::now(),
        )
        .await
        .expect("staged change");
        let forced_error = sqlx::query("INSERT INTO missing_table(value) VALUES (1)")
            .execute(&mut *tx)
            .await;
        assert!(forced_error.is_err());
        drop(tx);

        assert_eq!(
            store.get_streamer(streamer.id).await.unwrap().name,
            "事务主播"
        );
        assert_eq!(store.latest_sync_seq().await.unwrap(), latest);

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
            .expect("later update");
        assert_eq!(store.latest_sync_seq().await.unwrap(), latest + 1);
    }

    #[tokio::test]
    async fn devices_register_and_ack_monotonically() {
        let store = SqliteStore::in_memory().await.expect("store");
        let device = store
            .register_device("device-1", "客厅电脑", "windows", "0.2.0")
            .await
            .expect("register");
        assert!(device.online);
        assert_eq!(device.last_ack_seq, 0);
        let acked = store.ack_device("device-1", 12).await.expect("ack");
        assert_eq!(acked.last_ack_seq, 12);
        let older = store.ack_device("device-1", 7).await.expect("old ack");
        assert_eq!(older.last_ack_seq, 12);
        assert_eq!(store.list_devices().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn offline_device_logs_can_be_deleted_without_touching_sync_feed() {
        let store = SqliteStore::in_memory().await.expect("store");
        store
            .register_device("device-offline", "旧电脑", "windows", "0.2.0")
            .await
            .expect("register");
        let latest = store.latest_sync_seq().await.unwrap();

        let online_error = store
            .delete_device_log("device-offline")
            .await
            .expect_err("online device must be kept");
        assert!(online_error.to_string().contains("离线"));

        let stale_at =
            (Utc::now() - chrono::Duration::seconds(DEVICE_ONLINE_WINDOW_SECONDS + 1)).to_rfc3339();
        sqlx::query("UPDATE devices SET last_seen_at = ? WHERE device_id = ?")
            .bind(stale_at)
            .bind("device-offline")
            .execute(&store.pool)
            .await
            .expect("age device");

        store
            .delete_device_log("device-offline")
            .await
            .expect("delete offline log");
        assert!(store.list_devices().await.unwrap().is_empty());
        assert_eq!(store.latest_sync_seq().await.unwrap(), latest);

        let reconnected = store
            .register_device("device-offline", "旧电脑", "windows", "0.2.0")
            .await
            .expect("re-register");
        assert!(reconnected.online);
    }

    #[tokio::test]
    async fn pruning_without_candidates_does_not_wait_for_the_writer() {
        let directory =
            std::env::temp_dir().join(format!("oshical-prune-lock-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("directory");
        let store = SqliteStore::connect(directory.join("sync.db"))
            .await
            .expect("store");
        store
            .append_sync_change("test", "fresh", "upsert", 1, Some(json!({})))
            .await
            .expect("append");
        let writer = store
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .expect("hold writer");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            store.prune_sync_changes(SyncPrunePolicy::Before(Utc::now() - Duration::days(30))),
        )
        .await;
        let count_result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            store.prune_sync_changes(SyncPrunePolicy::KeepLatest(1)),
        )
        .await;
        let invalid = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            store.prune_sync_changes(SyncPrunePolicy::KeepLatest(-1)),
        )
        .await;
        writer.rollback().await.expect("release writer");
        assert_eq!(
            count_result
                .expect("count lookup must not acquire a writer")
                .unwrap(),
            0
        );
        assert!(invalid
            .expect("invalid policy must be checked before waiting")
            .is_err());
        assert_eq!(
            result
                .expect("read-only boundary lookup must not acquire a write lock")
                .expect("prune"),
            0
        );
        store.pool.close().await;
        drop(store);
        // SQLx's SQLite worker may release the last OS file handle shortly
        // after pool closure. Windows refuses directory removal until then.
        // Retry ONLY that teardown race; never relax the lock assertions above.
        let cleanup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match tokio::fs::remove_dir_all(&directory).await {
                Ok(()) => break,
                Err(error)
                    if (error.kind() == std::io::ErrorKind::PermissionDenied
                        || error.raw_os_error() == Some(32))
                        && tokio::time::Instant::now() < cleanup_deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => panic!("cleanup after SQLite worker shutdown: {error}"),
            }
        }
    }

    #[tokio::test]
    async fn pruning_commits_small_prefixes_with_matching_cursor_metadata() {
        let store = SqliteStore::in_memory().await.expect("store");
        sqlx::query(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 530)
             INSERT INTO sync_changes(entity_type,entity_id,operation,entity_revision,payload_json,actor_type,created_at)
             SELECT 'test', CAST(x AS TEXT), 'upsert', 1, '{}', 'system', '2026-11-01T00:00:00+00:00' FROM n",
        ).execute(&store.pool).await.expect("seed changes");
        sqlx::query("INSERT INTO sync_feed_meta(key,value,updated_at) VALUES('min_retained_seq',0,'test') ON CONFLICT(key) DO UPDATE SET value=0")
            .execute(&store.pool).await.expect("seed metadata");
        sqlx::raw_sql(
            "CREATE TABLE prune_audit(remaining INTEGER, boundary INTEGER);
             CREATE TRIGGER audit_prune AFTER UPDATE OF value ON sync_feed_meta
             WHEN NEW.key = 'min_retained_seq'
             BEGIN
                 INSERT INTO prune_audit SELECT COUNT(*), NEW.value FROM sync_changes;
             END;",
        )
        .execute(&store.pool)
        .await
        .expect("audit commits");
        let latest = store.latest_sync_seq().await.unwrap();
        assert_eq!(
            store
                .prune_sync_changes(SyncPrunePolicy::KeepLatest(7))
                .await
                .unwrap(),
            523
        );
        let audit: Vec<(i64, i64)> =
            sqlx::query_as("SELECT remaining,boundary FROM prune_audit ORDER BY rowid")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert!(
            audit.len() >= 3,
            "deletion must be split into bounded transactions: {audit:?}"
        );
        let mut previous = 530;
        for (remaining, boundary) in audit {
            assert!((1..=256).contains(&(previous - remaining)));
            assert_eq!(
                boundary,
                latest - remaining + 1,
                "cursor and deleted prefix must commit together"
            );
            previous = remaining;
        }
        assert_eq!(previous, 7);
        assert_eq!(store.latest_sync_seq().await.unwrap(), latest);
        assert_eq!(store.min_retained_sync_seq().await.unwrap(), latest - 6);
        assert!(
            store
                .sync_changes_after(0, 100)
                .await
                .unwrap()
                .reset_required
        );
        let retained = store.sync_changes_after(latest - 7, 100).await.unwrap();
        assert!(!retained.reset_required);
        assert_eq!(retained.changes.len(), 7);
    }

    #[tokio::test]
    async fn pruning_failed_batch_preserves_committed_cursor_and_can_resume() {
        let store = SqliteStore::in_memory().await.unwrap();
        sqlx::query(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 530)
             INSERT INTO sync_changes(entity_type,entity_id,operation,entity_revision,payload_json,actor_type,created_at)
             SELECT 'test', CAST(x AS TEXT), 'upsert', 1, '{}', 'system', '2026-11-01T00:00:00Z' FROM n",
        ).execute(&store.pool).await.unwrap();
        let latest = store.latest_sync_seq().await.unwrap();
        sqlx::raw_sql(
            "CREATE TRIGGER fail_second_batch BEFORE DELETE ON sync_changes
             WHEN OLD.seq > 256 BEGIN SELECT RAISE(ABORT, 'injected batch failure'); END;",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(store
            .prune_sync_changes(SyncPrunePolicy::KeepLatest(7))
            .await
            .is_err());
        assert_eq!(store.min_retained_sync_seq().await.unwrap(), 257);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_changes")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(remaining, 274);
        sqlx::query("DROP TRIGGER fail_second_batch")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .prune_sync_changes(SyncPrunePolicy::KeepLatest(7))
                .await
                .unwrap(),
            267
        );
        assert_eq!(store.min_retained_sync_seq().await.unwrap(), latest - 6);
        assert_eq!(store.latest_sync_seq().await.unwrap(), latest);
    }

    #[tokio::test]
    async fn pruning_fixed_boundary_retains_changes_appended_between_batches() {
        let store = SqliteStore::in_memory().await.unwrap();
        sqlx::query(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 530)
             INSERT INTO sync_changes(entity_type,entity_id,operation,entity_revision,payload_json,actor_type,created_at)
             SELECT 'test', CAST(x AS TEXT), 'upsert', 1, '{}', 'system', '2026-11-01T00:00:00Z' FROM n",
        ).execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO sync_feed_meta(key,value,updated_at) VALUES('min_retained_seq',0,'test') ON CONFLICT(key) DO UPDATE SET value=0")
            .execute(&store.pool).await.unwrap();
        // Inject an append at the first prefix commit, before the following batch.
        sqlx::raw_sql(
            "CREATE TRIGGER append_during_prune AFTER UPDATE OF value ON sync_feed_meta
             WHEN NEW.key='min_retained_seq' AND OLD.value=0
             BEGIN
                 INSERT INTO sync_changes(entity_type,entity_id,operation,entity_revision,payload_json,actor_type,created_at)
                 VALUES('test','concurrent','upsert',1,'{}','system','2026-11-10T00:00:00Z');
             END;",
        ).execute(&store.pool).await.unwrap();
        assert_eq!(
            store
                .prune_sync_changes(SyncPrunePolicy::KeepLatest(7))
                .await
                .unwrap(),
            523
        );
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_changes")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            remaining, 8,
            "new appends are not part of the original deletion boundary"
        );
        assert_eq!(store.latest_sync_seq().await.unwrap(), 531);
        let page = store.sync_changes_after(530, 100).await.unwrap();
        assert!(!page.reset_required);
        assert_eq!(page.changes[0].entity_id, "concurrent");
    }

    #[tokio::test]
    async fn pruning_before_preserves_timezone_comparison_and_uses_covering_index() {
        let store = SqliteStore::in_memory().await.expect("store");
        for (index, timestamp) in ["2026-11-02T00:00:00+08:00", "2026-11-01T17:00:00+00:00"]
            .into_iter()
            .enumerate()
        {
            store
                .append_sync_change("test", &index.to_string(), "upsert", 1, Some(json!({})))
                .await
                .unwrap();
            sqlx::query("UPDATE sync_changes SET created_at=? WHERE entity_id=?")
                .bind(timestamp)
                .bind(index.to_string())
                .execute(&store.pool)
                .await
                .unwrap();
        }
        let plan = sqlx::query(&format!(
            "EXPLAIN QUERY PLAN {SYNC_PRUNE_BEFORE_BOUNDARY_SQL}"
        ))
        .bind("2026-11-01T16:30:00Z")
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert!(plan.iter().any(|row| row
            .get::<String, _>("detail")
            .contains("COVERING INDEX idx_sync_changes_created")));
        let before = "2026-11-01T16:30:00Z".parse().unwrap();
        assert_eq!(
            store
                .prune_sync_changes(SyncPrunePolicy::Before(before))
                .await
                .unwrap(),
            1
        );
        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT entity_id FROM sync_changes ORDER BY seq")
                .fetch_all(&store.pool)
                .await
                .unwrap();
        assert_eq!(remaining, vec!["1"]);
    }

    #[tokio::test]
    async fn pruning_advances_the_retained_cursor_boundary() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = add_streamer(&store, "105", "清理主播").await;
        add_schedule(&store, streamer.id).await;

        let before = store.sync_changes_after(0, 100).await.expect("before");
        assert!(!before.reset_required);
        assert!(!before.changes.is_empty());
        let latest = before.latest_seq;
        assert!(latest > 1);

        let deleted = store
            .prune_sync_changes(SyncPrunePolicy::KeepLatest(1))
            .await
            .expect("prune");
        assert_eq!(deleted, (latest - 1) as u64);
        assert_eq!(store.latest_sync_seq().await.unwrap(), latest);
        assert_eq!(store.min_retained_sync_seq().await.unwrap(), latest);

        let stale = store.sync_changes_after(0, 100).await.expect("stale");
        assert!(stale.reset_required);
        assert!(stale.changes.is_empty());

        let retained = store
            .sync_changes_after(latest - 1, 100)
            .await
            .expect("retained");
        assert!(!retained.reset_required);
        assert_eq!(retained.changes.len(), 1);
        assert_eq!(retained.changes[0].seq, latest);
    }

    #[tokio::test]
    async fn cursor_ahead_of_latest_requires_reset() {
        let store = SqliteStore::in_memory().await.expect("store");
        let latest = store.latest_sync_seq().await.expect("latest");
        let page = store
            .sync_changes_after(latest.saturating_add(1), 100)
            .await
            .expect("page");
        assert!(page.reset_required);
        assert!(page.changes.is_empty());
        assert_eq!(page.latest_seq, latest);
    }

    #[tokio::test]
    async fn retained_sequence_metadata_survives_reopen() {
        let directory =
            std::env::temp_dir().join(format!("oshical-sync-prune-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("temp directory");
        let path = directory.join("sync.db");

        let store = SqliteStore::connect(&path).await.expect("store");
        let feed_id = store.sync_feed_id().await.expect("feed id");
        for index in 0..3 {
            store
                .append_sync_change(
                    "test",
                    &index.to_string(),
                    "upsert",
                    1,
                    Some(json!({ "index": index })),
                )
                .await
                .expect("append");
        }
        let latest = store.latest_sync_seq().await.expect("latest");
        assert_eq!(latest, 3);
        assert_eq!(store.min_retained_sync_seq().await.unwrap(), 1);
        assert_eq!(
            store
                .prune_sync_changes(SyncPrunePolicy::KeepLatest(0))
                .await
                .expect("prune"),
            3
        );
        assert_eq!(store.latest_sync_seq().await.unwrap(), latest);
        assert_eq!(store.min_retained_sync_seq().await.unwrap(), latest + 1);
        store.pool.close().await;
        drop(store);

        let reopened = SqliteStore::connect(&path).await.expect("reopen");
        assert_eq!(reopened.sync_feed_id().await.unwrap(), feed_id);
        assert_eq!(reopened.latest_sync_seq().await.unwrap(), latest);
        assert_eq!(reopened.min_retained_sync_seq().await.unwrap(), latest + 1);
        let next_seq = reopened
            .append_sync_change("test", "next", "upsert", 1, Some(json!({ "index": 3 })))
            .await
            .expect("next append");
        assert_eq!(next_seq, latest + 1);
        assert_eq!(reopened.min_retained_sync_seq().await.unwrap(), latest + 1);
        let changes = reopened
            .sync_changes_after(latest, 100)
            .await
            .expect("changes");
        assert!(!changes.reset_required);
        assert_eq!(changes.feed_id, feed_id);
        assert_eq!(changes.changes.len(), 1);
        assert_eq!(changes.changes[0].seq, latest + 1);

        reopened.pool.close().await;
        drop(reopened);
        let _ = tokio::fs::remove_dir_all(&directory).await;
    }

    #[tokio::test]
    async fn live_session_create_checkpoint_and_finish_emit_changes() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = add_streamer(&store, "106", "直播同步主播").await;
        let baseline = store.latest_sync_seq().await.expect("baseline");
        let started_at = Utc::now();
        let session = store
            .ensure_live_session(
                streamer.id,
                "106",
                &streamer.name,
                "同步直播",
                None,
                started_at,
                started_at,
            )
            .await
            .expect("session");
        assert_eq!(session.revision, 1);
        let after_create = store.latest_sync_seq().await.expect("created");
        assert_eq!(after_create, baseline + 1);
        let created = store
            .sync_changes_after(baseline, 100)
            .await
            .expect("created changes");
        assert_eq!(created.changes.len(), 1);
        assert_eq!(created.changes[0].entity_type, "live_session");
        assert_eq!(created.changes[0].entity_id, session.id.to_string());
        assert_eq!(
            created.changes[0].payload.as_ref().unwrap()["title"],
            "同步直播"
        );

        store
            .ensure_live_session(
                streamer.id,
                "106",
                &streamer.name,
                "同步直播",
                None,
                started_at,
                started_at + Duration::seconds(20),
            )
            .await
            .expect("unchanged session");
        assert_eq!(store.latest_sync_seq().await.unwrap(), after_create);

        let stats = LiveSessionStats {
            peak_high_energy: 555,
            danmaku_count: 88,
            interactive_user_count: 12,
            paid_user_count: 3,
            revenue_cents: 900,
            data_quality: "collecting".to_owned(),
            collector_state: "active".to_owned(),
            ..Default::default()
        };
        store
            .update_live_session_stats(session.id, &stats)
            .await
            .expect("checkpoint");
        let after_checkpoint = store.latest_sync_seq().await.expect("checkpoint seq");
        assert_eq!(after_checkpoint, after_create + 1);
        let checkpoint = store
            .sync_changes_after(after_create, 100)
            .await
            .expect("checkpoint changes");
        assert_eq!(checkpoint.changes.len(), 1);
        assert_eq!(checkpoint.changes[0].entity_type, "live_session");
        assert_eq!(
            checkpoint.changes[0].payload.as_ref().unwrap()["danmakuCount"],
            88
        );
        assert_eq!(
            checkpoint.changes[0].payload.as_ref().unwrap()["peakHighEnergy"],
            555
        );

        let finished_stats = LiveSessionStats {
            data_quality: "complete".to_owned(),
            collector_state: "finished".to_owned(),
            ..stats
        };
        store
            .finish_live_session(session.id, started_at + Duration::hours(1), &finished_stats)
            .await
            .expect("finish");
        let after_finish = store.latest_sync_seq().await.expect("finish seq");
        assert_eq!(after_finish, after_checkpoint + 1);
        let finished = store
            .sync_changes_after(after_checkpoint, 100)
            .await
            .expect("finish changes");
        assert_eq!(finished.changes.len(), 1);
        let payload = finished.changes[0].payload.as_ref().unwrap();
        assert_eq!(payload["collectorState"], "finished");
        assert!(payload["endedAt"].is_string());
        assert_eq!(
            store.get_live_session(session.id).await.unwrap().revision,
            3
        );
    }

    #[tokio::test]
    async fn unchanged_live_status_does_not_spam_sync_log() {
        let store = SqliteStore::in_memory().await.expect("store");
        let streamer = add_streamer(&store, "104", "开播主播").await;
        let latest = store.latest_sync_seq().await.expect("latest");
        let started_at = Utc::now();
        let first = LiveSnapshot {
            is_live: true,
            title: "稳定开播".to_owned(),
            cover_url: None,
            live_url: Some("https://live.bilibili.com/104".to_owned()),
            online: 10,
            started_at: Some(started_at),
            checked_at: started_at,
        };
        store
            .set_live_status(streamer.id, &first)
            .await
            .expect("first status");
        let after_first = store.latest_sync_seq().await.expect("first seq");
        assert_eq!(after_first, latest + 1);

        let second = LiveSnapshot {
            checked_at: started_at + Duration::seconds(20),
            ..first
        };
        store
            .set_live_status(streamer.id, &second)
            .await
            .expect("second status");
        assert_eq!(
            store.latest_sync_seq().await.expect("second seq"),
            after_first
        );
    }
}
