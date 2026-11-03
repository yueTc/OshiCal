import {
  isPermissionGranted,
  onAction,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import type { LiveEventPayload } from "../types";
import { isTauri, openExternalUrl } from "./api";

export async function ensureNotificationPermission(): Promise<boolean> {
  if (!isTauri) {
    return false;
  }
  let granted = await isPermissionGranted();
  if (!granted) {
    granted = (await requestPermission()) === "granted";
  }
  return granted;
}

export async function installNotificationActionHandler(): Promise<() => void> {
  if (!isTauri) {
    return () => undefined;
  }
  const listener = await onAction(async (action) => {
    const extra = action.notification?.extra ?? {};
    const url = typeof extra.liveUrl === "string" ? extra.liveUrl : "";
    if (url) {
      try {
        await openExternalUrl(url);
      } catch (error) {
        console.error("打开直播间失败", error);
      }
    }
  });
  return () => {
    void listener.unregister();
  };
}

export function notifyLive(event: LiveEventPayload): void {
  if (!isTauri) {
    return;
  }
  sendNotification({
    title: `${event.streamerName} 开播了`,
    body: event.title || "点击打开直播间",
    autoCancel: true,
    extra: {
      streamerId: event.streamerId,
      liveUrl: event.liveUrl ?? "",
    },
  });
}
