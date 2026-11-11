use super::*;

impl SqliteStore {
    /// Refresh device liveness without changing its acknowledged sequence.
    pub async fn touch_device(&self, device_id: &str) -> AppResult<()> {
        let device_id = device_id.trim();
        if device_id.is_empty() {
            return Err(AppError::validation("deviceId 不能为空"));
        }
        let updated = sqlx::query(
            "UPDATE devices SET last_seen_at = ? WHERE device_id = ? AND revoked_at IS NULL",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(device_id)
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::not_found(format!("设备 {device_id}")));
        }
        Ok(())
    }

    /// Commit the write, notification event and replay receipt atomically.
    pub async fn apply_schedule_mutation(&self, mutation: &SyncMutation) -> AppResult<Value> {
        if mutation.entity != "schedule"
            || mutation.mutation_id.trim().is_empty()
            || mutation.mutation_id.len() > 128
        {
            return Err(AppError::validation("日程同步请求标识无效"));
        }
        let request_json = serde_json::to_string(&json!({
            "entity":mutation.entity, "operation":mutation.operation,
            "payload":mutation.payload, "baseUpdatedAt":mutation.base_updated_at,
            "deviceId":mutation.device_id, "baseRevision":mutation.base_revision
        }))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let receipt = sqlx::query(
            "SELECT request_json, result_json FROM mutation_receipts WHERE mutation_id = ?",
        )
        .bind(&mutation.mutation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(receipt) = receipt {
            if receipt.get::<String, _>("request_json") != request_json {
                return Err(AppError::validation("同步 ID 已用于不同的请求内容"));
            }
            let value = serde_json::from_str(&receipt.get::<String, _>("result_json"))?;
            tx.commit().await?;
            return Ok(value);
        }
        let now = Utc::now().to_rfc3339();
        let actor = match mutation.device_id.as_deref() {
            Some(device_id) => SyncActor::device(device_id),
            None => SyncActor::system(),
        }
        .with_mutation(&mutation.mutation_id);
        let id = match mutation.operation.as_str() {
            "create" => {
                let input: NewSchedule = serde_json::from_value(mutation.payload.clone())?;
                validate_schedule_input(&input)?;
                if input.source_kind != "manual" {
                    return Err(AppError::validation("离线新增只允许手动日程"));
                }
                let existing: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM schedules WHERE streamer_id = ? AND start_at = ? AND title = ?",
                )
                .bind(input.streamer_id)
                .bind(input.start_at.to_rfc3339())
                .bind(&input.title)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(id) = existing {
                    id
                } else {
                    let participants_json = serde_json::to_string(&input.participants)?;
                    sqlx::query(
                        "INSERT INTO schedules
                        (streamer_id,participants_json,title,start_at,end_at,room_url,
                         room_owner_mid,room_owner_name,source_kind,source_post_id,
                         source_image_url,source_author_mid,source_fingerprint,source_identity,
                         source_published_at,confidence,manual_locked,created_at,updated_at)
                        VALUES (?,?,?,?,?,?,?,?,'manual',?,?,?,?,?,?,1.0,1,?,?)",
                    )
                    .bind(input.streamer_id)
                    .bind(participants_json)
                    .bind(input.title)
                    .bind(input.start_at.to_rfc3339())
                    .bind(input.end_at.map(|time| time.to_rfc3339()))
                    .bind(input.room_url)
                    .bind(input.room_owner_mid)
                    .bind(input.room_owner_name)
                    .bind(input.source_post_id)
                    .bind(input.source_image_url)
                    .bind(input.source_author_mid)
                    .bind(input.source_fingerprint)
                    .bind(format!("manual:mutation:{}", mutation.mutation_id))
                    .bind(input.source_published_at.map(|time| time.to_rfc3339()))
                    .bind(&now)
                    .bind(&now)
                    .execute(&mut *tx)
                    .await?
                    .last_insert_rowid()
                }
            }
            "update" | "delete" => {
                let id = mutation
                    .payload
                    .get("id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| AppError::validation("缺少日程 id"))?;
                let current = sqlx::query("SELECT * FROM schedules WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| AppError::not_found(format!("日程 {id}")))?;
                let current_revision: i64 = current.get("revision");
                if let Some(base_revision) = mutation.base_revision {
                    if base_revision != current_revision {
                        return Err(AppError::External {
                            code: "SYNC_CONFLICT".to_owned(),
                            message: format!(
                                "服务器日程已变化：期望 revision {base_revision}，当前 {current_revision}"
                            ),
                        });
                    }
                } else {
                    let base = mutation.base_updated_at.ok_or_else(|| {
                        AppError::validation("修改和删除必须提供 baseRevision 或 baseUpdatedAt")
                    })?;
                    let current_version = parse_ts(&current.get::<String, _>("updated_at"))?;
                    if base != current_version {
                        return Err(AppError::External {
                            code: "SYNC_CONFLICT".to_owned(),
                            message: "服务器日程已变化，请选择冲突策略".to_owned(),
                        });
                    }
                }
                if mutation.operation == "delete" {
                    if let (Some(rule_id), Some(occurrence_date)) = (
                        current.try_get::<Option<i64>, _>("weekly_rule_id")?,
                        current.try_get::<Option<String>, _>("weekly_occurrence_date")?,
                    ) {
                        let rule_exists: Option<i64> =
                            sqlx::query_scalar("SELECT id FROM weekly_schedule_rules WHERE id = ?")
                                .bind(rule_id)
                                .fetch_optional(&mut *tx)
                                .await?;
                        if rule_exists.is_some() {
                            self.save_weekly_exception_on(
                                &mut tx,
                                rule_id,
                                &occurrence_date,
                                "skip",
                                None,
                                actor,
                            )
                            .await?;
                        }
                    }

                    // Preserve every source carried by the aggregate or its
                    // merged source rows. Use the normal helper so suppression
                    // revisions and sync events are not bypassed.
                    let source_kind: String = current.try_get("source_kind")?;
                    let streamer_id: i64 = current.try_get("streamer_id")?;
                    let mut suppressions = Vec::<(String, String)>::new();
                    if source_kind != "manual" {
                        if let (Some(post_id), Some(fingerprint)) = (
                            current.try_get::<Option<String>, _>("source_post_id")?,
                            current.try_get::<Option<String>, _>("source_fingerprint")?,
                        ) {
                            suppressions.push((post_id, fingerprint));
                        }
                        let source_rows = sqlx::query(
                            "SELECT source_post_id, source_fingerprint
                             FROM schedule_sources
                             WHERE schedule_id = ? AND source_post_id IS NOT NULL
                               AND source_fingerprint IS NOT NULL",
                        )
                        .bind(id)
                        .fetch_all(&mut *tx)
                        .await?;
                        for row in source_rows {
                            suppressions.push((
                                row.try_get::<String, _>("source_post_id")?,
                                row.try_get::<String, _>("source_fingerprint")?,
                            ));
                        }
                    }
                    suppressions.sort();
                    suppressions.dedup();
                    for (post_id, fingerprint) in suppressions {
                        self.add_source_suppression_on(
                            &mut tx,
                            streamer_id,
                            &post_id,
                            &fingerprint,
                            actor,
                        )
                        .await?;
                    }

                    // The aggregate delete cascades to schedule_sources. Emit
                    // their tombstones before the cascade while rows are still
                    // available to the sync payload builder.
                    let source_ids: Vec<i64> = sqlx::query_scalar(
                        "SELECT id FROM schedule_sources WHERE schedule_id = ? ORDER BY id",
                    )
                    .bind(id)
                    .fetch_all(&mut *tx)
                    .await?;
                    for source_id in source_ids {
                        self.emit_schedule_source_change(&mut tx, source_id, "delete", actor)
                            .await?;
                    }
                    self.emit_schedule_change(&mut tx, id, "delete", actor)
                        .await?;
                    let deleted = sqlx::query("DELETE FROM schedules WHERE id = ?")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    if deleted.rows_affected() != 1 {
                        return Err(AppError::not_found(format!("日程 {id}")));
                    }
                } else {
                    let update: UpdateSchedule = serde_json::from_value(
                        mutation
                            .payload
                            .get("update")
                            .cloned()
                            .ok_or_else(|| AppError::validation("缺少 update"))?,
                    )?;
                    let title = update.title.unwrap_or_else(|| current.get("title"));
                    let start = update
                        .start_at
                        .unwrap_or(parse_ts(&current.get::<String, _>("start_at"))?);
                    let end = update.end_at.or(parse_opt_ts(
                        current.get::<Option<String>, _>("end_at").as_deref(),
                    )?);
                    let room = update.room_url.or_else(|| current.get("room_url"));
                    if title.trim().is_empty() || title.chars().count() > 200 {
                        return Err(AppError::validation("日程标题长度必须为 1 到 200 字"));
                    }
                    if end.is_some_and(|time| time <= start) {
                        return Err(AppError::validation("结束时间必须晚于开始时间"));
                    }
                    if let Some(url) = &room {
                        validate_live_url(url)?;
                    }
                    sqlx::query(
                            "UPDATE schedules SET title=?,start_at=?,end_at=?,room_url=?,
                        manual_locked=1,source_changed=0,updated_at=?,revision=revision+1 WHERE id=?",
                        )
                        .bind(title.trim())
                        .bind(start.to_rfc3339())
                        .bind(end.map(|time| time.to_rfc3339()))
                        .bind(room)
                        .bind(&now)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    self.emit_schedule_change(&mut tx, id, "upsert", actor)
                        .await?;
                }
                id
            }
            _ => return Err(AppError::validation("不支持的日程同步操作")),
        };
        if mutation.operation == "create" {
            self.emit_schedule_change(&mut tx, id, "upsert", actor)
                .await?;
        }
        let result = json!({"scheduleId":id});
        sqlx::query("INSERT INTO event_journal(type,payload_json,created_at) VALUES('schedule_changed',?,?)")
            .bind(json!({"action":mutation.operation,"scheduleId":id}).to_string())
            .bind(&now).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO mutation_receipts(mutation_id,request_json,result_json,applied_at) VALUES(?,?,?,?)")
            .bind(&mutation.mutation_id).bind(request_json).bind(result.to_string()).bind(&now)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn apply_subscription_mutation(&self, mutation: &SyncMutation) -> AppResult<Value> {
        validate_mutation_shape(mutation, "subscription", &["update"])?;
        let request_json = mutation_request_json(mutation)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let receipt = sqlx::query(
            "SELECT request_json, result_json FROM mutation_receipts WHERE mutation_id = ?",
        )
        .bind(&mutation.mutation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(receipt) = receipt {
            if receipt.get::<String, _>("request_json") != request_json {
                return Err(AppError::validation("同步 ID 已用于不同的请求内容"));
            }
            let value: Value = serde_json::from_str(&receipt.get::<String, _>("result_json"))?;
            tx.commit().await?;
            return Ok(value);
        }

        let payload = mutation
            .payload
            .as_object()
            .ok_or_else(|| AppError::validation("subscription update payload 必须是对象"))?;
        let id = payload
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| AppError::validation("缺少订阅 id"))?;
        let current = sqlx::query(
            "SELECT name, schedule_enabled, live_notify_enabled, live_stats_enabled,
                    aliases_json, revision
             FROM streamers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::not_found(format!("主播 {id}")))?;
        let current_revision: i64 = current.get("revision");
        if let Some(base_revision) = mutation.base_revision {
            if base_revision != current_revision {
                return Err(AppError::External {
                    code: "SYNC_CONFLICT".to_owned(),
                    message: format!(
                        "订阅已变化：期望 revision {base_revision}，当前 {current_revision}"
                    ),
                });
            }
        }

        let update: UpdateSubscription = serde_json::from_value(
            payload
                .get("update")
                .cloned()
                .ok_or_else(|| AppError::validation("缺少 update"))?,
        )?;
        let name: String = current.get("name");
        let mut aliases =
            serde_json::from_str::<Vec<String>>(&current.get::<String, _>("aliases_json"))
                .map_err(|_| AppError::validation("主播别名数据损坏"))?;
        if let Some(value) = update.aliases {
            aliases = normalize_aliases(value, &name);
        }
        let schedule_enabled = update
            .schedule_enabled
            .unwrap_or_else(|| current.get::<i64, _>("schedule_enabled") != 0);
        let live_notify_enabled = update
            .live_notify_enabled
            .unwrap_or_else(|| current.get::<i64, _>("live_notify_enabled") != 0);
        let live_stats_enabled = update
            .live_stats_enabled
            .unwrap_or_else(|| current.get::<i64, _>("live_stats_enabled") != 0);
        let aliases_json = serde_json::to_string(&aliases)?;
        let current_schedule_enabled = current.get::<i64, _>("schedule_enabled") != 0;
        let current_live_notify_enabled = current.get::<i64, _>("live_notify_enabled") != 0;
        let current_live_stats_enabled = current.get::<i64, _>("live_stats_enabled") != 0;
        let current_aliases_json: String = current.get("aliases_json");
        let changed = schedule_enabled != current_schedule_enabled
            || live_notify_enabled != current_live_notify_enabled
            || live_stats_enabled != current_live_stats_enabled
            || aliases_json != current_aliases_json;
        let now = Utc::now().to_rfc3339();
        if !changed {
            // A repeated UI save is not a new synchronization fact. Keep the
            // entity revision and feed unchanged while still recording the
            // mutation receipt so a network retry remains idempotent.
            let result = json!({
                "streamerId": id,
                "revision": current_revision,
            });
            sqlx::query(
                "INSERT INTO mutation_receipts(mutation_id,request_json,result_json,applied_at)
                 VALUES(?,?,?,?)",
            )
            .bind(&mutation.mutation_id)
            .bind(request_json)
            .bind(result.to_string())
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(result);
        }

        let updated = sqlx::query(
            "UPDATE streamers SET schedule_enabled = ?, live_notify_enabled = ?,
                    live_stats_enabled = ?, aliases_json = ?, updated_at = ?,
                    revision = revision + 1
             WHERE id = ? AND revision = ?",
        )
        .bind(schedule_enabled)
        .bind(live_notify_enabled)
        .bind(live_stats_enabled)
        .bind(aliases_json)
        .bind(&now)
        .bind(id)
        .bind(current_revision)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::External {
                code: "SYNC_CONFLICT".to_owned(),
                message: format!("订阅已变化：期望 revision {current_revision}，请重新同步后再试"),
            });
        }
        self.emit_streamer_change(&mut tx, id, "upsert", sync_actor_for_mutation(mutation))
            .await?;
        let result = json!({
            "streamerId": id,
            "revision": current_revision + 1,
        });
        sqlx::query(
            "INSERT INTO mutation_receipts(mutation_id,request_json,result_json,applied_at)
             VALUES(?,?,?,?)",
        )
        .bind(&mutation.mutation_id)
        .bind(request_json)
        .bind(result.to_string())
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn apply_link_mutation(&self, mutation: &SyncMutation) -> AppResult<Value> {
        validate_mutation_shape(mutation, "link", &["create", "delete"])?;
        let request_json = mutation_request_json(mutation)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let receipt = sqlx::query(
            "SELECT request_json, result_json FROM mutation_receipts WHERE mutation_id = ?",
        )
        .bind(&mutation.mutation_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(receipt) = receipt {
            if receipt.get::<String, _>("request_json") != request_json {
                return Err(AppError::validation("同步 ID 已用于不同的请求内容"));
            }
            let value: Value = serde_json::from_str(&receipt.get::<String, _>("result_json"))?;
            tx.commit().await?;
            return Ok(value);
        }

        let streamer_id = mutation
            .payload
            .get("streamerId")
            .and_then(Value::as_i64)
            .ok_or_else(|| AppError::validation("缺少 streamerId"))?;
        let mid = mutation
            .payload
            .get("mid")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::validation("缺少官号 mid"))?;
        let existing = sqlx::query_as::<_, LinkedAccountRow>(
            "SELECT id, streamer_id, platform, mid, name, avatar_url, handle, profile_url,
                    revision, created_at
             FROM linked_accounts WHERE streamer_id = ? AND mid = ?",
        )
        .bind(streamer_id)
        .bind(mid)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(base_revision) = mutation.base_revision {
            let current_revision = existing.as_ref().map_or(0, |row| row.revision);
            if base_revision != current_revision {
                return Err(AppError::External {
                    code: "SYNC_CONFLICT".to_owned(),
                    message: format!(
                        "官号关联已变化：期望 revision {base_revision}，当前 {current_revision}"
                    ),
                });
            }
        }
        let now = Utc::now().to_rfc3339();
        let actor = sync_actor_for_mutation(mutation);
        let result = match mutation.operation.as_str() {
            "create" => {
                let (streamer_name, streamer_platform): (String, String) =
                    sqlx::query_as("SELECT name, platform FROM streamers WHERE id = ?")
                        .bind(streamer_id)
                        .fetch_optional(&mut *tx)
                        .await?
                        .ok_or_else(|| AppError::not_found(format!("主播 {streamer_id}")))?;
                let name = mutation
                    .payload
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(streamer_name.as_str());
                if name.chars().count() > 80 {
                    return Err(AppError::validation("官号昵称长度必须为 1 到 80 字"));
                }
                let avatar_url = mutation
                    .payload
                    .get("avatarUrl")
                    .or_else(|| mutation.payload.get("avatar_url"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let platform = mutation
                    .payload
                    .get("platform")
                    .and_then(Value::as_str)
                    .map(parse_platform)
                    .transpose()?
                    .unwrap_or(Platform::Bilibili);
                let handle = mutation
                    .payload
                    .get("handle")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let profile_url = mutation
                    .payload
                    .get("profileUrl")
                    .or_else(|| mutation.payload.get("profile_url"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                validate_platform_mid(platform, mid)?;
                if parse_platform(&streamer_platform)? != platform {
                    return Err(AppError::validation("关联账号必须与主播使用同一平台"));
                }
                sqlx::query(
                    "INSERT INTO linked_accounts
                        (streamer_id, platform, mid, name, avatar_url, handle, profile_url, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(streamer_id, mid) DO UPDATE SET
                        platform = excluded.platform,
                        name = excluded.name,
                        avatar_url = CASE
                            WHEN excluded.avatar_url LIKE 'data:image/%' THEN excluded.avatar_url
                            WHEN linked_accounts.avatar_url LIKE 'data:image/%' THEN linked_accounts.avatar_url
                            WHEN excluded.avatar_url <> '' THEN excluded.avatar_url
                            ELSE linked_accounts.avatar_url END,
                        handle = COALESCE(excluded.handle, linked_accounts.handle),
                        profile_url = COALESCE(excluded.profile_url, linked_accounts.profile_url),
                        revision = linked_accounts.revision + 1",
                )
                .bind(streamer_id)
                .bind(platform.as_str())
                .bind(mid)
                .bind(name)
                .bind(avatar_url)
                .bind(handle)
                .bind(profile_url)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                let row = sqlx::query_as::<_, LinkedAccountRow>(
                    "SELECT id, streamer_id, platform, mid, name, avatar_url, handle, profile_url,
                            revision, created_at
                     FROM linked_accounts WHERE streamer_id = ? AND mid = ?",
                )
                .bind(streamer_id)
                .bind(mid)
                .fetch_one(&mut *tx)
                .await?;
                self.emit_linked_account_change(&mut tx, row.id, "upsert", actor)
                    .await?;
                json!({
                    "linkedAccountId": row.id,
                    "streamerId": streamer_id,
                    "platform": row.platform,
                    "mid": mid,
                    "handle": row.handle,
                    "profileUrl": row.profile_url,
                    "revision": row.revision,
                })
            }
            "delete" => {
                let row = existing.ok_or_else(|| AppError::not_found(format!("官号 {mid}")))?;
                self.emit_linked_account_change(&mut tx, row.id, "delete", actor)
                    .await?;
                let deleted =
                    sqlx::query("DELETE FROM linked_accounts WHERE id = ? AND revision = ?")
                        .bind(row.id)
                        .bind(row.revision)
                        .execute(&mut *tx)
                        .await?;
                if deleted.rows_affected() != 1 {
                    return Err(AppError::External {
                        code: "SYNC_CONFLICT".to_owned(),
                        message: "官号关联已变化，请重新同步后再试".to_owned(),
                    });
                }
                json!({"streamerId": streamer_id, "mid": mid})
            }
            _ => return Err(AppError::validation("不支持的官号同步操作")),
        };
        sqlx::query(
            "INSERT INTO mutation_receipts(mutation_id,request_json,result_json,applied_at)
             VALUES(?,?,?,?)",
        )
        .bind(&mutation.mutation_id)
        .bind(request_json)
        .bind(result.to_string())
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

fn validate_mutation_shape(
    mutation: &SyncMutation,
    entity: &str,
    operations: &[&str],
) -> AppResult<()> {
    if mutation.entity != entity {
        return Err(AppError::validation("同步实体与处理程序不匹配"));
    }
    if !operations.contains(&mutation.operation.as_str()) {
        return Err(AppError::validation("不支持的同步操作"));
    }
    if mutation.mutation_id.trim().is_empty() || mutation.mutation_id.len() > 128 {
        return Err(AppError::validation("同步请求标识无效"));
    }
    Ok(())
}

fn mutation_request_json(mutation: &SyncMutation) -> AppResult<String> {
    Ok(serde_json::to_string(&json!({
        "entity": mutation.entity,
        "operation": mutation.operation,
        "payload": mutation.payload,
        "baseUpdatedAt": mutation.base_updated_at,
        "deviceId": mutation.device_id,
        "baseRevision": mutation.base_revision
    }))?)
}

fn sync_actor_for_mutation(mutation: &SyncMutation) -> SyncActor<'_> {
    match mutation.device_id.as_deref() {
        Some(device_id) => SyncActor::device(device_id),
        None => SyncActor::system(),
    }
    .with_mutation(&mutation.mutation_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn fixture(store: &SqliteStore) -> Schedule {
        let streamer = store
            .add_subscription(&AddSubscription {
                platform: Platform::Bilibili,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
                mid: "100".into(),
                name: Some("同步主播".into()),
                avatar_url: None,
                room_id: None,
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
            })
            .await
            .unwrap();
        store
            .upsert_schedule(&NewSchedule {
                streamer_id: streamer.id,
                participants: Vec::new(),
                title: "待修改日程".into(),
                start_at: Utc::now() + Duration::days(1),
                end_at: None,
                room_url: None,
                room_owner_mid: None,
                room_owner_name: None,
                source_kind: "manual".into(),
                source_post_id: None,
                source_image_url: None,
                source_author_mid: None,
                source_fingerprint: None,
                source_identity: Some("manual:fixture".into()),
                source_published_at: None,
                confidence: 1.0,
            })
            .await
            .unwrap()
            .0
    }

    fn mutation(schedule: &Schedule, operation: &str) -> SyncMutation {
        SyncMutation {
            mutation_id: "persistent-mutation".into(),
            operation: operation.into(),
            entity: "schedule".into(),
            payload: json!({"id":schedule.id,"update":{"title":"新的手动标题"}}),
            base_updated_at: Some(schedule.updated_at),
            device_id: None,
            base_revision: None,
            status: "pending".into(),
            last_error: None,
            created_at: Utc::now(),
            applied_at: None,
            result_json: None,
        }
    }

    #[tokio::test]
    async fn offline_create_preserves_participants_and_source_fields() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&AddSubscription {
                platform: Platform::Bilibili,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
                mid: "100200300".into(),
                name: Some("离线主播".into()),
                avatar_url: None,
                room_id: None,
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
            })
            .await
            .unwrap();
        let published_at = Utc::now() - Duration::hours(2);
        let input = NewSchedule {
            streamer_id: streamer.id,
            participants: vec!["甲".into(), "乙".into()],
            title: "离线完整日程".into(),
            start_at: Utc::now() + Duration::days(1),
            end_at: Some(Utc::now() + Duration::days(1) + Duration::hours(1)),
            room_url: Some("https://live.bilibili.com/1".into()),
            room_owner_mid: Some("owner".into()),
            room_owner_name: Some("房主".into()),
            source_kind: "manual".into(),
            source_post_id: Some("post".into()),
            source_image_url: Some("https://example.com/image.jpg".into()),
            source_author_mid: Some("author".into()),
            source_fingerprint: Some("fingerprint".into()),
            source_identity: Some("manual:offline-rich".into()),
            source_published_at: Some(published_at),
            confidence: 1.0,
        };
        let request = SyncMutation {
            mutation_id: "offline-rich-create".into(),
            operation: "create".into(),
            entity: "schedule".into(),
            payload: serde_json::to_value(&input).unwrap(),
            base_updated_at: None,
            device_id: Some("device-rich".into()),
            base_revision: None,
            status: "pending".into(),
            last_error: None,
            created_at: Utc::now(),
            applied_at: None,
            result_json: None,
        };
        let result = store.apply_schedule_mutation(&request).await.unwrap();
        let schedule_id = result["scheduleId"].as_i64().unwrap();
        let created = store.get_schedule(schedule_id).await.unwrap();
        assert_eq!(created.participants, input.participants);
        assert_eq!(created.room_owner_mid, input.room_owner_mid);
        assert_eq!(created.room_owner_name, input.room_owner_name);
        assert_eq!(created.source_post_id, input.source_post_id);
        assert_eq!(created.source_image_url, input.source_image_url);
        assert_eq!(created.source_author_mid, input.source_author_mid);
        assert_eq!(created.source_fingerprint, input.source_fingerprint);
        assert_eq!(
            created.source_published_at.map(|value| value.timestamp()),
            Some(published_at.timestamp())
        );
    }

    #[tokio::test]
    async fn concurrent_replay_commits_one_write_and_one_event() {
        let store = SqliteStore::in_memory().await.unwrap();
        let schedule = fixture(&store).await;
        let request = mutation(&schedule, "update");
        let (a, b) = tokio::join!(
            store.apply_schedule_mutation(&request),
            store.apply_schedule_mutation(&request)
        );
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(store.latest_event_seq().await.unwrap(), 1);
        assert_eq!(
            store.get_schedule(schedule.id).await.unwrap().title,
            "新的手动标题"
        );
        let mut changed = request.clone();
        changed.payload["update"]["title"] = json!("伪造重放");
        assert!(store.apply_schedule_mutation(&changed).await.is_err());
        assert_eq!(
            store.get_schedule(schedule.id).await.unwrap().title,
            "新的手动标题"
        );
    }

    #[tokio::test]
    async fn stale_delete_conflicts_without_receipt_or_event() {
        let store = SqliteStore::in_memory().await.unwrap();
        let schedule = fixture(&store).await;
        let mut request = mutation(&schedule, "delete");
        request.base_updated_at = Some(schedule.updated_at - Duration::seconds(1));
        let result = store.apply_schedule_mutation(&request).await;
        assert!(matches!(result,Err(AppError::External {code,..}) if code=="SYNC_CONFLICT"));
        assert!(store.get_schedule(schedule.id).await.is_ok());
        assert_eq!(store.latest_event_seq().await.unwrap(), 0);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mutation_receipts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn delete_receipt_survives_database_reopen() {
        let path = std::env::temp_dir().join(format!("oshical-sync-{}.db", uuid::Uuid::new_v4()));
        let store = SqliteStore::connect(&path).await.unwrap();
        let schedule = fixture(&store).await;
        let request = mutation(&schedule, "delete");
        let receipt = store.apply_schedule_mutation(&request).await.unwrap();
        store.pool.close().await;
        let reopened = SqliteStore::connect(&path).await.unwrap();
        assert_eq!(
            receipt,
            reopened.apply_schedule_mutation(&request).await.unwrap()
        );
        assert_eq!(reopened.latest_event_seq().await.unwrap(), 1);
        reopened.pool.close().await;
        drop(reopened);
        drop(store);
        // SQLx closes its SQLite worker asynchronously on Windows.
        for attempt in 0..100 {
            match std::fs::remove_file(&path) {
                Ok(()) => break,
                Err(error) if attempt == 99 => {
                    eprintln!("temporary database cleanup skipped: {error}");
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
    }

    #[tokio::test]
    async fn old_sync_queue_gains_receipt_column_without_losing_operations() {
        let options = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE sync_queue (
            mutation_id TEXT PRIMARY KEY,operation TEXT NOT NULL,entity TEXT NOT NULL,
            payload_json TEXT NOT NULL,base_updated_at TEXT,status TEXT NOT NULL,
            last_error TEXT,created_at TEXT NOT NULL,applied_at TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO sync_queue VALUES('legacy','delete','schedule','{}',NULL,'pending',NULL,?,NULL)")
            .bind(Utc::now().to_rfc3339()).execute(&pool).await.unwrap();
        let store = SqliteStore::initialize(pool).await.unwrap();
        let operations = store.list_pending_mutations(10).await.unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].mutation_id, "legacy");
        assert!(operations[0].result_json.is_none());
        // Initialization remains idempotent.
        let store = SqliteStore::initialize(store.pool).await.unwrap();
        assert_eq!(store.list_pending_mutations(10).await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn base_revision_protects_multi_device_updates_and_records_actor() {
        let store = SqliteStore::in_memory().await.unwrap();
        let schedule = fixture(&store).await;
        let mut request = mutation(&schedule, "update");
        request.mutation_id = "revision-device-a".into();
        request.device_id = Some("device-a".into());
        request.base_revision = Some(schedule.revision);
        store.apply_schedule_mutation(&request).await.unwrap();
        let updated = store.get_schedule(schedule.id).await.unwrap();
        assert_eq!(updated.revision, schedule.revision + 1);
        let actor: (String, Option<String>) = sqlx::query_as(
            "SELECT actor_type, actor_id FROM sync_changes
             WHERE entity_type = 'schedule' AND entity_id = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(schedule.id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(actor.0, "device");
        assert_eq!(actor.1.as_deref(), Some("device-a"));
        let mut stale = mutation(&updated, "update");
        stale.mutation_id = "revision-device-b".into();
        stale.device_id = Some("device-b".into());
        stale.base_revision = Some(schedule.revision);
        let result = store.apply_schedule_mutation(&stale).await;
        assert!(matches!(result, Err(AppError::External { code, .. }) if code == "SYNC_CONFLICT"));
    }

    fn subscription_mutation(
        streamer: &Streamer,
        mutation_id: &str,
        schedule_enabled: bool,
    ) -> SyncMutation {
        SyncMutation {
            mutation_id: mutation_id.into(),
            operation: "update".into(),
            entity: "subscription".into(),
            payload: json!({
                "id": streamer.id,
                "update": {"scheduleEnabled": schedule_enabled}
            }),
            base_updated_at: None,
            device_id: Some(format!("device-{mutation_id}")),
            base_revision: Some(streamer.revision),
            status: "pending".into(),
            last_error: None,
            created_at: Utc::now(),
            applied_at: None,
            result_json: None,
        }
    }

    #[tokio::test]
    async fn concurrent_subscription_updates_only_one_mutation_wins() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&AddSubscription {
                platform: Platform::Bilibili,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
                mid: "310".into(),
                name: Some("并发订阅主播".into()),
                avatar_url: None,
                room_id: None,
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
            })
            .await
            .unwrap();
        let first = subscription_mutation(&streamer, "subscription-a", false);
        let second = subscription_mutation(&streamer, "subscription-b", false);
        let (first_result, second_result) = tokio::join!(
            store.apply_subscription_mutation(&first),
            store.apply_subscription_mutation(&second)
        );
        assert_ne!(first_result.is_ok(), second_result.is_ok());
        let conflict = if first_result.is_err() {
            first_result
        } else {
            second_result
        };
        assert!(matches!(
            conflict,
            Err(AppError::External { code, .. }) if code == "SYNC_CONFLICT"
        ));
        let current = store.get_streamer(streamer.id).await.unwrap();
        assert_eq!(current.revision, streamer.revision + 1);
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mutation_receipts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(receipts, 1);
    }

    #[tokio::test]
    async fn subscription_replay_keeps_first_result_and_actor() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&AddSubscription {
                platform: Platform::Bilibili,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
                mid: "311".into(),
                name: Some("幂等订阅主播".into()),
                avatar_url: None,
                room_id: None,
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
            })
            .await
            .unwrap();
        let request = subscription_mutation(&streamer, "subscription-replay", false);
        let first = store.apply_subscription_mutation(&request).await.unwrap();
        let replay = store.apply_subscription_mutation(&request).await.unwrap();
        assert_eq!(first, replay);
        let current = store.get_streamer(streamer.id).await.unwrap();
        assert_eq!(current.revision, streamer.revision + 1);
        let changes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sync_changes WHERE mutation_id = ?")
                .bind(&request.mutation_id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(changes, 1);
        let actor: (String, Option<String>) =
            sqlx::query_as("SELECT actor_type, actor_id FROM sync_changes WHERE mutation_id = ?")
                .bind(&request.mutation_id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(actor.0, "device");
        assert_eq!(actor.1.as_deref(), Some("device-subscription-replay"));
    }

    #[tokio::test]
    async fn link_create_and_delete_replays_do_not_duplicate_or_return_404() {
        let store = SqliteStore::in_memory().await.unwrap();
        let streamer = store
            .add_subscription(&AddSubscription {
                platform: Platform::Bilibili,
                handle: None,
                profile_url: None,
                uploads_playlist_id: None,
                mid: "312".into(),
                name: Some("官号同步主播".into()),
                avatar_url: None,
                room_id: None,
                aliases: None,
                schedule_enabled: true,
                live_notify_enabled: true,
                live_stats_enabled: false,
            })
            .await
            .unwrap();
        let create = SyncMutation {
            mutation_id: "link-create-replay".into(),
            operation: "create".into(),
            entity: "link".into(),
            payload: json!({
                "streamerId": streamer.id,
                "mid": "200",
                "name": "同步官号",
                "avatarUrl": ""
            }),
            base_updated_at: None,
            device_id: Some("device-link".into()),
            base_revision: None,
            status: "pending".into(),
            last_error: None,
            created_at: Utc::now(),
            applied_at: None,
            result_json: None,
        };
        let first = store.apply_link_mutation(&create).await.unwrap();
        let replay = store.apply_link_mutation(&create).await.unwrap();
        assert_eq!(first, replay);
        let loaded = store.get_streamer(streamer.id).await.unwrap();
        assert_eq!(loaded.linked_accounts.len(), 1);
        assert_eq!(loaded.linked_accounts[0].revision, 1);

        let delete = SyncMutation {
            mutation_id: "link-delete-replay".into(),
            operation: "delete".into(),
            entity: "link".into(),
            payload: json!({"streamerId": streamer.id, "mid": "200"}),
            base_updated_at: None,
            device_id: Some("device-link".into()),
            base_revision: None,
            status: "pending".into(),
            last_error: None,
            created_at: Utc::now(),
            applied_at: None,
            result_json: None,
        };
        let first_delete = store.apply_link_mutation(&delete).await.unwrap();
        let replay_delete = store.apply_link_mutation(&delete).await.unwrap();
        assert_eq!(first_delete, replay_delete);
        let loaded = store.get_streamer(streamer.id).await.unwrap();
        assert!(loaded.linked_accounts.is_empty());
        let changes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sync_changes WHERE mutation_id IN (?, ?)")
                .bind(&create.mutation_id)
                .bind(&delete.mutation_id)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(changes, 2);
    }
}
