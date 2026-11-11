// Run only in CI: drives the real built App against a loopback, synthetic API.
// No application code is replaced, and no production server is contacted.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { resolve, extname, join, sep } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(join(process.env.OSHICAL_UI_RUNTIME, "package.json"));
const { chromium } = require("playwright");
const out = resolve("ui-artifacts");
await mkdir(out, { recursive: true });
const now = Date.now();
const iso = (time) => new Date(time).toISOString();
const start = now - 5 * 3600_000;
const settings = { dataMode: "server", serverUrl: "", livePollSeconds: 20,
  dynamicPollSeconds: 300, autoAnalyzeImages: true, notificationsEnabled: false,
  liveStatsEnabled: true, fertilityEnabled: false, llmBaseUrl: "https://example.invalid/v1",
  llmModel: "fixture", llmPromptVersion: "test", llmReasoningEffort: "auto", llmParallelism: 2, proxyPort: null };
const streamer = { id: 1, mid: "123", name: "曲线验收主播", avatarUrl: "", roomId: "456",
  liveUrl: "https://live.bilibili.com/456", aliases: [], scheduleEnabled: true,
  liveNotifyEnabled: false, liveStatsEnabled: true, fans: 1000, lastError: null,
  lastRefreshAt: iso(now), isLive: true, liveTitle: "采样曲线验收", liveCover: null,
  liveStartedAt: iso(start), liveCheckedAt: iso(now), linkedAccounts: [], sortOrder: 0,
  revision: 1, createdAt: iso(start) };
const schedule = { id: 1, streamerId: 1, streamerName: streamer.name, streamerAvatar: "",
  participants: [], title: "采样曲线验收", startAt: iso(start), endAt: null,
  roomUrl: streamer.liveUrl, roomOwnerMid: null, roomOwnerName: null, sourceKind: "manual",
  sourcePostId: null, sourceImageUrl: null, sourceAuthorMid: null, sourceFingerprint: null,
  sourceIdentity: null, sourcePublishedAt: null, weeklyRuleId: null, weeklyOccurrenceDate: null,
  confidence: 1, manualLocked: true, sourceChanged: false, cancelled: false,
  createdAt: iso(start), updatedAt: iso(now), revision: 1 };
const session = { id: 7, streamerId: 1, streamerName: streamer.name, roomId: "456",
  title: schedule.title, coverUrl: null, startedAt: iso(start), endedAt: null,
  durationSeconds: 18000, peakOnline: 0, peakHighEnergy: 1600, avgOnline: 0,
  onlineSamples: 0, onlineTotal: 0, revenueCents: 120000, giftRevenueCents: 90000,
  superChatRevenueCents: 20000, guardRevenueCents: 10000, paidUserCount: 35,
  interactiveUserCount: 500, danmakuCount: 6000, giftCount: 100, superChatCount: 4,
  guardCount: 1, reconnectCount: 1, segmentCount: 1, dataQuality: "partial",
  collectorState: "active", createdAt: iso(start), updatedAt: iso(now) };
const live = { scheduleId: 1, streamerId: 1, mid: "123", streamerName: streamer.name, avatarUrl: "",
  isLive: true, title: session.title, coverUrl: null, liveUrl: streamer.liveUrl,
  online: 0, startedAt: iso(start), checkedAt: iso(now), liveNotifyEnabled: false, revision: 1 };
let scenario = "normal";
let fail = false;
let extra = false;
const requests = [];
const baseSamples = Array.from({ length: 503 }, (_, i) => {
  const time = iso(start + i * 30_000 + (i >= 350 ? 150_000 : 0));
  const count = i === 502 ? 0 : i === 200 ? null : Math.round(900 + Math.sin(i / 30) * 360 + Math.cos(i / 7) * 120);
  return { id: i + 1, sessionId: 7, sampledAt: time, observedAt: count === null ? null : time,
    highEnergyCount: count, collectorState: count === null ? "reconnecting" : "active" };
});
const root = resolve("dist");
const server = createServer(async (req, res) => {
  const url = new URL(req.url, "http://127.0.0.1");
  const send = (data, status = 200) => {
    res.writeHead(status, { "Content-Type": "application/json" });
    res.end(JSON.stringify(status === 200 ? { ok: true, data } : { ok: false, error: { code: "FIXTURE", message: data } }));
  };
  try {
    if (url.pathname.startsWith("/api/")) {
      if (req.headers.authorization !== "Bearer ui-fixture") return send("Unauthorized", 401);
      if (url.pathname === "/api/subscriptions") return send([streamer]);
      if (url.pathname === "/api/live-status") return send([live]);
      if (url.pathname === "/api/schedules") return send([schedule]);
      if (url.pathname === "/api/settings") return send(settings);
      if (url.pathname === "/api/secrets/llm") return send({ apiKey: "", configured: false });
      if (url.pathname === "/api/bilibili/status") return send({ configured: false });
      if (url.pathname === "/api/live-sessions/by-schedule/1") return send(session);
      if (url.pathname === "/api/live-sessions/7/online-samples") {
        const after = Number(url.searchParams.get("afterId") ?? 0);
        requests.push({ after, scenario, at: Date.now() });
        if (scenario === "old") return send("Not found", 404);
        if (fail) return send("模拟连接中断", 503);
        const source = scenario === "empty" ? [] : baseSamples.map((value) => scenario === "missing"
          ? { ...value, highEnergyCount: null, observedAt: null } : value);
        if (extra && scenario === "normal") source.push({ ...baseSamples.at(-1), id: 504,
          sampledAt: iso(now), observedAt: iso(now), highEnergyCount: 777 });
        const candidates = source.filter((s) => s.id > after);
        const samples = candidates.slice(0, 500);
        return send({ sessionId: 7, metric: "bilibili_high_energy_users", sampleIntervalSeconds: 30,
          samples, nextAfterId: samples.at(-1)?.id ?? after, hasMore: candidates.length > samples.length });
      }
      if (url.pathname === "/api/sync/bootstrap") return send({ schemaVersion: 2,
        feedId: "ui-fixture", snapshotRevision: 1, minRetainedSeq: 0, serverTime: iso(now),
        subscriptions: [streamer], schedules: [schedule], weeklyRules: [], weeklyExceptions: [],
        scheduleSources: [], sourceSuppressions: [], serverSettings: settings, liveStatuses: [live], liveSessions: [session] });
      if (url.pathname === "/api/sync/changes") return send({ schemaVersion: 2, feedId: "ui-fixture",
        fromSeq: 2, toSeq: 1, latestSeq: 1, minRetainedSeq: 0, hasMore: false, resetRequired: false, changes: [] });
      if (url.pathname === "/api/devices/register") return send({ deviceId: "ui", name: "验收", platform: "web", appVersion: "test", online: true });
      if (url.pathname === "/api/devices") return send([]);
      if (url.pathname === "/api/sync/ack") return send({});
      if (url.pathname === "/api/fertility") return send({ enabled: false, statuses: [] });
      if (url.pathname === "/api/weekly-rules") return send([]);
      if (url.pathname === "/api/system/update") return send({ currentVersion: "0.3.30-beta.5", channel: "beta", status: "idle", autoCheckEnabled: false, autoUpdateEnabled: false });
      return send("Unused fixture route", 404);
    }
    const path = resolve(root, "." + decodeURIComponent(url.pathname));
    if (path !== root && !path.startsWith(root + sep)) { res.writeHead(403); res.end(); return; }
    let data;
    let name = path;
    try { data = await readFile(path); } catch { name = join(root, "index.html"); data = await readFile(name); }
    const types = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml" };
    res.writeHead(200, { "Content-Type": types[extname(name)] ?? "application/octet-stream" }); res.end(data);
  } catch (error) { res.writeHead(500); res.end(String(error)); }
});
await new Promise((done) => server.listen(0, "127.0.0.1", done));
const browser = await chromium.launch({ headless: true });
const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, timezoneId: "Asia/Shanghai", locale: "zh-CN" });
const page = await context.newPage();
const pageErrors = [];
page.on("pageerror", (error) => pageErrors.push(String(error)));
const results = [];
// Observe the actual entrance sequence without replacing app handlers or fixture responses.
await page.addInitScript(() => {
  window.__liveChartEntrance = [];
  const record = (event) => {
    const target = event.target;
    const card = document.querySelector('[data-schedule-id="1"]');
    const rect = card?.getBoundingClientRect();
    window.__liveChartEntrance.push({ type: event.type, at: performance.now(),
      button: event.button, target: target instanceof Element ? target.className : String(target),
      menuPresent: Boolean(document.querySelector('[role="menu"][aria-label="日程操作"]')),
      cardTop: rect?.top, cardBottom: rect?.bottom,
      viewportScroll: document.querySelector('.timeline-viewport')?.scrollTop });
    window.__liveChartEntrance = window.__liveChartEntrance.slice(-100);
  };
  for (const type of ["contextmenu", "click", "scroll", "pointerdown", "pointerup"])
    window.addEventListener(type, record, true);
});
try {
  await page.goto("http://127.0.0.1:" + server.address().port);
  await page.getByLabel("访问令牌", { exact: true }).fill("ui-fixture");
  await page.getByRole("button", { name: "连接服务器", exact: true }).click();
  const card = page.locator('[data-schedule-id="1"]').filter({ visible: true });
  await card.waitFor();
  await card.click({ button: "right" });
  await page.getByRole("button", { name: "查看直播数据", exact: true }).click();
  await page.getByRole("button", { name: "查看最高同时在线人数变化曲线", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "同接变化", exact: true });
  await dialog.getByRole("img", { name: "本场公开高能用户数变化曲线" }).waitFor();
  assert.deepEqual(requests.slice(0, 2).map((r) => r.after), [0, 500]);
  assert.equal(await dialog.locator("polyline").count(), 3);
  await page.screenshot({ path: join(out, "desktop-curve.png"), fullPage: true });
  results.push("真实入口：右键日程→查看直播数据→点击峰值卡片；两页加载503点与3段断线曲线通过");
  const slider = dialog.getByRole("slider", { name: "查看人数采样时间点", exact: true });
  await slider.focus(); await slider.press("End");
  assert.match(await slider.getAttribute("aria-valuetext"), /0 人/);
  await slider.press("ArrowLeft");
  assert.doesNotMatch(await slider.getAttribute("aria-valuetext"), /0 人$/);
  await dialog.getByRole("button", { name: "最近 1 小时", exact: true }).click();
  assert.equal(await dialog.getByRole("button", { name: "最近 1 小时", exact: true }).getAttribute("aria-pressed"), "true");
  await dialog.getByRole("button", { name: "整场", exact: true }).click();
  results.push("键盘滑块、人数真实0和时间范围切换通过");
  fail = true;
  await dialog.getByRole("button", { name: "刷新人数曲线", exact: true }).click();
  await dialog.getByRole("alert").waitFor();
  assert.equal(await dialog.locator("polyline").count(), 3);
  fail = false;
  await dialog.getByRole("button", { name: "重试", exact: true }).click();
  await dialog.getByRole("alert").waitFor({ state: "hidden" });
  results.push("断网刷新保留已有曲线；重试后恢复");
  await page.clock.install();
  extra = true;
  await page.clock.fastForward(31_000);
  await page.waitForFunction(() => document.querySelector('input[aria-label="查看人数采样时间点"]')?.getAttribute("max") === "503");
  assert.ok(requests.some((r) => r.after === 503));
  results.push("30秒自动增量读取新增点通过");
  await dialog.getByRole("button", { name: "返回直播统计", exact: true }).click();
  const peakCard = page.getByRole("button", { name: "查看最高同时在线人数变化曲线", exact: true });
  await peakCard.waitFor();
  const before = requests.length;
  await page.clock.fastForward(61_000);
  assert.equal(requests.length, before);
  results.push("返回后停止曲线轮询通过");
  await page.clock.resume();
  for (const mode of ["empty", "missing", "old"]) {
    scenario = mode; extra = false;
    await peakCard.click();
    if (mode === "empty") await page.getByText("暂无人数变化记录", { exact: true }).waitFor();
    if (mode === "missing") {
      const missingSlider = dialog.getByRole("slider", { name: "查看人数采样时间点", exact: true });
      await missingSlider.waitFor();
      assert.match(await missingSlider.getAttribute("aria-valuetext"), /采样缺失/);
      assert.equal(await dialog.locator("polyline").count(), 0);
    }
    if (mode === "old") {
      await dialog.getByRole("alert").waitFor();
      assert.match(await dialog.getByRole("alert").innerText(), /0.3.30-beta.5/);
    }
    await page.screenshot({ path: join(out, "curve-" + mode + ".png"), fullPage: true });
    await dialog.getByRole("button", { name: "返回直播统计", exact: true }).click();
  }
  results.push("旧场次无历史、全缺失、旧服务端404提示通过");
  scenario = "normal";
  await peakCard.click();
  await dialog.getByRole("img", { name: "本场公开高能用户数变化曲线" }).waitFor();
  await page.setViewportSize({ width: 480, height: 860 });
  await page.screenshot({ path: join(out, "curve-narrow.png"), fullPage: true });
  const bounds = await dialog.boundingBox();
  assert.ok(bounds && bounds.width <= 480 && bounds.x >= 0, "dialog overflows narrow viewport");
  assert.deepEqual(pageErrors, []);
  results.push("窄屏布局与无页面异常通过");
  await writeFile(join(out, "entrance-events.json"), JSON.stringify(await page.evaluate(() => window.__liveChartEntrance), null, 2));
  await writeFile(join(out, "result.json"), JSON.stringify({ results, requests, pageErrors }, null, 2));
  console.log(JSON.stringify({ uiChecksPassed: results.length, results }, null, 2));
  console.log("::notice title=Live chart UI::" + results.length + " real App interaction groups passed: " + results.join("; "));
} catch (error) {
  await writeFile(join(out, "entrance-events.json"), JSON.stringify(await page.evaluate(() => window.__liveChartEntrance), null, 2)).catch(() => {});
  await page.screenshot({ path: join(out, "failure.png"), fullPage: true }).catch(() => {});
  await writeFile(join(out, "failure.txt"), String(error) + "\n" + await page.locator("body").innerText());
  throw error;
} finally {
  await browser.close();
  await new Promise((done) => server.close(done));
}
