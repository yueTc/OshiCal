import { useEffect, useMemo, useRef, useState } from "react";
import type { DragEvent as ReactDragEvent } from "react";
import {
  BadgeCheck,
  CalendarClock,
  ChevronDown,
  ChevronUp,
  Clock3,
  ExternalLink,
  GripVertical,
  Link2,
  LoaderCircle,
  Plus,
  RefreshCw,
  Search,
  TrendingUp,
  Settings2,
  Trash2,
  UserPlus,
  UsersRound,
  X,
} from "lucide-react";
import type {
  AccountSearchResult,
  FanHistory,
  LinkedAccount,
  RefreshProgress,
  RefreshReport,
  Streamer,
  NewWeeklyScheduleRuleInput,
  UpdateWeeklyScheduleRuleInput,
  UpdateSubscriptionInput,
  WeeklyScheduleRule,
} from "../types";
import { explainError } from "../lib/api";
import { aggregateFanHistory } from "../lib/fan-history";
import type { FanBucket, FanGranularity } from "../lib/fan-history";
import { localDateKey } from "../lib/dates";
import { Avatar, EmptyState, ErrorBanner, Modal, Toggle } from "./ui";

const A_SOUL_OFFICIAL_MID = "703007996";
const A_SOUL_MEMBER_MIDS = new Set(["672342685", "672328094", "672353429"]);
const WEEKDAYS = [
  { bit: 1, label: "一", full: "周一" },
  { bit: 2, label: "二", full: "周二" },
  { bit: 4, label: "三", full: "周三" },
  { bit: 8, label: "四", full: "周四" },
  { bit: 16, label: "五", full: "周五" },
  { bit: 32, label: "六", full: "周六" },
  { bit: 64, label: "日", full: "周日" },
];

interface CreatorActions {
  search: (keyword: string) => Promise<AccountSearchResult[]>;
  add: (account: AccountSearchResult) => Promise<Streamer>;
  update: (id: number, update: UpdateSubscriptionInput) => Promise<Streamer>;
  reorder: (ids: number[]) => Promise<Streamer[]>;
  remove: (id: number) => Promise<void>;
  addLink: (id: number, mid: string) => Promise<unknown>;
  removeLink: (id: number, mid: string) => Promise<void>;
  refresh: (id: number) => Promise<RefreshReport>;
  fanHistory: (streamerId: number, days?: number) => Promise<FanHistory>;
  refreshFans: (streamerId: number) => Promise<FanHistory>;
  listWeeklyRules: (streamerId: number) => Promise<WeeklyScheduleRule[]>;
  createWeeklyRule: (input: NewWeeklyScheduleRuleInput) => Promise<WeeklyScheduleRule>;
  updateWeeklyRule: (id: number, update: UpdateWeeklyScheduleRuleInput) => Promise<WeeklyScheduleRule>;
  deleteWeeklyRule: (id: number) => Promise<void>;
}

interface WeeklyRuleDraft {
  id: number | null;
  ruleKey: string;
  title: string;
  weekdaysMask: number;
  startTime: string;
  endTime: string;
  effectiveFrom: string;
  effectiveUntil: string;
  enabled: boolean;
}

const FAN_COUNT_FORMATTER = new Intl.NumberFormat("zh-CN");
const FAN_COMPACT_FORMATTER = new Intl.NumberFormat("zh-CN", {
  notation: "compact",
  maximumFractionDigits: 1,
});

function formatFanCount(value: number | null | undefined): string {
  return value === null || value === undefined ? "--" : FAN_COMPACT_FORMATTER.format(value);
}

function createRuleKey(): string {
  return globalThis.crypto?.randomUUID?.()
    ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

function toggleKey(id: number, update: UpdateSubscriptionInput): string {
  return `${id}:${Object.keys(update).sort().join(",")}`;
}

function formatFanChange(value: number | null): string {
  if (value === null) {
    return "首次采集";
  }
  return `${value > 0 ? "+" : ""}${FAN_COUNT_FORMATTER.format(value)}`;
}

function FanTrendChart({ buckets }: { buckets: FanBucket[] }) {
  const [activeKey, setActiveKey] = useState<string | null>(null);

  if (buckets.length < 2) {
    return (
      <div className="fan-chart-empty">
        <TrendingUp size={22} />
        <span>再采集 1 次后即可绘制粉丝走势。</span>
      </div>
    );
  }

  const width = 760;
  const height = 230;
  const left = 34;
  const right = 18;
  const top = 18;
  const bottom = 34;
  const plotWidth = width - left - right;
  const plotHeight = height - top - bottom;
  const tooltipWidth = 174;
  const tooltipHeight = 66;
  const values = buckets.map((bucket) => bucket.fans);
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = Math.max(1, max - min);
  const points = buckets.map((bucket, index) => {
    const x = left + (index * plotWidth) / (buckets.length - 1);
    const y = top + ((max - bucket.fans) / span) * (plotHeight - 12) + 6;
    return { ...bucket, x, y };
  });
  const activePoint = activeKey ? points.find((point) => point.key === activeKey) ?? null : null;
  const activeTone = activePoint?.change === null
    ? "flat"
    : (activePoint?.change ?? 0) > 0
      ? "up"
      : (activePoint?.change ?? 0) < 0
        ? "down"
        : "flat";
  const tooltipX = activePoint
    ? Math.max(8, Math.min(width - tooltipWidth - 8, activePoint.x - tooltipWidth / 2))
    : 0;
  const preferredTooltipY = activePoint
    ? activePoint.y - tooltipHeight - 10 >= 8
      ? activePoint.y - tooltipHeight - 10
      : activePoint.y + 14
    : 0;
  const tooltipY = activePoint
    ? Math.min(height - tooltipHeight - 8, Math.max(8, preferredTooltipY))
    : 0;
  const line = points.map((point) => `${point.x.toFixed(2)},${point.y.toFixed(2)}`).join(" ");
  const area = `${left},${top + plotHeight} ${line} ${left + plotWidth},${top + plotHeight}`;
  const labelIndexes = [...new Set([
    0,
    Math.floor((points.length - 1) / 3),
    Math.floor(((points.length - 1) * 2) / 3),
    points.length - 1,
  ])];

  return (
    <div className="fan-chart-wrap">
      <svg
        className="fan-chart"
        viewBox={`0 0 ${width} ${height}`}
        role="img"
        aria-label={`粉丝趋势，从 ${FAN_COUNT_FORMATTER.format(buckets[0].fans)} 变化到 ${FAN_COUNT_FORMATTER.format(buckets[buckets.length - 1].fans)}`}
      >
        <defs>
          <linearGradient id="fan-chart-area" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="var(--primary)" stopOpacity="0.26" />
            <stop offset="100%" stopColor="var(--primary)" stopOpacity="0.02" />
          </linearGradient>
        </defs>
        <line className="fan-chart-grid" x1={left} x2={left + plotWidth} y1={top} y2={top} />
        <line className="fan-chart-grid" x1={left} x2={left + plotWidth} y1={top + plotHeight / 2} y2={top + plotHeight / 2} />
        <line className="fan-chart-grid" x1={left} x2={left + plotWidth} y1={top + plotHeight} y2={top + plotHeight} />
        <polygon className="fan-chart-area" points={area} />
        {activePoint ? (
          <line
            className="fan-chart-guide"
            x1={activePoint.x}
            x2={activePoint.x}
            y1={top}
            y2={top + plotHeight}
          />
        ) : null}
        <polyline className="fan-chart-line" points={line} />
        {activePoint ? (
          <circle className="fan-chart-point-halo" cx={activePoint.x} cy={activePoint.y} r={10} />
        ) : null}
        {points.map((point) => (
          <circle
            className={`fan-chart-point${point.key === activeKey ? " is-active" : ""}`}
            key={point.key}
            cx={point.x}
            cy={point.y}
            r={point.key === activeKey ? 5.5 : 4}
          />
        ))}
        {labelIndexes.map((index) => (
          <text
            className="fan-chart-label"
            key={`label-${points[index].key}`}
            x={points[index].x}
            y={height - 10}
            textAnchor={index === 0 ? "start" : index === points.length - 1 ? "end" : "middle"}
          >
            {points[index].label}
          </text>
        ))}
        {activePoint ? (
          <g
            className="fan-chart-tooltip"
            transform={`translate(${tooltipX.toFixed(2)} ${tooltipY.toFixed(2)})`}
            aria-hidden="true"
          >
            <rect width={tooltipWidth} height={tooltipHeight} rx={10} />
            <text className="fan-chart-tooltip-label" x={12} y={19}>{activePoint.label}</text>
            <text className="fan-chart-tooltip-value" x={12} y={40}>
              粉丝 {FAN_COUNT_FORMATTER.format(activePoint.fans)}
            </text>
            <text className={`fan-chart-tooltip-change ${activeTone}`} x={12} y={57}>
              变化 {formatFanChange(activePoint.change)}
            </text>
          </g>
        ) : null}
        {points.map((point) => (
          <circle
            className="fan-chart-hit"
            key={`hit-${point.key}`}
            cx={point.x}
            cy={point.y}
            r={15}
            tabIndex={0}
            role="img"
            aria-label={`${point.label}，粉丝 ${FAN_COUNT_FORMATTER.format(point.fans)}，变化 ${formatFanChange(point.change)}`}
            onMouseEnter={() => setActiveKey(point.key)}
            onMouseLeave={() => setActiveKey((current) => current === point.key ? null : current)}
            onFocus={() => setActiveKey(point.key)}
            onBlur={() => setActiveKey((current) => current === point.key ? null : current)}
          />
        ))}
      </svg>
    </div>
  );
}
export default function CreatorsView({
  subscriptions,
  refreshProgress,
  liveStatsEnabled,
  actions,
  onChanged,
  onOpenLiveRoom,
  onError,
}: {
  subscriptions: Streamer[];
  refreshProgress: Record<number, RefreshProgress>;
  liveStatsEnabled: boolean;
  actions: CreatorActions;
  onChanged: () => Promise<void>;
  onOpenLiveRoom: (streamer: Streamer) => Promise<void>;
  onError: (message: string) => void;
}) {
  const [keyword, setKeyword] = useState("");
  const [results, setResults] = useState<AccountSearchResult[]>([]);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [hasSearched, setHasSearched] = useState(false);
  const [busy, setBusy] = useState<number | null>(null);
  const addBusyRef = useRef(false);
  const [toggleBusyKeys, setToggleBusyKeys] = useState<Set<string>>(() => new Set());
  const toggleBusyKeysRef = useRef<Set<string>>(new Set());
  const removeBusyRef = useRef<Set<string>>(new Set());
  const [refreshBusy, setRefreshBusy] = useState<number | null>(null);
  const refreshBusyRef = useRef<number | null>(null);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const searchTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const searchRequestSeq = useRef(0);
  const searchCache = useRef(new Map<string, { at: number; results: AccountSearchResult[] }>());
  const [linkTarget, setLinkTarget] = useState<Streamer | null>(null);
  const [linkKeyword, setLinkKeyword] = useState("");
  const [linkResults, setLinkResults] = useState<AccountSearchResult[]>([]);
  const [linkSearching, setLinkSearching] = useState(false);
  const [linkBusyMid, setLinkBusyMid] = useState<string | null>(null);
  const linkSearchSeq = useRef(0);
  const linkTargetIdRef = useRef<number | null>(null);
  const linkQueryRef = useRef("");
  const linkActionBusyRef = useRef(false);
  const [aliasTarget, setAliasTarget] = useState<Streamer | null>(null);
  const [aliasText, setAliasText] = useState("");
  const [aliasSaving, setAliasSaving] = useState(false);
  const aliasRequestSeq = useRef(0);
  const aliasTargetIdRef = useRef<number | null>(null);
  const aliasSavingRef = useRef(false);
  const [weeklyTarget, setWeeklyTarget] = useState<Streamer | null>(null);
  const [weeklyDrafts, setWeeklyDrafts] = useState<WeeklyRuleDraft[]>([]);
  const [weeklyRemovedIds, setWeeklyRemovedIds] = useState<number[]>([]);
  const [weeklyLoading, setWeeklyLoading] = useState(false);
  const [weeklySaving, setWeeklySaving] = useState(false);
  const weeklyRequestSeq = useRef(0);
  const weeklyTargetIdRef = useRef<number | null>(null);
  const weeklySavingRef = useRef(false);
  const [fanTarget, setFanTarget] = useState<Streamer | null>(null);
  const [fanHistory, setFanHistory] = useState<FanHistory | null>(null);
  const [fanGranularity, setFanGranularity] = useState<FanGranularity>("hour");
  const [fanLoading, setFanLoading] = useState(false);
  const [fanRefreshing, setFanRefreshing] = useState(false);
  const [fanError, setFanError] = useState<string | null>(null);
  const fanRequestSeq = useRef(0);
  const fanLoadSeq = useRef(0);
  const fanRefreshSeq = useRef(0);
  const fanTargetIdRef = useRef<number | null>(null);
  const fanRefreshingRef = useRef(false);
  const [reorderBusy, setReorderBusy] = useState(false);
  const reorderBusyRef = useRef(false);
  const [draggingId, setDraggingId] = useState<number | null>(null);
  const draggingIdRef = useRef<number | null>(null);
  const [dropTargetId, setDropTargetId] = useState<number | null>(null);
  const [optimisticOrder, setOptimisticOrder] = useState<number[] | null>(null);

  useEffect(() => {
    setOptimisticOrder(null);
  }, [subscriptions]);

  const orderedSubscriptions = useMemo(() => {
    if (!optimisticOrder) {
      return subscriptions;
    }
    const byId = new Map(subscriptions.map((streamer) => [streamer.id, streamer]));
    const ordered = optimisticOrder.flatMap((id) => {
      const streamer = byId.get(id);
      return streamer ? [streamer] : [];
    });
    return ordered.length === subscriptions.length ? ordered : subscriptions;
  }, [optimisticOrder, subscriptions]);

  const fanBuckets = useMemo(
    () => aggregateFanHistory(
      fanHistory?.snapshots ?? [],
      fanGranularity,
      fanGranularity === "hour" ? 24 : 30,
    ),
    [fanGranularity, fanHistory],
  );
  const fanLatest = fanBuckets.at(-1)?.fans
    ?? fanHistory?.currentFans
    ?? fanTarget?.fans
    ?? null;
  const fanPeriodChange = fanBuckets.length > 1
    ? fanBuckets[fanBuckets.length - 1].fans - fanBuckets[0].fans
    : null;

  function cancelSearchTimer() {
    if (searchTimerRef.current) {
      clearTimeout(searchTimerRef.current);
      searchTimerRef.current = null;
    }
  }

  async function performSearch(rawKeyword: string) {
    const query = rawKeyword.trim();
    if (!query) {
      return;
    }
    cancelSearchTimer();
    const requestId = ++searchRequestSeq.current;
    const cacheKey = query.toLocaleLowerCase();
    const cached = searchCache.current.get(cacheKey);
    if (cached && Date.now() - cached.at < 5 * 60 * 1000) {
      setResults(cached.results);
      setSearchError(null);
      setHasSearched(true);
      setSearching(false);
      return;
    }
    setSearching(true);
    setSearchError(null);
    setHasSearched(false);
    try {
      const nextResults = await actions.search(query);
      if (requestId !== searchRequestSeq.current) {
        return;
      }
      searchCache.current.set(cacheKey, { at: Date.now(), results: nextResults });
      setResults(nextResults);
      setHasSearched(true);
    } catch (error) {
      if (requestId !== searchRequestSeq.current) {
        return;
      }
      setSearchError(explainError(error));
      setResults([]);
      setHasSearched(true);
    } finally {
      if (requestId === searchRequestSeq.current) {
        setSearching(false);
      }
    }
  }

  function handleKeywordChange(value: string) {
    setKeyword(value);
    setHasSearched(false);
    cancelSearchTimer();
    if (!value.trim()) {
      searchRequestSeq.current += 1;
      setResults([]);
      setSearchError(null);
      setSearching(false);
      return;
    }
    searchTimerRef.current = setTimeout(() => {
      void performSearch(value);
    }, 300);
  }

  function clearSearch() {
    cancelSearchTimer();
    searchRequestSeq.current += 1;
    setKeyword("");
    setResults([]);
    setSearchError(null);
    setHasSearched(false);
    setSearching(false);
    searchInputRef.current?.focus();
  }

  useEffect(() => () => {
    if (searchTimerRef.current) {
      clearTimeout(searchTimerRef.current);
    }
  }, []);

  function setToggleBusy(key: string, busy: boolean) {
    const next = new Set(toggleBusyKeysRef.current);
    if (busy) {
      next.add(key);
    } else {
      next.delete(key);
    }
    toggleBusyKeysRef.current = next;
    setToggleBusyKeys(next);
  }

  async function addAccount(account: AccountSearchResult) {
    if (addBusyRef.current) {
      return;
    }
    addBusyRef.current = true;
    setBusy(Number(account.mid));
    try {
      const streamer = await actions.add(account);
      await onChanged();
      void refreshStreamer(streamer);
    } catch (error) {
      onError(explainError(error));
    } finally {
      addBusyRef.current = false;
      setBusy(null);
    }
  }

  async function toggle(streamer: Streamer, update: UpdateSubscriptionInput) {
    const key = toggleKey(streamer.id, update);
    if (toggleBusyKeysRef.current.has(key)) {
      return;
    }
    setToggleBusy(key, true);
    try {
      await actions.update(streamer.id, update);
      await onChanged();
    } catch (error) {
      onError(explainError(error));
    } finally {
      setToggleBusy(key, false);
    }
  }

  async function removeStreamer(streamer: Streamer) {
    const key = `streamer:${streamer.id}`;
    if (removeBusyRef.current.has(key)) {
      return;
    }
    if (!window.confirm(`删除主播「${streamer.name}」？她的日程、官号关联和来源记录会一起删除。`)) {
      return;
    }
    removeBusyRef.current.add(key);
    try {
      await actions.remove(streamer.id);
      await onChanged();
    } catch (error) {
      onError(explainError(error));
    } finally {
      removeBusyRef.current.delete(key);
    }
  }

  async function refreshStreamer(streamer: Streamer) {
    // React state is not synchronous. A rapid double-click can otherwise send
    // two IPC requests before the disabled button is rendered.
    if (refreshBusyRef.current !== null) {
      return;
    }
    refreshBusyRef.current = streamer.id;
    setRefreshBusy(streamer.id);
    try {
      await actions.refresh(streamer.id);
      await onChanged();
    } catch (error) {
      onError(explainError(error));
    } finally {
      refreshBusyRef.current = null;
      setRefreshBusy(null);
    }
  }

  function openLinkDialog(streamer: Streamer) {
    linkSearchSeq.current += 1;
    linkTargetIdRef.current = streamer.id;
    linkQueryRef.current = "";
    setLinkTarget(streamer);
    setLinkKeyword("");
    setLinkResults([]);
    setLinkSearching(false);
  }

  function closeLinkDialog() {
    linkSearchSeq.current += 1;
    linkTargetIdRef.current = null;
    linkQueryRef.current = "";
    setLinkTarget(null);
    setLinkKeyword("");
    setLinkResults([]);
    setLinkSearching(false);
  }

  function updateLinkKeyword(value: string) {
    linkSearchSeq.current += 1;
    linkQueryRef.current = value;
    setLinkKeyword(value);
    setLinkResults([]);
    setLinkSearching(false);
  }

  async function searchLinkAccounts() {
    const target = linkTarget;
    const query = linkKeyword.trim();
    if (!target || !query) {
      return;
    }
    const requestId = ++linkSearchSeq.current;
    linkQueryRef.current = query;
    setLinkSearching(true);
    try {
      const results = await actions.search(query);
      if (
        linkSearchSeq.current !== requestId
        || linkTargetIdRef.current !== target.id
        || linkQueryRef.current.trim() !== query
      ) {
        return;
      }
      setLinkResults(results);
    } catch (error) {
      if (
        linkSearchSeq.current === requestId
        && linkTargetIdRef.current === target.id
        && linkQueryRef.current.trim() === query
      ) {
        onError(explainError(error));
      }
    } finally {
      if (
        linkSearchSeq.current === requestId
        && linkTargetIdRef.current === target.id
        && linkQueryRef.current.trim() === query
      ) {
        setLinkSearching(false);
      }
    }
  }

  async function linkAccount(account: AccountSearchResult) {
    const target = linkTarget;
    if (!target || linkActionBusyRef.current) {
      return;
    }
    const targetId = target.id;
    const requestId = linkSearchSeq.current;
    linkActionBusyRef.current = true;
    setLinkBusyMid(account.mid);
    try {
      await actions.addLink(targetId, account.mid);
      if (linkSearchSeq.current === requestId && linkTargetIdRef.current === targetId) {
        closeLinkDialog();
      }
    } catch (error) {
      if (linkTargetIdRef.current === targetId) {
        onError(explainError(error));
      }
      return;
    } finally {
      linkActionBusyRef.current = false;
      setLinkBusyMid(null);
    }
    try {
      await onChanged();
    } catch (error) {
      onError(`官号已关联，但刷新主播列表失败：${explainError(error)}`);
    }
  }

  async function removeLink(streamer: Streamer, linked: LinkedAccount) {
    const key = `link:${streamer.id}:${linked.mid}`;
    if (removeBusyRef.current.has(key)) {
      return;
    }
    removeBusyRef.current.add(key);
    try {
      await actions.removeLink(streamer.id, linked.mid);
      await onChanged();
    } catch (error) {
      onError(explainError(error));
    } finally {
      removeBusyRef.current.delete(key);
    }
  }

  function openAlias(streamer: Streamer) {
    aliasRequestSeq.current += 1;
    aliasTargetIdRef.current = streamer.id;
    setAliasTarget(streamer);
    setAliasText(streamer.aliases.filter((value) => value !== streamer.name).join("，"));
  }

  function closeAlias() {
    if (aliasSavingRef.current) {
      return;
    }
    aliasRequestSeq.current += 1;
    aliasTargetIdRef.current = null;
    setAliasTarget(null);
  }

  async function saveAlias() {
    const target = aliasTarget;
    if (!target || aliasSavingRef.current) {
      return;
    }
    const targetId = target.id;
    const requestId = ++aliasRequestSeq.current;
    const aliases = aliasText
      .split(/[,，\n]/)
      .map((value) => value.trim())
      .filter(Boolean);
    aliasSavingRef.current = true;
    setAliasSaving(true);
    try {
      await actions.update(targetId, { aliases });
      if (aliasRequestSeq.current === requestId && aliasTargetIdRef.current === targetId) {
        aliasRequestSeq.current += 1;
        aliasTargetIdRef.current = null;
        setAliasTarget(null);
      }
    } catch (error) {
      if (aliasRequestSeq.current === requestId && aliasTargetIdRef.current === targetId) {
        onError(explainError(error));
      }
      return;
    } finally {
      aliasSavingRef.current = false;
      setAliasSaving(false);
    }
    try {
      await onChanged();
    } catch (error) {
      onError(`别名已保存，但刷新主播列表失败：${explainError(error)}`);
    }
  }

  function emptyWeeklyDraft(): WeeklyRuleDraft {
    return {
      id: null,
      ruleKey: createRuleKey(),
      title: "固定直播",
      weekdaysMask: 0,
      startTime: "20:00",
      endTime: "",
      effectiveFrom: localDateKey(new Date()),
      effectiveUntil: "",
      enabled: true,
    };
  }

  async function openWeeklyRules(streamer: Streamer) {
    if (weeklySavingRef.current) {
      return;
    }
    const requestId = ++weeklyRequestSeq.current;
    const targetId = streamer.id;
    weeklyTargetIdRef.current = targetId;
    setWeeklyTarget(streamer);
    setWeeklyDrafts([]);
    setWeeklyRemovedIds([]);
    setWeeklyLoading(true);
    try {
      const rules = await actions.listWeeklyRules(targetId);
      if (weeklyRequestSeq.current !== requestId || weeklyTargetIdRef.current !== targetId) {
        return;
      }
      setWeeklyDrafts(rules.map((rule) => ({
        id: rule.id,
        ruleKey: rule.ruleKey,
        title: rule.title,
        weekdaysMask: rule.weekdaysMask,
        startTime: rule.startTime,
        endTime: rule.endTime ?? "",
        effectiveFrom: rule.effectiveFrom,
        effectiveUntil: rule.effectiveUntil ?? "",
        enabled: rule.enabled,
      })));
    } catch (error) {
      if (weeklyRequestSeq.current === requestId && weeklyTargetIdRef.current === targetId) {
        onError(explainError(error));
      }
    } finally {
      if (weeklyRequestSeq.current === requestId && weeklyTargetIdRef.current === targetId) {
        setWeeklyLoading(false);
      }
    }
  }

  function closeWeeklyRules() {
    if (weeklySavingRef.current) {
      return;
    }
    weeklyRequestSeq.current += 1;
    weeklyTargetIdRef.current = null;
    setWeeklyTarget(null);
    setWeeklyLoading(false);
    setWeeklyRemovedIds([]);
  }

  function addWeeklyDraft() {
    if (!weeklyTarget || weeklySavingRef.current) {
      return;
    }
    setWeeklyDrafts((current) => [...current, emptyWeeklyDraft()]);
  }

  function updateWeeklyDraft(index: number, patch: Partial<WeeklyRuleDraft>) {
    if (weeklySavingRef.current) {
      return;
    }
    setWeeklyDrafts((current) => current.map((draft, draftIndex) => (
      draftIndex === index ? { ...draft, ...patch } : draft
    )));
  }

  function removeWeeklyDraft(index: number) {
    if (weeklySavingRef.current) {
      return;
    }
    setWeeklyDrafts((current) => {
      const draft = current[index];
      if (draft && draft.id !== null) {
        setWeeklyRemovedIds((ids) => ids.includes(draft.id) ? ids : [...ids, draft.id]);
      }
      return current.filter((_, draftIndex) => draftIndex !== index);
    });
  }

  function applySavedWeeklyRule(rule: WeeklyScheduleRule) {
    setWeeklyDrafts((current) => current.map((draft) => (
      draft.ruleKey === rule.ruleKey
        ? {
            ...draft,
            id: rule.id,
            ruleKey: rule.ruleKey,
            title: rule.title,
            weekdaysMask: rule.weekdaysMask,
            startTime: rule.startTime,
            endTime: rule.endTime ?? "",
            effectiveFrom: rule.effectiveFrom,
            effectiveUntil: rule.effectiveUntil ?? "",
            enabled: rule.enabled,
          }
        : draft
    )));
  }

  async function createWeeklyRuleRecoverably(
    target: Streamer,
    draft: WeeklyRuleDraft,
  ): Promise<WeeklyScheduleRule> {
    const ruleKey = draft.ruleKey;
    try {
      return await actions.createWeeklyRule({
        ruleKey,
        streamerId: target.id,
        title: draft.title.trim(),
        weekdaysMask: draft.weekdaysMask,
        startTime: draft.startTime,
        endTime: draft.endTime || null,
        roomUrl: target.liveUrl,
        effectiveFrom: draft.effectiveFrom,
        effectiveUntil: draft.effectiveUntil || null,
        enabled: draft.enabled,
      });
    } catch (createError) {
      // A timed-out create may still have committed on the server. Reconcile
      // by the stable client rule key before surfacing the failure.
      try {
        const existing = (await actions.listWeeklyRules(target.id))
          .find((rule) => rule.ruleKey === ruleKey);
        if (existing) {
          return existing;
        }
      } catch {
        // Preserve the original mutation error when reconciliation is unavailable.
      }
      throw createError;
    }
  }

  async function deleteWeeklyRuleRecoverably(id: number, targetId: number) {
    try {
      await actions.deleteWeeklyRule(id);
    } catch (deleteError) {
      try {
        const remaining = await actions.listWeeklyRules(targetId);
        if (!remaining.some((rule) => rule.id === id)) {
          return;
        }
      } catch {
        // Preserve the original mutation error when reconciliation is unavailable.
      }
      throw deleteError;
    }
  }

  async function saveWeeklyRules() {
    if (!weeklyTarget || weeklySavingRef.current) {
      return;
    }
    if (weeklyDrafts.some((draft) => !draft.title.trim() || draft.weekdaysMask === 0)) {
      onError("每条固定周表都需要填写主题并至少选择一个星期");
      return;
    }
    const target = weeklyTarget;
    const targetId = target.id;
    const removedIds = [...weeklyRemovedIds];
    const drafts = weeklyDrafts.map((draft) => ({ ...draft }));
    weeklySavingRef.current = true;
    setWeeklySaving(true);
    try {
      for (const id of removedIds) {
        await deleteWeeklyRuleRecoverably(id, targetId);
        setWeeklyRemovedIds((ids) => ids.filter((currentId) => currentId !== id));
      }
      for (const draft of drafts) {
        if (draft.id !== null) {
          const saved = await actions.updateWeeklyRule(draft.id, {
            title: draft.title.trim(),
            weekdaysMask: draft.weekdaysMask,
            startTime: draft.startTime,
            endTime: draft.endTime || null,
            clearEndTime: !draft.endTime,
            effectiveFrom: draft.effectiveFrom,
            effectiveUntil: draft.effectiveUntil || null,
            clearEffectiveUntil: !draft.effectiveUntil,
            enabled: draft.enabled,
          });
          applySavedWeeklyRule(saved);
        } else {
          const created = await createWeeklyRuleRecoverably(target, draft);
          applySavedWeeklyRule(created);
        }
      }
    } catch (error) {
      onError(explainError(error));
      return;
    } finally {
      weeklySavingRef.current = false;
      setWeeklySaving(false);
    }

    setWeeklyRemovedIds([]);
    weeklyRequestSeq.current += 1;
    weeklyTargetIdRef.current = null;
    setWeeklyTarget(null);
    try {
      await onChanged();
    } catch (error) {
      onError(`周表已保存，但刷新主播列表失败：${explainError(error)}`);
    }
  }


  async function persistOrder(next: Streamer[]) {
    if (reorderBusyRef.current) {
      return;
    }
    const currentIds = orderedSubscriptions.map((streamer) => streamer.id);
    const nextIds = next.map((streamer) => streamer.id);
    if (currentIds.every((id, index) => id === nextIds[index])) {
      return;
    }
    reorderBusyRef.current = true;
    setOptimisticOrder(nextIds);
    setReorderBusy(true);
    try {
      const saved = await actions.reorder(nextIds);
      setOptimisticOrder(saved.map((streamer) => streamer.id));
      await onChanged();
    } catch (error) {
      setOptimisticOrder(null);
      onError(explainError(error));
    } finally {
      reorderBusyRef.current = false;
      setReorderBusy(false);
      draggingIdRef.current = null;
      setDraggingId(null);
      setDropTargetId(null);
    }
  }

  function moveStreamer(streamerId: number, delta: number) {
    const from = orderedSubscriptions.findIndex((streamer) => streamer.id === streamerId);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= orderedSubscriptions.length) {
      return;
    }
    const next = [...orderedSubscriptions];
    const [moved] = next.splice(from, 1);
    next.splice(to, 0, moved);
    void persistOrder(next);
  }

  function beginReorderDrag(event: ReactDragEvent<HTMLButtonElement>, streamerId: number) {
    if (reorderBusyRef.current) {
      event.preventDefault();
      return;
    }
    event.stopPropagation();
    draggingIdRef.current = streamerId;
    event.dataTransfer.effectAllowed = "move";
    event.dataTransfer.setData("application/x-oshical-streamer-id", String(streamerId));
    event.dataTransfer.setData("text/plain", String(streamerId));
    const card = event.currentTarget.closest(".creator-card");
    if (card instanceof HTMLElement) {
      const rect = card.getBoundingClientRect();
      event.dataTransfer.setDragImage(
        card,
        event.clientX - rect.left,
        event.clientY - rect.top,
      );
    }
    setDraggingId(streamerId);
  }

  function dragOverStreamer(event: ReactDragEvent<HTMLElement>, streamerId: number) {
    const sourceId = draggingIdRef.current;
    if (sourceId === null || sourceId === streamerId) {
      return;
    }
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
    setDropTargetId(streamerId);
  }

  function dropOnStreamer(event: ReactDragEvent<HTMLElement>, streamerId: number) {
    event.preventDefault();
    const rawId =
      event.dataTransfer.getData("application/x-oshical-streamer-id") ||
      event.dataTransfer.getData("text/plain") ||
      String(draggingIdRef.current ?? "");
    const sourceId = Number(rawId);
    draggingIdRef.current = null;
    setDraggingId(null);
    setDropTargetId(null);
    if (!Number.isFinite(sourceId) || sourceId === streamerId) {
      return;
    }
    const from = orderedSubscriptions.findIndex((streamer) => streamer.id === sourceId);
    const to = orderedSubscriptions.findIndex((streamer) => streamer.id === streamerId);
    if (from < 0 || to < 0) {
      return;
    }
    const next = [...orderedSubscriptions];
    const [moved] = next.splice(from, 1);
    next.splice(to, 0, moved);
    void persistOrder(next);
  }

  async function loadFanHistory(streamer: Streamer) {
    const targetVersion = fanRequestSeq.current;
    const requestId = ++fanLoadSeq.current;
    setFanLoading(true);
    setFanError(null);
    try {
      const history = await actions.fanHistory(streamer.id, 30);
      if (
        fanRequestSeq.current !== targetVersion
        || fanLoadSeq.current !== requestId
        || fanTargetIdRef.current !== streamer.id
      ) {
        return;
      }
      setFanHistory(history);
      setFanError(null);
    } catch (error) {
      if (
        fanRequestSeq.current === targetVersion
        && fanLoadSeq.current === requestId
        && fanTargetIdRef.current === streamer.id
      ) {
        const message = explainError(error);
        setFanError(message);
        onError(message);
      }
    } finally {
      if (
        fanRequestSeq.current === targetVersion
        && fanLoadSeq.current === requestId
        && fanTargetIdRef.current === streamer.id
      ) {
        setFanLoading(false);
      }
    }
  }

  function openFanHistory(streamer: Streamer) {
    fanRequestSeq.current += 1;
    fanTargetIdRef.current = streamer.id;
    fanRefreshingRef.current = false;
    setFanTarget(streamer);
    setFanHistory(null);
    setFanError(null);
    setFanRefreshing(false);
    setFanGranularity("hour");
    void loadFanHistory(streamer);
  }

  function closeFanHistory() {
    fanRequestSeq.current += 1;
    fanLoadSeq.current += 1;
    fanRefreshSeq.current += 1;
    fanTargetIdRef.current = null;
    fanRefreshingRef.current = false;
    setFanTarget(null);
    setFanHistory(null);
    setFanError(null);
    setFanLoading(false);
    setFanRefreshing(false);
  }

  async function refreshFanHistory() {
    if (!fanTarget || fanRefreshingRef.current) {
      return;
    }
    const targetId = fanTarget.id;
    const targetVersion = fanRequestSeq.current;
    const requestId = ++fanRefreshSeq.current;
    // A refresh supersedes an in-flight history read, so that read can no
    // longer overwrite the fresher snapshot or strand the loading indicator.
    fanLoadSeq.current += 1;
    setFanLoading(false);
    fanRefreshingRef.current = true;
    setFanRefreshing(true);
    setFanError(null);
    try {
      const history = await actions.refreshFans(targetId);
      if (
        fanRequestSeq.current !== targetVersion
        || fanRefreshSeq.current !== requestId
        || fanTargetIdRef.current !== targetId
      ) {
        return;
      }
      setFanHistory(history);
      setFanError(null);
    } catch (error) {
      if (
        fanRequestSeq.current === targetVersion
        && fanRefreshSeq.current === requestId
        && fanTargetIdRef.current === targetId
      ) {
        const message = explainError(error);
        setFanError(message);
        onError(message);
      }
      return;
    } finally {
      if (
        fanRequestSeq.current === targetVersion
        && fanRefreshSeq.current === requestId
        && fanTargetIdRef.current === targetId
      ) {
        fanRefreshingRef.current = false;
        setFanRefreshing(false);
      }
    }

    try {
      await onChanged();
    } catch (error) {
      onError(`粉丝数据已采集，但刷新主播列表失败：${explainError(error)}`);
    }
  }

  return (
    <section className="page creators-page">
      <header className="page-header">
        <div>
          <h1 className="schedule-title">主播库</h1>
          <p className="page-subtitle">管理日程订阅、开播通知、直播数据和官号来源。</p>
        </div>
        <div className="stat-chip"><UsersRound size={17} />{subscriptions.length} 位主播</div>
      </header>

      <p className="creator-order-hint" role="status">
        <GripVertical size={15} />
        {reorderBusy ? "正在保存主播顺序…" : "拖动左侧手柄或使用上下按钮调整顺序，左侧主播栏会同步变化。"}
      </p>

      <div className="search-panel">
        <div className="search-input">
          <div className="search-field">
            <Search size={18} />
            <input
              ref={searchInputRef}
              value={keyword}
              onChange={(event) => handleKeywordChange(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  cancelSearchTimer();
                  void performSearch(keyword);
                } else if (event.key === "Escape" && keyword) {
                  clearSearch();
                }
              }}
              placeholder="输入 B 站昵称、UID、@昵称或空间链接"
            />
            {keyword ? (
              <button
                type="button"
                className="search-clear"
                aria-label="清空搜索内容"
                title="清空搜索内容"
                onClick={clearSearch}
              >
                <X size={15} />
              </button>
            ) : null}
          </div>
          <button
            className="button primary"
            onClick={() => {
              cancelSearchTimer();
              void performSearch(keyword);
            }}
            disabled={searching || !keyword.trim()}
          >
            {searching ? "搜索中…" : "搜索"}
          </button>
        </div>
        {searchError ? <ErrorBanner message={searchError} onRetry={() => void performSearch(keyword)} /> : null}
        {hasSearched && !searching && !searchError && results.length === 0 ? (
          <div className="search-empty">未找到匹配主播，请尝试完整昵称、UID 或 @昵称。</div>
        ) : null}
        {results.length ? (
          <div className="search-results">
            <div className="search-result-count">找到 {results.length} 位主播</div>
            {results.slice(0, 20).map((account) => {
              const subscribed = subscriptions.some((streamer) => streamer.mid === account.mid);
              return (
                <article className="search-result" key={account.mid}>
                  <Avatar url={account.avatarUrl} name={account.name} size="lg" live={account.isLive} />
                  <div>
                    <h3>{account.name}</h3>
                    <p>UID {account.mid} · {account.fans.toLocaleString()} 粉丝</p>
                    {account.sign ? <p className="result-sign">{account.sign}</p> : null}
                  </div>
                  <button
                    className="button small"
                    onClick={() => void addAccount(account)}
                    disabled={subscribed || busy !== null}
                  >
                    {subscribed ? <><BadgeCheck size={15} />已订阅</> : busy === Number(account.mid) ? "订阅中…" : <><UserPlus size={15} />订阅</>}
                  </button>
                </article>
              );
            })}
          </div>
        ) : null}
      </div>

      {subscriptions.length === 0 ? (
        <EmptyState title="还没有订阅主播" description="在上方搜索 B 站账号并添加。" />
      ) : (
        <div className="creator-list">
          {orderedSubscriptions.map((streamer, index) => (
            <article
              className={`creator-card ${streamer.isLive ? "is-live" : ""} ${draggingId === streamer.id ? "is-dragging" : ""} ${dropTargetId === streamer.id ? "is-drop-target" : ""}`}
              key={streamer.id}
              onDragOver={(event) => dragOverStreamer(event, streamer.id)}
              onDragLeave={() => setDropTargetId((current) => current === streamer.id ? null : current)}
              onDrop={(event) => dropOnStreamer(event, streamer.id)}
            >
              <div className="creator-order-tools">
                <button
                  type="button"
                  className="creator-drag-handle"
                  draggable={!reorderBusy}
                  tabIndex={0}
                  title={`拖动调整 ${streamer.name} 的顺序`}
                  aria-label={`拖动调整 ${streamer.name} 的顺序`}
                  onDragStart={(event) => beginReorderDrag(event, streamer.id)}
                  onDragEnd={() => {
                    draggingIdRef.current = null;
                    setDraggingId(null);
                    setDropTargetId(null);
                  }}
                >
                  <GripVertical size={17} />
                </button>
                <button
                  type="button"
                  className="creator-order-step"
                  title={`上移 ${streamer.name}`}
                  aria-label={`上移 ${streamer.name}`}
                  disabled={reorderBusy || index === 0}
                  onClick={() => moveStreamer(streamer.id, -1)}
                >
                  <ChevronUp size={14} />
                </button>
                <button
                  type="button"
                  className="creator-order-step"
                  title={`下移 ${streamer.name}`}
                  aria-label={`下移 ${streamer.name}`}
                  disabled={reorderBusy || index === orderedSubscriptions.length - 1}
                  onClick={() => moveStreamer(streamer.id, 1)}
                >
                  <ChevronDown size={14} />
                </button>
              </div>
              <Avatar url={streamer.avatarUrl} name={streamer.name} size="lg" live={streamer.isLive} />
              <div className="creator-main">
                <div className="creator-title">
                  <h3>{streamer.name}</h3>
                  <span>UID {streamer.mid}</span>
                  {streamer.isLive ? <span className="live-label">直播中</span> : null}
                </div>
                {streamer.lastError ? <p className="inline-error">{streamer.lastError}</p> : null}
                <div className="creator-toggles">
                  <Toggle
                    checked={streamer.scheduleEnabled}
                    disabled={toggleBusyKeys.has(toggleKey(streamer.id, { scheduleEnabled: true }))}
                    onChange={(value) => void toggle(streamer, { scheduleEnabled: value })}
                    label="订阅日程"
                  />
                  <Toggle
                    checked={streamer.liveNotifyEnabled}
                    disabled={toggleBusyKeys.has(toggleKey(streamer.id, { liveNotifyEnabled: true }))}
                    onChange={(value) => void toggle(streamer, { liveNotifyEnabled: value })}
                    label="开播通知"
                  />
                  {liveStatsEnabled ? (
                    <Toggle
                      checked={streamer.liveStatsEnabled}
                      disabled={toggleBusyKeys.has(toggleKey(streamer.id, { liveStatsEnabled: true }))}
                      onChange={(value) => void toggle(streamer, { liveStatsEnabled: value })}
                      label="订阅直播数据"
                    />
                  ) : null}
                </div>
                {refreshProgress[streamer.id] ? (
                  <div
                    className={`creator-refresh-progress ${refreshProgress[streamer.id].failed ? "is-failed" : ""} ${refreshProgress[streamer.id].done ? "is-done" : ""}`}
                    role="status"
                    aria-live="polite"
                  >
                    <div className="creator-refresh-meta">
                      <span>{refreshProgress[streamer.id].stage}</span>
                      <strong>{refreshProgress[streamer.id].percent}%</strong>
                    </div>
                    <div className="progress-track">
                      <i style={{ width: `${refreshProgress[streamer.id].percent}%` }} />
                    </div>
                  </div>
                ) : null}
                <div className="linked-row">
                  <span className="linked-label"><Link2 size={14} />官号</span>
                  {streamer.linkedAccounts.length ? streamer.linkedAccounts.map((linked) => (
                    <span className="linked-pill" key={linked.mid}>
                      {linked.name}
                      <button aria-label={`移除官号 ${linked.name}`} onClick={() => void removeLink(streamer, linked)}>
                        <X size={12} />
                      </button>
                    </span>
                  )) : A_SOUL_MEMBER_MIDS.has(streamer.mid) ? null : (
                    <span className="linked-empty">未关联</span>
                  )}
                  {A_SOUL_MEMBER_MIDS.has(streamer.mid) &&
                  !streamer.linkedAccounts.some((linked) => linked.mid === A_SOUL_OFFICIAL_MID) ? (
                    <span className="linked-pill automatic" title="A-SOUL 成员会自动抓取该官号">
                      <Link2 size={12} />A-SOUL_Official · 自动
                    </span>
                  ) : null}
                  <button className="text-button" onClick={() => openLinkDialog(streamer)}>
                    <Plus size={13} />关联官号
                  </button>
                </div>
              </div>
              <div className="creator-actions">
                <button
                  type="button"
                  className="creator-fan-button"
                  title={streamer.fans === null ? "尚未采集粉丝数" : `${FAN_COUNT_FORMATTER.format(streamer.fans)} 粉丝，点击查看变化`}
                  onClick={() => openFanHistory(streamer)}
                >
                  <TrendingUp size={14} />
                  <strong>{formatFanCount(streamer.fans)}</strong>
                  <span>粉丝</span>
                  <em>查看趋势</em>
                </button>
                <button
                  className={`icon-button live-room ${streamer.isLive ? "is-live" : ""}`}
                  title={streamer.liveUrl || streamer.roomId
                    ? `进入 ${streamer.name} 的直播间`
                    : `${streamer.name} 暂未配置直播间地址`}
                  aria-label={`进入 ${streamer.name} 的直播间`}
                  onClick={() => void onOpenLiveRoom(streamer)}
                  disabled={!streamer.liveUrl && !streamer.roomId}
                >
                  <ExternalLink size={17} />
                </button>
                <button
                  className="icon-button"
                  title={`设置 ${streamer.name} 的固定周表`}
                  aria-label={`设置 ${streamer.name} 的固定周表`}
                  onClick={() => void openWeeklyRules(streamer)}
                >
                  <CalendarClock size={17} />
                </button>
                <button
                  className="icon-button"
                  title={`重新拉取 ${streamer.name} 的日程`}
                  aria-label={`重新拉取 ${streamer.name} 的日程`}
                  onClick={() => void refreshStreamer(streamer)}
                  disabled={refreshBusy !== null}
                >
                  <RefreshCw className={refreshBusy === streamer.id ? "spin" : ""} size={17} />
                </button>
                <button className="icon-button" title="编辑别名" onClick={() => openAlias(streamer)}><Settings2 size={17} /></button>
                <button className="icon-button danger" title="删除主播" onClick={() => void removeStreamer(streamer)}><Trash2 size={17} /></button>
              </div>
            </article>
          ))}
        </div>
      )}

      {fanTarget ? (
        <Modal title={`粉丝变化 · ${fanTarget.name}`} onClose={closeFanHistory} wide>
          <div className="fan-modal">
            <div className="fan-modal-note">
              <Clock3 size={15} />
              <span>
                默认每小时自动采集一次
                {fanHistory?.checkedAt
                  ? ` · 最近采集 ${new Date(fanHistory.checkedAt).toLocaleString("zh-CN", { hour12: false })}`
                  : " · 等待首次采集"}
              </span>
            </div>
            <div className="fan-stat-grid">
              <div>
                <span>当前粉丝</span>
                <strong>{fanLatest === null ? "--" : FAN_COUNT_FORMATTER.format(fanLatest)}</strong>
              </div>
              <div>
                <span>{fanGranularity === "hour" ? "近 24 小时" : "近 30 天"}净增</span>
                <strong className={`fan-change ${fanPeriodChange === null ? "flat" : fanPeriodChange > 0 ? "up" : fanPeriodChange < 0 ? "down" : "flat"}`}>
                  {fanPeriodChange === null ? "--" : formatFanChange(fanPeriodChange)}
                </strong>
              </div>
              <div>
                <span>统计点</span>
                <strong>{fanBuckets.length}</strong>
              </div>
            </div>
            <div className="fan-toolbar">
              <div className="fan-segmented" role="group" aria-label="粉丝统计粒度">
                <button
                  type="button"
                  className={fanGranularity === "hour" ? "active" : ""}
                  aria-pressed={fanGranularity === "hour"}
                  onClick={() => setFanGranularity("hour")}
                >
                  小时统计
                </button>
                <button
                  type="button"
                  className={fanGranularity === "day" ? "active" : ""}
                  aria-pressed={fanGranularity === "day"}
                  onClick={() => setFanGranularity("day")}
                >
                  日统计
                </button>
              </div>
              <button
                type="button"
                className="button small"
                onClick={() => void refreshFanHistory()}
                disabled={fanRefreshing || fanLoading}
              >
                <RefreshCw className={fanRefreshing ? "spin" : ""} size={14} />
                {fanRefreshing ? "采集中…" : "立即采集"}
              </button>
            </div>
            {fanError ? (
              <ErrorBanner
                message={fanError}
                onRetry={() => {
                  if (fanTarget) {
                    void loadFanHistory(fanTarget);
                  }
                }}
              />
            ) : fanLoading ? (
              <div className="fan-loading" role="status">
                <LoaderCircle className="spin" size={20} />
                正在读取粉丝历史…
              </div>
            ) : fanBuckets.length === 0 ? (
              <div className="fan-chart-empty">
                <TrendingUp size={22} />
                <strong>还没有粉丝快照</strong>
                <span>后台将在启动后及之后每小时自动采集，也可以点击“立即采集”。</span>
              </div>
            ) : (
              <>
                <FanTrendChart buckets={fanBuckets} />
                <div className="fan-history-table" aria-label="粉丝历史统计">
                  <div className="fan-history-row is-head">
                    <span>时间</span>
                    <span>粉丝数</span>
                    <span>变化</span>
                  </div>
                  {[...fanBuckets].reverse().map((bucket) => (
                    <div className="fan-history-row" key={bucket.key}>
                      <span>{bucket.label}</span>
                      <strong>{FAN_COUNT_FORMATTER.format(bucket.fans)}</strong>
                      <em className={bucket.change === null ? "flat" : bucket.change > 0 ? "up" : bucket.change < 0 ? "down" : "flat"}>
                        {formatFanChange(bucket.change)}
                      </em>
                    </div>
                  ))}
                </div>
              </>
            )}
          </div>
        </Modal>
      ) : null}

      {linkTarget ? (
        <Modal title={`为 ${linkTarget.name} 关联官号`} onClose={closeLinkDialog}>
          <div className="search-input compact">
            <div className="search-field">
              <Search size={17} />
              <input
                value={linkKeyword}
                onChange={(event) => updateLinkKeyword(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    void searchLinkAccounts();
                  } else if (event.key === "Escape" && linkKeyword) {
                    updateLinkKeyword("");
                  }
                }}
                placeholder="搜索官号昵称、UID、@昵称或空间链接"
              />
              {linkKeyword ? (
                <button
                  type="button"
                  className="search-clear"
                  aria-label="清空官号搜索"
                  title="清空官号搜索"
                  onClick={() => updateLinkKeyword("")}
                >
                  <X size={15} />
                </button>
              ) : null}
            </div>
            <button
              className="button secondary small"
              onClick={() => void searchLinkAccounts()}
              disabled={linkSearching || !linkKeyword.trim()}
            >
              {linkSearching ? "搜索中…" : "搜索"}
            </button>
          </div>
          <div className="modal-list">
            {linkResults.map((account) => {
              const already = linkTarget.linkedAccounts.some((linked) => linked.mid === account.mid);
              return (
                <div className="modal-list-row" key={account.mid}>
                  <Avatar url={account.avatarUrl} name={account.name} />
                  <div><strong>{account.name}</strong><span>UID {account.mid}</span></div>
                  <button
                    className="button small"
                    disabled={already || linkBusyMid !== null}
                    onClick={() => void linkAccount(account)}
                  >
                    {already ? "已关联" : linkBusyMid === account.mid ? "关联中…" : "关联"}
                  </button>
                </div>
              );
            })}
          </div>
        </Modal>
      ) : null}

      {aliasTarget ? (
        <Modal
          title={`${aliasTarget.name} 的识别别名`}
          onClose={closeAlias}
          footer={
            <>
              <button className="button secondary" onClick={closeAlias} disabled={aliasSaving}>取消</button>
              <button className="button primary" onClick={() => void saveAlias()} disabled={aliasSaving}>
                {aliasSaving ? "保存中…" : "保存"}
              </button>
            </>
          }
        >
          <label className="stacked-field">
            <span>每行或用逗号分隔。图片日程会使用这些别名做唯一匹配。</span>
            <textarea rows={6} value={aliasText} disabled={aliasSaving} onChange={(event) => setAliasText(event.target.value)} />
          </label>
        </Modal>
      ) : null}

      {weeklyTarget ? (
        <Modal
          title={`${weeklyTarget.name} 的固定周表`}
          onClose={closeWeeklyRules}
          wide
          footer={
            <>
              <button className="button secondary" onClick={closeWeeklyRules} disabled={weeklySaving}>取消</button>
              <button className="button primary" onClick={() => void saveWeeklyRules()} disabled={weeklySaving || weeklyLoading}>
                {weeklySaving ? "保存中…" : "保存周表"}
              </button>
            </>
          }
        >
          <div className="weekly-modal-head">
            <Avatar url={weeklyTarget.avatarUrl} name={weeklyTarget.name} />
            <div>
              <strong>每周重复生成日程</strong>
              <span>只影响该主播；未来 12 周自动生成，自动抓取不会覆盖。</span>
            </div>
          </div>
          {weeklyLoading ? <p className="muted">正在读取固定周表…</p> : (
            <div className="weekly-rule-list">
              {weeklyDrafts.map((draft, index) => (
                <article className="weekly-rule-card" key={draft.ruleKey}>
                  <div className="weekly-rule-top">
                    <label className="stacked-field weekly-title-field">
                      <span>直播主题</span>
                      <input
                        value={draft.title}
                        onChange={(event) => updateWeeklyDraft(index, { title: event.target.value })}
                        disabled={weeklySaving}
                        placeholder="例如：固定杂谈"
                      />
                    </label>
                    <label className="weekly-enabled">
                      <input
                        type="checkbox"
                        checked={draft.enabled}
                        onChange={(event) => updateWeeklyDraft(index, { enabled: event.target.checked })}
                        disabled={weeklySaving}
                      />
                      启用
                    </label>
                    <button
                      className="icon-button danger"
                      title="删除这项周表"
                      onClick={() => removeWeeklyDraft(index)}
                      disabled={weeklySaving}
                    >
                      <Trash2 size={16} />
                    </button>
                  </div>
                  <div className="weekday-picker" aria-label="选择星期">
                    {WEEKDAYS.map((weekday) => {
                      const active = (draft.weekdaysMask & weekday.bit) !== 0;
                      return (
                        <button
                          type="button"
                          key={weekday.bit}
                          className={active ? "active" : ""}
                          title={weekday.full}
                          disabled={weeklySaving}
                          onClick={() => updateWeeklyDraft(index, {
                            weekdaysMask: active
                              ? draft.weekdaysMask & ~weekday.bit
                              : draft.weekdaysMask | weekday.bit,
                          })}
                        >
                          {weekday.label}
                        </button>
                      );
                    })}
                  </div>
                  <div className="weekly-time-grid">
                    <label><span>开始</span><input type="time" value={draft.startTime} disabled={weeklySaving} onChange={(event) => updateWeeklyDraft(index, { startTime: event.target.value })} /></label>
                    <label><span>结束（可选）</span><input type="time" value={draft.endTime} disabled={weeklySaving} onChange={(event) => updateWeeklyDraft(index, { endTime: event.target.value })} /></label>
                    <label><span>生效日期</span><input type="date" value={draft.effectiveFrom} disabled={weeklySaving} onChange={(event) => updateWeeklyDraft(index, { effectiveFrom: event.target.value })} /></label>
                    <label><span>结束日期（可选）</span><input type="date" value={draft.effectiveUntil} disabled={weeklySaving} onChange={(event) => updateWeeklyDraft(index, { effectiveUntil: event.target.value })} /></label>
                  </div>
                </article>
              ))}
              {!weeklyDrafts.length ? (
                <div className="weekly-empty">还没有固定周表，点击下方按钮添加一周安排。</div>
              ) : null}
              <button className="button secondary wide" onClick={addWeeklyDraft} disabled={weeklySaving}>
                <Plus size={15} /> 添加一周安排
              </button>
            </div>
          )}
        </Modal>
      ) : null}
    </section>
  );
}







