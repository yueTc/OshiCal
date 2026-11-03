import { getVersion } from "@tauri-apps/api/app";
import { check, type DownloadEvent } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

const CHECK_TIMEOUT_MS = 30_000;

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

export async function currentAppVersion(): Promise<string> {
  return getVersion();
}

export async function checkForUpdate(): Promise<AvailableUpdate | null> {
  const update = await check({ timeout: CHECK_TIMEOUT_MS });
  if (!update) {
    return null;
  }
  const summary: AvailableUpdate = {
    currentVersion: update.currentVersion,
    version: update.version,
    date: update.date,
    body: update.body,
  };
  await update.close().catch(() => undefined);
  return summary;
}

export async function downloadAndInstallUpdate(
  onProgress: (progress: UpdateProgress) => void,
): Promise<void> {
  const update = await check({ timeout: CHECK_TIMEOUT_MS });
  if (!update) {
    throw new Error("当前已是最新版本");
  }
  let downloaded = 0;
  let total: number | null = null;
  const handleProgress = (event: DownloadEvent) => {
    if (event.event === "Started") {
      downloaded = 0;
      total = event.data.contentLength ?? null;
      onProgress({ downloaded, total });
      return;
    }
    if (event.event === "Progress") {
      downloaded += event.data.chunkLength;
      onProgress({ downloaded, total });
      return;
    }
    total = downloaded;
    onProgress({ downloaded, total });
  };
  try {
    await update.downloadAndInstall(handleProgress, { restartAfterInstall: true });
  } catch (error) {
    await update.close().catch(() => undefined);
    throw error;
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
