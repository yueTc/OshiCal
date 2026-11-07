import type { FertilitySnapshot, FertilityStatus, Streamer } from "../types";

/**
 * LAPLACE 原始 status → 展示文案与样式类名。
 *
 * 展示层统一使用「月相」这套委婉说法，括号里保留直白解释，
 * 既不冒犯也不至于看不懂。
 */
export interface FertilityStatusMeta {
  label: string;
  emoji: string;
  className: string;
  hint: string;
}

const STATUS_META: Record<string, FertilityStatusMeta> = {
  menstruating: {
    label: "月隐",
    emoji: "🌙",
    className: "is-menstruating",
    hint: "LAPLACE 记录显示正处于月隐期（经期）",
  },
  fertile: {
    label: "花期",
    emoji: "🌸",
    className: "is-fertile",
    hint: "LAPLACE 预测处于花期（易孕窗口）",
  },
  ovulating: {
    label: "月圆",
    emoji: "🌕",
    className: "is-ovulating",
    hint: "LAPLACE 预测处于月圆期（排卵窗口）",
  },
  normal: {
    label: "晴日",
    emoji: "☀️",
    className: "is-normal",
    hint: "LAPLACE 记录显示处于晴日（常规期）",
  },
};

export const UNKNOWN_FERTILITY_META: FertilityStatusMeta = {
  label: "未知",
  emoji: "•",
  className: "is-unknown",
  hint: "LAPLACE 返回了未识别的状态值",
};

export const DISABLED_FERTILITY_META: FertilityStatusMeta = {
  label: "未开启",
  emoji: "🌫️",
  className: "is-disabled",
  hint: "开启「月相」后才会抓取 LAPLACE 数据",
};

export const MISSING_FERTILITY_META: FertilityStatusMeta = {
  label: "未收录",
  emoji: "·",
  className: "is-missing",
  hint: "LAPLACE 目前没有收录这位主播",
};

export function fertilityStatusMeta(status: string | null | undefined): FertilityStatusMeta {
  if (!status) {
    return UNKNOWN_FERTILITY_META;
  }
  return STATUS_META[status.trim().toLowerCase()] ?? UNKNOWN_FERTILITY_META;
}

/** `2026-11-11` → `11月11日`；空值给出「暂未预测」。 */
export function formatNextPeriod(value: string | null | undefined): string {
  if (!value) {
    return "暂未预测";
  }
  const match = /^(\d{4})-(\d{2})-(\d{2})/.exec(value.trim());
  return match ? `${Number(match[2])}月${Number(match[3])}日` : value;
}

/** 主播口径的紧凑标签：`🌙 月隐 · 第 2 天`。 */
export function fertilityCompactLabel(status: FertilityStatus): string {
  const meta = fertilityStatusMeta(status.status);
  const day = status.dayInCycle;
  return day !== null && day !== undefined && day > 0
    ? `${meta.emoji} ${meta.label} · 第 ${day} 天`
    : `${meta.emoji} ${meta.label}`;
}

/**
 * 按主播 UID 命中 LAPLACE 记录；未命中时再看关联官号。
 * 与服务端 `match_fertility_statuses` 保持一致。
 */
export function findFertilityStatus(
  snapshot: FertilitySnapshot | null | undefined,
  streamer: Pick<Streamer, "id" | "mid" | "linkedAccounts">,
): FertilityStatus | null {
  if (!snapshot?.enabled) {
    return null;
  }
  const statuses = snapshot.statuses ?? [];
  const direct = statuses.find(
    (item) => item.streamerId === streamer.id || item.mid === streamer.mid,
  );
  if (direct) {
    return direct;
  }
  const linkedMids = new Set(
    (streamer.linkedAccounts ?? []).map((linked) => linked.mid),
  );
  if (!linkedMids.size) {
    return null;
  }
  return statuses.find((item) => linkedMids.has(item.matchedMid)) ?? null;
}

export function fertilitySourceLabel(source: string | null | undefined): string {
  switch (source) {
    case "laplace":
      return "LAPLACE 直连";
    case "jina-reader":
      return "LAPLACE · Reader 代理";
    case "none":
      return "暂无数据";
    case "disabled":
      return "未开启";
    default:
      return source ? source : "未知来源";
  }
}

export function formatFertilityFetchedAt(value: string | null | undefined): string {
  if (!value) {
    return "未知";
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return "未知";
  }
  return new Intl.DateTimeFormat("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}
