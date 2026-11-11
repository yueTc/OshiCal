import { useEffect, useRef, useState } from "react";
import { BarChart3, ChevronLeft, ChevronRight, LoaderCircle } from "lucide-react";
import type {
  LiveOnlineSamplesPage, LiveSession, LiveSuperChatTotal, Platform,
} from "../types";
import { platformFromLiveSession } from "../lib/live-online";
import { LiveOnlineChart } from "./LiveOnlineChart";
import { Avatar, EmptyState, Modal } from "./ui";

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

function formatSuperChatTotal(total: LiveSuperChatTotal): string {
  const amount = total.amountMicros / 1_000_000;
  if (!Number.isFinite(amount)) return total.currency ?? "—";
  if (!total.currency) return amount.toLocaleString("zh-CN");
  try {
    return new Intl.NumberFormat("zh-CN", {
      style: "currency",
      currency: total.currency,
      maximumFractionDigits: 2,
    }).format(amount);
  } catch {
    return `${total.currency} ${amount.toLocaleString("zh-CN")}`;
  }
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

function liveQualityLabel(
  session: LiveSession,
  platform: Platform,
  statsUnavailable: boolean,
): string {
  if (statsUnavailable || session.collectorState === "degraded") return "采集降级";
  if (platform === "bilibili" && liveCollectionFailed(session)) return "采集失败";
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

function liveQualityDetail(
  session: LiveSession,
  platform: Platform,
  youtubeStatsAvailable: boolean | null,
): string {
  if (platform === "youtube") {
    if (session.collectorState === "degraded" || youtubeStatsAvailable === false) {
      return "YouTube OAuth 授权失效或尚未授权，聊天、Super Chat 和会员统计已降级；并发观看人数和直播时长仍继续采样。";
    }
    if (session.dataQuality === "complete") return "YouTube 并发观看、聊天与付费事件统计完整。";
    if (session.dataQuality === "partial") {
      return "YouTube 并发观看人数持续采样；Super Chat 金额按原币种保存，不折算为人民币。";
    }
    return "YouTube 采集端正在统计，直播结束后给出完整度。";
  }
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

// 日程页与主播库共用同一数据口径、完整度提示和同接曲线。
export function LiveDataModal({
  session, loading = false, streamerAvatar, streamerName, title, loadPage, onClose, onBack,
}: {
  session: LiveSession | null;
  loading?: boolean;
  streamerAvatar: string;
  streamerName: string;
  title: string;
  loadPage: (sessionId: number, afterId: number, signal?: AbortSignal) => Promise<LiveOnlineSamplesPage>;
  onClose: () => void;
  onBack?: () => void;
}) {
  const [onlineChartOpen, setOnlineChartOpen] = useState(false);
  const [youtubeSummary, setYoutubeSummary] = useState<{
    membershipCount: number | null;
    statsAvailable: boolean | null;
    superChatTotals: LiveSuperChatTotal[];
  } | null>(null);
  const peakCardRef = useRef<HTMLButtonElement>(null);
  const loadPageRef = useRef(loadPage);
  loadPageRef.current = loadPage;
  const platform = session ? platformFromLiveSession(session) : "bilibili";
  const isYoutube = platform === "youtube";
  const liveEventsUnavailable = session && !isYoutube ? liveCollectionFailed(session) : false;
  const youtubeStatsUnavailable = isYoutube
    && (session?.collectorState === "degraded" || youtubeSummary?.statsAvailable === false);
  const youtubeStatsKnown = youtubeSummary?.statsAvailable === true
    && session?.collectorState !== "degraded";

  useEffect(() => {
    setYoutubeSummary(null);
    if (!session || !isYoutube || loading) return;
    let disposed = false;
    const controller = new AbortController();
    void loadPageRef.current(session.id, 0, controller.signal).then((page) => {
      if (disposed) return;
      setYoutubeSummary({
        membershipCount: page.membershipCount ?? null,
        statsAvailable: page.statsAvailable ?? null,
        superChatTotals: page.superChatTotals ?? [],
      });
    }).catch((error: unknown) => {
      if (disposed || (error instanceof Error && error.name === "AbortError")) return;
      setYoutubeSummary(null);
    });
    return () => {
      disposed = true;
      controller.abort();
    };
  }, [session?.id, isYoutube, loading]);

  useEffect(() => {
    const handleEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      event.preventDefault();
      if (onlineChartOpen) {
        setOnlineChartOpen(false);
        requestAnimationFrame(() => peakCardRef.current?.focus());
      } else {
        onClose();
      }
    };
    window.addEventListener("keydown", handleEscape);
    return () => window.removeEventListener("keydown", handleEscape);
  }, [onClose, onlineChartOpen]);

  const superChatTotals = youtubeSummary?.superChatTotals ?? [];
  const historicalSuperChatCount = superChatTotals.reduce((sum, total) => sum + total.eventCount, 0);
  const membershipCount = youtubeSummary?.membershipCount ?? null;
  const youtubeSuperChatCount = youtubeStatsKnown
    ? (session?.superChatCount ?? 0)
    : (superChatTotals.length > 0 ? historicalSuperChatCount : null);

  return (
    <Modal
      title={onlineChartOpen ? "同接变化" : "直播数据"}
      onClose={onClose}
      wide
      footer={onBack ? <button className="button secondary" onClick={onBack}><ChevronLeft size={16} />返回最近直播</button> : undefined}
      className={onlineChartOpen ? "online-chart-modal" : undefined}
    >
      {loading ? (
        <div className="live-stats-loading"><LoaderCircle className="spin" size={24} />正在读取直播数据…</div>
      ) : session && onlineChartOpen ? (
        <LiveOnlineChart key={session.id} session={session}
          loadPage={loadPage} onBack={() => {
            setOnlineChartOpen(false);
            requestAnimationFrame(() => peakCardRef.current?.focus());
          }} />
      ) : session ? (
        <div className="live-stats-panel">
          <div className="live-stats-head">
            <Avatar url={streamerAvatar} name={streamerName} size="md" />
            <div>
              <strong>{session.title || title}</strong>
              <span>
                {formatLiveTime(session.startedAt)}
                {session.endedAt ? ` - ${formatLiveTime(session.endedAt)}` : " · 直播中"}
              </span>
            </div>
            <em
              className={`live-data-quality ${liveEventsUnavailable ? "failed" : session.dataQuality}`}
              title={liveQualityDetail(session, platform, youtubeSummary?.statsAvailable ?? null)}
            >
              {liveQualityLabel(session, platform, youtubeStatsUnavailable)}
            </em>
          </div>
          <div className="live-stat-grid">
            <div><span>时长</span><strong>{formatLiveDuration(session.durationSeconds)}</strong></div>
            {isYoutube ? (
              <>
                <div><span>会员</span><strong>{membershipCount === null ? "—" : formatCount(membershipCount)}</strong></div>
                <button type="button" ref={peakCardRef} className="live-stat-chart-card"
                  onClick={() => setOnlineChartOpen(true)} aria-label="查看最高并发观看人数变化曲线"
                  title="查看 YouTube 并发观看人数采样变化曲线">
                  <span>最高并发观看<ChevronRight size={13} aria-hidden="true" /></span>
                  <strong>{session.peakOnline > 0 ? formatCount(session.peakOnline) : "—"}</strong>
                  <small><BarChart3 size={12} aria-hidden="true" />查看变化曲线</small>
                </button>
                <div><span>Super Chat</span><strong>{youtubeSuperChatCount === null ? "—" : formatCount(youtubeSuperChatCount)}</strong></div>
                <div><span>付费用户</span><strong>{youtubeStatsKnown ? formatCount(session.paidUserCount) : "—"}</strong></div>
                <div><span>互动人数</span><strong>{youtubeStatsKnown ? formatCount(session.interactiveUserCount) : "—"}</strong></div>
                <div><span>弹幕</span><strong>{youtubeStatsKnown ? formatCount(session.danmakuCount) : "—"}</strong></div>
              </>
            ) : (
              <>
                <div><span>营收(估)</span><strong>{liveEventsUnavailable ? "—" : formatMoney(session.revenueCents)}</strong></div>
                <div><span>付费人数(估)</span><strong>{liveEventsUnavailable ? "—" : formatCount(session.paidUserCount)}</strong></div>
                <button type="button" ref={peakCardRef} className="live-stat-chart-card"
                  onClick={() => setOnlineChartOpen(true)} aria-label="查看最高同时在线人数变化曲线"
                  title="查看公开高能用户数的采样变化曲线">
                  <span>最高同时在线<ChevronRight size={13} aria-hidden="true" /></span>
                  <strong>{session.peakHighEnergy > 0 ? formatCount(session.peakHighEnergy) : "—"}</strong>
                  <small><BarChart3 size={12} aria-hidden="true" />查看变化曲线</small>
                </button>
                <div><span>互动人数(估)</span><strong>{liveEventsUnavailable ? "—" : formatCount(session.interactiveUserCount)}</strong></div>
                <div><span>弹幕</span><strong>{liveEventsUnavailable ? "—" : formatCount(session.danmakuCount)}</strong></div>
              </>
            )}
          </div>
          {isYoutube ? (
            <div className="live-revenue-breakdown">
              <span>Super Chat 原币种</span>
              {superChatTotals.length > 0 ? superChatTotals.map((total) => (
                <span key={total.currency ?? "unknown"}>
                  {formatSuperChatTotal(total)} · {formatCount(total.eventCount)} 笔
                </span>
              )) : <span>—</span>}
            </div>
          ) : (
            <div className="live-revenue-breakdown">
              <span>礼物 {liveEventsUnavailable ? "—" : formatMoney(session.giftRevenueCents)}</span>
              <span>SC {liveEventsUnavailable ? "—" : formatMoney(session.superChatRevenueCents)}</span>
              <span>舰长 {liveEventsUnavailable ? "—" : formatMoney(session.guardRevenueCents)}</span>
            </div>
          )}
          {isYoutube && youtubeStatsUnavailable ? (
            <p className="form-hint live-stats-error-note">
              YouTube OAuth 授权失效或尚未授权，聊天、Super Chat 和会员统计已降级；并发观看与时长仍继续采样。
            </p>
          ) : !isYoutube && liveEventsUnavailable ? (
            <p className="form-hint live-stats-error-note">
              本次未能建立 B 站弹幕采集连接（重试 {formatCount(session.reconnectCount)} 次），事件类指标不可用；时长仍来自直播状态轮询。
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
  );
}
