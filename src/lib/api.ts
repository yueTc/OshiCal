import { invoke } from "@tauri-apps/api/core";
import type {
  AccountSearchResult,
  AppSettings,
  ApplySyncBatchResult,
  ApplySyncBootstrapResult,
  BiliQrLoginPoll,
  BiliQrLoginStart,
  ServerBiliQrLoginStart,
  ServerBiliQrLoginPoll,
  ServerBiliStatus,
  Bootstrap,
  DeviceInfo,
  DeviceRegistration,
  FertilitySnapshot,
  ImportPreview,
  ImportReport,
  LiveSession,
  LiveStatus,
  LlmDiscoveryInput,
  LlmSecret,
  LlmTestInput,
  LlmTestReport,
  NewScheduleInput,
  NewWeeklyScheduleRuleInput,
  ProxyTestReport,
  RefreshJob,
  RefreshJobAccepted,
  RefreshReport,
  Schedule,
  SecretStatus,
  ServerSnapshot,
  Streamer,
  SyncBatch,
  SyncBootstrap,
  SyncChangesPage,
  SyncMeta,
  SyncMutation,
  UpdateScheduleInput,
  UpdateSubscriptionInput,
  UpdateWeeklyScheduleRuleInput,
  WeeklyScheduleRule,
} from "../types";

export const isTauri = "__TAURI_INTERNALS__" in window;

export class ApiFailure extends Error {
  code: string;
  status: number;

  constructor(message: string, code = "REQUEST_FAILED", status = 0) {
    super(message);
    this.name = "ApiFailure";
    this.code = code;
    this.status = status;
  }
}

interface Envelope<T> {
  ok: boolean;
  data?: T;
  error?: { code: string; message: string };
}

interface ServerApiOptions {
  baseUrl: string;
  token: string;
}

export class ServerApi {
  private baseUrl: string;
  private token: string;

  constructor(options: ServerApiOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.token = options.token;
  }

  setToken(token: string): void {
    this.token = token;
  }

  eventSocketUrl(after = 0, protocol = 2): string {
    const url = new URL(this.baseUrl || window.location.origin);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    const basePath = url.pathname.replace(/\/+$/, "");
    url.pathname = `${basePath}/api/ws`;
    url.search = `?after=${Math.max(0, after)}&protocol=${protocol}`;
    return url.toString();
  }

  eventToken(): string {
    return this.token;
  }

  private async request<T>(path: string, init?: RequestInit): Promise<T> {
    let response: Response;
    try {
      response = await fetch(`${this.baseUrl}${path}`, {
        ...init,
        headers: {
          Authorization: `Bearer ${this.token}`,
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
    let envelope: Envelope<T> | null = null;
    try {
      envelope = text ? (JSON.parse(text) as Envelope<T>) : null;
    } catch {
      throw new ApiFailure("服务器返回了非 JSON 响应", "INVALID_RESPONSE", response.status);
    }
    if (!response.ok || !envelope?.ok) {
      const code = envelope?.error?.code ?? `HTTP_${response.status}`;
      const message = envelope?.error?.message ?? `请求失败: HTTP ${response.status}`;
      throw new ApiFailure(message, code, response.status);
    }
    return envelope.data as T;
  }

  health(): Promise<{ status: string; version: string }> {
    return this.request("/api/health");
  }

  search(keyword: string): Promise<AccountSearchResult[]> {
    return this.request(`/api/search?q=${encodeURIComponent(keyword)}`);
  }

  subscriptions(): Promise<Streamer[]> {
    return this.request("/api/subscriptions");
  }

  addSubscription(input: NewSubscriptionInput): Promise<Streamer> {
    return this.request("/api/subscriptions", {
      method: "POST",
      body: JSON.stringify(input),
    });
  }

  updateSubscription(id: number, update: UpdateSubscriptionInput): Promise<Streamer> {
    return this.request(`/api/subscriptions/${id}`, {
      method: "PATCH",
      body: JSON.stringify(update),
    });
  }

  reorderSubscriptions(ids: number[]): Promise<Streamer[]> {
    return this.request("/api/subscriptions/reorder", {
      method: "POST",
      body: JSON.stringify({ ids }),
    });
  }

  deleteSubscription(id: number): Promise<void> {
    return this.request(`/api/subscriptions/${id}`, { method: "DELETE" });
  }

  addLinkedAccount(id: number, mid: string): Promise<unknown> {
    return this.request(`/api/subscriptions/${id}/links`, {
      method: "POST",
      body: JSON.stringify({ mid }),
    });
  }

  removeLinkedAccount(id: number, mid: string): Promise<unknown> {
    return this.request(`/api/subscriptions/${id}/links/${mid}`, { method: "DELETE" });
  }

  refresh(): Promise<RefreshReport> {
    return this.request("/api/refresh", { method: "POST" });
  }

  refreshStreamer(id: number): Promise<RefreshReport> {
    return this.request(`/api/subscriptions/${id}/refresh`, { method: "POST" });
  }

  schedules(from: string, to: string): Promise<Schedule[]> {
    return this.request(
      `/api/schedules?from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`,
    );
  }

  createSchedule(input: NewScheduleInput): Promise<Schedule> {
    return this.request("/api/schedules", {
      method: "POST",
      body: JSON.stringify(input),
    });
  }

  updateSchedule(id: number, update: UpdateScheduleInput): Promise<Schedule> {
    return this.request(`/api/schedules/${id}`, {
      method: "PATCH",
      body: JSON.stringify(update),
    });
  }

  deleteSchedule(id: number): Promise<void> {
    return this.request(`/api/schedules/${id}`, { method: "DELETE" });
  }

  listWeeklyRules(streamerId: number): Promise<WeeklyScheduleRule[]> {
    return this.request(`/api/weekly-rules?streamerId=${encodeURIComponent(streamerId)}`);
  }

  createWeeklyRule(input: NewWeeklyScheduleRuleInput): Promise<WeeklyScheduleRule> {
    return this.request("/api/weekly-rules", {
      method: "POST",
      body: JSON.stringify(input),
    });
  }

  updateWeeklyRule(id: number, update: UpdateWeeklyScheduleRuleInput): Promise<WeeklyScheduleRule> {
    return this.request(`/api/weekly-rules/${id}`, {
      method: "PATCH",
      body: JSON.stringify(update),
    });
  }

  deleteWeeklyRule(id: number): Promise<void> {
    return this.request(`/api/weekly-rules/${id}`, { method: "DELETE" });
  }

  liveStatuses(): Promise<LiveStatus[]> {
    return this.request("/api/live-status");
  }

  liveSessions(streamerId?: number, limit = 50): Promise<LiveSession[]> {
    const params = new URLSearchParams({ limit: String(limit) });
    if (streamerId !== undefined) {
      params.set("streamerId", String(streamerId));
    }
    return this.request(`/api/live-sessions?${params.toString()}`);
  }

  liveSessionForSchedule(scheduleId: number): Promise<LiveSession | null> {
    return this.request(`/api/live-sessions/by-schedule/${scheduleId}`);
  }

  fertility(refresh = false): Promise<FertilitySnapshot> {
    return this.request(`/api/fertility?refresh=${refresh ? "true" : "false"}`);
  }
  settings(): Promise<AppSettings> {
    return this.request("/api/settings");
  }

  saveSettings(settings: AppSettings): Promise<AppSettings> {
    return this.request("/api/settings", {
      method: "PUT",
      body: JSON.stringify(settings),
    });
  }

  llmSecret(): Promise<LlmSecret> {
    return this.request("/api/secrets/llm");
  }

  saveLlmSecret(apiKey: string): Promise<LlmSecret> {
    return this.request("/api/secrets/llm", {
      method: "PUT",
      body: JSON.stringify({ apiKey }),
    });
  }

  serverBiliStatus(): Promise<ServerBiliStatus> {
    return this.request("/api/bilibili/status");
  }

  startServerBiliQrLogin(): Promise<ServerBiliQrLoginStart> {
    return this.request("/api/bilibili/login/qr", {
      method: "POST",
      body: JSON.stringify({}),
    });
  }

  pollServerBiliQrLogin(sessionId: string): Promise<ServerBiliQrLoginPoll> {
    return this.request(`/api/bilibili/login/qr/${encodeURIComponent(sessionId)}`);
  }

  logoutServerBili(): Promise<ServerBiliStatus> {
    return this.request("/api/bilibili/logout", {
      method: "POST",
      body: JSON.stringify({}),
    });
  }

  testProxy(proxyPort: number): Promise<ProxyTestReport> {
    return this.request("/api/proxy/test", {
      method: "POST",
      body: JSON.stringify({ proxyPort }),
    });
  }

  testLlm(input: LlmTestInput): Promise<LlmTestReport> {
    return this.request("/api/llm/test", {
      method: "POST",
      body: JSON.stringify({ ...input, apiKey: input.apiKey ?? "" }),
    });
  }

  listLlmModels(input: LlmDiscoveryInput): Promise<string[]> {
    return this.request("/api/llm/models", {
      method: "POST",
      body: JSON.stringify({ ...input, apiKey: input.apiKey ?? "" }),
    });
  }

  snapshot(): Promise<ServerSnapshot> {
    return this.request("/api/sync/snapshot");
  }
  previewMigration(snapshot: ServerSnapshot): Promise<ImportPreview> {
    return this.request("/api/migration/preview", {
      method: "POST",
      body: JSON.stringify(snapshot),
    });
  }

  applyMigration(
    snapshot: ServerSnapshot,
    strategy: "server" | "client" | "skip",
    clientId: string,
    migrationId: string,
  ): Promise<ImportReport> {
    return this.request("/api/migration/apply", {
      method: "POST",
      body: JSON.stringify({ snapshot, strategy, clientId, migrationId }),
    });
  }


  registerDevice(registration: DeviceRegistration): Promise<DeviceInfo> {
    return this.request("/api/devices/register", {
      method: "POST",
      body: JSON.stringify(registration),
    });
  }

  deleteDeviceLog(deviceId: string): Promise<{ deleted: string; logOnly: boolean }> {
    return this.request(`/api/devices/${encodeURIComponent(deviceId)}`, {
      method: "DELETE",
    });
  }

  syncBootstrap(): Promise<SyncBootstrap> {
    return this.request("/api/sync/bootstrap");
  }

  syncChanges(after = 0, limit = 200): Promise<SyncChangesPage> {
    return this.request(
      `/api/sync/changes?after=${Math.max(0, after)}&limit=${Math.min(500, Math.max(1, limit))}`,
    );
  }

  syncAck(deviceId: string, seq: number): Promise<{ deviceId: string; seq: number }> {
    return this.request("/api/sync/ack", {
      method: "POST",
      body: JSON.stringify({ deviceId, seq }),
    });
  }

  refreshJob(streamerId?: number): Promise<RefreshJobAccepted> {
    const path = streamerId === undefined
      ? "/api/jobs/refresh"
      : `/api/jobs/refresh/${streamerId}`;
    return this.request(path, {
      method: "POST",
      body: JSON.stringify({}),
    });
  }
  refreshJobStatus(jobId: string): Promise<RefreshJob> {
    return this.request("/api/jobs/" + encodeURIComponent(jobId));
  }


  applyMutations(
    mutations: SyncMutation[],
  ): Promise<{ mutationId: string; status: string; error: string | null }[]> {
    return this.request("/api/sync/mutations", {
      method: "POST",
      body: JSON.stringify(mutations),
    });
  }
}

export interface NewSubscriptionInput {
  mid: string;
  name?: string;
  avatarUrl?: string;
  roomId?: string | null;
  aliases?: string[];
  scheduleEnabled: boolean;
  liveNotifyEnabled: boolean;
  liveStatsEnabled?: boolean;
}

export class LocalApi {
  bootstrap(): Promise<Bootstrap> {
    return invoke("bootstrap");
  }

  search(keyword: string): Promise<AccountSearchResult[]> {
    return invoke("search_accounts", { keyword });
  }

  addSubscription(input: NewSubscriptionInput): Promise<Streamer> {
    return invoke("add_subscription", { input });
  }

  updateSubscription(id: number, update: UpdateSubscriptionInput): Promise<Streamer> {
    return invoke("update_subscription", { id, update });
  }

  reorderSubscriptions(ids: number[]): Promise<Streamer[]> {
    return invoke("reorder_subscriptions", { ids });
  }

  deleteSubscription(id: number): Promise<void> {
    return invoke("delete_subscription", { id });
  }

  addLinkedAccount(id: number, mid: string): Promise<unknown> {
    return invoke("add_linked_account", { id, mid });
  }

  removeLinkedAccount(id: number, mid: string): Promise<void> {
    return invoke("remove_linked_account", { id, mid });
  }

  refresh(): Promise<RefreshReport> {
    return invoke("refresh_all");
  }

  refreshStreamer(id: number): Promise<RefreshReport> {
    return invoke("refresh_streamer", { id });
  }

  schedules(from: string, to: string): Promise<Schedule[]> {
    return invoke("list_schedules", { from, to });
  }

  createSchedule(input: NewScheduleInput): Promise<Schedule> {
    return invoke("create_schedule", { input });
  }

  updateSchedule(id: number, update: UpdateScheduleInput): Promise<Schedule> {
    return invoke("update_schedule", { id, update });
  }

  deleteSchedule(id: number): Promise<void> {
    return invoke("delete_schedule", { id });
  }

  createCachedSchedule(input: NewScheduleInput): Promise<Schedule> {
    return invoke("cache_create_schedule", { input });
  }

  updateCachedSchedule(id: number, update: UpdateScheduleInput): Promise<Schedule> {
    return invoke("cache_update_schedule", { id, update });
  }

  deleteCachedSchedule(id: number): Promise<void> {
    return invoke("cache_delete_schedule", { id });
  }

  purgeOfflineSchedules(): Promise<number> {
    return invoke("purge_offline_schedules");
  }

  listWeeklyRules(streamerId: number): Promise<WeeklyScheduleRule[]> {
    return invoke("list_weekly_rules", { streamerId });
  }

  createWeeklyRule(input: NewWeeklyScheduleRuleInput): Promise<WeeklyScheduleRule> {
    return invoke("create_weekly_rule", { input });
  }

  updateWeeklyRule(id: number, update: UpdateWeeklyScheduleRuleInput): Promise<WeeklyScheduleRule> {
    return invoke("update_weekly_rule", { id, update });
  }

  deleteWeeklyRule(id: number): Promise<void> {
    return invoke("delete_weekly_rule", { id });
  }

  liveStatuses(): Promise<LiveStatus[]> {
    return invoke("list_live_statuses");
  }

  liveSessions(streamerId?: number, limit = 50): Promise<LiveSession[]> {
    return invoke("list_live_sessions", { streamerId, limit });
  }

  liveSessionForSchedule(scheduleId: number): Promise<LiveSession | null> {
    return invoke("live_session_for_schedule", { scheduleId });
  }

  fertility(refresh = false): Promise<FertilitySnapshot> {
    return invoke("list_fertility_statuses", { refresh });
  }
  settings(): Promise<AppSettings> {
    return invoke("get_settings");
  }

  saveSettings(settings: AppSettings): Promise<AppSettings> {
    return invoke("save_settings", { settings });
  }

  testProxy(proxyPort: number): Promise<ProxyTestReport> {
    return invoke("test_proxy", { input: { proxyPort } });
  }

  testLlm(input: LlmTestInput): Promise<LlmTestReport> {
    return invoke("test_llm", {
      config: { ...input, apiKey: input.apiKey ?? "" },
    });
  }

  listLlmModels(input: LlmDiscoveryInput): Promise<string[]> {
    return invoke("list_llm_models", {
      config: { ...input, apiKey: input.apiKey ?? "" },
    });
  }

  saveSecrets(input: {
    llmApiKey?: string;
    biliCookie?: string;
    serverToken?: string;
  }): Promise<SecretStatus> {
    return invoke("save_secrets", { input });
  }

  secretStatus(): Promise<SecretStatus> {
    return invoke("get_secret_status");
  }

  openBiliBrowserLogin(): Promise<void> {
    return invoke("open_bilibili_browser_login");
  }

  startBiliQrLogin(): Promise<BiliQrLoginStart> {
    return invoke("start_bilibili_qr_login");
  }

  pollBiliQrLogin(qrcodeKey: string): Promise<BiliQrLoginPoll> {
    return invoke("poll_bilibili_qr_login", { qrcodeKey });
  }

  clearBiliLogin(): Promise<SecretStatus> {
    return invoke("clear_bilibili_cookie");
  }

  serverToken(): Promise<string | null> {
    return invoke("get_server_token");
  }

  startRemoteMonitor(serverUrl: string, token: string): Promise<void> {
    return invoke("start_remote_monitor", { serverUrl, token });
  }

  stopRemoteMonitor(): Promise<void> {
    return invoke("stop_remote_monitor");
  }

  rebuildRemoteSync(serverUrl: string, token: string): Promise<number> {
    return invoke("rebuild_remote_sync", { serverUrl, token });
  }

  deviceId(): Promise<string> {
    return invoke("get_device_id");
  }

  syncMeta(serverUrl: string): Promise<SyncMeta> {
    return invoke("get_sync_meta", { serverUrl });
  }

  listSyncDevices(serverUrl: string, token: string): Promise<DeviceInfo[]> {
    return invoke("list_sync_devices", { serverUrl, token });
  }

  syncCursor(serverUrl: string): Promise<number> {
    return invoke("get_sync_cursor", { serverUrl });
  }

  setSyncAck(serverUrl: string, seq: number): Promise<SyncMeta> {
    return invoke("set_sync_ack", { serverUrl, seq });
  }

  applySyncBatch(serverUrl: string, batch: SyncBatch): Promise<ApplySyncBatchResult> {
    return invoke("apply_sync_batch", { serverUrl, batch });
  }

  applySyncBootstrap(
    serverUrl: string,
    bootstrap: SyncBootstrap,
  ): Promise<ApplySyncBootstrapResult> {
    return invoke("apply_sync_bootstrap", { serverUrl, bootstrap });
  }

  enqueueMutation(mutation: SyncMutation): Promise<void> {
    return invoke("enqueue_mutation", { mutation });
  }

  pendingMutations(): Promise<SyncMutation[]> {
    return invoke("list_pending_mutations");
  }

  exportSnapshot(): Promise<ServerSnapshot> {
    return invoke("export_client_snapshot");
  }

  updateMutationStatus(mutationId: string, status: string, error?: string): Promise<void> {
    return invoke("update_mutation_status", { mutationId, status, error });
  }

  cachedSchedules(from: string, to: string): Promise<Schedule[]> {
    return invoke("cached_schedules", { from, to });
  }

  cachedSchedule(remoteId: number): Promise<Schedule | null> {
    return invoke("cached_schedule", { remoteId });
  }

  remoteEntity(entity: string, localId: number): Promise<number | null> {
    return invoke("remote_entity", { entity, localId });
  }

  previewSnapshot(snapshot: ServerSnapshot): Promise<ImportPreview> {
    return invoke("preview_server_snapshot", { snapshot });
  }

  importSnapshot(snapshot: ServerSnapshot, strategy: "server" | "client" | "skip"): Promise<{
    subscriptions: number;
    schedules: number;
    skipped: number;
  }> {
    return invoke("import_server_snapshot", { snapshot, strategy });
  }
}

export async function openExternalUrl(url: string): Promise<void> {
  if (isTauri) {
    await invoke("open_external_url", { url });
    return;
  }
  const parsed = new URL(url);
  if (
    parsed.protocol !== "https:" ||
    !["live.bilibili.com", "space.bilibili.com", "t.bilibili.com", "www.bilibili.com", "laplace.live"].includes(parsed.hostname)
  ) {
    throw new Error("只允许打开 B 站直播间、个人主页、动态、站内作品页面或 LAPLACE 魔法期页面");
  }
  window.open(parsed.toString(), "_blank", "noopener,noreferrer");
}

export function explainError(error: unknown): string {
  if (error instanceof ApiFailure) {
    return error.message;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}




