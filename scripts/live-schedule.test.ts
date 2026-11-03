import assert from "node:assert/strict";
import test from "node:test";
import { matchLiveSchedules } from "../src/lib/live-schedule.ts";

function schedule(id: number, streamerId: number, startAt: string, cancelled = false) {
  return { id, streamerId, startAt, cancelled };
}

function live(streamerId: number, startedAt: string | null, isLive = true, checkedAt = "2026-10-29T12:00:00.000Z") {
  return { streamerId, isLive, startedAt, checkedAt };
}

test("only the schedule closest to the live start is marked", () => {
  const matched = matchLiveSchedules(
    [
      schedule(1, 7, "2026-10-29T10:00:00.000Z"),
      schedule(2, 7, "2026-10-29T12:05:00.000Z"),
      schedule(3, 7, "2026-10-30T12:00:00.000Z"),
    ],
    [live(7, "2026-10-29T12:00:00.000Z")],
  );

  assert.deepEqual([...matched], [2]);
});

test("each active streamer gets one nearest schedule", () => {
  const matched = matchLiveSchedules(
    [
      schedule(1, 7, "2026-10-29T09:00:00.000Z"),
      schedule(2, 7, "2026-10-29T12:20:00.000Z"),
      schedule(3, 9, "2026-10-29T11:30:00.000Z"),
      schedule(4, 9, "2026-10-29T15:00:00.000Z"),
    ],
    [
      live(7, "2026-10-29T12:00:00.000Z"),
      live(9, "2026-10-29T15:10:00.000Z"),
    ],
  );

  assert.deepEqual([...matched].sort((left, right) => left - right), [2, 4]);
});

test("uses checked time when the platform has no live start", () => {
  const matched = matchLiveSchedules(
    [
      schedule(1, 7, "2026-10-29T08:00:00.000Z"),
      schedule(2, 7, "2026-10-29T12:10:00.000Z"),
    ],
    [live(7, null, true, "2026-10-29T12:00:00.000Z")],
  );

  assert.deepEqual([...matched], [2]);
});

test("ignores inactive streams and cancelled schedules", () => {
  const matched = matchLiveSchedules(
    [
      schedule(1, 7, "2026-10-29T12:00:00.000Z", true),
      schedule(2, 7, "2026-10-29T13:00:00.000Z"),
      schedule(3, 8, "2026-10-29T12:00:00.000Z"),
    ],
    [
      live(7, "2026-10-29T12:00:00.000Z"),
      live(8, "2026-10-29T12:00:00.000Z", false),
    ],
  );

  assert.deepEqual([...matched], [2]);
});
