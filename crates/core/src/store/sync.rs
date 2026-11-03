use super::*;

impl SqliteStore {
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
                    sqlx::query(
                        "INSERT INTO schedules
                        (streamer_id,title,start_at,end_at,room_url,source_kind,source_identity,
                         confidence,manual_locked,created_at,updated_at)
                        VALUES (?,?,?,?,?,'manual',?,1.0,1,?,?)",
                    )
                    .bind(input.streamer_id)
                    .bind(input.title)
                    .bind(input.start_at.to_rfc3339())
                    .bind(input.end_at.map(|time| time.to_rfc3339()))
                    .bind(input.room_url)
                    .bind(format!("manual:mutation:{}", mutation.mutation_id))
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
                    // Preserve every merged source before deleting the aggregate.
                    sqlx::query(
                        "INSERT OR IGNORE INTO source_suppressions
                        (streamer_id,source_post_id,source_fingerprint,created_at)
                        SELECT streamer_id,source_post_id,source_fingerprint,? FROM schedules
                        WHERE id = ? AND source_kind <> 'manual'
                        AND source_post_id IS NOT NULL AND source_fingerprint IS NOT NULL",
                    )
                    .bind(&now)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query(
                        "INSERT OR IGNORE INTO source_suppressions
                        (streamer_id,source_post_id,source_fingerprint,created_at)
                        SELECT s.streamer_id,ss.source_post_id,ss.source_fingerprint,?
                        FROM schedule_sources ss JOIN schedules s ON s.id = ss.schedule_id
                        WHERE s.id = ? AND ss.source_post_id IS NOT NULL
                        AND ss.source_fingerprint IS NOT NULL",
                    )
                    .bind(&now)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                    self.emit_schedule_change(&mut tx, id, "delete", actor)
                        .await?;
                    sqlx::query("DELETE FROM schedules WHERE id = ?")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn fixture(store: &SqliteStore) -> Schedule {
        let streamer = store
            .add_subscription(&AddSubscription {
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
        // SQLx closes its SQLite worker asynchronously on Windows.
        for attempt in 0..20 {
            match std::fs::remove_file(&path) {
                Ok(()) => break,
                Err(error) if attempt == 19 => panic!("temporary database cleanup: {error}"),
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
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
}
