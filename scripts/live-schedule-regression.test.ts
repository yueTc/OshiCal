import assert from "node:assert/strict";
import test from "node:test";
import { pathToFileURL } from "node:url";
const { matchLiveSchedules } = await import(process.env.OSHICAL_LIVE_SCHEDULE_MODULE
  ? pathToFileURL(process.env.OSHICAL_LIVE_SCHEDULE_MODULE).href : "../src/lib/live-schedule.ts");
const start = Date.parse("2026-11-11T12:00:00Z");
const card = (id: number, offset = 0, cancelled = false, streamerId = 1) => ({
  id, streamerId, startAt: new Date(start + offset).toISOString(), cancelled,
});
const live = (binding: number | null | undefined = undefined, known?: boolean) => ({
  streamerId: 1, isLive: true, startedAt: new Date(start).toISOString(),
  checkedAt: new Date(start + 30_000).toISOString(),
  ...(binding === undefined ? {} : { scheduleId: binding }),
  ...(known === undefined ? {} : { scheduleKnown: known }),
});

test("legacy yesterday-only cards must not be marked live", () => {
  assert.deepEqual([...matchLiveSchedules([card(1, -86400_000)], [live()])], []);
});
test("legacy two-hour boundary is inclusive, but one millisecond outside is excluded", () => {
  for (const sign of [-1, 1]) {
    assert.deepEqual([...matchLiveSchedules([card(1, sign * 7200_000)], [live()])], [1]);
    assert.deepEqual([...matchLiveSchedules([card(1, sign * 7200_001)], [live()])], []);
  }
});
test("explicit binding survives cross-midnight and ignores a closer unrelated card", () => {
  assert.deepEqual([...matchLiveSchedules([card(1, -86400_000), card(2)], [live(1)])], [1]);
});
test("explicit null on a known-owner server must not fall back to nearest schedule", () => {
  assert.deepEqual([...matchLiveSchedules([card(1)], [live(null, true)])], []);
});
test("legacy payload with missing ownership still uses bounded matching", () => {
  assert.deepEqual([...matchLiveSchedules([card(1)], [live()])], [1]);
});
test("an unloaded bound card must not highlight another card", () => {
  assert.deepEqual([...matchLiveSchedules([card(1)], [live(2)])], []);
});
test("cancelled or wrong-streamer bindings must not select another card", () => {
  assert.deepEqual([...matchLiveSchedules([card(1, 0, true), card(2)], [live(1)])], []);
  assert.deepEqual([...matchLiveSchedules([card(1, 0, false, 2), card(2)], [live(1)])], []);
});
test("legacy ties are deterministic by earlier time and then ID", () => {
  assert.deepEqual([...matchLiveSchedules([card(3, -60000), card(2, 60000), card(1, -60000)], [live()])], [1]);
});
