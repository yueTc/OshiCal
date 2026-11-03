import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import { differenceInCalendarDays } from "date-fns";
import {
  ChevronLeft,
  BarChart3,
  ChevronRight,
  Clock3,
  Grid3X3,
  GripHorizontal,
  List,
  LoaderCircle,
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

function liveQualityLabel(quality: string): string {
  if (quality === "complete") return "数据完整";
  if (quality === "partial") return "部分缺失";
  return "采集中";
}
type ViewMode = "timeline" | "grid";

const CALENDAR_BUFFER_BEFORE = 14;
const CALENDAR_BUFFER_AFTER = 20;
const GRID_WINDOW_BEFORE = 14;
const GRID_WINDOW_DAYS = 35;
const GRID_GAP = 10;
const GRID_DRAG_THRESHOLD = 42;

function visibleDayCountForWidth(width: number): number {
  if (width >= 1040) return 7;
  if (width >= 840) return 5;
  if (width >= 500) return 3;
  return 1;
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
  refresh: () => Promise<void>;
}

interface Draft {
  id: number | null;
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
};

const SOURCE_CLASS: Record<SourceKind, string> = {
  text: "source-text",
  reserve: "source-reserve",
  image: "source-image",
  manual: "source-manual",
  editor: "source-text",
  weekly: "source-weekly",
};

function emptyDraft(subscriptions: Streamer[]): Draft {
  const first = subscriptions[0];
  return {
    id: null,
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
  const [viewMode, setViewMode] = useState<ViewMode>(() => {
    const stored = localStorage.getItem("oshical.viewMode");
    return stored === "grid" ? "grid" : "timeline";
  });
  const [schedules, setSchedules] = useState<Schedule[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [menu, setMenu] = useState<{ schedule: Schedule; x: number; y: number } | null>(null);
  const [liveModal, setLiveModal] = useState<{
    schedule: Schedule;
    session: LiveSession | null;
    loading: boolean;
  } | null>(null);
  const longPress = useRef<ReturnType<typeof setTimeout> | null>(null);
  const longPressTriggered = useRef(false);
  const pageRef = useRef<HTMLElement>(null);
  const gridRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const loadedWindowRef = useRef<{ from: number; to: number } | null>(null);
  const hasLoadedScheduleRef = useRef(false);
  const lastRevisionRef = useRef<number | null>(null);
  const loadRequestRef = useRef(0);
  const gridDragRef = useRef<{ startX: number; deltaX: number } | null>(null);
  const suppressCardClickRef = useRef(false);
  const gridWheelLockRef = useRef(false);
  const [visibleDayCount, setVisibleDayCount] = useState(7);
  const [calendarWidth, setCalendarWidth] = useState(0);
  const [gridWindowStart, setGridWindowStart] = useState(() => addDays(new Date(), -GRID_WINDOW_BEFORE));
  const [dragDayDelta, setDragDayDelta] = useState(0);
  const pendingDragReleaseRef = useRef(false);

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
    : timelineDays;
  const rangeDays = viewMode === "grid" ? stripDays : timelineDays;
  const displayDays = viewMode === "grid" ? visibleDays : timelineDays;
  const gridDayWidth = calendarWidth > 0
    ? Math.max(0, (calendarWidth - GRID_GAP * (visibleDayCount - 1)) / visibleDayCount)
    : 160;
  const gridOffset = -visibleStartIndex * (gridDayWidth + GRID_GAP);
  const liveScheduleIds = useMemo(
    () => matchLiveSchedules(schedules, liveStatuses),
    [schedules, liveStatuses],
  );

  const load = useCallback(async (force = false) => {
    const anchorTime = startOfLocalDay(anchor);
    const loaded = loadedWindowRef.current;
    const revisionChanged = lastRevisionRef.current !== revision;
    if (!force && !revisionChanged && loaded && anchorTime >= loaded.from && anchorTime < loaded.to) {
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
  }, [actions, anchor, onError, revision]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    localStorage.setItem("oshical.viewMode", viewMode);
  }, [viewMode]);

  useLayoutEffect(() => {
    const element = pageRef.current;
    if (!element) return;
    const update = () => {
      const width = element.clientWidth;
      if (width <= 0) return;
      setCalendarWidth(width);
      const next = visibleDayCountForWidth(width);
      setVisibleDayCount((current) => (current === next ? current : next));
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

  useEffect(() => {
    const offset = differenceInCalendarDays(anchor, gridWindowStart);
    const maxOffset = GRID_WINDOW_DAYS - visibleDayCount - 2;
    if (offset < 2 || offset > maxOffset) {
      setGridWindowStart(addDays(anchor, -GRID_WINDOW_BEFORE));
    }
  }, [anchor, gridWindowStart, visibleDayCount]);

  useLayoutEffect(() => {
    if (!pendingDragReleaseRef.current) return;
    pendingDragReleaseRef.current = false;
    const grid = gridRef.current;
    if (!grid) return;
    requestAnimationFrame(() => {
      grid.classList.remove("is-dragging");
      grid.style.removeProperty("--drag-x");
    });
  }, [anchorIndex]);

  useEffect(() => {
    if (!active || viewMode !== "grid") return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
      const target = event.target;
      if (target instanceof HTMLElement && target.closest("input, textarea, select, [contenteditable='true'], .modal, .context-menu")) return;
      if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
        event.preventDefault();
        const daysToMove = event.shiftKey ? 7 : 1;
        setAnchor((value) => addDays(value, event.key === "ArrowLeft" ? -daysToMove : daysToMove));
      } else if (event.key === "Home" || event.key.toLowerCase() === "t") {
        event.preventDefault();
        setAnchor(new Date());
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
      setAnchor((value) => addDays(value, delta > 0 ? 1 : -1));
      window.setTimeout(() => { gridWheelLockRef.current = false; }, 180);
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, [active, anchor, loading, schedules.length, viewMode]);

  useEffect(() => {
    if (active) return;
    setMenu(null);
    setDraft(null);
    setLiveModal(null);
  }, [active]);

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
    setRefreshing(true);
    try {
      await actions.refresh();
      await load(true);
    } catch (refreshError) {
      onError(explainError(refreshError));
    } finally {
      setRefreshing(false);
    }
  }

  function openCreate() {
    setDraft(emptyDraft(subscriptions));
  }

  function openEdit(schedule: Schedule) {
    setDraft({
      id: schedule.id,
      streamerId: schedule.streamerId,
      title: schedule.title,
      startAt: inputDateTime(schedule.startAt),
      endAt: schedule.endAt ? inputDateTime(schedule.endAt) : "",
      roomUrl: schedule.roomUrl ?? "",
    });
    setMenu(null);
  }

  async function saveDraft() {
    if (!draft || !draft.streamerId || !draft.title.trim() || !draft.startAt) {
      return;
    }
    const startAt = new Date(draft.startAt).toISOString();
    const endAt = draft.endAt ? new Date(draft.endAt).toISOString() : null;
    const editingSchedule = draft.id
      ? schedules.find((schedule) => schedule.id === draft.id) ?? null
      : null;
    try {
      if (draft.id && editingSchedule) {
        await actions.update(editingSchedule, {
          title: draft.title.trim(),
          startAt,
          endAt,
          roomUrl: draft.roomUrl.trim() || null,
        });
      } else {
        const streamer = subscriptions.find((value) => value.id === draft.streamerId);
        await actions.create({
          streamerId: draft.streamerId,
          title: draft.title.trim(),
          startAt,
          endAt,
          roomUrl: draft.roomUrl.trim() || streamer?.liveUrl || null,
          sourceKind: "manual",
          sourcePostId: null,
          sourceImageUrl: null,
          sourceAuthorMid: null,
          sourceFingerprint: null,
          sourceIdentity: `manual:${crypto.randomUUID()}`,
          confidence: 1,
        });
      }
      setDraft(null);
      await load(true);
    } catch (saveError) {
      onError(explainError(saveError));
    }
  }

  async function removeSchedule(schedule: Schedule) {
    setMenu(null);
    const message = schedule.sourceKind === "weekly"
      ? `跳过本次固定周表「${schedule.title}」？后续周次不会受影响。`
      : `删除「${schedule.title}」？`;
    if (!window.confirm(message)) {
      return;
    }
    try {
      await actions.remove(schedule);
      await load(true);
    } catch (removeError) {
      onError(explainError(removeError));
    }
  }

  async function openSchedule(schedule: Schedule) {
    if (suppressCardClickRef.current) {
      suppressCardClickRef.current = false;
      return;
    }
    if (longPressTriggered.current) {
      longPressTriggered.current = false;
      return;
    }
    if (!schedule.roomUrl) {
      onError("该日程没有配置直播间地址");
      return;
    }
    try {
      await openExternalUrl(schedule.roomUrl);
    } catch (openError) {
      onError(explainError(openError));
    }
  }

  async function openLiveData(schedule: Schedule) {
    setMenu(null);
    setLiveModal({ schedule, session: null, loading: true });
    try {
      const session = await actions.liveSession(schedule.id);
      setLiveModal({ schedule, session, loading: false });
    } catch (liveError) {
      setLiveModal(null);
      onError(explainError(liveError));
    }
  }
  function showMenu(schedule: Schedule, x: number, y: number) {
    setMenu({ schedule, x, y });
  }

  function beginLongPress(schedule: Schedule, x: number, y: number) {
    if (longPress.current) {
      clearTimeout(longPress.current);
    }
    longPress.current = setTimeout(() => {
      longPressTriggered.current = true;
      showMenu(schedule, x, y);
    }, 550);
  }

  function endLongPress() {
    if (longPress.current) {
      clearTimeout(longPress.current);
      longPress.current = null;
    }
  }

  function shiftAnchor(delta: number) {
    setAnchor((value) => addDays(value, delta));
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
    gridDragRef.current = { startX: event.clientX, deltaX: 0 };
    try {
      event.currentTarget.setPointerCapture(event.pointerId);
    } catch {
      // Pointer capture improves dragging outside the grid; the drag still works without it.
    }
  }

  function moveGridDrag(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = gridDragRef.current;
    if (!drag) return;
    const deltaX = event.clientX - drag.startX;
    drag.deltaX = deltaX;
    if (Math.abs(deltaX) > 6) {
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
    const daysMoved = dragDaysForDelta(drag.deltaX);
    if (Math.abs(drag.deltaX) > 6) {
      suppressCardClickRef.current = true;
      window.setTimeout(() => { suppressCardClickRef.current = false; }, 120);
    }
    if (daysMoved !== 0) {
      pendingDragReleaseRef.current = true;
      shiftAnchor(-daysMoved);
    } else {
      gridRef.current?.classList.remove("is-dragging");
      gridRef.current?.style.removeProperty("--drag-x");
    }
    setDragDayDelta(0);
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
        tabIndex={0}
        role="button"
        onClick={() => void openSchedule(schedule)}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            void openSchedule(schedule);
          }
        }}
        onContextMenu={(event) => {
          event.preventDefault();
          showMenu(schedule, event.clientX, event.clientY);
        }}
        onPointerDown={(event) => beginLongPress(schedule, event.clientX, event.clientY)}
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

  return (
    <section ref={pageRef} className="page schedule-page" aria-label="直播日程">
      <header className="page-header schedule-toolbar">
        <div>
          <h1 className="schedule-title">直播日程</h1>
          <p className="page-subtitle" aria-live="polite">{rangeText} · {viewMode === "grid" ? `${visibleDays.length} 天 · 左右滑动切换` : "连续 7 天"}</p>
        </div>
        <div className="toolbar-actions">
          <div className="segmented" aria-label="视图模式">
            <button
              className={viewMode === "timeline" ? "active" : ""}
              onClick={() => setViewMode("timeline")}
              title="时间线"
            >
              <List size={17} />
            </button>
            <button
              className={viewMode === "grid" ? "active" : ""}
              onClick={() => setViewMode("grid")}
              title="日历网格"
            >
              <Grid3X3 size={17} />
            </button>
          </div>
          <button className="button secondary" onClick={() => setAnchor(new Date())}>
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
              onClick={() => setAnchor(day)}
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

      {error ? <ErrorBanner message={error} onRetry={() => void load(true)} /> : null}

      {loading ? (
        <div className="schedule-skeleton">
          {Array.from({ length: 5 }, (_, index) => <div key={index} className="skeleton-row" />)}
        </div>
      ) : viewMode === "timeline" ? (
        visibleScheduleCount === 0 ? (
          <EmptyState
            title={displayDays.length === 1 ? "这一天还没有日程" : `这 ${displayDays.length} 天还没有日程`}
            description={subscriptions.length ? "可以刷新订阅，或手动新增一条日程。" : "先搜索并订阅一位主播。"}
            action={!subscriptions.length ? <span className="muted">前往“主播库”添加订阅</span> : null}
          />
        ) : (
        <div className="timeline">
          {timelineDays.map((day) => {
            const values = grouped.get(localDateKey(day)) ?? [];
            return (
              <section className="day-section" key={localDateKey(day)}>
                <div className="day-heading">
                  <span>{day.getMonth() + 1}月{day.getDate()}日</span>
                  <strong>{["周日", "周一", "周二", "周三", "周四", "周五", "周六"][day.getDay()]}</strong>
                  <em>{values.length} 场</em>
                </div>
                <div className="day-events">
                  {values.length ? values.map(renderCard) : <div className="day-empty">暂无安排</div>}
                </div>
              </section>
            );
          })}
        </div>
        )
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
              aria-label={`日历网格，当前显示 ${visibleDays.length} 天，左右拖动切换日期`}
              title="按住左右拖动切换日期，一次可跨多天"
              onPointerDown={beginGridDrag}
              onPointerMove={moveGridDrag}
              onPointerUp={finishGridDrag}
              onPointerCancel={cancelGridDrag}
            >
              {gridWindowDays.map((day, index) => {
                const values = grouped.get(localDateKey(day)) ?? [];
                const isVisible = index >= visibleStartIndex && index < visibleStartIndex + visibleDayCount;
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
          <span className={`calendar-drag-badge ${dragDayDelta !== 0 ? "is-active" : ""}`} aria-hidden="true">
            <GripHorizontal size={14} />
            <span>{dragDayDelta === 0
              ? "拖动切换日期 · 可跨多天"
              : `${dragDayDelta > 0 ? "向后" : "向前"} ${Math.abs(dragDayDelta)} 天`}</span>
          </span>
        </div>
      )}

      {menu ? (
        <div
          ref={menuRef}
          className="context-menu"
          role="menu"
          aria-label="日程操作"
          style={{ left: menu.x, top: menu.y }}
          onClick={(event) => event.stopPropagation()}
        >
          <button onClick={() => void openLiveData(menu.schedule)}><BarChart3 size={15} />查看直播数据</button>
          <button onClick={() => openEdit(menu.schedule)}><Pencil size={15} />编辑</button>
          <button onClick={() => void removeSchedule(menu.schedule)} className="danger">
            <Trash2 size={15} />
            {menu.schedule.sourceKind === "weekly" ? "跳过本次" : "删除"}
          </button>
          {menu.schedule.sourcePostId ? (
            <button onClick={() => void openExternalUrl(`https://t.bilibili.com/${menu.schedule.sourcePostId}`).catch((error) => onError(explainError(error)))}>
              <UserRound size={15} />查看来源
            </button>
          ) : null}
        </div>
      ) : null}

      {liveModal ? (
        <Modal
          title="直播数据"
          onClose={() => setLiveModal(null)}
          wide
          footer={<button className="button secondary" onClick={() => setLiveModal(null)}>关闭</button>}
        >
          {liveModal.loading ? (
            <div className="live-stats-loading"><LoaderCircle className="spin" size={24} />正在读取直播数据…</div>
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
                <em className={`live-data-quality ${liveModal.session.dataQuality}`}>
                  {liveQualityLabel(liveModal.session.dataQuality)}
                </em>
              </div>
              <div className="live-stat-grid">
                <div><span>时长</span><strong>{formatLiveDuration(liveModal.session.durationSeconds)}</strong></div>
                <div><span>营收(估)</span><strong>{formatMoney(liveModal.session.revenueCents)}</strong></div>
                <div><span>付费人数(估)</span><strong>{formatCount(liveModal.session.paidUserCount)}</strong></div>
                <div><span>峰值人气</span><strong>{formatCount(liveModal.session.peakOnline)}</strong></div>
                <div><span>互动人数(估)</span><strong>{formatCount(liveModal.session.interactiveUserCount)}</strong></div>
                <div><span>弹幕</span><strong>{formatCount(liveModal.session.danmakuCount)}</strong></div>
              </div>
              <div className="live-revenue-breakdown">
                <span>礼物 {formatMoney(liveModal.session.giftRevenueCents)}</span>
                <span>SC {formatMoney(liveModal.session.superChatRevenueCents)}</span>
                <span>舰长 {formatMoney(liveModal.session.guardRevenueCents)}</span>
              </div>
              <p className="form-hint">
                营收与付费人数由公开直播事件估算，不含平台分成和活动特殊规则；粉丝侧无法获取官方“高能”指标，因此展示峰值人气。
              </p>
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
          title={draft.id ? "编辑日程" : "新增日程"}
          onClose={() => setDraft(null)}
          footer={
            <>
              <button className="button secondary" onClick={() => setDraft(null)}>取消</button>
              <button className="button primary" onClick={() => void saveDraft()}>保存</button>
            </>
          }
        >
          <div className="form-grid">
            <label>
              <span>主播</span>
              <SubscriptionSelect
                subscriptions={subscriptions}
                value={draft.streamerId}
                onChange={(streamerId) => {
                  const streamer = subscriptions.find((value) => value.id === streamerId);
                  setDraft({ ...draft, streamerId, roomUrl: streamer?.liveUrl ?? draft.roomUrl });
                }}
              />
            </label>
            <label>
              <span>主题</span>
              <input value={draft.title} onChange={(event) => setDraft({ ...draft, title: event.target.value })} placeholder="例如：周五杂谈" />
            </label>
            <label>
              <span>开始时间</span>
              <input type="datetime-local" value={draft.startAt} onChange={(event) => setDraft({ ...draft, startAt: event.target.value })} />
            </label>
            <label>
              <span>结束时间</span>
              <input type="datetime-local" value={draft.endAt} onChange={(event) => setDraft({ ...draft, endAt: event.target.value })} />
            </label>
            <label className="span-2">
              <span>直播间地址</span>
              <input value={draft.roomUrl} onChange={(event) => setDraft({ ...draft, roomUrl: event.target.value })} placeholder="https://live.bilibili.com/..." />
            </label>
          </div>
          <p className="form-hint"><Clock3 size={14} /> 手动修改后的自动日程会锁定，不再被抓取结果覆盖。</p>
        </Modal>
      ) : null}
    </section>
  );
}










