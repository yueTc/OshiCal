import assert from "node:assert/strict";
import test from "node:test";
import {
  clampTimelineScroll,
  timelineAnchorCompensation,
  timelineDragScrollTop,
  visibleTimelineDayKeys,
} from "../src/lib/timeline-layout.ts";

test("visible timeline days include partially clipped edge days", () => {
  const visible = visibleTimelineDayKeys(
    [
      { key: "2026-11-06", top: -80, bottom: 20 },
      { key: "2026-11-07", top: 20, bottom: 180 },
      { key: "2026-11-08", top: 180, bottom: 320 },
      { key: "2026-11-09", top: 320, bottom: 480 },
    ],
    0,
    360,
  );

  assert.deepEqual(visible, ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]);
});

test("visible timeline days ignore a sub-pixel sliver", () => {
  const visible = visibleTimelineDayKeys(
    [
      { key: "2026-11-06", top: -159, bottom: 0.5 },
      { key: "2026-11-07", top: 0.5, bottom: 160 },
      { key: "2026-11-08", top: 160, bottom: 320 },
    ],
    0,
    240,
    2,
  );

  assert.deepEqual(visible, ["2026-11-07", "2026-11-08"]);
});

test("layout changes preserve the current top day offset", () => {
  assert.equal(timelineAnchorCompensation(12, 37), -25);
  assert.equal(timelineAnchorCompensation(37, 12), 25);
  assert.equal(timelineAnchorCompensation(12, 12), 0);
});

test("timeline scroll is clamped to the real scroll range", () => {
  assert.equal(clampTimelineScroll(-20, 900), 0);
  assert.equal(clampTimelineScroll(420, 900), 420);
  assert.equal(clampTimelineScroll(1200, 900), 900);
  assert.equal(clampTimelineScroll(120, 0), 0);
});

test("timeline drag keeps the exact release position without snapping", () => {
  assert.equal(timelineDragScrollTop(500, 37.5, 1200), 462.5);
  assert.equal(timelineDragScrollTop(500, -63.25, 1200), 563.25);
});
