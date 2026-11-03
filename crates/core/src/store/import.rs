use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSnapshot {
    pub subscriptions: Vec<Streamer>,
    pub schedules: Vec<Schedule>,
    #[serde(default)]
    pub weekly_rules: Vec<WeeklyScheduleRule>,
    pub latest_event_seq: i64,
    pub exported_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportConflict {
    pub entity: String,
    pub label: String,
    pub local_id: i64,
    pub remote_id: i64,
    pub local: Value,
    pub remote: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    pub subscriptions: usize,
    pub schedules: usize,
    pub weekly_rules: usize,
    pub conflicts: Vec<ImportConflict>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub subscriptions: usize,
    pub schedules: usize,
    pub weekly_rules: usize,
    pub skipped: usize,
}

fn validate_snapshot(snapshot: &ServerSnapshot) -> AppResult<()> {
    if snapshot.subscriptions.len() > 1000
        || snapshot.schedules.len() > 20_000
        || snapshot.weekly_rules.len() > 5000
    {
        return Err(AppError::validation("快照超过导入大小限制"));
    }
    let mut ids = HashSet::new();
    let mut mids = HashSet::new();
    for streamer in &snapshot.subscriptions {
        validate_mid(&streamer.mid)?;
        if !ids.insert(streamer.id) || !mids.insert(&streamer.mid) {
            return Err(AppError::validation("快照含重复订阅 ID 或 MID"));
        }
        if streamer.name.trim().is_empty() || streamer.name.chars().count() > 80 {
            return Err(AppError::validation("快照主播名称无效"));
        }
        for link in &streamer.linked_accounts {
            validate_mid(&link.mid)?;
        }
    }
    let mut schedule_ids = HashSet::new();
    for schedule in &snapshot.schedules {
        if !ids.contains(&schedule.streamer_id) || !schedule_ids.insert(schedule.id) {
            return Err(AppError::validation("快照日程归属缺失或 ID 重复"));
        }
        validate_schedule_input(&new_schedule(schedule, schedule.streamer_id))?;
    }
    let mut rule_keys = HashSet::new();
    for rule in &snapshot.weekly_rules {
        if !ids.contains(&rule.streamer_id) || !rule_keys.insert(&rule.rule_key) {
            return Err(AppError::validation("快照固定周表归属缺失或规则键重复"));
        }
        crate::weekly::validate_rule_fields(
            &rule.title,
            rule.weekdays_mask,
            &rule.start_time,
            rule.end_time.as_deref(),
            &rule.timezone,
            rule.room_url.as_deref(),
            &rule.effective_from,
            rule.effective_until.as_deref(),
        )?;
    }
    Ok(())
}

fn new_schedule(remote: &Schedule, local_streamer: i64) -> NewSchedule {
    NewSchedule {
        streamer_id: local_streamer,
        participants: remote.participants.clone(),
        title: remote.title.clone(),
        start_at: remote.start_at,
        end_at: remote.end_at,
        room_url: remote.room_url.clone(),
        room_owner_mid: remote.room_owner_mid.clone(),
        room_owner_name: remote.room_owner_name.clone(),
        source_kind: remote.source_kind.clone(),
        source_post_id: remote.source_post_id.clone(),
        source_image_url: remote.source_image_url.clone(),
        source_author_mid: remote.source_author_mid.clone(),
        source_fingerprint: remote.source_fingerprint.clone(),
        source_identity: remote.source_identity.clone(),
        source_published_at: remote.source_published_at,
        confidence: remote.confidence,
    }
}

fn streamer_values(value: &Streamer) -> Value {
    json!({"name":value.name,"roomId":value.room_id,"scheduleEnabled":value.schedule_enabled,
        "liveNotifyEnabled":value.live_notify_enabled,"liveStatsEnabled":value.live_stats_enabled,"aliases":value.aliases})
}
fn schedule_values(value: &Schedule) -> Value {
    json!({"participants":value.participants,"title":value.title,"startAt":value.start_at,
        "endAt":value.end_at,"roomUrl":value.room_url,"roomOwnerMid":value.room_owner_mid,
        "roomOwnerName":value.room_owner_name,"sourcePublishedAt":value.source_published_at,
        "manualLocked":value.manual_locked,"cancelled":value.cancelled})
}
fn same_schedule(local: &Schedule, remote: &Schedule) -> bool {
    (local.source_identity.is_some() && local.source_identity == remote.source_identity)
        || (local.title == remote.title && local.start_at == remote.start_at)
}

impl SqliteStore {
    pub async fn preview_snapshot(&self, snapshot: &ServerSnapshot) -> AppResult<ImportPreview> {
        validate_snapshot(snapshot)?;
        let streamers = self.list_streamers().await?;
        let schedules = self.list_all_schedules().await?;
        let mut conflicts = Vec::new();
        for remote in &snapshot.subscriptions {
            if let Some(local) = streamers.iter().find(|local| local.mid == remote.mid) {
                if streamer_values(local) != streamer_values(remote) {
                    conflicts.push(ImportConflict {
                        entity: "subscription".into(),
                        label: remote.name.clone(),
                        local_id: local.id,
                        remote_id: remote.id,
                        local: streamer_values(local),
                        remote: streamer_values(remote),
                    });
                }
                for remote_schedule in snapshot
                    .schedules
                    .iter()
                    .filter(|item| item.streamer_id == remote.id)
                {
                    if let Some(local_schedule) = schedules.iter().find(|item| {
                        item.streamer_id == local.id && same_schedule(item, remote_schedule)
                    }) {
                        if schedule_values(local_schedule) != schedule_values(remote_schedule) {
                            conflicts.push(ImportConflict {
                                entity: "schedule".into(),
                                label: remote_schedule.title.clone(),
                                local_id: local_schedule.id,
                                remote_id: remote_schedule.id,
                                local: schedule_values(local_schedule),
                                remote: schedule_values(remote_schedule),
                            });
                        }
                    }
                }
            }
        }
        Ok(ImportPreview {
            subscriptions: snapshot.subscriptions.len(),
            schedules: snapshot.schedules.len(),
            weekly_rules: snapshot.weekly_rules.len(),
            conflicts,
        })
    }

    /// Import changes the client's database only. All rows and ID mappings commit together.
    pub async fn import_snapshot(
        &self,
        snapshot: &ServerSnapshot,
        strategy: &str,
        server_key: &str,
    ) -> AppResult<ImportReport> {
        if !matches!(strategy, "server" | "client" | "skip") || server_key.trim().is_empty() {
            return Err(AppError::validation("导入策略或服务器标识无效"));
        }
        validate_snapshot(snapshot)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = Utc::now().to_rfc3339();
        let mut ids = HashMap::new();
        let mut rule_ids = HashMap::new();
        let mut report = ImportReport {
            subscriptions: 0,
            schedules: 0,
            weekly_rules: 0,
            skipped: 0,
        };
        for remote in &snapshot.subscriptions {
            let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM streamers WHERE mid=?")
                .bind(&remote.mid)
                .fetch_optional(&mut *tx)
                .await?;
            let aliases =
                serde_json::to_string(&normalize_aliases(remote.aliases.clone(), &remote.name))?;
            let id = if let Some(id) = existing {
                if strategy == "server" {
                    sqlx::query("UPDATE streamers SET name=?,avatar_url=?,room_id=?,live_url=?,schedule_enabled=?,
                        live_notify_enabled=?,live_stats_enabled=?,aliases_json=?,updated_at=? WHERE id=?")
                        .bind(&remote.name).bind(&remote.avatar_url).bind(&remote.room_id).bind(&remote.live_url)
                        .bind(remote.schedule_enabled).bind(remote.live_notify_enabled).bind(remote.live_stats_enabled).bind(aliases).bind(&now).bind(id)
                        .execute(&mut *tx).await?;
                }
                id
            } else {
                sqlx::query("INSERT INTO streamers(mid,name,avatar_url,room_id,live_url,schedule_enabled,
                    live_notify_enabled,live_stats_enabled,aliases_json,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
                    .bind(&remote.mid).bind(&remote.name).bind(&remote.avatar_url).bind(&remote.room_id)
                    .bind(&remote.live_url).bind(remote.schedule_enabled).bind(remote.live_notify_enabled)
                    .bind(remote.live_stats_enabled).bind(aliases).bind(&now).bind(&now)
                    .execute(&mut *tx).await?.last_insert_rowid()
            };
            ids.insert(remote.id, id);
            report.subscriptions += 1;
            save_mapping(&mut tx, server_key, "subscription", remote.id, id, None).await?;
            for link in &remote.linked_accounts {
                let exists: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM linked_accounts WHERE streamer_id=? AND mid=?",
                )
                .bind(id)
                .bind(&link.mid)
                .fetch_optional(&mut *tx)
                .await?;
                if exists.is_some() && strategy != "server" {
                    continue;
                }
                sqlx::query("INSERT INTO linked_accounts(streamer_id,mid,name,avatar_url,created_at) VALUES(?,?,?,?,?)
                    ON CONFLICT(streamer_id,mid) DO UPDATE SET name=excluded.name,avatar_url=excluded.avatar_url")
                    .bind(id).bind(&link.mid).bind(&link.name).bind(&link.avatar_url).bind(&now)
                    .execute(&mut *tx).await?;
            }
        }
        for remote in &snapshot.schedules {
            let owner = ids[&remote.streamer_id];
            let existing:Option<i64>=sqlx::query_scalar("SELECT id FROM schedules WHERE streamer_id=?
                AND ((source_identity IS NOT NULL AND source_identity=?) OR (start_at=? AND title=?)) LIMIT 1")
                .bind(owner).bind(&remote.source_identity).bind(remote.start_at.to_rfc3339()).bind(&remote.title)
                .fetch_optional(&mut *tx).await?;
            let id = if let Some(id) = existing {
                id
            } else {
                sqlx::query("INSERT INTO schedules(streamer_id,title,start_at,source_kind,confidence,created_at,updated_at)
                    VALUES(?,?,?,?,?,?,?)")
                    .bind(owner).bind(&remote.title).bind(remote.start_at.to_rfc3339()).bind(&remote.source_kind)
                    .bind(remote.confidence).bind(&now).bind(&now).execute(&mut *tx).await?.last_insert_rowid()
            };
            if existing.is_none() || strategy == "server" {
                sqlx::query("UPDATE schedules SET title=?,start_at=?,end_at=?,room_url=?,source_kind=?,
                    source_post_id=?,source_image_url=?,source_author_mid=?,source_fingerprint=?,source_identity=?,
                    confidence=?,manual_locked=?,source_changed=?,cancelled=?,updated_at=? WHERE id=?")
                    .bind(&remote.title).bind(remote.start_at.to_rfc3339()).bind(remote.end_at.map(|date| date.to_rfc3339()))
                    .bind(&remote.room_url).bind(&remote.source_kind).bind(&remote.source_post_id)
                    .bind(&remote.source_image_url).bind(&remote.source_author_mid).bind(&remote.source_fingerprint)
                    .bind(&remote.source_identity).bind(remote.confidence).bind(remote.manual_locked)
                    .bind(remote.source_changed).bind(remote.cancelled).bind(remote.updated_at.to_rfc3339()).bind(id)
                    .execute(&mut *tx).await?;
                report.schedules += 1;
            } else {
                report.skipped += 1;
            }
            save_mapping(
                &mut tx,
                server_key,
                "schedule",
                remote.id,
                id,
                Some(remote.updated_at),
            )
            .await?;
        }
        for remote in &snapshot.weekly_rules {
            let owner = ids[&remote.streamer_id];
            let existing: Option<i64> =
                sqlx::query_scalar("SELECT id FROM weekly_schedule_rules WHERE rule_key = ?")
                    .bind(&remote.rule_key)
                    .fetch_optional(&mut *tx)
                    .await?;
            let id = if let Some(id) = existing {
                if strategy == "server" {
                    sqlx::query(
                        "UPDATE weekly_schedule_rules SET streamer_id=?, title=?, weekdays_mask=?,
                         start_time=?, end_time=?, timezone=?, room_url=?, effective_from=?,
                         effective_until=?, enabled=?, updated_at=? WHERE id=?",
                    )
                    .bind(owner)
                    .bind(&remote.title)
                    .bind(remote.weekdays_mask)
                    .bind(&remote.start_time)
                    .bind(&remote.end_time)
                    .bind(&remote.timezone)
                    .bind(&remote.room_url)
                    .bind(&remote.effective_from)
                    .bind(&remote.effective_until)
                    .bind(remote.enabled)
                    .bind(remote.updated_at.to_rfc3339())
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                }
                id
            } else {
                sqlx::query(
                    "INSERT INTO weekly_schedule_rules
                     (rule_key, streamer_id, title, weekdays_mask, start_time, end_time, timezone,
                      room_url, effective_from, effective_until, enabled, created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&remote.rule_key)
                .bind(owner)
                .bind(&remote.title)
                .bind(remote.weekdays_mask)
                .bind(&remote.start_time)
                .bind(&remote.end_time)
                .bind(&remote.timezone)
                .bind(&remote.room_url)
                .bind(&remote.effective_from)
                .bind(&remote.effective_until)
                .bind(remote.enabled)
                .bind(remote.created_at.to_rfc3339())
                .bind(remote.updated_at.to_rfc3339())
                .execute(&mut *tx)
                .await?
                .last_insert_rowid()
            };
            report.weekly_rules += 1;
            rule_ids.insert(remote.id, id);
            save_mapping(
                &mut tx,
                server_key,
                "weekly_rule",
                remote.id,
                id,
                Some(remote.updated_at),
            )
            .await?;
        }
        for remote in &snapshot.schedules {
            let (Some(remote_rule_id), Some(occurrence_date)) = (
                remote.weekly_rule_id,
                remote.weekly_occurrence_date.as_deref(),
            ) else {
                continue;
            };
            let Some(local_rule_id) = rule_ids.get(&remote_rule_id) else {
                continue;
            };
            sqlx::query(
                "UPDATE schedules SET weekly_rule_id = ?, weekly_occurrence_date = ?
                 WHERE streamer_id = ? AND source_identity = ?",
            )
            .bind(local_rule_id)
            .bind(occurrence_date)
            .bind(ids[&remote.streamer_id])
            .bind(&remote.source_identity)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(report)
    }

    pub async fn local_entity(
        &self,
        server_key: &str,
        entity: &str,
        remote_id: i64,
    ) -> AppResult<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT local_id FROM remote_entity_map WHERE server_key=? AND entity=? AND remote_id=?")
            .bind(server_key).bind(entity).bind(remote_id).fetch_optional(&self.pool).await?)
    }

    pub async fn remote_entity(
        &self,
        server_key: &str,
        entity: &str,
        local_id: i64,
    ) -> AppResult<Option<(i64, Option<DateTime<Utc>>)>> {
        let row = sqlx::query(
            "SELECT remote_id,remote_updated_at FROM remote_entity_map
            WHERE server_key=? AND entity=? AND local_id=?",
        )
        .bind(server_key)
        .bind(entity)
        .bind(local_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.get("remote_id"),
                parse_opt_ts(row.get::<Option<String>, _>("remote_updated_at").as_deref())?,
            ))
        })
        .transpose()
    }
}

async fn save_mapping(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    server: &str,
    entity: &str,
    remote: i64,
    local: i64,
    version: Option<DateTime<Utc>>,
) -> AppResult<()> {
    sqlx::query("INSERT INTO remote_entity_map(server_key,entity,remote_id,local_id,remote_updated_at) VALUES(?,?,?,?,?)
        ON CONFLICT(server_key,entity,remote_id) DO UPDATE SET local_id=excluded.local_id,remote_updated_at=excluded.remote_updated_at")
        .bind(server).bind(entity).bind(remote).bind(local).bind(version.map(|date| date.to_rfc3339()))
        .execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn seed(store: &SqliteStore, mid: &str, name: &str) -> Streamer {
        store
            .add_subscription(&AddSubscription {
                mid: mid.into(),
                name: Some(name.into()),
                avatar_url: None,
                room_id: Some("12345".into()),
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
            })
            .await
            .unwrap()
    }
    async fn schedule(store: &SqliteStore, owner: i64) -> Schedule {
        store
            .upsert_schedule(&NewSchedule {
                streamer_id: owner,
                participants: Vec::new(),
                title: "服务器日程".into(),
                start_at: Utc::now() + chrono::Duration::days(1),
                end_at: None,
                room_url: Some("https://live.bilibili.com/12345".into()),
                room_owner_mid: None,
                room_owner_name: None,
                source_kind: "text".into(),
                source_post_id: Some("1".into()),
                source_image_url: None,
                source_author_mid: Some("1".into()),
                source_fingerprint: Some("hash".into()),
                source_identity: Some("text:1".into()),
                source_published_at: None,
                confidence: 1.0,
            })
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn preview_is_read_only_and_import_strategies_preserve_the_selected_side() {
        for strategy in ["server", "client", "skip"] {
            let remote = SqliteStore::in_memory().await.unwrap();
            let owner = seed(&remote, "100", "远程主播").await;
            let event = schedule(&remote, owner.id).await;
            let remote_before =
                serde_json::to_value(remote.list_all_schedules().await.unwrap()).unwrap();
            let snapshot = ServerSnapshot {
                subscriptions: remote.list_streamers().await.unwrap(),
                schedules: remote.list_all_schedules().await.unwrap(),
                weekly_rules: Vec::new(),
                latest_event_seq: 0,
                exported_at: Utc::now(),
            };
            let local = SqliteStore::in_memory().await.unwrap();
            seed(&local, "999", "本地独有").await;
            local
                .import_snapshot(&snapshot, "client", "https://server")
                .await
                .unwrap();
            let imported = local.list_all_schedules().await.unwrap().remove(0);
            local
                .update_schedule(
                    imported.id,
                    &UpdateSchedule {
                        title: Some("本地改过的标题".into()),
                        start_at: Some(event.start_at + Duration::minutes(30)),
                        end_at: None,
                        room_url: None,
                    },
                )
                .await
                .unwrap();
            let before = serde_json::to_value(local.list_all_schedules().await.unwrap()).unwrap();
            let preview = local.preview_snapshot(&snapshot).await.unwrap();
            assert!(preview
                .conflicts
                .iter()
                .any(|item| item.entity == "schedule"));
            assert_eq!(
                before,
                serde_json::to_value(local.list_all_schedules().await.unwrap()).unwrap()
            );
            local
                .import_snapshot(&snapshot, strategy, "https://server")
                .await
                .unwrap();
            let events = local.list_all_schedules().await.unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].title,
                if strategy == "server" {
                    "服务器日程"
                } else {
                    "本地改过的标题"
                }
            );
            assert_eq!(local.list_streamers().await.unwrap().len(), 2);
            let map = local
                .remote_entity("https://server", "subscription", events[0].streamer_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(map.0, owner.id);
            assert_ne!(events[0].streamer_id, owner.id);
            assert_eq!(
                remote_before,
                serde_json::to_value(remote.list_all_schedules().await.unwrap()).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn invalid_snapshot_does_not_partially_import() {
        let store = SqliteStore::in_memory().await.unwrap();
        let remote = SqliteStore::in_memory().await.unwrap();
        let owner = seed(&remote, "100", "远程").await;
        let mut event = schedule(&remote, owner.id).await;
        event.streamer_id = 9999;
        let snapshot = ServerSnapshot {
            subscriptions: remote.list_streamers().await.unwrap(),
            schedules: vec![event],
            weekly_rules: Vec::new(),
            latest_event_seq: 0,
            exported_at: Utc::now(),
        };
        assert!(store
            .import_snapshot(&snapshot, "server", "https://server")
            .await
            .is_err());
        assert!(store.list_streamers().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn snapshot_imports_weekly_rules_with_rule_key_deduplication() {
        let remote = SqliteStore::in_memory().await.unwrap();
        let owner = seed(&remote, "100", "远程主播").await;
        remote
            .add_weekly_rule(&NewWeeklyScheduleRule {
                rule_key: Some("import-weekly-rule".to_owned()),
                streamer_id: owner.id,
                title: "导入周表".to_owned(),
                weekdays_mask: 0b0000_0101,
                start_time: "20:00".to_owned(),
                end_time: None,
                timezone: "Asia/Shanghai".to_owned(),
                room_url: None,
                effective_from: "2026-10-28".to_owned(),
                effective_until: None,
                enabled: true,
            })
            .await
            .unwrap();
        let snapshot = ServerSnapshot {
            subscriptions: remote.list_streamers().await.unwrap(),
            schedules: Vec::new(),
            weekly_rules: remote.list_weekly_rules(None).await.unwrap(),
            latest_event_seq: 0,
            exported_at: Utc::now(),
        };
        let local = SqliteStore::in_memory().await.unwrap();
        for _ in 0..2 {
            local
                .import_snapshot(&snapshot, "skip", "https://server")
                .await
                .unwrap();
        }
        let rules = local.list_weekly_rules(None).await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_key, "import-weekly-rule");
        assert_eq!(rules[0].weekdays_mask, 0b0000_0101);
    }
}
