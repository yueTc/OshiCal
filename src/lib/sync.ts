import { invoke } from "@tauri-apps/api/core";
import { ApiFailure, ServerApi, isTauri } from "./api";
import { persistWebSyncBatch, replaceWebSyncCache } from "./syncStorage";
import { httpBaseFromWebSocketUrl } from "./server-url";
import type {
  AppSettings,
  DeviceInfo,
  LiveSession,
  LiveStatus,
  Schedule,
  Streamer,
  SyncBootstrap,
  SyncChange,
  SyncChangesPage,
  WeeklyScheduleRule,
} from "../types";
export type { DeviceInfo, SyncBootstrap, SyncChange } from "../types";

export const SYNC_PROTOCOL_VERSION = 2;
export const SYNC_CHANGES_LIMIT = 200;
export const SYNC_ACK_INTERVAL_MS = 750;

export type SyncStatus =
  | "idle"
  | "connecting"
  | "connected"
  | "syncing"
  | "reset_required"
  | "offline"
  | "error";

export interface JobProgress {
  jobId: string;
  kind: string;
  streamerId?: number;
  streamerName?: string;
  stage: string;
  completed: number;
  total: number;
  percent: number;
  done: boolean;
  failed: boolean;
}

export interface SyncUiState {
  status: SyncStatus;
  cursorSeq: number;
  latestSeq: number;
  minRetainedSeq: number;
  ackedSeq: number;
  lastSyncAt: string | null;
  lastError: string | null;
  deviceId: string;
  protocolVersion: number | null;
}

export interface StoredSyncCursor {
  cursorSeq: number;
  latestSeq: number;
  minRetainedSeq: number;
  snapshotRevision: number;
  ackedSeq: number;
  lastSyncAt: string | null;
}

interface SyncEngineOptions {
  baseUrl: string;
  token: string;
  deviceId: string;
  initial: StoredSyncCursor;
  ackIntervalMs?: number;
  onState: (state: SyncUiState) => void;
  onChanges: (changes: SyncChange[]) => void | Promise<void>;
  onSnapshot: (snapshot: SyncBootstrap) => void | Promise<void>;
  onJobProgress?: (progress: JobProgress) => void;
  onResetComplete?: (cursorSeq: number) => void;
  fetchImpl?: typeof fetch;
  webCache?: boolean;
}

const CURSOR_STORAGE_PREFIX = "oshical.sync.cursor.v2:";
const DEVICE_ID_STORAGE_KEY = "oshical.deviceId";

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function finiteNumber(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function nonNegativeInteger(value: unknown, fallback = 0): number {
  const number = finiteNumber(value, fallback);
  return Math.max(0, Math.trunc(number));
}

function integerValue(value: unknown, fallback = -1): number {
  const number = finiteNumber(value, fallback);
  return Number.isFinite(number) ? Math.trunc(number) : fallback;
}

function stringValue(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function normalizeSyncChange(value: unknown): SyncChange | null {
  if (!isRecord(value)) {
    return null;
  }
  const operation = value.operation;
  if (operation !== "upsert" && operation !== "delete") {
    return null;
  }
  const seq = integerValue(value.seq);
  const entityType = stringValue(value.entityType);
  const entityId = stringValue(value.entityId);
  if (seq < 0 || !entityType || !entityId) {
    return null;
  }
  return {
    seq,
    entityType,
    entityId,
    operation,
    entityRevision: nonNegativeInteger(value.entityRevision),
    payload: value.payload ?? null,
    createdAt: stringValue(value.createdAt, new Date(0).toISOString()),
  };
}

function normalizeChanges(value: unknown): SyncChange[] | null {
  if (!Array.isArray(value)) {
    return null;
  }
  const changes: SyncChange[] = [];
  for (const item of value) {
    const change = normalizeSyncChange(item);
    if (!change) {
      return null;
    }
    changes.push(change);
  }
  return changes;
}

function validateContiguousChanges(changes: SyncChange[], expectedFrom: number, expectedTo: number): boolean {
  if (!changes.length || changes[0].seq !== expectedFrom) {
    return false;
  }
  for (let index = 1; index < changes.length; index += 1) {
    if (changes[index].seq !== changes[index - 1].seq + 1) {
      return false;
    }
  }
  return changes[changes.length - 1].seq === expectedTo;
}

function cursorStorageKey(baseUrl: string): string {
  return `${CURSOR_STORAGE_PREFIX}${baseUrl.replace(/\/+$/, "")}`;
}

export function idleSyncState(deviceId = ""): SyncUiState {
  return {
    status: "idle",
    cursorSeq: 0,
    latestSeq: 0,
    minRetainedSeq: 0,
    ackedSeq: 0,
    lastSyncAt: null,
    lastError: null,
    deviceId,
    protocolVersion: null,
  };
}

export function resolveHttpBase(api: ServerApi): string {
  return httpBaseFromWebSocketUrl(api.eventSocketUrl(0));
}

export function loadSyncCursor(baseUrl: string): StoredSyncCursor {
  try {
    const raw = window.localStorage.getItem(cursorStorageKey(baseUrl));
    if (!raw) {
      return { cursorSeq: 0, latestSeq: 0, minRetainedSeq: 0, snapshotRevision: 0, ackedSeq: 0, lastSyncAt: null };
    }
    const parsed = JSON.parse(raw) as Partial<StoredSyncCursor>;
    return {
      cursorSeq: nonNegativeInteger(parsed.cursorSeq),
      latestSeq: nonNegativeInteger(parsed.latestSeq),
      minRetainedSeq: nonNegativeInteger(parsed.minRetainedSeq),
      snapshotRevision: nonNegativeInteger(parsed.snapshotRevision),
      ackedSeq: nonNegativeInteger(parsed.ackedSeq),
      lastSyncAt: typeof parsed.lastSyncAt === "string" ? parsed.lastSyncAt : null,
    };
  } catch {
    return { cursorSeq: 0, latestSeq: 0, minRetainedSeq: 0, snapshotRevision: 0, ackedSeq: 0, lastSyncAt: null };
  }
}

function saveSyncCursor(baseUrl: string, state: SyncUiState, snapshotRevision: number): void {
  try {
    window.localStorage.setItem(
      cursorStorageKey(baseUrl),
      JSON.stringify({
        cursorSeq: state.cursorSeq,
        latestSeq: state.latestSeq,
        minRetainedSeq: state.minRetainedSeq,
        snapshotRevision: nonNegativeInteger(snapshotRevision),
        ackedSeq: state.ackedSeq,
        lastSyncAt: state.lastSyncAt,
      }),
    );
  } catch {
    // Cursor persistence is a cache concern; the live state remains usable.
  }
}

export async function resolveSyncDeviceId(): Promise<string> {
  if (isTauri) {
    try {
      const existing = await invoke<string>("get_device_id");
      if (existing?.trim()) {
        return existing.trim();
      }
    } catch {
      // Fall back to browser-local identity when the Tauri bridge is unavailable.
    }
  }
  try {
    const existing = window.localStorage.getItem(DEVICE_ID_STORAGE_KEY);
    if (existing?.trim()) {
      return existing.trim();
    }
    const created = crypto.randomUUID();
    window.localStorage.setItem(DEVICE_ID_STORAGE_KEY, created);
    return created;
  } catch {
    return crypto.randomUUID();
  }
}

export function syncDevicePlatform(): string {
  if (isTauri) {
    return "windows-desktop";
  }
  return navigator.userAgent.includes("Windows") ? "web-windows" : "web";
}

export function syncDeviceName(): string {
  if (isTauri) {
    return "Windows 桌面设备";
  }
  return navigator.userAgent.includes("Windows") ? "Windows 浏览器" : "Web 设备";
}

async function requestJson<T>(
  baseUrl: string,
  token: string,
  path: string,
  fetchImpl: typeof fetch,
  init?: RequestInit,
): Promise<T> {
  const url = `${baseUrl.replace(/\/+$/, "")}${path}`;
  let response: Response;
  try {
    response = await fetchImpl(url, {
      ...init,
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
        ...(init?.headers ?? {}),
      },
    });
  } catch (error) {
    throw new ApiFailure(
      error instanceof Error ? error.message : "服务器连接失败",
      "REMOTE_OFFLINE",
    );
  }

  const text = await response.text();
  let envelope: { ok?: boolean; data?: T; error?: { code?: string; message?: string } } | null = null;
  try {
    envelope = text
      ? (JSON.parse(text) as { ok?: boolean; data?: T; error?: { code?: string; message?: string } })
      : null;
  } catch {
    throw new ApiFailure("服务器返回了非 JSON 同步响应", "INVALID_RESPONSE", response.status);
  }
  if (!response.ok || !envelope?.ok) {
    throw new ApiFailure(
      envelope?.error?.message ?? `同步请求失败: HTTP ${response.status}`,
      envelope?.error?.code ?? `HTTP_${response.status}`,
      response.status,
    );
  }
  return envelope.data as T;
}

export async function fetchSyncChanges(
  baseUrl: string,
  token: string,
  after: number,
  limit = SYNC_CHANGES_LIMIT,
  fetchImpl: typeof fetch = fetch,
): Promise<SyncChangesPage> {
  const data = await requestJson<Record<string, unknown>>(
    baseUrl,
    token,
    `/api/sync/changes?after=${Math.max(0, after)}&limit=${Math.max(1, Math.min(500, limit))}`,
    fetchImpl,
  );
  const changes = normalizeChanges(data.changes);
  if (!changes) {
    throw new ApiFailure("服务器返回的同步变更格式无效", "INVALID_SYNC_CHANGES");
  }
  return {
    schemaVersion: nonNegativeInteger(data.schemaVersion),
    fromSeq: nonNegativeInteger(data.fromSeq),
    toSeq: nonNegativeInteger(data.toSeq),
    latestSeq: nonNegativeInteger(data.latestSeq),
    minRetainedSeq: nonNegativeInteger(data.minRetainedSeq),
    hasMore: data.hasMore === true,
    resetRequired: data.resetRequired === true,
    changes,
  };
}

export async function fetchSyncBootstrap(
  baseUrl: string,
  token: string,
  fetchImpl: typeof fetch = fetch,
): Promise<SyncBootstrap> {
  const data = await requestJson<Record<string, unknown>>(
    baseUrl,
    token,
    "/api/sync/bootstrap",
    fetchImpl,
  );
  if (!Array.isArray(data.subscriptions) || !Array.isArray(data.schedules)) {
    throw new ApiFailure("服务器返回的同步快照格式无效", "INVALID_SYNC_BOOTSTRAP");
  }
  return {
    schemaVersion: nonNegativeInteger(data.schemaVersion),
    snapshotRevision: nonNegativeInteger(data.snapshotRevision),
    minRetainedSeq: nonNegativeInteger(data.minRetainedSeq),
    serverTime: stringValue(data.serverTime, new Date().toISOString()),
    subscriptions: data.subscriptions as Streamer[],
    schedules: data.schedules as Schedule[],
    weeklyRules: Array.isArray(data.weeklyRules)
      ? (data.weeklyRules as WeeklyScheduleRule[])
      : [],
    weeklyExceptions: Array.isArray(data.weeklyExceptions) ? data.weeklyExceptions : [],
    scheduleSources: Array.isArray(data.scheduleSources) ? data.scheduleSources : [],
    sourceSuppressions: Array.isArray(data.sourceSuppressions) ? data.sourceSuppressions : [],
    serverSettings: isRecord(data.serverSettings) ? data.serverSettings : {},
    liveStatuses: Array.isArray(data.liveStatuses) ? (data.liveStatuses as LiveStatus[]) : [],
    liveSessions: Array.isArray(data.liveSessions) ? (data.liveSessions as LiveSession[]) : [],
  };
}

function normalizeDevice(value: unknown): DeviceInfo | null {
  if (!isRecord(value)) {
    return null;
  }
  const deviceId = stringValue(value.deviceId ?? value.device_id);
  if (!deviceId) {
    return null;
  }
  return {
    deviceId,
    name: stringValue(value.name, "未命名设备"),
    platform: stringValue(value.platform, "unknown"),
    appVersion: stringValue(value.appVersion ?? value.app_version, "unknown"),
    lastSeenAt: stringValue(value.lastSeenAt ?? value.last_seen_at, ""),
    lastAckSeq: nonNegativeInteger(value.lastAckSeq ?? value.last_ack_seq),
    online: value.online === true,
  };
}

export async function fetchSyncDevices(
  baseUrl: string,
  token: string,
  fetchImpl: typeof fetch = fetch,
): Promise<DeviceInfo[]> {
  const data = await requestJson<unknown>(baseUrl, token, "/api/devices", fetchImpl);
  const values = Array.isArray(data)
    ? data
    : isRecord(data) && Array.isArray(data.devices)
      ? data.devices
      : [];
  return values
    .map(normalizeDevice)
    .filter((device): device is DeviceInfo => Boolean(device));
}

export async function registerSyncDevice(
  baseUrl: string,
  token: string,
  deviceId: string,
  fetchImpl: typeof fetch = fetch,
): Promise<DeviceInfo | null> {
  const data = await requestJson<unknown>(baseUrl, token, "/api/devices/register", fetchImpl, {
    method: "POST",
    body: JSON.stringify({
      deviceId,
      name: syncDeviceName(),
      platform: syncDevicePlatform(),
      appVersion: "0.1.0",
    }),
  });
  return normalizeDevice(data);
}

export function isSyncMessage(value: Record<string, unknown>): boolean {
  return [
    "auth_ok",
    "sync_batch",
    "sync_available",
    "reset_required",
    "job_progress",
  ].includes(stringValue(value.type));
}

export function syncChangesFromMessage(value: Record<string, unknown>): SyncChange[] | null {
  return normalizeChanges(value.changes);
}

export function jobProgressFromMessage(value: Record<string, unknown>): JobProgress {
  return {
    jobId: stringValue(value.jobId, stringValue(value.id, "refresh")),
    kind: stringValue(value.kind, "refresh"),
    streamerId:
      typeof value.streamerId === "number" && Number.isFinite(value.streamerId)
        ? value.streamerId
        : undefined,
    streamerName: typeof value.streamerName === "string" ? value.streamerName : undefined,
    stage: stringValue(value.stage, "同步刷新进度"),
    completed: nonNegativeInteger(value.completed),
    total: nonNegativeInteger(value.total),
    percent: Math.max(0, Math.min(100, finiteNumber(value.percent))),
    done: value.done === true,
    failed: value.failed === true,
  };
}

function serverSettingsRecord(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) {
    return {};
  }
  return isRecord(value.serverSettings)
    ? value.serverSettings
    : isRecord(value.settings)
      ? value.settings
      : value;
}

export function mergeServerSettings(local: AppSettings, value: unknown): AppSettings {
  const remote = serverSettingsRecord(value);
  const numberOr = (key: string, fallback: number) =>
    typeof remote[key] === "number" && Number.isFinite(remote[key])
      ? (remote[key] as number)
      : fallback;
  const boolOr = (key: string, fallback: boolean) =>
    typeof remote[key] === "boolean" ? (remote[key] as boolean) : fallback;
  const stringOr = (key: string, fallback: string) =>
    typeof remote[key] === "string" ? (remote[key] as string) : fallback;
  const proxyValue = remote.proxyPort;
  return {
    ...local,
    livePollSeconds: numberOr("livePollSeconds", local.livePollSeconds),
    dynamicPollSeconds: numberOr("dynamicPollSeconds", local.dynamicPollSeconds),
    autoAnalyzeImages: boolOr("autoAnalyzeImages", local.autoAnalyzeImages),
    liveStatsEnabled: boolOr("liveStatsEnabled", local.liveStatsEnabled),
    llmBaseUrl: stringOr("llmBaseUrl", local.llmBaseUrl),
    llmModel: stringOr("llmModel", local.llmModel),
    llmPromptVersion: stringOr("llmPromptVersion", local.llmPromptVersion),
    proxyPort:
      proxyValue === null
        ? null
        : typeof proxyValue === "number" && Number.isFinite(proxyValue)
          ? proxyValue
          : local.proxyPort,
  };
}

export function serverSettingsForSave(local: AppSettings, remote: AppSettings): AppSettings {
  return {
    ...remote,
    livePollSeconds: local.livePollSeconds,
    dynamicPollSeconds: local.dynamicPollSeconds,
    autoAnalyzeImages: local.autoAnalyzeImages,
    liveStatsEnabled: local.liveStatsEnabled,
    llmBaseUrl: local.llmBaseUrl,
    llmModel: local.llmModel,
    llmPromptVersion: local.llmPromptVersion,
    proxyPort: local.proxyPort,
    dataMode: "server",
    serverUrl: remote.serverUrl || local.serverUrl,
    notificationsEnabled: remote.notificationsEnabled,
  };
}

export class SyncProtocolEngine {
  private readonly options: SyncEngineOptions;
  private stateValue: SyncUiState;
  private currentStateCallback: (state: SyncUiState) => void;
  private ackSender: ((message: Record<string, unknown>) => void | Promise<void>) | null = null;
  private messageChain: Promise<void> = Promise.resolve();
  private backfillPromise: Promise<void> | null = null;
  private resetPromise: Promise<void> | null = null;
  private ackTimer: ReturnType<typeof setTimeout> | null = null;
  private catchUpTimer: ReturnType<typeof setTimeout> | null = null;
  private snapshotRevisionValue: number;
  private disposed = false;

  constructor(options: SyncEngineOptions) {
    this.options = options;
    this.currentStateCallback = options.onState;
    this.snapshotRevisionValue = nonNegativeInteger(options.initial.snapshotRevision);
    this.stateValue = {
      ...idleSyncState(options.deviceId),
      cursorSeq: options.initial.cursorSeq,
      latestSeq: options.initial.latestSeq,
      minRetainedSeq: options.initial.minRetainedSeq,
      ackedSeq: options.initial.ackedSeq,
      lastSyncAt: options.initial.lastSyncAt,
      status: "connecting",
      protocolVersion: null,
    };
  }

  get state(): SyncUiState {
    return { ...this.stateValue };
  }

  get currentCursor(): number {
    return this.stateValue.cursorSeq;
  }

  setAckTransport(sender: ((message: Record<string, unknown>) => void | Promise<void>) | null): void {
    this.ackSender = sender;
  }

  replaceStateListener(listener: (state: SyncUiState) => void): void {
    this.currentStateCallback = listener;
  }

  async handleMessage(value: Record<string, unknown>): Promise<boolean> {
    const type = stringValue(value.type);
    if (!isSyncMessage(value)) {
      return false;
    }
    const task = this.messageChain.then(async () => {
      if (this.disposed) {
        return;
      }
      if (type === "auth_ok") {
        await this.handleAuthOk(value);
      } else if (type === "sync_batch") {
        await this.handleBatch(value);
      } else if (type === "sync_available") {
        this.update({ latestSeq: nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq) });
        await this.backfill();
      } else if (type === "reset_required") {
        await this.reset(value.reason);
      } else if (type === "job_progress") {
        this.options.onJobProgress?.(jobProgressFromMessage(value));
      }
    });
    this.messageChain = task.catch(() => undefined);
    try {
      await task;
    } catch (error) {
      this.fail(error);
    }
    return true;
  }

  markConnecting(): void {
    if (!this.disposed) {
      this.update({ status: "connecting", lastError: null });
    }
  }

  markOffline(): void {
    if (!this.disposed) {
      this.clearAckTimer();
      this.update({ status: "offline" });
    }
  }

  markConnected(): void {
    if (!this.disposed) {
      this.update({ status: "connected", lastError: null });
    }
  }

  acceptRemoteStatus(value: Record<string, unknown>): void {
    if (this.disposed) {
      return;
    }
    const error = stringValue(value.error);
    if (error) {
      this.update({ status: "error", lastError: error });
      return;
    }
    const protocolVersion =
      typeof value.protocolVersion === "number" && Number.isFinite(value.protocolVersion)
        ? value.protocolVersion
        : this.stateValue.protocolVersion;
    const cursorSeq = nonNegativeInteger(value.cursorSeq ?? value.lastSeq, this.stateValue.cursorSeq);
    const latestSeq = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
    const deviceId = stringValue(value.deviceId, this.stateValue.deviceId);
    const syncing = value.syncing === true || cursorSeq < latestSeq;
    this.update({
      status: syncing ? "syncing" : "connected",
      cursorSeq: Math.max(cursorSeq, this.stateValue.cursorSeq),
      latestSeq: Math.max(latestSeq, this.stateValue.latestSeq),
      deviceId,
      protocolVersion,
      lastError: null,
    });
  }

  acceptAppliedProgress(value: Record<string, unknown>): void {
    if (this.disposed) {
      return;
    }
    const cursorSeq = nonNegativeInteger(value.cursorSeq, this.stateValue.cursorSeq);
    const latestSeq = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
    this.update({
      cursorSeq: Math.max(cursorSeq, this.stateValue.cursorSeq),
      latestSeq: Math.max(latestSeq, this.stateValue.latestSeq),
      ackedSeq: Math.max(cursorSeq, this.stateValue.ackedSeq),
      lastSyncAt: new Date().toISOString(),
      status: cursorSeq < latestSeq ? "syncing" : "connected",
      protocolVersion: SYNC_PROTOCOL_VERSION,
      lastError: null,
    });
  }

  acceptLegacyAuth(value: Record<string, unknown>): void {
    const latest = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
    this.update({
      status: "connected",
      latestSeq: Math.max(latest, this.stateValue.cursorSeq),
      protocolVersion: null,
      lastError: null,
    });
  }

  async forceResync(reason = "manual_resync"): Promise<void> {
    await this.handleMessage({ type: "reset_required", reason });
  }

  dispose(): void {
    this.disposed = true;
    this.clearAckTimer();
    this.clearCatchUpTimer();
    this.ackSender = null;
  }

  private update(patch: Partial<SyncUiState>): void {
    if (this.disposed) {
      return;
    }
    this.stateValue = { ...this.stateValue, ...patch };
    saveSyncCursor(this.options.baseUrl, this.stateValue, this.snapshotRevisionValue);
    this.currentStateCallback(this.state);
  }

  private fail(error: unknown): void {
    const message =
      error instanceof ApiFailure || error instanceof Error ? error.message : String(error);
    this.clearAckTimer();
    this.update({ status: "error", lastError: message });
  }

  private async handleAuthOk(value: Record<string, unknown>): Promise<void> {
    if (nonNegativeInteger(value.protocolVersion) !== SYNC_PROTOCOL_VERSION) {
      this.acceptLegacyAuth(value);
      return;
    }
    const latestSeq = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
    const minRetainedSeq = nonNegativeInteger(value.minRetainedSeq, this.stateValue.minRetainedSeq);
    const resetRequired =
      value.resetRequired === true || this.stateValue.cursorSeq < Math.max(0, minRetainedSeq - 1);
    this.update({
      latestSeq: Math.max(latestSeq, this.stateValue.cursorSeq),
      minRetainedSeq,
      protocolVersion: SYNC_PROTOCOL_VERSION,
      lastError: null,
      status: resetRequired
        ? "reset_required"
        : this.stateValue.cursorSeq < latestSeq
          ? "syncing"
          : "connected",
    });
    if (resetRequired) {
      await this.reset("cursor_expired");
    } else if (this.stateValue.cursorSeq < latestSeq) {
      await this.backfill();
    }
    this.scheduleAck();
  }

  private async handleBatch(value: Record<string, unknown>): Promise<void> {
    const fromSeq = integerValue(value.fromSeq);
    const toSeq = integerValue(value.toSeq);
    const latestSeq = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
    const changes = normalizeChanges(value.changes);
    if (!changes) {
      this.update({ status: "error", lastError: "sync_batch 格式无效", latestSeq });
      return;
    }
    this.update({
      latestSeq: Math.max(latestSeq, this.stateValue.latestSeq),
      protocolVersion: SYNC_PROTOCOL_VERSION,
    });
    if (changes.some((change) => change.entityType === "sync_reset")) {
      await this.reset("data_imported");
      return;
    }
    if (!changes.length) {
      if (this.stateValue.cursorSeq < this.stateValue.latestSeq) {
        await this.backfill();
      } else {
        this.update({ status: "connected" });
      }
      return;
    }
    if (toSeq <= this.stateValue.cursorSeq) {
      this.scheduleAck();
      return;
    }
    if (fromSeq !== this.stateValue.cursorSeq + 1) {
      await this.backfill();
      return;
    }
    if (!validateContiguousChanges(changes, fromSeq, toSeq)) {
      this.update({
        status: "error",
        lastError: "sync_batch 序号不连续，已停止应用",
      });
      await this.backfill();
      return;
    }

    this.update({ status: "syncing", lastError: null });
    try {
      // Persist before notifying the UI. A schedule revision can synchronously
      // read the local mirror, so the web cache must already contain this batch.
      await this.persistChanges(
        changes,
        toSeq,
        Math.max(this.stateValue.latestSeq, toSeq),
        this.stateValue.minRetainedSeq,
      );
      await this.options.onChanges(changes);
      this.update({
        cursorSeq: toSeq,
        latestSeq: Math.max(this.stateValue.latestSeq, toSeq),
        lastSyncAt: new Date().toISOString(),
        status: toSeq < this.stateValue.latestSeq ? "syncing" : "connected",
      });
      this.scheduleAck();
    } catch (error) {
      this.fail(error);
    }
  }

  private async persistChanges(
    changes: SyncChange[],
    cursorSeq: number,
    latestSeq: number,
    minRetainedSeq: number,
  ): Promise<void> {
    if (!this.options.webCache) {
      return;
    }
    await persistWebSyncBatch(this.options.baseUrl, changes, {
      cursorSeq,
      latestSeq,
      minRetainedSeq,
      snapshotRevision: this.snapshotRevisionValue,
      ackedSeq: this.stateValue.ackedSeq,
      lastSyncAt: new Date().toISOString(),
    });
  }

  private async backfill(): Promise<void> {
    if (this.backfillPromise) {
      return this.backfillPromise;
    }
    this.backfillPromise = this.runBackfill().finally(() => {
      this.backfillPromise = null;
    });
    return this.backfillPromise;
  }

  private async runBackfill(): Promise<void> {
    this.update({ status: "syncing", lastError: null });
    let pageCount = 0;
    while (!this.disposed) {
      pageCount += 1;
      if (pageCount > 200) {
        throw new ApiFailure("同步补拉超过安全页数，请重新载入快照", "SYNC_BACKFILL_LIMIT");
      }
      const page = await fetchSyncChanges(
        this.options.baseUrl,
        this.options.token,
        this.stateValue.cursorSeq,
        SYNC_CHANGES_LIMIT,
        this.options.fetchImpl ?? fetch,
      );
      this.update({
        latestSeq: Math.max(page.latestSeq, this.stateValue.latestSeq),
        minRetainedSeq: page.minRetainedSeq,
      });
      if (
        page.resetRequired ||
        this.stateValue.cursorSeq < Math.max(0, page.minRetainedSeq - 1)
      ) {
        await this.reset("cursor_expired");
        return;
      }
      if (!page.changes.length) {
        this.update({
          status:
            this.stateValue.cursorSeq < this.stateValue.latestSeq ? "reset_required" : "connected",
          lastError:
            this.stateValue.cursorSeq < this.stateValue.latestSeq
              ? "服务器没有返回缺失的同步变更，请重新载入快照"
              : null,
        });
        return;
      }
      const expectedFrom = this.stateValue.cursorSeq + 1;
      const expectedTo = page.changes[page.changes.length - 1].seq;
      if (!validateContiguousChanges(page.changes, expectedFrom, expectedTo)) {
        await this.reset("backfill_gap");
        return;
      }
      await this.options.onChanges(page.changes);
      await this.persistChanges(
        page.changes,
        expectedTo,
        Math.max(this.stateValue.latestSeq, page.latestSeq),
        page.minRetainedSeq,
      );
      this.update({
        cursorSeq: expectedTo,
        latestSeq: Math.max(this.stateValue.latestSeq, page.latestSeq),
        lastSyncAt: new Date().toISOString(),
        status: page.hasMore || expectedTo < page.latestSeq ? "syncing" : "connected",
        lastError: null,
      });
      this.scheduleAck();
      if (!page.hasMore || expectedTo >= page.latestSeq) {
        return;
      }
    }
  }

  private async reset(reason: unknown): Promise<void> {
    if (this.resetPromise) {
      return this.resetPromise;
    }
    const reasonText = stringValue(reason, "reset_required");
    this.update({ status: "reset_required", lastError: `同步需要重置：${reasonText}` });
    this.resetPromise = this.runReset().finally(() => {
      this.resetPromise = null;
    });
    return this.resetPromise;
  }

  private async runReset(): Promise<void> {
    this.clearAckTimer();
    const snapshot = await fetchSyncBootstrap(
      this.options.baseUrl,
      this.options.token,
      this.options.fetchImpl ?? fetch,
    );
    this.snapshotRevisionValue = snapshot.snapshotRevision;
    if (this.options.webCache) {
      // Replace the mirror before the snapshot listener bumps the schedule
      // revision; otherwise a cache-first view can briefly read the old mirror.
      await replaceWebSyncCache(this.options.baseUrl, snapshot, {
        cursorSeq: snapshot.snapshotRevision,
        latestSeq: Math.max(snapshot.snapshotRevision, this.stateValue.latestSeq),
        minRetainedSeq: snapshot.minRetainedSeq,
        snapshotRevision: snapshot.snapshotRevision,
        ackedSeq: this.stateValue.ackedSeq,
        lastSyncAt: new Date().toISOString(),
      });
    }
    await this.options.onSnapshot(snapshot);
    this.update({
      cursorSeq: snapshot.snapshotRevision,
      latestSeq: Math.max(snapshot.snapshotRevision, this.stateValue.latestSeq),
      minRetainedSeq: snapshot.minRetainedSeq,
      lastSyncAt: new Date().toISOString(),
      status: "connected",
      lastError: null,
      protocolVersion: SYNC_PROTOCOL_VERSION,
    });
    this.scheduleAck();
    this.options.onResetComplete?.(snapshot.snapshotRevision);
    if (this.stateValue.cursorSeq < this.stateValue.latestSeq) {
      this.scheduleCatchUp();
    }
  }

  private scheduleCatchUp(): void {
    if (this.catchUpTimer) {
      clearTimeout(this.catchUpTimer);
    }
    this.catchUpTimer = setTimeout(() => {
      this.catchUpTimer = null;
      if (!this.disposed && this.stateValue.cursorSeq < this.stateValue.latestSeq) {
        void this.backfill().catch((error) => this.fail(error));
      }
    }, 0);
  }

  private scheduleAck(): void {
    if (this.stateValue.cursorSeq <= this.stateValue.ackedSeq) {
      return;
    }
    this.clearAckTimer();
    this.ackTimer = setTimeout(() => {
      this.ackTimer = null;
      void this.flushAck();
    }, this.options.ackIntervalMs ?? SYNC_ACK_INTERVAL_MS);
  }

  private async flushAck(): Promise<void> {
    const seq = this.stateValue.cursorSeq;
    if (seq <= this.stateValue.ackedSeq || !this.ackSender || this.disposed) {
      return;
    }
    try {
      await this.ackSender({ type: "ack", seq });
      this.update({ ackedSeq: seq });
    } catch {
      if (!this.disposed && this.stateValue.cursorSeq > this.stateValue.ackedSeq) {
        this.scheduleAck();
      }
    }
  }

  private clearAckTimer(): void {
    if (this.ackTimer) {
      clearTimeout(this.ackTimer);
      this.ackTimer = null;
    }
  }

  private clearCatchUpTimer(): void {
    if (this.catchUpTimer) {
      clearTimeout(this.catchUpTimer);
      this.catchUpTimer = null;
    }
  }
}

export { isRecord as syncRecordValue };
