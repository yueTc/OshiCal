//! In-memory registry for asynchronous server refresh jobs.
//!
//! A job is visible while it is queued or running and for five minutes after it
//! reaches a terminal state. At most 200 jobs are retained. The registry is
//! deliberately process-local: jobs are not restored after a server restart and
//! clients must treat an unknown job id as expired.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use oshical_core::{RefreshProgress, RefreshReport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

const JOB_RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_RETAINED_JOBS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshJobKind {
    RefreshAll,
    RefreshStreamer,
}

impl RefreshJobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RefreshAll => "refresh_all",
            Self::RefreshStreamer => "refresh_streamer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

impl RefreshJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshJobProgress {
    pub stage: String,
    pub completed: usize,
    pub total: usize,
    pub percent: u8,
    pub done: bool,
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshJob {
    pub job_id: String,
    pub kind: RefreshJobKind,
    pub streamer_id: Option<i64>,
    pub streamer_name: Option<String>,
    pub status: RefreshJobStatus,
    pub progress: RefreshJobProgress,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub report: Option<RefreshReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JobRegistration {
    pub job: RefreshJob,
    pub created: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshJobAccepted {
    pub job_id: String,
    pub status: String,
    pub deduplicated: bool,
}

impl From<&JobRegistration> for RefreshJobAccepted {
    fn from(registration: &JobRegistration) -> Self {
        Self {
            job_id: registration.job.job_id.clone(),
            status: registration.job.status.as_str().to_owned(),
            deduplicated: !registration.created,
        }
    }
}

#[derive(Default)]
struct JobStore {
    jobs: Vec<RefreshJob>,
    request_keys: HashMap<String, String>,
}

#[derive(Clone, Default)]
pub struct RefreshJobManager {
    inner: Arc<Mutex<JobStore>>,
}

impl RefreshJobManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_all(&self) -> JobRegistration {
        self.register(RefreshJobKind::RefreshAll, None, None, None)
    }

    pub fn create_all_with_key(&self, key: String) -> JobRegistration {
        self.register(RefreshJobKind::RefreshAll, None, None, Some(key))
    }

    pub fn create_streamer_with_key(
        &self,
        streamer_id: i64,
        streamer_name: impl Into<String>,
        key: String,
    ) -> JobRegistration {
        self.register(
            RefreshJobKind::RefreshStreamer,
            Some(streamer_id),
            Some(streamer_name.into()),
            Some(key),
        )
    }

    pub fn create_streamer(
        &self,
        streamer_id: i64,
        streamer_name: impl Into<String>,
    ) -> JobRegistration {
        self.register(
            RefreshJobKind::RefreshStreamer,
            Some(streamer_id),
            Some(streamer_name.into()),
            None,
        )
    }

    pub fn get(&self, job_id: &str) -> Option<RefreshJob> {
        let now = Utc::now();
        let mut store = self.lock();
        prune_jobs(&mut store, now);
        store.jobs.iter().find(|job| job.job_id == job_id).cloned()
    }

    pub fn begin(&self, job_id: &str) -> Option<RefreshJob> {
        let now = Utc::now();
        let mut store = self.lock();
        prune_jobs(&mut store, now);
        let job = store.jobs.iter_mut().find(|job| job.job_id == job_id)?;
        if job.status == RefreshJobStatus::Queued {
            job.status = RefreshJobStatus::Running;
            job.started_at = Some(now);
        }
        Some(job.clone())
    }

    pub fn update_streamer_progress(
        &self,
        job_id: &str,
        progress: &RefreshProgress,
    ) -> Option<RefreshJob> {
        let now = Utc::now();
        let mut store = self.lock();
        prune_jobs(&mut store, now);
        let job = store.jobs.iter_mut().find(|job| job.job_id == job_id)?;
        if !job.status.is_active() {
            return Some(job.clone());
        }
        if job.status == RefreshJobStatus::Queued {
            job.status = RefreshJobStatus::Running;
            job.started_at = Some(now);
        }
        // Keep immutable request identity: a full-refresh progress target is
        // id=0, and a single caller can follow a full task. Neither should turn
        // this job into a different request or break active-job deduplication.
        if job.kind == RefreshJobKind::RefreshAll {
            job.streamer_name = Some("全部主播".to_owned());
        } else if job.streamer_id == Some(progress.streamer_id) {
            job.streamer_name = Some(progress.streamer_name.clone());
        }
        job.progress = RefreshJobProgress {
            stage: progress.stage.clone(),
            completed: progress.completed,
            total: progress.total,
            percent: progress.percent.min(100),
            done: progress.done,
            failed: progress.failed,
        };
        Some(job.clone())
    }

    pub fn update_progress(
        &self,
        job_id: &str,
        mut progress: RefreshJobProgress,
    ) -> Option<RefreshJob> {
        let now = Utc::now();
        let mut store = self.lock();
        prune_jobs(&mut store, now);
        let job = store.jobs.iter_mut().find(|job| job.job_id == job_id)?;
        if !job.status.is_active() {
            return Some(job.clone());
        }
        if job.status == RefreshJobStatus::Queued {
            job.status = RefreshJobStatus::Running;
            job.started_at = Some(now);
        }
        progress.percent = progress.percent.min(100);
        job.progress = progress;
        Some(job.clone())
    }

    pub fn finish(
        &self,
        job_id: &str,
        report: RefreshReport,
        stage: impl Into<String>,
    ) -> Option<RefreshJob> {
        let now = Utc::now();
        let partial_failure = !report.failed_streamers.is_empty();
        let error = partial_failure
            .then(|| format!("部分来源刷新失败: {}", report.failed_streamers.join(", ")));
        self.finish_with_status(
            job_id,
            report,
            stage.into(),
            if partial_failure {
                RefreshJobStatus::Failed
            } else {
                RefreshJobStatus::Completed
            },
            error,
            now,
        )
    }

    pub fn fail(&self, job_id: &str, error: impl Into<String>) -> Option<RefreshJob> {
        let now = Utc::now();
        let mut store = self.lock();
        prune_jobs(&mut store, now);
        let job = store.jobs.iter_mut().find(|job| job.job_id == job_id)?;
        let error = error.into();
        job.status = RefreshJobStatus::Failed;
        job.started_at.get_or_insert(now);
        job.finished_at = Some(now);
        job.progress = RefreshJobProgress {
            stage: format!("刷新失败: {error}"),
            completed: 1,
            total: 1,
            percent: 100,
            done: true,
            failed: true,
        };
        job.error = Some(error);
        Some(job.clone())
    }

    fn finish_with_status(
        &self,
        job_id: &str,
        report: RefreshReport,
        stage: String,
        status: RefreshJobStatus,
        error: Option<String>,
        now: DateTime<Utc>,
    ) -> Option<RefreshJob> {
        let mut store = self.lock();
        prune_jobs(&mut store, now);
        let job = store.jobs.iter_mut().find(|job| job.job_id == job_id)?;
        let partial_failure = !report.failed_streamers.is_empty();
        job.status = status;
        job.started_at.get_or_insert(now);
        job.finished_at = Some(now);
        job.progress = RefreshJobProgress {
            stage,
            completed: 1,
            total: 1,
            percent: 100,
            done: true,
            failed: job.status == RefreshJobStatus::Failed || partial_failure,
        };
        job.report = Some(report);
        job.error = error;
        Some(job.clone())
    }

    fn register(
        &self,
        kind: RefreshJobKind,
        streamer_id: Option<i64>,
        streamer_name: Option<String>,
        request_key: Option<String>,
    ) -> JobRegistration {
        let now = Utc::now();
        let mut store = self.lock();
        prune_jobs(&mut store, now);

        if let Some(existing) = store
            .jobs
            .iter()
            .find(|job| {
                job.status.is_active()
                    && job.kind == kind
                    && job.streamer_id == streamer_id
                    && store.request_keys.get(&job.job_id) == request_key.as_ref()
            })
            .cloned()
        {
            return JobRegistration {
                job: existing,
                created: false,
            };
        }

        let job = RefreshJob {
            job_id: Uuid::new_v4().to_string(),
            kind,
            streamer_id,
            streamer_name,
            status: RefreshJobStatus::Queued,
            progress: RefreshJobProgress {
                stage: match kind {
                    RefreshJobKind::RefreshAll => "等待全量刷新".to_owned(),
                    RefreshJobKind::RefreshStreamer => "等待主播刷新".to_owned(),
                },
                ..RefreshJobProgress::default()
            },
            created_at: now,
            started_at: None,
            finished_at: None,
            report: None,
            error: None,
        };
        if let Some(key) = request_key {
            store.request_keys.insert(job.job_id.clone(), key);
        }
        store.jobs.push(job.clone());
        prune_jobs(&mut store, now);
        JobRegistration { job, created: true }
    }

    fn lock(&self) -> MutexGuard<'_, JobStore> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn job_progress_message(job: &RefreshJob) -> Value {
    let report_failed = job
        .report
        .as_ref()
        .is_some_and(|report| !report.failed_streamers.is_empty());
    json!({
        "type": "job_progress",
        "jobId": job.job_id,
        "kind": job.kind,
        "streamerId": job.streamer_id,
        "streamerName": job.streamer_name,
        "status": job.status,
        "stage": job.progress.stage,
        "completed": job.progress.completed,
        "total": job.progress.total,
        "percent": job.progress.percent,
        "done": job.progress.done || job.status.is_terminal(),
        "failed": job.progress.failed || report_failed || job.status == RefreshJobStatus::Failed,
        "createdAt": job.created_at,
        "startedAt": job.started_at,
        "finishedAt": job.finished_at,
        "report": job.report,
        "error": job.error,
    })
}

fn prune_jobs(store: &mut JobStore, now: DateTime<Utc>) {
    let cutoff = now
        - ChronoDuration::from_std(JOB_RETENTION).unwrap_or_else(|_| ChronoDuration::minutes(5));
    store.jobs.retain(|job| {
        job.status.is_active()
            || job
                .finished_at
                .is_none_or(|finished_at| finished_at >= cutoff)
    });
    while store.jobs.len() > MAX_RETAINED_JOBS {
        let Some(index) = store.jobs.iter().position(|job| job.status.is_terminal()) else {
            break;
        };
        store.jobs.remove(index);
    }
    let remaining: std::collections::HashSet<_> =
        store.jobs.iter().map(|job| job.job_id.clone()).collect();
    store.request_keys.retain(|id, _| remaining.contains(id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(failed_streamers: &[&str]) -> RefreshReport {
        RefreshReport {
            refreshed_streamers: 1,
            failed_streamers: failed_streamers
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            new_schedules: 2,
            cancelled_schedules: 1,
            llm_calls: 0,
            llm_cache_hits: 0,
        }
    }

    #[test]
    fn streamer_refresh_is_deduplicated_while_active() {
        let manager = RefreshJobManager::new();
        let first = manager.create_streamer(7, "主播七");
        let duplicate = manager.create_streamer(7, "主播七");
        let other = manager.create_streamer(8, "主播八");

        assert!(first.created);
        assert!(!duplicate.created);
        assert_eq!(first.job.job_id, duplicate.job.job_id);
        assert!(other.created);
        assert_ne!(first.job.job_id, other.job.job_id);

        manager.finish(&first.job.job_id, report(&[]), "刷新完成");
        let after_finish = manager.create_streamer(7, "主播七");
        assert!(after_finish.created);
        assert_ne!(first.job.job_id, after_finish.job.job_id);
    }

    #[test]
    fn shared_progress_does_not_change_the_requested_job_identity() {
        let manager = RefreshJobManager::new();
        let all = manager.create_all_with_key("all".to_owned());
        let single = manager.create_streamer_with_key(7, "主播七", "one".to_owned());
        let progress = RefreshProgress {
            streamer_id: 0,
            streamer_name: "全部主播".to_owned(),
            stage: "已加入全量刷新".to_owned(),
            completed: 1,
            total: 3,
            percent: 33,
            done: false,
            failed: false,
        };
        manager
            .update_streamer_progress(&all.job.job_id, &progress)
            .unwrap();
        manager
            .update_streamer_progress(&single.job.job_id, &progress)
            .unwrap();
        assert!(!manager.create_all_with_key("all".to_owned()).created);
        assert!(
            !manager
                .create_streamer_with_key(7, "主播七", "one".to_owned())
                .created
        );
        assert_eq!(
            manager.get(&single.job.job_id).unwrap().streamer_id,
            Some(7)
        );
        assert_eq!(manager.get(&all.job.job_id).unwrap().streamer_id, None);
    }

    #[test]
    fn changed_request_snapshot_is_not_deduplicated_into_an_old_job() {
        let manager = RefreshJobManager::new();
        let first = manager.create_all_with_key("config-a".to_owned());
        assert!(!manager.create_all_with_key("config-a".to_owned()).created);
        let changed = manager.create_all_with_key("config-b".to_owned());
        assert!(changed.created);
        assert_ne!(first.job.job_id, changed.job.job_id);
        let one = manager.create_streamer_with_key(1, "甲", "target-a".to_owned());
        assert!(
            !manager
                .create_streamer_with_key(1, "甲", "target-a".to_owned())
                .created
        );
        assert!(
            manager
                .create_streamer_with_key(1, "甲", "target-b".to_owned())
                .created
        );
        manager.finish(&one.job.job_id, report(&[]), "done");
        assert!(
            manager
                .create_streamer_with_key(1, "甲", "target-a".to_owned())
                .created
        );
    }

    #[test]
    fn full_refresh_is_deduplicated_while_active() {
        let manager = RefreshJobManager::new();
        let first = manager.create_all();
        let duplicate = manager.create_all();
        assert!(first.created);
        assert!(!duplicate.created);
        assert_eq!(first.job.job_id, duplicate.job.job_id);

        manager.fail(&first.job.job_id, "network");
        let after_finish = manager.create_all();
        assert!(after_finish.created);
        assert_ne!(first.job.job_id, after_finish.job.job_id);
    }

    #[test]
    fn completed_job_is_queryable_with_progress_and_report() {
        let manager = RefreshJobManager::new();
        let registration = manager.create_streamer(11, "主播十一");
        let job_id = registration.job.job_id;

        let running = manager.begin(&job_id).expect("running job");
        assert_eq!(running.status, RefreshJobStatus::Running);
        assert!(running.started_at.is_some());

        let progressed = manager
            .update_progress(
                &job_id,
                RefreshJobProgress {
                    stage: "解析动态".to_owned(),
                    completed: 2,
                    total: 4,
                    percent: 50,
                    done: false,
                    failed: false,
                },
            )
            .expect("progress");
        assert_eq!(progressed.progress.completed, 2);
        assert_eq!(progressed.progress.percent, 50);

        let finished = manager
            .finish(&job_id, report(&[]), "主播刷新完成")
            .expect("finished job");
        assert_eq!(finished.status, RefreshJobStatus::Completed);
        assert_eq!(finished.progress.percent, 100);
        assert!(finished.progress.done);
        assert!(finished.finished_at.is_some());
        assert_eq!(finished.report.expect("report").new_schedules, 2);

        let queried = manager.get(&job_id).expect("queryable job");
        assert_eq!(queried.status, RefreshJobStatus::Completed);
    }

    #[test]
    fn failed_and_partial_jobs_record_failure_state() {
        let manager = RefreshJobManager::new();
        let failed = manager.create_all();
        manager
            .fail(&failed.job.job_id, "boom")
            .expect("failed job");
        let failed = manager.get(&failed.job.job_id).expect("failed query");
        assert_eq!(failed.status, RefreshJobStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert!(failed.progress.done);
        assert!(failed.progress.failed);

        let partial = manager.create_streamer(13, "主播十三");
        manager.begin(&partial.job.job_id);
        let partial = manager
            .finish(&partial.job.job_id, report(&["来源 A"]), "部分失败")
            .expect("partial job");
        assert_eq!(partial.status, RefreshJobStatus::Failed);
        assert!(partial.error.is_some());
        assert!(partial.progress.failed);
    }

    #[test]
    fn terminal_job_retention_is_capped_at_200() {
        let manager = RefreshJobManager::new();
        let first = manager.create_streamer(0, "主播零");
        manager.finish(&first.job.job_id, report(&[]), "刷新完成");
        for streamer_id in 1..=200 {
            let registration = manager.create_streamer(streamer_id, "主播");
            manager.finish(&registration.job.job_id, report(&[]), "刷新完成");
        }

        assert!(manager.get(&first.job.job_id).is_none());
    }
}
