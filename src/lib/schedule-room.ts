import type { Schedule, Streamer } from "../types";

type RoomSchedule = Pick<Schedule, "roomUrl" | "roomOwnerMid" | "roomOwnerName" | "participants">;
type RoomStreamer = Pick<Streamer, "platform" | "mid" | "name" | "aliases" | "liveUrl" | "roomId">;

function normalizeOwnerName(name: string): string {
  return name.trim().replace(/^[@＠]+/, "").trim().toLowerCase();
}

export function resolveScheduleRoomUrl(schedule: RoomSchedule, streamer?: RoomStreamer): string | null {
  const explicitUrl = schedule.roomUrl?.trim();
  if (explicitUrl) return explicitUrl;

  // A schedule's streamer is a participant, not necessarily the room owner.
  if (!streamer || (schedule.participants?.length ?? 0) > 1) return null;
  const ownerMid = schedule.roomOwnerMid?.trim();
  if (ownerMid && ownerMid !== streamer.mid) return null;
  const ownerName = normalizeOwnerName(schedule.roomOwnerName ?? "");
  if (!ownerMid && ownerName && ![streamer.name, ...streamer.aliases].some(
    (name) => normalizeOwnerName(name) === ownerName,
  )) return null;

  const streamerUrl = streamer.liveUrl?.trim();
  if (streamerUrl) return streamerUrl;
  // roomOwnerMid is an account UID, never a Bilibili room number.
  const roomId = streamer.roomId;
  if (streamer.platform === "youtube" || !roomId || !/^\d+$/.test(roomId) || /^0+$/.test(roomId)) return null;
  return `https://live.bilibili.com/${roomId}`;
}
