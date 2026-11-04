use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AccountSearchResult {
    pub mid: String,
    pub name: String,
    pub avatar_url: String,
    #[serde(default)]
    pub sign: String,
    pub room_id: Option<String>,
    #[serde(default)]
    pub fans: u64,
    #[serde(default)]
    pub is_live: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LinkedAccount {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub streamer_id: i64,
    pub mid: String,
    pub name: String,
    pub avatar_url: String,
    #[serde(default)]
    pub revision: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Streamer {
    pub id: i64,
    pub mid: String,
    pub name: String,
    pub avatar_url: String,
    pub room_id: Option<String>,
    pub live_url: Option<String>,
    pub aliases: Vec<String>,
    pub schedule_enabled: bool,
    pub live_notify_enabled: bool,
    #[serde(default)]
    pub live_stats_enabled: bool,
    #[serde(default)]
    pub fans: Option<u64>,
    pub last_error: Option<String>,
    pub last_refresh_at: Option<DateTime<Utc>>,
    pub is_live: bool,
    pub live_title: Option<String>,
    pub live_cover: Option<String>,
    pub live_started_at: Option<DateTime<Utc>>,
    pub live_checked_at: Option<DateTime<Utc>>,
    pub linked_accounts: Vec<LinkedAccount>,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default)]
    pub revision: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FertilityStatus {
    pub streamer_id: i64,
    pub mid: String,
    pub streamer_name: String,
    pub streamer_avatar: String,
    pub status: String,
    pub day_in_cycle: Option<i64>,
    pub ovulation_day: Option<i64>,
    pub next_period: Option<String>,
    pub cycle_length: Option<i64>,
    pub period_length: Option<i64>,
    pub data_points: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FertilitySnapshot {
    pub fetched_at: DateTime<Utc>,
    pub source: String,
    pub stale: bool,
    pub warning: Option<String>,
    pub total_available: usize,
    pub matched: usize,
    pub statuses: Vec<FertilityStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSubscription {
    pub schedule_enabled: Option<bool>,
    pub live_notify_enabled: Option<bool>,
    pub live_stats_enabled: Option<bool>,
    pub aliases: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddSubscription {
    pub mid: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub room_id: Option<String>,
    pub aliases: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub schedule_enabled: bool,
    #[serde(default = "default_true")]
    pub live_notify_enabled: bool,
    #[serde(default)]
    pub live_stats_enabled: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicPost {
    pub id: String,
    pub author_mid: String,
    pub author_name: String,
    pub published_at: DateTime<Utc>,
    pub text: String,
    pub images: Vec<String>,
    pub source_url: String,
    pub raw_kind: String,
    pub reserve: Option<ReserveCard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReserveCard {
    pub title: String,
    pub description: String,
    pub room_id: Option<String>,
    pub jump_url: Option<String>,
    /// B 站预约卡片类型：1 为视频预约，2 为直播预约。
    #[serde(default)]
    pub stype: Option<i64>,
}

impl ReserveCard {
    pub fn is_live(&self) -> bool {
        self.stype == Some(2)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: i64,
    pub streamer_id: i64,
    pub streamer_name: String,
    pub streamer_avatar: String,
    #[serde(default)]
    pub participants: Vec<String>,
    pub title: String,
    pub start_at: DateTime<Utc>,
    pub end_at: Option<DateTime<Utc>>,
    pub room_url: Option<String>,
    pub room_owner_mid: Option<String>,
    pub room_owner_name: Option<String>,
    pub source_kind: String,
    pub source_post_id: Option<String>,
    pub source_image_url: Option<String>,
    pub source_author_mid: Option<String>,
    pub source_fingerprint: Option<String>,
    pub source_identity: Option<String>,
    pub source_published_at: Option<DateTime<Utc>>,
    pub weekly_rule_id: Option<i64>,
    pub weekly_occurrence_date: Option<String>,
    pub confidence: f64,
    pub manual_locked: bool,
    pub source_changed: bool,
    pub cancelled: bool,
    #[serde(default)]
    pub revision: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyScheduleRule {
    pub id: i64,
    pub rule_key: String,
    pub streamer_id: i64,
    pub title: String,
    pub weekdays_mask: u8,
    pub start_time: String,
    pub end_time: Option<String>,
    pub timezone: String,
    pub room_url: Option<String>,
    pub effective_from: String,
    pub effective_until: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub revision: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewWeeklyScheduleRule {
    #[serde(default)]
    pub rule_key: Option<String>,
    pub streamer_id: i64,
    pub title: String,
    pub weekdays_mask: u8,
    pub start_time: String,
    pub end_time: Option<String>,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub room_url: Option<String>,
    pub effective_from: String,
    pub effective_until: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWeeklyScheduleRule {
    pub title: Option<String>,
    pub weekdays_mask: Option<u8>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    #[serde(default)]
    pub clear_end_time: bool,
    pub room_url: Option<String>,
    #[serde(default)]
    pub clear_room_url: bool,
    pub effective_from: Option<String>,
    pub effective_until: Option<String>,
    #[serde(default)]
    pub clear_effective_until: bool,
    pub enabled: Option<bool>,
}

fn default_timezone() -> String {
    "Asia/Shanghai".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSchedule {
    pub streamer_id: i64,
    #[serde(default)]
    pub participants: Vec<String>,
    pub title: String,
    pub start_at: DateTime<Utc>,
    pub end_at: Option<DateTime<Utc>>,
    pub room_url: Option<String>,
    pub room_owner_mid: Option<String>,
    pub room_owner_name: Option<String>,
    pub source_kind: String,
    pub source_post_id: Option<String>,
    pub source_image_url: Option<String>,
    pub source_author_mid: Option<String>,
    pub source_fingerprint: Option<String>,
    pub source_identity: Option<String>,
    pub source_published_at: Option<DateTime<Utc>>,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
}

const fn default_confidence() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSchedule {
    pub title: Option<String>,
    pub start_at: Option<DateTime<Utc>>,
    pub end_at: Option<DateTime<Utc>>,
    pub room_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LiveRoomOwner {
    pub mid: String,
    pub name: String,
    pub room_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveSnapshot {
    pub is_live: bool,
    pub title: String,
    pub cover_url: Option<String>,
    pub live_url: Option<String>,
    pub online: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSuppression {
    pub id: i64,
    pub streamer_id: i64,
    pub source_post_id: String,
    pub source_fingerprint: String,
    #[serde(default)]
    pub revision: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyScheduleException {
    pub id: i64,
    pub rule_id: i64,
    pub occurrence_date: String,
    pub exception_kind: String,
    pub schedule_id: Option<i64>,
    #[serde(default)]
    pub revision: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSource {
    pub id: i64,
    pub schedule_id: i64,
    pub source_kind: String,
    pub source_post_id: Option<String>,
    pub source_image_url: Option<String>,
    pub source_fingerprint: Option<String>,
    pub source_identity: String,
    pub confidence: f64,
    #[serde(default)]
    pub revision: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncMutation {
    pub mutation_id: String,
    pub operation: String,
    pub entity: String,
    pub payload: serde_json::Value,
    pub base_updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub base_revision: Option<i64>,
    pub status: String,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub applied_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub result_json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveSession {
    pub id: i64,
    pub streamer_id: i64,
    pub streamer_name: String,
    pub room_id: String,
    pub title: String,
    pub cover_url: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_seconds: i64,
    pub peak_online: u64,
    #[serde(default)]
    pub peak_high_energy: u64,
    pub avg_online: u64,
    pub online_samples: i64,
    pub online_total: i64,
    pub revenue_cents: i64,
    pub gift_revenue_cents: i64,
    pub super_chat_revenue_cents: i64,
    pub guard_revenue_cents: i64,
    pub paid_user_count: i64,
    pub interactive_user_count: i64,
    pub danmaku_count: i64,
    pub gift_count: i64,
    pub super_chat_count: i64,
    pub guard_count: i64,
    pub reconnect_count: i64,
    pub data_quality: String,
    pub collector_state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub revision: i64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LiveSessionStats {
    pub peak_high_energy: i64,
    pub revenue_cents: i64,
    pub gift_revenue_cents: i64,
    pub super_chat_revenue_cents: i64,
    pub guard_revenue_cents: i64,
    pub paid_user_count: i64,
    pub interactive_user_count: i64,
    pub danmaku_count: i64,
    pub gift_count: i64,
    pub super_chat_count: i64,
    pub guard_count: i64,
    pub reconnect_count: i64,
    pub data_quality: String,
    pub collector_state: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FanSnapshot {
    pub captured_at: DateTime<Utc>,
    pub fans: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FanHistory {
    pub streamer_id: i64,
    pub current_fans: Option<u64>,
    pub checked_at: Option<DateTime<Utc>>,
    pub snapshots: Vec<FanSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FanRefreshReport {
    pub refreshed_streamers: usize,
    pub failed_streamers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LiveStatus {
    pub streamer_id: i64,
    pub mid: String,
    pub streamer_name: String,
    pub avatar_url: String,
    pub is_live: bool,
    pub title: String,
    pub cover_url: Option<String>,
    pub live_url: Option<String>,
    pub online: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub checked_at: DateTime<Utc>,
    pub live_notify_enabled: bool,
    #[serde(default)]
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    pub live_poll_seconds: u64,
    pub dynamic_poll_seconds: u64,
    pub auto_analyze_images: bool,
    pub live_stats_enabled: bool,
    pub llm_base_url: String,
    pub llm_model: String,
    pub llm_prompt_version: String,
    pub proxy_port: Option<u16>,
    #[serde(default)]
    pub revision: i64,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            live_poll_seconds: 20,
            dynamic_poll_seconds: 300,
            auto_analyze_images: true,
            live_stats_enabled: false,
            llm_base_url: "https://api.openai.com/v1".to_owned(),
            llm_model: String::new(),
            llm_prompt_version: "schedule-v1".to_owned(),
            proxy_port: None,
            revision: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncChange {
    pub seq: i64,
    pub entity_type: String,
    pub entity_id: String,
    pub operation: String,
    pub entity_revision: i64,
    pub payload: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncChangesPage {
    pub schema_version: u32,
    pub from_seq: i64,
    pub to_seq: i64,
    pub latest_seq: i64,
    pub min_retained_seq: i64,
    pub has_more: bool,
    pub reset_required: bool,
    pub changes: Vec<SyncChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncBootstrap {
    pub schema_version: u32,
    pub snapshot_revision: i64,
    pub min_retained_seq: i64,
    pub server_time: DateTime<Utc>,
    pub subscriptions: Vec<Streamer>,
    pub schedules: Vec<Schedule>,
    pub weekly_rules: Vec<WeeklyScheduleRule>,
    pub weekly_exceptions: Vec<WeeklyScheduleException>,
    pub schedule_sources: Vec<ScheduleSource>,
    pub source_suppressions: Vec<SourceSuppression>,
    pub server_settings: ServerSettings,
    pub live_statuses: Vec<LiveStatus>,
    pub live_sessions: Vec<LiveSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub app_version: String,
    pub last_seen_at: DateTime<Utc>,
    pub last_ack_seq: i64,
    pub online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEvent {
    pub seq: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthState {
    pub database_ok: bool,
    pub subscription_count: i64,
    pub schedule_count: i64,
    pub last_live_check_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshReport {
    pub refreshed_streamers: usize,
    pub failed_streamers: Vec<String>,
    pub new_schedules: usize,
    pub cancelled_schedules: usize,
    pub llm_calls: usize,
    pub llm_cache_hits: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RefreshProgress {
    pub streamer_id: i64,
    pub streamer_name: String,
    pub stage: String,
    pub completed: usize,
    pub total: usize,
    pub percent: u8,
    pub done: bool,
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSyncState {
    pub key: String,
    pub value: String,
    pub updated_at: DateTime<Utc>,
}
