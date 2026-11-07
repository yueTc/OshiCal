import assert from "node:assert/strict";
import test from "node:test";
import {
  DISABLED_FERTILITY_META,
  fertilityCompactLabel,
  fertilitySourceLabel,
  fertilityStatusMeta,
  findFertilityStatus,
  formatNextPeriod,
  UNKNOWN_FERTILITY_META,
} from "../src/lib/fertility.ts";
import type { FertilitySnapshot, FertilityStatus, LinkedAccount, Streamer } from "../src/types.ts";

function status(overrides: Partial<FertilityStatus>): FertilityStatus {
  return {
    streamerId: 1,
    mid: "100",
    streamerName: "主播甲",
    streamerAvatar: "",
    status: "normal",
    dayInCycle: null,
    ovulationDay: null,
    nextPeriod: null,
    cycleLength: null,
    periodLength: null,
    dataPoints: 0,
    matchedMid: "100",
    ...overrides,
  };
}

function snapshot(overrides: Partial<FertilitySnapshot>): FertilitySnapshot {
  return {
    enabled: true,
    fetchedAt: "2026-11-07T10:00:00Z",
    source: "laplace",
    stale: false,
    warning: null,
    totalAvailable: 1638,
    matched: 0,
    statuses: [],
    ...overrides,
  };
}

function streamer(overrides: Partial<Streamer>): Pick<Streamer, "id" | "mid" | "linkedAccounts"> {
  return {
    id: 1,
    mid: "100",
    linkedAccounts: [],
    ...overrides,
  };
}

function link(mid: string): LinkedAccount {
  return { mid, name: `官号 ${mid}`, avatarUrl: "", createdAt: "2026-11-07T10:00:00Z" };
}

test("known statuses map to their own labels and classes", () => {
  for (const [status, label, className] of [
    ["menstruating", "魔法期", "is-menstruating"],
    ["fertile", "易孕期", "is-fertile"],
    ["ovulating", "排卵期", "is-ovulating"],
    ["normal", "安全期", "is-normal"],
  ] as const) {
    const meta = fertilityStatusMeta(status);
    assert.equal(meta.label, label);
    assert.equal(meta.className, className);
  }
});

test("unknown and blank statuses fall back instead of throwing", () => {
  assert.equal(fertilityStatusMeta("MEGA_PERIOD"), UNKNOWN_FERTILITY_META);
  assert.equal(fertilityStatusMeta(""), UNKNOWN_FERTILITY_META);
  assert.equal(fertilityStatusMeta(null), UNKNOWN_FERTILITY_META);
  assert.equal(DISABLED_FERTILITY_META.label, "未开启");
});

test("statuses are matched case-insensitively with surrounding spaces", () => {
  assert.equal(fertilityStatusMeta("  Menstruating ").label, "魔法期");
});

test("next period renders as a short Chinese date", () => {
  assert.equal(formatNextPeriod("2026-11-11"), "11月11日");
  assert.equal(formatNextPeriod("2026-01-05T00:00:00Z"), "1月5日");
  assert.equal(formatNextPeriod(null), "暂未预测");
  assert.equal(formatNextPeriod("unknown"), "unknown");
});

test("compact label includes the cycle day when present", () => {
  assert.equal(
    fertilityCompactLabel(status({ status: "menstruating", dayInCycle: 2 })),
    "🩸 魔法期 · 第 2 天",
  );
  assert.equal(fertilityCompactLabel(status({ status: "normal", dayInCycle: 0 })), "💚 安全期");
  assert.equal(fertilityCompactLabel(status({ status: "normal", dayInCycle: null })), "💚 安全期");
});

test("disabled snapshots never match a streamer", () => {
  const snapshotValue = snapshot({ enabled: false, statuses: [status({})] });
  assert.equal(findFertilityStatus(snapshotValue, streamer({})), null);
  assert.equal(findFertilityStatus(null, streamer({})), null);
});

test("a streamer matches by its own mid first", () => {
  const found = findFertilityStatus(
    snapshot({ statuses: [status({ streamerId: 99, mid: "100", matchedMid: "100" })] }),
    streamer({ id: 1, mid: "100" }),
  );
  assert.equal(found?.matchedMid, "100");
});

test("a streamer falls back to its linked official account", () => {
  const found = findFertilityStatus(
    snapshot({
      statuses: [status({ streamerId: 2, mid: "950", matchedMid: "901", status: "fertile" })],
    }),
    streamer({ id: 1, mid: "950", linkedAccounts: [link("901")] }),
  );
  assert.equal(found?.status, "fertile");
  assert.equal(found?.matchedMid, "901");
});

test("unrecorded streamers return null instead of borrowing another row", () => {
  const found = findFertilityStatus(
    snapshot({ statuses: [status({ streamerId: 2, mid: "950", matchedMid: "901" })] }),
    streamer({ id: 1, mid: "777", linkedAccounts: [link("888")] }),
  );
  assert.equal(found, null);
});

test("source labels stay human readable", () => {
  assert.equal(fertilitySourceLabel("laplace"), "LAPLACE 直连");
  assert.equal(fertilitySourceLabel("jina-reader"), "LAPLACE · Reader 代理");
  assert.equal(fertilitySourceLabel("disabled"), "未开启");
  assert.equal(fertilitySourceLabel(""), "未知来源");
});
