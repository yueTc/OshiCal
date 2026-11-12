// Run only in CI: drives the real built App against a loopback, synthetic API.
// No application code is replaced, and no production server is contacted.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { resolve, extname, join, sep } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(join(process.env.OSHICAL_UI_RUNTIME, "package.json"));
const { chromium } = require("playwright");
const out = resolve("ui-artifacts/broadcast");
await mkdir(out, { recursive: true });
const now = Date.parse("2026-11-11T12:00:00Z");
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
  roomUrl: streamer.liveUrl, roomOwnerMid: null, roomOwnerName: null, sourceKind: "live",
  sourcePostId: null, sourceImageUrl: null, sourceAuthorMid: null, sourceFingerprint: null,
  sourceIdentity: null, sourcePublishedAt: null, weeklyRuleId: null, weeklyOccurrenceDate: null,
  confidence: 1, manualLocked: false, sourceChanged: false, cancelled: false,
  createdAt: iso(start), updatedAt: iso(now), revision: 1 };
const session = { id: 7, scheduleId: 1, streamerId: 1, streamerName: streamer.name, roomId: "456",
  title: schedule.title, coverUrl: null, startedAt: iso(start), endedAt: null,
  durationSeconds: 18000, peakOnline: 0, peakHighEnergy: 1600, avgOnline: 0,
  onlineSamples: 0, onlineTotal: 0, revenueCents: 120000, giftRevenueCents: 90000,
  superChatRevenueCents: 20000, guardRevenueCents: 10000, paidUserCount: 35,
  interactiveUserCount: 500, danmakuCount: 6000, giftCount: 100, superChatCount: 4,
  guardCount: 1, reconnectCount: 1, segmentCount: 1, dataQuality: "partial",
  collectorState: "active", createdAt: iso(start), updatedAt: iso(now) };
const live = { streamerId: 1, mid: "123", streamerName: streamer.name, avatarUrl: "",
  isLive: true, title: session.title, coverUrl: null, liveUrl: streamer.liveUrl,
  online: 0, startedAt: iso(start), checkedAt: iso(now), liveNotifyEnabled: false, revision: 1 };
const yesterday = { ...schedule, id: 2, title: "昨天的直播", startAt: iso(start - 86400_000), sourceKind: "reserve" };
let schedules = [yesterday];
let currentLive = { ...live, isLive: false, scheduleId: null };
let sessions = [];
let recentScenario = "normal";
let recentDelay = 0;
let sequence = 1;
const changes = [];
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
      if (url.pathname === "/api/live-status") return send([currentLive]);
      if (url.pathname === "/api/schedules") return send(schedules);
      if (url.pathname === "/api/settings") return send(settings);
      if (url.pathname === "/api/secrets/llm") return send({ apiKey: "", configured: false });
      if (url.pathname === "/api/bilibili/status") return send({ configured: false });
      if (url.pathname === "/api/live-sessions") {
        requests.push({ recentStreamer: url.searchParams.get("streamerId"), recentLimit: url.searchParams.get("limit") });
        const result = recentScenario === "empty" ? [] : sessions;
        const failed = recentScenario === "error";
        if (recentDelay) await new Promise((done) => setTimeout(done, recentDelay));
        return failed ? send("模拟最近直播读取失败", 503) : send(result);
      }
      if (url.pathname.startsWith("/api/live-sessions/by-schedule/")) {
        requests.push({ scheduleLookup: url.pathname });
        return send(url.pathname.endsWith("/1") ? session : null);
      }
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
      if (url.pathname === "/api/sync/bootstrap") return send({ schemaVersion: 3,
        feedId: "ui-fixture", snapshotRevision: sequence, minRetainedSeq: 0, serverTime: iso(now),
        subscriptions: [streamer], schedules, weeklyRules: [], weeklyExceptions: [],
        scheduleSources: [], sourceSuppressions: [], serverSettings: settings, liveStatuses: [currentLive], liveSessions: sessions });
      if (url.pathname === "/api/sync/changes") {
        const after = Number(url.searchParams.get("after") ?? 0);
        const rows = changes.filter((change) => change.seq > after);
        return send({ schemaVersion: 3, feedId: "ui-fixture", fromSeq: after+1,
          toSeq: sequence, latestSeq: sequence, minRetainedSeq: 0, hasMore: false, resetRequired: false, changes: rows });
      }
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
let socket;
let socketReady;
const ready = new Promise((resolve) => { socketReady = resolve; });
await page.routeWebSocket(/\/api\/ws/, (route) => {
  socket = route;
  route.onMessage((message) => {
    if (JSON.parse(String(message)).type === "auth") {
      route.send(JSON.stringify({ type: "auth_ok", protocolVersion: 2, latestSeq: sequence,
        minRetainedSeq: 0, feedId: "ui-fixture", deviceId: "ui" }));
      socketReady();
    }
  });
});
function publish(items) {
  const first = sequence + 1;
  const batch = items.map(([entityType, payload, operation = "upsert"]) => ({
    seq: ++sequence, entityType, entityId: String(payload.id ?? payload.streamerId), operation,
    entityRevision: sequence, payload, createdAt: iso(now),
  }));
  changes.push(...batch);
  socket.send(JSON.stringify({ type: "sync_batch", feedId: "ui-fixture", fromSeq: first,
    toSeq: sequence, latestSeq: sequence, changes: batch }));
}
const results = [];
try {
  await page.clock.install({ time: new Date(now) });
  await page.goto("http://127.0.0.1:" + server.address().port);
  await page.getByLabel("访问令牌", { exact: true }).fill("ui-fixture");
  await page.getByRole("button", { name: "连接服务器", exact: true }).click();
  const oldCard = page.locator('[data-schedule-id="2"]').filter({ visible: true });
  await oldCard.waitFor();
  await ready;
  assert.equal(await page.locator(".schedule-card.is-live").count(), 0);
  results.push("开播前昨天卡片不高亮");
  schedules = [yesterday, schedule]; currentLive = { ...live, scheduleId: 1 }; sessions = [session];
  publish([["schedule", schedule], ["live_session", session], ["live_status", currentLive]]);
  const today = page.locator('[data-schedule-id="1"]').filter({ visible: true });
  await today.waitFor();
  await page.waitForFunction(() => document.querySelector('[data-schedule-id="1"]')?.classList.contains("is-live"));
  assert.equal(await page.locator(".schedule-card.is-live").count(), 1);
  assert.ok(!(await oldCard.getAttribute("class")).includes("is-live"));
  assert.match(await today.innerText(), /开播补录/);
  await page.screenshot({ path: join(out, "unannounced-live.png"), fullPage: true });
  results.push("真实增量同步自动补出今日卡片；仅本场高亮，昨天卡片不变");
  const other = { ...schedule, id: 3, startAt: iso(now), title: "另一场预告", sourceKind: "reserve" };
  schedules.push(other); currentLive = { ...currentLive, startedAt: iso(now), revision: 2 };
  publish([["schedule", other], ["live_status", currentLive]]);
  await page.locator('[data-schedule-id="3"]').filter({ visible: true }).waitFor();
  assert.ok((await today.getAttribute("class")).includes("is-live"));
  assert.equal(await page.locator('[data-schedule-id="3"].is-live').count(), 0);
  results.push("重连后平台开播时间重置且另一卡片更近，仍绑定原卡");
  // Settle driver-initiated scrolling before testing the user's right-click entrance.
  await today.scrollIntoViewIfNeeded();
  await today.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  await today.click({ button: "right" });
  await page.getByRole("menu", { name: "日程操作", exact: true }).waitFor();
  await page.getByRole("button", { name: "查看直播数据", exact: true }).click();
  await page.getByRole("button", { name: "查看最高同时在线人数变化曲线", exact: true }).waitFor();
  assert.deepEqual(requests.filter((r) => r.scheduleLookup).map((r) => r.scheduleLookup), ["/api/live-sessions/by-schedule/1"]);
  await page.screenshot({ path: join(out, "correct-live-session.png"), fullPage: true });
  results.push("右键本场卡片打开实际绑定的统计场次");
  await page.keyboard.press("Escape");
  currentLive = { ...currentLive, scheduleId: null, revision: 3 };
  publish([["live_status", currentLive]]);
  await page.waitForFunction(() => !document.querySelector(".schedule-card.is-live"));
  results.push("显式无绑定时不回退到其他预告");
  currentLive = { ...currentLive, scheduleId: 1, revision: 4 };
  schedules = schedules.filter((s) => s.id !== 1);
  publish([["schedule", { id: 1 }, "delete"], ["live_status", currentLive]]);
  await today.waitFor({ state: "hidden" });
  assert.equal(await page.locator(".schedule-card.is-live").count(), 0);
  assert.deepEqual(pageErrors, []);
  results.push("绑定卡片删除/未加载时不高亮昨天或其他卡片；页面无异常");
  // The creator-library entry must use the real session, not a nearby schedule lookup.
  await page.getByRole("button", { name: "主播库", exact: true }).click();
  const more = page.getByRole("button", { name: `${streamer.name} 的更多操作`, exact: true });
  const moreMenu = page.getByRole("menu", { name: `${streamer.name} 的更多操作`, exact: true });
  await more.waitFor();
  assert.equal(await page.getByRole("button", { name: `设置 ${streamer.name} 的固定周表`, exact: true }).count(), 0);
  await more.click();
  await moreMenu.waitFor();
  assert.deepEqual(await moreMenu.getByRole("menuitem").allTextContents(), ["设定周表", "设置别名", "查看最近直播"]);
  await more.click();
  await moreMenu.waitFor({ state: "hidden" });
  await more.click();
  await page.keyboard.press("End");
  assert.equal(await moreMenu.getByRole("menuitem", { name: "查看最近直播", exact: true }).evaluate((el) => el === document.activeElement), true);
  await page.keyboard.press("Escape");
  assert.equal(await more.evaluate((el) => el === document.activeElement), true);
  for (const [entry, title] of [["设定周表", `${streamer.name} 的固定周表`], ["设置别名", `${streamer.name} 的识别别名`]]) {
    await more.click();
    await moreMenu.getByRole("menuitem", { name: entry, exact: true }).click();
    const modal = page.getByRole("dialog", { name: title, exact: true });
    await modal.waitFor();
    await modal.getByRole("button", { name: "关闭", exact: true }).click();
  }
  results.push("主播库周表与别名移入三点菜单，原对话框保留；重复点击、键盘导航与 Escape 还焦有效");
  const older = { ...session, id: 8, scheduleId: null, title: "历史直播无日程绑定", startedAt: iso(start - 86400_000),
    endedAt: iso(start - 80000_000), dataQuality: "complete", reconnectCount: 0, revenueCents: 55500 };
  sessions = [older, { ...session, streamerId: 999, id: 99 }, session];
  const openRecent = async () => {
    await more.click();
    await moreMenu.getByRole("menuitem", { name: "查看最近直播", exact: true }).click();
  };
  await openRecent();
  const recentDialog = page.getByRole("dialog", { name: `${streamer.name} · 最近直播`, exact: true });
  await recentDialog.locator(".recent-live-card").first().waitFor();
  assert.equal(await recentDialog.locator(".recent-live-card").count(), 2);
  assert.deepEqual(await recentDialog.locator(".recent-live-card").evaluateAll((els) => els.map((el) => el.dataset.sessionId)), ["7", "8"]);
  const recentRequest = requests.filter((request) => request.recentStreamer).at(-1);
  assert.equal(recentRequest.recentStreamer, "1");
  assert.equal(recentRequest.recentLimit, "50");
  await page.screenshot({ path: join(out, "creator-recent-lives.png"), fullPage: true });
  await recentDialog.locator('[data-session-id="8"]').click();
  const stats = page.getByRole("dialog", { name: "直播数据", exact: true });
  assert.match(await stats.innerText(), /历史直播无日程绑定/);
  assert.match(await stats.innerText(), /555\.0/);
  await stats.getByRole("button", { name: "返回最近直播", exact: true }).click();
  assert.equal(await recentDialog.locator('[data-session-id="8"]').evaluate((el) => el === document.activeElement), true);
  await recentDialog.locator('[data-session-id="7"]').click();
  await stats.getByRole("button", { name: "查看最高同时在线人数变化曲线", exact: true }).click();
  // SVG 的 aria-labelledby 同时引用标题与坐标说明，完整无障碍名称不只有标题。
  await page.getByRole("dialog", { name: "同接变化", exact: true })
    .getByRole("img", { name: /^本场公开高能用户数变化曲线 横轴为北京时间，纵轴为人数。/ }).waitFor();
  await page.getByRole("button", { name: "返回最近直播", exact: true }).click();
  assert.equal(await recentDialog.locator(".recent-live-card").count(), 2);
  results.push("最近直播仅显示本主播，倒序卡片直接打开对应场次（含无日程绑定场次）；曲线与返回列表有效");
  recentScenario = "error";
  await recentDialog.getByRole("button", { name: "刷新最近直播", exact: true }).click();
  await recentDialog.getByRole("alert").waitFor();
  recentScenario = "empty";
  await recentDialog.getByRole("button", { name: "重试", exact: true }).click();
  await recentDialog.getByRole("heading", { name: "还没有最近直播记录", exact: true }).waitFor();
  assert.equal(await recentDialog.locator(".recent-live-card").count(), 0);
  await recentDialog.getByRole("button", { name: "关闭", exact: true }).click();
  recentScenario = "normal";
  recentDelay = 250;
  const delayed = page.waitForResponse((response) => new URL(response.url()).pathname === "/api/live-sessions");
  await openRecent();
  await recentDialog.getByRole("status").waitFor();
  await recentDialog.getByRole("button", { name: "关闭", exact: true }).click();
  await delayed;
  assert.equal(await page.getByRole("dialog").count(), 0);
  recentDelay = 0;
  await page.setViewportSize({ width: 390, height: 844 });
  await more.click();
  const menuBounds = await moreMenu.boundingBox();
  assert.ok(menuBounds.x >= 0 && menuBounds.x + menuBounds.width <= 390);
  assert.ok(menuBounds.y >= 0 && menuBounds.y + menuBounds.height <= 844);
  await moreMenu.getByRole("menuitem", { name: "查看最近直播", exact: true }).click();
  await recentDialog.locator(".recent-live-card").first().waitFor();
  await page.screenshot({ path: join(out, "creator-recent-lives-mobile.png"), fullPage: true });
  await page.keyboard.press("Escape");
  await recentDialog.waitFor({ state: "hidden" });
  assert.deepEqual(pageErrors, []);
  results.push("最近直播错误可重试、空态无旧卡片、关闭后延迟响应不重开；窄屏菜单和卡片验收");
  await writeFile(join(out, "result.json"), JSON.stringify({ results, pageErrors, requests }, null, 2));
  console.log(JSON.stringify({ uiChecksPassed: results.length, results }, null, 2));
  console.log("::notice title=Broadcast card UI::" + results.length + " real App interaction groups passed: " + results.join("; "));
} catch (error) {
  await page.screenshot({ path: join(out, "failure.png"), fullPage: true }).catch(() => {});
  await writeFile(join(out, "failure.txt"), String(error) + "\n" + await page.locator("body").innerText());
  throw error;
} finally {
  await browser.close();
  await new Promise((done) => server.close(done));
}
