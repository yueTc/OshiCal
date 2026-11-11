import assert from "node:assert/strict";
import test from "node:test";
import { loadOnlineSamplePages, nearestOnlinePoint, onlinePoints, onlineSegments, onlineTime, thinOnlineSegment } from "../src/lib/live-online.ts";
import type { LiveOnlineSample, LiveOnlineSamplesPage } from "../src/types.ts";

const BASE = Date.parse("2026-11-11T15:59:00Z");
function sample(id: number, value: number | null, seconds = (id - 1) * 30): LiveOnlineSample {
  const time = new Date(BASE + seconds * 1000).toISOString();
  return { id, sessionId: 7, sampledAt: time, observedAt: value === null ? null : time,
    highEnergyCount: value, collectorState: "active" };
}
function page(samples: LiveOnlineSample[], nextAfterId: number, hasMore = false): LiveOnlineSamplesPage {
  return { sessionId: 7, metric: "bilibili_high_energy_users", sampleIntervalSeconds: 30, samples, nextAfterId, hasMore };
}

test("loads all pages in order, then requests only newer samples", async () => {
  const cursors: number[] = [];
  const loader = async (_id: number, afterId: number) => {
    cursors.push(afterId);
    return afterId === 0 ? page([sample(1, 100)], 1, true)
      : afterId === 1 ? page([sample(2, 70), sample(3, 0)], 3)
      : page([], afterId);
  };
  const loaded = await loadOnlineSamplePages(loader, 7);
  assert.deepEqual(cursors, [0, 1]);
  assert.deepEqual(loaded.samples.map((s) => s.highEnergyCount), [100, 70, 0]);
  assert.equal(loaded.nextAfterId, 3);
  const next = await loadOnlineSamplePages(loader, 7, loaded.nextAfterId);
  assert.deepEqual(next.samples, []);
  assert.deepEqual(cursors, [0, 1, 3]);
});

test("rejects empty/nonadvancing, overlapping, wrong-session and wrong-metric pages", async () => {
  const invalid = [
    page([], 0, true), page([sample(1, 1)], 0, true),
    page([sample(2, 2), sample(1, 1)], 1), page([sample(1, 1), sample(1, 1)], 1),
    { ...page([sample(1, 1)], 1), sessionId: 8 },
    { ...page([sample(1, 1)], 1), metric: "total_viewers" },
    page([{ ...sample(1, 1), sessionId: 8 }], 1),
  ];
  for (const value of invalid) {
    await assert.rejects(loadOnlineSamplePages(async () => value as LiveOnlineSamplesPage, 7));
  }
});

test("does not accept partial historical pages if a later request fails", async () => {
  let calls = 0;
  await assert.rejects(loadOnlineSamplePages(async () => {
    calls += 1;
    if (calls === 1) return page([sample(1, 1)], 1, true);
    throw new Error("离线");
  }, 7), /离线/);
  assert.equal(calls, 2);
});

test("cancellation stops before starting and also discards a delayed response", async () => {
  const before = new AbortController(); before.abort();
  let calls = 0;
  await assert.rejects(loadOnlineSamplePages(async () => {
    calls += 1; return page([], 0);
  }, 7, 0, before.signal), /取消/);
  assert.equal(calls, 0);
  const during = new AbortController();
  await assert.rejects(loadOnlineSamplePages(async () => {
    during.abort(); return page([sample(1, 100)], 1);
  }, 7, 0, during.signal), /取消/);
});

test("preserves declines and genuine zero while splitting null and reconnect gaps", () => {
  const points = onlinePoints([sample(1, 100), sample(2, 70), sample(3, 0),
    sample(4, null), sample(5, 30), { ...sample(6, 90), collectorState: "reconnecting" }, sample(7, 25)]);
  assert.deepEqual(points.map((p) => p.value), [100, 70, 0, null, 30, null, 25]);
  assert.deepEqual(onlineSegments(points).map((s) => s.map((p) => p.value)), [[100, 70, 0], [30], [25]]);
});

test("long recording interruptions are not drawn as continuous lines", () => {
  const points = onlinePoints([sample(1, 100), sample(2, 200, 30), sample(3, 70, 150)]);
  assert.deepEqual(onlineSegments(points).map((s) => s.length), [2, 1]);
});

test("invalid count or observation is missing, never converted to zero", () => {
  const points = onlinePoints([
    sample(1, -1), sample(2, Number.NaN), sample(3, Number.POSITIVE_INFINITY), sample(4, 1.5),
    { ...sample(5, 100), observedAt: null },
    { ...sample(6, 100), observedAt: "invalid" },
    { ...sample(7, 100), observedAt: new Date(BASE + 999_000).toISOString() },
    sample(8, 0),
  ]);
  assert.deepEqual(points.map((p) => p.value), [null, null, null, null, null, null, null, 0]);
});

test("empty, single, all-zero and all-missing series remain explicit", () => {
  assert.deepEqual(onlinePoints([]), []);
  assert.deepEqual(onlineSegments(onlinePoints([sample(1, null)])), []);
  assert.equal(onlineSegments(onlinePoints([sample(1, 0)]))[0][0].value, 0);
  assert.deepEqual(onlineSegments(onlinePoints([sample(1, 0), sample(2, 0)]))[0].map((p) => p.value), [0, 0]);
});

test("sorts timestamps, excludes invalid times and finds nearest points", () => {
  const points = onlinePoints([sample(3, 30), sample(1, 10), { ...sample(4, 9), sampledAt: "bad" }, sample(2, 20)]);
  assert.deepEqual(points.map((p) => p.id), [1, 2, 3]);
  assert.equal(nearestOnlinePoint(points, BASE - 1000), 0);
  assert.equal(nearestOnlinePoint(points, BASE + 40_000), 1);
  assert.equal(nearestOnlinePoint(points, BASE + 90_000), 2);
  assert.equal(nearestOnlinePoint([], BASE), -1);
});

test("downsamples long SVG segments without losing extrema or endpoints", () => {
  const points = onlinePoints(Array.from({length: 10_000}, (_, i) => sample(i + 1, i === 5432 ? 99999 : i === 4567 ? 0 : 200)));
  const thin = thinOnlineSegment(points, 800);
  assert.ok(thin.length <= 800);
  assert.equal(thin[0].id, 1);
  assert.equal(thin.at(-1)?.id, 10000);
  assert.ok(thin.some((p) => p.value === 99999));
  assert.ok(thin.some((p) => p.value === 0));
  assert.ok(thin.every((p, i) => i === 0 || p.time > thin[i - 1].time));
});

test("time labels explicitly use Beijing time across midnight", () => {
  assert.match(onlineTime(BASE), /23:59:00/);
  assert.match(onlineTime(BASE + 120_000), /00:01:00/);
});
