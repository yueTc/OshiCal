import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { relaunch } from "@tauri-apps/plugin-process";
import type { UpdateChannel } from "../types";
import { readClientGithubMirror } from "./github-mirror";

const CLIENT_UPDATE_CHANNEL_KEY = "oshical.updateChannel";

export interface AvailableUpdate {
  currentVersion: string;
  version: string;
  date?: string;
  body?: string;
}

export interface UpdateProgress {
  downloaded: number;
  total: number | null;
}

interface UpdateProgressEvent extends UpdateProgress {
  done: boolean;
}

export async function currentAppVersion(): Promise<string> {
  return getVersion();
}

export function readUpdateChannel(): UpdateChannel {
  try {
    return localStorage.getItem(CLIENT_UPDATE_CHANNEL_KEY) === "beta" ? "beta" : "stable";
  } catch {
    return "stable";
  }
}

export function saveUpdateChannel(channel: UpdateChannel): UpdateChannel {
  try {
    localStorage.setItem(CLIENT_UPDATE_CHANNEL_KEY, channel);
  } catch {
    // The in-memory setting remains active for this page session.
  }
  return channel;
}

export async function checkForUpdate(
  channel: UpdateChannel = readUpdateChannel(),
): Promise<AvailableUpdate | null> {
  return invoke<AvailableUpdate | null>("check_app_update", {
    mirrorUrl: readClientGithubMirror() || null,
    channel,
  });
}

export async function downloadAndInstallUpdate(
  onProgress: (progress: UpdateProgress) => void,
  channel: UpdateChannel = readUpdateChannel(),
): Promise<void> {
  const unlisten = await listen<UpdateProgressEvent>("app-update-progress", (event) => {
    onProgress({ downloaded: event.payload.downloaded, total: event.payload.total });
  });
  try {
    await invoke("install_app_update", {
      mirrorUrl: readClientGithubMirror() || null,
      channel,
    });
  } finally {
    unlisten();
  }
}

export async function relaunchApplication(): Promise<void> {
  await relaunch();
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 B";
  }
  const units = ["B", "KB", "MB", "GB"];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${value.toFixed(exponent === 0 ? 0 : 1)} ${units[exponent]}`;
}
