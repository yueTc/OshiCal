use axum::extract::{Path, Query, State};
use axum::Json;
use oshical_core::{AppError, SyncChange};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{ok, ApiResult, AppState};

const DEFAULT_CHANGES_LIMIT: i64 = 200;
const MAX_CHANGES_LIMIT: i64 = 500;
const WS_BATCH_LIMIT: i64 = 500;
const BATCH_MAX_CHANGES: usize = 100;
const BATCH_MAX_BYTES: usize = 512 * 1024;

// WP1 storage contract used by this module:
// - SyncBootstrap store.sync_bootstrap()
// - SyncChangesPage store.sync_changes_after(after, limit)
// - i64 store.latest_sync_seq()
// - i64 store.min_retained_sync_seq()
// - DeviceInfo store.register_device(device_id, name, platform, app_version)
// - Vec<DeviceInfo> store.list_devices()
// - DeviceInfo store.ack_device(device_id, seq)
// - () store.delete_device_log(device_id)
//
// Keeping every Core call in this module makes the server protocol code
// independent from the physical layout of the sync-feed tables.

#[derive(Debug, Deserialize)]
pub(crate) struct ChangesQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

pub(crate) async fn bootstrap(State(state): State<AppState>) -> ApiResult {
    ok(state.service.store().sync_bootstrap().await?)
}

pub(crate) async fn changes(
    State(state): State<AppState>,
    Query(query): Query<ChangesQuery>,
) -> ApiResult {
    let after = query.after.unwrap_or(0).max(0);
    let limit = query
        .limit
        .unwrap_or(DEFAULT_CHANGES_LIMIT)
        .clamp(1, MAX_CHANGES_LIMIT);
    ok(state
        .service
        .store()
        .sync_changes_after(after, limit)
        .await?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AckInput {
    device_id: String,
    seq: i64,
}

pub(crate) async fn ack(State(state): State<AppState>, Json(input): Json<AckInput>) -> ApiResult {
    validate_device_id(&input.device_id)?;
    if input.seq < 0 {
        return Err(AppError::validation("同步序号不能为负数").into());
    }
    ok(state
        .service
        .store()
        .ack_device(input.device_id.trim(), input.seq)
        .await?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceRegisterInput {
    device_id: String,
    name: String,
    platform: String,
    app_version: String,
}

pub(crate) async fn register_device(
    State(state): State<AppState>,
    Json(input): Json<DeviceRegisterInput>,
) -> ApiResult {
    validate_device_id(&input.device_id)?;
    validate_text("设备名称", &input.name, 128)?;
    validate_text("设备平台", &input.platform, 64)?;
    validate_text("应用版本", &input.app_version, 64)?;
    ok(state
        .service
        .store()
        .register_device(
            input.device_id.trim(),
            input.name.trim(),
            input.platform.trim(),
            input.app_version.trim(),
        )
        .await?)
}

pub(crate) async fn list_devices(State(state): State<AppState>) -> ApiResult {
    ok(state.service.store().list_devices().await?)
}

pub(crate) async fn delete_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> ApiResult {
    validate_device_id(&device_id)?;
    state
        .service
        .store()
        .delete_device_log(device_id.trim())
        .await?;
    ok(json!({"deleted": device_id.trim(), "logOnly": true}))
}

pub(crate) fn validate_device_id(device_id: &str) -> Result<(), AppError> {
    validate_text("deviceId", device_id, 128)
}

fn validate_text(label: &str, value: &str, max_len: usize) -> Result<(), AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::validation(format!("{label} 不能为空")));
    }
    if value.chars().count() > max_len {
        return Err(AppError::validation(format!(
            "{label} 不能超过 {max_len} 个字符"
        )));
    }
    Ok(())
}

/// Read every pending change and publish it through the existing broadcast hub.
///
/// `after` is the last sequence successfully read by the publisher. The runtime
/// pump and other direct publishers can share this function without sharing a connection.
pub(crate) async fn publish_sync_feed(state: &AppState, after: &mut i64) -> Result<(), AppError> {
    loop {
        let page = state
            .service
            .store()
            .sync_changes_after(*after, WS_BATCH_LIMIT)
            .await?;
        if page.reset_required {
            let _ = state.hub.send(reset_required_message(
                page.latest_seq,
                page.min_retained_seq,
                "cursor_expired",
            ));
            *after = page.latest_seq;
            return Ok(());
        }
        if page.changes.is_empty() {
            *after = (*after).max(page.latest_seq);
            return Ok(());
        }

        let previous = *after;
        for message in sync_batch_messages(&page.changes, page.latest_seq)? {
            let _ = state.hub.send(message);
        }
        *after = page
            .changes
            .last()
            .map(|change| change.seq)
            .unwrap_or(*after);
        if *after <= previous || !page.has_more || *after >= page.latest_seq {
            return Ok(());
        }
    }
}

pub(crate) fn sync_batch_messages(
    changes: &[SyncChange],
    latest_seq: i64,
) -> Result<Vec<Value>, AppError> {
    if changes.is_empty() {
        return Ok(Vec::new());
    }

    let mut messages = Vec::new();
    let mut batch = Vec::<Value>::new();
    let mut batch_bytes = 0_usize;

    for change in changes {
        let value = serde_json::to_value(change)?;
        let value_bytes = serde_json::to_vec(&value)?.len();
        let would_exceed_count = batch.len() >= BATCH_MAX_CHANGES;
        let would_exceed_bytes =
            !batch.is_empty() && batch_bytes.saturating_add(value_bytes) > BATCH_MAX_BYTES;
        if would_exceed_count || would_exceed_bytes {
            messages.push(sync_batch_message(&batch, latest_seq));
            batch.clear();
            batch_bytes = 0;
        }
        batch_bytes = batch_bytes.saturating_add(value_bytes);
        batch.push(value);
    }

    if !batch.is_empty() {
        messages.push(sync_batch_message(&batch, latest_seq));
    }
    Ok(messages)
}

fn sync_batch_message(changes: &[Value], latest_seq: i64) -> Value {
    let from_seq = changes
        .first()
        .and_then(|change| change.get("seq"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let to_seq = changes
        .last()
        .and_then(|change| change.get("seq"))
        .and_then(Value::as_i64)
        .unwrap_or(from_seq);
    json!({
        "type": "sync_batch",
        "fromSeq": from_seq,
        "toSeq": to_seq,
        "latestSeq": latest_seq,
        "changes": changes,
    })
}

pub(crate) fn reset_required_message(
    latest_seq: i64,
    min_retained_seq: i64,
    reason: &str,
) -> Value {
    json!({
        "type": "reset_required",
        "reason": reason,
        "latestSeq": latest_seq,
        "minRetainedSeq": min_retained_seq,
    })
}
