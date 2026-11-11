//! Lightweight broadcast identity and schedule ownership, independent of statistics.
use super::*;
use serde::{Deserialize, Serialize};

pub(crate) const LIVE_RECONNECT_MINUTES: i64 = 30;
pub(crate) const LIVE_SCHEDULE_WINDOW_SECONDS: i64 = 2 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct LiveBroadcast {
    pub id: String,
    pub streamer_id: i64,
    pub room_id: String,
    pub started_at: String,
    pub last_segment_started_at: Option<String>,
    pub last_seen_at: String,
    pub offline_since: Option<String>,
    pub schedule_id: Option<i64>,
    pub schedule_decided: bool,
    pub segment_count: i64,
}

fn room_matches(room_url: Option<&str>, owner: Option<&str>, mid: &str, room: &str) -> bool {
    if let Some(owner) = owner {
        return owner == mid;
    }
    room_url.is_none_or(|value| {
        url::Url::parse(value).is_ok_and(|url| url.path().trim_matches('/') == room)
    })
}

impl SqliteStore {
    pub async fn list_live_broadcasts(&self) -> AppResult<Vec<LiveBroadcast>> {
        Ok(
            sqlx::query_as("SELECT * FROM live_broadcasts ORDER BY rowid")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub(super) async fn observe_live_broadcast_on(
        &self,
        tx: &mut SqliteConnection,
        streamer_id: i64,
        snapshot: &LiveSnapshot,
    ) -> AppResult<Option<LiveBroadcast>> {
        let prior: Option<LiveBroadcast> = sqlx::query_as(
            "SELECT * FROM live_broadcasts WHERE streamer_id = ? ORDER BY julianday(last_seen_at) DESC, julianday(started_at) DESC, rowid DESC LIMIT 1",
        )
        .bind(streamer_id)
        .fetch_optional(&mut *tx)
        .await?;
        let checked = snapshot.checked_at;
        if !snapshot.is_live {
            if let Some(mut prior) = prior {
                if prior.offline_since.is_none() {
                    prior.offline_since = Some(checked.to_rfc3339());
                    sqlx::query("UPDATE live_broadcasts SET offline_since = ? WHERE id = ?")
                        .bind(&prior.offline_since)
                        .bind(&prior.id)
                        .execute(&mut *tx)
                        .await?;
                }
                return Ok(Some(prior));
            }
            return Ok(None);
        }
        let streamer = sqlx::query(
            "SELECT mid, room_id, name, schedule_enabled, live_url FROM streamers WHERE id = ?",
        )
        .bind(streamer_id)
        .fetch_one(&mut *tx)
        .await?;
        let room: Option<String> = streamer.try_get("room_id")?;
        let Some(room) = room.filter(|room| !room.trim().is_empty()) else {
            return Ok(None);
        };
        let room = room.trim().to_owned();
        let mid: String = streamer.try_get("mid")?;
        let raw_start = snapshot.started_at.filter(|start| *start <= checked);
        let segment_start = raw_start.map(|start| start.to_rfc3339());
        let resume = if let Some(prior) = prior.as_ref().filter(|prior| prior.room_id == room) {
            let last_seen = parse_ts(&prior.last_seen_at)?;
            let boundary = parse_opt_ts(prior.offline_since.as_deref())?.unwrap_or(last_seen);
            let gap = raw_start.unwrap_or(checked).signed_duration_since(boundary);
            let within_window = gap >= chrono::Duration::zero()
                && gap <= chrono::Duration::minutes(LIVE_RECONNECT_MINUTES);
            let unchanged =
                segment_start.is_some() && segment_start == prior.last_segment_started_at;
            let supplement = prior.last_segment_started_at.is_none()
                && prior.offline_since.is_none()
                && raw_start.is_some_and(|start| start <= last_seen);
            let missing_continuous = raw_start.is_none()
                && prior.offline_since.is_none()
                && checked.signed_duration_since(last_seen)
                    <= chrono::Duration::minutes(LIVE_RECONNECT_MINUTES);
            unchanged || supplement || within_window || missing_continuous
        } else {
            false
        };
        let mut broadcast = if resume {
            let mut prior = prior.expect("resume requires a previous broadcast");
            let new_segment = (segment_start.is_some()
                && prior.last_segment_started_at.is_some()
                && segment_start != prior.last_segment_started_at)
                || prior.offline_since.is_some();
            if new_segment {
                prior.segment_count += 1;
            }
            if segment_start.is_some() {
                prior.last_segment_started_at = segment_start;
            }
            prior.last_seen_at = checked.to_rfc3339();
            prior.offline_since = None;
            prior
        } else {
            LiveBroadcast {
                id: uuid::Uuid::new_v4().to_string(),
                streamer_id,
                room_id: room.clone(),
                started_at: raw_start.unwrap_or(checked).to_rfc3339(),
                last_segment_started_at: segment_start,
                last_seen_at: checked.to_rfc3339(),
                offline_since: None,
                schedule_id: None,
                schedule_decided: false,
                segment_count: 1,
            }
        };
        let enabled: bool = streamer.try_get("schedule_enabled")?;
        if enabled && !broadcast.schedule_decided {
            let start = parse_ts(&broadcast.started_at)?;
            let rows = sqlx::query(
                "SELECT id, room_url, room_owner_mid FROM schedules s
                 WHERE streamer_id = ? AND cancelled = 0
                   AND ABS(unixepoch(start_at) - unixepoch(?)) <= ?
                   AND NOT EXISTS (SELECT 1 FROM live_broadcasts b WHERE b.schedule_id = s.id)
                 ORDER BY ABS(unixepoch(start_at) - unixepoch(?)), unixepoch(start_at), id",
            )
            .bind(streamer_id)
            .bind(&broadcast.started_at)
            .bind(LIVE_SCHEDULE_WINDOW_SECONDS)
            .bind(&broadcast.started_at)
            .fetch_all(&mut *tx)
            .await?;
            for row in rows {
                let url: Option<String> = row.try_get("room_url")?;
                let owner: Option<String> = row.try_get("room_owner_mid")?;
                if room_matches(url.as_deref(), owner.as_deref(), &mid, &room) {
                    broadcast.schedule_id = Some(row.try_get("id")?);
                    break;
                }
            }
            if broadcast.schedule_id.is_none() {
                let base: String = snapshot.title.trim().chars().take(200).collect();
                let base = if base.is_empty() {
                    "直播".to_owned()
                } else {
                    base
                };
                let mut title = base.clone();
                let mut suffix = 0;
                loop {
                    let collision: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM schedules WHERE streamer_id=? AND start_at=? AND title=?)")
                        .bind(streamer_id).bind(&broadcast.started_at).bind(&title).fetch_one(&mut *tx).await?;
                    if !collision {
                        break;
                    }
                    suffix += 1;
                    let tail = format!("（开播补录{suffix}）");
                    title = base
                        .chars()
                        .take(200 - tail.chars().count())
                        .collect::<String>()
                        + &tail;
                }
                let live_url: Option<String> = streamer.try_get("live_url")?;
                let input = NewSchedule {
                    streamer_id,
                    participants: Vec::new(),
                    title,
                    start_at: start,
                    end_at: None,
                    room_url: Some(
                        snapshot
                            .live_url
                            .clone()
                            .or(live_url)
                            .unwrap_or_else(|| format!("https://live.bilibili.com/{room}")),
                    ),
                    room_owner_mid: Some(mid.clone()),
                    room_owner_name: Some(streamer.try_get("name")?),
                    source_kind: "live".to_owned(),
                    source_post_id: None,
                    source_image_url: None,
                    source_author_mid: Some(mid.clone()),
                    source_fingerprint: Some(broadcast.id.clone()),
                    source_identity: Some(format!("live:{}", broadcast.id)),
                    source_published_at: None,
                    confidence: 1.0,
                };
                broadcast.schedule_id = Some(self.upsert_schedule_on(tx, &input).await?.0);
                self.append_event_on(tx, "schedule_changed", &json!({"streamerId": streamer_id}))
                    .await?;
            }
            broadcast.schedule_decided = true;
        }
        sqlx::query(
            "INSERT INTO live_broadcasts (id, streamer_id, room_id, started_at, last_segment_started_at,
                last_seen_at, offline_since, schedule_id, schedule_decided, segment_count)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET last_segment_started_at=excluded.last_segment_started_at,
                last_seen_at=excluded.last_seen_at, offline_since=excluded.offline_since,
                schedule_id=excluded.schedule_id, schedule_decided=excluded.schedule_decided,
                segment_count=excluded.segment_count",
        ).bind(&broadcast.id).bind(streamer_id).bind(&room).bind(&broadcast.started_at)
            .bind(&broadcast.last_segment_started_at).bind(&broadcast.last_seen_at)
            .bind(&broadcast.offline_since).bind(broadcast.schedule_id).bind(broadcast.schedule_decided)
            .bind(broadcast.segment_count).execute(&mut *tx).await?;
        Ok(Some(broadcast))
    }

    pub async fn merge_late_live_schedule(&self, input: &NewSchedule) -> AppResult<bool> {
        if !matches!(
            input.source_kind.as_str(),
            "text" | "image" | "reserve" | "editor"
        ) {
            return Ok(false);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(
            "SELECT s.id, st.mid, b.room_id FROM live_broadcasts b
             JOIN schedules s ON s.id=b.schedule_id JOIN streamers st ON st.id=b.streamer_id
             WHERE b.streamer_id=? AND s.source_kind='live' AND s.cancelled=0
               AND ABS(unixepoch(b.started_at)-unixepoch(?)) <= ?
             ORDER BY ABS(unixepoch(b.started_at)-unixepoch(?)), unixepoch(b.started_at), s.id",
        )
        .bind(input.streamer_id)
        .bind(input.start_at.to_rfc3339())
        .bind(LIVE_SCHEDULE_WINDOW_SECONDS)
        .bind(input.start_at.to_rfc3339())
        .fetch_all(&mut *tx)
        .await?;
        for row in rows {
            let mid: String = row.try_get("mid")?;
            let room: String = row.try_get("room_id")?;
            if !room_matches(
                input.room_url.as_deref(),
                input.room_owner_mid.as_deref(),
                &mid,
                &room,
            ) {
                continue;
            }
            let id: i64 = row.try_get("id")?;
            self.attach_schedule_source_on(&mut tx, id, input, SyncActor::collector())
                .await?;
            tx.commit().await?;
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn ensure_broadcast_live_session(
        &self,
        streamer_id: i64,
        title: &str,
        cover: Option<&str>,
        checked: DateTime<Utc>,
    ) -> AppResult<LiveSession> {
        let broadcast: LiveBroadcast = sqlx::query_as(
            "SELECT * FROM live_broadcasts WHERE streamer_id=? ORDER BY julianday(last_seen_at) DESC, julianday(started_at) DESC, rowid DESC LIMIT 1",
        )
        .bind(streamer_id)
        .fetch_one(&self.pool)
        .await?;
        let name: String = sqlx::query_scalar("SELECT name FROM streamers WHERE id=?")
            .bind(streamer_id)
            .fetch_one(&self.pool)
            .await?;
        // Exact canonical identity: statistics never run a second independent merge decision.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM live_sessions WHERE streamer_id=? AND started_at=?")
                .bind(streamer_id)
                .bind(&broadcast.started_at)
                .fetch_optional(&mut *tx)
                .await?;
        let id = if let Some(id) = existing {
            id
        } else {
            sqlx::query(
                "INSERT INTO live_sessions (streamer_id, streamer_name, room_id, title,
                cover_url, started_at, last_segment_started_at, created_at, updated_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(streamer_id)
            .bind(&name)
            .bind(&broadcast.room_id)
            .bind(title)
            .bind(cover)
            .bind(&broadcast.started_at)
            .bind(
                broadcast
                    .last_segment_started_at
                    .as_deref()
                    .unwrap_or(&broadcast.started_at),
            )
            .bind(checked.to_rfc3339())
            .bind(checked.to_rfc3339())
            .execute(&mut *tx)
            .await?
            .last_insert_rowid()
        };
        let changed = sqlx::query("UPDATE live_sessions SET schedule_id=?, title=?,
            cover_url=COALESCE(?,cover_url), ended_at=NULL, collector_state='active',
            data_quality=CASE WHEN segment_count < ? THEN 'partial' ELSE data_quality END,
            segment_count=?, last_segment_started_at=?, updated_at=?, revision=revision+1
            WHERE id=? AND (schedule_id IS NOT ? OR title<>? OR (? IS NOT NULL AND cover_url IS NOT ?)
                OR ended_at IS NOT NULL OR collector_state<>'active' OR segment_count<>?)")
            .bind(broadcast.schedule_id).bind(title).bind(cover).bind(broadcast.segment_count)
            .bind(broadcast.segment_count).bind(broadcast.last_segment_started_at.as_deref().unwrap_or(&broadcast.started_at))
            .bind(checked.to_rfc3339()).bind(id).bind(broadcast.schedule_id).bind(title)
            .bind(cover).bind(cover).bind(broadcast.segment_count).execute(&mut *tx).await?;
        if existing.is_none() || changed.rows_affected() > 0 {
            self.emit_live_session_change(&mut tx, id, "upsert", SyncActor::collector())
                .await?;
        }
        tx.commit().await?;
        self.get_live_session(id).await
    }

    pub(super) async fn clear_live_schedule_status_on(
        &self,
        tx: &mut SqliteConnection,
        id: i64,
        actor: SyncActor<'_>,
    ) -> AppResult<()> {
        let owners: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM streamers WHERE live_schedule_id=?")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        sqlx::query("UPDATE streamers SET live_schedule_id=NULL, live_revision=live_revision+1 WHERE live_schedule_id=?")
            .bind(id).execute(&mut *tx).await?;
        for owner in owners {
            self.emit_live_status_change(tx, owner, actor).await?;
        }
        Ok(())
    }

    async fn append_event_on(
        &self,
        tx: &mut SqliteConnection,
        kind: &str,
        payload: &Value,
    ) -> AppResult<()> {
        sqlx::query("INSERT INTO event_journal(type,payload_json,created_at) VALUES (?,?,?)")
            .bind(kind)
            .bind(payload.to_string())
            .bind(Utc::now().to_rfc3339())
            .execute(tx)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn time() -> DateTime<Utc> {
        "2026-11-11T12:00:00Z".parse().unwrap()
    }
    async fn owner(store: &SqliteStore) -> Streamer {
        store
            .add_subscription(&AddSubscription {
                mid: "100".into(),
                name: Some("直播主播".into()),
                avatar_url: None,
                room_id: Some("12345".into()),
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: false,
                live_stats_enabled: false,
            })
            .await
            .unwrap()
    }
    fn snapshot(
        started: Option<DateTime<Utc>>,
        checked: DateTime<Utc>,
        live: bool,
    ) -> LiveSnapshot {
        LiveSnapshot {
            is_live: live,
            title: "无预告直播".into(),
            cover_url: None,
            live_url: Some("https://live.bilibili.com/12345".into()),
            online: 0,
            started_at: started,
            checked_at: checked,
        }
    }
    fn schedule(owner: i64, start: DateTime<Utc>) -> NewSchedule {
        NewSchedule {
            streamer_id: owner,
            participants: Vec::new(),
            title: "预告".into(),
            start_at: start,
            end_at: None,
            room_url: None,
            room_owner_mid: None,
            room_owner_name: None,
            source_kind: "reserve".into(),
            source_post_id: None,
            source_image_url: None,
            source_author_mid: None,
            source_fingerprint: None,
            source_identity: None,
            source_published_at: None,
            confidence: 1.0,
        }
    }
    async fn status(store: &SqliteStore) -> LiveStatus {
        store.list_live_statuses().await.unwrap().remove(0)
    }

    #[tokio::test]
    async fn unannounced_live_creates_today_card_without_statistics_or_notifications() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        let yesterday = store
            .upsert_schedule(&schedule(owner.id, time() - Duration::days(1)))
            .await
            .unwrap()
            .0;
        let event = snapshot(Some(time()), time(), true);
        store.set_live_status(owner.id, &event).await.unwrap();
        let id = status(&store).await.schedule_id.unwrap();
        assert_ne!(id, yesterday.id);
        let card = store.get_schedule(id).await.unwrap();
        assert_eq!(card.source_kind, "live");
        assert_eq!(card.start_at, time());
        assert_eq!(card.title, "无预告直播");
        assert!(store.list_live_sessions(None, 10).await.unwrap().is_empty());
        for _ in 0..3 {
            store.set_live_status(owner.id, &event).await.unwrap();
        }
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 2);
        store
            .set_live_status(
                owner.id,
                &snapshot(None, time() + Duration::hours(2), false),
            )
            .await
            .unwrap();
        assert_eq!(store.get_schedule(id).await.unwrap().id, id);
        assert!(!status(&store).await.is_live);
    }

    #[tokio::test]
    async fn schedule_window_is_inclusive_and_does_not_take_cancelled_or_foreign_rooms() {
        for seconds in [-7201, -7200, 7200, 7201] {
            let store = SqliteStore::in_memory().await.unwrap();
            let owner = owner(&store).await;
            let card = store
                .upsert_schedule(&schedule(owner.id, time() + Duration::seconds(seconds)))
                .await
                .unwrap()
                .0;
            store
                .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
                .await
                .unwrap();
            assert_eq!(
                status(&store).await.schedule_id == Some(card.id),
                seconds.abs() <= 7200,
                "{seconds}"
            );
        }
        for cancelled in [false, true] {
            let store = SqliteStore::in_memory().await.unwrap();
            let owner = owner(&store).await;
            let mut input = schedule(owner.id, time());
            if !cancelled {
                input.room_url = Some("https://live.bilibili.com/99999".into());
            }
            let card = store.upsert_schedule(&input).await.unwrap().0;
            if cancelled {
                store.cancel_schedule(card.id).await.unwrap();
            }
            store
                .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
                .await
                .unwrap();
            assert_ne!(status(&store).await.schedule_id, Some(card.id));
        }
    }

    #[tokio::test]
    async fn reconnect_boundaries_share_card_and_statistics_identity() {
        for seconds in [29 * 60, 30 * 60, 30 * 60 + 1] {
            let store = SqliteStore::in_memory().await.unwrap();
            let owner = owner(&store).await;
            store
                .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
                .await
                .unwrap();
            let first = status(&store).await.schedule_id;
            let session = store
                .ensure_broadcast_live_session(owner.id, "直播", None, time())
                .await
                .unwrap();
            let ended = time() + Duration::hours(1);
            store
                .set_live_status(owner.id, &snapshot(None, ended, false))
                .await
                .unwrap();
            store
                .finish_open_live_session(session.id, ended)
                .await
                .unwrap();
            let restarted = ended + Duration::seconds(seconds);
            store
                .set_live_status(owner.id, &snapshot(Some(restarted), restarted, true))
                .await
                .unwrap();
            let next = store
                .ensure_broadcast_live_session(owner.id, "直播", None, restarted)
                .await
                .unwrap();
            assert_eq!(next.id == session.id, seconds <= 1800);
            assert_eq!(status(&store).await.schedule_id == first, seconds <= 1800);
            assert_eq!(next.segment_count, if seconds <= 1800 { 2 } else { 1 });
            store
                .set_live_status(
                    owner.id,
                    &snapshot(Some(restarted), restarted + Duration::seconds(20), true),
                )
                .await
                .unwrap();
            let again = store
                .ensure_broadcast_live_session(
                    owner.id,
                    "改标题",
                    None,
                    restarted + Duration::seconds(20),
                )
                .await
                .unwrap();
            assert_eq!(next.id, again.id);
            assert_eq!(next.segment_count, again.segment_count);
        }
    }

    #[tokio::test]
    async fn offline_polling_does_not_extend_window_and_missing_offline_is_bounded() {
        for observed_offline in [true, false] {
            let store = SqliteStore::in_memory().await.unwrap();
            let owner = owner(&store).await;
            store
                .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
                .await
                .unwrap();
            let first = status(&store).await.schedule_id;
            if observed_offline {
                for minutes in [1, 10, 29, 31] {
                    store
                        .set_live_status(
                            owner.id,
                            &snapshot(None, time() + Duration::minutes(minutes), false),
                        )
                        .await
                        .unwrap();
                }
                assert_eq!(
                    store.list_live_broadcasts().await.unwrap()[0].offline_since,
                    Some((time() + Duration::minutes(1)).to_rfc3339())
                );
            }
            let next = time() + Duration::minutes(32);
            store
                .set_live_status(owner.id, &snapshot(Some(next), next, true))
                .await
                .unwrap();
            assert_ne!(status(&store).await.schedule_id, first);
        }
    }

    #[tokio::test]
    async fn missing_start_then_platform_time_preserves_first_detection_and_cross_midnight() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        let start: DateTime<Utc> = "2026-11-11T15:55:00Z".parse().unwrap();
        store
            .set_live_status(owner.id, &snapshot(None, start, true))
            .await
            .unwrap();
        let first = status(&store).await.schedule_id.unwrap();
        store
            .set_live_status(
                owner.id,
                &snapshot(None, start + Duration::minutes(4), true),
            )
            .await
            .unwrap();
        let raw = start - Duration::minutes(2);
        for minutes in [10, 120, 600] {
            store
                .set_live_status(
                    owner.id,
                    &snapshot(Some(raw), start + Duration::minutes(minutes), true),
                )
                .await
                .unwrap();
            assert_eq!(status(&store).await.schedule_id, Some(first));
        }
        assert_eq!(store.get_schedule(first).await.unwrap().start_at, start);
        assert_eq!(store.list_live_broadcasts().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deletion_cancellation_and_manual_edits_are_not_undone() {
        for delete in [false, true] {
            let store = SqliteStore::in_memory().await.unwrap();
            let owner = owner(&store).await;
            store
                .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
                .await
                .unwrap();
            let id = status(&store).await.schedule_id.unwrap();
            store
                .update_schedule(
                    id,
                    &UpdateSchedule {
                        title: Some("我的标题".into()),
                        start_at: None,
                        end_at: None,
                        room_url: None,
                    },
                )
                .await
                .unwrap();
            let mut event = snapshot(Some(time()), time() + Duration::minutes(1), true);
            event.title = "新直播标题".into();
            store.set_live_status(owner.id, &event).await.unwrap();
            assert_eq!(store.get_schedule(id).await.unwrap().title, "我的标题");
            if delete {
                store.delete_schedule(id).await.unwrap();
            } else {
                store.cancel_schedule(id).await.unwrap();
            }
            assert_eq!(status(&store).await.schedule_id, None);
            store
                .set_live_status(
                    owner.id,
                    &snapshot(None, time() + Duration::minutes(2), false),
                )
                .await
                .unwrap();
            let next = time() + Duration::minutes(5);
            store
                .set_live_status(owner.id, &snapshot(Some(next), next, true))
                .await
                .unwrap();
            assert_eq!(status(&store).await.schedule_id, None);
            assert_eq!(
                store.list_all_schedules().await.unwrap().len(),
                usize::from(!delete)
            );
        }
    }

    #[tokio::test]
    async fn late_announcement_attaches_without_replacing_card_or_manual_title() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        store
            .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
            .await
            .unwrap();
        let id = status(&store).await.schedule_id.unwrap();
        store
            .update_schedule(
                id,
                &UpdateSchedule {
                    title: Some("用户标题".into()),
                    start_at: None,
                    end_at: None,
                    room_url: None,
                },
            )
            .await
            .unwrap();
        let mut input = schedule(owner.id, time() + Duration::hours(2));
        input.source_identity = Some("reserve:late".into());
        assert!(store.merge_late_live_schedule(&input).await.unwrap());
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 1);
        assert_eq!(store.get_schedule(id).await.unwrap().title, "用户标题");
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM schedule_sources WHERE schedule_id=? AND source_identity='reserve:late'")
            .bind(id).fetch_one(&store.pool).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn concurrent_observations_are_idempotent_and_stale_results_are_ignored() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        let event = snapshot(Some(time()), time(), true);
        let (a, b) = tokio::join!(
            store.set_live_status(owner.id, &event),
            store.set_live_status(owner.id, &event)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 1);
        store
            .set_live_status(
                owner.id,
                &snapshot(None, time() - Duration::seconds(1), false),
            )
            .await
            .unwrap();
        assert!(status(&store).await.is_live);
        assert_eq!(
            store.list_live_broadcasts().await.unwrap()[0].segment_count,
            1
        );
    }

    #[tokio::test]
    async fn schedule_switch_and_statistics_lookup_use_persistent_ownership() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        sqlx::query("UPDATE streamers SET schedule_enabled=0 WHERE id=?")
            .bind(owner.id)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
            .await
            .unwrap();
        assert!(store.list_all_schedules().await.unwrap().is_empty());
        sqlx::query("UPDATE streamers SET schedule_enabled=1 WHERE id=?")
            .bind(owner.id)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .set_live_status(
                owner.id,
                &snapshot(Some(time()), time() + Duration::hours(5), true),
            )
            .await
            .unwrap();
        let id = status(&store).await.schedule_id.unwrap();
        let session = store
            .ensure_broadcast_live_session(owner.id, "直播", None, time() + Duration::hours(5))
            .await
            .unwrap();
        store
            .update_schedule(
                id,
                &UpdateSchedule {
                    title: None,
                    start_at: Some(time() - Duration::days(1)),
                    end_at: None,
                    room_url: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .find_live_session_for_schedule(id)
                .await
                .unwrap()
                .unwrap()
                .id,
            session.id
        );
        let other = store
            .upsert_schedule(&schedule(owner.id, time()))
            .await
            .unwrap()
            .0;
        assert!(store
            .find_live_session_for_schedule(other.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn title_normalization_and_disk_restart_preserve_identity() {
        let dir = std::env::temp_dir().join(format!("oshical-broadcast-{}", uuid::Uuid::new_v4()));
        let path = dir.join("data.db");
        let store = SqliteStore::connect(&path).await.unwrap();
        let owner = owner(&store).await;
        let mut event = snapshot(Some(time()), time(), true);
        event.title = " ".into();
        store.set_live_status(owner.id, &event).await.unwrap();
        let id = status(&store).await.schedule_id.unwrap();
        assert_eq!(store.get_schedule(id).await.unwrap().title, "直播");
        store.pool.close().await;
        drop(store);
        let store = SqliteStore::connect(&path).await.unwrap();
        event.checked_at = time() + Duration::minutes(1);
        event.title = "长".repeat(201);
        store.set_live_status(owner.id, &event).await.unwrap();
        assert_eq!(status(&store).await.schedule_id, Some(id));
        store
            .set_live_status(
                owner.id,
                &snapshot(None, time() + Duration::minutes(2), false),
            )
            .await
            .unwrap();
        event.started_at = Some(time() + Duration::hours(1));
        event.checked_at = time() + Duration::hours(1);
        store.set_live_status(owner.id, &event).await.unwrap();
        let next = status(&store).await.schedule_id.unwrap();
        assert_ne!(id, next);
        assert_eq!(
            store
                .get_schedule(next)
                .await
                .unwrap()
                .title
                .chars()
                .count(),
            200
        );
        store.pool.close().await;
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn same_platform_start_and_network_failures_do_not_split_a_long_broadcast() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        store
            .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
            .await
            .unwrap();
        let first = status(&store).await.schedule_id;
        store
            .record_streamer_error(owner.id, "请求超时")
            .await
            .unwrap();
        assert_eq!(
            store.list_live_broadcasts().await.unwrap()[0].offline_since,
            None
        );
        store
            .set_live_status(
                owner.id,
                &snapshot(None, time() + Duration::hours(1), false),
            )
            .await
            .unwrap();
        store
            .set_live_status(
                owner.id,
                &snapshot(Some(time()), time() + Duration::hours(2), true),
            )
            .await
            .unwrap();
        assert_eq!(status(&store).await.schedule_id, first);
        assert_eq!(store.list_live_broadcasts().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn transaction_rolls_back_state_card_sources_and_events_together() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        let before = store.latest_event_seq().await.unwrap();
        let mut event = snapshot(Some(time()), time(), true);
        event.live_url = Some("javascript:alert(1)".into());
        assert!(store.set_live_status(owner.id, &event).await.is_err());
        assert!(store.list_all_schedules().await.unwrap().is_empty());
        assert!(store.list_live_broadcasts().await.unwrap().is_empty());
        assert!(!status(&store).await.is_live);
        assert_eq!(store.latest_event_seq().await.unwrap(), before);
        let sources: i64 = sqlx::query_scalar("SELECT count(*) FROM schedule_sources")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(sources, 0);
    }

    #[tokio::test]
    async fn cancelled_natural_key_is_not_resurrected_and_owned_cards_are_not_reused() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        let mut input = schedule(owner.id, time());
        input.title = "无预告直播".into();
        let cancelled = store.upsert_schedule(&input).await.unwrap().0;
        store.cancel_schedule(cancelled.id).await.unwrap();
        store
            .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
            .await
            .unwrap();
        let first = status(&store).await.schedule_id.unwrap();
        assert_ne!(first, cancelled.id);
        assert!(store.get_schedule(cancelled.id).await.unwrap().cancelled);
        store
            .set_live_status(
                owner.id,
                &snapshot(None, time() + Duration::minutes(1), false),
            )
            .await
            .unwrap();
        let next = time() + Duration::minutes(32);
        store
            .set_live_status(owner.id, &snapshot(Some(next), next, true))
            .await
            .unwrap();
        let second = status(&store).await.schedule_id.unwrap();
        assert_ne!(first, second);
        assert_eq!(store.list_live_broadcasts().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reused_previews_are_selected_by_distance_then_time_then_id() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        let later = store
            .upsert_schedule(&schedule(owner.id, time() + Duration::minutes(10)))
            .await
            .unwrap()
            .0;
        let earlier = store
            .upsert_schedule(&schedule(owner.id, time() - Duration::minutes(10)))
            .await
            .unwrap()
            .0;
        let mut duplicate = schedule(owner.id, earlier.start_at);
        duplicate.title = "同一时间另一预告".into();
        let duplicate = store.upsert_schedule(&duplicate).await.unwrap().0;
        store
            .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
            .await
            .unwrap();
        assert!(earlier.id < duplicate.id);
        assert_ne!(earlier.id, later.id);
        assert_eq!(status(&store).await.schedule_id, Some(earlier.id));
        assert_eq!(store.list_all_schedules().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn statistics_reconnect_preserves_counters_and_emits_schedule_ownership() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        store
            .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
            .await
            .unwrap();
        let first = store
            .ensure_broadcast_live_session(owner.id, "直播", None, time())
            .await
            .unwrap();
        store
            .update_live_session_stats(
                first.id,
                &LiveSessionStats {
                    danmaku_count: 786,
                    revenue_cents: 21670,
                    data_quality: "partial".into(),
                    collector_state: "active".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ended = time() + Duration::minutes(5);
        store
            .set_live_status(owner.id, &snapshot(None, ended, false))
            .await
            .unwrap();
        store
            .finish_open_live_session(first.id, ended)
            .await
            .unwrap();
        let resumed = ended + Duration::minutes(29);
        store
            .set_live_status(owner.id, &snapshot(Some(resumed), resumed, true))
            .await
            .unwrap();
        let second = store
            .ensure_broadcast_live_session(owner.id, "直播", None, resumed)
            .await
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.danmaku_count, 786);
        assert_eq!(second.revenue_cents, 21670);
        let bootstrap = store.sync_bootstrap().await.unwrap();
        assert_eq!(bootstrap.live_statuses[0].schedule_id, second.schedule_id);
        assert_eq!(bootstrap.live_sessions[0].schedule_id, second.schedule_id);
    }

    async fn exported(store: &SqliteStore) -> ServerSnapshot {
        let bootstrap = store.sync_bootstrap().await.unwrap();
        ServerSnapshot {
            schema_version: SERVER_SNAPSHOT_SCHEMA_VERSION,
            subscriptions: bootstrap.subscriptions,
            schedules: bootstrap.schedules,
            weekly_rules: bootstrap.weekly_rules,
            weekly_exceptions: bootstrap.weekly_exceptions,
            schedule_sources: bootstrap.schedule_sources,
            source_suppressions: bootstrap.source_suppressions,
            live_broadcasts: store.list_live_broadcasts().await.unwrap(),
            latest_event_seq: store.latest_event_seq().await.unwrap(),
            exported_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn snapshot_roundtrip_remaps_ids_and_retains_deleted_card_suppression() {
        for deleted in [false, true] {
            let source = SqliteStore::in_memory().await.unwrap();
            let owner = owner(&source).await;
            source
                .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
                .await
                .unwrap();
            let original = status(&source).await.schedule_id.unwrap();
            if deleted {
                source.delete_schedule(original).await.unwrap();
            }
            let snapshot_data = exported(&source).await;
            let target = SqliteStore::in_memory().await.unwrap();
            let filler: AddSubscription =
                serde_json::from_value(json!({"mid":"999","name":"占位主播"})).unwrap();
            let filler = target.add_subscription(&filler).await.unwrap();
            target
                .upsert_schedule(&schedule(filler.id, time()))
                .await
                .unwrap();
            target
                .import_snapshot(&snapshot_data, "server", "https://fixture.invalid")
                .await
                .unwrap();
            let imported = target
                .list_streamers()
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.mid == "100")
                .unwrap();
            assert_ne!(imported.id, owner.id);
            let states = target.list_live_broadcasts().await.unwrap();
            assert_eq!(states[0].id, snapshot_data.live_broadcasts[0].id);
            assert_eq!(states[0].streamer_id, imported.id);
            if deleted {
                assert_eq!(states[0].schedule_id, None);
            } else {
                assert_ne!(states[0].schedule_id, Some(original));
            }
            target
                .set_live_status(
                    imported.id,
                    &snapshot(Some(time()), time() + Duration::minutes(1), true),
                )
                .await
                .unwrap();
            let rows = target.list_all_schedules().await.unwrap();
            assert_eq!(rows.len(), if deleted { 1 } else { 2 });
            assert_eq!(target.list_live_broadcasts().await.unwrap().len(), 1);
            assert_eq!(
                target
                    .list_live_statuses()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|s| s.streamer_id == imported.id)
                    .unwrap()
                    .schedule_id,
                states[0].schedule_id
            );
        }
    }

    #[tokio::test]
    async fn snapshot_validation_legacy_defaults_and_state_digest_cover_broadcasts() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        store
            .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
            .await
            .unwrap();
        let snapshot_data = exported(&store).await;
        let mut legacy = serde_json::to_value(&snapshot_data).unwrap();
        legacy.as_object_mut().unwrap().remove("liveBroadcasts");
        assert!(serde_json::from_value::<ServerSnapshot>(legacy)
            .unwrap()
            .live_broadcasts
            .is_empty());
        let before = store.migration_state_digest().await.unwrap();
        sqlx::query("UPDATE live_broadcasts SET segment_count=2")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_ne!(before, store.migration_state_digest().await.unwrap());
        let mut invalid = snapshot_data;
        invalid.live_broadcasts[0].schedule_id = Some(99999);
        let target = SqliteStore::in_memory().await.unwrap();
        assert!(target
            .import_snapshot(&invalid, "server", "https://fixture.invalid")
            .await
            .is_err());
        assert!(target.list_streamers().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn snapshot_conflict_policy_matches_existing_server_client_semantics() {
        let store = SqliteStore::in_memory().await.unwrap();
        let owner = owner(&store).await;
        store
            .set_live_status(owner.id, &snapshot(Some(time()), time(), true))
            .await
            .unwrap();
        let mut snapshot_data = exported(&store).await;
        snapshot_data.live_broadcasts[0].segment_count = 9;
        assert!(store
            .preview_snapshot(&snapshot_data)
            .await
            .unwrap()
            .conflicts
            .iter()
            .any(|c| c.entity == "live_broadcast"));
        store
            .import_snapshot(&snapshot_data, "client", "https://fixture.invalid")
            .await
            .unwrap();
        assert_eq!(
            store.list_live_broadcasts().await.unwrap()[0].segment_count,
            1
        );
        store
            .import_snapshot(&snapshot_data, "server", "https://fixture.invalid")
            .await
            .unwrap();
        assert_eq!(
            store.list_live_broadcasts().await.unwrap()[0].segment_count,
            9
        );
    }
}
