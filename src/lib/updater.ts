import { getVersion } from "@tauri-apps/api/app";
import { check, type DownloadEvent, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

const CHECK_TIMEOUT_MS = 45_000;
const DOWNLOAD_TIMEOUT_MS = 180_000;
const RETRY_DELAYS_MS = [1_000, 2_500];
let pendingUpdate: Update | null = null;

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

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
}

async function withRetry<T>(operation: () => Promise<T>): Promise<T> {
  let lastError: unknown;
  for (let attempt = 0; attempt <= RETRY_DELAYS_MS.length; attempt += 1) {
    try {
      return await operation();
    } catch (error) {
      lastError = error;
      if (attempt < RETRY_DELAYS_MS.length) {
        await sleep(RETRY_DELAYS_MS[attempt]);
      }
    }
  }
  throw lastError;
}

async function checkUpdate(): Promise<Update | null> {
  return withRetry(() => check({ timeout: CHECK_TIMEOUT_MS }));
}

async function closePendingUpdate(): Promise<void> {
  const update = pendingUpdate;
  pendingUpdate = null;
  await update?.close().catch(() => undefined);
}

export async function currentAppVersion(): Promise<string> {
  return getVersion();
}

export async function checkForUpdate(retain = false): Promise<AvailableUpdate | null> {
  await closePendingUpdate();
  const update = await checkUpdate();
  if (!update) {
    return null;
  }
  const summary: AvailableUpdate = {
    currentVersion: update.currentVersion,
    version: update.version,
    date: update.date,
    body: update.body,
  };
  if (retain) {
    pendingUpdate = update;
  } else {
    await update.close().catch(() => undefined);
  }
  return summary;
}

export async function downloadAndInstallUpdate(
  onProgress: (progress: UpdateProgress) => void,
): Promise<void> {
  let lastError: unknown;
  for (let attempt = 0; attempt <= RETRY_DELAYS_MS.length; attempt += 1) {
    let update = pendingUpdate;
    pendingUpdate = null;
    if (!update) {
      update = await checkUpdate();
    }
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
      await update.downloadAndInstall(handleProgress, {
        restartAfterInstall: true,
        timeout: DOWNLOAD_TIMEOUT_MS,
      });
      return;
    } catch (error) {
      lastError = error;
      await update.close().catch(() => undefined);
      if (attempt < RETRY_DELAYS_MS.length) {
        await sleep(RETRY_DELAYS_MS[attempt]);
      }
    }
  }
  throw lastError instanceof Error ? lastError : new Error("安装更新失败");
}

export async function relaunchApplication(): Promise<void> {
  await closePendingUpdate();
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
