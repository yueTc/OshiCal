import { useEffect, useRef, useState } from "react";
import { BarChart3, ChevronRight, LoaderCircle, RefreshCw } from "lucide-react";
import type { LiveOnlineSamplesPage, LiveSession, Streamer } from "../types";
import { explainError } from "../lib/api";
import { RECENT_LIVE_LIMIT, recentLivesForStreamer } from "../lib/recent-lives";
import { LiveDataModal } from "./LiveDataModal";
import { Avatar, EmptyState, ErrorBanner, Modal } from "./ui";

export interface RecentLiveActions {
  liveSessions: (streamerId: number, limit: number) => Promise<LiveSession[]>;
  liveOnlineSamples: (sessionId: number, afterId: number, signal?: AbortSignal) => Promise<LiveOnlineSamplesPage>;
}

export function CreatorRecentLives({ streamer, actions, onClose }: {
  streamer: Streamer;
  actions: RecentLiveActions;
  onClose: () => void;
}) {
  const [sessions, setSessions] = useState<LiveSession[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);
  const [selected, setSelected] = useState<LiveSession | null>(null);
  const returnSessionId = useRef<number | null>(null);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    void actions.liveSessions(streamer.id, RECENT_LIVE_LIMIT).then((result) => {
      if (!cancelled) setSessions(recentLivesForStreamer(result, streamer.id));
    }).catch((loadError: unknown) => {
      if (!cancelled) setError(explainError(loadError));
    }).finally(() => {
      if (!cancelled) setLoading(false);
    });
    // 关闭、切换主播或重新请求后，旧响应不得覆盖当前列表。
    return () => { cancelled = true; };
  }, [actions, streamer.id, reload]);

  useEffect(() => {
    if (!selected && returnSessionId.current !== null) {
      listRef.current?.querySelector<HTMLButtonElement>(`[data-session-id="${returnSessionId.current}"]`)?.focus();
      returnSessionId.current = null;
    }
  }, [selected]);

  useEffect(() => {
    if (selected) return;
    const handleEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !event.defaultPrevented) {
        event.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", handleEscape);
    return () => window.removeEventListener("keydown", handleEscape);
  }, [onClose, selected]);

  if (selected) {
    return (
      <LiveDataModal
        key={selected.id}
        session={selected}
        streamerAvatar={streamer.avatarUrl}
        streamerName={streamer.name}
        title={selected.title}
        loadPage={actions.liveOnlineSamples}
        onClose={onClose}
        onBack={() => {
          returnSessionId.current = selected.id;
          setSelected(null);
        }}
      />
    );
  }

  return (
    <Modal title={`${streamer.name} · 最近直播`} onClose={onClose} wide>
      <div className="recent-lives-head">
        <p className="form-hint">最近 {RECENT_LIVE_LIMIT} 场已采集直播，按开播时间倒序排列。点击卡片查看该场直播数据。</p>
        <button className="icon-button" type="button" title="刷新最近直播" aria-label="刷新最近直播"
          disabled={loading} onClick={() => setReload((value) => value + 1)}>
          <RefreshCw size={17} className={loading ? "spin" : ""} />
        </button>
      </div>
      {loading ? (
        <div className="live-stats-loading" role="status"><LoaderCircle className="spin" size={24} />正在读取最近直播…</div>
      ) : error ? (
        <ErrorBanner message={error} onRetry={() => setReload((value) => value + 1)} />
      ) : sessions.length ? (
        <div className="recent-lives-list" ref={listRef}>
          {sessions.map((session) => (
            <button type="button" key={session.id} data-session-id={session.id}
              className={`schedule-card recent-live-card ${session.endedAt ? "" : "is-live"}`}
              aria-label={`查看 ${streamer.name} ${new Date(session.startedAt).toLocaleString("zh-CN")} ${session.title || "直播"} 的数据`}
              onClick={() => setSelected(session)}>
              <span className="schedule-time">
                {new Date(session.startedAt).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false })}
                <small>{new Date(session.startedAt).toLocaleDateString("zh-CN")}</small>
              </span>
              <Avatar url={streamer.avatarUrl} name={streamer.name} />
              <span className="schedule-content recent-live-content">
                <span className="schedule-streamer">
                  <span>{streamer.name}</span>
                  {session.endedAt ? <span className="recent-live-ended">已结束</span> : <span className="live-label">直播中</span>}
                </span>
                <strong className="recent-live-title">{session.title || "未命名直播"}</strong>
                <span className="recent-live-link"><BarChart3 size={13} aria-hidden="true" />查看直播数据</span>
              </span>
              <ChevronRight className="recent-live-arrow" size={17} aria-hidden="true" />
            </button>
          ))}
        </div>
      ) : (
        <EmptyState title="还没有最近直播记录"
          description="这里只显示已采集的直播场次，不包含未来日程。请在设置中开启直播数据统计，并为该主播打开“订阅直播数据”；直播期间需保持采集端运行，历史未采集的场次不会补录。" />
      )}
    </Modal>
  );
}
