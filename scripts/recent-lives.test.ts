import assert from "node:assert/strict";
import test from "node:test";
import { RECENT_LIVE_LIMIT, recentLivesForStreamer } from "../src/lib/recent-lives.ts";
import type { LiveSession } from "../src/types.ts";

const session = (id: number, streamerId: number, startedAt: string) => ({
  id, streamerId, startedAt,
}) as LiveSession;

test("recent lives only contain the selected streamer, sorted by actual start time", () => {
  const input = [session(1, 1, "2026-11-10T20:00:00+08:00"),
    session(2, 2, "2026-11-12T20:00:00+08:00"),
    session(3, 1, "2026-11-11T11:00:00Z")];
  assert.deepEqual(recentLivesForStreamer(input, 1).map((value) => value.id), [3, 1]);
  assert.deepEqual(input.map((value) => value.id), [1, 2, 3]);
});

test("equal start times are deterministic by descending session ID", () => {
  const time = "2026-11-11T12:00:00Z";
  assert.deepEqual(recentLivesForStreamer([session(1, 1, time), session(3, 1, time),
    session(2, 1, time)], 1).map((value) => value.id), [3, 2, 1]);
});

test("only the latest fifty selected-streamer sessions are kept", () => {
  const rows = Array.from({ length: 70 }, (_, i) => session(i + 1, 1, new Date(1760000000000 + i * 60000).toISOString()));
  rows.push(session(999, 2, "2026-11-12T12:00:00Z"));
  const result = recentLivesForStreamer(rows, 1);
  assert.equal(result.length, RECENT_LIVE_LIMIT);
  assert.equal(result[0].id, 70);
  assert.equal(result.at(-1)?.id, 21);
});

test("missing or other-streamer history is empty", () => {
  assert.deepEqual(recentLivesForStreamer([], 1), []);
  assert.deepEqual(recentLivesForStreamer([session(1, 2, "2026-11-11T12:00:00Z")], 1), []);
});
