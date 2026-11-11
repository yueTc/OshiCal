import assert from "node:assert/strict";
import test from "node:test";
import { refreshCompletionProgress, refreshStreamerJob } from "../src/lib/refresh-job.ts";
import type { RefreshJob, RefreshJobAccepted, RefreshProgress, RefreshReport } from "../src/types.ts";

const STREAMER_ID = 42;
const STREAMER_NAME = "泽音";

function report(overrides: Partial<RefreshReport> = {}): RefreshReport {
  return {
    refreshedStreamers: 1,
    failedStreamers: [],
    newSchedules: 2,
    cancelledSchedules: 0,
    llmCalls: 1,
    llmCacheHits: 3,
    ...overrides,
  };
}

function job(status: RefreshJob["status"], overrides: Partial<RefreshJob> = {}): RefreshJob {
  const done = status === "completed" || status === "failed";
  return {
    jobId: "job-1",
    kind: "refresh_streamer",
    streamerId: STREAMER_ID,
    streamerName: STREAMER_NAME,
    status,
    progress: {
      stage: status === "queued" ? "等待调度" : status === "running" ? "正在识别" : "刷新完成",
      completed: done ? 1 : 0,
      total: 1,
      percent: done ? 100 : 0,
      done,
      failed: status === "failed",
    },
    createdAt: "2026-11-11T00:00:00Z",
    startedAt: null,
    finishedAt: null,
    report: status === "completed" ? report() : null,
    error: null,
    ...overrides,
  };
}

function harness(
  jobs: RefreshJob[],
  accepted: RefreshJobAccepted = { jobId: "job-1", status: "queued" },
) {
  const posted: (number | undefined)[] = [];
  const polled: string[] = [];
  const sleeps: number[] = [];
  const progress: RefreshProgress[] = [];
  const api = {
    async refreshJob(id?: number): Promise<RefreshJobAccepted> {
      posted.push(id);
      return accepted;
    },
    async refreshJobStatus(id: string): Promise<RefreshJob> {
      polled.push(id);
      const next = jobs[polled.length - 1];
      assert.ok(next, "unexpected extra status request");
      return next;
    },
  };
  const sleep = async (ms: number): Promise<void> => { sleeps.push(ms); };
  const onProgress = (value: RefreshProgress): void => { progress.push(value); };
  const run = () => refreshStreamerJob(api, STREAMER_ID, STREAMER_NAME, onProgress, { sleep });
  return { api, posted, polled, sleeps, progress, sleep, onProgress, run };
}

test("follows queued -> running -> completed, posting once and polling every 1000ms", async () => {
  const finalReport = report();
  const h = harness([job("queued"), job("running"), job("completed", { report: finalReport })]);
  assert.equal(await h.run(), finalReport);
  assert.deepEqual(h.posted, [STREAMER_ID]);
  assert.deepEqual(h.polled, ["job-1", "job-1", "job-1"]);
  assert.deepEqual(h.sleeps, [1000, 1000]);
  assert.deepEqual(h.progress.map((value) => value.stage), ["等待调度", "正在识别", "刷新完成"]);
  assert.deepEqual(h.progress.map((value) => value.done), [false, false, true]);
  assert.ok(h.progress.every((value) => value.streamerId === STREAMER_ID
    && value.streamerName === STREAMER_NAME && !value.failed));
});

test("deduplicated acceptance still follows the existing job instead of creating another", async () => {
  const h = harness([job("running"), job("completed")], {
    jobId: "job-1", status: "running", deduplicated: true,
  });
  await h.run();
  assert.deepEqual(h.posted, [STREAMER_ID]);
  assert.equal(h.polled.length, 2);
  assert.deepEqual(h.sleeps, [1000]);
});

test("even completed acceptance must be polled for its actual report", async () => {
  const h = harness([job("completed")], {
    jobId: "job-1", status: "completed", deduplicated: true,
  });
  await h.run();
  assert.deepEqual(h.polled, ["job-1"]);
  assert.deepEqual(h.sleeps, []);
  assert.deepEqual(h.posted, [STREAMER_ID]);
});

test("partial failure is a failed terminal progress with the original reason", async () => {
  const finalReport = report({ failedStreamers: ["泽音：模型请求超时"] });
  const h = harness([job("completed", { report: finalReport })]);
  assert.equal(await h.run(), finalReport);
  const terminal = h.progress.at(-1)!;
  assert.equal(terminal.done, true);
  assert.equal(terminal.failed, true);
  assert.match(terminal.stage, /部分刷新失败/);
  assert.match(terminal.stage, /泽音：模型请求超时/);
});

test("shared full-refresh progress keeps its stage and the requested streamer target", async () => {
  const full = { kind: "refresh_all" as const, streamerId: 0, streamerName: "全部主播" };
  const shared = { stage: "共享全量结果", completed: 8, total: 8, percent: 100, done: true, failed: false };
  const finalReport = report({ refreshedStreamers: 8, cancelledSchedules: 19 });
  const h = harness([
    job("running", { ...full, progress: { ...shared, stage: "全量刷新：识别主播乙", done: false, percent: 50 } }),
    job("completed", { ...full, progress: shared, report: finalReport }),
  ]);
  assert.equal(await h.run(), finalReport);
  assert.deepEqual(h.progress.map((value) => value.stage), ["全量刷新：识别主播乙", "共享全量结果"]);
  assert.ok(h.progress.every((value) => value.streamerId === STREAMER_ID && value.streamerName === STREAMER_NAME));
  assert.equal(h.progress.at(-1)!.completed, 8);
  assert.equal(h.progress.at(-1)!.total, 8);
  assert.doesNotMatch(h.progress.at(-1)!.stage, /删除|19/);
});

test("partial shared-full failure preserves the shared stage and does not attribute it to this streamer", async () => {
  const h = harness([job("completed", {
    kind: "refresh_all", streamerId: null, streamerName: null,
    progress: { stage: "共享全量结果", completed: 8, total: 8, percent: 100, done: true, failed: false },
    report: report({ refreshedStreamers: 8, failedStreamers: ["主播乙：下载失败"], cancelledSchedules: 27 }),
  })]);
  await h.run();
  const terminal = h.progress.at(-1)!;
  assert.equal(terminal.streamerId, STREAMER_ID);
  assert.equal(terminal.streamerName, STREAMER_NAME);
  assert.equal(terminal.failed, true);
  assert.match(terminal.stage, /^共享全量结果；/);
  assert.match(terminal.stage, /主播乙：下载失败/);
  assert.doesNotMatch(terminal.stage, /删除|27|泽音.*下载失败/);
});

test("failed task emits failed terminal progress and rejects even if a report exists", async () => {
  const h = harness([job("failed", { error: "数据库写入失败", report: report() })]);
  await assert.rejects(h.run(), /刷新任务失败：数据库写入失败/);
  assert.equal(h.progress.at(-1)!.done, true);
  assert.equal(h.progress.at(-1)!.failed, true);
  assert.match(h.progress.at(-1)!.stage, /数据库写入失败/);
  assert.deepEqual(h.posted, [STREAMER_ID]);
  assert.deepEqual(h.sleeps, []);
});

test("failed task without an error still rejects with an explicit reason", async () => {
  const h = harness([job("failed", {
    progress: { stage: "", completed: 0, total: 1, percent: 0, done: true, failed: false },
  })]);
  await assert.rejects(h.run(), /刷新任务失败：服务器未提供失败原因/);
  assert.equal(h.progress.at(-1)!.failed, true);
});

test("completed task without a valid report never emits successful terminal progress", async () => {
  for (const invalid of [null, undefined, {}, { ...report(), failedStreamers: null }, { ...report(), llmCalls: -1 }]) {
    const h = harness([job("completed", { report: invalid as RefreshReport | null })]);
    await assert.rejects(h.run(), /未返回有效的刷新报告/);
    assert.equal(h.progress.at(-1)!.done, true);
    assert.equal(h.progress.at(-1)!.failed, true);
    assert.deepEqual(h.posted, [STREAMER_ID]);
  }
});

test("invalid accepted IDs and statuses fail closed without polling", async () => {
  for (const invalid of [
    { jobId: "", status: "queued" },
    { jobId: " job-1", status: "queued" },
    { jobId: "job\n1", status: "queued" },
    { jobId: null, status: "queued" },
    { jobId: "job-1", status: "unknown" },
    null,
  ]) {
    const h = harness([], invalid as unknown as RefreshJobAccepted);
    // harness's default argument only applies to undefined, not null.
    await assert.rejects(h.run(), /无效的刷新任务 ID 或状态/);
    assert.deepEqual(h.posted, [STREAMER_ID]);
    assert.deepEqual(h.polled, []);
    assert.deepEqual(h.progress, []);
  }
});

test("invalid queried ID, mismatched ID and unknown status fail closed", async () => {
  for (const invalid of [
    job("running", { jobId: "" }),
    job("running", { jobId: "other-job" }),
    job("running", { status: "unknown" as RefreshJob["status"] }),
  ]) {
    const h = harness([invalid]);
    await assert.rejects(h.run(), /无效的任务 ID 或状态/);
    assert.deepEqual(h.posted, [STREAMER_ID]);
    assert.deepEqual(h.polled, ["job-1"]);
    assert.deepEqual(h.sleeps, []);
    assert.deepEqual(h.progress, []);
  }
});

test("malformed progress is rejected rather than rendered as success", async () => {
  for (const invalid of [null, {}, { ...job("running").progress, percent: NaN }]) {
    const h = harness([job("running", { progress: invalid as RefreshJob["progress"] })]);
    await assert.rejects(h.run(), /无效的进度/);
    assert.deepEqual(h.progress, []);
    assert.deepEqual(h.sleeps, []);
  }
});

test("POST network error propagates without retry or polling", async () => {
  const h = harness([]);
  const error = new Error("POST network down");
  let posts = 0;
  h.api.refreshJob = async () => { posts += 1; throw error; };
  await assert.rejects(h.run(), (actual) => actual === error);
  assert.equal(posts, 1);
  assert.deepEqual(h.polled, []);
  assert.deepEqual(h.sleeps, []);
});

test("poll network error propagates without another POST or silent retries", async () => {
  const h = harness([job("running")]);
  const original = h.api.refreshJobStatus;
  const error = new Error("status network down");
  let polls = 0;
  h.api.refreshJobStatus = async (id) => {
    polls += 1;
    if (polls === 2) throw error;
    return original(id);
  };
  await assert.rejects(h.run(), (actual) => actual === error);
  assert.equal(polls, 2);
  assert.deepEqual(h.posted, [STREAMER_ID]);
  assert.deepEqual(h.sleeps, [1000]);
});

test("custom polling uses injected sleep with no real delay", async () => {
  const h = harness([job("queued"), job("running"), job("completed")]);
  await refreshStreamerJob(h.api, STREAMER_ID, STREAMER_NAME, h.onProgress, { sleep: h.sleep, pollMs: 7 });
  assert.deepEqual(h.sleeps, [7, 7]);
  assert.deepEqual(h.posted, [STREAMER_ID]);
});

test("invalid poll intervals and streamer IDs fail before POST", async () => {
  for (const pollMs of [0, -1, NaN, Infinity, 1.5, 2_147_483_648]) {
    const h = harness([]);
    await assert.rejects(
      refreshStreamerJob(h.api, STREAMER_ID, STREAMER_NAME, h.onProgress, { sleep: h.sleep, pollMs }),
      /轮询间隔无效/,
    );
    assert.deepEqual(h.posted, []);
  }
  for (const id of [0, -1, NaN, 1.5]) {
    const h = harness([]);
    await assert.rejects(refreshStreamerJob(h.api, id, STREAMER_NAME, h.onProgress, { sleep: h.sleep }), /主播 ID 无效/);
    assert.deepEqual(h.posted, []);
  }
});

test("local completion exposes partial-failure reasons and never labels global cancellations as deletions", () => {
  const partial = refreshCompletionProgress(report({ failedStreamers: ["主播乙：模型超时"], cancelledSchedules: 31 }), STREAMER_ID, STREAMER_NAME);
  assert.equal(partial.failed, true);
  assert.equal(partial.done, true);
  assert.equal(partial.percent, 100);
  assert.equal(partial.streamerId, STREAMER_ID);
  assert.equal(partial.streamerName, STREAMER_NAME);
  assert.match(partial.stage, /主播乙：模型超时/);
  assert.doesNotMatch(partial.stage, /删除|31/);
  assert.deepEqual(refreshCompletionProgress(report({ cancelledSchedules: 31 }), STREAMER_ID, STREAMER_NAME), {
    streamerId: STREAMER_ID, streamerName: STREAMER_NAME, stage: "刷新完成",
    completed: 1, total: 1, percent: 100, done: true, failed: false,
  });
});

test("a server failed progress flag cannot be overwritten by a successful report", async () => {
  const h = harness([job("completed", {
    progress: { stage: "刷新部分失败", completed: 1, total: 1, percent: 100, done: true, failed: true },
  })]);
  await h.run();
  assert.equal(h.progress.at(-1)!.failed, true);
  assert.equal(h.progress.at(-1)!.stage, "刷新部分失败");
});
