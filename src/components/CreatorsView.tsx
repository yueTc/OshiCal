import { useEffect, useMemo, useRef, useState } from "react";
import type {
  DragEvent as ReactDragEvent,
  KeyboardEvent as ReactKeyboardEvent,
} from "react";
import {
  BadgeCheck,
  CalendarClock,
  ChevronDown,
  ChevronUp,
  Clock3,
  ExternalLink,
  GripVertical,
  Link2,
  MoreHorizontal,
  History,
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
  FertilitySnapshot,
  LinkedAccount,
  Platform,
  RefreshProgress,
  RefreshReport,
  Streamer,
  NewWeeklyScheduleRuleInput,
  UpdateWeeklyScheduleRuleInput,
  UpdateSubscriptionInput,
  WeeklyScheduleRule,
} from "../types";
import { explainError, openExternalUrl } from "../lib/api";
import { aggregateFanHistory } from "../lib/fan-history";
import type { FanBucket, FanGranularity } from "../lib/fan-history";
import { useStickyHeader } from "../lib/sticky-header";
import { captureMenuScroll } from "../lib/menu-scroll";
import {
  fertilityCompactLabel,
  fertilitySourceLabel,
  fertilityStatusMeta,
  findFertilityStatus,
  formatFertilityFetchedAt,
  formatNextPeriod,
} from "../lib/fertility";
import { localDateKey } from "../lib/dates";
import { Avatar, EmptyState, ErrorBanner, Modal, Toggle } from "./ui";
import { CreatorRecentLives, type RecentLiveActions } from "./CreatorRecentLives";

const A_SOUL_OFFICIAL_MID = "703007996";
const A_SOUL_MEMBER_MIDS = new Set(["672342685", "672328094", "672353429"]);

function streamerPlatform(streamer: Pick<Streamer, "platform">): Platform {
  return streamer.platform === "youtube" ? "youtube" : "bilibili";
}

function accountPlatform(account: Pick<AccountSearchResult, "platform">): Platform {
  return account.platform === "youtube" ? "youtube" : "bilibili";
}

function accountKey(platform: Platform, mid: string): string {
  return `${platform}:${mid}`;
}

function looksLikeYoutubeAccountInput(value: string): boolean {
  const input = value.trim();
  if (!input) {
    return false;
  }
  if (
    input.startsWith("@")
    || /^UC[A-Za-z0-9_-]{22}$/.test(input)
    || /^(?:(?:www|m|music)\.)?youtube\.com\//i.test(input)
    || /^youtu\.be\//i.test(input)
  ) {
    return true;
  }
  try {
    const url = new URL(input);
    return (url.protocol === "http:" || url.protocol === "https:")
      && (url.hostname === "youtu.be" || url.hostname === "youtube.com" || url.hostname.endsWith(".youtube.com"));
  } catch {
    return false;
  }
}

function PlatformBadge({ platform }: { platform: Platform }) {
  return (
    <span className={`platform-badge ${platform}`}>
      {platform === "youtube" ? "YouTube" : "Bilibili"}
    </span>
  );
}
const WEEKDAYS = [
  { bit: 1, label: "一", full: "周一" },
  { bit: 2, label: "二", full: "周二" },
  { bit: 4, label: "三", full: "周三" },
  { bit: 8, label: "四", full: "周四" },
  { bit: 16, label: "五", full: "周五" },
  { bit: 32, label: "六", full: "周六" },
  { bit: 64, label: "日", full: "周日" },
];

interface CreatorActions extends RecentLiveActions {
  search: (keyword: string, platform: Platform) => Promise<AccountSearchResult[]>;
  add: (account: AccountSearchResult) => Promise<Streamer>;
  update: (id: number, update: UpdateSubscriptionInput) => Promise<Streamer>;
  reorder: (ids: number[]) => Promise<Streamer[]>;
  remove: (id: number) => Promise<void>;
  addLink: (id: number, platform: Platform, mid: string) => Promise<unknown>;
  removeLink: (id: number, platform: Platform, mid: string) => Promise<void>;
  refresh: (id: number) => Promise<RefreshReport>;
  fanHistory: (streamerId: number, days?: number) => Promise<FanHistory>;
  refreshFans: (streamerId: number) => Promise<FanHistory>;
  fertility: (refresh: boolean) => Promise<FertilitySnapshot>;
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
  fertilityEnabled,
  youtubeEnabled,
  actions,
  onChanged,
  onOpenLiveRoom,
  onError,
}: {
  subscriptions: Streamer[];
  refreshProgress: Record<number, RefreshProgress>;
  liveStatsEnabled: boolean;
  fertilityEnabled: boolean;
  youtubeEnabled: boolean;
  actions: CreatorActions;
  onChanged: () => Promise<void>;
  onOpenLiveRoom: (streamer: Streamer) => Promise<void>;
  onError: (message: string) => void;
}) {
  const [platform, setPlatform] = useState<Platform>("bilibili");
  const [keyword, setKeyword] = useState("");
  const [results, setResults] = useState<AccountSearchResult[]>([]);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [hasSearched, setHasSearched] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
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
  const [linkPlatform, setLinkPlatform] = useState<Platform>("bilibili");
  const linkPlatformRef = useRef<Platform>("bilibili");
  const [linkKeyword, setLinkKeyword] = useState("");
  const [linkResults, setLinkResults] = useState<AccountSearchResult[]>([]);
  const [linkSearching, setLinkSearching] = useState(false);
  const [linkBusyMid, setLinkBusyMid] = useState<string | null>(null);
  const linkSearchSeq = useRef(0);
  const linkTargetIdRef = useRef<number | null>(null);
  const linkQueryRef = useRef("");
  const linkActionBusyRef = useRef(false);
  const [moreMenu, setMoreMenu] = useState<{ streamer: Streamer; x: number; y: number } | null>(null);
  const moreMenuRef = useRef<HTMLDivElement>(null);
  const moreTriggerRef = useRef<HTMLButtonElement | null>(null);
  const moreScrollChangedRef = useRef<() => boolean>(() => false);
  const [recentTarget, setRecentTarget] = useState<Streamer | null>(null);
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
  const [fertilitySnapshot, setFertilitySnapshot] = useState<FertilitySnapshot | null>(null);
  const [fertilityLoading, setFertilityLoading] = useState(false);
  const [fertilityRefreshing, setFertilityRefreshing] = useState(false);
  const [fertilityError, setFertilityError] = useState<string | null>(null);
  const [fertilityTarget, setFertilityTarget] = useState<Streamer | null>(null);
  const fertilityRequestSeq = useRef(0);
  const fertilityRefreshingRef = useRef(false);
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
  const { stickyRef, stickyPinned } = useStickyHeader<HTMLDivElement>();
  // Every visible gate and handler uses this derived value. It closes the
  // one-render window where persisted state could still say "youtube" after the
  // feature switch has already been turned off.
  const currentPlatform: Platform = youtubeEnabled ? platform : "bilibili";
  const visibleResults = useMemo(
    () => results.filter((account) => accountPlatform(account) === currentPlatform),
    [currentPlatform, results],
  );

  useEffect(() => {
    setOptimisticOrder(null);
  }, [subscriptions]);

  useEffect(() => {
    if (youtubeEnabled) {
      return;
    }
    // Disabling the feature must also close entry points that are already open,
    // especially a recent-lives/statistics dialog owned by a hidden YT card.
    if (linkTarget && streamerPlatform(linkTarget) === "youtube") {
      linkSearchSeq.current += 1;
      linkTargetIdRef.current = null;
      linkQueryRef.current = "";
      linkActionBusyRef.current = false;
      setLinkTarget(null);
      setLinkPlatform("bilibili");
      linkPlatformRef.current = "bilibili";
      setLinkKeyword("");
      setLinkResults([]);
      setLinkSearching(false);
      setLinkBusyMid(null);
    }
    if (moreMenu && streamerPlatform(moreMenu.streamer) === "youtube") {
      setMoreMenu(null);
    }
    if (recentTarget && streamerPlatform(recentTarget) === "youtube") {
      setRecentTarget(null);
    }
    if (aliasTarget && streamerPlatform(aliasTarget) === "youtube") {
      aliasRequestSeq.current += 1;
      aliasTargetIdRef.current = null;
      setAliasTarget(null);
      setAliasText("");
    }
    if (weeklyTarget && streamerPlatform(weeklyTarget) === "youtube") {
      weeklyRequestSeq.current += 1;
      weeklyTargetIdRef.current = null;
      setWeeklyTarget(null);
      setWeeklyDrafts([]);
      setWeeklyRemovedIds([]);
    }
    if (fanTarget && streamerPlatform(fanTarget) === "youtube") {
      fanRequestSeq.current += 1;
      fanLoadSeq.current += 1;
      fanRefreshSeq.current += 1;
      fanTargetIdRef.current = null;
      setFanTarget(null);
      setFanHistory(null);
    }
    if (fertilityTarget && streamerPlatform(fertilityTarget) === "youtube") {
      setFertilityTarget(null);
    }
  }, [
    aliasTarget,
    fanTarget,
    fertilityTarget,
    linkTarget,
    moreMenu,
    recentTarget,
    weeklyTarget,
    youtubeEnabled,
  ]);

  useEffect(() => {
    if (youtubeEnabled || platform === "bilibili") {
      return;
    }
    cancelSearchTimer();
    searchRequestSeq.current += 1;
    setPlatform("bilibili");
    setKeyword("");
    setResults([]);
    setSearchError(null);
    setHasSearched(false);
    setSearching(false);
  }, [platform, youtubeEnabled]);

  useEffect(() => {
    fertilityRequestSeq.current += 1;
    const requestId = fertilityRequestSeq.current;
    if (!fertilityEnabled) {
      // 关闭后立刻清空，避免界面继续显示上次抓取的周期数据。
      setFertilitySnapshot(null);
      setFertilityError(null);
      setFertilityLoading(false);
      setFertilityTarget(null);
      return;
    }
    setFertilityLoading(true);
    setFertilityError(null);
    void (async () => {
      try {
        const snapshot = await actions.fertility(false);
        if (fertilityRequestSeq.current !== requestId) {
          return;
        }
        setFertilitySnapshot(snapshot);
      } catch (error) {
        if (fertilityRequestSeq.current === requestId) {
          setFertilityError(explainError(error));
        }
      } finally {
        if (fertilityRequestSeq.current === requestId) {
          setFertilityLoading(false);
        }
      }
    })();
  }, [actions, fertilityEnabled]);

  function closeMoreMenu(returnFocus = false) {
    setMoreMenu(null);
    if (returnFocus) moreTriggerRef.current?.focus();
  }

  useEffect(() => {
    if (!moreMenu) return;
    const menu = moreMenuRef.current;
    if (menu) {
      const bounds = menu.getBoundingClientRect();
      menu.style.left = `${Math.max(8, Math.min(moreMenu.x, window.innerWidth - bounds.width - 8))}px`;
      menu.style.top = `${Math.max(8, Math.min(moreMenu.y, window.innerHeight - bounds.height - 8))}px`;
      menu.querySelector<HTMLButtonElement>("button")?.focus();
    }
    const dismiss = (event: PointerEvent) => {
      if (event.target instanceof Node && (menu?.contains(event.target) || moreTriggerRef.current?.contains(event.target))) return;
      setMoreMenu(null);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setMoreMenu(null);
        moreTriggerRef.current?.focus();
      }
    };
    const dismissOnMove = () => setMoreMenu(null);
    const dismissOnScroll = () => {
      if (moreScrollChangedRef.current()) setMoreMenu(null);
    };
    window.addEventListener("pointerdown", dismiss);
    window.addEventListener("keydown", escape);
    window.addEventListener("resize", dismissOnMove);
    window.addEventListener("scroll", dismissOnScroll, true);
    return () => {
      window.removeEventListener("pointerdown", dismiss);
      window.removeEventListener("keydown", escape);
      window.removeEventListener("resize", dismissOnMove);
      window.removeEventListener("scroll", dismissOnScroll, true);
    };
  }, [moreMenu]);

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
  const platformSubscriptions = useMemo(
    () => orderedSubscriptions.filter((streamer) => (
      streamerPlatform(streamer) === currentPlatform
    )),
    [currentPlatform, orderedSubscriptions],
  );

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
  const fanTargetLabel = fanTarget && streamerPlatform(fanTarget) === "youtube"
    ? "订阅者"
    : "粉丝";

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
    const searchPlatform = currentPlatform;
    if (searchPlatform === "youtube" && !youtubeEnabled) {
      setResults([]);
      setSearchError("YouTube 功能未开启");
      setHasSearched(true);
      return;
    }
    const requestId = ++searchRequestSeq.current;
    const cacheKey = `${searchPlatform}:${query.toLocaleLowerCase()}`;
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
      const nextResults = await actions.search(query, searchPlatform);
      if (requestId !== searchRequestSeq.current || searchPlatform !== currentPlatform) {
        return;
      }
      const platformResults = nextResults.filter((account) => accountPlatform(account) === searchPlatform);
      searchCache.current.set(cacheKey, { at: Date.now(), results: platformResults });
      setResults(platformResults);
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
    if (currentPlatform === "youtube") {
      // YouTube search.list 使用独立且昂贵的配额，只允许显式提交。
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

  function selectPlatform(next: Platform) {
    if (next === currentPlatform || (next === "youtube" && !youtubeEnabled)) {
      return;
    }
    cancelSearchTimer();
    searchRequestSeq.current += 1;
    setPlatform(next);
    setKeyword("");
    setResults([]);
    setSearchError(null);
    setHasSearched(false);
    setSearching(false);
    searchInputRef.current?.focus();
  }

  function handlePlatformTabKeyDown(
    event: ReactKeyboardEvent<HTMLButtonElement>,
    current: Platform,
  ) {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) {
      return;
    }
    event.preventDefault();
    const order: Platform[] = ["bilibili", "youtube"];
    const currentIndex = order.indexOf(current);
    const nextIndex = event.key === "Home"
      ? 0
      : event.key === "End"
        ? order.length - 1
        : (currentIndex + (event.key === "ArrowRight" ? 1 : -1) + order.length) % order.length;
    const next = order[nextIndex];
    selectPlatform(next);
    const tablist = event.currentTarget.parentElement;
    requestAnimationFrame(() => {
      tablist
        ?.querySelectorAll<HTMLButtonElement>('[role="tab"]')
        .item(nextIndex)
        ?.focus();
    });
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
    if (accountPlatform(account) === "youtube" && !youtubeEnabled) {
      onError("YouTube 功能未开启");
      return;
    }
    addBusyRef.current = true;
    setBusy(accountKey(accountPlatform(account), account.mid));
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

  async function addYoutubeInput() {
    const input = keyword.trim();
    if (!youtubeEnabled || currentPlatform !== "youtube" || !input || !looksLikeYoutubeAccountInput(input) || addBusyRef.current) {
      return;
    }
    addBusyRef.current = true;
    const busyKey = accountKey("youtube", input);
    setBusy(busyKey);
    try {
      const streamer = await actions.add({
        platform: "youtube",
        mid: input,
        name: input,
        avatarUrl: "",
        sign: "",
        roomId: null,
        fans: 0,
        isLive: false,
      });
      await onChanged();
      clearSearch();
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
    if (streamerPlatform(streamer) === "youtube" && !youtubeEnabled) {
      onError("YouTube 功能未开启");
      return;
    }
    linkSearchSeq.current += 1;
    linkTargetIdRef.current = streamer.id;
    linkQueryRef.current = "";
    setLinkTarget(streamer);
    const platform = streamerPlatform(streamer);
    linkPlatformRef.current = platform;
    setLinkPlatform(platform);
    setLinkKeyword("");
    setLinkResults([]);
    setLinkSearching(false);
  }

  function closeLinkDialog() {
    linkSearchSeq.current += 1;
    linkTargetIdRef.current = null;
    linkQueryRef.current = "";
    setLinkTarget(null);
    linkPlatformRef.current = "bilibili";
    setLinkPlatform("bilibili");
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
    const searchPlatform = linkPlatform;
    if (searchPlatform === "youtube" && !youtubeEnabled) {
      onError("YouTube 功能未开启");
      return;
    }
    linkQueryRef.current = query;
    setLinkSearching(true);
    try {
      const results = await actions.search(query, searchPlatform);
      if (
        linkSearchSeq.current !== requestId
        || linkTargetIdRef.current !== target.id
        || linkPlatformRef.current !== searchPlatform
        || linkQueryRef.current.trim() !== query
      ) {
        return;
      }
      setLinkResults(results.filter((account) => accountPlatform(account) === searchPlatform));
    } catch (error) {
      if (
        linkSearchSeq.current === requestId
        && linkTargetIdRef.current === target.id
        && linkPlatformRef.current === searchPlatform
        && linkQueryRef.current.trim() === query
      ) {
        onError(explainError(error));
      }
    } finally {
      if (
        linkSearchSeq.current === requestId
        && linkTargetIdRef.current === target.id
        && linkPlatformRef.current === searchPlatform
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
    const accountPlatformValue = accountPlatform(account);
    if (accountPlatformValue !== linkPlatform || (accountPlatformValue === "youtube" && !youtubeEnabled)) {
      onError(accountPlatformValue === "youtube" && !youtubeEnabled
        ? "YouTube 功能未开启"
        : "只能关联同一平台的频道");
      return;
    }
    linkActionBusyRef.current = true;
    setLinkBusyMid(account.mid);
    try {
      await actions.addLink(targetId, accountPlatformValue, account.mid);
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
    const linkedPlatform = linked.platform === "youtube" ? "youtube" : streamerPlatform(streamer);
    if (linkedPlatform === "youtube" && !youtubeEnabled) {
      onError("YouTube 功能未开启");
      return;
    }
    const key = `link:${streamer.id}:${linkedPlatform}:${linked.mid}`;
    if (removeBusyRef.current.has(key)) {
      return;
    }
    removeBusyRef.current.add(key);
    try {
      await actions.removeLink(streamer.id, linkedPlatform, linked.mid);
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
        const removedId = draft.id;
        setWeeklyRemovedIds((ids) => ids.includes(removedId) ? ids : [...ids, removedId]);
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

  function mergePlatformOrder(nextPlatformOrder: Streamer[]): Streamer[] {
    const visibleIds = new Set(nextPlatformOrder.map((streamer) => streamer.id));
    let cursor = 0;
    return orderedSubscriptions.map((streamer) => (
      visibleIds.has(streamer.id) ? nextPlatformOrder[cursor++] : streamer
    ));
  }

  function moveStreamer(streamerId: number, delta: number) {
    const from = platformSubscriptions.findIndex((streamer) => streamer.id === streamerId);
    const to = from + delta;
    if (from < 0 || to < 0 || to >= platformSubscriptions.length) {
      return;
    }
    const next = [...platformSubscriptions];
    const [moved] = next.splice(from, 1);
    next.splice(to, 0, moved);
    void persistOrder(mergePlatformOrder(next));
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
    const from = platformSubscriptions.findIndex((streamer) => streamer.id === sourceId);
    const to = platformSubscriptions.findIndex((streamer) => streamer.id === streamerId);
    if (from < 0 || to < 0) {
      return;
    }
    const next = [...platformSubscriptions];
    const [moved] = next.splice(from, 1);
    next.splice(to, 0, moved);
    void persistOrder(mergePlatformOrder(next));
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

  async function refreshFertility() {
    if (fertilityRefreshingRef.current) {
      return;
    }
    fertilityRefreshingRef.current = true;
    const requestId = ++fertilityRequestSeq.current;
    setFertilityRefreshing(true);
    setFertilityError(null);
    try {
      const snapshot = await actions.fertility(true);
      if (fertilityRequestSeq.current !== requestId) {
        return;
      }
      setFertilitySnapshot(snapshot);
    } catch (error) {
      if (fertilityRequestSeq.current === requestId) {
        const message = explainError(error);
        setFertilityError(message);
        onError(message);
      }
    } finally {
      if (fertilityRequestSeq.current === requestId) {
        fertilityRefreshingRef.current = false;
        setFertilityRefreshing(false);
      }
    }
  }

  async function openFertilitySource() {
    try {
      await openExternalUrl("https://laplace.live/ovu");
    } catch (error) {
      onError(explainError(error));
    }
  }

  function openFertilityDetail(streamer: Streamer) {
    // 开关关闭时不渲染入口，这里只是兜底，避免任何路径把空数据弹窗打开。
    if (!fertilityEnabled) {
      return;
    }
    setFertilityTarget(streamer);
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
      <div ref={stickyRef} className={`sticky-page-header creators-sticky ${stickyPinned ? "is-pinned" : ""}`}>
        <header className="page-header">
          <div>
            <h1 className="schedule-title">主播库</h1>
          </div>
          <div className="stat-chip" role="status">
            <UsersRound size={17} />{reorderBusy ? "正在保存顺序…" : `${platformSubscriptions.length} 位主播`}
          </div>
        </header>

        <div className="search-panel">
          {youtubeEnabled ? (
            <div className="creator-platform-switch" role="tablist" aria-label="主播平台">
              <button
                type="button"
                role="tab"
                aria-selected={currentPlatform === "bilibili"}
                className={currentPlatform === "bilibili" ? "active" : ""}
                tabIndex={currentPlatform === "bilibili" ? 0 : -1}
                onClick={() => selectPlatform("bilibili")}
                onKeyDown={(event) => handlePlatformTabKeyDown(event, "bilibili")}
              >
                Bilibili
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={currentPlatform === "youtube"}
                className={currentPlatform === "youtube" ? "active" : ""}
                tabIndex={currentPlatform === "youtube" ? 0 : -1}
                onClick={() => selectPlatform("youtube")}
                onKeyDown={(event) => handlePlatformTabKeyDown(event, "youtube")}
              >
                YouTube
              </button>
            </div>
          ) : null}
          <div className="search-input">
            <div className="search-field">
              <Search size={18} />
              <input
                ref={searchInputRef}
                value={keyword}
                aria-label={currentPlatform === "youtube"
                  ? "搜索 YouTube 频道"
                  : "搜索 B 站主播"}
                onChange={(event) => handleKeywordChange(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    cancelSearchTimer();
                    void performSearch(keyword);
                  } else if (event.key === "Escape" && keyword) {
                    clearSearch();
                  }
                }}
                placeholder={currentPlatform === "youtube"
                  ? "输入频道名、@handle、频道 ID 或 YouTube 链接"
                  : "输入 B 站昵称、UID、@昵称或空间链接"}
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
              type="button"
              className="button primary"
              onClick={() => {
                cancelSearchTimer();
                void performSearch(keyword);
              }}
              disabled={searching || !keyword.trim()}
            >
              {searching ? "搜索中…" : currentPlatform === "youtube" ? "搜索频道" : "搜索"}
            </button>
            {currentPlatform === "youtube" && looksLikeYoutubeAccountInput(keyword) ? (
              <button
                type="button"
                className="button secondary youtube-direct-add"
                aria-label="直接添加 YouTube 频道"
                onClick={() => void addYoutubeInput()}
                disabled={searching || busy !== null}
              >
                <UserPlus size={15} />
                直接添加频道
              </button>
            ) : null}
          </div>
          {searchError ? <ErrorBanner message={searchError} onRetry={() => void performSearch(keyword)} /> : null}
        </div>
      </div>

      {hasSearched && !searching && !searchError && visibleResults.length === 0 ? (
        <div className="search-panel search-results-panel">
          <div className="search-empty">
            {currentPlatform === "youtube"
              ? "未找到 YouTube 频道，请尝试 @handle、频道 ID 或完整频道链接。"
              : "未找到匹配主播，请尝试完整昵称、UID 或 @昵称。"}
          </div>
        </div>
      ) : null}
      {visibleResults.length ? (
        <div className="search-panel search-results-panel">
          <div className="search-results">
            <div className="search-result-count">找到 {visibleResults.length} 位主播</div>
            {visibleResults.slice(0, 20).map((account) => {
              const resultPlatform = accountPlatform(account);
              const busyKey = accountKey(resultPlatform, account.mid);
              const subscribed = subscriptions.some((streamer) => (
                streamerPlatform(streamer) === resultPlatform && streamer.mid === account.mid
              ));
              const handle = account.handle?.trim();
              return (
                <article className={`search-result ${resultPlatform}`} key={busyKey}>
                  <Avatar url={account.avatarUrl} name={account.name} size="lg" live={account.isLive} />
                  <div>
                    <div className="search-result-title"><h3>{account.name}</h3><PlatformBadge platform={resultPlatform} /></div>
                    <p>
                      {resultPlatform === "youtube"
                        ? `${handle || account.mid} · ${account.fans.toLocaleString()} 订阅者`
                        : `UID ${account.mid} · ${account.fans.toLocaleString()} 粉丝`}
                      {account.isLive ? " · 直播中" : ""}
                    </p>
                    {account.sign ? <p className="result-sign">{account.sign}</p> : null}
                  </div>
                  <button
                    type="button"
                    className="button small"
                    onClick={() => void addAccount(account)}
                    disabled={subscribed || busy !== null}
                  >
                    {subscribed ? <><BadgeCheck size={15} />已订阅</> : busy === busyKey ? "订阅中…" : <><UserPlus size={15} />订阅</>}
                  </button>
                </article>
              );
            })}
          </div>
        </div>
      ) : null}

      {moreMenu && (youtubeEnabled || streamerPlatform(moreMenu.streamer) === "bilibili") ? (
        <div id="creator-more-menu" className="context-menu creator-more-menu" ref={moreMenuRef}
          role="menu" aria-label={`${moreMenu.streamer.name} 的更多操作`}
          style={{ left: moreMenu.x, top: moreMenu.y }}
          onBlur={(event) => {
            if (event.relatedTarget !== moreTriggerRef.current && !event.currentTarget.contains(event.relatedTarget as Node | null)) closeMoreMenu();
          }}
          onKeyDown={(event) => {
            if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
            event.preventDefault();
            const items = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>("button"));
            const index = items.indexOf(document.activeElement as HTMLButtonElement);
            const next = event.key === "Home" ? 0 : event.key === "End" ? items.length - 1
              : (index + (event.key === "ArrowDown" ? 1 : -1) + items.length) % items.length;
            items[next]?.focus();
          }}>
          <button type="button" role="menuitem" onClick={() => {
            const target = moreMenu.streamer;
            closeMoreMenu(true);
            void openWeeklyRules(target);
          }}><CalendarClock size={16} />设定周表</button>
          <button type="button" role="menuitem" onClick={() => {
            const target = moreMenu.streamer;
            closeMoreMenu(true);
            openAlias(target);
          }}><Settings2 size={16} />设置别名</button>
          <button type="button" role="menuitem" onClick={() => {
            const target = moreMenu.streamer;
            closeMoreMenu(true);
            setRecentTarget(target);
          }}><History size={16} />查看最近直播</button>
        </div>
      ) : null}
      {recentTarget && (youtubeEnabled || streamerPlatform(recentTarget) === "bilibili") ? (
        <CreatorRecentLives key={recentTarget.id} streamer={recentTarget} actions={actions}
          onClose={() => {
            setRecentTarget(null);
            moreTriggerRef.current?.focus();
          }} />
      ) : null}

      {platformSubscriptions.length === 0 ? (
        <EmptyState
          title={currentPlatform === "youtube" ? "还没有订阅 YouTube 频道" : "还没有订阅主播"}
          description={currentPlatform === "youtube"
            ? "在上方搜索频道、@handle 或粘贴频道链接并添加。"
            : "在上方搜索 B 站账号并添加。"}
        />
      ) : (
        <div className="creator-list">
          {platformSubscriptions.map((streamer, index) => {
            const cardPlatform = streamerPlatform(streamer);
            const channelLabel = cardPlatform === "youtube"
              ? streamer.handle?.trim() || streamer.mid
              : `UID ${streamer.mid}`;
            const youtubeLiveUrl = cardPlatform === "youtube"
              ? streamer.liveUrl || (streamer.liveVideoId
                ? `https://www.youtube.com/watch?v=${encodeURIComponent(streamer.liveVideoId)}`
                : null)
              : null;
            const canOpen = Boolean(youtubeLiveUrl || streamer.liveUrl || streamer.roomId || streamer.profileUrl || streamer.mid);
            return (
            <article
              className={`creator-card ${cardPlatform} ${streamer.isLive ? "is-live" : ""} ${draggingId === streamer.id ? "is-dragging" : ""} ${dropTargetId === streamer.id ? "is-drop-target" : ""}`}
              key={streamer.id}
              onDoubleClick={(event) => {
                if (event.target instanceof Element && event.target.closest("button, a, input, textarea, select")) {
                  return;
                }
                void onOpenLiveRoom(streamer);
              }}
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
                  disabled={reorderBusy || index === platformSubscriptions.length - 1}
                  onClick={() => moveStreamer(streamer.id, 1)}
                >
                  <ChevronDown size={14} />
                </button>
              </div>
              <Avatar url={streamer.avatarUrl} name={streamer.name} size="lg" live={streamer.isLive} />
              <div className="creator-main">
                <div className="creator-title">
                  <h3>{streamer.name}</h3>
                  <PlatformBadge platform={cardPlatform} />
                  <span className="creator-channel-id">{channelLabel}</span>
                  {streamer.isLive ? <span className="live-label">直播中</span> : null}
                </div>
                <div className="creator-actions">
                  {fertilityEnabled && cardPlatform === "bilibili" ? (
                    (() => {
                      const status = findFertilityStatus(fertilitySnapshot, streamer);
                      const meta = status ? fertilityStatusMeta(status.status) : null;
                      // 开关关闭时整块不渲染：没有数据就没有展示价值，也不该占位。
                      return (
                        <button
                          type="button"
                          className={`creator-fertility-button ${meta?.className ?? "is-missing"}`}
                          title={
                            status
                              ? `${meta?.label ?? "月相"}${status.dayInCycle ? ` · 周期第 ${status.dayInCycle} 天` : ""} · 下次 ${formatNextPeriod(status.nextPeriod)} · 点击查看详情`
                              : "LAPLACE 暂未收录这位主播，点击查看说明"
                          }
                          aria-label={`${streamer.name} 的月相参考`}
                          onClick={() => openFertilityDetail(streamer)}
                        >
                          {status ? (
                            <>
                              <strong>{fertilityCompactLabel(status)}</strong>
                              <em>{formatNextPeriod(status.nextPeriod)}</em>
                            </>
                          ) : (
                            <strong>{fertilityLoading ? "读取中…" : "· 未收录"}</strong>
                          )}
                        </button>
                      );
                    })()
                  ) : null}
                  <button
                    type="button"
                    className="creator-fan-button"
                    title={streamer.fans === null
                      ? (cardPlatform === "youtube" ? "尚未采集订阅者数" : "尚未采集粉丝数")
                      : `${FAN_COUNT_FORMATTER.format(streamer.fans)} ${cardPlatform === "youtube" ? "订阅者" : "粉丝"}，点击查看变化`}
                    onClick={() => openFanHistory(streamer)}
                  >
                    <TrendingUp size={14} />
                    <strong>{formatFanCount(streamer.fans)}</strong>
                    <span>{cardPlatform === "youtube" ? "订阅者" : "粉丝"}</span>
                    <em>查看趋势</em>
                  </button>
                  <button
                    className={`icon-button live-room ${streamer.isLive ? "is-live" : ""}`}
                    title={cardPlatform === "youtube"
                      ? streamer.isLive && youtubeLiveUrl
                        ? `进入 ${streamer.name} 的 YouTube 直播间`
                        : `打开 ${streamer.name} 的 YouTube 频道`
                      : streamer.liveUrl || streamer.roomId
                        ? `进入 ${streamer.name} 的直播间`
                        : `${streamer.name} 暂未配置直播间地址`}
                    aria-label={cardPlatform === "youtube" && !(streamer.isLive && youtubeLiveUrl)
                      ? `打开 ${streamer.name} 的 YouTube 频道`
                      : `进入 ${streamer.name} 的直播间`}
                    onClick={() => void onOpenLiveRoom(streamer)}
                    disabled={!canOpen}
                  >
                    <ExternalLink size={17} />
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
                  <button
                    type="button"
                    className="icon-button"
                    title={`${streamer.name} 的更多操作`}
                    aria-label={`${streamer.name} 的更多操作`}
                    aria-haspopup="menu"
                    aria-expanded={moreMenu?.streamer.id === streamer.id}
                    aria-controls={moreMenu?.streamer.id === streamer.id ? "creator-more-menu" : undefined}
                    onClick={(event) => {
                      if (moreMenu?.streamer.id === streamer.id) {
                        closeMoreMenu(true);
                        return;
                      }
                      const bounds = event.currentTarget.getBoundingClientRect();
                      moreTriggerRef.current = event.currentTarget;
                      moreScrollChangedRef.current = captureMenuScroll(event.currentTarget);
                      setMoreMenu({ streamer, x: bounds.right - 200, y: bounds.bottom + 6 });
                    }}
                  ><MoreHorizontal size={17} /></button>
                  <button
                    type="button"
                    className="icon-button danger"
                    title="删除主播"
                    aria-label={`删除主播 ${streamer.name}`}
                    onClick={() => void removeStreamer(streamer)}
                  >
                    <Trash2 size={17} />
                  </button>
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
                  <span className="linked-label"><Link2 size={14} />{cardPlatform === "youtube" ? "频道" : "官号"}</span>
                  {streamer.linkedAccounts.length ? streamer.linkedAccounts.map((linked) => {
                    const linkedPlatform = linked.platform === "youtube" ? "youtube" : cardPlatform;
                    return (
                    <span className="linked-pill" key={accountKey(linkedPlatform, linked.mid)}>
                      {linked.name}
                      <button
                        type="button"
                        aria-label={`${cardPlatform === "youtube" ? "移除频道" : "移除官号"} ${linked.name}`}
                        onClick={() => void removeLink(streamer, linked)}
                      >
                        <X size={12} />
                      </button>
                    </span>
                    );
                  }) : cardPlatform === "bilibili" && A_SOUL_MEMBER_MIDS.has(streamer.mid) ? null : (
                    <span className="linked-empty">未关联</span>
                  )}
                  {cardPlatform === "bilibili" && A_SOUL_MEMBER_MIDS.has(streamer.mid) &&
                  !streamer.linkedAccounts.some((linked) => linked.mid === A_SOUL_OFFICIAL_MID) ? (
                    <span className="linked-pill automatic" title="A-SOUL 成员会自动抓取该官号">
                      <Link2 size={12} />A-SOUL_Official · 自动
                    </span>
                  ) : null}
                  <button type="button" className="text-button" onClick={() => openLinkDialog(streamer)}>
                    <Plus size={13} />{cardPlatform === "youtube" ? "关联频道" : "关联官号"}
                  </button>
                </div>
              </div>
            </article>
            );
          })}
        </div>
      )}

      {fanTarget && (youtubeEnabled || streamerPlatform(fanTarget) === "bilibili") ? (
        <Modal title={`${fanTargetLabel}变化 · ${fanTarget.name}`} onClose={closeFanHistory} wide>
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
                <span>当前{fanTargetLabel}</span>
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
                正在读取{fanTargetLabel}历史…
              </div>
            ) : fanBuckets.length === 0 ? (
              <div className="fan-chart-empty">
                <TrendingUp size={22} />
                <strong>还没有{fanTargetLabel}快照</strong>
                <span>后台将在启动后及之后每小时自动采集，也可以点击“立即采集”。</span>
              </div>
            ) : (
              <>
                <FanTrendChart buckets={fanBuckets} />
                <div className="fan-history-table" aria-label={`${fanTargetLabel}历史统计`}>
                  <div className="fan-history-row is-head">
                    <span>时间</span>
                    <span>{fanTargetLabel}数</span>
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

      {fertilityTarget && (youtubeEnabled || streamerPlatform(fertilityTarget) === "bilibili") ? (
        <Modal
          title={`月相 · ${fertilityTarget.name}`}
          onClose={() => setFertilityTarget(null)}
          footer={
            <>
              <button
                className="button secondary"
                onClick={() => {
                  setFertilityTarget(null);
                  void openFertilitySource();
                }}
              >
                <ExternalLink size={15} />前往 LAPLACE
              </button>
              <button
                className="button primary"
                onClick={() => void refreshFertility()}
                disabled={fertilityRefreshing}
              >
                <RefreshCw className={fertilityRefreshing ? "spin" : ""} size={15} />
                {fertilityRefreshing ? "刷新中…" : "立即刷新"}
              </button>
            </>
          }
        >
          {(() => {
            const status = findFertilityStatus(fertilitySnapshot, fertilityTarget);
            const meta = status ? fertilityStatusMeta(status.status) : null;
            return (
              <div className="fertility-detail">
                <div className="fertility-detail-head">
                  <Avatar url={fertilityTarget.avatarUrl} name={fertilityTarget.name} size="lg" />
                  <div>
                    <strong>{fertilityTarget.name}</strong>
                    <span>UID {fertilityTarget.mid}</span>
                  </div>
                  <span className={`fertility-status-pill ${meta?.className ?? "is-missing"}`}>
                    {status ? `${meta?.emoji} ${meta?.label}` : "未收录"}
                  </span>
                </div>

                {fertilityError ? <ErrorBanner message={fertilityError} onRetry={() => void refreshFertility()} /> : null}

                {status ? (
                  <dl className="fertility-detail-grid">
                    <div><dt>当前状态</dt><dd>{meta?.hint ?? meta?.label}</dd></div>
                    <div><dt>周期内第几天</dt><dd>{status.dayInCycle === null ? "--" : `第 ${status.dayInCycle} 天`}</dd></div>
                    <div><dt>下次月隐</dt><dd>{formatNextPeriod(status.nextPeriod)}</dd></div>
                    <div><dt>月圆日</dt><dd>{status.ovulationDay === null ? "--" : `第 ${status.ovulationDay} 天`}</dd></div>
                    <div><dt>有效周期长度</dt><dd>{status.cycleLength === null ? "--" : `${status.cycleLength} 天`}</dd></div>
                    <div><dt>月隐时长</dt><dd>{status.periodLength === null ? "--" : `${status.periodLength} 天`}</dd></div>
                    <div><dt>数据点</dt><dd>{status.dataPoints} 条记录</dd></div>
                    <div><dt>匹配账号</dt><dd>UID {status.matchedMid}</dd></div>
                  </dl>
                ) : (
                  <p className="fertility-detail-empty">
                    LAPLACE 目前没有收录这位主播。可以先去原站确认，或在 LAPLACE 提交投稿。
                  </p>
                )}

                <div className="fertility-detail-source">
                  <span>{fertilitySourceLabel(fertilitySnapshot?.source)}</span>
                  <span>抓取于 {formatFertilityFetchedAt(fertilitySnapshot?.fetchedAt)}</span>
                  {fertilitySnapshot?.stale ? <strong>使用缓存数据</strong> : null}
                </div>
                {fertilitySnapshot?.warning ? (
                  <p className="fertility-detail-warning">{fertilitySnapshot.warning}</p>
                ) : null}
                <p className="fertility-detail-note">
                  数据由 LAPLACE 及其投稿者整理与预测，仅供娱乐参考，不是医疗信息。
                </p>
              </div>
            );
          })()}
        </Modal>
      ) : null}

      {linkTarget && (youtubeEnabled || streamerPlatform(linkTarget) === "bilibili") ? (
        <Modal title={`为 ${linkTarget.name} 关联${linkPlatform === "youtube" ? "频道" : "官号"}`} onClose={closeLinkDialog}>
          <div className="search-input compact">
            <div className="search-field">
              <Search size={17} />
              <input
                value={linkKeyword}
                aria-label={`搜索${linkPlatform === "youtube" ? " YouTube 频道" : " B 站官号"}`}
                onChange={(event) => updateLinkKeyword(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    void searchLinkAccounts();
                  } else if (event.key === "Escape" && linkKeyword) {
                    updateLinkKeyword("");
                  }
                }}
                placeholder={linkPlatform === "youtube"
                  ? "搜索 YouTube 频道名、@handle、频道 ID 或链接"
                  : "搜索官号昵称、UID、@昵称或空间链接"}
              />
              {linkKeyword ? (
                <button
                  type="button"
                  className="search-clear"
                  aria-label={`清空${linkPlatform === "youtube" ? "频道" : "官号"}搜索`}
                  title={`清空${linkPlatform === "youtube" ? "频道" : "官号"}搜索`}
                  onClick={() => updateLinkKeyword("")}
                >
                  <X size={15} />
                </button>
              ) : null}
            </div>
            <button
              type="button"
              className="button secondary small"
              onClick={() => void searchLinkAccounts()}
              disabled={linkSearching || !linkKeyword.trim()}
            >
              {linkSearching ? "搜索中…" : "搜索"}
            </button>
          </div>
          <div className="modal-list">
            {linkResults.map((account) => {
              const resultPlatform = accountPlatform(account);
              const already = linkTarget.linkedAccounts.some((linked) => (
                (linked.platform === "youtube" ? "youtube" : linkPlatform) === resultPlatform
                  && linked.mid === account.mid
              ));
              return (
                <div className="modal-list-row youtube-link-row" key={accountKey(resultPlatform, account.mid)}>
                  <Avatar url={account.avatarUrl} name={account.name} />
                  <div>
                    <div className="search-result-title"><strong>{account.name}</strong><PlatformBadge platform={resultPlatform} /></div>
                    <span>{resultPlatform === "youtube" ? account.handle || account.mid : `UID ${account.mid}`}</span>
                  </div>
                  <button
                    type="button"
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

      {aliasTarget && (youtubeEnabled || streamerPlatform(aliasTarget) === "bilibili") ? (
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

      {weeklyTarget && (youtubeEnabled || streamerPlatform(weeklyTarget) === "bilibili") ? (
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
