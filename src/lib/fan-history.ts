import type { FanSnapshot } from "../types";

export type FanGranularity = "hour" | "day";

export interface FanBucket {
  key: string;
  label: string;
  capturedAt: string;
  fans: number;
  change: number | null;
}

interface LocalParts {
  day: string;
  hour: string;
}

const SHANGHAI_PARTS = new Intl.DateTimeFormat("en-CA", {
  timeZone: "Asia/Shanghai",
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  hourCycle: "h23",
});

function shanghaiParts(value: string): LocalParts | null {
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) {
    return null;
  }
  const values = new Map(
    SHANGHAI_PARTS.formatToParts(date).map((part) => [part.type, part.value]),
  );
  const year = values.get("year");
  const month = values.get("month");
  const day = values.get("day");
  const hour = values.get("hour");
  if (!year || !month || !day || !hour) {
    return null;
  }
  return {
    day: `${year}-${month}-${day}`,
    hour: hour === "24" ? "00" : hour,
  };
}

function makeLabel(parts: LocalParts, granularity: FanGranularity): string {
  const shortDay = parts.day.slice(5);
  return granularity === "hour" ? `${shortDay} ${parts.hour}:00` : shortDay;
}

export function aggregateFanHistory(
  snapshots: readonly FanSnapshot[],
  granularity: FanGranularity,
  limit: number,
): FanBucket[] {
  const latestByBucket = new Map<string, FanSnapshot & LocalParts>();

  for (const snapshot of snapshots) {
    if (!Number.isFinite(snapshot.fans) || snapshot.fans < 0) {
      continue;
    }
    const parts = shanghaiParts(snapshot.capturedAt);
    if (!parts) {
      continue;
    }
    const key = granularity === "hour" ? `${parts.day}T${parts.hour}` : parts.day;
    const existing = latestByBucket.get(key);
    if (!existing || Date.parse(snapshot.capturedAt) > Date.parse(existing.capturedAt)) {
      latestByBucket.set(key, { ...snapshot, ...parts });
    }
  }

  const ordered = [...latestByBucket.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([key, sample]) => ({
      key,
      label: makeLabel(sample, granularity),
      capturedAt: sample.capturedAt,
      fans: sample.fans,
      change: null as number | null,
    }));

  let previousFans: number | null = null;
  for (const bucket of ordered) {
    bucket.change = previousFans === null ? null : bucket.fans - previousFans;
    previousFans = bucket.fans;
  }

  const count = Math.max(1, Math.floor(limit));
  return ordered.slice(-count);
}
