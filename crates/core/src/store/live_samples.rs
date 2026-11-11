use super::*;
use crate::models::{LiveOnlineSample, LiveOnlineSamplesPage};

#[derive(sqlx::FromRow)]
struct SampleRow {
    id: i64,
    session_id: i64,
    sampled_at: String,
    observed_at: Option<String>,
    high_energy_count: Option<i64>,
    collector_state: String,
}

impl SqliteStore {
    /// A compact independent row. No sync change or parent revision is emitted.
    pub async fn record_live_online_sample(
        &self,
        session_id: i64,
        sampled_at: DateTime<Utc>,
        observation: Option<(DateTime<Utc>, i64)>,
        collector_state: &str,
    ) -> AppResult<()> {
        if observation.is_some_and(|(at, count)| count < 0 || at > sampled_at) {
            return Err(AppError::validation("直播人数观测值或时间非法"));
        }
        if !matches!(collector_state, "active" | "reconnecting" | "finished") {
            return Err(AppError::validation("直播人数采样状态非法"));
        }
        sqlx::query(
            "INSERT INTO live_online_samples
                (session_id, sampled_at, observed_at, high_energy_count, collector_state)
             SELECT id, ?, ?, ?, ? FROM live_sessions WHERE id = ? AND ended_at IS NULL
             ON CONFLICT(session_id, sampled_at) DO NOTHING",
        )
        .bind(sampled_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .bind(observation.map(|(at, _)| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
        .bind(observation.map(|(_, count)| count))
        .bind(collector_state)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_live_online_samples(
        &self,
        session_id: i64,
        after_id: i64,
        limit: i64,
    ) -> AppResult<LiveOnlineSamplesPage> {
        if after_id < 0 || !(1..=1000).contains(&limit) {
            return Err(AppError::validation(
                "采样游标必须非负，limit 必须为 1 到 1000",
            ));
        }
        self.get_live_session(session_id).await?;
        let rows = sqlx::query_as::<_, SampleRow>(
            "SELECT id, session_id, sampled_at, observed_at, high_energy_count, collector_state
             FROM live_online_samples WHERE session_id = ? AND id > ? ORDER BY id LIMIT ?",
        )
        .bind(session_id)
        .bind(after_id)
        .bind(limit + 1)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > limit as usize;
        let samples = rows
            .into_iter()
            .take(limit as usize)
            .map(|row| {
                Ok(LiveOnlineSample {
                    id: row.id,
                    session_id: row.session_id,
                    sampled_at: parse_ts(&row.sampled_at)?,
                    observed_at: parse_opt_ts(row.observed_at.as_deref())?,
                    high_energy_count: row.high_energy_count,
                    collector_state: row.collector_state,
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        let next_after_id = samples.last().map_or(after_id, |sample| sample.id);
        Ok(LiveOnlineSamplesPage {
            session_id,
            metric: "bilibili_high_energy_users".to_owned(),
            sample_interval_seconds: 30,
            samples,
            next_after_id,
            has_more,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AddSubscription;

    async fn fixture() -> (SqliteStore, LiveSession) {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&AddSubscription {
                mid: "123".to_owned(),
                name: Some("测试".to_owned()),
                avatar_url: None,
                room_id: Some("456".to_owned()),
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: true,
            })
            .await
            .unwrap();
        let now = Utc::now() - chrono::Duration::minutes(5);
        let session = store
            .ensure_live_session(streamer.id, "456", "测试", "标题", None, now, now)
            .await
            .unwrap();
        (store, session)
    }

    #[tokio::test]
    async fn preserves_zero_declines_gaps_and_paginates_without_sync_growth() {
        let (store, session) = fixture().await;
        let seq = store.latest_sync_seq().await.unwrap();
        let now = Utc::now();
        for (offset, count) in [(0, Some(100)), (30, Some(70)), (60, Some(0)), (90, None)] {
            let at = now + chrono::Duration::seconds(offset);
            store
                .record_live_online_sample(session.id, at, count.map(|n| (at, n)), "active")
                .await
                .unwrap();
        }
        // Same checkpoint is idempotent, not overwritten by a retry.
        store
            .record_live_online_sample(session.id, now, Some((now, 999)), "active")
            .await
            .unwrap();
        let page = store
            .list_live_online_samples(session.id, 0, 2)
            .await
            .unwrap();
        assert!(page.has_more);
        assert_eq!(page.samples[0].high_energy_count, Some(100));
        assert_eq!(page.samples[1].high_energy_count, Some(70));
        let tail = store
            .list_live_online_samples(session.id, page.next_after_id, 2)
            .await
            .unwrap();
        assert!(!tail.has_more);
        assert_eq!(tail.samples[0].high_energy_count, Some(0));
        assert_eq!(tail.samples[1].high_energy_count, None);
        assert!(tail.samples[1].observed_at.is_none());
        assert_eq!(store.latest_sync_seq().await.unwrap(), seq);
        assert_eq!(
            store
                .list_live_online_samples(session.id, tail.next_after_id, 2)
                .await
                .unwrap()
                .samples
                .len(),
            0
        );
        store
            .delete_subscription(session.streamer_id)
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_online_samples")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn samples_survive_reinitialization_and_segment_reopening() {
        let (store, session) = fixture().await;
        let at = Utc::now();
        store
            .record_live_online_sample(session.id, at, Some((at, 42)), "active")
            .await
            .unwrap();
        store
            .finish_open_live_session(session.id, at)
            .await
            .unwrap();
        let reopened = store
            .ensure_live_session(
                session.streamer_id,
                &session.room_id,
                &session.streamer_name,
                &session.title,
                None,
                at + chrono::Duration::seconds(60),
                at + chrono::Duration::seconds(60),
            )
            .await
            .unwrap();
        assert_eq!(reopened.id, session.id);
        // Re-running schema initialization must not erase existing sample rows.
        let reinitialized = SqliteStore::initialize(store.pool.clone()).await.unwrap();
        let page = reinitialized
            .list_live_online_samples(session.id, 0, 10)
            .await
            .unwrap();
        assert_eq!(page.samples.len(), 1);
        assert_eq!(page.samples[0].high_energy_count, Some(42));
        let next_at = at + chrono::Duration::seconds(90);
        reinitialized
            .record_live_online_sample(session.id, next_at, None, "reconnecting")
            .await
            .unwrap();
        let page = reinitialized
            .list_live_online_samples(session.id, page.next_after_id, 10)
            .await
            .unwrap();
        assert_eq!(page.samples.len(), 1);
        assert_eq!(page.samples[0].collector_state, "reconnecting");
        assert!(page.samples[0].high_energy_count.is_none());
    }

    #[tokio::test]
    async fn validates_and_keeps_old_sessions_without_fabricated_history() {
        let (store, session) = fixture().await;
        assert!(store
            .list_live_online_samples(session.id, 0, 500)
            .await
            .unwrap()
            .samples
            .is_empty());
        assert!(store
            .list_live_online_samples(session.id, -1, 500)
            .await
            .is_err());
        assert!(store
            .list_live_online_samples(session.id, 0, 1001)
            .await
            .is_err());
        assert!(store
            .list_live_online_samples(i64::MAX, 0, 10)
            .await
            .is_err());
        let now = Utc::now();
        assert!(store
            .record_live_online_sample(session.id, now, Some((now, -1)), "active")
            .await
            .is_err());
        assert!(store
            .record_live_online_sample(
                session.id,
                now,
                Some((now + chrono::Duration::seconds(1), 1)),
                "active"
            )
            .await
            .is_err());
        store
            .finish_open_live_session(session.id, now)
            .await
            .unwrap();
        store
            .record_live_online_sample(session.id, now, Some((now, 2)), "active")
            .await
            .unwrap();
        assert!(store
            .list_live_online_samples(session.id, 0, 10)
            .await
            .unwrap()
            .samples
            .is_empty());
    }
}
