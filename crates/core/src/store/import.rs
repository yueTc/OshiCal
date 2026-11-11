use super::*;
use crate::models::{ScheduleSource, WeeklyScheduleException};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::{HashMap, HashSet};

pub const SERVER_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSnapshot {
    pub schema_version: u32,
    pub subscriptions: Vec<Streamer>,
    pub schedules: Vec<Schedule>,
    #[serde(default)]
    pub weekly_rules: Vec<WeeklyScheduleRule>,
    #[serde(default)]
    pub weekly_exceptions: Vec<WeeklyScheduleException>,
    #[serde(default)]
    pub schedule_sources: Vec<ScheduleSource>,
    #[serde(default)]
    pub source_suppressions: Vec<SourceSuppression>,
    #[serde(default)]
    pub live_broadcasts: Vec<LiveBroadcast>,
    pub latest_event_seq: i64,
    pub exported_at: DateTime<Utc>,
}

/// `local` is always the destination database and `remote` is the incoming
/// snapshot. The HTTP migration endpoint deliberately swaps them in its response
/// because there the destination is the server and the incoming snapshot is the
/// client; callers must not reinterpret these fields based on entity names.
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

#[derive(Debug)]
pub enum MigrationApplyOutcome {
    Applied(ImportReport),
    Existing(String),
}

fn validate_snapshot(snapshot: &ServerSnapshot) -> AppResult<()> {
    if snapshot.schema_version != SERVER_SNAPSHOT_SCHEMA_VERSION {
        return Err(AppError::validation(format!(
            "不支持的快照版本 {}，当前仅支持 {}",
            snapshot.schema_version, SERVER_SNAPSHOT_SCHEMA_VERSION
        )));
    }
    if snapshot.subscriptions.len() > 1000
        || snapshot.schedules.len() > 20_000
        || snapshot.weekly_rules.len() > 5000
        || snapshot.weekly_exceptions.len() > 20_000
        || snapshot.schedule_sources.len() > 20_000
        || snapshot.source_suppressions.len() > 20_000
        || snapshot.live_broadcasts.len() > 20_000
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

    let mut rule_ids = HashSet::new();
    let mut rule_keys = HashSet::new();
    let mut rule_owners = HashMap::new();
    for rule in &snapshot.weekly_rules {
        if !ids.contains(&rule.streamer_id)
            || rule.rule_key.trim().is_empty()
            || !rule_ids.insert(rule.id)
            || !rule_keys.insert(&rule.rule_key)
        {
            return Err(AppError::validation(
                "快照固定周表归属缺失、ID/规则键重复或规则键为空",
            ));
        }
        rule_owners.insert(rule.id, rule.streamer_id);
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

    let mut schedule_ids = HashSet::new();
    let mut schedule_owners = HashMap::new();
    for schedule in &snapshot.schedules {
        if !ids.contains(&schedule.streamer_id) || !schedule_ids.insert(schedule.id) {
            return Err(AppError::validation("快照日程归属缺失或 ID 重复"));
        }
        validate_schedule_input(&new_schedule(schedule, schedule.streamer_id))?;
        if schedule.weekly_rule_id.is_some() != schedule.weekly_occurrence_date.is_some() {
            return Err(AppError::validation(
                "快照周表场次的固定周表与场次日期必须同时存在",
            ));
        }
        if let Some(rule_id) = schedule.weekly_rule_id {
            let owner = rule_owners
                .get(&rule_id)
                .ok_or_else(|| AppError::validation("快照日程引用了不存在的固定周表"))?;
            if *owner != schedule.streamer_id {
                return Err(AppError::validation("快照日程与固定周表归属不一致"));
            }
        }
        if let Some(date) = schedule.weekly_occurrence_date.as_deref() {
            if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
                return Err(AppError::validation("快照周表场次日期格式错误"));
            }
        }
        schedule_owners.insert(schedule.id, schedule.streamer_id);
    }

    let mut broadcast_ids = HashSet::new();
    let mut broadcast_schedules = HashSet::new();
    for broadcast in &snapshot.live_broadcasts {
        if !ids.contains(&broadcast.streamer_id)
            || uuid::Uuid::parse_str(&broadcast.id).is_err()
            || !broadcast_ids.insert(&broadcast.id)
            || broadcast.segment_count < 1
            || broadcast.room_id.is_empty()
            || !broadcast.room_id.chars().all(|ch| ch.is_ascii_digit())
        {
            return Err(AppError::validation("直播场次身份、归属或推流段数无效"));
        }
        let started = parse_ts(&broadcast.started_at)?;
        let seen = parse_ts(&broadcast.last_seen_at)?;
        if seen < started
            || parse_opt_ts(broadcast.offline_since.as_deref())?.is_some_and(|time| time < seen)
            || parse_opt_ts(broadcast.last_segment_started_at.as_deref())?
                .is_some_and(|time| time > seen)
        {
            return Err(AppError::validation("直播场次观测时间无效"));
        }
        if let Some(id) = broadcast.schedule_id {
            if schedule_owners.get(&id) != Some(&broadcast.streamer_id)
                || !broadcast_schedules.insert(id)
                || !broadcast.schedule_decided
            {
                return Err(AppError::validation("直播卡片归属缺失或重复绑定"));
            }
        }
    }
    let mut exception_ids = HashSet::new();
    let mut exception_keys = HashSet::new();
    for exception in &snapshot.weekly_exceptions {
        if !exception_ids.insert(exception.id)
            || !exception_keys.insert((exception.rule_id, exception.occurrence_date.clone()))
        {
            return Err(AppError::validation("快照周表例外 ID 或规则场次键重复"));
        }
        let rule_owner = rule_owners
            .get(&exception.rule_id)
            .ok_or_else(|| AppError::validation("快照周表例外引用了不存在的固定周表"))?;
        if !matches!(exception.exception_kind.as_str(), "override" | "skip") {
            return Err(AppError::validation("快照周表例外类型无效"));
        }
        if chrono::NaiveDate::parse_from_str(&exception.occurrence_date, "%Y-%m-%d").is_err() {
            return Err(AppError::validation("快照周表例外日期格式错误"));
        }
        if let Some(schedule_id) = exception.schedule_id {
            let schedule_owner = schedule_owners
                .get(&schedule_id)
                .ok_or_else(|| AppError::validation("快照周表例外引用了不存在的日程"))?;
            if *schedule_owner != *rule_owner {
                return Err(AppError::validation("快照周表例外与日程归属不一致"));
            }
        }
    }

    let mut source_ids = HashSet::new();
    let mut source_keys = HashSet::new();
    for source in &snapshot.schedule_sources {
        if !source_ids.insert(source.id)
            || !source_keys.insert((source.schedule_id, source.source_identity.clone()))
        {
            return Err(AppError::validation("快照日程来源 ID 或来源键重复"));
        }
        if !schedule_ids.contains(&source.schedule_id) {
            return Err(AppError::validation("快照日程来源引用了不存在的日程"));
        }
        if source.source_kind.trim().is_empty()
            || source.source_identity.trim().is_empty()
            || !source.confidence.is_finite()
            || !(0.0..=1.0).contains(&source.confidence)
        {
            return Err(AppError::validation("快照日程来源字段无效"));
        }
    }

    let mut suppression_ids = HashSet::new();
    let mut suppression_keys = HashSet::new();
    for suppression in &snapshot.source_suppressions {
        if !suppression_ids.insert(suppression.id)
            || !suppression_keys.insert((
                suppression.streamer_id,
                suppression.source_post_id.clone(),
                suppression.source_fingerprint.clone(),
            ))
        {
            return Err(AppError::validation("快照来源抑制 ID 或抑制键重复"));
        }
        if !ids.contains(&suppression.streamer_id) {
            return Err(AppError::validation("快照来源抑制引用了不存在的订阅"));
        }
        if suppression.source_post_id.trim().is_empty()
            || suppression.source_fingerprint.trim().is_empty()
        {
            return Err(AppError::validation("快照来源抑制字段无效"));
        }
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

fn weekly_rule_values(rule: &WeeklyScheduleRule, streamer_mid: &str) -> Value {
    json!({
        "streamerMid": streamer_mid,
        "ruleKey": rule.rule_key,
        "title": rule.title,
        "weekdaysMask": rule.weekdays_mask,
        "startTime": rule.start_time,
        "endTime": rule.end_time,
        "timezone": rule.timezone,
        "roomUrl": rule.room_url,
        "effectiveFrom": rule.effective_from,
        "effectiveUntil": rule.effective_until,
        "enabled": rule.enabled,
        "revision": rule.revision,
    })
}

fn schedule_reference(schedule: Option<&Schedule>) -> Value {
    schedule.map_or(Value::Null, |schedule| {
        json!({
            "sourceIdentity": schedule.source_identity,
            "title": schedule.title,
            "startAt": schedule.start_at,
        })
    })
}

fn weekly_exception_values(
    exception: &WeeklyScheduleException,
    schedule: Option<&Schedule>,
) -> Value {
    json!({
        "occurrenceDate": exception.occurrence_date,
        "exceptionKind": exception.exception_kind,
        "schedule": schedule_reference(schedule),
        "revision": exception.revision,
    })
}

fn schedule_source_values(source: &ScheduleSource, schedule: Option<&Schedule>) -> Value {
    json!({
        "schedule": schedule_reference(schedule),
        "sourceKind": source.source_kind,
        "sourcePostId": source.source_post_id,
        "sourceImageUrl": source.source_image_url,
        "sourceFingerprint": source.source_fingerprint,
        "sourceIdentity": source.source_identity,
        "confidence": source.confidence,
        "revision": source.revision,
    })
}

fn source_suppression_values(suppression: &SourceSuppression, streamer_mid: &str) -> Value {
    json!({
        "streamerMid": streamer_mid,
        "sourcePostId": suppression.source_post_id,
        "sourceFingerprint": suppression.source_fingerprint,
        "revision": suppression.revision,
        "createdAt": suppression.created_at,
    })
}

fn mapped_local_id(ids: &HashMap<i64, i64>, remote_id: i64) -> AppResult<i64> {
    ids.get(&remote_id)
        .copied()
        .ok_or_else(|| AppError::validation(format!("快照引用的实体 {remote_id} 未导入")))
}

fn advanced_revision(local_revision: i64, remote_revision: i64) -> i64 {
    local_revision
        .max(0)
        .saturating_add(1)
        .max(remote_revision.max(1))
}

fn non_regressing_timestamp(local: DateTime<Utc>, remote: DateTime<Utc>) -> DateTime<Utc> {
    local.max(remote)
}

impl SqliteStore {
    pub async fn preview_snapshot(&self, snapshot: &ServerSnapshot) -> AppResult<ImportPreview> {
        validate_snapshot(snapshot)?;
        let streamers = self.list_streamers().await?;
        let schedules = self.list_all_schedules().await?;
        let weekly_rules = self.list_weekly_rules(None).await?;
        let source_suppressions = self.list_source_suppressions().await?;

        let local_streamer_mid_by_id: HashMap<i64, &str> = streamers
            .iter()
            .map(|streamer| (streamer.id, streamer.mid.as_str()))
            .collect();
        let remote_streamer_mid_by_id: HashMap<i64, &str> = snapshot
            .subscriptions
            .iter()
            .map(|streamer| (streamer.id, streamer.mid.as_str()))
            .collect();
        let local_rule_by_key: HashMap<&str, &WeeklyScheduleRule> = weekly_rules
            .iter()
            .map(|rule| (rule.rule_key.as_str(), rule))
            .collect();
        let local_rule_key_by_id: HashMap<i64, &str> = weekly_rules
            .iter()
            .map(|rule| (rule.id, rule.rule_key.as_str()))
            .collect();
        let remote_rule_key_by_id: HashMap<i64, &str> = snapshot
            .weekly_rules
            .iter()
            .map(|rule| (rule.id, rule.rule_key.as_str()))
            .collect();

        let local_exception_rows = sqlx::query_as::<
            _,
            (i64, i64, String, String, Option<i64>, i64, String),
        >(
            "SELECT id, rule_id, occurrence_date, exception_kind, schedule_id, revision, created_at
             FROM weekly_schedule_exceptions ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut local_exceptions = HashMap::new();
        for (id, rule_id, occurrence_date, exception_kind, schedule_id, revision, created_at) in
            local_exception_rows
        {
            let Some(rule_key) = local_rule_key_by_id.get(&rule_id) else {
                continue;
            };
            local_exceptions.insert(
                ((*rule_key).to_owned(), occurrence_date.clone()),
                WeeklyScheduleException {
                    id,
                    rule_id,
                    occurrence_date,
                    exception_kind,
                    schedule_id,
                    revision,
                    created_at: parse_ts(&created_at)?,
                },
            );
        }

        let local_source_rows = sqlx::query_as::<
            _,
            (
                i64,
                i64,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
                f64,
                i64,
                String,
            ),
        >(
            "SELECT id, schedule_id, source_kind, source_post_id, source_image_url,
                    source_fingerprint, source_identity, confidence, revision, created_at
             FROM schedule_sources ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut local_sources = HashMap::new();
        for (
            id,
            schedule_id,
            source_kind,
            source_post_id,
            source_image_url,
            source_fingerprint,
            source_identity,
            confidence,
            revision,
            created_at,
        ) in local_source_rows
        {
            local_sources.insert(
                (schedule_id, source_identity.clone()),
                ScheduleSource {
                    id,
                    schedule_id,
                    source_kind,
                    source_post_id,
                    source_image_url,
                    source_fingerprint,
                    source_identity,
                    confidence,
                    revision,
                    created_at: parse_ts(&created_at)?,
                },
            );
        }

        let local_suppression_by_key: HashMap<(String, String, String), &SourceSuppression> =
            source_suppressions
                .iter()
                .filter_map(|suppression| {
                    local_streamer_mid_by_id
                        .get(&suppression.streamer_id)
                        .map(|streamer_mid| {
                            (
                                (
                                    (*streamer_mid).to_owned(),
                                    suppression.source_post_id.clone(),
                                    suppression.source_fingerprint.clone(),
                                ),
                                suppression,
                            )
                        })
                })
                .collect();

        let mut conflicts = Vec::new();
        let mut remote_to_local_schedule = HashMap::new();
        for remote in &snapshot.subscriptions {
            let Some(local) = streamers.iter().find(|local| local.mid == remote.mid) else {
                continue;
            };
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
                let Some(local_schedule) = schedules.iter().find(|item| {
                    item.streamer_id == local.id && same_schedule(item, remote_schedule)
                }) else {
                    continue;
                };
                remote_to_local_schedule.insert(remote_schedule.id, local_schedule.id);
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

        for remote_rule in &snapshot.weekly_rules {
            let Some(local_rule) = local_rule_by_key.get(remote_rule.rule_key.as_str()) else {
                continue;
            };
            let local_streamer_mid = local_streamer_mid_by_id
                .get(&local_rule.streamer_id)
                .copied()
                .unwrap_or_default();
            let remote_streamer_mid = remote_streamer_mid_by_id
                .get(&remote_rule.streamer_id)
                .copied()
                .unwrap_or_default();
            let local = weekly_rule_values(local_rule, local_streamer_mid);
            let remote = weekly_rule_values(remote_rule, remote_streamer_mid);
            if local != remote {
                conflicts.push(ImportConflict {
                    entity: "weekly_rule".into(),
                    label: remote_rule.title.clone(),
                    local_id: local_rule.id,
                    remote_id: remote_rule.id,
                    local,
                    remote,
                });
            }
        }

        for remote_exception in &snapshot.weekly_exceptions {
            let Some(remote_rule_key) = remote_rule_key_by_id.get(&remote_exception.rule_id) else {
                continue;
            };
            let Some(local_exception) = local_exceptions.get(&(
                (*remote_rule_key).to_owned(),
                remote_exception.occurrence_date.clone(),
            )) else {
                continue;
            };
            let local_schedule = local_exception
                .schedule_id
                .and_then(|id| schedules.iter().find(|schedule| schedule.id == id));
            let remote_schedule = remote_exception
                .schedule_id
                .and_then(|id| snapshot.schedules.iter().find(|schedule| schedule.id == id));
            let local = weekly_exception_values(local_exception, local_schedule);
            let remote = weekly_exception_values(remote_exception, remote_schedule);
            if local != remote {
                conflicts.push(ImportConflict {
                    entity: "weekly_exception".into(),
                    label: format!("{} {}", remote_rule_key, remote_exception.occurrence_date),
                    local_id: local_exception.id,
                    remote_id: remote_exception.id,
                    local,
                    remote,
                });
            }
        }

        for remote_source in &snapshot.schedule_sources {
            let Some(local_schedule_id) = remote_to_local_schedule
                .get(&remote_source.schedule_id)
                .copied()
            else {
                continue;
            };
            let Some(local_source) =
                local_sources.get(&(local_schedule_id, remote_source.source_identity.clone()))
            else {
                continue;
            };
            let local_schedule = schedules
                .iter()
                .find(|schedule| schedule.id == local_source.schedule_id);
            let remote_schedule = snapshot
                .schedules
                .iter()
                .find(|schedule| schedule.id == remote_source.schedule_id);
            let local = schedule_source_values(local_source, local_schedule);
            let remote = schedule_source_values(remote_source, remote_schedule);
            if local != remote {
                conflicts.push(ImportConflict {
                    entity: "schedule_source".into(),
                    label: remote_source.source_identity.clone(),
                    local_id: local_source.id,
                    remote_id: remote_source.id,
                    local,
                    remote,
                });
            }
        }

        for remote_suppression in &snapshot.source_suppressions {
            let Some(remote_streamer_mid) = remote_streamer_mid_by_id
                .get(&remote_suppression.streamer_id)
                .copied()
            else {
                continue;
            };
            let Some(local_suppression) = local_suppression_by_key.get(&(
                remote_streamer_mid.to_owned(),
                remote_suppression.source_post_id.clone(),
                remote_suppression.source_fingerprint.clone(),
            )) else {
                continue;
            };
            let local = source_suppression_values(local_suppression, remote_streamer_mid);
            let remote = source_suppression_values(remote_suppression, remote_streamer_mid);
            if local != remote {
                conflicts.push(ImportConflict {
                    entity: "source_suppression".into(),
                    label: format!(
                        "{} {}",
                        remote_suppression.source_post_id, remote_suppression.source_fingerprint
                    ),
                    local_id: local_suppression.id,
                    remote_id: remote_suppression.id,
                    local,
                    remote,
                });
            }
        }

        let broadcasts = self.list_live_broadcasts().await?;
        for remote in &snapshot.live_broadcasts {
            if let Some(local) = broadcasts.iter().find(|local| local.id == remote.id) {
                let mut local_value = serde_json::to_value(local)?;
                let mut remote_value = serde_json::to_value(remote)?;
                // Database-local references are deliberately compared through their content below.
                local_value.as_object_mut().unwrap().remove("streamerId");
                remote_value.as_object_mut().unwrap().remove("streamerId");
                local_value["scheduleId"] = schedule_reference(
                    local
                        .schedule_id
                        .and_then(|id| schedules.iter().find(|s| s.id == id)),
                );
                remote_value["scheduleId"] = schedule_reference(
                    remote
                        .schedule_id
                        .and_then(|id| snapshot.schedules.iter().find(|s| s.id == id)),
                );
                if local_value != remote_value {
                    conflicts.push(ImportConflict {
                        entity: "live_broadcast".into(),
                        label: format!("直播场次 {}", remote.id),
                        local_id: local.streamer_id,
                        remote_id: remote.streamer_id,
                        local: local_value,
                        remote: remote_value,
                    });
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
        validate_import_request(snapshot, strategy, server_key)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let report = import_snapshot_on(&mut tx, snapshot, strategy, server_key).await?;
        tx.commit().await?;
        Ok(report)
    }

    /// Applies a server migration transactionally: imported data, sync reset,
    /// notification event and idempotency receipt either all commit or all roll back.
    #[allow(clippy::too_many_arguments)]
    pub async fn import_snapshot_migration(
        &self,
        snapshot: &ServerSnapshot,
        strategy: &str,
        server_key: &str,
        migration_id: &str,
        receipt_key: &str,
        public_strategy: &str,
        snapshot_digest: &str,
        expected_server_digest: Option<&str>,
        event_type: &str,
        event_payload: &Value,
    ) -> AppResult<MigrationApplyOutcome> {
        validate_import_request(snapshot, strategy, server_key)?;
        if migration_id.trim().is_empty() || receipt_key.trim().is_empty() {
            return Err(AppError::validation("迁移标识不能为空"));
        }

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) =
            sqlx::query_scalar::<_, String>("SELECT value FROM client_sync_state WHERE key = ?")
                .bind(receipt_key)
                .fetch_optional(&mut *tx)
                .await?
        {
            tx.commit().await?;
            return Ok(MigrationApplyOutcome::Existing(existing));
        }
        if let Some(expected) = expected_server_digest {
            let current = migration_state_digest_on(&mut tx).await?;
            if expected != current {
                return Err(AppError::External {
                    code: "MIGRATION_STALE_PREVIEW".to_owned(),
                    message: "服务器数据已在预览后发生变化，请重新预览迁移冲突".to_owned(),
                });
            }
        }

        let report = import_snapshot_on(&mut tx, snapshot, strategy, server_key).await?;
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let reset_payload = json!({"id": migration_id, "reason": "data_imported"});
        let change = sqlx::query(
            "INSERT INTO sync_changes
                (entity_type, entity_id, operation, entity_revision, payload_json,
                 actor_type, actor_id, mutation_id, created_at)
             VALUES ('sync_reset', ?, 'upsert', 1, ?, 'system', NULL, ?, ?)",
        )
        .bind(migration_id)
        .bind(serde_json::to_string(&reset_payload)?)
        .bind(migration_id)
        .bind(&now_text)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO sync_feed_meta (key, value, updated_at) VALUES ('latest_seq', ?, ?)
             ON CONFLICT(key) DO UPDATE SET
                value = MAX(value, excluded.value),
                updated_at = excluded.updated_at",
        )
        .bind(change.last_insert_rowid())
        .bind(&now_text)
        .execute(&mut *tx)
        .await?;

        let mut payload = event_payload.clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert("subscriptions".to_owned(), json!(report.subscriptions));
            object.insert("schedules".to_owned(), json!(report.schedules));
            object.insert("weeklyRules".to_owned(), json!(report.weekly_rules));
            object.insert("skipped".to_owned(), json!(report.skipped));
        }
        sqlx::query("INSERT INTO event_journal (type, payload_json, created_at) VALUES (?, ?, ?)")
            .bind(event_type)
            .bind(serde_json::to_string(&payload)?)
            .bind(&now_text)
            .execute(&mut *tx)
            .await?;

        let receipt = json!({
            "strategy": public_strategy,
            "snapshotDigest": snapshot_digest,
            "report": report.clone(),
            "completedAt": now,
        });
        sqlx::query("INSERT INTO client_sync_state (key, value, updated_at) VALUES (?, ?, ?)")
            .bind(receipt_key)
            .bind(serde_json::to_string(&receipt)?)
            .bind(&now_text)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(MigrationApplyOutcome::Applied(report))
    }

    pub async fn migration_state_digest(&self) -> AppResult<String> {
        let mut tx = self.pool.begin().await?;
        let digest = migration_state_digest_on(&mut tx).await?;
        tx.commit().await?;
        Ok(digest)
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

fn digest_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

async fn hash_migration_table(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hasher: &mut Sha256,
    table: &str,
    columns: &[&str],
    order_by: &str,
) -> AppResult<()> {
    digest_part(hasher, table.as_bytes());
    for column in columns {
        digest_part(hasher, column.as_bytes());
    }
    let projection = columns
        .iter()
        .map(|column| format!("COALESCE(CAST(quote({column}) AS TEXT), '')"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {projection} FROM {table} ORDER BY {order_by}");
    let rows = sqlx::query(&sql).fetch_all(&mut **tx).await?;
    digest_part(hasher, &(rows.len() as u64).to_le_bytes());
    for row in rows {
        for index in 0..columns.len() {
            let value: String = row.try_get(index)?;
            digest_part(hasher, value.as_bytes());
        }
    }
    Ok(())
}

async fn migration_state_digest_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> AppResult<String> {
    let latest_sync: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM sync_changes")
        .fetch_one(&mut **tx)
        .await?;
    let latest_event: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM event_journal")
        .fetch_one(&mut **tx)
        .await?;
    let feed_id: String = sqlx::query_scalar(
        "SELECT COALESCE(
            (SELECT feed_id FROM sync_feed_identity WHERE singleton = 1),
            ''
         )",
    )
    .fetch_one(&mut **tx)
    .await?;

    let mut hasher = Sha256::new();
    hasher.update(b"oshical-migration-state-v2\x00");
    for value in [latest_sync, latest_event] {
        digest_part(&mut hasher, &value.to_le_bytes());
    }
    digest_part(&mut hasher, feed_id.as_bytes());

    let tables: [(&str, &[&str], &str); 12] = [
        (
            "streamers",
            &[
                "id",
                "mid",
                "name",
                "avatar_url",
                "room_id",
                "live_url",
                "schedule_enabled",
                "live_notify_enabled",
                "live_stats_enabled",
                "fans",
                "aliases_json",
                "last_error",
                "last_refresh_at",
                "live_is_live",
                "live_title",
                "live_cover",
                "live_started_at",
                "live_checked_at",
                "live_schedule_id",
                "live_schedule_known",
                "sort_order",
                "created_at",
                "updated_at",
                "revision",
                "live_revision",
            ],
            "id",
        ),
        (
            "linked_accounts",
            &[
                "id",
                "streamer_id",
                "mid",
                "name",
                "avatar_url",
                "created_at",
                "revision",
            ],
            "id",
        ),
        (
            "schedules",
            &[
                "id",
                "streamer_id",
                "participants_json",
                "title",
                "start_at",
                "end_at",
                "room_url",
                "room_owner_mid",
                "room_owner_name",
                "source_kind",
                "source_post_id",
                "source_image_url",
                "source_author_mid",
                "source_fingerprint",
                "source_identity",
                "source_published_at",
                "weekly_rule_id",
                "weekly_occurrence_date",
                "confidence",
                "manual_locked",
                "source_changed",
                "cancelled",
                "created_at",
                "updated_at",
                "revision",
            ],
            "id",
        ),
        (
            "weekly_schedule_rules",
            &[
                "id",
                "rule_key",
                "streamer_id",
                "title",
                "weekdays_mask",
                "start_time",
                "end_time",
                "timezone",
                "room_url",
                "effective_from",
                "effective_until",
                "enabled",
                "created_at",
                "updated_at",
                "revision",
            ],
            "id",
        ),
        (
            "weekly_schedule_exceptions",
            &[
                "id",
                "rule_id",
                "occurrence_date",
                "exception_kind",
                "schedule_id",
                "created_at",
                "revision",
            ],
            "id",
        ),
        (
            "schedule_sources",
            &[
                "id",
                "schedule_id",
                "source_kind",
                "source_post_id",
                "source_image_url",
                "source_fingerprint",
                "source_identity",
                "confidence",
                "created_at",
                "revision",
            ],
            "id",
        ),
        (
            "source_suppressions",
            &[
                "id",
                "streamer_id",
                "source_post_id",
                "source_fingerprint",
                "created_at",
                "revision",
            ],
            "id",
        ),
        (
            "settings",
            &["key", "value", "updated_at", "revision"],
            "key",
        ),
        (
            "remote_entity_map",
            &[
                "server_key",
                "entity",
                "remote_id",
                "local_id",
                "remote_updated_at",
            ],
            "server_key, entity, remote_id",
        ),
        (
            "sync_feed_identity",
            &["singleton", "feed_id", "created_at"],
            "singleton",
        ),
        ("sync_feed_meta", &["key", "value", "updated_at"], "key"),
        (
            "live_broadcasts",
            &[
                "id",
                "streamer_id",
                "room_id",
                "started_at",
                "last_segment_started_at",
                "last_seen_at",
                "offline_since",
                "schedule_id",
                "schedule_decided",
                "segment_count",
            ],
            "id",
        ),
    ];
    for (table, columns, order_by) in tables {
        hash_migration_table(tx, &mut hasher, table, columns, order_by).await?;
    }

    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    Ok(format!("sha256:{encoded}"))
}

fn validate_import_request(
    snapshot: &ServerSnapshot,
    strategy: &str,
    server_key: &str,
) -> AppResult<()> {
    if !matches!(strategy, "server" | "client" | "skip") || server_key.trim().is_empty() {
        return Err(AppError::validation("导入策略或服务器标识无效"));
    }
    validate_snapshot(snapshot)
}

async fn import_snapshot_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    snapshot: &ServerSnapshot,
    strategy: &str,
    server_key: &str,
) -> AppResult<ImportReport> {
    let now = Utc::now().to_rfc3339();
    let mut ids = HashMap::new();
    let mut schedule_ids = HashMap::new();
    let mut applied_schedules = HashSet::new();
    let mut rule_ids = HashMap::new();
    let mut report = ImportReport {
        subscriptions: 0,
        schedules: 0,
        weekly_rules: 0,
        skipped: 0,
    };
    for remote in &snapshot.subscriptions {
        let existing: Option<(i64, String)> =
            sqlx::query_as("SELECT id, updated_at FROM streamers WHERE mid=?")
                .bind(&remote.mid)
                .fetch_optional(&mut **tx)
                .await?;
        let aliases =
            serde_json::to_string(&normalize_aliases(remote.aliases.clone(), &remote.name))?;
        let id = if let Some((id, local_updated_at)) = existing {
            if strategy == "server" {
                let local_updated_at = parse_ts(&local_updated_at)?;
                let updated_at = local_updated_at.max(Utc::now()).to_rfc3339();
                sqlx::query(
                    "UPDATE streamers SET name=?, avatar_url=?, room_id=?, live_url=?,
                            schedule_enabled=?, live_notify_enabled=?, live_stats_enabled=?, fans=?,
                            aliases_json=?, updated_at=?, revision=MAX(revision + 1, ?)
                         WHERE id=?",
                )
                .bind(&remote.name)
                .bind(&remote.avatar_url)
                .bind(&remote.room_id)
                .bind(&remote.live_url)
                .bind(remote.schedule_enabled)
                .bind(remote.live_notify_enabled)
                .bind(remote.live_stats_enabled)
                .bind(remote.fans.and_then(|value| i64::try_from(value).ok()))
                .bind(aliases)
                .bind(updated_at)
                .bind(remote.revision.max(1))
                .bind(id)
                .execute(&mut **tx)
                .await?;
            }
            id
        } else {
            sqlx::query(
                "INSERT INTO streamers
                        (mid, name, avatar_url, room_id, live_url, schedule_enabled,
                         live_notify_enabled, live_stats_enabled, fans, aliases_json,
                         created_at, updated_at, revision)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&remote.mid)
            .bind(&remote.name)
            .bind(&remote.avatar_url)
            .bind(&remote.room_id)
            .bind(&remote.live_url)
            .bind(remote.schedule_enabled)
            .bind(remote.live_notify_enabled)
            .bind(remote.live_stats_enabled)
            .bind(remote.fans.and_then(|value| i64::try_from(value).ok()))
            .bind(aliases)
            .bind(remote.created_at.to_rfc3339())
            .bind(&now)
            .bind(remote.revision.max(1))
            .execute(&mut **tx)
            .await?
            .last_insert_rowid()
        };
        ids.insert(remote.id, id);
        report.subscriptions += 1;
        save_mapping(tx, server_key, "subscription", remote.id, id, None).await?;
        for link in &remote.linked_accounts {
            let existing: Option<(i64, i64)> = sqlx::query_as(
                "SELECT id, revision FROM linked_accounts WHERE streamer_id=? AND mid=?",
            )
            .bind(id)
            .bind(&link.mid)
            .fetch_optional(&mut **tx)
            .await?;
            if let Some((link_id, local_revision)) = existing {
                if strategy != "server" {
                    continue;
                }
                sqlx::query(
                    "UPDATE linked_accounts
                         SET name=?, avatar_url=?, revision=?
                         WHERE id=?",
                )
                .bind(&link.name)
                .bind(&link.avatar_url)
                .bind(advanced_revision(local_revision, link.revision))
                .bind(link_id)
                .execute(&mut **tx)
                .await?;
            } else {
                sqlx::query(
                    "INSERT INTO linked_accounts
                            (streamer_id, mid, name, avatar_url, created_at, revision)
                         VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(id)
                .bind(&link.mid)
                .bind(&link.name)
                .bind(&link.avatar_url)
                .bind(link.created_at.to_rfc3339())
                .bind(link.revision.max(1))
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    for remote in &snapshot.schedules {
        let owner = ids[&remote.streamer_id];
        let existing: Option<(i64, i64, String)> = sqlx::query_as(
            "SELECT id, revision, updated_at FROM schedules
                 WHERE streamer_id=?
                   AND ((source_identity IS NOT NULL AND source_identity=?)
                        OR (start_at=? AND title=?))
                 LIMIT 1",
        )
        .bind(owner)
        .bind(&remote.source_identity)
        .bind(remote.start_at.to_rfc3339())
        .bind(&remote.title)
        .fetch_optional(&mut **tx)
        .await?;
        let inserted = existing.is_none();
        let participants_json = serde_json::to_string(&remote.participants)?;
        let id = if let Some((id, local_revision, local_updated_at)) = existing {
            if strategy == "server" {
                let local_updated_at = parse_ts(&local_updated_at)?;
                let updated_at =
                    non_regressing_timestamp(local_updated_at, remote.updated_at).to_rfc3339();
                sqlx::query(
                    "UPDATE schedules SET participants_json=?, title=?, start_at=?, end_at=?,
                            room_url=?, room_owner_mid=?, room_owner_name=?, source_kind=?,
                            source_post_id=?, source_image_url=?, source_author_mid=?,
                            source_fingerprint=?, source_identity=?, source_published_at=?,
                            confidence=?, manual_locked=?, source_changed=?, cancelled=?,
                            weekly_rule_id=NULL, weekly_occurrence_date=NULL,
                            revision=?, updated_at=? WHERE id=?",
                )
                .bind(&participants_json)
                .bind(&remote.title)
                .bind(remote.start_at.to_rfc3339())
                .bind(remote.end_at.map(|date| date.to_rfc3339()))
                .bind(&remote.room_url)
                .bind(&remote.room_owner_mid)
                .bind(&remote.room_owner_name)
                .bind(&remote.source_kind)
                .bind(&remote.source_post_id)
                .bind(&remote.source_image_url)
                .bind(&remote.source_author_mid)
                .bind(&remote.source_fingerprint)
                .bind(&remote.source_identity)
                .bind(remote.source_published_at.map(|date| date.to_rfc3339()))
                .bind(remote.confidence)
                .bind(remote.manual_locked)
                .bind(remote.source_changed)
                .bind(remote.cancelled)
                .bind(advanced_revision(local_revision, remote.revision))
                .bind(updated_at)
                .bind(id)
                .execute(&mut **tx)
                .await?;
                report.schedules += 1;
            } else {
                report.skipped += 1;
            }
            id
        } else {
            sqlx::query(
                "INSERT INTO schedules
                        (streamer_id, participants_json, title, start_at, end_at, room_url,
                         room_owner_mid, room_owner_name, source_kind, source_post_id,
                         source_image_url, source_author_mid, source_fingerprint, source_identity,
                         source_published_at, confidence, manual_locked, source_changed, cancelled,
                         created_at, updated_at, revision)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(owner)
            .bind(&participants_json)
            .bind(&remote.title)
            .bind(remote.start_at.to_rfc3339())
            .bind(remote.end_at.map(|date| date.to_rfc3339()))
            .bind(&remote.room_url)
            .bind(&remote.room_owner_mid)
            .bind(&remote.room_owner_name)
            .bind(&remote.source_kind)
            .bind(&remote.source_post_id)
            .bind(&remote.source_image_url)
            .bind(&remote.source_author_mid)
            .bind(&remote.source_fingerprint)
            .bind(&remote.source_identity)
            .bind(remote.source_published_at.map(|date| date.to_rfc3339()))
            .bind(remote.confidence)
            .bind(remote.manual_locked)
            .bind(remote.source_changed)
            .bind(remote.cancelled)
            .bind(remote.created_at.to_rfc3339())
            .bind(remote.updated_at.to_rfc3339())
            .bind(remote.revision.max(1))
            .execute(&mut **tx)
            .await?
            .last_insert_rowid()
        };
        if inserted {
            report.schedules += 1;
        }
        if inserted || strategy == "server" {
            applied_schedules.insert(remote.id);
        }
        save_mapping(
            tx,
            server_key,
            "schedule",
            remote.id,
            id,
            Some(remote.updated_at),
        )
        .await?;
        schedule_ids.insert(remote.id, id);
    }
    for remote in &snapshot.weekly_rules {
        let owner = ids[&remote.streamer_id];
        let existing: Option<(i64, i64, String)> = sqlx::query_as(
            "SELECT id, revision, updated_at FROM weekly_schedule_rules WHERE rule_key = ?",
        )
        .bind(&remote.rule_key)
        .fetch_optional(&mut **tx)
        .await?;
        let id = if let Some((id, local_revision, local_updated_at)) = existing {
            if strategy == "server" {
                let local_updated_at = parse_ts(&local_updated_at)?;
                let updated_at =
                    non_regressing_timestamp(local_updated_at, remote.updated_at).to_rfc3339();
                sqlx::query(
                    "UPDATE weekly_schedule_rules SET streamer_id=?, title=?, weekdays_mask=?,
                            start_time=?, end_time=?, timezone=?, room_url=?, effective_from=?,
                            effective_until=?, enabled=?, updated_at=?, revision=? WHERE id=?",
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
                .bind(updated_at)
                .bind(advanced_revision(local_revision, remote.revision))
                .bind(id)
                .execute(&mut **tx)
                .await?;
            }
            id
        } else {
            sqlx::query(
                "INSERT INTO weekly_schedule_rules
                     (rule_key, streamer_id, title, weekdays_mask, start_time, end_time, timezone,
                      room_url, effective_from, effective_until, enabled, created_at, updated_at,
                      revision)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
            .bind(remote.revision.max(1))
            .execute(&mut **tx)
            .await?
            .last_insert_rowid()
        };
        report.weekly_rules += 1;
        rule_ids.insert(remote.id, id);
        save_mapping(
            tx,
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
        if !applied_schedules.contains(&remote.id) {
            continue;
        }
        let local_rule_id = mapped_local_id(&rule_ids, remote_rule_id)?;
        let local_schedule_id = mapped_local_id(&schedule_ids, remote.id)?;
        let local_updated_at: String =
            sqlx::query_scalar("SELECT updated_at FROM schedules WHERE id = ?")
                .bind(local_schedule_id)
                .fetch_one(&mut **tx)
                .await?;
        let updated_at =
            non_regressing_timestamp(parse_ts(&local_updated_at)?, remote.updated_at).to_rfc3339();
        sqlx::query(
            "UPDATE schedules SET weekly_rule_id = ?, weekly_occurrence_date = ?,
                    revision=MAX(revision + 1, ?), updated_at=? WHERE id = ?",
        )
        .bind(local_rule_id)
        .bind(occurrence_date)
        .bind(remote.revision.max(1))
        .bind(updated_at)
        .bind(local_schedule_id)
        .execute(&mut **tx)
        .await?;
    }

    for remote in &snapshot.schedule_sources {
        let local_schedule_id = mapped_local_id(&schedule_ids, remote.schedule_id)?;
        let existing: Option<(i64, i64)> = sqlx::query_as(
            "SELECT id, revision FROM schedule_sources
                 WHERE schedule_id = ? AND source_identity = ?",
        )
        .bind(local_schedule_id)
        .bind(&remote.source_identity)
        .fetch_optional(&mut **tx)
        .await?;
        let id = if let Some((id, local_revision)) = existing {
            if strategy == "server" {
                sqlx::query(
                    "UPDATE schedule_sources SET source_kind = ?, source_post_id = ?,
                            source_image_url = ?, source_fingerprint = ?, confidence = ?,
                            revision = ?
                         WHERE id = ?",
                )
                .bind(&remote.source_kind)
                .bind(&remote.source_post_id)
                .bind(&remote.source_image_url)
                .bind(&remote.source_fingerprint)
                .bind(remote.confidence)
                .bind(advanced_revision(local_revision, remote.revision))
                .bind(id)
                .execute(&mut **tx)
                .await?;
            }
            id
        } else {
            sqlx::query(
                "INSERT INTO schedule_sources
                        (schedule_id, source_kind, source_post_id, source_image_url,
                         source_fingerprint, source_identity, confidence, created_at, revision)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(local_schedule_id)
            .bind(&remote.source_kind)
            .bind(&remote.source_post_id)
            .bind(&remote.source_image_url)
            .bind(&remote.source_fingerprint)
            .bind(&remote.source_identity)
            .bind(remote.confidence)
            .bind(remote.created_at.to_rfc3339())
            .bind(remote.revision.max(1))
            .execute(&mut **tx)
            .await?
            .last_insert_rowid()
        };
        save_mapping(tx, server_key, "schedule_source", remote.id, id, None).await?;
    }

    for remote in &snapshot.source_suppressions {
        let local_streamer_id = mapped_local_id(&ids, remote.streamer_id)?;
        let existing: Option<(i64, i64)> = sqlx::query_as(
            "SELECT id, revision FROM source_suppressions
                 WHERE streamer_id = ? AND source_post_id = ? AND source_fingerprint = ?",
        )
        .bind(local_streamer_id)
        .bind(&remote.source_post_id)
        .bind(&remote.source_fingerprint)
        .fetch_optional(&mut **tx)
        .await?;
        let id = if let Some((id, local_revision)) = existing {
            if strategy == "server" {
                sqlx::query("UPDATE source_suppressions SET revision = ? WHERE id = ?")
                    .bind(advanced_revision(local_revision, remote.revision))
                    .bind(id)
                    .execute(&mut **tx)
                    .await?;
            }
            id
        } else {
            sqlx::query(
                "INSERT INTO source_suppressions
                        (streamer_id, source_post_id, source_fingerprint, created_at, revision)
                     VALUES (?, ?, ?, ?, ?)",
            )
            .bind(local_streamer_id)
            .bind(&remote.source_post_id)
            .bind(&remote.source_fingerprint)
            .bind(remote.created_at.to_rfc3339())
            .bind(remote.revision.max(1))
            .execute(&mut **tx)
            .await?
            .last_insert_rowid()
        };
        save_mapping(tx, server_key, "source_suppression", remote.id, id, None).await?;
    }

    for remote in &snapshot.weekly_exceptions {
        let local_rule_id = mapped_local_id(&rule_ids, remote.rule_id)?;
        let local_schedule_id = remote
            .schedule_id
            .map(|schedule_id| mapped_local_id(&schedule_ids, schedule_id))
            .transpose()?;
        let existing: Option<(i64, i64)> = sqlx::query_as(
            "SELECT id, revision FROM weekly_schedule_exceptions
                 WHERE rule_id = ? AND occurrence_date = ?",
        )
        .bind(local_rule_id)
        .bind(&remote.occurrence_date)
        .fetch_optional(&mut **tx)
        .await?;
        let id = if let Some((id, local_revision)) = existing {
            if strategy == "server" {
                sqlx::query(
                    "UPDATE weekly_schedule_exceptions
                         SET exception_kind = ?, schedule_id = ?, revision = ? WHERE id = ?",
                )
                .bind(&remote.exception_kind)
                .bind(local_schedule_id)
                .bind(advanced_revision(local_revision, remote.revision))
                .bind(id)
                .execute(&mut **tx)
                .await?;
            }
            id
        } else {
            sqlx::query(
                    "INSERT INTO weekly_schedule_exceptions
                        (rule_id, occurrence_date, exception_kind, schedule_id, created_at, revision)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(local_rule_id)
                .bind(&remote.occurrence_date)
                .bind(&remote.exception_kind)
                .bind(local_schedule_id)
                .bind(remote.created_at.to_rfc3339())
                .bind(remote.revision.max(1))
                .execute(&mut **tx)
                .await?
                .last_insert_rowid()
        };
        save_mapping(tx, server_key, "weekly_exception", remote.id, id, None).await?;
    }

    for remote in &snapshot.live_broadcasts {
        let owner = mapped_local_id(&ids, remote.streamer_id)?;
        let schedule = remote
            .schedule_id
            .map(|id| mapped_local_id(&schedule_ids, id))
            .transpose()?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM live_broadcasts WHERE id=?)")
                .bind(&remote.id)
                .fetch_one(&mut **tx)
                .await?;
        if exists && strategy != "server" {
            continue;
        }
        let occupied: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM live_broadcasts WHERE schedule_id=? AND id<>?)",
        )
        .bind(schedule)
        .bind(&remote.id)
        .fetch_one(&mut **tx)
        .await?;
        if occupied {
            report.skipped += 1;
            continue;
        }
        sqlx::query("INSERT INTO live_broadcasts (id,streamer_id,room_id,started_at,last_segment_started_at,
            last_seen_at,offline_since,schedule_id,schedule_decided,segment_count) VALUES (?,?,?,?,?,?,?,?,?,?)
            ON CONFLICT(id) DO UPDATE SET streamer_id=excluded.streamer_id,room_id=excluded.room_id,
            started_at=excluded.started_at,last_segment_started_at=excluded.last_segment_started_at,
            last_seen_at=excluded.last_seen_at,offline_since=excluded.offline_since,
            schedule_id=excluded.schedule_id,schedule_decided=excluded.schedule_decided,segment_count=excluded.segment_count")
            .bind(&remote.id).bind(owner).bind(&remote.room_id).bind(&remote.started_at)
            .bind(&remote.last_segment_started_at).bind(&remote.last_seen_at).bind(&remote.offline_since)
            .bind(schedule).bind(remote.schedule_decided).bind(remote.segment_count).execute(&mut **tx).await?;
    }
    Ok(report)
}

async fn save_mapping(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    server: &str,
    entity: &str,
    remote: i64,
    local: i64,
    version: Option<DateTime<Utc>>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO remote_entity_map
            (server_key, entity, remote_id, local_id, remote_updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(server_key, entity, remote_id) DO UPDATE SET
            local_id = excluded.local_id,
            remote_updated_at = CASE
                WHEN excluded.remote_updated_at IS NULL
                    THEN remote_entity_map.remote_updated_at
                WHEN remote_entity_map.remote_updated_at IS NULL
                    THEN excluded.remote_updated_at
                WHEN julianday(excluded.remote_updated_at)
                    > julianday(remote_entity_map.remote_updated_at)
                    THEN excluded.remote_updated_at
                ELSE remote_entity_map.remote_updated_at
            END",
    )
    .bind(server)
    .bind(entity)
    .bind(remote)
    .bind(local)
    .bind(version.map(|date| date.to_rfc3339()))
    .execute(&mut **tx)
    .await?;
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

    async fn snapshot_from_store(store: &SqliteStore) -> ServerSnapshot {
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
            latest_event_seq: bootstrap.snapshot_revision,
            exported_at: Utc::now(),
        }
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
                schema_version: SERVER_SNAPSHOT_SCHEMA_VERSION,
                subscriptions: remote.list_streamers().await.unwrap(),
                schedules: remote.list_all_schedules().await.unwrap(),
                weekly_rules: Vec::new(),
                weekly_exceptions: Vec::new(),
                schedule_sources: Vec::new(),
                source_suppressions: Vec::new(),
                live_broadcasts: Vec::new(),
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
            schema_version: SERVER_SNAPSHOT_SCHEMA_VERSION,
            subscriptions: remote.list_streamers().await.unwrap(),
            schedules: vec![event],
            weekly_rules: Vec::new(),
            weekly_exceptions: Vec::new(),
            schedule_sources: Vec::new(),
            source_suppressions: Vec::new(),
            live_broadcasts: Vec::new(),
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
            schema_version: SERVER_SNAPSHOT_SCHEMA_VERSION,
            subscriptions: remote.list_streamers().await.unwrap(),
            schedules: Vec::new(),
            weekly_rules: remote.list_weekly_rules(None).await.unwrap(),
            weekly_exceptions: Vec::new(),
            schedule_sources: Vec::new(),
            source_suppressions: Vec::new(),
            live_broadcasts: Vec::new(),
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

    #[test]
    fn old_snapshot_without_auxiliary_arrays_remains_compatible() {
        let snapshot = ServerSnapshot {
            schema_version: SERVER_SNAPSHOT_SCHEMA_VERSION,
            subscriptions: Vec::new(),
            schedules: Vec::new(),
            weekly_rules: Vec::new(),
            weekly_exceptions: Vec::new(),
            schedule_sources: Vec::new(),
            source_suppressions: Vec::new(),
            live_broadcasts: Vec::new(),
            latest_event_seq: 0,
            exported_at: Utc::now(),
        };
        let mut value = serde_json::to_value(snapshot).unwrap();
        let object = value.as_object_mut().unwrap();
        assert!(object.remove("weeklyExceptions").is_some());
        assert!(object.remove("scheduleSources").is_some());
        assert!(object.remove("sourceSuppressions").is_some());

        let restored: ServerSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert!(restored.weekly_exceptions.is_empty());
        assert!(restored.schedule_sources.is_empty());
        assert!(restored.source_suppressions.is_empty());

        let mut missing_schema = value;
        assert!(missing_schema
            .as_object_mut()
            .unwrap()
            .remove("schemaVersion")
            .is_some());
        assert!(serde_json::from_value::<ServerSnapshot>(missing_schema).is_err());
    }

    #[tokio::test]
    async fn snapshot_import_preserves_auxiliary_entities_and_is_idempotent() {
        let remote = SqliteStore::in_memory().await.unwrap();
        let owner = seed(&remote, "100", "远程主播").await;
        let rule = remote
            .add_weekly_rule(&NewWeeklyScheduleRule {
                rule_key: Some("auxiliary-rule".to_owned()),
                streamer_id: owner.id,
                title: "辅助数据周表".to_owned(),
                weekdays_mask: 0b0000_0001,
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
        let event = schedule(&remote, owner.id).await;
        remote
            .attach_schedule_source(
                event.id,
                &NewSchedule {
                    streamer_id: owner.id,
                    participants: Vec::new(),
                    title: event.title.clone(),
                    start_at: event.start_at,
                    end_at: event.end_at,
                    room_url: event.room_url.clone(),
                    room_owner_mid: None,
                    room_owner_name: None,
                    source_kind: "text".to_owned(),
                    source_post_id: Some("2".to_owned()),
                    source_image_url: None,
                    source_author_mid: Some("2".to_owned()),
                    source_fingerprint: Some("hash-2".to_owned()),
                    source_identity: Some("text:2".to_owned()),
                    source_published_at: None,
                    confidence: 0.8,
                },
            )
            .await
            .unwrap();
        remote
            .save_weekly_exception(rule.id, "2026-11-09", "override", Some(event.id))
            .await
            .unwrap();
        remote
            .add_source_suppression(owner.id, "deleted-post", "deleted-fingerprint")
            .await
            .unwrap();

        let snapshot = snapshot_from_store(&remote).await;
        let remote_exception_id = snapshot.weekly_exceptions[0].id;
        let remote_source_id = snapshot
            .schedule_sources
            .iter()
            .find(|source| source.source_identity == "text:2")
            .unwrap()
            .id;
        let remote_suppression_id = snapshot.source_suppressions[0].id;

        let local = SqliteStore::in_memory().await.unwrap();
        for _ in 0..2 {
            local
                .import_snapshot(&snapshot, "server", "https://server")
                .await
                .unwrap();
        }

        let bootstrap = local.sync_bootstrap().await.unwrap();
        assert_eq!(bootstrap.weekly_rules.len(), 1);
        assert_eq!(bootstrap.weekly_exceptions.len(), 1);
        assert_eq!(bootstrap.schedule_sources.len(), 2);
        assert_eq!(bootstrap.source_suppressions.len(), 1);

        let local_streamer_id = local
            .local_entity("https://server", "subscription", owner.id)
            .await
            .unwrap()
            .unwrap();
        let local_rule_id = local
            .local_entity("https://server", "weekly_rule", rule.id)
            .await
            .unwrap()
            .unwrap();
        let local_schedule_id = local
            .local_entity("https://server", "schedule", event.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bootstrap.weekly_exceptions[0].rule_id, local_rule_id);
        assert_eq!(
            bootstrap.weekly_exceptions[0].schedule_id,
            Some(local_schedule_id)
        );
        let imported_source = bootstrap
            .schedule_sources
            .iter()
            .find(|source| source.source_identity == "text:2")
            .unwrap();
        assert_eq!(imported_source.schedule_id, local_schedule_id);
        assert_eq!(imported_source.confidence, 0.8);
        assert_eq!(
            bootstrap.source_suppressions[0].streamer_id,
            local_streamer_id
        );
        assert!(local
            .local_entity("https://server", "weekly_exception", remote_exception_id)
            .await
            .unwrap()
            .is_some());
        assert!(local
            .local_entity("https://server", "schedule_source", remote_source_id)
            .await
            .unwrap()
            .is_some());
        assert!(local
            .local_entity(
                "https://server",
                "source_suppression",
                remote_suppression_id,
            )
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn server_snapshot_clears_stale_weekly_schedule_association() {
        let remote = SqliteStore::in_memory().await.unwrap();
        let owner = seed(&remote, "504", "周表迁移主播").await;
        schedule(&remote, owner.id).await;
        let snapshot = snapshot_from_store(&remote).await;

        let local = SqliteStore::in_memory().await.unwrap();
        local
            .import_snapshot(&snapshot, "server", "https://server")
            .await
            .unwrap();
        let bootstrap = local.sync_bootstrap().await.unwrap();
        let local_streamer_id = bootstrap.subscriptions[0].id;
        let local_schedule_id = bootstrap.schedules[0].id;
        let rule = local
            .add_weekly_rule(&NewWeeklyScheduleRule {
                rule_key: Some("stale-weekly-rule".to_owned()),
                streamer_id: local_streamer_id,
                title: "旧周表".to_owned(),
                weekdays_mask: 0b0000_0001,
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
        sqlx::query(
            "UPDATE schedules SET weekly_rule_id = ?, weekly_occurrence_date = ? WHERE id = ?",
        )
        .bind(rule.id)
        .bind("2026-11-09")
        .bind(local_schedule_id)
        .execute(local.pool())
        .await
        .unwrap();

        local
            .import_snapshot(&snapshot, "server", "https://server")
            .await
            .unwrap();
        let imported = local.sync_bootstrap().await.unwrap().schedules.remove(0);
        assert!(imported.weekly_rule_id.is_none());
        assert!(imported.weekly_occurrence_date.is_none());
    }

    #[tokio::test]
    async fn migration_import_preserves_schedule_fields_and_commits_once() {
        let remote = SqliteStore::in_memory().await.unwrap();
        let owner = seed(&remote, "501", "迁移主播").await;
        let published_at = Utc::now() - Duration::hours(3);
        remote
            .upsert_schedule(&NewSchedule {
                streamer_id: owner.id,
                participants: vec!["甲".to_owned(), "乙".to_owned()],
                title: "完整迁移日程".to_owned(),
                start_at: Utc::now() + Duration::days(1),
                end_at: Some(Utc::now() + Duration::days(1) + Duration::hours(2)),
                room_url: Some("https://live.bilibili.com/12345".to_owned()),
                room_owner_mid: Some("9001".to_owned()),
                room_owner_name: Some("房主".to_owned()),
                source_kind: "dynamic".to_owned(),
                source_post_id: Some("post-501".to_owned()),
                source_image_url: Some("https://example.com/source.jpg".to_owned()),
                source_author_mid: Some("501".to_owned()),
                source_fingerprint: Some("fingerprint-501".to_owned()),
                source_identity: Some("dynamic:post-501".to_owned()),
                source_published_at: Some(published_at),
                confidence: 0.9,
            })
            .await
            .unwrap();
        let snapshot = snapshot_from_store(&remote).await;

        let local = SqliteStore::in_memory().await.unwrap();
        let digest = local.migration_state_digest().await.unwrap();
        let event = json!({
            "clientId": "client-1",
            "migrationId": "migration-1",
            "strategy": "client"
        });
        let outcome = local
            .import_snapshot_migration(
                &snapshot,
                "server",
                "migration:client-1",
                "migration-1",
                "server:migration_receipt:v1:migration-1",
                "client",
                "sha256:snapshot",
                Some(&digest),
                "data_imported",
                &event,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, MigrationApplyOutcome::Applied(_)));

        let imported = local.sync_bootstrap().await.unwrap().schedules.remove(0);
        assert_eq!(imported.participants, vec!["甲", "乙"]);
        assert_eq!(imported.room_owner_mid.as_deref(), Some("9001"));
        assert_eq!(imported.room_owner_name.as_deref(), Some("房主"));
        assert_eq!(
            imported.source_published_at.map(|value| value.timestamp()),
            Some(published_at.timestamp())
        );

        let sync_changes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sync_changes WHERE entity_type='sync_reset'")
                .fetch_one(local.pool())
                .await
                .unwrap();
        let events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM event_journal WHERE type='data_imported'")
                .fetch_one(local.pool())
                .await
                .unwrap();
        let receipts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM client_sync_state
             WHERE key='server:migration_receipt:v1:migration-1'",
        )
        .fetch_one(local.pool())
        .await
        .unwrap();
        assert_eq!((sync_changes, events, receipts), (1, 1, 1));

        let replay = local
            .import_snapshot_migration(
                &snapshot,
                "server",
                "migration:client-1",
                "migration-1",
                "server:migration_receipt:v1:migration-1",
                "client",
                "sha256:snapshot",
                None,
                "data_imported",
                &event,
            )
            .await
            .unwrap();
        assert!(matches!(replay, MigrationApplyOutcome::Existing(_)));
        let receipts_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_sync_state")
            .fetch_one(local.pool())
            .await
            .unwrap();
        assert_eq!(receipts_after, receipts);
    }

    #[tokio::test]
    async fn migration_rejects_stale_preview_digest_without_writes() {
        let remote = SqliteStore::in_memory().await.unwrap();
        let owner = seed(&remote, "502", "预览主播").await;
        schedule(&remote, owner.id).await;
        let snapshot = snapshot_from_store(&remote).await;

        let local = SqliteStore::in_memory().await.unwrap();
        let expected = local.migration_state_digest().await.unwrap();
        seed(&local, "503", "后改数据").await;
        let error = local
            .import_snapshot_migration(
                &snapshot,
                "server",
                "migration:client-2",
                "migration-2",
                "server:migration_receipt:v1:migration-2",
                "client",
                "sha256:snapshot",
                Some(&expected),
                "data_imported",
                &json!({"clientId": "client-2"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::External { ref code, .. } if code == "MIGRATION_STALE_PREVIEW"
        ));
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_sync_state")
            .fetch_one(local.pool())
            .await
            .unwrap();
        let imported: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schedules")
            .fetch_one(local.pool())
            .await
            .unwrap();
        assert_eq!((receipts, imported), (0, 0));
    }
}
