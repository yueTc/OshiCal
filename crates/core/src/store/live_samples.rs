use super::*;
use crate::models::{
    LiveOnlineSample, LiveOnlineSamplesPage, LiveSuperChatEvent, LiveSuperChatTotal,
    SuperChatEventInput,
};
use sqlx::Row;

#[derive(sqlx::FromRow)]
struct SampleRow {
    id: i64,
    session_id: i64,
    sampled_at: String,
    observed_at: Option<String>,
    high_energy_count: Option<i64>,
    collector_state: String,
}

#[derive(sqlx::FromRow)]
struct SuperChatTotalRow {
    currency: Option<String>,
    amount_micros: i64,
    event_count: i64,
}

#[derive(sqlx::FromRow)]
struct SuperChatEventRow {
    id: String,
    session_id: i64,
    kind: String,
    live_chat_id: Option<String>,
    created_at: Option<String>,
    user_channel_id: Option<String>,
    user_display_name: Option<String>,
    amount_micros: Option<i64>,
    currency: Option<String>,
    amount_display_string: Option<String>,
    tier: Option<i64>,
    captured_at: String,
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
        let summary = sqlx::query(
            "SELECT st.platform, ls.membership_count, ls.youtube_stats_available
             FROM live_sessions ls
             JOIN streamers st ON st.id = ls.streamer_id WHERE ls.id = ?",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        let platform = parse_platform(&summary.try_get::<String, _>("platform")?)?;
        let metric = match platform {
            Platform::Youtube => "youtube_concurrent_viewers",
            Platform::Bilibili => "bilibili_high_energy_users",
        };
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
        let (membership_count, stats_available, super_chat_totals) =
            if platform == Platform::Youtube {
                let totals: Vec<LiveSuperChatTotal> = sqlx::query_as::<_, SuperChatTotalRow>(
                    "SELECT currency, COALESCE(SUM(amount_micros), 0) AS amount_micros,
                            COUNT(*) AS event_count
                     FROM live_super_chat_events
                     WHERE session_id = ? AND amount_micros IS NOT NULL
                     GROUP BY currency ORDER BY currency IS NULL, currency",
                )
                .bind(session_id)
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|row| LiveSuperChatTotal {
                    currency: row.currency,
                    amount_micros: row.amount_micros,
                    event_count: row.event_count,
                })
                .collect();
                (
                    summary.try_get::<Option<i64>, _>("membership_count")?,
                    summary
                        .try_get::<Option<i64>, _>("youtube_stats_available")?
                        .map(|value| value != 0),
                    totals,
                )
            } else {
                (None, None, Vec::new())
            };
        Ok(LiveOnlineSamplesPage {
            session_id,
            platform,
            metric: metric.to_owned(),
            sample_interval_seconds: 30,
            samples,
            next_after_id,
            has_more,
            membership_count,
            stats_available,
            super_chat_totals,
        })
    }

    /// Persist one original-currency Super Chat / Super Sticker event.
    /// Returns `false` for an idempotent duplicate.
    pub async fn record_youtube_super_chat_event(
        &self,
        session_id: i64,
        event: &SuperChatEventInput,
    ) -> AppResult<bool> {
        let id = event.id.trim();
        if id.is_empty() {
            return Err(AppError::validation("YouTube 付费事件 ID 不能为空"));
        }
        if event.kind.trim().is_empty() {
            return Err(AppError::validation("YouTube 付费事件类型不能为空"));
        }
        let amount_micros = event
            .amount_micros
            .map(|value| {
                i64::try_from(value)
                    .map_err(|_| AppError::validation("YouTube Super Chat 金额超出范围"))
            })
            .transpose()?;
        let tier = event
            .tier
            .map(|value| {
                i64::try_from(value)
                    .map_err(|_| AppError::validation("YouTube Super Chat 等级超出范围"))
            })
            .transpose()?;
        let live_chat_id = event
            .live_chat_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let currency = event
            .currency
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let display_string = event
            .amount_display_string
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        self.get_live_session(session_id).await?;
        let platform: String = sqlx::query_scalar(
            "SELECT st.platform FROM live_sessions ls
             JOIN streamers st ON st.id = ls.streamer_id WHERE ls.id = ?",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        if parse_platform(&platform)? != Platform::Youtube {
            return Err(AppError::validation("付费事件只能写入 YouTube 直播场次"));
        }
        let captured_at = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let inserted = sqlx::query(
            "INSERT INTO live_super_chat_events
                (id, session_id, kind, live_chat_id, created_at, user_channel_id,
                 user_display_name, amount_micros, currency, amount_display_string,
                 tier, captured_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(id)
        .bind(session_id)
        .bind(event.kind.trim())
        .bind(live_chat_id)
        .bind(event.created_at.as_ref().map(|value| value.to_rfc3339()))
        .bind(
            event
                .user_channel_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        )
        .bind(
            event
                .user_display_name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        )
        .bind(amount_micros)
        .bind(currency)
        .bind(display_string)
        .bind(tier)
        .bind(captured_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() > 0 {
            let updated = sqlx::query(
                "UPDATE live_sessions SET
                    super_chat_count = (
                        SELECT COUNT(*) FROM live_super_chat_events WHERE session_id = ?
                    ),
                    paid_user_count = MAX(paid_user_count, (
                        SELECT COUNT(DISTINCT COALESCE(user_channel_id, id))
                        FROM live_super_chat_events WHERE session_id = ?
                    )),
                    data_quality = 'partial', updated_at = ?, revision = revision + 1
                 WHERE id = ? AND ended_at IS NULL",
            )
            .bind(session_id)
            .bind(session_id)
            .bind(captured_at.to_rfc3339())
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() > 0 {
                self.emit_live_session_change(
                    &mut tx,
                    session_id,
                    "upsert",
                    SyncActor::collector(),
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(inserted.rows_affected() > 0)
    }

    pub async fn list_live_super_chat_events(
        &self,
        session_id: i64,
        limit: i64,
    ) -> AppResult<Vec<LiveSuperChatEvent>> {
        if !(1..=1000).contains(&limit) {
            return Err(AppError::validation("limit 必须为 1 到 1000"));
        }
        self.get_live_session(session_id).await?;
        let rows = sqlx::query_as::<_, SuperChatEventRow>(
            "SELECT id, session_id, kind, live_chat_id, created_at, user_channel_id,
                    user_display_name, amount_micros, currency, amount_display_string,
                    tier, captured_at
             FROM live_super_chat_events
             WHERE session_id = ?
             ORDER BY COALESCE(created_at, captured_at), id LIMIT ?",
        )
        .bind(session_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(LiveSuperChatEvent {
                    id: row.id,
                    session_id: row.session_id,
                    kind: row.kind,
                    live_chat_id: row.live_chat_id,
                    created_at: parse_opt_ts(row.created_at.as_deref())?,
                    user_channel_id: row.user_channel_id,
                    user_display_name: row.user_display_name,
                    amount_micros: row.amount_micros,
                    currency: row.currency,
                    amount_display_string: row.amount_display_string,
                    tier: row.tier,
                    captured_at: parse_ts(&row.captured_at)?,
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_youtube_live_metrics(
        &self,
        session_id: i64,
        peak_online: u64,
        avg_online: u64,
        online_samples: u64,
        online_total: u64,
        duration_seconds: u64,
        danmaku_count: Option<u64>,
        super_chat_count: Option<u64>,
        membership_count: Option<u64>,
        paid_user_count: Option<u64>,
        interactive_user_count: Option<u64>,
        stats_available: bool,
        data_quality: &str,
        collector_state: &str,
    ) -> AppResult<()> {
        if !matches!(
            collector_state,
            "active" | "degraded" | "reconnecting" | "finished"
        ) {
            return Err(AppError::validation("YouTube 采集状态非法"));
        }
        self.get_live_session(session_id).await?;
        let platform: String = sqlx::query_scalar(
            "SELECT st.platform FROM live_sessions ls
             JOIN streamers st ON st.id = ls.streamer_id WHERE ls.id = ?",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        if parse_platform(&platform)? != Platform::Youtube {
            return Err(AppError::validation(
                "YouTube 指标只能写入 YouTube 直播场次",
            ));
        }
        let peak_online = i64::try_from(peak_online)
            .map_err(|_| AppError::validation("YouTube 峰值观看人数超出范围"))?;
        let avg_online = i64::try_from(avg_online)
            .map_err(|_| AppError::validation("YouTube 平均观看人数超出范围"))?;
        let online_samples = i64::try_from(online_samples)
            .map_err(|_| AppError::validation("YouTube 观看采样数超出范围"))?;
        let online_total = i64::try_from(online_total)
            .map_err(|_| AppError::validation("YouTube 观看采样总量超出范围"))?;
        let duration_seconds = i64::try_from(duration_seconds)
            .map_err(|_| AppError::validation("YouTube 直播时长超出范围"))?;
        let convert = |value: Option<u64>, label: &str| -> AppResult<Option<i64>> {
            value
                .map(|value| {
                    i64::try_from(value)
                        .map_err(|_| AppError::validation(format!("{label}超出范围")))
                })
                .transpose()
        };
        let danmaku_count = convert(danmaku_count, "YouTube 弹幕数")?;
        let super_chat_count = convert(super_chat_count, "YouTube Super Chat 数")?;
        let membership_count = convert(membership_count, "YouTube 会员事件数")?;
        let paid_user_count = convert(paid_user_count, "YouTube 付费用户数")?;
        let interactive_user_count = convert(interactive_user_count, "YouTube 互动用户数")?;
        let stats_available = if stats_available { 1_i64 } else { 0_i64 };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE live_sessions SET
                peak_online = MAX(peak_online, ?),
                avg_online = ?,
                online_samples = ?,
                online_total = ?,
                duration_seconds = CASE WHEN ended_at IS NULL THEN ? ELSE duration_seconds END,
                danmaku_count = MAX(danmaku_count, COALESCE(?, danmaku_count)),
                super_chat_count = MAX(super_chat_count, COALESCE(?, super_chat_count)),
                membership_count = COALESCE(MAX(COALESCE(membership_count, 0), ?), membership_count),
                paid_user_count = MAX(paid_user_count, COALESCE(?, paid_user_count)),
                interactive_user_count = MAX(interactive_user_count, COALESCE(?, interactive_user_count)),
                youtube_stats_available = ?,
                data_quality = ?,
                collector_state = ?,
                updated_at = ?, revision = revision + 1
             WHERE id = ? AND ended_at IS NULL
               AND (peak_online IS NOT MAX(peak_online, ?)
                    OR avg_online IS NOT ?
                    OR online_samples IS NOT ?
                    OR online_total IS NOT ?
                    OR duration_seconds IS NOT ?
                    OR danmaku_count IS NOT MAX(danmaku_count, COALESCE(?, danmaku_count))
                    OR super_chat_count IS NOT MAX(super_chat_count, COALESCE(?, super_chat_count))
                    OR membership_count IS NOT COALESCE(MAX(COALESCE(membership_count, 0), ?), membership_count)
                    OR paid_user_count IS NOT MAX(paid_user_count, COALESCE(?, paid_user_count))
                    OR interactive_user_count IS NOT MAX(interactive_user_count, COALESCE(?, interactive_user_count))
                    OR youtube_stats_available IS NOT ?
                    OR data_quality IS NOT ?
                    OR collector_state IS NOT ?)",
        )
        .bind(peak_online)
        .bind(avg_online)
        .bind(online_samples)
        .bind(online_total)
        .bind(duration_seconds)
        .bind(danmaku_count)
        .bind(super_chat_count)
        .bind(membership_count)
        .bind(paid_user_count)
        .bind(interactive_user_count)
        .bind(stats_available)
        .bind(data_quality)
        .bind(collector_state)
        .bind(Utc::now().to_rfc3339())
        .bind(session_id)
        .bind(peak_online)
        .bind(avg_online)
        .bind(online_samples)
        .bind(online_total)
        .bind(duration_seconds)
        .bind(danmaku_count)
        .bind(super_chat_count)
        .bind(membership_count)
        .bind(paid_user_count)
        .bind(interactive_user_count)
        .bind(stats_available)
        .bind(data_quality)
        .bind(collector_state)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() > 0 {
            self.emit_live_session_change(&mut tx, session_id, "upsert", SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        Ok(())
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
                platform: Platform::Bilibili,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
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
    async fn youtube_membership_is_separate_from_bilibili_guard_count() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&AddSubscription {
                platform: Platform::Youtube,
                mid: "UC123".to_owned(),
                name: Some("YouTube 测试".to_owned()),
                avatar_url: None,
                room_id: None,
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: true,
                handle: Some("@test".to_owned()),
                profile_url: Some("https://www.youtube.com/@test".to_owned()),
                uploads_playlist_id: None,
            })
            .await
            .unwrap();
        let now = Utc::now() - chrono::Duration::minutes(5);
        let session = store
            .ensure_youtube_live_session(
                streamer.id,
                "video123",
                &streamer.name,
                "YouTube 直播",
                None,
                now,
                now,
            )
            .await
            .unwrap();
        store
            .update_youtube_live_metrics(
                session.id,
                321,
                250,
                3,
                750,
                600,
                Some(12),
                Some(2),
                Some(7),
                Some(2),
                Some(4),
                true,
                "partial",
                "active",
            )
            .await
            .unwrap();
        let (membership, guard_count, peak_high_energy): (Option<i64>, i64, i64) = sqlx::query_as(
            "SELECT membership_count, guard_count, peak_high_energy
             FROM live_sessions WHERE id = ?",
        )
        .bind(session.id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(membership, Some(7));
        assert_eq!(guard_count, 0);
        assert_eq!(peak_high_energy, 0);
        // A restarted collector starts its local counters at zero; cumulative
        // fields must not regress within the same live session.
        store
            .update_youtube_live_metrics(
                session.id,
                321,
                250,
                3,
                750,
                620,
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                true,
                "partial",
                "active",
            )
            .await
            .unwrap();
        let membership_after_restart: Option<i64> =
            sqlx::query_scalar("SELECT membership_count FROM live_sessions WHERE id = ?")
                .bind(session.id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(membership_after_restart, Some(7));
        let page = store
            .list_live_online_samples(session.id, 0, 10)
            .await
            .unwrap();
        assert_eq!(page.platform, Platform::Youtube);
        assert_eq!(page.metric, "youtube_concurrent_viewers");
        assert_eq!(page.membership_count, Some(7));
        assert_eq!(page.stats_available, Some(true));
    }

    #[tokio::test]
    async fn youtube_super_chat_keeps_original_currency_and_never_writes_cny() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&AddSubscription {
                platform: Platform::Youtube,
                mid: "UC456".to_owned(),
                name: Some("Super Chat 测试".to_owned()),
                avatar_url: None,
                room_id: None,
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: true,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
            })
            .await
            .unwrap();
        let now = Utc::now() - chrono::Duration::minutes(5);
        let session = store
            .ensure_youtube_live_session(
                streamer.id,
                "video456",
                &streamer.name,
                "Super Chat 直播",
                None,
                now,
                now,
            )
            .await
            .unwrap();
        let usd = SuperChatEventInput {
            id: "sc-usd".to_owned(),
            kind: "superChatEvent".to_owned(),
            live_chat_id: Some("chat-1".to_owned()),
            created_at: Some(now),
            user_channel_id: Some("user-1".to_owned()),
            user_display_name: Some("Viewer".to_owned()),
            amount_micros: Some(5_000_000),
            currency: Some("USD".to_owned()),
            amount_display_string: Some("$5.00".to_owned()),
            tier: Some(2),
        };
        let jpy = SuperChatEventInput {
            id: "sc-jpy".to_owned(),
            kind: "superStickerEvent".to_owned(),
            live_chat_id: Some("chat-1".to_owned()),
            created_at: Some(now),
            user_channel_id: Some("user-2".to_owned()),
            user_display_name: Some("Viewer 2".to_owned()),
            amount_micros: Some(1_000_000),
            currency: Some("JPY".to_owned()),
            amount_display_string: Some("¥1,000".to_owned()),
            tier: Some(1),
        };
        assert!(store
            .record_youtube_super_chat_event(session.id, &usd)
            .await
            .unwrap());
        assert!(store
            .record_youtube_super_chat_event(session.id, &jpy)
            .await
            .unwrap());
        assert!(!store
            .record_youtube_super_chat_event(session.id, &usd)
            .await
            .unwrap());
        let page = store
            .list_live_online_samples(session.id, 0, 10)
            .await
            .unwrap();
        assert_eq!(page.super_chat_totals.len(), 2);
        assert!(page
            .super_chat_totals
            .iter()
            .any(|total| total.currency.as_deref() == Some("USD")
                && total.amount_micros == 5_000_000
                && total.event_count == 1));
        assert!(page
            .super_chat_totals
            .iter()
            .any(|total| total.currency.as_deref() == Some("JPY")
                && total.amount_micros == 1_000_000
                && total.event_count == 1));
        let events = store
            .list_live_super_chat_events(session.id, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| event.id == "sc-usd"
            && event.currency.as_deref() == Some("USD")
            && event.amount_display_string.as_deref() == Some("$5.00")));
        let (super_chat_count, super_chat_revenue, guard_count): (i64, i64, i64) = sqlx::query_as(
            "SELECT super_chat_count, super_chat_revenue_cents, guard_count
             FROM live_sessions WHERE id = ?",
        )
        .bind(session.id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(super_chat_count, 2);
        assert_eq!(super_chat_revenue, 0);
        assert_eq!(guard_count, 0);
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
