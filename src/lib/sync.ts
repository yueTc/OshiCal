import { invoke } from "@tauri-apps/api/core";
import { ApiFailure, ServerApi, isTauri } from "./api";
import {
  loadedWebSyncFeedId,
  persistWebSyncBatchWithResult,
  replaceWebSyncCacheWithResult,
  webSyncCacheWasLoaded,
} from "./syncStorage";
import type { WebSyncCacheWriteResult } from "./syncStorage";
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
export const SYNC_SCHEMA_VERSION = 2;
export const SYNC_CHANGES_LIMIT = 200;
export const SYNC_ACK_INTERVAL_MS = 750;
const SYNC_HTTP_TIMEOUT_MS = 30_000;

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
  feedId: string;
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
  feedId?: string;
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
  return typeof value === "number" && Number.isSafeInteger(value)
    ? Math.max(0, value)
    : fallback;
}

function requiredSequence(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new ApiFailure(`服务器返回的 ${label} 无效`, "INVALID_SYNC_RESPONSE");
  }
  return value;
}

function integerValue(value: unknown, fallback = -1): number {
  return typeof value === "number" && Number.isSafeInteger(value) ? value : fallback;
}

function stringValue(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function normalizeFeedId(value: unknown): string {
  return stringValue(value).trim();
}

function syncFeedChanged(
  localFeedId: string,
  localCursorSeq: number,
  remoteFeedId: string,
): boolean {
  const normalizedRemote = normalizeFeedId(remoteFeedId);
  const normalizedLocal = normalizeFeedId(localFeedId);
  if (!normalizedRemote) {
    // Once a server epoch has been persisted, an epoch-less downgrade must
    // rebuild instead of silently reusing a potentially unrelated sequence.
    return Boolean(normalizedLocal) && localCursorSeq > 0;
  }
  if (!normalizedLocal) {
    return localCursorSeq > 0;
  }
  return normalizedLocal !== normalizedRemote;
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
    feedId: "",
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
      return { feedId: "", cursorSeq: 0, latestSeq: 0, minRetainedSeq: 0, snapshotRevision: 0, ackedSeq: 0, lastSyncAt: null };
    }
    const parsed = JSON.parse(raw) as Partial<StoredSyncCursor>;
    const cursorSeq = nonNegativeInteger(parsed.cursorSeq);
    return {
      feedId: normalizeFeedId(parsed.feedId),
      cursorSeq,
      latestSeq: nonNegativeInteger(parsed.latestSeq),
      minRetainedSeq: nonNegativeInteger(parsed.minRetainedSeq),
      snapshotRevision: nonNegativeInteger(parsed.snapshotRevision),
      ackedSeq: Math.min(cursorSeq, nonNegativeInteger(parsed.ackedSeq)),
      lastSyncAt: typeof parsed.lastSyncAt === "string" ? parsed.lastSyncAt : null,
    };
  } catch {
    return { feedId: "", cursorSeq: 0, latestSeq: 0, minRetainedSeq: 0, snapshotRevision: 0, ackedSeq: 0, lastSyncAt: null };
  }
}

function saveSyncCursor(baseUrl: string, state: SyncUiState, snapshotRevision: number): void {
  try {
    window.localStorage.setItem(
      cursorStorageKey(baseUrl),
      JSON.stringify({
        feedId: state.feedId,
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
  externalSignal?: AbortSignal,
): Promise<T> {
  const url = `${baseUrl.replace(/\/+$/, "")}${path}`;
  const controller = new AbortController();
  const forwardAbort = () => controller.abort(externalSignal?.reason);
  if (externalSignal?.aborted) {
    forwardAbort();
  } else {
    externalSignal?.addEventListener("abort", forwardAbort, { once: true });
  }
  const timeout = setTimeout(() => {
    controller.abort(new DOMException("同步请求超时", "TimeoutError"));
  }, SYNC_HTTP_TIMEOUT_MS);
  let response: Response;
  let text: string;
  try {
    response = await fetchImpl(url, {
      ...init,
      signal: controller.signal,
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
        ...(init?.headers ?? {}),
      },
    });
    text = await response.text();
  } catch (error) {
    const timedOut = controller.signal.aborted && !externalSignal?.aborted;
    throw new ApiFailure(
      timedOut
        ? `同步请求超时（${Math.round(SYNC_HTTP_TIMEOUT_MS / 1000)} 秒）`
        : error instanceof Error
          ? error.message
          : "服务器连接失败",
      timedOut ? "REQUEST_TIMEOUT" : "REMOTE_OFFLINE",
    );
  } finally {
    clearTimeout(timeout);
    externalSignal?.removeEventListener("abort", forwardAbort);
  }

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

export async function acknowledgeSyncCursor(
  baseUrl: string,
  token: string,
  deviceId: string,
  seq: number,
  fetchImpl: typeof fetch = fetch,
  signal?: AbortSignal,
): Promise<number> {
  const normalizedDeviceId = deviceId.trim();
  if (!normalizedDeviceId) {
    throw new ApiFailure("同步设备 ID 为空，拒绝 ACK", "INVALID_SYNC_ACK");
  }
  if (!Number.isSafeInteger(seq) || seq < 0) {
    throw new ApiFailure("同步 ACK 序号无效", "INVALID_SYNC_ACK");
  }
  const requestedSeq = seq;
  const data = await requestJson<unknown>(
    baseUrl,
    token,
    "/api/sync/ack",
    fetchImpl,
    {
      method: "POST",
      body: JSON.stringify({ deviceId: normalizedDeviceId, seq: requestedSeq }),
    },
    signal,
  );
  if (!isRecord(data)) {
    throw new ApiFailure("服务器未返回同步 ACK 确认", "INVALID_SYNC_ACK");
  }
  const confirmedDeviceId = stringValue(data.deviceId ?? data.device_id).trim();
  if (!confirmedDeviceId || confirmedDeviceId !== normalizedDeviceId) {
    throw new ApiFailure("服务器 ACK 确认的设备不匹配", "INVALID_SYNC_ACK");
  }
  const rawAckedSeq = data.lastAckSeq ?? data.last_ack_seq;
  if (
    typeof rawAckedSeq !== "number" ||
    !Number.isSafeInteger(rawAckedSeq) ||
    rawAckedSeq < 0
  ) {
    throw new ApiFailure("服务器未确认 ACK 已持久化", "SYNC_ACK_NOT_PERSISTED");
  }
  const ackedSeq = rawAckedSeq;
  if (ackedSeq < requestedSeq) {
    throw new ApiFailure(
      `服务器仅确认到 ${ackedSeq}，尚未持久化 ${requestedSeq}`,
      "SYNC_ACK_NOT_PERSISTED",
    );
  }
  return ackedSeq;
}

export async function fetchSyncChanges(
  baseUrl: string,
  token: string,
  after: number,
  limit = SYNC_CHANGES_LIMIT,
  fetchImpl: typeof fetch = fetch,
  signal?: AbortSignal,
): Promise<SyncChangesPage> {
  const data = await requestJson<Record<string, unknown>>(
    baseUrl,
    token,
    `/api/sync/changes?after=${Math.max(0, after)}&limit=${Math.max(1, Math.min(500, limit))}`,
    fetchImpl,
    undefined,
    signal,
  );
  const schemaVersion = requiredSequence(data.schemaVersion, "schemaVersion");
  if (schemaVersion !== SYNC_SCHEMA_VERSION) {
    throw new ApiFailure(
      `同步 schema 版本不兼容（服务器 ${schemaVersion}，客户端 ${SYNC_SCHEMA_VERSION}），请升级客户端`,
      "SYNC_SCHEMA_INCOMPATIBLE",
    );
  }
  const changes = normalizeChanges(data.changes);
  if (!changes) {
    throw new ApiFailure("服务器返回的同步变更格式无效", "INVALID_SYNC_CHANGES");
  }
  if (data.feedId !== undefined && typeof data.feedId !== "string") {
    throw new ApiFailure("服务器返回的 feedId 无效", "INVALID_SYNC_CHANGES");
  }
  return {
    schemaVersion,
    feedId: normalizeFeedId(data.feedId),
    fromSeq: requiredSequence(data.fromSeq, "fromSeq"),
    toSeq: requiredSequence(data.toSeq, "toSeq"),
    latestSeq: requiredSequence(data.latestSeq, "latestSeq"),
    minRetainedSeq: requiredSequence(data.minRetainedSeq, "minRetainedSeq"),
    hasMore: data.hasMore === true,
    resetRequired: data.resetRequired === true,
    changes,
  };
}

export async function fetchSyncBootstrap(
  baseUrl: string,
  token: string,
  fetchImpl: typeof fetch = fetch,
  signal?: AbortSignal,
): Promise<SyncBootstrap> {
  const data = await requestJson<Record<string, unknown>>(
    baseUrl,
    token,
    "/api/sync/bootstrap",
    fetchImpl,
    undefined,
    signal,
  );
  const schemaVersion = requiredSequence(data.schemaVersion, "schemaVersion");
  if (schemaVersion !== SYNC_SCHEMA_VERSION) {
    throw new ApiFailure(
      `同步 schema 版本不兼容（服务器 ${schemaVersion}，客户端 ${SYNC_SCHEMA_VERSION}），请升级客户端`,
      "SYNC_SCHEMA_INCOMPATIBLE",
    );
  }
  if (!Array.isArray(data.subscriptions) || !Array.isArray(data.schedules)) {
    throw new ApiFailure("服务器返回的同步快照格式无效", "INVALID_SYNC_BOOTSTRAP");
  }
  if (data.feedId !== undefined && typeof data.feedId !== "string") {
    throw new ApiFailure("服务器返回的 feedId 无效", "INVALID_SYNC_BOOTSTRAP");
  }
  return {
    schemaVersion,
    feedId: normalizeFeedId(data.feedId),
    snapshotRevision: requiredSequence(data.snapshotRevision, "snapshotRevision"),
    minRetainedSeq: requiredSequence(data.minRetainedSeq, "minRetainedSeq"),
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
  private ackSender: ((message: Record<string, unknown>) => boolean | Promise<boolean>) | null = null;
  private ackPromise: Promise<void> | null = null;
  private messageChain: Promise<void> = Promise.resolve();
  private resetGeneration = 0;
  private backfillPromise: Promise<void> | null = null;
  private resetPromise: Promise<void> | null = null;
  private ackTimer: ReturnType<typeof setTimeout> | null = null;
  private catchUpTimer: ReturnType<typeof setTimeout> | null = null;
  private snapshotRevisionValue: number;
  private webCacheEnabled: boolean;
  private webCachePersisted = false;
  private disposed = false;
  private readonly abortController = new AbortController();

  constructor(options: SyncEngineOptions) {
    this.options = options;
    this.currentStateCallback = options.onState;
    this.snapshotRevisionValue = nonNegativeInteger(options.initial.snapshotRevision);
    this.webCacheEnabled = options.webCache === true;
    this.webCachePersisted =
      this.webCacheEnabled && webSyncCacheWasLoaded(options.baseUrl);
    const persistedFeedId =
      options.initial.feedId ||
      loadedWebSyncFeedId(options.baseUrl) ||
      loadSyncCursor(options.baseUrl).feedId ||
      "";
    const initialCursorSeq = nonNegativeInteger(options.initial.cursorSeq);
    this.stateValue = {
      ...idleSyncState(options.deviceId),
      feedId: normalizeFeedId(persistedFeedId),
      cursorSeq: initialCursorSeq,
      latestSeq: nonNegativeInteger(options.initial.latestSeq),
      minRetainedSeq: nonNegativeInteger(options.initial.minRetainedSeq),
      ackedSeq: Math.min(
        initialCursorSeq,
        nonNegativeInteger(options.initial.ackedSeq),
      ),
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

  setAckTransport(
    sender: ((message: Record<string, unknown>) => boolean | Promise<boolean>) | null,
  ): void {
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
        const remoteFeedId = normalizeFeedId(value.feedId);
        if (syncFeedChanged(this.stateValue.feedId, this.stateValue.cursorSeq, remoteFeedId)) {
          await this.reset("feed_changed");
          return;
        }
        const latestSeq = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
        if (latestSeq < this.stateValue.cursorSeq || latestSeq < this.stateValue.latestSeq) {
          await this.reset("sequence_regressed");
          return;
        }
        this.update({
          feedId: remoteFeedId || this.stateValue.feedId,
          latestSeq,
        });
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
      this.ackSender = null;
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
    const minRetainedSeq = nonNegativeInteger(
      value.minRetainedSeq,
      this.stateValue.minRetainedSeq,
    );
    const remoteFeedId = normalizeFeedId(value.feedId);
    const deviceId = stringValue(value.deviceId, this.stateValue.deviceId);
    if (!isTauri && syncFeedChanged(this.stateValue.feedId, this.stateValue.cursorSeq, remoteFeedId)) {
      this.update({
        status: "reset_required",
        deviceId,
        protocolVersion,
        lastError: "同步需要重置：服务器同步源已变化",
      });
      void this.reset("feed_changed").catch((resetError) => this.fail(resetError));
      return;
    }
    const staleSequence =
      cursorSeq <= this.stateValue.cursorSeq && latestSeq < this.stateValue.latestSeq;
    if (staleSequence) {
      // Check this before treating a lower latestSeq as a server regression.
      // A queued status event from the previous connection can carry an older
      // cursor/latest pair after the new connection has already advanced them.
      this.update({
        status:
          value.syncing === true || this.stateValue.cursorSeq < this.stateValue.latestSeq
            ? "syncing"
            : "connected",
        deviceId,
        protocolVersion,
        lastError: null,
      });
      return;
    }
    if (
      latestSeq < this.stateValue.cursorSeq ||
      latestSeq < this.stateValue.latestSeq
    ) {
      this.update({
        status: "reset_required",
        latestSeq,
        minRetainedSeq,
        deviceId,
        protocolVersion,
        lastError: "同步需要重置：服务器序号回退",
      });
      void this.reset("sequence_regressed").catch((resetError) => this.fail(resetError));
      return;
    }
    const mergedCursorSeq = Math.max(cursorSeq, this.stateValue.cursorSeq);
    const cursorExpired =
      this.stateValue.cursorSeq < Math.max(0, minRetainedSeq - 1);
    if (cursorExpired) {
      this.update({
        status: "reset_required",
        minRetainedSeq,
        deviceId,
        protocolVersion,
        lastError: "同步需要重置：游标已超出服务器保留范围",
      });
      void this.reset("cursor_expired").catch((resetError) => this.fail(resetError));
      return;
    }
    const reportedAckedSeq =
      typeof value.ackedSeq === "number" && Number.isFinite(value.ackedSeq)
        ? nonNegativeInteger(value.ackedSeq)
        : this.stateValue.ackedSeq;
    const syncing = value.syncing === true || mergedCursorSeq < latestSeq;
    this.update({
      status: syncing ? "syncing" : "connected",
      cursorSeq: mergedCursorSeq,
      latestSeq,
      minRetainedSeq,
      feedId: remoteFeedId || this.stateValue.feedId,
      ackedSeq: Math.max(
        this.stateValue.ackedSeq,
        Math.min(reportedAckedSeq, mergedCursorSeq),
      ),
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
    const minRetainedSeq = nonNegativeInteger(
      value.minRetainedSeq,
      this.stateValue.minRetainedSeq,
    );
    const remoteFeedId = normalizeFeedId(value.feedId);
    if (!isTauri && syncFeedChanged(this.stateValue.feedId, this.stateValue.cursorSeq, remoteFeedId)) {
      this.update({
        status: "reset_required",
        protocolVersion: SYNC_PROTOCOL_VERSION,
        lastError: "同步需要重置：服务器同步源已变化",
      });
      void this.reset("feed_changed").catch((resetError) => this.fail(resetError));
      return;
    }
    if (
      cursorSeq <= this.stateValue.cursorSeq &&
      latestSeq < this.stateValue.latestSeq
    ) {
      // A queued apply notification from an older connection is informational;
      // the newer cursor/latest pair has already won. This check must precede
      // the regression path or every late lower latestSeq would force a reset.
      return;
    }
    if (
      latestSeq < this.stateValue.cursorSeq ||
      latestSeq < this.stateValue.latestSeq
    ) {
      this.update({
        status: "reset_required",
        latestSeq,
        minRetainedSeq,
        protocolVersion: SYNC_PROTOCOL_VERSION,
        lastError: "同步需要重置：服务器序号回退",
      });
      void this.reset("sequence_regressed").catch((resetError) => this.fail(resetError));
      return;
    }
    const mergedCursorSeq = Math.max(cursorSeq, this.stateValue.cursorSeq);
    const cursorExpired =
      this.stateValue.cursorSeq < Math.max(0, minRetainedSeq - 1);
    if (cursorExpired) {
      this.update({
        status: "reset_required",
        minRetainedSeq,
        protocolVersion: SYNC_PROTOCOL_VERSION,
        lastError: "同步需要重置：游标已超出服务器保留范围",
      });
      void this.reset("cursor_expired").catch((resetError) => this.fail(resetError));
      return;
    }
    if (
      cursorSeq <= this.stateValue.cursorSeq &&
      latestSeq <= this.stateValue.latestSeq
    ) {
      return;
    }
    const reportedAckedSeq =
      typeof value.ackedSeq === "number" && Number.isFinite(value.ackedSeq)
        ? nonNegativeInteger(value.ackedSeq)
        : this.stateValue.ackedSeq;
    this.update({
      cursorSeq: mergedCursorSeq,
      latestSeq,
      minRetainedSeq,
      feedId: remoteFeedId || this.stateValue.feedId,
      ackedSeq: Math.max(
        this.stateValue.ackedSeq,
        Math.min(reportedAckedSeq, mergedCursorSeq),
      ),
      lastSyncAt: new Date().toISOString(),
      status: mergedCursorSeq < latestSeq ? "syncing" : "connected",
      protocolVersion: SYNC_PROTOCOL_VERSION,
      lastError: null,
    });
  }

  acceptLegacyAuth(value: Record<string, unknown>): void {
    const latest = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
    if (latest < this.stateValue.cursorSeq) {
      this.update({
        status: "reset_required",
        latestSeq: latest,
        protocolVersion: null,
        lastError: "同步需要重置：服务器序号回退",
      });
      void this.reset("sequence_regressed").catch((resetError) => this.fail(resetError));
      return;
    }
    this.update({
      status: "connected",
      latestSeq: latest,
      protocolVersion: null,
      lastError: null,
    });
  }

  async forceResync(reason = "manual_resync"): Promise<void> {
    await this.handleMessage({ type: "reset_required", reason });
  }

  dispose(): void {
    this.disposed = true;
    this.abortController.abort();
    this.clearAckTimer();
    this.clearCatchUpTimer();
    this.ackSender = null;
  }

  private update(patch: Partial<SyncUiState>): void {
    if (this.disposed) {
      return;
    }
    const next = { ...this.stateValue, ...patch };
    // ACK can finish while a reset/reconnect is swapping sequence spaces. Keep
    // the persisted ACK invariant even if a stale completion reaches update().
    next.cursorSeq = nonNegativeInteger(next.cursorSeq);
    next.ackedSeq = Math.min(nonNegativeInteger(next.ackedSeq), next.cursorSeq);
    this.stateValue = next;
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
    if (this.webCacheEnabled && !this.webCachePersisted) {
      // The localStorage cursor is not a substitute for entity data. Rebuild the
      // mirror if IndexedDB was unavailable or never held a complete snapshot.
      await this.reset("web_cache_missing");
      return;
    }
    const latestSeq = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
    const minRetainedSeq = nonNegativeInteger(value.minRetainedSeq, this.stateValue.minRetainedSeq);
    const remoteFeedId = normalizeFeedId(value.feedId);
    const feedChanged = syncFeedChanged(
      this.stateValue.feedId,
      this.stateValue.cursorSeq,
      remoteFeedId,
    );
    const sequenceRegressed =
      latestSeq < this.stateValue.cursorSeq ||
      latestSeq < this.stateValue.latestSeq;
    const cursorExpired = this.stateValue.cursorSeq < Math.max(0, minRetainedSeq - 1);
    const resetRequired =
      value.resetRequired === true || sequenceRegressed || cursorExpired || feedChanged;
    this.update({
      feedId: resetRequired
        ? this.stateValue.feedId
        : remoteFeedId || this.stateValue.feedId,
      latestSeq,
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
      await this.reset(
        feedChanged
          ? "feed_changed"
          : sequenceRegressed
            ? "sequence_regressed"
            : "cursor_expired",
      );
    } else if (this.stateValue.cursorSeq < latestSeq) {
      await this.backfill();
    }
    this.scheduleAck();
  }

  private async handleBatch(value: Record<string, unknown>): Promise<void> {
    const generation = this.resetGeneration;
    if (this.disposed || generation !== this.resetGeneration) {
      return;
    }
    const fromSeq = integerValue(value.fromSeq);
    const toSeq = integerValue(value.toSeq);
    const latestSeq = nonNegativeInteger(value.latestSeq, this.stateValue.latestSeq);
    const remoteFeedId = normalizeFeedId(value.feedId);
    const changes = normalizeChanges(value.changes);
    if (!changes) {
      this.update({ status: "error", lastError: "sync_batch 格式无效", latestSeq });
      return;
    }
    if (syncFeedChanged(this.stateValue.feedId, this.stateValue.cursorSeq, remoteFeedId)) {
      await this.reset("feed_changed");
      return;
    }
    if (changes.some((change) => change.entityType === "sync_reset")) {
      await this.reset("data_imported");
      return;
    }
    const batchAlreadyApplied =
      toSeq <= this.stateValue.cursorSeq &&
      (changes.length === 0 ||
        (fromSeq === changes[0].seq &&
          toSeq === changes[changes.length - 1].seq &&
          changes.every((change) => change.seq <= this.stateValue.cursorSeq)));
    if (batchAlreadyApplied) {
      // WebSocket and HTTP reconciliation can overlap. A queued batch may be
      // older than a cursor already advanced by backfill; it must not trigger a
      // reset merely because its snapshot of latestSeq is old.
      if (latestSeq > this.stateValue.latestSeq) {
        this.update({
          feedId: remoteFeedId || this.stateValue.feedId,
          latestSeq,
          status:
            this.stateValue.cursorSeq < latestSeq
              ? "syncing"
              : this.stateValue.status,
        });
        await this.backfill();
      } else {
        this.scheduleAck();
      }
      return;
    }
    if (
      latestSeq < this.stateValue.cursorSeq ||
      latestSeq < this.stateValue.latestSeq
    ) {
      await this.reset("sequence_regressed");
      return;
    }
    if (!changes.length) {
      if (this.stateValue.cursorSeq < latestSeq) {
        await this.backfill();
      } else {
        if (
          toSeq > this.stateValue.cursorSeq ||
          (toSeq === this.stateValue.cursorSeq && fromSeq !== this.stateValue.cursorSeq + 1)
        ) {
          await this.reset("backfill_empty");
          return;
        }
        await this.persistChanges(
          [],
          this.stateValue.cursorSeq,
          latestSeq,
          this.stateValue.minRetainedSeq,
          remoteFeedId || this.stateValue.feedId,
        );
        if (this.disposed || generation !== this.resetGeneration) {
          return;
        }
        this.update({
          feedId: remoteFeedId || this.stateValue.feedId,
          latestSeq,
          protocolVersion: SYNC_PROTOCOL_VERSION,
          status: "connected",
        });
      }
      return;
    }
    if (toSeq <= this.stateValue.cursorSeq) {
      this.update({
        feedId: remoteFeedId || this.stateValue.feedId,
        latestSeq,
        protocolVersion: SYNC_PROTOCOL_VERSION,
      });
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

    const nextLatestSeq = Math.max(latestSeq, toSeq);
    this.update({
      status: "syncing",
      lastError: null,
    });
    try {
      if (this.disposed || generation !== this.resetGeneration) {
        return;
      }
      // Persist before notifying the UI. A schedule revision can synchronously
      // read the local mirror, so the web cache must already contain this batch.
      await this.persistChanges(
        changes,
        toSeq,
        nextLatestSeq,
        this.stateValue.minRetainedSeq,
        remoteFeedId || this.stateValue.feedId,
      );
      if (this.disposed || generation !== this.resetGeneration) {
        return;
      }
      await this.options.onChanges(changes);
      if (this.disposed || generation !== this.resetGeneration) {
        return;
      }
      this.update({
        feedId: remoteFeedId || this.stateValue.feedId,
        cursorSeq: toSeq,
        latestSeq: nextLatestSeq,
        protocolVersion: SYNC_PROTOCOL_VERSION,
        lastSyncAt: new Date().toISOString(),
        status: toSeq < nextLatestSeq ? "syncing" : "connected",
      });
      this.scheduleAck();
    } catch (error) {
      if (!this.disposed && generation === this.resetGeneration) {
        this.fail(error);
      }
    }
  }

  private applyWebCacheWriteResult(result: WebSyncCacheWriteResult, action: string): void {
    if (result.status === "persisted") {
      this.webCachePersisted = true;
      return;
    }
    if (result.status === "unavailable" && !this.webCachePersisted) {
      // A browser may block or temporarily fail to open IndexedDB. Disable a
      // cache that never worked, but never continue past a cache that was
      // previously initialized and then became unavailable.
      this.webCacheEnabled = false;
      return;
    }
    const detail = result.error?.message ? `：${result.error.message}` : "";
    throw new ApiFailure(
      `${action}失败，已暂停同步游标${detail}`,
      "WEB_SYNC_CACHE_WRITE_FAILED",
    );
  }

  private async persistChanges(
    changes: SyncChange[],
    cursorSeq: number,
    latestSeq: number,
    minRetainedSeq: number,
    feedId = this.stateValue.feedId,
  ): Promise<void> {
    if (!this.webCacheEnabled || this.disposed) {
      return;
    }
    const result = await persistWebSyncBatchWithResult(this.options.baseUrl, changes, {
      feedId,
      cursorSeq,
      latestSeq,
      minRetainedSeq,
      snapshotRevision: this.snapshotRevisionValue,
      ackedSeq: this.stateValue.ackedSeq,
      lastSyncAt: new Date().toISOString(),
    });
    this.applyWebCacheWriteResult(result, "写入同步缓存");
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
    const generation = this.resetGeneration;
    this.update({ status: "syncing", lastError: null });
    let pageCount = 0;
    while (!this.disposed && generation === this.resetGeneration) {
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
        this.abortController.signal,
      );
      if (this.disposed || generation !== this.resetGeneration) {
        return;
      }
      const remoteFeedId = normalizeFeedId(page.feedId);
      const feedChanged = syncFeedChanged(
        this.stateValue.feedId,
        this.stateValue.cursorSeq,
        remoteFeedId,
      );
      const sequenceRegressed =
        page.latestSeq < this.stateValue.cursorSeq ||
        page.latestSeq < this.stateValue.latestSeq;
      const cursorExpired = this.stateValue.cursorSeq < Math.max(0, page.minRetainedSeq - 1);
      if (page.resetRequired || sequenceRegressed || cursorExpired || feedChanged) {
        await this.reset(
          feedChanged
            ? "feed_changed"
            : sequenceRegressed
              ? "sequence_regressed"
              : page.resetRequired
                ? "server_reset_required"
                : "cursor_expired",
        );
        return;
      }
      if (!page.changes.length) {
        if (
          this.stateValue.cursorSeq < page.latestSeq ||
          page.toSeq !== this.stateValue.cursorSeq ||
          page.fromSeq !== this.stateValue.cursorSeq + 1
        ) {
          await this.reset("backfill_empty");
        } else {
          await this.persistChanges(
            [],
            this.stateValue.cursorSeq,
            page.latestSeq,
            page.minRetainedSeq,
            remoteFeedId || this.stateValue.feedId,
          );
          if (this.disposed || generation !== this.resetGeneration) {
            return;
          }
          this.update({
            feedId: remoteFeedId || this.stateValue.feedId,
            latestSeq: page.latestSeq,
            minRetainedSeq: page.minRetainedSeq,
            status: "connected",
            lastError: null,
          });
        }
        return;
      }
      const expectedFrom = this.stateValue.cursorSeq + 1;
      const expectedTo = page.changes[page.changes.length - 1].seq;
      if (
        page.fromSeq !== expectedFrom ||
        page.toSeq !== expectedTo ||
        page.latestSeq < expectedTo ||
        !validateContiguousChanges(page.changes, expectedFrom, expectedTo)
      ) {
        await this.reset("backfill_gap");
        return;
      }
      // Keep cache-first views consistent with the live path: the persisted
      // mirror must include this page before listeners trigger a UI read.
      await this.persistChanges(
        page.changes,
        expectedTo,
        page.latestSeq,
        page.minRetainedSeq,
        remoteFeedId || this.stateValue.feedId,
      );
      if (this.disposed || generation !== this.resetGeneration) {
        return;
      }
      await this.options.onChanges(page.changes);
      if (this.disposed || generation !== this.resetGeneration) {
        return;
      }
      this.update({
        feedId: remoteFeedId || this.stateValue.feedId,
        cursorSeq: expectedTo,
        latestSeq: page.latestSeq,
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
    this.clearCatchUpTimer();
    this.resetGeneration += 1;
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
      this.abortController.signal,
    );
    if (this.disposed) {
      return;
    }
    const snapshotFeedId = normalizeFeedId(snapshot.feedId);
    // A bootstrap can switch feed epochs or move backwards. ACK state from the
    // previous sequence space is not evidence for this snapshot, so force a
    // fresh durable ACK instead of carrying an old ackedSeq forward.
    const nextAckedSeq = 0;
    if (this.webCacheEnabled) {
      // Replace the mirror before the snapshot listener bumps the schedule
      // revision; otherwise a cache-first view can briefly read the old mirror.
      const result = await replaceWebSyncCacheWithResult(this.options.baseUrl, snapshot, {
        feedId: snapshotFeedId,
        cursorSeq: snapshot.snapshotRevision,
        latestSeq: snapshot.snapshotRevision,
        minRetainedSeq: snapshot.minRetainedSeq,
        snapshotRevision: snapshot.snapshotRevision,
        ackedSeq: nextAckedSeq,
        lastSyncAt: new Date().toISOString(),
      });
      this.applyWebCacheWriteResult(result, "替换同步缓存");
    }
    if (this.disposed) {
      return;
    }
    // Only publish the new snapshot revision after the durable cache write has
    // succeeded; otherwise a failed replacement would leave a mixed revision.
    this.snapshotRevisionValue = snapshot.snapshotRevision;
    await this.options.onSnapshot(snapshot);
    if (this.disposed) {
      return;
    }
    this.update({
      feedId: snapshotFeedId,
      cursorSeq: snapshot.snapshotRevision,
      latestSeq: snapshot.snapshotRevision,
      minRetainedSeq: snapshot.minRetainedSeq,
      ackedSeq: nextAckedSeq,
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
    if (
      this.ackPromise ||
      !this.ackSender ||
      this.stateValue.cursorSeq <= this.stateValue.ackedSeq ||
      this.disposed
    ) {
      return;
    }
    this.clearAckTimer();
    this.ackTimer = setTimeout(() => {
      this.ackTimer = null;
      void this.flushAck();
    }, this.options.ackIntervalMs ?? SYNC_ACK_INTERVAL_MS);
  }

  private flushAck(): Promise<void> {
    if (this.ackPromise) {
      return this.ackPromise;
    }
    const seq = this.stateValue.cursorSeq;
    if (seq <= this.stateValue.ackedSeq || !this.ackSender || this.disposed) {
      return Promise.resolve();
    }
    const generation = this.resetGeneration;
    const task = (async () => {
      try {
        // A WebSocket send only proves transport acceptance. The REST endpoint
        // reads back the persisted device ACK before the local cursor advances.
        const confirmedSeq = await acknowledgeSyncCursor(
          this.options.baseUrl,
          this.options.token,
          this.stateValue.deviceId,
          seq,
          this.options.fetchImpl ?? fetch,
          this.abortController.signal,
        );
        if (this.disposed || generation !== this.resetGeneration) {
          return;
        }
        try {
          await this.ackSender?.({ type: "ack", seq });
        } catch {
          // The durable REST ACK already succeeded; the socket copy is advisory.
        }
        if (this.disposed || generation !== this.resetGeneration) {
          return;
        }
        this.update({
          ackedSeq: Math.max(
            this.stateValue.ackedSeq,
            Math.min(confirmedSeq, this.stateValue.cursorSeq),
          ),
        });
      } catch {
        // Never advance ackedSeq without a persisted server confirmation.
      }
    })();
    const tracked = task.finally(() => {
      this.ackPromise = null;
      if (!this.disposed && this.stateValue.cursorSeq > this.stateValue.ackedSeq) {
        this.scheduleAck();
      }
    });
    this.ackPromise = tracked;
    return tracked;
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
