import type { LiveSession } from "../types";

export const RECENT_LIVE_LIMIT = 50;

/** 只展示当前主播的真实采集场次；不修改 API 返回数组。 */
export function recentLivesForStreamer(sessions: LiveSession[], streamerId: number): LiveSession[] {
  return sessions
    .filter((session) => session.streamerId === streamerId)
    .sort((a, b) => (Date.parse(b.startedAt) || 0) - (Date.parse(a.startedAt) || 0) || b.id - a.id)
    .slice(0, RECENT_LIVE_LIMIT);
}
