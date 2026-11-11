import { useEffect, useId, useMemo, useRef, useState } from "react";
import { ChevronLeft, LoaderCircle, RefreshCw, TrendingUp } from "lucide-react";
import type { LiveOnlineSample, LiveSession, Platform } from "../types";
import {
  loadOnlineSamplePages, nearestOnlinePoint, onlinePoints, onlineSegments, onlineTime,
  platformFromLiveSession, thinOnlineSegment,
} from "../lib/live-online";
import type { OnlinePageLoader } from "../lib/live-online";
import { EmptyState, ErrorBanner } from "./ui";

const NUMBER = new Intl.NumberFormat("zh-CN");
const WIDTH = 720;
const HEIGHT = 256;
const LEFT = 54;
const RIGHT = WIDTH - 18;
const TOP = 20;
const BOTTOM = HEIGHT - 38;

function curveError(error: unknown): string {
  if (typeof error === "object" && error !== null && "status" in error && error.status === 404) {
    return "服务器不支持人数历史接口，或本场记录已不存在；请确认服务端为 0.3.30-beta.5 或更新版本。";
  }
  return error instanceof Error ? error.message : "读取人数曲线失败，请重试";
}

export function LiveOnlineChart({ session, loadPage, onBack }: {
  session: LiveSession;
  loadPage: OnlinePageLoader;
  onBack: () => void;
}) {
  const [samples, setSamples] = useState<LiveOnlineSample[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [retry, setRetry] = useState(0);
  const [hours, setHours] = useState(0);
  const [platform, setPlatform] = useState<Platform>(() => platformFromLiveSession(session));
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const refreshRef = useRef<(() => void) | null>(null);
  const titleId = useId();
  const descId = useId();

  useEffect(() => {
    let disposed = false;
    let busy = false;
    let cursor = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const controller = new AbortController();
    setSamples([]);
    setSelectedId(null);
    setLoading(true);
    setError("");
    const refresh = async () => {
      if (busy || disposed) return;
      if (timer) clearTimeout(timer);
      busy = true;
      setLoading(true);
      try {
        const page = await loadOnlineSamplePages(loadPage, session.id, cursor, controller.signal);
        if (disposed) return;
        cursor = page.nextAfterId;
        setPlatform(page.platform);
        setSamples((current) => [...current, ...page.samples]);
        setError("");
      } catch (cause) {
        if (!disposed) setError(curveError(cause));
      } finally {
        busy = false;
        if (!disposed) {
          setLoading(false);
          if (!session.endedAt) timer = setTimeout(() => void refresh(), 30_000);
        }
      }
    };
    refreshRef.current = () => { void refresh(); };
    void refresh();
    return () => {
      disposed = true;
      controller.abort();
      refreshRef.current = null;
      if (timer) clearTimeout(timer);
    };
  }, [session.id, session.endedAt, loadPage, retry]);

  const allPoints = useMemo(() => onlinePoints(samples), [samples]);
  const points = useMemo(() => {
    const last = allPoints.at(-1);
    if (!hours || !last) return allPoints;
    return allPoints.filter((point) => point.time >= last.time - hours * 3600_000);
  }, [allPoints, hours]);
  const segments = useMemo(() => onlineSegments(points), [points]);
  const valid = points.filter((point) => point.value !== null);
  const peak = valid.reduce((max, point) => Math.max(max, point.value ?? 0), 0);
  const maxY = Math.max(1, Math.ceil(peak / 4) * 4);
  const start = points[0]?.time ?? 0;
  const end = Math.max(start + 30_000, points.at(-1)?.time ?? start);
  const x = (time: number) => LEFT + (time - start) / (end - start) * (RIGHT - LEFT);
  const y = (value: number) => BOTTOM - value / maxY * (BOTTOM - TOP);
  const selectedIndex = Math.max(0, points.findIndex((point) => point.id === selectedId));
  const selected = selectedId === null ? points.at(-1) : points[selectedIndex];
  const currentIndex = selected ? points.indexOf(selected) : 0;

  return (
    <div className="online-chart-panel">
      <div className="online-chart-toolbar">
        <button type="button" className="text-button online-chart-back" onClick={onBack} autoFocus>
          <ChevronLeft size={16} />返回直播统计
        </button>
        <button type="button" className="button secondary online-chart-refresh" disabled={loading}
          onClick={() => refreshRef.current?.()} aria-label="刷新人数曲线">
          {loading ? <LoaderCircle size={14} className="spin" /> : <RefreshCw size={14} />}刷新
        </button>
      </div>
      <div className="online-chart-heading">
        <span className="online-chart-icon" aria-hidden="true"><TrendingUp size={21} /></span>
        <div><h3>{session.streamerName} · 人数走势</h3><p>{platform === "youtube" ? "YouTube 并发观看人数" : "B 站公开高能用户数"} · 北京时间 · 约每 30 秒采样{!session.endedAt ? " · 自动更新中" : " · 已结束"}</p></div>
      </div>
      {error ? <ErrorBanner message={error} onRetry={() => {
        if (samples.length) refreshRef.current?.();
        else setRetry((value) => value + 1);
      }} /> : null}
      {loading && samples.length === 0 ? (
        <div className="live-stats-loading" role="status"><LoaderCircle size={18} className="spin" />正在读取本场人数历史…</div>
      ) : points.length === 0 ? (
        <EmptyState title={error ? "人数历史暂不可用" : "暂无人数变化记录"}
          description={platform === "youtube"
            ? "YouTube 并发观看人数需要采集端持续运行；缺失采样不会补造为 0。"
            : "历史峰值无法还原为曲线。需要升级采集端，并在开启直播统计后保持采集；旧场次不会补造历史。"} />
      ) : (
        <>
          <div className="online-chart-summary">
            <div><span>已采样峰值</span><strong>{valid.length ? NUMBER.format(peak) : "—"}<small>人</small></strong></div>
            <div><span>{selected ? onlineTime(selected.time) : "采样时间"}</span>
              <strong>{selected?.value !== null && selected?.value !== undefined ? NUMBER.format(selected.value) : "—"}<small>{selected?.value === null ? "缺失" : "人"}</small></strong>
            </div>
            <div><span>有效 / 缺失采样点</span><strong className="online-chart-small-value">{NUMBER.format(valid.length)}<small>/ {NUMBER.format(points.length - valid.length)}</small></strong></div>
          </div>
          <div className="online-chart-ranges" role="group" aria-label="人数曲线时间范围">
            {[[0, "整场"], [1, "最近 1 小时"], [3, "最近 3 小时"]].map(([value, label]) => (
              <button type="button" key={value} aria-pressed={hours === value}
                onClick={() => { setHours(Number(value)); setSelectedId(null); }}>{label}</button>
            ))}
          </div>
          <div className="online-chart-canvas">
            <svg viewBox={"0 0 " + WIDTH + " " + HEIGHT} role="img"
              aria-labelledby={titleId + " " + descId}
              onPointerMove={(event) => {
                const box = event.currentTarget.getBoundingClientRect();
                if (!box.width) return;
                const position = (event.clientX - box.left) / box.width * WIDTH;
                const time = start + Math.max(0, Math.min(1, (position - LEFT) / (RIGHT - LEFT))) * (end - start);
                const index = nearestOnlinePoint(points, time);
                if (index >= 0) setSelectedId(points[index].id);
              }}
              onPointerLeave={() => setSelectedId(null)}>
              <title id={titleId}>{platform === "youtube" ? "本场 YouTube 并发观看人数变化曲线" : "本场公开高能用户数变化曲线"}</title>
              <desc id={descId}>横轴为北京时间，纵轴为人数。缺失数据和长时间未采集区间断开显示；可用下方时间滑块查看每个采样点。</desc>
              {[0, 1, 2, 3, 4].map((tick) => {
                const value = maxY * tick / 4;
                return <g key={tick} className="online-chart-grid">
                  <line x1={LEFT} x2={RIGHT} y1={y(value)} y2={y(value)} />
                  <text x={LEFT - 10} y={y(value) + 4} textAnchor="end">{NUMBER.format(value)}</text>
                </g>;
              })}
              {segments.map((segment) => {
                const drawing = thinOnlineSegment(segment);
                const coordinates = drawing.map((point) => x(point.time).toFixed(2) + "," + y(point.value ?? 0).toFixed(2));
                return <g key={segment[0].id}>
                  {drawing.length > 1 ? <>
                    <polygon className="online-chart-area" points={x(drawing[0].time) + "," + BOTTOM + " " + coordinates.join(" ") + " " + x(drawing.at(-1)!.time) + "," + BOTTOM} />
                    <polyline className="online-chart-line" points={coordinates.join(" ")} />
                  </> : <circle className="online-chart-dot" cx={x(drawing[0].time)} cy={y(drawing[0].value ?? 0)} r={3} />}
                </g>;
              })}
              {selected ? <g className="online-chart-selection">
                <line x1={x(selected.time)} x2={x(selected.time)} y1={TOP} y2={BOTTOM} />
                {selected.value !== null ? <circle cx={x(selected.time)} cy={y(selected.value)} r={5} /> : null}
              </g> : null}
              {[0, 0.5, 1].map((fraction) => <text className="online-chart-time" key={fraction}
                x={LEFT + (RIGHT - LEFT) * fraction} y={HEIGHT - 10}
                textAnchor={fraction === 0 ? "start" : fraction === 1 ? "end" : "middle"}>
                {onlineTime(start + (end - start) * fraction).slice(0, -3)}
              </text>)}
            </svg>
          </div>
          <label className="online-chart-scrubber">
            <span>查看采样时间点 <small>可拖动或使用方向键</small></span>
            <input type="range" min={0} max={Math.max(0, points.length - 1)} value={currentIndex}
              aria-label="查看人数采样时间点" aria-valuetext={selected ? onlineTime(selected.time) + "，" + (selected.value === null ? "采样缺失" : selected.value + " 人") : "暂无采样"}
              onChange={(event) => setSelectedId(points[Number(event.target.value)]?.id ?? null)} />
          </label>

        </>
      )}
    </div>
  );
}
