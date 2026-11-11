//! Persistent analysis-failure backoff, separate from successful analysis results.
//!
//! Callers serialize attempts for the same key, call `failure` only after a real
//! failed analysis, and call `clear` after success. Reading a waiting state never
//! writes or extends its deadline. Manual and automatic refreshes use this same
//! policy; there is no timer or sleeping task here.
use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::store::SqliteStore;

const MAX_MESSAGE_CHARS: usize = 2048;
const MAX_STATE_BYTES: usize = 16 * 1024;
const BEIJING_OFFSET_SECONDS: i32 = 8 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AnalysisRetryState {
    #[serde(deserialize_with = "deserialize_failures")]
    pub(crate) failures: u32,
    pub(crate) next_retry_at: DateTime<Utc>,
    pub(crate) message: String,
}

impl AnalysisRetryState {
    /// Equality is due: a refresh at the deadline may attempt analysis again.
    pub(crate) fn is_due(&self, now: DateTime<Utc>) -> bool {
        now >= self.next_retry_at
    }

    /// For refresh outcomes/UI, not logs: the message can contain upstream text.
    pub(crate) fn retry_message(&self) -> String {
        let offset = FixedOffset::east_opt(BEIJING_OFFSET_SECONDS)
            .expect("Beijing's fixed UTC+08:00 offset is valid");
        format!(
            "{}；连续失败 {} 次，下次可重试：{}（北京时间，UTC+08:00）",
            self.message,
            self.failures,
            self.next_retry_at
                .with_timezone(&offset)
                .format("%Y-%m-%d %H:%M:%S")
        )
    }
}

/// Fingerprints, never raw credentials, should be supplied by the caller.
///
/// `content_key` must cover source identity, publication time, text, the selected
/// first two images and reservation data. `config_key` must cover model, endpoint,
/// prompt/version, thinking options and API-credential fingerprint. A change to
/// either bypasses the old backoff without modifying old successful analyses.
/// Length framing prevents different tuples from sharing the same digest input.
pub(crate) fn retry_state_key(post_id: &str, content_key: &str, config_key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"oshical-analysis-retry-v1");
    for part in [post_id, content_key, config_key] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("analysis_retry:v1:{:x}", digest.finalize())
}

/// Corrupt state is treated as absent so it cannot permanently block a post.
/// Database failures still propagate; warnings deliberately omit key and payload.
pub(crate) async fn load(store: &SqliteStore, key: &str) -> AppResult<Option<AnalysisRetryState>> {
    let Some(raw) = store.get_client_sync_state(key).await? else {
        return Ok(None);
    };
    if raw.len() <= MAX_STATE_BYTES {
        if let Ok(state) = serde_json::from_str::<Option<AnalysisRetryState>>(&raw) {
            match state {
                None => return Ok(None),
                Some(mut state)
                    if state.failures > 0
                        && state
                            .next_retry_at
                            .checked_add_signed(Duration::seconds(i64::from(
                                BEIJING_OFFSET_SECONDS,
                            )))
                            .is_some() =>
                {
                    state.message = bounded_message(&state.message);
                    return Ok(Some(state));
                }
                Some(_) => {}
            }
        }
    }
    tracing::warn!("识别重试状态无效，忽略该状态并允许重新识别");
    Ok(None)
}

/// Record one actual failed attempt, never a refresh skipped during backoff.
/// The caller must redact credentials from `message` before persisting it.
pub(crate) async fn failure(
    store: &SqliteStore,
    key: &str,
    now: DateTime<Utc>,
    message: &str,
) -> AppResult<AnalysisRetryState> {
    let failures = load(store, key)
        .await?
        .map_or(1, |state| state.failures.saturating_add(1));
    let next_retry_at = now
        .checked_add_signed(retry_delay(failures))
        .filter(|at| {
            at.checked_add_signed(Duration::seconds(i64::from(BEIJING_OFFSET_SECONDS)))
                .is_some()
        })
        .ok_or_else(|| AppError::validation("识别重试时间超出支持范围"))?;
    let state = AnalysisRetryState {
        failures,
        next_retry_at,
        message: bounded_message(message),
    };
    store
        .set_client_sync_state(key, &serde_json::to_string(&state)?)
        .await?;
    Ok(state)
}

pub(crate) async fn clear(store: &SqliteStore, key: &str) -> AppResult<()> {
    store.delete_client_sync_state(key).await?;
    Ok(())
}

fn retry_delay(failures: u32) -> Duration {
    Duration::minutes(match failures {
        0 | 1 => 1,
        2 => 5,
        3 => 15,
        _ => 30,
    })
}

fn bounded_message(message: &str) -> String {
    message.chars().take(MAX_MESSAGE_CHARS).collect()
}

fn deserialize_failures<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let failures = u64::deserialize(deserializer)?;
    Ok(failures.min(u64::from(u32::MAX)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-11T01:02:03Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[tokio::test]
    async fn failure_persists_json_and_round_trips_through_store() {
        let store = SqliteStore::in_memory().await.unwrap();
        let key = retry_state_key("post", "content", "config");
        assert_eq!(load(&store, &key).await.unwrap(), None);
        let state = failure(&store, &key, now(), "请求发送失败").await.unwrap();
        assert_eq!(state.failures, 1);
        assert_eq!(state.next_retry_at, now() + Duration::minutes(1));
        let raw = store.get_client_sync_state(&key).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<AnalysisRetryState>(&raw).unwrap(),
            state
        );
        // A separate store handle reloads database state, not an in-memory map.
        assert_eq!(load(&store.clone(), &key).await.unwrap(), Some(state));
    }

    #[test]
    fn early_is_not_due_but_equality_and_later_are_due() {
        let state = AnalysisRetryState {
            failures: 1,
            next_retry_at: now() + Duration::minutes(1),
            message: "timeout".into(),
        };
        assert!(!state.is_due(now()));
        assert!(!state.is_due(state.next_retry_at - Duration::nanoseconds(1)));
        assert!(state.is_due(state.next_retry_at));
        assert!(state.is_due(state.next_retry_at + Duration::nanoseconds(1)));
    }

    #[tokio::test]
    async fn actual_failures_follow_four_step_delay_with_fixed_cap() {
        let store = SqliteStore::in_memory().await.unwrap();
        let mut attempted_at = now();
        for (index, minutes) in [1, 5, 15, 30, 30, 30].into_iter().enumerate() {
            let state = failure(&store, "retry", attempted_at, "timeout")
                .await
                .unwrap();
            assert_eq!(state.failures, index as u32 + 1);
            assert_eq!(
                state.next_retry_at - attempted_at,
                Duration::minutes(minutes)
            );
            attempted_at = state.next_retry_at;
        }
    }

    #[tokio::test]
    async fn skipped_refreshes_neither_increment_nor_extend_the_deadline() {
        let store = SqliteStore::in_memory().await.unwrap();
        let state = failure(&store, "retry", now(), "timeout").await.unwrap();
        let original = store.get_client_sync_state("retry").await.unwrap();
        for seconds in [0, 20, 59] {
            let loaded = load(&store, "retry").await.unwrap().unwrap();
            assert!(!loaded.is_due(now() + Duration::seconds(seconds)));
            assert_eq!(loaded, state);
            assert_eq!(
                store.get_client_sync_state("retry").await.unwrap(),
                original
            );
        }
    }

    #[tokio::test]
    async fn content_config_and_post_changes_have_independent_retry_state() {
        let store = SqliteStore::in_memory().await.unwrap();
        let original = retry_state_key("post", "content", "config");
        failure(&store, &original, now(), "timeout").await.unwrap();
        for changed in [
            retry_state_key("post", "changed-content", "config"),
            retry_state_key("post", "content", "changed-config"),
            retry_state_key("other-post", "content", "config"),
        ] {
            assert_ne!(original, changed);
            assert_eq!(load(&store, &changed).await.unwrap(), None);
            assert_eq!(
                failure(&store, &changed, now(), "timeout")
                    .await
                    .unwrap()
                    .failures,
                1
            );
        }
        assert_eq!(load(&store, &original).await.unwrap().unwrap().failures, 1);
    }

    #[test]
    fn keys_are_deterministic_framed_and_contain_only_a_digest() {
        let key = retry_state_key("private-post", "private-content", "private-config");
        assert_eq!(
            key,
            retry_state_key("private-post", "private-content", "private-config")
        );
        let digest = key.strip_prefix("analysis_retry:v1:").unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(
            retry_state_key("ab", "c", "d"),
            retry_state_key("a", "bc", "d")
        );
        assert_ne!(
            retry_state_key("a", "b", "cd"),
            retry_state_key("a", "bc", "d")
        );
    }

    #[tokio::test]
    async fn success_clear_is_idempotent_and_next_failure_restarts_at_one() {
        let store = SqliteStore::in_memory().await.unwrap();
        failure(&store, "retry", now(), "timeout").await.unwrap();
        clear(&store, "retry").await.unwrap();
        assert_eq!(load(&store, "retry").await.unwrap(), None);
        assert_eq!(store.get_client_sync_state("retry").await.unwrap(), None);
        clear(&store, "retry").await.unwrap();
        let state = failure(&store, "retry", now(), "another timeout")
            .await
            .unwrap();
        assert_eq!(state.failures, 1);
        assert_eq!(state.next_retry_at - now(), Duration::minutes(1));
    }

    #[tokio::test]
    async fn malformed_states_allow_retry_without_overwriting_the_bad_record() {
        let store = SqliteStore::in_memory().await.unwrap();
        for raw in [
            "not json".to_owned(),
            "{}".to_owned(),
            json!({"failures": 1, "next_retry_at": "bad-time", "message": "timeout"}).to_string(),
            json!({"failures": -1, "next_retry_at": now(), "message": "timeout"}).to_string(),
            json!({"failures": 0, "next_retry_at": now(), "message": "timeout"}).to_string(),
            json!({"failures": 1, "next_retry_at": now(), "message": null}).to_string(),
            "x".repeat(MAX_STATE_BYTES + 1),
        ] {
            store.set_client_sync_state("retry", &raw).await.unwrap();
            assert_eq!(load(&store, "retry").await.unwrap(), None);
            assert_eq!(
                store.get_client_sync_state("retry").await.unwrap(),
                Some(raw)
            );
        }
        let state = failure(&store, "retry", now(), "real attempt failed")
            .await
            .unwrap();
        assert_eq!(state.failures, 1);
    }

    #[tokio::test]
    async fn null_is_compatible_with_legacy_cleared_state() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.set_client_sync_state("retry", "null").await.unwrap();
        assert_eq!(load(&store, "retry").await.unwrap(), None);
        assert_eq!(
            failure(&store, "retry", now(), "timeout")
                .await
                .unwrap()
                .failures,
            1
        );
    }

    #[tokio::test]
    async fn huge_failure_count_saturates_without_overflow_or_longer_delay() {
        let store = SqliteStore::in_memory().await.unwrap();
        let raw = json!({
            "failures": u64::MAX,
            "next_retry_at": now(),
            "message": "timeout"
        })
        .to_string();
        store.set_client_sync_state("retry", &raw).await.unwrap();
        assert_eq!(
            load(&store, "retry").await.unwrap().unwrap().failures,
            u32::MAX
        );
        let state = failure(&store, "retry", now(), "again").await.unwrap();
        assert_eq!(state.failures, u32::MAX);
        assert_eq!(state.next_retry_at - now(), Duration::minutes(30));
    }

    #[tokio::test]
    async fn stored_message_is_utf8_safe_and_bounded() {
        let store = SqliteStore::in_memory().await.unwrap();
        let message = "识别失败🦀".repeat(MAX_MESSAGE_CHARS);
        let state = failure(&store, "retry", now(), &message).await.unwrap();
        assert_eq!(state.message.chars().count(), MAX_MESSAGE_CHARS);
        assert_eq!(
            state.message,
            message.chars().take(MAX_MESSAGE_CHARS).collect::<String>()
        );
        assert!(
            store
                .get_client_sync_state("retry")
                .await
                .unwrap()
                .unwrap()
                .len()
                <= MAX_STATE_BYTES
        );
        assert_eq!(load(&store, "retry").await.unwrap(), Some(state));
    }

    #[tokio::test]
    async fn retry_description_includes_original_error_and_explicit_beijing_time() {
        let store = SqliteStore::in_memory().await.unwrap();
        let state = failure(&store, "retry", now(), "请求超时").await.unwrap();
        assert_eq!(
            state.retry_message(),
            "请求超时；连续失败 1 次，下次可重试：2026-11-11 09:03:03（北京时间，UTC+08:00）"
        );
    }

    #[tokio::test]
    async fn datetime_overflow_returns_error_without_writing_state() {
        let store = SqliteStore::in_memory().await.unwrap();
        assert!(
            failure(&store, "retry", DateTime::<Utc>::MAX_UTC, "timeout")
                .await
                .is_err()
        );
        assert_eq!(store.get_client_sync_state("retry").await.unwrap(), None);
        store
            .set_client_sync_state(
                "retry",
                &json!({
                    "failures": 1,
                    "next_retry_at": DateTime::<Utc>::MAX_UTC,
                    "message": "timeout"
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(load(&store, "retry").await.unwrap(), None);
    }

    #[tokio::test]
    async fn database_errors_propagate_instead_of_looking_like_missing_state() {
        let store = SqliteStore::in_memory().await.unwrap();
        store.pool().close().await;
        assert!(matches!(
            load(&store, "retry").await,
            Err(AppError::Database(_))
        ));
        assert!(matches!(
            failure(&store, "retry", now(), "timeout").await,
            Err(AppError::Database(_))
        ));
        assert!(matches!(
            clear(&store, "retry").await,
            Err(AppError::Database(_))
        ));
    }
}
