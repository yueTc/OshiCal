import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import type { PointerEvent as ReactPointerEvent, MouseEvent as ReactMouseEvent } from "react";
import { flushSync } from "react-dom";
import { LiveOnlineChart } from "./LiveOnlineChart";
import { differenceInCalendarDays } from "date-fns";
import {
  ChevronLeft,
  BarChart3,
  ChevronRight,
  Clock3,
  ExternalLink,
  Grid3X3,
  GripVertical,
  List,
  LoaderCircle,
  Minus,
  MonitorPlay,
  MoreHorizontal,
  Pencil,
  Plus,
  RefreshCw,
  RotateCcw,
  Trash2,
  UserRound,
} from "lucide-react";
import type {
  LiveSession,
  LiveOnlineSamplesPage,
  LiveStatus,
  NewScheduleInput,
  Schedule,
  SourceKind,
  Streamer,
  UpdateScheduleInput,
} from "../types";
import { addDays, eventDayKey, inputDateTime, localDateKey, rangeAroundDays, rollingDays, timeLabel } from "../lib/dates";
import { explainError, openExternalUrl } from "../lib/api";
import { matchLiveSchedules } from "../lib/live-schedule";
import { useStickyHeader } from "../lib/sticky-header";
import { timelineAnchorCompensation, timelineDragScrollTop, visibleTimelineDayKeys } from "../lib/timeline-layout";
import { Avatar, EmptyState, ErrorBanner, Modal, SubscriptionSelect } from "./ui";

function formatLiveDuration(seconds: number): string {
  if (seconds < 60) return `${Math.max(0, seconds)}s`;
  const hours = seconds / 3600;
  if (hours >= 1) return `${hours.toFixed(1)}h`;
  return `${Math.max(1, Math.round(seconds / 60))}m`;
}

function formatMoney(cents: number): string {
  return `¥${(cents / 100).toFixed(1)}`;
}

function formatCount(value: number): string {
  if (value >= 10000) return `${(value / 10000).toFixed(1)}万`;
  return value.toLocaleString("zh-CN");
}

function formatLiveTime(value: string): string {
  return new Date(value).toLocaleString("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function liveCollectionFailed(session: LiveSession): boolean {
  return session.reconnectCount > 0
    && session.peakHighEnergy === 0
    && session.danmakuCount === 0
    && session.giftCount === 0
    && session.superChatCount === 0
    && session.guardCount === 0;
}

// @decision LIVE-QUALITY-1 “部分缺失”是整场标记：任意一次掉线重连、任意一个无法解析的
// 弹幕包、任意一条缺少 UID 的礼物都会把整场降级，所以徽标必须带上原因，否则用户只会看到
// 一堆正常数字旁边挂着一个没有解释的“部分缺失”。
// 断流重开现在会并回同一场（segmentCount > 1），这同样是一次真实的数据空洞，要一起报出来。
function liveInterruptions(session: LiveSession): number {
  return Math.max(0, (session.segmentCount ?? 1) - 1);
}

function liveQualityLabel(session: LiveSession): string {
  if (liveCollectionFailed(session)) return "采集失败";
  if (session.dataQuality === "complete") return "数据完整";
  if (session.dataQuality === "partial") {
    const reasons: string[] = [];
    if (session.reconnectCount > 0) reasons.push(`重连 ${session.reconnectCount} 次`);
    const interruptions = liveInterruptions(session);
    if (interruptions > 0) reasons.push(`断流 ${interruptions} 次`);
    return reasons.length > 0 ? `部分缺失 · ${reasons.join(" · ")}` : "部分缺失";
  }
  return "采集中";
}

function liveQualityDetail(session: LiveSession): string {
  if (liveCollectionFailed(session)) {
    return "弹幕连接始终没有建立：营收、付费人数、互动人数和弹幕不可用，时长仍来自直播状态轮询。";
  }
  if (session.dataQuality === "complete") return "弹幕采集全程在线，事件类指标完整。";
  if (session.dataQuality === "partial") {
    const reasons: string[] = [];
    if (session.reconnectCount > 0) reasons.push(`重连 ${session.reconnectCount} 次`);
    const interruptions = liveInterruptions(session);
    if (interruptions > 0) {
      reasons.push(`断流重开 ${interruptions} 次（已合并为同一场，共 ${session.segmentCount} 段）`);
    }
    if (reasons.length > 0) {
      return `采集期间${reasons.join("、")}，断线窗口内的弹幕和礼物可能没有被计入，数值偏保守。`;
    }
    if (session.collectorState === "active" || session.collectorState === "reconnecting") {
      return "采集端仍在统计这场直播，数字会继续增长。";
    }
    return "采集过程中出现过无法解析的数据包或缺少 UID 的事件，个别指标可能偏低。";
  }
  return "采集端正在统计，直播结束后给出完整度。";
}
type ViewMode = "timeline" | "grid";

const CALENDAR_BUFFER_BEFORE = 14;
const CALENDAR_BUFFER_AFTER = 20;
const GRID_WINDOW_BEFORE = 14;
const GRID_WINDOW_DAYS = 35;
const GRID_GAP = 10;
const GRID_DAY_COUNT_MAX = 7;
const GRID_DAY_WIDTH_MIN = 120;
const GRID_DAY_WIDTH_MAX = 360;
const GRID_DAY_WIDTH_STEP = 20;
const GRID_DAY_WIDTH_DEFAULT = 180;
const GRID_DAY_WIDTH_KEY = "oshical.gridDayWidth";
const GRID_DRAG_THRESHOLD = 42;
const TIMELINE_COLUMNS_MIN = 1;
const TIMELINE_COLUMNS_MAX = 4;
const TIMELINE_COLUMNS_DEFAULT = 2;
const TIMELINE_COLUMNS_KEY = "oshical.timelineColumns";
const TIMELINE_CARD_MIN_WIDTH = 250;
const TIMELINE_EVENT_GAP = 10;
const TIMELINE_DATE_GAP = 8;
const TIMELINE_GAP = 18;
const TIMELINE_VISIBLE_MIN_PX = 2;

function clampGridDayWidth(width: number): number {
  return Math.min(GRID_DAY_WIDTH_MAX, Math.max(GRID_DAY_WIDTH_MIN, width));
}

function readGridDayWidthPreference(): number {
  const stored = Number.parseInt(readLocalPreference(GRID_DAY_WIDTH_KEY) ?? "", 10);
  return Number.isFinite(stored) ? clampGridDayWidth(stored) : GRID_DAY_WIDTH_DEFAULT;
}

function clampTimelineColumns(columns: number): number {
  return Math.min(TIMELINE_COLUMNS_MAX, Math.max(TIMELINE_COLUMNS_MIN, columns));
}

function readTimelineColumnsPreference(): number {
  const stored = Number.parseInt(readLocalPreference(TIMELINE_COLUMNS_KEY) ?? "", 10);
  return Number.isFinite(stored) ? clampTimelineColumns(stored) : TIMELINE_COLUMNS_DEFAULT;
}

function prefersReducedMotion(): boolean {
  return typeof window !== "undefined"
    && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

function startOfLocalDay(date: Date): number {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
}

interface ScheduleActions {
  load: (from: string, to: string) => Promise<Schedule[]>;
  loadCached?: (from: string, to: string) => Promise<Schedule[] | null>;
  create: (input: NewScheduleInput) => Promise<Schedule>;
  update: (schedule: Schedule, update: UpdateScheduleInput) => Promise<Schedule>;
  remove: (schedule: Schedule) => Promise<void>;
  liveSession: (scheduleId: number) => Promise<LiveSession | null>;
  liveOnlineSamples: (sessionId: number, afterId: number, signal?: AbortSignal) => Promise<LiveOnlineSamplesPage>;
  refresh: () => Promise<void>;
}

interface Draft {
  id: number | null;
  sourceIdentity: string;
  baselineUpdatedAt: string | null;
  baselineRevision: number | null;
  streamerId: number | null;
  title: string;
  startAt: string;
  endAt: string;
  roomUrl: string;
}

const SOURCE_LABELS: Record<SourceKind, string> = {
  text: "文本",
  reserve: "预约",
  image: "图片",
  manual: "手动",
  editor: "文本",
  weekly: "固定周表",
  live: "开播补录",
};

const SOURCE_CLASS: Record<SourceKind, string> = {
  text: "source-text",
  reserve: "source-reserve",
  image: "source-image",
  manual: "source-manual",
  editor: "source-text",
  weekly: "source-weekly",
  live: "source-reserve",
};

function createManualSourceIdentity(): string {
  const random = globalThis.crypto?.randomUUID?.()
    ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
  return `manual:${random}`;
}

function readLocalPreference(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeLocalPreference(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Preferences are optional; storage failures must not break the schedule view.
  }
}

function emptyDraft(subscriptions: Streamer[]): Draft {
  const first = subscriptions[0];
  return {
    id: null,
    sourceIdentity: createManualSourceIdentity(),
    baselineUpdatedAt: null,
    baselineRevision: null,
    streamerId: first?.id ?? null,
    title: "",
    startAt: inputDateTime(new Date(Date.now() + 60 * 60 * 1000).toISOString()),
    endAt: "",
    roomUrl: first?.liveUrl ?? "",
  };
}

export default function ScheduleView({
  active,
  subscriptions,
  liveStatuses,
  actions,
  revision,
  onError,
}: {
  active: boolean;
  subscriptions: Streamer[];
  liveStatuses: LiveStatus[];
  actions: ScheduleActions;
  revision: number;
  onError: (message: string) => void;
}) {
  const [anchor, setAnchor] = useState(() => new Date());
  const [viewMode, setViewMode] = useState<ViewMode>(() =>
    readLocalPreference("oshical.viewMode") === "grid" ? "grid" : "timeline",
  );
  const [viewEnterAnimation, setViewEnterAnimation] = useState(false);
  const [schedules, setSchedules] = useState<Schedule[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [savingDraft, setSavingDraft] = useState(false);
  const [menu, setMenu] = useState<{ schedule: Schedule; x: number; y: number } | null>(null);
  const [onlineChartOpen, setOnlineChartOpen] = useState(false);
  const peakCardRef = useRef<HTMLButtonElement>(null);
  const [liveModal, setLiveModal] = useState<{
    schedule: Schedule;
    session: LiveSession | null;
    loading: boolean;
  } | null>(null);
  const longPress = useRef<ReturnType<typeof setTimeout> | null>(null);
  const longPressStartRef = useRef<{ x: number; y: number } | null>(null);
  const longPressTriggered = useRef(false);
  const suppressClickTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const gridWheelTimerRef = useRef<number | null>(null);
  const pageRef = useRef<HTMLElement>(null);
  const { stickyRef: scheduleStickyRef, stickyPinned: scheduleStickyPinned } = useStickyHeader<HTMLDivElement>();
  const gridRef = useRef<HTMLDivElement>(null);
  const timelineViewportRef = useRef<HTMLDivElement>(null);
  const timelineRef = useRef<HTMLDivElement>(null);
  const timelineVisibleFrameRef = useRef<number | null>(null);
  const timelineTopAnchorRef = useRef<{ key: string; offset: number } | null>(null);
  const timelineScrollTargetRef = useRef<string | null>(localDateKey(new Date()));
  const menuRef = useRef<HTMLDivElement>(null);
  const loadedWindowRef = useRef<{ from: number; to: number } | null>(null);
  const hasLoadedScheduleRef = useRef(false);
  const lastRevisionRef = useRef<number | null>(null);
  const loadRequestRef = useRef(0);
  const savingDraftRef = useRef(false);
  const draftRef = useRef<Draft | null>(draft);
  const refreshRef = useRef(false);
  const removingScheduleIdsRef = useRef<Set<number>>(new Set());
  const liveRequestRef = useRef(0);
  const gridDragRef = useRef<{ startX: number; deltaX: number; pointerId: number; captured: boolean } | null>(null);
  const timelineSwipeRef = useRef<{ startY: number; startScrollTop: number; moved: boolean; pointerId: number } | null>(null);
  const suppressCardClickRef = useRef(false);
  const gridWheelLockRef = useRef(false);
  const [gridDayWidth, setGridDayWidth] = useState(readGridDayWidthPreference);
  const [timelineColumns, setTimelineColumns] = useState(readTimelineColumnsPreference);
  const [calendarWidth, setCalendarWidth] = useState(0);
  const visibleDayCount = useMemo(() => {
    if (calendarWidth <= 0) return GRID_DAY_COUNT_MAX;
    return Math.min(
      GRID_DAY_COUNT_MAX,
      Math.max(1, Math.floor((calendarWidth + GRID_GAP) / (gridDayWidth + GRID_GAP))),
    );
  }, [calendarWidth, gridDayWidth]);
  const timelineColumnsMax = useMemo(() => {
    if (calendarWidth <= 0) return TIMELINE_COLUMNS_MAX;
    if (calendarWidth <= 620) return 1;
    const dateColumnWidth = calendarWidth <= 960 ? 94 : 132;
    const availableCardWidth = Math.max(0, calendarWidth - dateColumnWidth - TIMELINE_DATE_GAP);
    return Math.max(
      1,
      Math.min(
        TIMELINE_COLUMNS_MAX,
        Math.floor((availableCardWidth + TIMELINE_EVENT_GAP) / (TIMELINE_CARD_MIN_WIDTH + TIMELINE_EVENT_GAP)),
      ),
    );
  }, [calendarWidth]);
  const effectiveTimelineColumns = Math.min(timelineColumns, timelineColumnsMax);
  const [gridWindowStart, setGridWindowStart] = useState(() => addDays(new Date(), -GRID_WINDOW_BEFORE));
  const [dragDayDelta, setDragDayDelta] = useState(0);
  const [gridPanRemainder, setGridPanRemainder] = useState(0);
  const [timelineVisibleDays, setTimelineVisibleDays] = useState<Date[]>([]);
  const [timelineViewportHeight, setTimelineViewportHeight] = useState(0);
  draftRef.current = draft;

  const timelineDays = useMemo(() => rollingDays(anchor), [anchor]);
  const gridWindowDays = useMemo(
    () => Array.from({ length: GRID_WINDOW_DAYS }, (_, index) => addDays(gridWindowStart, index)),
    [gridWindowStart],
  );
  const anchorIndex = differenceInCalendarDays(anchor, gridWindowStart);
  const visibleStartIndex = Math.max(0, Math.min(anchorIndex, GRID_WINDOW_DAYS - visibleDayCount));
  const visibleDays = useMemo(
    () => gridWindowDays.slice(visibleStartIndex, visibleStartIndex + visibleDayCount),
    [gridWindowDays, visibleDayCount, visibleStartIndex],
  );
  const previewAnchor = dragDayDelta === 0 ? anchor : addDays(anchor, dragDayDelta);
  const previewStartIndex = Math.max(
    0,
    Math.min(differenceInCalendarDays(previewAnchor, gridWindowStart), GRID_WINDOW_DAYS - visibleDayCount),
  );
  const stripDays = viewMode === "grid"
    ? gridWindowDays.slice(previewStartIndex, previewStartIndex + visibleDayCount)
    : timelineVisibleDays.length
      ? timelineVisibleDays
      : timelineDays.slice(0, 1);
  const rangeDays = stripDays;
  const peekBefore = viewMode === "grid" && gridPanRemainder > 0;
  const peekAfter = viewMode === "grid" && gridPanRemainder < 0;
  const displayDays = useMemo(() => {
    const days = viewMode === "grid" ? visibleDays : timelineVisibleDays.length ? timelineVisibleDays : timelineDays;
    return [
      ...(peekBefore ? [addDays(days[0], -1)] : []),
      ...days,
      ...(peekAfter ? [addDays(days[days.length - 1], 1)] : []),
    ];
  }, [peekAfter, peekBefore, timelineDays, timelineVisibleDays, viewMode, visibleDays]);
  const timelineVisibleKeySet = useMemo(
    () => new Set(timelineVisibleDays.map(localDateKey)),
    [timelineVisibleDays],
  );
  const visibleStartTime = startOfLocalDay(displayDays[0] ?? anchor);
  const visibleEndTime = startOfLocalDay(addDays(displayDays[displayDays.length - 1] ?? anchor, 1));
  const gridOffset = -visibleStartIndex * (gridDayWidth + GRID_GAP) + gridPanRemainder;
  const changeGridDayWidth = (delta: number) => {
    setGridPanRemainder(0);
    setGridDayWidth((current) => clampGridDayWidth(current + delta));
  };
  const changeTimelineColumns = (delta: number) => {
    setTimelineColumns((current) => clampTimelineColumns(
      Math.min(current, timelineColumnsMax) + delta,
    ));
  };
  const changeViewMode = (next: ViewMode) => {
    if (next === viewMode) return;
    setGridPanRemainder(0);
    setTimelineVisibleDays([]);
    timelineTopAnchorRef.current = null;
    if (next === "timeline") {
      timelineScrollTargetRef.current = localDateKey(anchor);
    }
    setViewEnterAnimation(false);
    if (prefersReducedMotion()) {
      setViewMode(next);
      return;
    }
    if (typeof document.startViewTransition === "function") {
      document.startViewTransition(() => {
        flushSync(() => setViewMode(next));
      }).finished.catch(() => undefined);
      return;
    }
    setViewEnterAnimation(true);
    setViewMode(next);
  };
  const liveScheduleIds = useMemo(
    () => matchLiveSchedules(schedules, liveStatuses),
    [schedules, liveStatuses],
  );

  const load = useCallback(async (force = false) => {
    const loaded = loadedWindowRef.current;
    const revisionChanged = lastRevisionRef.current !== revision;
    if (!force && !revisionChanged && loaded && visibleStartTime >= loaded.from && visibleEndTime <= loaded.to) {
      return;
    }
    const requestId = ++loadRequestRef.current;
    const range = rangeAroundDays(anchor, CALENDAR_BUFFER_BEFORE, CALENDAR_BUFFER_AFTER);
    // Keep already rendered schedules in place while a revision sync or range
    // refresh runs. The skeleton is only for the very first load.
    if (!hasLoadedScheduleRef.current) {
      setLoading(true);
    }
    setError(null);
    try {
      // Server mode keeps an initialized local mirror. Read it without waiting
      // for a network round trip; only a cold or uninitialized cache falls back
      // to the authoritative HTTP range query.
      if (actions.loadCached) {
        try {
          const cached = await actions.loadCached(range.from, range.to);
          if (requestId !== loadRequestRef.current) return;
          if (cached !== null) {
            loadedWindowRef.current = { from: Date.parse(range.from), to: Date.parse(range.to) };
            lastRevisionRef.current = revision;
            hasLoadedScheduleRef.current = true;
            setSchedules(cached);
            return;
          }
        } catch {
          // A cache-read failure must not prevent the HTTP fallback.
        }
      }
      const values = await actions.load(range.from, range.to);
      if (requestId !== loadRequestRef.current) return;
      loadedWindowRef.current = { from: Date.parse(range.from), to: Date.parse(range.to) };
      lastRevisionRef.current = revision;
      hasLoadedScheduleRef.current = true;
      setSchedules(values);
    } catch (loadError) {
      if (requestId !== loadRequestRef.current) return;
      const message = explainError(loadError);
      setError(message);
      onError(message);
    } finally {
      if (requestId === loadRequestRef.current) {
        setLoading(false);
      }
    }
  }, [actions, anchor, onError, revision, visibleEndTime, visibleStartTime]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    writeLocalPreference("oshical.viewMode", viewMode);
  }, [viewMode]);

  useEffect(() => {
    writeLocalPreference(GRID_DAY_WIDTH_KEY, String(gridDayWidth));
  }, [gridDayWidth]);

  useEffect(() => {
    writeLocalPreference(TIMELINE_COLUMNS_KEY, String(timelineColumns));
  }, [timelineColumns]);

  useLayoutEffect(() => {
    const element = pageRef.current;
    if (!element) return;
    const update = () => {
      const width = element.clientWidth;
      if (width <= 0) return;
      setCalendarWidth(width);
    };
    update();
    if (typeof ResizeObserver === "undefined") {
      window.addEventListener("resize", update);
      return () => window.removeEventListener("resize", update);
    }
    const observer = new ResizeObserver(update);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const captureTimelineTopAnchor = useCallback(() => {
    const viewport = timelineViewportRef.current;
    if (!viewport) return;
    const viewportRect = viewport.getBoundingClientRect();
    const days = viewport.querySelectorAll<HTMLElement>("[data-timeline-day]");
    for (const day of days) {
      const rect = day.getBoundingClientRect();
      if (rect.bottom > viewportRect.top + 1 && rect.top < viewportRect.bottom) {
        const key = day.dataset.timelineDay;
        if (key) {
          timelineTopAnchorRef.current = { key, offset: rect.top - viewportRect.top };
        }
        return;
      }
    }
  }, []);

  const restoreTimelineTopAnchor = useCallback(() => {
    const viewport = timelineViewportRef.current;
    const snapshot = timelineTopAnchorRef.current;
    if (!viewport || !snapshot || timelineSwipeRef.current) return;
    const day = viewport.querySelector<HTMLElement>(`[data-timeline-day="${snapshot.key}"]`);
    if (!day) return;
    const viewportRect = viewport.getBoundingClientRect();
    const currentOffset = day.getBoundingClientRect().top - viewportRect.top;
    const compensation = timelineAnchorCompensation(currentOffset, snapshot.offset);
    if (Math.abs(compensation) > 0.5) {
      viewport.scrollTop += compensation;
    }
  }, []);

  const readTimelineVisibleDays = useCallback(() => {
    const viewport = timelineViewportRef.current;
    if (!viewport) return;
    const viewportRect = viewport.getBoundingClientRect();
    const visibleTop = Math.max(0, viewportRect.top);
    const visibleBottom = Math.min(window.innerHeight, viewportRect.bottom);
    const rects = Array.from(viewport.querySelectorAll<HTMLElement>("[data-timeline-day]"))
      .map((day) => ({
        key: day.dataset.timelineDay ?? "",
        top: day.getBoundingClientRect().top,
        bottom: day.getBoundingClientRect().bottom,
      }))
      .filter((day) => day.key);
    const keys = visibleTimelineDayKeys(rects, visibleTop, visibleBottom, TIMELINE_VISIBLE_MIN_PX);
    const byKey = new Map(gridWindowDays.map((day) => [localDateKey(day), day]));
    const next = keys.flatMap((key) => {
      const day = byKey.get(key);
      return day ? [day] : [];
    });
    setTimelineVisibleDays((current) => {
      if (current.length === next.length && current.every((day, index) => localDateKey(day) === localDateKey(next[index]))) {
        return current;
      }
      return next;
    });
    captureTimelineTopAnchor();
    const topDay = next[0];
    if (topDay && !timelineSwipeRef.current && localDateKey(topDay) !== localDateKey(anchor)) {
      setAnchor(topDay);
    }
  }, [anchor, captureTimelineTopAnchor, gridWindowDays]);

  const scheduleTimelineMeasurement = useCallback(() => {
    if (timelineVisibleFrameRef.current !== null) return;
    timelineVisibleFrameRef.current = requestAnimationFrame(() => {
      timelineVisibleFrameRef.current = null;
      readTimelineVisibleDays();
    });
  }, [readTimelineVisibleDays]);

  const updateTimelineViewportHeight = useCallback(() => {
    const viewport = timelineViewportRef.current;
    if (!viewport) return;
    const top = Math.max(0, viewport.getBoundingClientRect().top);
    const height = Math.min(900, Math.max(240, window.innerHeight - top - 24));
    setTimelineViewportHeight((current) => Math.abs(current - height) < 0.5 ? current : height);
  }, []);

  useLayoutEffect(() => {
    if (viewMode !== "timeline" || loading) return;
    const timeline = timelineRef.current;
    if (!timeline) return;
    const measure = () => {
      updateTimelineViewportHeight();
      restoreTimelineTopAnchor();
      scheduleTimelineMeasurement();
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(timeline);
    return () => observer.disconnect();
  }, [
    calendarWidth,
    effectiveTimelineColumns,
    gridWindowStart,
    loading,
    restoreTimelineTopAnchor,
    scheduleTimelineMeasurement,
    schedules,
    updateTimelineViewportHeight,
    viewMode,
  ]);

  useLayoutEffect(() => {
    const offset = differenceInCalendarDays(anchor, gridWindowStart);
    const maxOffset = viewMode === "timeline"
      ? GRID_WINDOW_DAYS - 8
      : GRID_WINDOW_DAYS - visibleDayCount - 2;
    if (offset < 2 || offset > maxOffset) {
      if (viewMode === "timeline" && timelineScrollTargetRef.current === null) {
        captureTimelineTopAnchor();
      }
      setGridWindowStart(addDays(anchor, -GRID_WINDOW_BEFORE));
    }
  }, [anchor, captureTimelineTopAnchor, gridWindowStart, viewMode, visibleDayCount]);

  useLayoutEffect(() => {
    if (viewMode !== "timeline" || loading) return;
    const viewport = timelineViewportRef.current;
    const targetKey = timelineScrollTargetRef.current;
    if (!viewport || !targetKey || timelineViewportHeight <= 0) return;
    const target = viewport.querySelector<HTMLElement>(`[data-timeline-day="${targetKey}"]`);
    if (!target) return;
    const viewportRect = viewport.getBoundingClientRect();
    viewport.scrollTop += target.getBoundingClientRect().top - viewportRect.top;
    timelineScrollTargetRef.current = null;
    captureTimelineTopAnchor();
    scheduleTimelineMeasurement();
  }, [
    anchor,
    captureTimelineTopAnchor,
    effectiveTimelineColumns,
    gridWindowStart,
    loading,
    scheduleTimelineMeasurement,
    schedules,
    timelineViewportHeight,
    viewMode,
  ]);

  useEffect(() => {
    if (!active || viewMode !== "timeline" || loading) return;
    const viewport = timelineViewportRef.current;
    if (!viewport) return;
    const onScroll = () => {
      captureTimelineTopAnchor();
      scheduleTimelineMeasurement();
    };
    const onWindowScroll = () => {
      scheduleTimelineMeasurement();
    };
    const onWindowResize = () => {
      updateTimelineViewportHeight();
      restoreTimelineTopAnchor();
      scheduleTimelineMeasurement();
    };
    viewport.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("scroll", onWindowScroll, { passive: true });
    window.addEventListener("resize", onWindowResize);
    return () => {
      viewport.removeEventListener("scroll", onScroll);
      window.removeEventListener("scroll", onWindowScroll);
      window.removeEventListener("resize", onWindowResize);
    };
  }, [
    active,
    captureTimelineTopAnchor,
    loading,
    restoreTimelineTopAnchor,
    scheduleTimelineMeasurement,
    updateTimelineViewportHeight,
    viewMode,
  ]);

  useEffect(() => {
    if (!active) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
      const target = event.target;
      if (target instanceof HTMLElement && target.closest("input, textarea, select, [contenteditable='true'], .modal, .context-menu")) return;
      if (viewMode === "grid" && (event.key === "ArrowLeft" || event.key === "ArrowRight")) {
        event.preventDefault();
        const daysToMove = event.shiftKey ? 7 : 1;
        setGridPanRemainder(0);
        setAnchor((value) => addDays(value, event.key === "ArrowLeft" ? -daysToMove : daysToMove));
      } else if (viewMode === "timeline" && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
        event.preventDefault();
        const daysToMove = event.shiftKey ? 7 : 1;
        setAnchor((value) => {
          const next = addDays(value, event.key === "ArrowUp" ? -daysToMove : daysToMove);
          timelineScrollTargetRef.current = localDateKey(next);
          return next;
        });
      } else if (event.key === "Home" || event.key.toLowerCase() === "t") {
        event.preventDefault();
        setGridPanRemainder(0);
        const today = new Date();
        timelineScrollTargetRef.current = localDateKey(today);
        setAnchor(today);
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [active, viewMode]);

  useEffect(() => {
    if (!active || viewMode !== "grid" || loading) return;
    const element = gridRef.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      const horizontal = Math.abs(event.deltaX) > Math.abs(event.deltaY);
      if (!horizontal && !event.shiftKey) return;
      const delta = event.deltaX || event.deltaY;
      if (!delta) return;
      event.preventDefault();
      if (gridWheelLockRef.current) return;
      gridWheelLockRef.current = true;
      setGridPanRemainder(0);
      setAnchor((value) => addDays(value, delta > 0 ? 1 : -1));
      if (gridWheelTimerRef.current !== null) {
        window.clearTimeout(gridWheelTimerRef.current);
      }
      gridWheelTimerRef.current = window.setTimeout(() => {
        gridWheelTimerRef.current = null;
        gridWheelLockRef.current = false;
      }, 180);
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, [active, anchor, loading, schedules.length, viewMode]);

  useEffect(() => {
    if (active) return;
    liveRequestRef.current += 1;
    if (longPress.current !== null) {
      clearTimeout(longPress.current);
      longPress.current = null;
    }
    longPressStartRef.current = null;
    longPressTriggered.current = false;
    setMenu(null);
    setDraft(null);
    setLiveModal(null);
  }, [active]);

  useEffect(() => () => {
    if (longPress.current !== null) {
      clearTimeout(longPress.current);
    }
    if (suppressClickTimerRef.current !== null) {
      clearTimeout(suppressClickTimerRef.current);
    }
    if (gridWheelTimerRef.current !== null) {
      clearTimeout(gridWheelTimerRef.current);
    }
    if (timelineVisibleFrameRef.current !== null) {
      cancelAnimationFrame(timelineVisibleFrameRef.current);
      timelineVisibleFrameRef.current = null;
    }
  }, []);

  useEffect(() => {
    const close = () => setMenu(null);
    window.addEventListener("click", close);
    window.addEventListener("scroll", close, true);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("scroll", close, true);
    };
  }, []);

  useLayoutEffect(() => {
    const element = menuRef.current;
    if (!menu) return;
    if (!element) return;
    const margin = 8;
    const rect = element.getBoundingClientRect();
    const maxX = Math.max(margin, window.innerWidth - rect.width - margin);
    const maxY = Math.max(margin, window.innerHeight - rect.height - margin);
    const x = Math.min(Math.max(menu.x, margin), maxX);
    const y = Math.min(Math.max(menu.y, margin), maxY);
    if (x !== menu.x || y !== menu.y) {
      setMenu({ ...menu, x, y });
    }
  }, [menu]);

  const grouped = useMemo(() => {
    const map = new Map<string, Schedule[]>();
    for (const day of [...gridWindowDays, ...timelineDays]) {
      map.set(localDateKey(day), []);
    }
    for (const schedule of schedules) {
      const key = eventDayKey(schedule.startAt);
      const values = map.get(key);
      if (values) {
        values.push(schedule);
      }
    }
    for (const values of map.values()) {
      values.sort((left, right) => left.startAt.localeCompare(right.startAt));
    }
    return map;
  }, [gridWindowDays, schedules, timelineDays]);

  const visibleScheduleCount = useMemo(
    () => displayDays.reduce((count, day) => count + (grouped.get(localDateKey(day))?.length ?? 0), 0),
    [displayDays, grouped],
  );

  async function refresh() {
    if (refreshRef.current) {
      return;
    }
    refreshRef.current = true;
    setRefreshing(true);
    try {
      await actions.refresh();
      await load(true);
    } catch (refreshError) {
      onError(explainError(refreshError));
    } finally {
      refreshRef.current = false;
      setRefreshing(false);
    }
  }

  function updateDraft(patch: Partial<Draft>) {
    setDraft((current) => current ? { ...current, ...patch } : current);
  }

  function openCreate() {
    setDraft(emptyDraft(subscriptions));
  }

  function closeDraft() {
    if (!savingDraftRef.current) {
      setDraft(null);
    }
  }

  function openEdit(schedule: Schedule) {
    setDraft({
      id: schedule.id,
      sourceIdentity: schedule.sourceIdentity ?? "",
      baselineUpdatedAt: schedule.updatedAt,
      baselineRevision: schedule.revision ?? null,
      streamerId: schedule.streamerId,
      title: schedule.title,
      startAt: inputDateTime(schedule.startAt),
      endAt: schedule.endAt ? inputDateTime(schedule.endAt) : "",
      roomUrl: schedule.roomUrl ?? "",
    });
    setMenu(null);
  }

  async function saveDraft() {
    if (savingDraftRef.current) {
      return;
    }
    const submittedDraft = draftRef.current;
    if (!submittedDraft || !submittedDraft.streamerId || !submittedDraft.title.trim() || !submittedDraft.startAt) {
      return;
    }
    const startDate = new Date(submittedDraft.startAt);
    const endDate = submittedDraft.endAt ? new Date(submittedDraft.endAt) : null;
    if (!Number.isFinite(startDate.getTime()) || (endDate !== null && !Number.isFinite(endDate.getTime()))) {
      onError("日程时间格式无效，请重新选择开始和结束时间。");
      return;
    }
    if (endDate !== null && endDate.getTime() < startDate.getTime()) {
      onError("结束时间不能早于开始时间。");
      return;
    }
    const startAt = startDate.toISOString();
    const endAt = endDate?.toISOString() ?? null;
    const editingSchedule = submittedDraft.id !== null
      ? schedules.find((schedule) => schedule.id === submittedDraft.id) ?? null
      : null;
    if (submittedDraft.id !== null && !editingSchedule) {
      setDraft(null);
      onError("原日程已不存在，已重新加载日程列表。");
      await load(true);
      return;
    }
    if (
      editingSchedule
      && (
        editingSchedule.updatedAt !== submittedDraft.baselineUpdatedAt
        || (
          submittedDraft.baselineRevision !== null
          && editingSchedule.revision !== undefined
          && editingSchedule.revision !== submittedDraft.baselineRevision
        )
      )
    ) {
      setDraft(null);
      onError("原日程已在其他位置更新，已重新加载日程列表；请重新打开并确认最新内容。");
      await load(true);
      return;
    }
    savingDraftRef.current = true;
    setSavingDraft(true);
    try {
      if (editingSchedule) {
        await actions.update(editingSchedule, {
          title: submittedDraft.title.trim(),
          startAt,
          endAt,
          roomUrl: submittedDraft.roomUrl.trim() || null,
        });
      } else {
        const streamer = subscriptions.find((value) => value.id === submittedDraft.streamerId);
        await actions.create({
          streamerId: submittedDraft.streamerId,
          title: submittedDraft.title.trim(),
          startAt,
          endAt,
          roomUrl: submittedDraft.roomUrl.trim() || streamer?.liveUrl || null,
          sourceKind: "manual",
          sourcePostId: null,
          sourceImageUrl: null,
          sourceAuthorMid: null,
          sourceFingerprint: null,
          sourceIdentity: submittedDraft.sourceIdentity,
          confidence: 1,
        });
      }
    } catch (saveError) {
      onError(explainError(saveError));
      return;
    } finally {
      savingDraftRef.current = false;
      setSavingDraft(false);
    }
    setDraft(null);
    await load(true);
  }

  async function removeSchedule(schedule: Schedule) {
    setMenu(null);
    if (removingScheduleIdsRef.current.has(schedule.id)) {
      return;
    }
    const message = schedule.sourceKind === "weekly"
      ? `跳过本次固定周表「${schedule.title}」？后续周次不会受影响。`
      : `删除「${schedule.title}」？`;
    if (!window.confirm(message)) {
      return;
    }
    removingScheduleIdsRef.current.add(schedule.id);
    try {
      await actions.remove(schedule);
      await load(true);
    } catch (removeError) {
      onError(explainError(removeError));
    } finally {
      removingScheduleIdsRef.current.delete(schedule.id);
    }
  }

  // @decision LIVE-ROOM-1 进入直播间统一按 日程直播间地址 → 主播直播间地址 → 主播房间号 解析，
  // 点击卡片和右键菜单共用同一个目标，避免两条入口落到不同房间。
  function resolveRoomUrl(schedule: Schedule): string | null {
    if (schedule.roomUrl) {
      return schedule.roomUrl;
    }
    const streamer = subscriptions.find((value) => value.id === schedule.streamerId);
    if (streamer?.liveUrl) {
      return streamer.liveUrl;
    }
    const roomId = streamer?.roomId ?? schedule.roomOwnerMid;
    return roomId ? `https://live.bilibili.com/${roomId}` : null;
  }

  async function enterLiveRoom(schedule: Schedule) {
    setMenu(null);
    const roomUrl = resolveRoomUrl(schedule);
    if (!roomUrl) {
      onError(`「${schedule.title}」还没有直播间地址，可在右键菜单里选择“编辑”补充。`);
      return;
    }
    try {
      await openExternalUrl(roomUrl);
    } catch (openError) {
      onError(explainError(openError));
    }
  }

  // @decision SCHEDULE-INTERACTION-2 右键菜单的“查看个人主页”按主播 UID 打开 space 主页，
  // 和主播库右键的同一入口保持一致；UID 缺失时不跳转，避免落到错误账号。
  function resolveProfileMid(schedule: Schedule): string | null {
    const streamer = subscriptions.find((value) => value.id === schedule.streamerId);
    const mid = (streamer?.mid ?? schedule.roomOwnerMid ?? "").trim();
    return mid ? mid : null;
  }

  async function openStreamerProfile(schedule: Schedule) {
    setMenu(null);
    const mid = resolveProfileMid(schedule);
    if (!mid) {
      onError(`「${schedule.streamerName}」缺少 UID，无法打开个人主页。`);
      return;
    }
    try {
      await openExternalUrl(`https://space.bilibili.com/${encodeURIComponent(mid)}`);
    } catch (openError) {
      onError(explainError(openError));
    }
  }

  // @decision SCHEDULE-INTERACTION-1 打开直播间绑定在双击上：卡片单击只做聚焦和长按菜单，
  // 双击（或键盘 Enter）才跳外部直播间，避免拖动时间线、长按菜单或误触时被浏览器抢走焦点。
  async function openSchedule(schedule: Schedule) {
    if (suppressCardClickRef.current) {
      suppressCardClickRef.current = false;
      longPressTriggered.current = false;
      if (suppressClickTimerRef.current !== null) {
        clearTimeout(suppressClickTimerRef.current);
        suppressClickTimerRef.current = null;
      }
      return;
    }
    const roomUrl = resolveRoomUrl(schedule);
    if (!roomUrl) {
      onError("该日程没有配置直播间地址");
      return;
    }
    try {
      await openExternalUrl(roomUrl);
    } catch (openError) {
      onError(explainError(openError));
    }
  }

  function closeLiveData() {
    setOnlineChartOpen(false);
    liveRequestRef.current += 1;
    setLiveModal(null);
  }

  async function openLiveData(schedule: Schedule) {
    setOnlineChartOpen(false);
    const requestId = ++liveRequestRef.current;
    setMenu(null);
    setLiveModal({ schedule, session: null, loading: true });
    try {
      const session = await actions.liveSession(schedule.id);
      if (liveRequestRef.current !== requestId) {
        return;
      }
      setLiveModal({ schedule, session, loading: false });
    } catch (liveError) {
      if (liveRequestRef.current !== requestId) {
        return;
      }
      setLiveModal(null);
      onError(explainError(liveError));
    }
  }
  function showMenu(schedule: Schedule, x: number, y: number) {
    setMenu({ schedule, x, y });
  }

  function beginLongPress(schedule: Schedule, x: number, y: number) {
    if (longPress.current !== null) {
      clearTimeout(longPress.current);
    }
    if (suppressClickTimerRef.current !== null) {
      clearTimeout(suppressClickTimerRef.current);
      suppressClickTimerRef.current = null;
    }
    longPressTriggered.current = false;
    longPressStartRef.current = { x, y };
    longPress.current = setTimeout(() => {
      longPress.current = null;
      longPressTriggered.current = true;
      suppressCardClickRef.current = true;
      showMenu(schedule, x, y);
    }, 550);
  }

  function endLongPress() {
    if (longPress.current !== null) {
      clearTimeout(longPress.current);
      longPress.current = null;
    }
    longPressStartRef.current = null;
    if (!longPressTriggered.current) {
      return;
    }
    if (suppressClickTimerRef.current !== null) {
      clearTimeout(suppressClickTimerRef.current);
    }
    suppressClickTimerRef.current = setTimeout(() => {
      suppressClickTimerRef.current = null;
      suppressCardClickRef.current = false;
      longPressTriggered.current = false;
    }, 350);
  }

  function suppressCardClickBriefly() {
    suppressCardClickRef.current = true;
    if (suppressClickTimerRef.current !== null) {
      clearTimeout(suppressClickTimerRef.current);
    }
    suppressClickTimerRef.current = setTimeout(() => {
      suppressClickTimerRef.current = null;
      suppressCardClickRef.current = false;
    }, 120);
  }

  function cardScheduleAtPoint(x: number, y: number): Schedule | null {
    if (typeof document.elementFromPoint !== "function") return null;
    const hit = document.elementFromPoint(x, y);
    const card = hit instanceof Element ? hit.closest("[data-schedule-id]") : null;
    const raw = card?.getAttribute("data-schedule-id");
    if (!raw) return null;
    const id = Number(raw);
    if (!Number.isFinite(id)) return null;
    return schedules.find((value) => value.id === id) ?? null;
  }

  /**
   * 拖动容器在个别浏览器/输入设备上仍可能把 dblclick 重定向到容器自身，
   * 这里按坐标命中卡片兜底，保证「双击日程卡片进入对应直播间」不会静默失效。
   */
  function handleBoardDoubleClick(event: ReactMouseEvent<HTMLDivElement>) {
    if (event.target !== event.currentTarget) return;
    const target = cardScheduleAtPoint(event.clientX, event.clientY);
    if (target) {
      void openSchedule(target);
    }
  }

  function resetPanPositions() {
    setGridPanRemainder(0);
  }

  function shiftAnchor(delta: number) {
    resetPanPositions();
    setAnchor((value) => {
      const next = addDays(value, delta);
      timelineScrollTargetRef.current = localDateKey(next);
      return next;
    });
  }

  function navigateToDate(day: Date) {
    resetPanPositions();
    timelineScrollTargetRef.current = localDateKey(day);
    setAnchor(day);
  }

  // @decision TL-SCROLL-1 时间线使用原生 scrollTop 作为唯一位移坐标，避免锚点、像素余量和布局测量互相补偿产生回弹。
  function beginTimelineSwipe(event: ReactPointerEvent<HTMLDivElement>) {
    if (event.button !== 0 || !(event.target instanceof Element)) return;
    if (event.target.closest("button, a, input, textarea, select")) return;
    if (event.pointerType === "touch") return;
    const viewport = timelineViewportRef.current;
    if (!viewport) return;
    // @decision SCHEDULE-DBLCLICK-1 指针捕获推迟到真正开始拖动时再设置：按下即捕获会把
    // 兼容的 mouseup 重定向到容器，浏览器据此算出的 click/dblclick 目标是容器而不是卡片，
    // 卡片上的「双击进入直播间」会静默失效。
    timelineSwipeRef.current = {
      startY: event.clientY,
      startScrollTop: viewport.scrollTop,
      moved: false,
      pointerId: event.pointerId,
    };
  }

  function moveTimelineSwipe(event: ReactPointerEvent<HTMLDivElement>) {
    const swipe = timelineSwipeRef.current;
    if (!swipe) return;
    const deltaY = event.clientY - swipe.startY;
    if (Math.abs(deltaY) > 6) {
      if (!swipe.moved) {
        try {
          event.currentTarget.setPointerCapture(swipe.pointerId);
        } catch {
          // Pointer capture keeps a drag working outside the timeline; it is not required.
        }
      }
      swipe.moved = true;
      endLongPress();
      timelineRef.current?.classList.add("is-dragging");
      const viewport = timelineViewportRef.current;
      if (viewport) {
        viewport.scrollTop = timelineDragScrollTop(
          swipe.startScrollTop,
          deltaY,
          viewport.scrollHeight - viewport.clientHeight,
        );
      }
    }
  }

  function finishTimelineSwipe(event: ReactPointerEvent<HTMLDivElement>) {
    const swipe = timelineSwipeRef.current;
    timelineSwipeRef.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    if (!swipe) return;
    timelineRef.current?.classList.remove("is-dragging");
    if (swipe.moved) suppressCardClickBriefly();
    captureTimelineTopAnchor();
    scheduleTimelineMeasurement();
    const topKey = timelineTopAnchorRef.current?.key;
    const topDay = topKey ? gridWindowDays.find((day) => localDateKey(day) === topKey) : null;
    if (topDay && localDateKey(topDay) !== localDateKey(anchor)) {
      setAnchor(topDay);
    }
  }

  function cancelTimelineSwipe() {
    timelineSwipeRef.current = null;
    timelineRef.current?.classList.remove("is-dragging");
  }

  function gridDayStep(): number {
    return gridDayWidth + GRID_GAP;
  }

  function dragDaysForDelta(deltaX: number): number {
    if (Math.abs(deltaX) < GRID_DRAG_THRESHOLD) return 0;
    const raw = Math.round(deltaX / gridDayStep());
    return raw === 0 ? Math.sign(deltaX) : raw;
  }

  function beginGridDrag(event: ReactPointerEvent<HTMLDivElement>) {
    if (event.button !== 0 || !(event.target instanceof Element)) return;
    if (event.target.closest("button, a, input, textarea, select")) return;
    setDragDayDelta(0);
    // 同 SCHEDULE-DBLCLICK-1：拖动开始前不做指针捕获，否则卡片的 click/dblclick 会被容器接管。
    gridDragRef.current = { startX: event.clientX, deltaX: 0, pointerId: event.pointerId, captured: false };
  }

  function moveGridDrag(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = gridDragRef.current;
    if (!drag) return;
    const deltaX = event.clientX - drag.startX;
    drag.deltaX = deltaX;
    if (Math.abs(deltaX) > 6) {
      if (!drag.captured) {
        try {
          event.currentTarget.setPointerCapture(drag.pointerId);
        } catch {
          // Pointer capture improves dragging outside the grid; the drag still works without it.
        }
        drag.captured = true;
      }
      endLongPress();
      gridRef.current?.classList.add("is-dragging");
      gridRef.current?.style.setProperty("--drag-x", `${deltaX}px`);
      setDragDayDelta(-dragDaysForDelta(deltaX));
    }
  }

  function finishGridDrag(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = gridDragRef.current;
    gridDragRef.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    if (!drag) return;
    if (Math.abs(drag.deltaX) <= 6) {
      gridRef.current?.classList.remove("is-dragging");
      gridRef.current?.style.removeProperty("--drag-x");
      setDragDayDelta(0);
      return;
    }
    suppressCardClickBriefly();
    const daysMoved = dragDaysForDelta(drag.deltaX);
    const nextRemainder = gridPanRemainder + drag.deltaX - daysMoved * gridDayStep();
    flushSync(() => {
      setGridPanRemainder(nextRemainder);
      if (daysMoved !== 0) {
        setAnchor((value) => addDays(value, -daysMoved));
      }
      setDragDayDelta(0);
    });
    gridRef.current?.classList.remove("is-dragging");
    gridRef.current?.style.removeProperty("--drag-x");
  }

  function cancelGridDrag() {
    gridDragRef.current = null;
    gridRef.current?.classList.remove("is-dragging");
    gridRef.current?.style.removeProperty("--drag-x");
    setDragDayDelta(0);
  }

  function renderCard(schedule: Schedule) {
    const isLiveSchedule = liveScheduleIds.has(schedule.id);
    const participants = schedule.participants ?? [];
    const isGroup = participants.length > 1;
    const displayName = isGroup ? participants.join(" × ") : schedule.streamerName;
    return (
      <article
        key={schedule.id}
        className={`schedule-card ${isLiveSchedule ? "is-live" : ""}`}
        data-schedule-id={schedule.id}
        tabIndex={0}
        role="button"
        title={`双击进入直播间 · ${displayName}`}
        onDoubleClick={(event) => {
          if (event.target instanceof Element && event.target.closest("button, a, input, textarea, select")) {
            return;
          }
          void openSchedule(schedule);
        }}
        onKeyDown={(event) => {
          if (event.target !== event.currentTarget) return;
          if (event.key !== "Enter" && event.key !== " ") return;
          event.preventDefault();
          void openSchedule(schedule);
        }}
        onContextMenu={(event) => {
          event.preventDefault();
          showMenu(schedule, event.clientX, event.clientY);
        }}
        onPointerDown={(event) => {
          if (event.button !== 0) return;
          if (event.target instanceof Element && event.target.closest("button, a, input, textarea, select")) {
            return;
          }
          beginLongPress(schedule, event.clientX, event.clientY);
        }}
        onPointerMove={(event) => {
          const start = longPressStartRef.current;
          if (!start) return;
          if (Math.hypot(event.clientX - start.x, event.clientY - start.y) > 8) {
            endLongPress();
          }
        }}
        onPointerUp={endLongPress}
        onPointerLeave={endLongPress}
        onPointerCancel={endLongPress}
      >
        {viewMode === "timeline" ? (
          <div className="schedule-time">
            {timeLabel(schedule.startAt)}
            {schedule.endAt ? <small>{timeLabel(schedule.endAt)}</small> : null}
          </div>
        ) : null}
        <Avatar
          url={schedule.streamerAvatar}
          name={schedule.streamerName}
          size="md"
          live={isLiveSchedule}
        />
        <div className="schedule-content">
          <div className="schedule-streamer">
            {isGroup ? <span className="group-label">团播</span> : null}
            <span title={displayName}>{displayName}</span>
            {isLiveSchedule ? <span className="live-label">直播中</span> : null}
          </div>
          <div className={`schedule-title-line ${viewMode === "grid" ? "is-compact" : ""}`}>
            {viewMode === "grid" ? (
              <span className="schedule-time-inline">
                {timeLabel(schedule.startAt)}
                {schedule.endAt ? <small>{timeLabel(schedule.endAt)}</small> : null}
              </span>
            ) : null}
            <h3>{schedule.title}</h3>
          </div>
          <div className="schedule-meta">
            <span className={`source-badge ${SOURCE_CLASS[schedule.sourceKind]}`}>
              {SOURCE_LABELS[schedule.sourceKind] ?? schedule.sourceKind}
            </span>
            {schedule.roomOwnerName ? (
              <span className="tiny-badge room-owner" title="直播间所属账号">
                直播间：{schedule.roomOwnerName}
              </span>
            ) : null}
            {schedule.manualLocked ? <span className="tiny-badge">手动锁定</span> : null}
            {schedule.sourceChanged ? <span className="tiny-badge warning">来源有变化</span> : null}
          </div>
        </div>
        <button
          className="icon-button card-more"
          aria-label="更多操作"
          onClick={(event) => {
            event.stopPropagation();
            const rect = event.currentTarget.getBoundingClientRect();
            showMenu(schedule, rect.right - 180, rect.bottom + 4);
          }}
        >
          <MoreHorizontal size={17} />
        </button>
      </article>
    );
  }

  const rangeText = rangeDays.length === 1
    ? `${rangeDays[0].getMonth() + 1}月${rangeDays[0].getDate()}日`
    : `${rangeDays[0].getMonth() + 1}月${rangeDays[0].getDate()}日 - ${rangeDays[rangeDays.length - 1].getMonth() + 1}月${rangeDays[rangeDays.length - 1].getDate()}日`;
  const liveEventsUnavailable = liveModal?.session ? liveCollectionFailed(liveModal.session) : false;
  const menuProfileMid = menu ? resolveProfileMid(menu.schedule) : null;

  return (
    <section ref={pageRef} className="page schedule-page" aria-label="直播日程">
      <div
        ref={scheduleStickyRef}
        className={`sticky-page-header schedule-sticky ${scheduleStickyPinned ? "is-pinned" : ""}`}
      >
        <header className="page-header schedule-toolbar">
          <div>
            <h1 className="schedule-title">直播日程</h1>
            <p className="page-subtitle" aria-live="polite">{rangeText} · {viewMode === "grid" ? `${visibleDays.length} 天 · 左右拖动浏览` : `${Math.max(1, timelineVisibleDays.length)} 天 · 上下拖动浏览`}</p>
          </div>
          <div className="toolbar-actions">
            {viewMode === "timeline" ? (
              <div className="calendar-width-control" role="group" aria-label="调整每行日程数量">
                <span className="calendar-width-label timeline-columns-label">每行</span>
                <button
                  type="button"
                  onClick={() => changeTimelineColumns(-1)}
                  disabled={effectiveTimelineColumns <= TIMELINE_COLUMNS_MIN}
                  title="减少每行日程"
                  aria-label="减少每行日程数量"
                >
                  <Minus size={14} />
                </button>
                <strong className="calendar-control-value">{effectiveTimelineColumns}</strong>
                <button
                  type="button"
                  onClick={() => changeTimelineColumns(1)}
                  disabled={effectiveTimelineColumns >= timelineColumnsMax}
                  title="增加每行日程"
                  aria-label="增加每行日程数量"
                >
                  <Plus size={14} />
                </button>
              </div>
            ) : (
              <div className="calendar-width-control" role="group" aria-label="调整日历栏宽">
                <span className="calendar-width-label">栏宽</span>
                <button
                  type="button"
                  onClick={() => changeGridDayWidth(-GRID_DAY_WIDTH_STEP)}
                  disabled={gridDayWidth <= GRID_DAY_WIDTH_MIN}
                  title="缩窄栏宽"
                  aria-label="缩窄栏宽"
                >
                  <Minus size={14} />
                </button>
                <button
                  type="button"
                  onClick={() => changeGridDayWidth(GRID_DAY_WIDTH_STEP)}
                  disabled={gridDayWidth >= GRID_DAY_WIDTH_MAX}
                  title="加宽栏宽"
                  aria-label="加宽栏宽"
                >
                  <Plus size={14} />
                </button>
              </div>
            )}
            <div className="segmented" aria-label="视图模式">
              <button
                type="button"
                className={viewMode === "timeline" ? "active" : ""}
                onClick={() => changeViewMode("timeline")}
                aria-label="切换到时间线"
                aria-pressed={viewMode === "timeline"}
                title="时间线"
              >
                <List size={17} />
              </button>
              <button
                type="button"
                className={viewMode === "grid" ? "active" : ""}
                onClick={() => changeViewMode("grid")}
                aria-label="切换到日历网格"
                aria-pressed={viewMode === "grid"}
                title="日历网格"
              >
                <Grid3X3 size={17} />
              </button>
            </div>
            <button className="button secondary" onClick={() => navigateToDate(new Date())}>
              <RotateCcw size={16} />
              回到今天
            </button>
            <button className="button secondary" onClick={() => void refresh()} disabled={refreshing}>
              <RefreshCw size={16} className={refreshing ? "spin" : ""} />
              刷新
            </button>
            <button className="button primary" onClick={openCreate} disabled={!subscriptions.length}>
              <Plus size={17} />
              新增日程
            </button>
          </div>
        </header>

        <div className="day-navigator">
          <button className="icon-button" onClick={() => shiftAnchor(-1)} aria-label="前移一天">
            <ChevronLeft size={19} />
          </button>
          <div className="day-strip" style={{ gridTemplateColumns: `repeat(${stripDays.length}, minmax(56px, 1fr))` }}>
            {stripDays.map((day, index) => (
              <button
                key={localDateKey(day)}
                className={`day-pill ${index === 0 ? "active" : ""}`}
                aria-current={index === 0 ? "date" : undefined}
                onClick={() => navigateToDate(day)}
              >
                <span>{["日", "一", "二", "三", "四", "五", "六"][day.getDay()]}</span>
                <strong>{day.getDate()}</strong>
              </button>
            ))}
          </div>
          <button className="icon-button" onClick={() => shiftAnchor(1)} aria-label="后移一天">
            <ChevronRight size={19} />
          </button>
        </div>
      </div>

      <div
        key={viewMode}
        className={`schedule-view-surface${viewEnterAnimation ? " is-entering" : ""}`}
        onAnimationEnd={(event) => {
          if (event.target === event.currentTarget) setViewEnterAnimation(false);
        }}
      >
      {error ? <ErrorBanner message={error} onRetry={() => void load(true)} /> : null}

      {loading ? (
        <div className="schedule-skeleton">
          {Array.from({ length: 5 }, (_, index) => <div key={index} className="skeleton-row" />)}
        </div>
      ) : viewMode === "timeline" ? (
        <div className="timeline-shell">
          <div
            ref={timelineViewportRef}
            className="timeline-viewport"
            style={{ height: `${timelineViewportHeight}px` }}
            tabIndex={0}
          >
            <div
              ref={timelineRef}
              className="timeline"
              data-timeline-columns={effectiveTimelineColumns}
              style={{
                "--timeline-columns": effectiveTimelineColumns,
                "--timeline-gap": `${TIMELINE_GAP}px`,
              } as CSSProperties}
              role="group"
              aria-label={`时间线，当前显示 ${Math.max(1, timelineVisibleDays.length)} 天，可上下拖动浏览`}
              title="按住上下拖动浏览；松手后停留当前位置"
              onPointerDown={beginTimelineSwipe}
              onPointerMove={moveTimelineSwipe}
              onPointerUp={finishTimelineSwipe}
              onPointerCancel={cancelTimelineSwipe}
              onDoubleClick={handleBoardDoubleClick}
            >
              {gridWindowDays.map((day) => {
                const values = grouped.get(localDateKey(day)) ?? [];
                const isVisible = timelineVisibleKeySet.size === 0
                  ? true
                  : timelineVisibleKeySet.has(localDateKey(day));
                return (
                  <section
                    className="day-section"
                    key={localDateKey(day)}
                    data-timeline-day={localDateKey(day)}
                    aria-hidden={!isVisible}
                    inert={!isVisible ? true : undefined}
                  >
                    <div className="day-heading" title="上下拖动浏览">
                      <span>{day.getMonth() + 1}月{day.getDate()}日</span>
                      <strong>{["周日", "周一", "周二", "周三", "周四", "周五", "周六"][day.getDay()]}</strong>
                      <em>{values.length} 场</em>
                      <GripVertical className="day-heading-grip" size={14} aria-hidden="true" />
                    </div>
                    <div className="day-events">
                      {values.length ? values.map(renderCard) : <div className="day-empty">暂无安排</div>}
                    </div>
                  </section>
                );
              })}
            </div>
          </div>
          {visibleScheduleCount === 0 ? (
            <div className="timeline-empty-overlay" aria-live="polite">
              <EmptyState
                title={displayDays.length === 1 ? "这一天还没有日程" : `这 ${displayDays.length} 天还没有日程`}
                description={subscriptions.length ? "可以刷新订阅，或手动新增一条日程；仍可上下拖动浏览。" : "先搜索并订阅一位主播。"}
                action={!subscriptions.length ? <span className="muted">前往“主播库”添加订阅</span> : null}
              />
            </div>
          ) : null}
        </div>
      ) : (
        <div className="calendar-grid-wrap">
          <div className="calendar-viewport">
            <div
              ref={gridRef}
              className="calendar-grid"
              style={{ "--grid-day-w": `${gridDayWidth}px`, "--grid-offset": `${gridOffset}px` } as CSSProperties}
              data-visible-days={visibleDays.length}
              data-window-days={gridWindowDays.length}
              role="group"
              aria-label={`日历网格，当前显示 ${visibleDays.length} 天，可左右拖动浏览`}
              title="按住左右拖动浏览；松手后停留当前位置"
              onPointerDown={beginGridDrag}
              onPointerMove={moveGridDrag}
              onPointerUp={finishGridDrag}
              onPointerCancel={cancelGridDrag}
              onDoubleClick={handleBoardDoubleClick}
            >
              {gridWindowDays.map((day, index) => {
                const values = grouped.get(localDateKey(day)) ?? [];
                const isVisible = index >= visibleStartIndex - (peekBefore ? 1 : 0)
                  && index < visibleStartIndex + visibleDayCount + (peekAfter ? 1 : 0);
                return (
                  <section
                    className="grid-day"
                    key={localDateKey(day)}
                    aria-hidden={!isVisible}
                    inert={!isVisible ? true : undefined}
                  >
                    <header>
                      <strong>{day.getDate()}</strong>
                      <span>{["周日", "周一", "周二", "周三", "周四", "周五", "周六"][day.getDay()]}</span>
                    </header>
                    <div>{values.map(renderCard)}</div>
                  </section>
                );
              })}
            </div>
          </div>
          {visibleScheduleCount === 0 ? (
            <span className="calendar-empty-hint">这几天暂无日程 · 拖动可继续查看</span>
          ) : null}
        </div>
      )}
      </div>

      {menu ? (
        <div
          ref={menuRef}
          className="context-menu"
          role="menu"
          aria-label="日程操作"
          style={{ left: menu.x, top: menu.y }}
          onClick={(event) => event.stopPropagation()}
        >
          <button
            className={`live-room-entry${liveScheduleIds.has(menu.schedule.id) ? " is-live" : ""}`}
            title={resolveRoomUrl(menu.schedule) ?? "该日程还没有直播间地址"}
            onClick={() => void enterLiveRoom(menu.schedule)}
          >
            <MonitorPlay size={15} />进入直播间
          </button>
          <button onClick={() => void openLiveData(menu.schedule)}><BarChart3 size={15} />查看直播数据</button>
          {menu.schedule.sourcePostId ? (
            <button onClick={() => void openExternalUrl(`https://t.bilibili.com/${menu.schedule.sourcePostId}`).catch((error) => onError(explainError(error)))}>
              <ExternalLink size={15} />查看来源
            </button>
          ) : null}
          {menuProfileMid ? (
            <button
              onClick={() => void openStreamerProfile(menu.schedule)}
              title={`https://space.bilibili.com/${menuProfileMid}`}
            >
              <UserRound size={15} />查看个人主页
            </button>
          ) : null}
          <button onClick={() => openEdit(menu.schedule)}><Pencil size={15} />编辑</button>
          <button onClick={() => void removeSchedule(menu.schedule)} className="danger">
            <Trash2 size={15} />
            {menu.schedule.sourceKind === "weekly" ? "跳过本次" : "删除"}
          </button>
        </div>
      ) : null}

      {liveModal ? (
        <Modal
          title={onlineChartOpen ? "同接变化" : "直播数据"}
          onClose={closeLiveData}
          wide
          className={onlineChartOpen ? "online-chart-modal" : undefined}
        >
          {liveModal.loading ? (
            <div className="live-stats-loading"><LoaderCircle className="spin" size={24} />正在读取直播数据…</div>
          ) : liveModal.session && onlineChartOpen ? (
            <LiveOnlineChart key={liveModal.session.id} session={liveModal.session}
              loadPage={actions.liveOnlineSamples} onBack={() => {
                setOnlineChartOpen(false);
                requestAnimationFrame(() => peakCardRef.current?.focus());
              }} />
          ) : liveModal.session ? (
            <div className="live-stats-panel">
              <div className="live-stats-head">
                <Avatar url={liveModal.schedule.streamerAvatar} name={liveModal.schedule.streamerName} size="md" />
                <div>
                  <strong>{liveModal.session.title || liveModal.schedule.title}</strong>
                  <span>
                    {formatLiveTime(liveModal.session.startedAt)}
                    {liveModal.session.endedAt ? ` - ${formatLiveTime(liveModal.session.endedAt)}` : " · 直播中"}
                  </span>
                </div>
                <em
                  className={`live-data-quality ${liveEventsUnavailable ? "failed" : liveModal.session.dataQuality}`}
                  title={liveQualityDetail(liveModal.session)}
                >
                  {liveQualityLabel(liveModal.session)}
                </em>
              </div>
              <div className="live-stat-grid">
                <div><span>时长</span><strong>{formatLiveDuration(liveModal.session.durationSeconds)}</strong></div>
                <div><span>营收(估)</span><strong>{liveEventsUnavailable ? "—" : formatMoney(liveModal.session.revenueCents)}</strong></div>
                <div><span>付费人数(估)</span><strong>{liveEventsUnavailable ? "—" : formatCount(liveModal.session.paidUserCount)}</strong></div>
                <button type="button" ref={peakCardRef} className="live-stat-chart-card"
                  onClick={() => setOnlineChartOpen(true)} aria-label="查看最高同时在线人数变化曲线"
                  title="查看公开高能用户数的采样变化曲线">
                  <span>最高同时在线<ChevronRight size={13} aria-hidden="true" /></span>
                  <strong>{liveModal.session.peakHighEnergy > 0 ? formatCount(liveModal.session.peakHighEnergy) : "—"}</strong>
                  <small><BarChart3 size={12} aria-hidden="true" />查看变化曲线</small>
                </button>
                <div><span>互动人数(估)</span><strong>{liveEventsUnavailable ? "—" : formatCount(liveModal.session.interactiveUserCount)}</strong></div>
                <div><span>弹幕</span><strong>{liveEventsUnavailable ? "—" : formatCount(liveModal.session.danmakuCount)}</strong></div>
              </div>
              <div className="live-revenue-breakdown">
                <span>礼物 {liveEventsUnavailable ? "—" : formatMoney(liveModal.session.giftRevenueCents)}</span>
                <span>SC {liveEventsUnavailable ? "—" : formatMoney(liveModal.session.superChatRevenueCents)}</span>
                <span>舰长 {liveEventsUnavailable ? "—" : formatMoney(liveModal.session.guardRevenueCents)}</span>
              </div>
              {liveEventsUnavailable ? (
                <p className="form-hint live-stats-error-note">
                  本次未能建立 B 站弹幕采集连接（重试 {formatCount(liveModal.session.reconnectCount)} 次），事件类指标不可用；时长仍来自直播状态轮询。
                </p>
              ) : null}
            </div>
          ) : (
            <EmptyState
              title="还没有这场直播的数据"
              description="需要先在设置中开启直播数据统计，并在主播库为对应主播订阅直播数据，直播期间保持采集端运行。"
            />
          )}
        </Modal>
      ) : null}
      {draft ? (
        <Modal
          title={draft.id !== null ? "编辑日程" : "新增日程"}
          onClose={closeDraft}
          footer={
            <>
              <button className="button secondary" onClick={closeDraft} disabled={savingDraft}>取消</button>
              <button className="button primary" onClick={() => void saveDraft()} disabled={savingDraft}>保存</button>
            </>
          }
        >
          <fieldset
            className="form-grid"
            disabled={savingDraft}
            style={{ border: 0, margin: 0, minWidth: 0, padding: 0 }}
            aria-busy={savingDraft}
          >
            <label>
              <span>主播</span>
              <SubscriptionSelect
                subscriptions={subscriptions}
                value={draft.streamerId}
                onChange={(streamerId) => {
                  const streamer = subscriptions.find((value) => value.id === streamerId);
                  updateDraft({ streamerId, roomUrl: streamer?.liveUrl ?? draft.roomUrl });
                }}
              />
            </label>
            <label>
              <span>主题</span>
              <input value={draft.title} onChange={(event) => updateDraft({ title: event.target.value })} placeholder="例如：周五杂谈" />
            </label>
            <label>
              <span>开始时间</span>
              <input type="datetime-local" value={draft.startAt} onChange={(event) => updateDraft({ startAt: event.target.value })} />
            </label>
            <label>
              <span>结束时间</span>
              <input type="datetime-local" value={draft.endAt} onChange={(event) => updateDraft({ endAt: event.target.value })} />
            </label>
            <label className="span-2">
              <span>直播间地址</span>
              <input value={draft.roomUrl} onChange={(event) => updateDraft({ roomUrl: event.target.value })} placeholder="https://live.bilibili.com/..." />
            </label>
          </fieldset>
          <p className="form-hint"><Clock3 size={14} /> 手动修改后的自动日程会锁定，不再被抓取结果覆盖。</p>
        </Modal>
      ) : null}
    </section>
  );
}










