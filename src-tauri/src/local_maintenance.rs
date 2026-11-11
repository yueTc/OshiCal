use std::time::{Duration, Instant};

use oshical_core::{AppResult, SqliteStore, SyncPrunePolicy};

// Same defaults as the server runtime. Operate only on the independent local
// store; never prune the remote mirror or pending offline mutations.
const STARTUP_DELAY: Duration = Duration::from_secs(60);
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const RETENTION_DAYS: i64 = 7;
const RETENTION_ROWS: i64 = 10_000;
const RETENTION_PAYLOAD_BYTES: i64 = 20 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct LocalPruneSchedule {
    next_due: Option<Instant>,
}

impl LocalPruneSchedule {
    pub(crate) fn due(&mut self, now: Instant, local_mode: bool) -> bool {
        if !local_mode {
            self.next_due = None;
            return false;
        }
        let next = self.next_due.get_or_insert(now + STARTUP_DELAY);
        if now < *next {
            return false;
        }
        *next = now + PRUNE_INTERVAL;
        true
    }
}

pub(crate) async fn prune_local_sync_history(store: &SqliteStore) -> AppResult<()> {
    // Continue independent policies on failure, but report the first error.
    let mut first_error = None;
    for policy in [
        SyncPrunePolicy::Before(chrono::Utc::now() - chrono::Duration::days(RETENTION_DAYS)),
        SyncPrunePolicy::KeepLatest(RETENTION_ROWS),
        SyncPrunePolicy::KeepPayloadBytes(RETENTION_PAYLOAD_BYTES),
    ] {
        if let Err(error) = store.prune_sync_changes(policy).await {
            tracing::warn!(%error, "本地同步历史清理失败");
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_one_minute_then_runs_hourly_and_resets_when_leaving_local_mode() {
        let mut schedule = LocalPruneSchedule::default();
        let start = Instant::now();
        assert!(!schedule.due(start, true));
        assert!(!schedule.due(start + Duration::from_secs(59), true));
        assert!(schedule.due(start + STARTUP_DELAY, true));
        assert!(!schedule.due(start + STARTUP_DELAY + Duration::from_secs(3599), true));
        assert!(schedule.due(start + STARTUP_DELAY + PRUNE_INTERVAL, true));
        assert!(!schedule.due(start + Duration::from_secs(4000), false));
        assert!(!schedule.due(start + Duration::from_secs(8000), true));
        assert!(schedule.due(start + Duration::from_secs(8060), true));
    }

    #[tokio::test]
    async fn retention_keeps_latest_rows_and_preserves_business_records() {
        let store = SqliteStore::in_memory().await.unwrap();
        // No business fixture needed: verify pending edits remain outside the log.
        let mutation = oshical_core::SyncMutation {
            mutation_id: "offline-edit".to_owned(),
            operation: "create".to_owned(),
            entity: "schedule".to_owned(),
            payload: serde_json::json!({"title":"待提交"}),
            base_updated_at: None,
            device_id: None,
            base_revision: None,
            status: "pending".to_owned(),
            last_error: None,
            created_at: chrono::Utc::now(),
            applied_at: None,
            result_json: None,
        };
        store.enqueue_mutation(&mutation).await.unwrap();
        let pool = store.pool();
        sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 10005)
            INSERT INTO sync_changes(entity_type,entity_id,operation,entity_revision,payload_json,actor_type,created_at)
            SELECT 'test', CAST(x AS TEXT), 'upsert', 1, '{}', 'system', ? FROM n")
            .bind(chrono::Utc::now().to_rfc3339()).execute(pool).await.unwrap();
        prune_local_sync_history(&store).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_changes")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(count, RETENTION_ROWS);
        assert!(
            store
                .sync_changes_after(0, 10)
                .await
                .unwrap()
                .reset_required
        );
        assert_eq!(
            store.list_pending_mutations(10).await.unwrap()[0].mutation_id,
            "offline-edit"
        );
    }
}
