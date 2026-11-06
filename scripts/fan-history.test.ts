import assert from "node:assert/strict";
import test from "node:test";
import { aggregateFanHistory } from "../src/lib/fan-history.ts";

test("aggregates hourly changes in Asia/Shanghai and keeps the latest sample", () => {
  const buckets = aggregateFanHistory(
    [
      { capturedAt: "2026-11-02T15:30:00.000Z", fans: 100 },
      { capturedAt: "2026-11-02T16:10:00.000Z", fans: 101 },
      { capturedAt: "2026-11-02T16:50:00.000Z", fans: 102 },
      { capturedAt: "2026-11-03T15:30:00.000Z", fans: 110 },
    ],
    "hour",
    24,
  );

  assert.deepEqual(
    buckets.map((bucket) => [bucket.label, bucket.fans, bucket.change]),
    [
      ["11-02 23:00", 100, null],
      ["11-03 00:00", 102, 2],
      ["11-03 23:00", 110, 8],
    ],
  );
});

test("aggregates daily changes across the Shanghai midnight boundary", () => {
  const buckets = aggregateFanHistory(
    [
      { capturedAt: "2026-11-02T15:30:00.000Z", fans: 100 },
      { capturedAt: "2026-11-02T16:10:00.000Z", fans: 101 },
      { capturedAt: "2026-11-03T15:30:00.000Z", fans: 110 },
      { capturedAt: "2026-11-03T16:10:00.000Z", fans: 112 },
    ],
    "day",
    30,
  );

  assert.deepEqual(
    buckets.map((bucket) => [bucket.label, bucket.fans, bucket.change]),
    [
      ["11-02", 100, null],
      ["11-03", 110, 10],
      ["11-04", 112, 2],
    ],
  );
});

test("keeps the previous bucket for delta calculation after limiting the window", () => {
  const buckets = aggregateFanHistory(
    [
      { capturedAt: "2026-11-01T04:00:00.000Z", fans: 10 },
      { capturedAt: "2026-11-02T04:00:00.000Z", fans: 14 },
      { capturedAt: "2026-11-03T04:00:00.000Z", fans: 20 },
    ],
    "day",
    2,
  );

  assert.equal(buckets.length, 2);
  assert.equal(buckets[0].label, "11-02");
  assert.equal(buckets[0].change, 4);
  assert.equal(buckets[1].change, 6);
});

test("ignores invalid timestamps and negative counts", () => {
  const buckets = aggregateFanHistory(
    [
      { capturedAt: "not-a-date", fans: 99 },
      { capturedAt: "2026-11-03T04:00:00.000Z", fans: -1 },
      { capturedAt: "2026-11-03T05:00:00.000Z", fans: 12 },
    ],
    "hour",
    24,
  );

  assert.equal(buckets.length, 1);
  assert.equal(buckets[0].fans, 12);
});
