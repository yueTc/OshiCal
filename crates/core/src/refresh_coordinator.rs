//! Process-local refresh sharing. The operation still owns the service's refresh lock;
//! registration includes queued operations, not only operations which acquired that lock.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
};

use tokio::{sync::watch, task::JoinError};

use crate::{
    error::{AppError, AppResult},
    models::{RefreshProgress, RefreshReport},
};

pub(crate) type ProgressSink = Arc<dyn Fn(RefreshProgress) + Send + Sync>;

#[derive(Clone, Debug)]
pub(crate) struct RefreshScope {
    pub config_key: String,
    pub targets: BTreeMap<i64, String>,
    pub all: bool,
}

impl RefreshScope {
    pub(crate) fn covers(&self, requested: &Self) -> bool {
        if self.config_key != requested.config_key {
            return false;
        }
        // Two all-refresh snapshots must match exactly, including removals. A
        // previously registered all-refresh must not conceal subscription changes.
        if requested.all {
            return self.all && self.targets == requested.targets;
        }
        requested
            .targets
            .iter()
            .all(|(id, fingerprint)| self.targets.get(id) == Some(fingerprint))
    }
}

#[derive(Clone, Default)]
pub(crate) struct RefreshCoordinator {
    registry: Arc<Mutex<Vec<Arc<RunningRefresh>>>>,
}

struct RunningRefresh {
    scope: RefreshScope,
    updates: watch::Sender<RefreshSnapshot>,
}

#[derive(Clone, Default)]
struct RefreshSnapshot {
    progress: Option<RefreshProgress>,
    terminal: Option<Result<RefreshReport, SharedFailure>>,
}

#[derive(Clone)]
struct SharedFailure {
    code: &'static str,
    message: String,
}

impl SharedFailure {
    fn from_error(error: AppError) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }

    fn from_join_error(error: JoinError) -> Self {
        let is_panic = error.is_panic();
        let is_cancelled = error.is_cancelled();
        // JoinError's Display includes the panic payload, which can contain
        // credentials or user content. Neither log nor replay that payload.
        tracing::error!(is_panic, is_cancelled, "刷新后台任务异常退出");
        let (code, message) = if is_panic {
            ("REFRESH_TASK_PANICKED", "刷新后台任务发生内部异常，请重试")
        } else if is_cancelled {
            ("REFRESH_TASK_CANCELLED", "刷新后台任务意外取消，请重试")
        } else {
            ("REFRESH_TASK_FAILED", "刷新后台任务异常退出，请重试")
        };
        Self {
            code,
            message: message.into(),
        }
    }

    fn into_error(self) -> AppError {
        AppError::Shared {
            code: self.code,
            message: self.message,
        }
    }
}

impl RefreshCoordinator {
    /// Shares an unfinished covering operation, including its latest progress.
    ///
    /// An all-refresh shared with a single-streamer caller returns the *whole*
    /// report, not single-streamer counts. Joined progress explicitly says so.
    /// The caller owns only its subscription: dropping it (or panicking in its
    /// callback) cannot cancel the detached worker or other subscribers.
    pub(crate) async fn run<F, Fut>(
        &self,
        scope: RefreshScope,
        on_progress: &(dyn Fn(RefreshProgress) + Send + Sync),
        operation: F,
    ) -> AppResult<RefreshReport>
    where
        F: FnOnce(ProgressSink) -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<RefreshReport>> + Send + 'static,
    {
        let mut operation = Some(operation);
        let (mut updates, joined_all) = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(running) = registry.iter().find(|task| task.scope.covers(&scope)) {
                (running.updates.subscribe(), Some(running.scope.all))
            } else {
                let initial = RefreshSnapshot {
                    progress: Some(RefreshProgress {
                        streamer_id: 0,
                        streamer_name: String::new(),
                        stage: "刷新任务已登记，等待开始".into(),
                        completed: 0,
                        total: scope.targets.len(),
                        percent: 0,
                        done: false,
                        failed: false,
                    }),
                    terminal: None,
                };
                let (sender, receiver) = watch::channel(initial);
                let running = Arc::new(RunningRefresh {
                    scope,
                    updates: sender,
                });
                registry.push(Arc::clone(&running));

                let progress_task = Arc::clone(&running);
                let progress_sink: ProgressSink = Arc::new(move |progress| {
                    // A late callback retained by an operation must never undo
                    // its terminal state. No registry lock is needed to publish.
                    progress_task.updates.send_if_modified(|snapshot| {
                        if snapshot.terminal.is_some() {
                            return false;
                        }
                        snapshot.progress = Some(progress.clone());
                        true
                    });
                });
                let operation = operation.take().expect("new refresh owns its operation");
                let worker = tokio::spawn(async move { operation(progress_sink).await });
                let coordinator = self.clone();
                // There is no await between registration and spawning the
                // supervisor. Cancellation of run() cannot orphan a registry row.
                tokio::spawn(async move {
                    let terminal = match worker.await {
                        Ok(result) => result.map_err(SharedFailure::from_error),
                        Err(error) => Err(SharedFailure::from_join_error(error)),
                    };
                    coordinator.finish(&running, terminal);
                });
                (receiver, None)
            }
        };
        // Discard an unused joiner's closure outside the registry lock; its Drop
        // implementation is caller code just like the progress callback.
        drop(operation);

        loop {
            // Never keep a watch borrow across callbacks/awaits: either could
            // reenter the coordinator while a publisher needs the channel lock.
            let snapshot = updates.borrow_and_update().clone();
            if let Some(mut progress) = snapshot.progress {
                if let Some(all) = joined_all {
                    let prefix = if all {
                        "已加入正在进行的全量刷新（共享全量结果）"
                    } else {
                        "已加入正在进行的刷新"
                    };
                    progress.stage = format!("{prefix}：{}", progress.stage);
                }
                on_progress(progress);
            }
            if let Some(result) = snapshot.terminal {
                return result.map_err(SharedFailure::into_error);
            }
            if updates.changed().await.is_err() {
                return Err(AppError::Shared {
                    code: "REFRESH_TASK_LOST",
                    message: "刷新任务进度通道意外关闭，未取得刷新结果".into(),
                });
            }
        }
    }

    fn finish(
        &self,
        running: &Arc<RunningRefresh>,
        terminal: Result<RefreshReport, SharedFailure>,
    ) {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        // Publish and unregister under the same registry lock. A new run either
        // subscribes before completion or starts fresh, never joins finished data.
        running.updates.send_modify(|snapshot| {
            if let Some(progress) = snapshot.progress.as_mut() {
                progress.done = true;
                match &terminal {
                    Ok(report) => {
                        progress.percent = 100;
                        progress.completed = progress.total;
                        progress.failed = !report.failed_streamers.is_empty();
                        progress.stage = if progress.failed {
                            "刷新完成（存在失败主播）".into()
                        } else {
                            "刷新完成".into()
                        };
                    }
                    Err(error) => {
                        progress.failed = true;
                        progress.stage = format!("刷新失败：{}", error.message);
                    }
                }
            }
            snapshot.terminal = Some(terminal.clone());
        });
        registry.retain(|task| !Arc::ptr_eq(task, running));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::{
        sync::{mpsc, Barrier, Notify},
        task::JoinHandle,
    };

    use super::*;

    fn scope(all: bool, targets: &[(i64, &str)]) -> RefreshScope {
        RefreshScope {
            config_key: "config-v1".into(),
            targets: targets
                .iter()
                .map(|(id, fingerprint)| (*id, (*fingerprint).into()))
                .collect(),
            all,
        }
    }

    fn progress(stage: &str) -> RefreshProgress {
        RefreshProgress {
            streamer_id: 1,
            streamer_name: "泽音".into(),
            stage: stage.into(),
            completed: 1,
            total: 3,
            percent: 33,
            done: false,
            failed: false,
        }
    }

    fn report() -> RefreshReport {
        RefreshReport {
            refreshed_streamers: 3,
            failed_streamers: vec![],
            new_schedules: 7,
            cancelled_schedules: 2,
            llm_calls: 4,
            llm_cache_hits: 8,
        }
    }

    // Timeouts are only failure bounds; ordering is driven by channels, Notify,
    // and Barrier rather than sleeps or assumptions about scheduler speed.
    async fn bounded<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(10), future)
            .await
            .expect("coordinator test stalled")
    }

    fn request<F, Fut>(
        coordinator: RefreshCoordinator,
        scope: RefreshScope,
        operation: F,
    ) -> (
        JoinHandle<AppResult<RefreshReport>>,
        mpsc::UnboundedReceiver<RefreshProgress>,
    )
    where
        F: FnOnce(ProgressSink) -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<RefreshReport>> + Send + 'static,
    {
        let (sender, receiver) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            coordinator
                .run(
                    scope,
                    &move |progress| {
                        let _ = sender.send(progress);
                    },
                    operation,
                )
                .await
        });
        (handle, receiver)
    }

    async fn observe(
        updates: &mut mpsc::UnboundedReceiver<RefreshProgress>,
        stage: &str,
    ) -> RefreshProgress {
        bounded(async {
            loop {
                let progress = updates.recv().await.expect("progress channel closed");
                if progress.stage.contains(stage) {
                    return progress;
                }
            }
        })
        .await
    }

    fn assert_report(actual: &RefreshReport, expected: &RefreshReport) {
        assert_eq!(actual.refreshed_streamers, expected.refreshed_streamers);
        assert_eq!(actual.failed_streamers, expected.failed_streamers);
        assert_eq!(actual.new_schedules, expected.new_schedules);
        assert_eq!(actual.cancelled_schedules, expected.cancelled_schedules);
        assert_eq!(actual.llm_calls, expected.llm_calls);
        assert_eq!(actual.llm_cache_hits, expected.llm_cache_hits);
    }

    #[test]
    fn scope_coverage_is_directional_and_snapshot_sensitive() {
        let all = scope(true, &[(1, "a"), (2, "b")]);
        let single = scope(false, &[(1, "a")]);
        assert!(all.covers(&all.clone()));
        assert!(all.covers(&single));
        assert!(single.covers(&single.clone()));
        assert!(!single.covers(&all));
        assert!(!single.covers(&scope(false, &[(2, "b")])));
        assert!(!all.covers(&scope(false, &[(1, "changed")])));
        assert!(!all.covers(&scope(false, &[(3, "new")])));
        assert!(!all.covers(&scope(true, &[(1, "a")])));
        assert!(!all.covers(&scope(true, &[(1, "a"), (2, "b"), (3, "new")])));
        assert!(!all.covers(&scope(true, &[(1, "a"), (2, "changed")])));
        let mut changed_config = single.clone();
        changed_config.config_key = "config-v2".into();
        assert!(!all.covers(&changed_config));
        assert!(!single.covers(&changed_config));
        assert!(scope(true, &[]).covers(&scope(true, &[])));
        assert!(!scope(true, &[]).covers(&single));
    }

    #[tokio::test]
    async fn identical_requests_execute_once_and_share_progress_and_report() {
        let coordinator = RefreshCoordinator::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let continue_progress = Arc::new(Notify::new());
        let operation_calls = Arc::clone(&calls);
        let operation_release = Arc::clone(&release);
        let operation_continue = Arc::clone(&continue_progress);
        let requested = scope(true, &[(1, "a"), (2, "b")]);
        let (first, mut first_updates) = request(
            coordinator.clone(),
            requested.clone(),
            move |sink| async move {
                operation_calls.fetch_add(1, Ordering::SeqCst);
                sink(progress("抓取动态"));
                operation_continue.notified().await;
                sink(progress("模型识别"));
                operation_release.notified().await;
                Ok(report())
            },
        );
        observe(&mut first_updates, "抓取动态").await;
        let duplicate_calls = Arc::clone(&calls);
        let (second, mut second_updates) =
            request(coordinator.clone(), requested, move |_| async move {
                duplicate_calls.fetch_add(1, Ordering::SeqCst);
                panic!("joined operation must never run");
            });
        let joined = observe(&mut second_updates, "抓取动态").await;
        assert!(joined.stage.starts_with("已加入"));
        continue_progress.notify_one();
        observe(&mut first_updates, "模型识别").await;
        observe(&mut second_updates, "模型识别").await;
        release.notify_one();
        assert_report(&bounded(first).await.unwrap().unwrap(), &report());
        assert_report(&bounded(second).await.unwrap().unwrap(), &report());
        assert!(observe(&mut first_updates, "刷新完成").await.done);
        assert!(observe(&mut second_updates, "刷新完成").await.done);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(coordinator.registry.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn late_joiner_replays_latest_progress_without_new_worker_event() {
        let coordinator = RefreshCoordinator::default();
        let release = Arc::new(Notify::new());
        let operation_release = Arc::clone(&release);
        let requested = scope(false, &[(1, "a")]);
        let (first, mut first_updates) = request(
            coordinator.clone(),
            requested.clone(),
            move |sink| async move {
                sink(progress("识别第 3 条动态"));
                operation_release.notified().await;
                Ok(report())
            },
        );
        let latest = observe(&mut first_updates, "识别第 3 条动态").await;
        let (second, mut second_updates) =
            request(coordinator, requested, |_| async { panic!("must join") });
        let replayed = observe(&mut second_updates, "识别第 3 条动态").await;
        assert_eq!(replayed.streamer_id, latest.streamer_id);
        assert_eq!(replayed.streamer_name, latest.streamer_name);
        assert_eq!(replayed.completed, latest.completed);
        assert_eq!(replayed.total, latest.total);
        assert_eq!(replayed.percent, latest.percent);
        release.notify_one();
        bounded(first).await.unwrap().unwrap();
        bounded(second).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn single_joining_all_keeps_whole_partial_failure_report() {
        let coordinator = RefreshCoordinator::default();
        let release = Arc::new(Notify::new());
        let operation_release = Arc::clone(&release);
        let mut expected = report();
        expected.failed_streamers = vec!["另一位主播".into()];
        let returned = expected.clone();
        let (first, mut first_updates) = request(
            coordinator.clone(),
            scope(true, &[(1, "a"), (2, "b")]),
            move |sink| async move {
                sink(progress("处理中"));
                operation_release.notified().await;
                Ok(returned)
            },
        );
        observe(&mut first_updates, "处理中").await;
        let (single, mut single_updates) =
            request(coordinator, scope(false, &[(1, "a")]), |_| async {
                panic!("all-refresh covers single")
            });
        let shared = observe(&mut single_updates, "处理中").await;
        assert!(shared.stage.contains("共享全量结果"));
        release.notify_one();
        assert_report(&bounded(first).await.unwrap().unwrap(), &expected);
        assert_report(&bounded(single).await.unwrap().unwrap(), &expected);
        let terminal = observe(&mut single_updates, "刷新完成").await;
        assert!(terminal.done && terminal.failed);
    }

    #[tokio::test]
    async fn incompatible_scopes_register_independent_operations() {
        let original = scope(true, &[(1, "a"), (2, "b")]);
        let mut changed_config = original.clone();
        changed_config.config_key = "config-v2".into();
        let pairs = [
            (scope(false, &[(1, "a")]), original.clone()),
            (original.clone(), changed_config),
            (original.clone(), scope(true, &[(1, "a")])),
            (
                original.clone(),
                scope(true, &[(1, "a"), (2, "b"), (3, "new")]),
            ),
            (original.clone(), scope(false, &[(3, "new")])),
            (original, scope(false, &[(1, "changed")])),
            (scope(false, &[(1, "a")]), scope(false, &[(1, "changed")])),
            (scope(false, &[(1, "a")]), scope(false, &[(2, "b")])),
        ];
        for (first_scope, second_scope) in pairs {
            let coordinator = RefreshCoordinator::default();
            let barrier = Arc::new(Barrier::new(3));
            let first_barrier = Arc::clone(&barrier);
            let (first, mut first_updates) =
                request(coordinator.clone(), first_scope, move |sink| async move {
                    sink(progress("独立任务一"));
                    first_barrier.wait().await;
                    Ok(report())
                });
            observe(&mut first_updates, "独立任务一").await;
            let second_barrier = Arc::clone(&barrier);
            let (second, mut second_updates) =
                request(coordinator.clone(), second_scope, move |sink| async move {
                    sink(progress("独立任务二"));
                    second_barrier.wait().await;
                    Ok(report())
                });
            let independent = observe(&mut second_updates, "独立任务二").await;
            assert!(!independent.stage.starts_with("已加入"));
            assert_eq!(coordinator.registry.lock().unwrap().len(), 2);
            bounded(barrier.wait()).await;
            bounded(first).await.unwrap().unwrap();
            bounded(second).await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn pending_operation_can_be_joined_before_service_lock_is_acquired() {
        let coordinator = RefreshCoordinator::default();
        let service_lock = Arc::new(tokio::sync::Mutex::new(()));
        let held = service_lock.lock().await;
        let operation_lock = Arc::clone(&service_lock);
        let requested = scope(true, &[(1, "a")]);
        let (first, mut first_updates) = request(
            coordinator.clone(),
            requested.clone(),
            move |sink| async move {
                sink(progress("等待刷新锁"));
                let _guard = operation_lock.lock().await;
                Ok(report())
            },
        );
        observe(&mut first_updates, "等待刷新锁").await;
        let (second, mut second_updates) = request(coordinator, requested, |_| async {
            panic!("must join pending")
        });
        assert!(observe(&mut second_updates, "等待刷新锁")
            .await
            .stage
            .starts_with("已加入"));
        drop(held);
        bounded(first).await.unwrap().unwrap();
        bounded(second).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelling_either_waiter_does_not_cancel_worker() {
        for cancel_creator in [true, false] {
            let coordinator = RefreshCoordinator::default();
            let release = Arc::new(Notify::new());
            let operation_release = Arc::clone(&release);
            let requested = scope(false, &[(1, "a")]);
            let (first, mut first_updates) = request(
                coordinator.clone(),
                requested.clone(),
                move |sink| async move {
                    sink(progress("运行中"));
                    operation_release.notified().await;
                    Ok(report())
                },
            );
            observe(&mut first_updates, "运行中").await;
            let (second, mut second_updates) = request(coordinator.clone(), requested, |_| async {
                panic!("must join")
            });
            observe(&mut second_updates, "运行中").await;
            let (cancelled, remaining) = if cancel_creator {
                (first, second)
            } else {
                (second, first)
            };
            cancelled.abort();
            assert!(bounded(cancelled).await.unwrap_err().is_cancelled());
            release.notify_one();
            assert_report(&bounded(remaining).await.unwrap().unwrap(), &report());
            assert!(coordinator.registry.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn worker_completes_even_after_every_waiter_disconnects() {
        let coordinator = RefreshCoordinator::default();
        let release = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let operation_release = Arc::clone(&release);
        let operation_finished = Arc::clone(&finished);
        let (caller, mut updates) = request(
            coordinator.clone(),
            scope(false, &[(1, "a")]),
            move |sink| async move {
                sink(progress("运行中"));
                operation_release.notified().await;
                operation_finished.notify_one();
                Ok(report())
            },
        );
        observe(&mut updates, "运行中").await;
        caller.abort();
        assert!(bounded(caller).await.unwrap_err().is_cancelled());
        drop(updates);
        release.notify_one();
        bounded(finished.notified()).await;
        bounded(async {
            loop {
                if coordinator.registry.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn shared_failure_preserves_original_api_code_and_message() {
        let coordinator = RefreshCoordinator::default();
        let release = Arc::new(Notify::new());
        let operation_release = Arc::clone(&release);
        let requested = scope(true, &[(1, "a")]);
        let source = AppError::External {
            code: "BILI_NETWORK".into(),
            message: "上传超时".into(),
        };
        let expected_code = source.code();
        let expected_message = source.to_string();
        let (first, mut first_updates) = request(
            coordinator.clone(),
            requested.clone(),
            move |sink| async move {
                sink(progress("上传中"));
                operation_release.notified().await;
                Err(source)
            },
        );
        observe(&mut first_updates, "上传中").await;
        let (second, mut second_updates) = request(coordinator.clone(), requested, |_| async {
            panic!("must join")
        });
        observe(&mut second_updates, "上传中").await;
        release.notify_one();
        for caller in [first, second] {
            let error = bounded(caller).await.unwrap().unwrap_err();
            assert_eq!(error.code(), expected_code);
            assert_eq!(error.to_string(), expected_message);
        }
        let failed = observe(&mut second_updates, "刷新失败").await;
        assert!(failed.done && failed.failed);
        assert!(coordinator.registry.lock().unwrap().is_empty());
        coordinator
            .run(scope(true, &[(1, "a")]), &|_| {}, |_| async {
                Ok(report())
            })
            .await
            .unwrap();
    }

    #[test]
    fn error_projection_preserves_multiple_error_classes() {
        let errors = [
            AppError::Database(sqlx::Error::PoolTimedOut),
            AppError::Validation("参数错误".into()),
            AppError::NotFound("主播不存在".into()),
            AppError::Unauthorized,
            AppError::NotConfigured("模型".into()),
            AppError::External {
                code: "LLM_401".into(),
                message: "认证失败".into(),
            },
        ];
        for error in errors {
            let code = error.code();
            let message = error.to_string();
            let projected = SharedFailure::from_error(error).into_error();
            assert_eq!(projected.code(), code);
            assert_eq!(projected.to_string(), message);
        }
    }

    #[tokio::test]
    async fn cancelled_worker_join_error_is_not_success() {
        let worker = tokio::spawn(std::future::pending::<()>());
        worker.abort();
        let failure = SharedFailure::from_join_error(bounded(worker).await.unwrap_err());
        let error = failure.into_error();
        assert_eq!(error.code(), "REFRESH_TASK_CANCELLED");
        assert_eq!(error.to_string(), "刷新后台任务意外取消，请重试");
    }

    #[tokio::test]
    async fn worker_panic_finishes_all_waiters_and_allows_retry() {
        let coordinator = RefreshCoordinator::default();
        let release = Arc::new(Notify::new());
        let operation_release = Arc::clone(&release);
        let requested = scope(false, &[(1, "a")]);
        let (first, mut first_updates) = request(
            coordinator.clone(),
            requested.clone(),
            move |sink| async move {
                sink(progress("即将异常"));
                operation_release.notified().await;
                panic!("controlled refresh worker panic");
            },
        );
        observe(&mut first_updates, "即将异常").await;
        let (second, mut second_updates) =
            request(coordinator.clone(), requested.clone(), |_| async {
                panic!("must join")
            });
        observe(&mut second_updates, "即将异常").await;
        release.notify_one();
        for caller in [first, second] {
            let error = bounded(caller).await.unwrap().unwrap_err();
            assert_eq!(error.code(), "REFRESH_TASK_PANICKED");
            assert_eq!(error.to_string(), "刷新后台任务发生内部异常，请重试");
            assert!(!error
                .to_string()
                .contains("controlled refresh worker panic"));
        }
        let failed = observe(&mut second_updates, "刷新失败").await;
        assert!(failed.failed && failed.done);
        assert!(coordinator.registry.lock().unwrap().is_empty());
        let retried = bounded(coordinator.run(requested, &|_| {}, |_| async { Ok(report()) }))
            .await
            .unwrap();
        assert_report(&retried, &report());
    }

    #[tokio::test]
    async fn completed_result_is_never_reused_by_a_new_run() {
        let coordinator = RefreshCoordinator::default();
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let operation_calls = Arc::clone(&calls);
            bounded(
                coordinator.run(scope(false, &[(1, "a")]), &|_| {}, move |_| async move {
                    operation_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(report())
                }),
            )
            .await
            .unwrap();
            assert!(coordinator.registry.lock().unwrap().is_empty());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn callbacks_are_outside_registry_lock_and_may_panic_independently() {
        let coordinator = RefreshCoordinator::default();
        let callback_coordinator = coordinator.clone();
        let callback_release = Arc::new(Notify::new());
        let operation_release = Arc::clone(&callback_release);
        bounded(coordinator.run(
            scope(false, &[(1, "a")]),
            &move |progress| {
                if !progress.done {
                    // Keep the worker blocked until this assertion, so a
                    // concurrent finish cannot make try_lock spuriously fail.
                    assert!(callback_coordinator.registry.try_lock().is_ok());
                    callback_release.notify_one();
                }
            },
            move |_| async move {
                operation_release.notified().await;
                Ok(report())
            },
        ))
        .await
        .unwrap();

        let release = Arc::new(Notify::new());
        let operation_release = Arc::clone(&release);
        let requested = scope(false, &[(1, "a")]);
        let caller_coordinator = coordinator.clone();
        let caller_scope = requested.clone();
        let caller = tokio::spawn(async move {
            caller_coordinator
                .run(
                    caller_scope,
                    &|_| panic!("controlled caller callback panic"),
                    move |sink| async move {
                        sink(progress("worker survives callback"));
                        operation_release.notified().await;
                        Ok(report())
                    },
                )
                .await
        });
        assert!(bounded(caller).await.unwrap_err().is_panic());
        let (remaining, mut remaining_updates) =
            request(coordinator, requested, |_| async { panic!("must join") });
        observe(&mut remaining_updates, "worker survives callback").await;
        release.notify_one();
        assert_report(&bounded(remaining).await.unwrap().unwrap(), &report());
    }
}
