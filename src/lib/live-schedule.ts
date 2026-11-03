import type { LiveStatus, Schedule } from "../types";

type LiveScheduleCandidate = Pick<Schedule, "id" | "streamerId" | "startAt" | "cancelled">;
type ActiveLiveStatus = Pick<LiveStatus, "streamerId" | "isLive" | "startedAt" | "checkedAt">;

function parseTime(value: string | null | undefined): number | null {
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function liveReferenceTime(status: ActiveLiveStatus, fallbackNow: number): number {
  return parseTime(status.startedAt) ?? parseTime(status.checkedAt) ?? fallbackNow;
}

/**
 * Match each active live stream to exactly one schedule for that streamer.
 *
 * A streamer can have several schedule cards in the loaded calendar window.
 * Marking every card as live makes historical and future entries look live at
 * the same time, so use the start time closest to when the broadcast began.
 */
export function matchLiveSchedules(
  schedules: readonly LiveScheduleCandidate[],
  liveStatuses: readonly ActiveLiveStatus[],
  fallbackNow = Date.now(),
): Set<number> {
  const activeByStreamer = new Map<number, ActiveLiveStatus>();
  for (const status of liveStatuses) {
    if (!status.isLive) continue;
    const existing = activeByStreamer.get(status.streamerId);
    if (!existing || liveReferenceTime(status, fallbackNow) >= liveReferenceTime(existing, fallbackNow)) {
      activeByStreamer.set(status.streamerId, status);
    }
  }

  const schedulesByStreamer = new Map<number, LiveScheduleCandidate[]>();
  for (const schedule of schedules) {
    if (schedule.cancelled || !activeByStreamer.has(schedule.streamerId)) continue;
    const values = schedulesByStreamer.get(schedule.streamerId);
    if (values) {
      values.push(schedule);
    } else {
      schedulesByStreamer.set(schedule.streamerId, [schedule]);
    }
  }

  const matchedScheduleIds = new Set<number>();
  for (const [streamerId, candidates] of schedulesByStreamer) {
    const live = activeByStreamer.get(streamerId);
    if (!live) continue;
    const referenceTime = liveReferenceTime(live, fallbackNow);
    let bestSchedule: LiveScheduleCandidate | null = null;
    let bestDistance = Number.POSITIVE_INFINITY;
    let bestStart = Number.POSITIVE_INFINITY;

    for (const schedule of candidates) {
      const start = parseTime(schedule.startAt);
      if (start === null) continue;
      const distance = Math.abs(start - referenceTime);
      const closer = distance < bestDistance
        || (distance === bestDistance && start < bestStart)
        || (distance === bestDistance && start === bestStart && (!bestSchedule || schedule.id < bestSchedule.id));
      if (closer) {
        bestSchedule = schedule;
        bestDistance = distance;
        bestStart = start;
      }
    }

    if (bestSchedule) {
      matchedScheduleIds.add(bestSchedule.id);
    }
  }

  return matchedScheduleIds;
}
