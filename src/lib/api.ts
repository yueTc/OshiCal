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
  FanHistory,
  FertilitySnapshot,
  ImportPreview,
  ImportReport,
  LiveSession,
  LiveOnlineSamplesPage,
  LiveStatus,
  LlmDiscoveryInput,
  LlmSecret,
  LlmTestInput,
  LlmTestReport,
  MigrationPreview,
  NewScheduleInput,
  NewWeeklyScheduleRuleInput,
  Platform,
  ProxyTestReport,
  RefreshJob,
  RefreshJobAccepted,
  RefreshReport,
  Schedule,
  SecretStatus,
  ServerSnapshot,
  ServerUpdateInfo,
  ServerUpdateSettings,
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
  YoutubeCredentialStatus,
  YoutubeOAuthStart,
  YoutubeWebLoginStart,
} from "../types";

export const isTauri = "__TAURI_INTERNALS__" in window;
const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;
const LONG_REQUEST_TIMEOUT_MS = 180_000;

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

function recordValue(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function stringFrom(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function booleanFrom(value: unknown): boolean | null {
  return typeof value === "boolean" ? value : null;
}

export function normalizeYoutubeStatus(value: unknown): YoutubeCredentialStatus {
  const record = recordValue(value);
  const secrets = recordValue(record.secrets ?? record);
  const apiKey = recordValue(secrets.apiKey ?? secrets.api_key ?? secrets);
  const oauth = recordValue(secrets.oauth ?? secrets);
  const webSession = recordValue(
    secrets.webSession ?? secrets.web_session ?? secrets.session ?? secrets,
  );
  const oauthExpired = booleanFrom(oauth.oauthExpired ?? oauth.oauth_expired ?? oauth.expired) === true;
  // Status is deliberately projected to state only. Provider errors can echo
  // request details, so never put a raw token/key/cookie or arbitrary error text
  // into the credential panel DOM.
  const oauthError = stringFrom(
    oauth.lastError ?? oauth.last_error,
  );
  const statusError = stringFrom(
    secrets.lastError ?? secrets.last_error ?? record.lastError ?? record.last_error,
  );
  return {
    apiKeyConfigured:
      booleanFrom(apiKey.configured ?? apiKey.apiKeyConfigured ?? apiKey.api_key_configured)
      ?? false,
    apiKeyValid: booleanFrom(apiKey.apiKeyValid ?? apiKey.api_key_valid),
    oauthConfigured:
      booleanFrom(oauth.configured ?? oauth.oauthConfigured ?? oauth.oauth_configured)
      ?? false,
    oauthExpired,
    oauthAccount: stringFrom(oauth.account ?? oauth.oauthAccount ?? oauth.oauth_account),
    webSessionConfigured:
      booleanFrom(webSession.configured ?? webSession.webSessionConfigured ?? webSession.web_session_configured)
      ?? false,
    webSessionAccount: stringFrom(
      webSession.account
        ?? webSession.webSessionAccount
        ?? webSession.web_session_account,
    ),
    checkedAt: stringFrom(record.checkedAt ?? record.checked_at),
    lastError: oauthExpired
      ? "Google OAuth 已过期，请重新授权。"
      : oauthError || statusError
        ? "YouTube 凭据状态检查失败，请重新授权或稍后重试。"
        : null,
  };
}

function normalizeYoutubeOAuthStart(value: unknown): YoutubeOAuthStart {
  const record = recordValue(value);
  return {
    authorizationUrl: stringFrom(record.authorizationUrl ?? record.authUrl ?? record.url) ?? "",
    state: stringFrom(record.state),
    expiresInSeconds: typeof record.expiresInSeconds === "number" ? record.expiresInSeconds : undefined,
  };
}

function normalizeYoutubeWebLoginStart(value: unknown): YoutubeWebLoginStart {
  const record = recordValue(value);
  return {
    loginUrl: stringFrom(record.loginUrl ?? record.url),
    message: stringFrom(record.message ?? record.note),
  };
}

function normalizeAccountSearchResult(
  value: unknown,
  fallbackPlatform: Platform,
): AccountSearchResult {
  const record = recordValue(value);
  const platform = record.platform === "youtube"
    ? "youtube"
    : record.platform === "bilibili"
      ? "bilibili"
      : fallbackPlatform;
  const fans = typeof record.fans === "number" && Number.isFinite(record.fans)
    ? Math.max(0, Math.trunc(record.fans))
    : 0;
  return {
    platform,
    mid: stringFrom(record.mid) ?? "",
    name: stringFrom(record.name) ?? "",
    avatarUrl: stringFrom(record.avatarUrl) ?? "",
    sign: stringFrom(record.sign) ?? "",
    handle: stringFrom(record.handle),
    profileUrl: stringFrom(record.profileUrl),
    uploadsPlaylistId: stringFrom(record.uploadsPlaylistId),
    roomId: stringFrom(record.roomId),
    fans,
    isLive: booleanFrom(record.isLive) ?? false,
    liveUrl: stringFrom(record.liveUrl),
  };
}

function normalizeAccountSearchResults(
  value: unknown,
  fallbackPlatform: Platform,
): AccountSearchResult[] {
  if (!Array.isArray(value)) {
    throw new ApiFailure("账号搜索响应格式无效", "INVALID_RESPONSE");
  }
  return value.map((item) => normalizeAccountSearchResult(item, fallbackPlatform));
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

  private async request<T>(
    path: string,
    init?: RequestInit,
    timeoutMs = DEFAULT_REQUEST_TIMEOUT_MS,
  ): Promise<T> {
    const controller = new AbortController();
    const externalSignal = init?.signal;
    const forwardAbort = () => controller.abort(externalSignal?.reason);
    if (externalSignal?.aborted) {
      forwardAbort();
    } else {
      externalSignal?.addEventListener("abort", forwardAbort, { once: true });
    }
    const timeout = window.setTimeout(() => {
      controller.abort(new DOMException("请求超时", "TimeoutError"));
    }, timeoutMs);
    let response: Response;
    let text: string;
    try {
      response = await fetch(`${this.baseUrl}${path}`, {
        ...init,
        signal: controller.signal,
        headers: {
          Authorization: `Bearer ${this.token}`,
          "Content-Type": "application/json",
          ...(init?.headers ?? {}),
        },
      });
      text = await response.text();
    } catch (error) {
      const timedOut = controller.signal.aborted && !externalSignal?.aborted;
      const aborted = externalSignal?.aborted === true;
      throw new ApiFailure(
        aborted
          ? "请求已取消"
          : timedOut
            ? `服务器请求超时（${Math.round(timeoutMs / 1000)} 秒）`
            : error instanceof Error
              ? error.message
              : "服务器连接失败",
        aborted ? "REQUEST_ABORTED" : timedOut ? "REQUEST_TIMEOUT" : "REMOTE_OFFLINE",
      );
    } finally {
      window.clearTimeout(timeout);
      externalSignal?.removeEventListener("abort", forwardAbort);
    }
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

  async search(keyword: string, platform: Platform = "bilibili"): Promise<AccountSearchResult[]> {
    const path = platform === "youtube"
      ? `/api/youtube/search?q=${encodeURIComponent(keyword)}`
      : `/api/search?q=${encodeURIComponent(keyword)}`;
    return normalizeAccountSearchResults(await this.request<unknown>(path), platform);
  }

  subscriptions(): Promise<Streamer[]> {
    return this.request("/api/subscriptions");
  }

  addSubscription(input: NewSubscriptionInput): Promise<Streamer> {
    return this.request(input.platform === "youtube" ? "/api/youtube/subscriptions" : "/api/subscriptions", {
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

  addLinkedAccount(id: number, mid: string, platform: Platform = "bilibili"): Promise<unknown> {
    return this.request(`/api/subscriptions/${id}/links`, {
      method: "POST",
      body: JSON.stringify({ platform, mid }),
    });
  }

  removeLinkedAccount(id: number, mid: string, platform: Platform = "bilibili"): Promise<unknown> {
    const base = platform === "youtube" ? "/api/youtube/subscriptions" : "/api/subscriptions";
    return this.request(
      `${base}/${id}/links/${encodeURIComponent(mid)}`,
      { method: "DELETE" },
    );
  }

  refresh(): Promise<RefreshReport> {
    return this.request("/api/refresh", { method: "POST" }, LONG_REQUEST_TIMEOUT_MS);
  }

  refreshStreamer(id: number): Promise<RefreshReport> {
    return this.request(`/api/subscriptions/${id}/refresh`, { method: "POST" }, LONG_REQUEST_TIMEOUT_MS);
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

  fanHistory(streamerId: number, days = 30): Promise<FanHistory> {
    return this.request(
      `/api/subscriptions/${streamerId}/fans?days=${encodeURIComponent(days)}`,
    );
  }

  refreshFans(streamerId: number): Promise<FanHistory> {
    return this.request(
      `/api/subscriptions/${streamerId}/fans/refresh`,
      { method: "POST" },
      LONG_REQUEST_TIMEOUT_MS,
    );
  }

  liveSessions(streamerId?: number, limit = 50): Promise<LiveSession[]> {
    const params = new URLSearchParams({ limit: String(limit) });
    if (streamerId !== undefined) {
      params.set("streamerId", String(streamerId));
    }
    return this.request(`/api/live-sessions?${params.toString()}`);
  }

  liveOnlineSamples(sessionId: number, afterId = 0, limit = 500, signal?: AbortSignal): Promise<LiveOnlineSamplesPage> {
    const params = new URLSearchParams({ afterId: String(afterId), limit: String(limit) });
    return this.request(
      "/api/live-sessions/" + sessionId + "/online-samples?" + params.toString(),
      { signal },
    );
  }

  liveSessionForSchedule(scheduleId: number): Promise<LiveSession | null> {
    return this.request(`/api/live-sessions/by-schedule/${scheduleId}`);
  }

  /** 读取 LAPLACE 生理期数据；refresh 为 true 时绕过服务端 6 小时缓存。 */
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

  serverUpdate(): Promise<ServerUpdateInfo> {
    return this.request("/api/system/update");
  }

  saveServerUpdateSettings(settings: ServerUpdateSettings): Promise<ServerUpdateInfo> {
    return this.request("/api/system/update", {
      method: "PUT",
      body: JSON.stringify(settings),
    });
  }

  checkServerUpdate(): Promise<ServerUpdateInfo> {
    return this.request(
      "/api/system/update/check",
      { method: "POST", body: JSON.stringify({}) },
      LONG_REQUEST_TIMEOUT_MS,
    );
  }

  installServerUpdate(): Promise<ServerUpdateInfo> {
    return this.request(
      "/api/system/update/install",
      { method: "POST", body: JSON.stringify({}) },
      LONG_REQUEST_TIMEOUT_MS,
    );
  }

  llmSecret(): Promise<LlmSecret> {
    return this.request("/api/secrets/llm");
  }

  saveLlmSecret(apiKey: string, baseUrl?: string): Promise<LlmSecret> {
    return this.request("/api/secrets/llm", {
      method: "PUT",
      body: JSON.stringify({
        apiKey,
        ...(baseUrl?.trim() ? { baseUrl: baseUrl.trim() } : {}),
      }),
    });
  }

  async youtubeStatus(): Promise<YoutubeCredentialStatus> {
    return normalizeYoutubeStatus(await this.request<unknown>("/api/secrets/youtube"));
  }

  async saveYoutubeApiKey(apiKey: string): Promise<YoutubeCredentialStatus> {
    await this.request<unknown>("/api/secrets/youtube", {
      method: "PUT",
      body: JSON.stringify({ apiKey }),
    });
    return this.youtubeStatus();
  }

  async clearYoutubeApiKey(): Promise<YoutubeCredentialStatus> {
    await this.request<unknown>("/api/secrets/youtube", { method: "DELETE" });
    return this.youtubeStatus();
  }

  async testYoutubeApiKey(): Promise<YoutubeCredentialStatus> {
    const status = await this.youtubeStatus();
    if (!status.apiKeyConfigured) {
      return { ...status, apiKeyValid: false, checkedAt: new Date().toISOString() };
    }
    // Data API search.list is invoked only by the explicit test button; status
    // polling must never consume YouTube quota.
    await this.search("YouTube", "youtube");
    return {
      ...status,
      apiKeyValid: true,
      checkedAt: new Date().toISOString(),
      lastError: null,
    };
  }

  async startYoutubeOAuth(): Promise<YoutubeOAuthStart> {
    return normalizeYoutubeOAuthStart(await this.request<unknown>("/api/youtube/oauth/start", {
      method: "POST",
      body: JSON.stringify({}),
    }));
  }

  async completeYoutubeOAuth(code: string, state: string): Promise<YoutubeCredentialStatus> {
    await this.request<unknown>("/api/youtube/oauth/complete", {
      method: "POST",
      body: JSON.stringify({ code, state }),
    });
    return this.youtubeStatus();
  }

  async clearYoutubeOAuth(): Promise<YoutubeCredentialStatus> {
    await this.request<unknown>("/api/youtube/oauth/logout", {
      method: "POST",
      body: JSON.stringify({}),
    });
    return this.youtubeStatus();
  }

  async startYoutubeWebLogin(): Promise<YoutubeWebLoginStart> {
    return normalizeYoutubeWebLoginStart(await this.request<unknown>("/api/youtube/session/start", {
      method: "POST",
      body: JSON.stringify({}),
    }));
  }

  async saveYoutubeWebSession(cookie: string): Promise<YoutubeCredentialStatus> {
    await this.request<unknown>("/api/youtube/session", {
      method: "PUT",
      body: JSON.stringify({ cookie }),
    });
    return this.youtubeStatus();
  }

  async clearYoutubeWebSession(): Promise<YoutubeCredentialStatus> {
    await this.request<unknown>("/api/youtube/session", { method: "DELETE" });
    return this.youtubeStatus();
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
    return this.request(
      "/api/proxy/test",
      { method: "POST", body: JSON.stringify({ proxyPort }) },
      LONG_REQUEST_TIMEOUT_MS,
    );
  }

  testLlm(input: LlmTestInput): Promise<LlmTestReport> {
    return this.request(
      "/api/llm/test",
      { method: "POST", body: JSON.stringify({ ...input, apiKey: input.apiKey ?? "" }) },
      LONG_REQUEST_TIMEOUT_MS,
    );
  }

  listLlmModels(input: LlmDiscoveryInput): Promise<string[]> {
    return this.request(
      "/api/llm/models",
      { method: "POST", body: JSON.stringify({ ...input, apiKey: input.apiKey ?? "" }) },
      LONG_REQUEST_TIMEOUT_MS,
    );
  }

  snapshot(): Promise<ServerSnapshot> {
    return this.request("/api/sync/snapshot");
  }
  previewMigration(snapshot: ServerSnapshot): Promise<MigrationPreview> {
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
    expectedServerDigest: string,
  ): Promise<ImportReport> {
    return this.request(
      "/api/migration/apply",
      {
        method: "POST",
        body: JSON.stringify({
          snapshot,
          strategy,
          clientId,
          migrationId,
          expectedServerDigest,
        }),
      },
      LONG_REQUEST_TIMEOUT_MS,
    );
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
  platform?: Platform;
  mid: string;
  name?: string;
  avatarUrl?: string;
  handle?: string | null;
  profileUrl?: string | null;
  uploadsPlaylistId?: string | null;
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

  async search(keyword: string, platform: Platform = "bilibili"): Promise<AccountSearchResult[]> {
    return normalizeAccountSearchResults(
      await invoke<unknown>("search_accounts", { keyword, platform }),
      platform,
    );
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

  addLinkedAccount(id: number, mid: string, platform: Platform = "bilibili"): Promise<unknown> {
    return invoke("add_linked_account", { id, mid, platform });
  }

  removeLinkedAccount(id: number, mid: string, platform: Platform = "bilibili"): Promise<void> {
    return invoke("remove_linked_account", { id, mid, platform });
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

  fanHistory(streamerId: number, days = 30): Promise<FanHistory> {
    return invoke("get_fan_history", { streamerId, days });
  }

  refreshFans(streamerId: number): Promise<FanHistory> {
    return invoke("refresh_streamer_fans", { streamerId });
  }

  liveSessions(streamerId?: number, limit = 50): Promise<LiveSession[]> {
    return invoke("list_live_sessions", { streamerId, limit });
  }

  liveOnlineSamples(sessionId: number, afterId = 0, limit = 500): Promise<LiveOnlineSamplesPage> {
    return invoke("list_live_online_samples", { sessionId, afterId, limit });
  }

  liveSessionForSchedule(scheduleId: number): Promise<LiveSession | null> {
    return invoke("live_session_for_schedule", { scheduleId });
  }

  /** 「寻血猎犬」关闭时返回 enabled=false 的空快照，不会触网。 */
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

  async youtubeStatus(): Promise<YoutubeCredentialStatus> {
    return normalizeYoutubeStatus(await invoke<unknown>("youtube_status"));
  }

  async setYoutubeApiKey(apiKey: string): Promise<YoutubeCredentialStatus> {
    return normalizeYoutubeStatus(await invoke<unknown>("set_youtube_api_key", { apiKey }));
  }

  async clearYoutubeApiKey(): Promise<YoutubeCredentialStatus> {
    return normalizeYoutubeStatus(await invoke<unknown>("clear_youtube_api_key"));
  }

  async testYoutubeApiKey(): Promise<YoutubeCredentialStatus> {
    const status = await this.youtubeStatus();
    if (!status.apiKeyConfigured) {
      return { ...status, apiKeyValid: false, checkedAt: new Date().toISOString() };
    }
    await this.search("YouTube", "youtube");
    return {
      ...status,
      apiKeyValid: true,
      checkedAt: new Date().toISOString(),
      lastError: null,
    };
  }

  async startYoutubeOAuth(): Promise<YoutubeOAuthStart> {
    return normalizeYoutubeOAuthStart(await invoke<unknown>("start_youtube_oauth"));
  }

  async completeYoutubeOAuth(code: string, state: string): Promise<YoutubeCredentialStatus> {
    return normalizeYoutubeStatus(await invoke<unknown>("complete_youtube_oauth", { code, oauthState: state }));
  }

  async clearYoutubeOAuth(): Promise<YoutubeCredentialStatus> {
    return normalizeYoutubeStatus(await invoke<unknown>("clear_youtube_oauth"));
  }

  async startYoutubeWebLogin(): Promise<YoutubeWebLoginStart> {
    return normalizeYoutubeWebLoginStart(await invoke<unknown>("start_youtube_web_login"));
  }

  async saveYoutubeWebSession(cookie: string): Promise<YoutubeCredentialStatus> {
    return normalizeYoutubeStatus(await invoke<unknown>("set_youtube_web_session", { cookie }));
  }

  async clearYoutubeWebSession(): Promise<YoutubeCredentialStatus> {
    return normalizeYoutubeStatus(await invoke<unknown>("clear_youtube_web_session"));
  }

  serverToken(): Promise<string | null> {
    return invoke("get_server_token");
  }

  startRemoteMonitor(
    serverUrl: string,
    token: string,
  ): Promise<{ generation: number }> {
    return invoke("start_remote_monitor", { serverUrl, token });
  }

  stopRemoteMonitor(expectedGeneration: number): Promise<boolean> {
    return invoke("stop_remote_monitor", { expectedGeneration });
  }

  rebuildRemoteSync(
    serverUrl: string,
    token: string,
  ): Promise<{ cursorSeq: number; monitorGeneration: number }> {
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
    ![
      "live.bilibili.com",
      "space.bilibili.com",
      "t.bilibili.com",
      "www.bilibili.com",
      "laplace.live",
      "youtube.com",
      "www.youtube.com",
      "youtu.be",
      "accounts.google.com",
    ].includes(parsed.hostname)
  ) {
    throw new Error("只允许打开 B 站、YouTube、Google 授权页面或 LAPLACE 生理期页面");
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
