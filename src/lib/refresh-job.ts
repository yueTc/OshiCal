import type {
  RefreshJob,
  RefreshJobAccepted,
  RefreshProgress,
  RefreshReport,
} from "../types";

interface RefreshJobApi {
  refreshJob(id?: number): Promise<RefreshJobAccepted>;
  refreshJobStatus(id: string): Promise<RefreshJob>;
}

interface RefreshJobOptions {
  sleep?: (ms: number) => Promise<void>;
  pollMs?: number;
}

function isJobId(value: unknown): value is string {
  return typeof value === "string"
    && value.length > 0
    && !/[\s\u0000-\u001f\u007f]/u.test(value);
}

function isJobStatus(value: unknown): value is RefreshJob["status"] {
  return value === "queued" || value === "running"
    || value === "completed" || value === "failed";
}

function isReport(value: unknown): value is RefreshReport {
  if (!value || typeof value !== "object") {
    return false;
  }
  const report = value as Partial<RefreshReport>;
  return Array.isArray(report.failedStreamers)
    && report.failedStreamers.every((reason) => typeof reason === "string")
    && [
      report.refreshedStreamers,
      report.newSchedules,
      report.cancelledSchedules,
      report.llmCalls,
      report.llmCacheHits,
    ].every((count) => typeof count === "number" && Number.isSafeInteger(count) && count >= 0);
}

function isProgress(value: unknown): value is RefreshJob["progress"] {
  if (!value || typeof value !== "object") {
    return false;
  }
  const progress = value as Partial<RefreshJob["progress"]>;
  return typeof progress.stage === "string"
    && typeof progress.done === "boolean"
    && typeof progress.failed === "boolean"
    && [progress.completed, progress.total].every(
      (count) => typeof count === "number" && Number.isSafeInteger(count) && count >= 0,
    )
    && typeof progress.percent === "number"
    && Number.isFinite(progress.percent)
    && progress.percent >= 0
    && progress.percent <= 100;
}

function withStage(stage: string, detail: string): string {
  return !stage || stage === detail ? detail : `${stage}；${detail}`;
}

/** Completion for one requested streamer; report counters may cover a shared full refresh. */
export function refreshCompletionProgress(
  report: RefreshReport,
  streamerId: number,
  streamerName: string,
): RefreshProgress {
  if (!isReport(report)) {
    throw new Error("刷新结果无效：缺少有效的刷新报告");
  }
  const failed = report.failedStreamers.length > 0;
  return {
    streamerId,
    streamerName,
    stage: failed
      ? `刷新结束，部分刷新失败：${report.failedStreamers.join("；")}`
      : "刷新完成",
    completed: 1,
    total: 1,
    percent: 100,
    done: true,
    failed,
  };
}

/** Submit once, then follow the accepted job even when it was deduplicated. */
export async function refreshStreamerJob(
  api: RefreshJobApi,
  streamerId: number,
  streamerName: string,
  onProgress: (progress: RefreshProgress) => void,
  options: RefreshJobOptions = {},
): Promise<RefreshReport> {
  if (!Number.isSafeInteger(streamerId) || streamerId <= 0) {
    throw new Error("刷新主播 ID 无效");
  }
  return followRefreshJob(api, streamerId, streamerName, onProgress, options);
}

export async function refreshAllJob(
  api: RefreshJobApi,
  onProgress: (progress: RefreshProgress) => void,
  options: RefreshJobOptions = {},
): Promise<RefreshReport> {
  return followRefreshJob(api, undefined, "全部主播", onProgress, options);
}

async function followRefreshJob(
  api: RefreshJobApi,
  streamerId: number | undefined,
  streamerName: string,
  onProgress: (progress: RefreshProgress) => void,
  options: RefreshJobOptions,
): Promise<RefreshReport> {
  const pollMs = options.pollMs ?? 1000;
  if (!Number.isSafeInteger(pollMs) || pollMs <= 0 || pollMs > 2_147_483_647) {
    throw new Error("刷新任务轮询间隔无效");
  }
  const sleep = options.sleep ?? ((ms: number) => new Promise<void>((resolve) => {
    setTimeout(resolve, ms);
  }));
  const accepted = await api.refreshJob(streamerId);
  if (!accepted || !isJobId(accepted.jobId) || !isJobStatus(accepted.status)) {
    throw new Error("服务器返回了无效的刷新任务 ID 或状态");
  }
  const jobId = accepted.jobId;
  for (;;) {
    // Network errors propagate. Never retry POST or silently create another job.
    const job = await api.refreshJobStatus(jobId);
    if (!job || !isJobId(job.jobId) || job.jobId !== jobId || !isJobStatus(job.status)) {
      throw new Error("刷新任务查询返回了无效的任务 ID 或状态");
    }
    if (!isProgress(job.progress)) {
      throw new Error("刷新任务查询返回了无效的进度");
    }
    // A full job can report streamerId=0/null; keep the user's original target.
    const progress: RefreshProgress = {
      ...job.progress,
      streamerId: streamerId ?? 0,
      streamerName,
      done: job.status === "completed" || job.status === "failed",
    };
    if (job.status === "failed") {
      const reason = job.error?.trim() || job.progress.stage || "服务器未提供失败原因";
      const message = `刷新任务失败：${reason}`;
      onProgress({ ...progress, stage: withStage(progress.stage, message), failed: true });
      throw new Error(message);
    }
    if (job.status === "completed") {
      if (!isReport(job.report)) {
        const message = "刷新任务已结束，但未返回有效的刷新报告";
        onProgress({ ...progress, stage: withStage(progress.stage, message), failed: true });
        throw new Error(message);
      }
      const completion = refreshCompletionProgress(job.report, streamerId ?? 0, streamerName);
      onProgress({
        ...progress,
        stage: completion.failed
          ? withStage(progress.stage, completion.stage)
          : progress.stage || completion.stage,
        failed: progress.failed || completion.failed,
      });
      return job.report;
    }
    onProgress(progress);
    await sleep(pollMs);
  }
}
