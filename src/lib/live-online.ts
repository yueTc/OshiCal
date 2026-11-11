import type {
  LiveOnlineMetric, LiveOnlineSample, LiveOnlineSamplesPage, LiveSession, Platform,
} from "../types";

export type OnlinePageLoader = (
  sessionId: number, afterId: number, signal?: AbortSignal,
) => Promise<LiveOnlineSamplesPage>;

const ONLINE_METRICS: ReadonlySet<LiveOnlineMetric> =
  new Set<LiveOnlineMetric>([
    "bilibili_high_energy_users",
    "youtube_concurrent_viewers",
  ]);

export function platformForMetric(metric: LiveOnlineMetric): Platform {
  return metric === "youtube_concurrent_viewers" ? "youtube" : "bilibili";
}

/**
 * Prefer the explicit projection supplied by newer transports. Older payloads
 * have no platform field, so retain the room-id fallback in one place: Bilibili
 * room IDs are numeric while YouTube uses channel/video IDs.
 */
export function platformFromLiveSession(
  session: Pick<LiveSession, "roomId" | "platform">,
): Platform {
  if (session.platform === "youtube" || session.platform === "bilibili") {
    return session.platform;
  }
  const roomId = session.roomId.trim();
  return roomId !== "" && !/^\d+$/.test(roomId) ? "youtube" : "bilibili";
}

/** Read every page, never silently label a partial first page as a whole session. */
export async function loadOnlineSamplePages(
  loadPage: OnlinePageLoader,
  sessionId: number,
  afterId = 0,
  signal?: AbortSignal,
): Promise<{
  samples: LiveOnlineSample[];
  nextAfterId: number;
  platform: Platform;
  metric: LiveOnlineMetric;
}> {
  const samples: LiveOnlineSample[] = [];
  let cursor = afterId;
  let resultPlatform: Platform | null = null;
  let resultMetric: LiveOnlineMetric | null = null;
  for (let pageIndex = 0; pageIndex < 1000; pageIndex += 1) {
    if (signal?.aborted) throw new Error("人数曲线请求已取消");
    const page = await loadPage(sessionId, cursor, signal);
    if (signal?.aborted) throw new Error("人数曲线请求已取消");
    if (page.sessionId !== sessionId || !ONLINE_METRICS.has(page.metric)
      || !Array.isArray(page.samples) || typeof page.hasMore !== "boolean"
      || !Number.isSafeInteger(page.nextAfterId) || page.nextAfterId < cursor) {
      throw new Error("人数曲线响应格式或统计口径不匹配");
    }
    if (page.platform !== undefined && page.platform !== "bilibili" && page.platform !== "youtube") {
      throw new Error("人数曲线平台字段无效");
    }
    const metricPlatform = platformForMetric(page.metric);
    const pagePlatform = page.platform ?? metricPlatform;
    if (pagePlatform !== metricPlatform) {
      throw new Error("人数曲线平台与统计口径不一致");
    }
    if ((resultMetric !== null && resultMetric !== page.metric)
      || (resultPlatform !== null && resultPlatform !== pagePlatform)) {
      throw new Error("人数曲线分页统计口径前后不一致");
    }
    resultMetric = page.metric;
    resultPlatform = pagePlatform;
    let lastId = cursor;
    for (const sample of page.samples) {
      if (sample.sessionId !== sessionId || !Number.isSafeInteger(sample.id) || sample.id <= lastId) {
        throw new Error("人数曲线分页顺序异常，请重试");
      }
      lastId = sample.id;
    }
    if (lastId !== page.nextAfterId || (page.hasMore && lastId === cursor)) {
      throw new Error("人数曲线分页游标未正确推进，请重试");
    }
    samples.push(...page.samples);
    cursor = lastId;
    if (!page.hasMore) {
      if (resultPlatform === null || resultMetric === null) {
        throw new Error("人数曲线响应缺少统计口径");
      }
      return {
        samples,
        nextAfterId: cursor,
        platform: resultPlatform,
        metric: resultMetric,
      };
    }
  }
  throw new Error("本场人数历史过长，暂时无法完整读取，请稍后重试");
}

export interface OnlinePoint {
  id: number;
  time: number;
  observedAt: string | null;
  value: number | null;
}

export function onlinePoints(samples: readonly LiveOnlineSample[]): OnlinePoint[] {
  return samples.filter((sample) => Number.isFinite(Date.parse(sample.sampledAt)))
    .map((sample) => ({
      id: sample.id,
      time: Date.parse(sample.sampledAt),
      observedAt: sample.observedAt,
      value: sample.collectorState !== "reconnecting"
        && sample.observedAt !== null && Number.isFinite(Date.parse(sample.observedAt))
        && Date.parse(sample.observedAt) <= Date.parse(sample.sampledAt)
        && typeof sample.highEnergyCount === "number"
        && Number.isSafeInteger(sample.highEnergyCount) && sample.highEnergyCount >= 0
        ? sample.highEnergyCount : null,
    }))
    .sort((a, b) => a.time - b.time || a.id - b.id);
}

/** Null checkpoints and long recording gaps both split the curve. */
export function onlineSegments(points: readonly OnlinePoint[], intervalSeconds = 30): OnlinePoint[][] {
  const segments: OnlinePoint[][] = [];
  let current: OnlinePoint[] = [];
  let previous: OnlinePoint | undefined;
  for (const point of points) {
    if (point.value === null || (previous && point.time - previous.time > intervalSeconds * 2500)) {
      if (current.length) segments.push(current);
      current = [];
    }
    if (point.value !== null) current.push(point);
    previous = point;
  }
  if (current.length) segments.push(current);
  return segments;
}

/** Bound SVG path complexity, retaining the first/last and each bucket's extrema. */
export function thinOnlineSegment(points: readonly OnlinePoint[], budget = 800): OnlinePoint[] {
  if (points.length <= budget) return [...points];
  const bucketSize = Math.ceil(points.length / Math.max(1, Math.floor(budget / 4)));
  const result: OnlinePoint[] = [];
  for (let start = 0; start < points.length; start += bucketSize) {
    const bucket = points.slice(start, start + bucketSize);
    let min = 0;
    let max = 0;
    bucket.forEach((point, index) => {
      if ((point.value ?? 0) < (bucket[min].value ?? 0)) min = index;
      if ((point.value ?? 0) > (bucket[max].value ?? 0)) max = index;
    });
    for (const index of [...new Set([0, min, max, bucket.length - 1])].sort((a, b) => a - b)) {
      result.push(bucket[index]);
    }
  }
  return result;
}

const TIME_FORMAT = new Intl.DateTimeFormat("zh-CN", {
  timeZone: "Asia/Shanghai", month: "2-digit", day: "2-digit",
  hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23",
});
export function onlineTime(time: number): string {
  return TIME_FORMAT.format(new Date(time));
}

export function nearestOnlinePoint(points: readonly OnlinePoint[], time: number): number {
  if (!points.length) return -1;
  let low = 0;
  let high = points.length - 1;
  while (low < high) {
    const mid = Math.floor((low + high) / 2);
    if (points[mid].time < time) low = mid + 1;
    else high = mid;
  }
  return low > 0 && time - points[low - 1].time <= points[low].time - time ? low - 1 : low;
}
