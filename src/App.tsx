import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  CSSProperties,
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
} from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  CalendarRange,
  Cloud,
  ExternalLink,
  HardDrive,
  LoaderCircle,
  Minus,
  RefreshCw,
  Square,
  X,
  Settings as SettingsIcon,
  Trash2,
  UserRound,
  UsersRound,
  Wifi,
  WifiOff,
} from "lucide-react";
import ScheduleView from "./components/ScheduleView";
import CreatorsView from "./components/CreatorsView";
import SettingsView, { type SettingsViewHandle } from "./components/SettingsView";
import { Avatar, ErrorBanner, Modal } from "./components/ui";
import BrandMark from "./components/BrandMark";
import {
  applyAppearance,
  readAppearanceSettings,
  saveAppearanceSettings,
  type AppearanceSettings,
} from "./lib/appearance";
import {
  ApiFailure,
  LocalApi,
  ServerApi,
  explainError,
  isTauri,
  openExternalUrl,
} from "./lib/api";
import { applyAutostart, readAutostart } from "./lib/autostart";
import { refreshAllJob, refreshCompletionProgress, refreshStreamerJob } from "./lib/refresh-job";
import {
  ensureNotificationPermission,
  installNotificationActionHandler,
  notifyLive,
} from "./lib/notifications";
import type { SecretInput } from "./lib/secret-types";
import { checkForUpdate } from "./lib/updater";
import {
  SYNC_PROTOCOL_VERSION,
  SyncProtocolEngine,
  fetchSyncDevices,
  idleSyncState,
  jobProgressFromMessage,
  loadSyncCursor,
  mergeServerSettings,
  registerSyncDevice,
  resolveHttpBase,
  resolveSyncDeviceId,
  serverSettingsForSave,
  syncChangesFromMessage,
  syncRecordValue,
} from "./lib/sync";
import type {
  DeviceInfo,
  JobProgress,
  SyncBootstrap,
  SyncChange,
  SyncUiState,
} from "./lib/sync";
import { isWebSyncCacheUsable, loadWebSyncCache } from "./lib/syncStorage";
import type {
  AccountSearchResult,
  ImportPreview,
  MigrationPreview,
  AppSettings,
  Bootstrap,
  FanHistory,
  FertilitySnapshot,
  LlmDiscoveryInput,
  LlmTestInput,
  LlmTestReport,
  LiveEventPayload,
  LiveStatus,
  NewScheduleInput,
  NewWeeklyScheduleRuleInput,
  Platform,
  ProxyTestReport,
  RefreshProgress,
  RefreshReport,
  Schedule,
  SecretStatus,
  ServerEvent,
  ServerSnapshot,
  ServerUpdateSettings,
  Streamer,
  SyncMutation,
  UpdateScheduleInput,
  UpdateSubscriptionInput,
  UpdateWeeklyScheduleRuleInput,
  WeeklyScheduleRule,
} from "./types";

type Page = "schedule" | "creators" | "settings";

const SIDEBAR_WIDTH_KEY = "oshical.sidebarWidth";
const WEB_DEVICE_PREFERENCES_KEY = "oshical.devicePreferences";
const SERVER_ONBOARDING_KEY_PREFIX = "oshical.serverOnboarding";
const SERVER_ONBOARDING_DEVICE_FALLBACK = "anonymous";
const DEFAULT_SIDEBAR_WIDTH = 244;
const MIN_SIDEBAR_WIDTH = 210;
const MAX_SIDEBAR_WIDTH = 420;

function compareStreamers(left: Streamer, right: Streamer): number {
  const order = (left.sortOrder ?? 0) - (right.sortOrder ?? 0);
  return order || left.name.localeCompare(right.name, "zh-CN");
}

function readSidebarWidth(): number {
  let raw: string | null = null;
  try {
    raw = window.localStorage.getItem(SIDEBAR_WIDTH_KEY);
  } catch {
    return DEFAULT_SIDEBAR_WIDTH;
  }
  const parsed = raw ? Number(raw) : DEFAULT_SIDEBAR_WIDTH;
  return Number.isFinite(parsed)
    ? Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, parsed))
    : DEFAULT_SIDEBAR_WIDTH;
}

function readWebDevicePreferences(): AppSettings {
  try {
    const raw = window.localStorage.getItem(WEB_DEVICE_PREFERENCES_KEY);
    if (!raw) {
      return DEFAULT_SETTINGS;
    }
    const parsed = JSON.parse(raw) as Partial<AppSettings>;
    return {
      ...DEFAULT_SETTINGS,
      notificationsEnabled:
        typeof parsed.notificationsEnabled === "boolean"
          ? parsed.notificationsEnabled
          : DEFAULT_SETTINGS.notificationsEnabled,
    };
  } catch {
    return DEFAULT_SETTINGS;
  }
}

function saveWebDevicePreferences(settings: AppSettings): void {
  try {
    window.localStorage.setItem(
      WEB_DEVICE_PREFERENCES_KEY,
      JSON.stringify({ notificationsEnabled: settings.notificationsEnabled }),
    );
  } catch {
    // Device preferences can remain in memory when storage is unavailable.
  }
}


function normalizeServerIdentity(serverUrl: string): string {
  const trimmed = serverUrl.trim();
  if (!trimmed) {
    return "";
  }
  try {
    const url = new URL(trimmed);
    url.hash = "";
    url.search = "";
    const path = url.pathname.replace(/\/+$/, "");
    return `${url.origin}${path === "/" ? "" : path}`;
  } catch {
    return trimmed.replace(/\/+$/, "");
  }
}

function serverOnboardingKey(serverUrl: string, deviceId: string): string {
  const identity = normalizeServerIdentity(serverUrl) || "unknown-server";
  const device = deviceId.trim() || SERVER_ONBOARDING_DEVICE_FALLBACK;
  return `${SERVER_ONBOARDING_KEY_PREFIX}:${encodeURIComponent(identity)}:${encodeURIComponent(device)}`;
}

function hasCompletedServerOnboarding(serverUrl: string, deviceId: string): boolean {
  try {
    return window.localStorage.getItem(serverOnboardingKey(serverUrl, deviceId)) === "1";
  } catch {
    return false;
  }
}

function markServerOnboardingComplete(serverUrl: string, deviceId: string): void {
  try {
    window.localStorage.setItem(serverOnboardingKey(serverUrl, deviceId), "1");
  } catch {
    // The setup prompt can reappear when browser storage is unavailable.
  }
}

const DEFAULT_SETTINGS: AppSettings = {
  dataMode: "local",
  serverUrl: "",
  livePollSeconds: 20,
  dynamicPollSeconds: 300,
  autoAnalyzeImages: true,
  notificationsEnabled: true,
  liveStatsEnabled: false,
  fertilityEnabled: false,
  youtubeEnabled: false,
  llmBaseUrl: "https://api.openai.com/v1",
  llmModel: "",
  llmPromptVersion: "schedule-v1",
  llmReasoningEffort: "auto",
  llmParallelism: 1,
  proxyPort: null,
};

function maskLlmApiKey(apiKey: string): string {
  const value = apiKey.trim();
  return value ? "••••••••" + value.slice(-4) : "";
}

function isRemoteUncertain(error: unknown): error is ApiFailure {
  return error instanceof ApiFailure
    && (error.code === "REMOTE_OFFLINE" || error.code === "REQUEST_TIMEOUT");
}

const EMPTY_SECRETS: SecretStatus = {
  llmConfigured: false,
  biliConfigured: false,
  serverTokenConfigured: false,
};

const localApi = isTauri ? new LocalApi() : null;
const desktopWindow = isTauri ? getCurrentWindow() : null;
const initialAppearance = readAppearanceSettings();
applyAppearance(initialAppearance);

export default function App() {
  const [page, setPage] = useState<Page>("schedule");
  const settingsViewRef = useRef<SettingsViewHandle>(null);
  const [scheduleRevision, setScheduleRevision] = useState(0);
  const [ready, setReady] = useState(false);
  const [settings, setSettings] = useState<AppSettings>(DEFAULT_SETTINGS);
  const [appearance, setAppearance] = useState<AppearanceSettings>(initialAppearance);
  const [subscriptions, setSubscriptions] = useState<Streamer[]>([]);
  const [liveStatuses, setLiveStatuses] = useState<LiveStatus[]>([]);
  const [secrets, setSecrets] = useState<SecretStatus>(EMPTY_SECRETS);
  const [serverApi, setServerApi] = useState<ServerApi | null>(null);
  const [serverToken, setServerToken] = useState("");
  const [connection, setConnection] = useState<"local" | "connecting" | "connected" | "offline">(
    isTauri ? "local" : "offline",
  );
  const [connectionError, setConnectionError] = useState<string | null>(null);
  const [syncState, setSyncState] = useState<SyncUiState>(() => idleSyncState());
  const [syncDevices, setSyncDevices] = useState<DeviceInfo[]>([]);
  const [syncDevicesLoading, setSyncDevicesLoading] = useState(false);
  const [syncDevicesError, setSyncDevicesError] = useState<string | null>(null);
  const [serverBiliConfigured, setServerBiliConfigured] = useState(false);
  const [activeJobs, setActiveJobs] = useState<Record<string, JobProgress>>({});
  const [toast, setToast] = useState<string | null>(null);
  const [refreshProgress, setRefreshProgress] = useState<Record<number, RefreshProgress>>({});
  const [autostart, setAutostart] = useState(false);
  const [pendingImport, setPendingImport] = useState<{
    snapshot: ServerSnapshot;
    preview: ImportPreview;
    api: ServerApi;
  } | null>(null);
  const [pendingServerSetup, setPendingServerSetup] = useState<{
    api: ServerApi;
    token: string;
    deviceId: string;
  } | null>(null);
  const [pendingClientMigration, setPendingClientMigration] = useState<{
    snapshot: ServerSnapshot;
    preview: MigrationPreview;
    api: ServerApi;
    deviceId: string;
    migrationId: string;
  } | null>(null);
  const [migrationBusy, setMigrationBusy] = useState(false);
  const [importBusy, setImportBusy] = useState(false);
  const [authGateError, setAuthGateError] = useState<string | null>(null);
  const [llmApiKeyHint, setLlmApiKeyHint] = useState("");
  const [sidebarWidth, setSidebarWidth] = useState(readSidebarWidth);
  const [sidebarResizing, setSidebarResizing] = useState(false);
  const [creatorMenu, setCreatorMenu] = useState<{ streamer: Streamer; x: number; y: number } | null>(null);
  const [sidebarRefreshingId, setSidebarRefreshingId] = useState<number | null>(null);
  const settingsRef = useRef(settings);
  const connectionRef = useRef(connection);
  const syncStateRef = useRef(syncState);
  const syncEngineRef = useRef<SyncProtocolEngine | null>(null);
  const serverApiRef = useRef<ServerApi | null>(serverApi);
  const serverTokenRef = useRef(serverToken);
  const connectionEpochRef = useRef(0);
  const activeDataRequestSeq = useRef(0);
  const syncDevicesRequestSeq = useRef(0);
  const settingsRequestSeq = useRef(0);
  const llmSecretRequestSeq = useRef(0);
  const migrationBusyRef = useRef(false);
  const importBusyRef = useRef(false);
  const importPreviewBusyRef = useRef(false);
  const resyncInFlightRef = useRef(false);
  const mutationResyncInFlightRef = useRef(false);
  const remoteMonitorGenerationRef = useRef<number | null>(null);
  const remoteMonitorTransitionRef = useRef<Promise<void>>(Promise.resolve());
  const jobClearTimers = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const legacyRefreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const seenLiveSeq = useRef(0);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const refreshClearTimers = useRef(new Map<number, ReturnType<typeof setTimeout>>());
  const sidebarResizeOrigin = useRef<{ startX: number; startWidth: number } | null>(null);
  const orderedSubscriptions = useMemo(
    () => [...subscriptions].sort(compareStreamers),
    [subscriptions],
  );
  const visibleSubscriptions = useMemo(
    () => settings.youtubeEnabled
      ? orderedSubscriptions
      : orderedSubscriptions.filter((streamer) => streamer.platform !== "youtube"),
    [orderedSubscriptions, settings.youtubeEnabled],
  );
  const hiddenStreamerIds = useMemo(
    () => settings.youtubeEnabled
      ? new Set<number>()
      : new Set(
        subscriptions
          .filter((streamer) => streamer.platform === "youtube")
          .map((streamer) => streamer.id),
      ),
    [settings.youtubeEnabled, subscriptions],
  );
  const visibleLiveStatuses = useMemo(
    () => settings.youtubeEnabled
      ? liveStatuses
      : liveStatuses.filter((status) => (
        status.platform !== "youtube" && !hiddenStreamerIds.has(status.streamerId)
      )),
    [hiddenStreamerIds, liveStatuses, settings.youtubeEnabled],
  );

  // Remote monitor commands have backend-side generations. Serializing them in
  // the UI preserves the order implied by connectionEpochRef, so a stale
  // connect cannot start after a newer one and replace its monitor.
  const enqueueRemoteMonitorTransition = useCallback(
    (operation: () => Promise<void>): Promise<void> => {
      const next = remoteMonitorTransitionRef.current
        .catch(() => undefined)
        .then(operation);
      remoteMonitorTransitionRef.current = next;
      return next;
    },
    [],
  );

  const startRemoteMonitorForEpoch = useCallback(
    async (epoch: number, serverUrl: string, token: string): Promise<void> => {
      if (!localApi) {
        throw new Error("桌面 API 未初始化");
      }
      await enqueueRemoteMonitorTransition(async () => {
        if (epoch !== connectionEpochRef.current) {
          return;
        }
        const session = await localApi.startRemoteMonitor(serverUrl, token);
        remoteMonitorGenerationRef.current = session.generation;
        if (epoch !== connectionEpochRef.current) {
          await localApi.stopRemoteMonitor(session.generation).catch(() => false);
          if (remoteMonitorGenerationRef.current === session.generation) {
            // The backend only stops this exact generation. A false result means
            // a newer connection already owns the monitor.
            remoteMonitorGenerationRef.current = null;
          }
        }
      });
    },
    [enqueueRemoteMonitorTransition],
  );

  const stopRemoteMonitorGeneration = useCallback(
    async (generation: number | null): Promise<void> => {
      if (!localApi || generation === null) {
        return;
      }
      await enqueueRemoteMonitorTransition(async () => {
        await localApi.stopRemoteMonitor(generation).catch(() => false);
        if (remoteMonitorGenerationRef.current === generation) {
          remoteMonitorGenerationRef.current = null;
        }
      });
    },
    [enqueueRemoteMonitorTransition],
  );

  useEffect(() => {
    settingsRef.current = settings;
  }, [settings]);

  useEffect(() => {
    applyAppearance(appearance);
    saveAppearanceSettings(appearance);
  }, [appearance]);

  useEffect(() => {
    connectionRef.current = connection;
  }, [connection]);

  useEffect(() => {
    syncStateRef.current = syncState;
  }, [syncState]);

  useEffect(() => {
    serverApiRef.current = serverApi;
  }, [serverApi]);

  useEffect(() => {
    serverTokenRef.current = serverToken;
  }, [serverToken]);

  useEffect(() => {
    try {
      window.localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarWidth));
    } catch {
      // Resizing remains available even when storage is unavailable.
    }
  }, [sidebarWidth]);

  useEffect(() => {
    if (!creatorMenu) {
      return;
    }
    const close = () => setCreatorMenu(null);
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        close();
      }
    };
    window.addEventListener("click", close);
    window.addEventListener("scroll", close, true);
    window.addEventListener("resize", close);
    window.addEventListener("keydown", closeOnEscape);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("scroll", close, true);
      window.removeEventListener("resize", close);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, [creatorMenu]);

  useEffect(() => {
    if (!settings.youtubeEnabled && creatorMenu?.streamer.platform === "youtube") {
      setCreatorMenu(null);
    }
  }, [creatorMenu, settings.youtubeEnabled]);

  const showToast = useCallback((message: string) => {
    setToast(message);
    if (toastTimer.current) {
      clearTimeout(toastTimer.current);
    }
    toastTimer.current = setTimeout(() => setToast(null), 5500);
  }, []);

  const requireYoutubeEnabled = useCallback(() => {
    if (!settings.youtubeEnabled) {
      throw new Error("YouTube 功能未开启");
    }
  }, [settings.youtubeEnabled]);

  useEffect(() => {
    if (!isTauri) {
      return;
    }
    let disposed = false;
    void checkForUpdate()
      .then((update) => {
        if (!disposed && update) {
          showToast(`发现新版本 v${update.version}，可在“设置 → 软件更新”中安装`);
        }
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
    };
  }, [showToast]);

  useEffect(() => () => {
    if (toastTimer.current) {
      clearTimeout(toastTimer.current);
    }
  }, []);

  const applyRefreshProgress = useCallback((progress: RefreshProgress) => {
    setRefreshProgress((current) => ({ ...current, [progress.streamerId]: progress }));
    const previous = refreshClearTimers.current.get(progress.streamerId);
    if (previous) {
      clearTimeout(previous);
    }
    if (progress.done || progress.failed) {
      const timer = setTimeout(() => {
        setRefreshProgress((current) => {
          const next = { ...current };
          delete next[progress.streamerId];
          return next;
        });
        refreshClearTimers.current.delete(progress.streamerId);
      }, progress.failed ? 5000 : 1800);
      refreshClearTimers.current.set(progress.streamerId, timer);
    }
  }, []);

  useEffect(() => () => {
    for (const timer of refreshClearTimers.current.values()) {
      clearTimeout(timer);
    }
    for (const timer of jobClearTimers.current.values()) {
      clearTimeout(timer);
    }
    if (legacyRefreshTimer.current) {
      clearTimeout(legacyRefreshTimer.current);
    }
  }, []);

  const refreshActiveData = useCallback(async () => {
    const requestId = ++activeDataRequestSeq.current;
    const api = serverApiRef.current;
    if (!isTauri || settingsRef.current.dataMode === "server") {
      if (!api) {
        return;
      }
      const [nextSubscriptions, nextLive] = await Promise.all([
        api.subscriptions(),
        api.liveStatuses(),
      ]);
      if (
        requestId !== activeDataRequestSeq.current ||
        serverApiRef.current !== api
      ) {
        return;
      }
      setSubscriptions(nextSubscriptions);
      setLiveStatuses(nextLive);
      return;
    }
    if (localApi) {
      const epoch = connectionEpochRef.current;
      const boot = await localApi.bootstrap();
      if (
        requestId !== activeDataRequestSeq.current ||
        epoch !== connectionEpochRef.current ||
        settingsRef.current.dataMode !== "local"
      ) {
        return;
      }
      setSubscriptions(boot.subscriptions);
      setLiveStatuses(boot.liveStatuses);
      setSecrets(boot.secrets);
    }
  }, []);

  const applyJobProgress = useCallback((progress: JobProgress) => {
    setActiveJobs((current) => ({ ...current, [progress.jobId]: progress }));
    const previous = jobClearTimers.current.get(progress.jobId);
    if (previous) {
      clearTimeout(previous);
    }
    if (progress.done || progress.failed) {
      const timer = setTimeout(() => {
        setActiveJobs((current) => {
          const next = { ...current };
          delete next[progress.jobId];
          return next;
        });
        jobClearTimers.current.delete(progress.jobId);
      }, progress.failed ? 5000 : 3500);
      jobClearTimers.current.set(progress.jobId, timer);
    }
    if (typeof progress.streamerId === "number") {
      applyRefreshProgress({
        streamerId: progress.streamerId,
        streamerName: progress.streamerName ?? `主播 ${progress.streamerId}`,
        stage: progress.stage,
        completed: progress.completed,
        total: progress.total,
        percent: progress.percent,
        done: progress.done,
        failed: progress.failed,
      });
    }
  }, [applyRefreshProgress]);

  const refreshSyncDevices = useCallback(async () => {
    const api = serverApiRef.current;
    if (!api) {
      setSyncDevices([]);
      setSyncDevicesError(null);
      return;
    }
    const requestId = ++syncDevicesRequestSeq.current;
    setSyncDevicesLoading(true);
    setSyncDevicesError(null);
    try {
      const baseUrl = resolveHttpBase(api);
      const devices = isTauri && localApi
        ? await localApi.listSyncDevices(baseUrl, api.eventToken())
        : await fetchSyncDevices(baseUrl, api.eventToken());
      if (requestId !== syncDevicesRequestSeq.current || serverApiRef.current !== api) {
        return;
      }
      setSyncDevices(devices);
    } catch (error) {
      if (requestId !== syncDevicesRequestSeq.current || serverApiRef.current !== api) {
        return;
      }
      setSyncDevices([]);
      setSyncDevicesError(`设备列表暂不可用：${explainError(error)}`);
    } finally {
      if (requestId === syncDevicesRequestSeq.current) {
        setSyncDevicesLoading(false);
      }
    }
  }, []);

  const deleteSyncDeviceLog = useCallback(async (device: DeviceInfo) => {
    if (!serverApi) {
      throw new Error("服务器未连接");
    }
    if (device.online) {
      throw new Error("只能删除离线设备的日志");
    }
    if (device.deviceId === syncStateRef.current.deviceId) {
      throw new Error("不能删除当前设备的日志");
    }
    await serverApi.deleteDeviceLog(device.deviceId);
    await refreshSyncDevices();
    showToast(`已删除 ${device.name} 的离线日志`);
  }, [refreshSyncDevices, serverApi, showToast]);

  const syncServerLlmSecret = useCallback(async (api: ServerApi): Promise<void> => {
    const requestId = ++llmSecretRequestSeq.current;
    try {
      const secret = await api.llmSecret();
      if (requestId !== llmSecretRequestSeq.current || serverApiRef.current !== api) {
        return;
      }
      if (secret.apiKey) {
        setLlmApiKeyHint(maskLlmApiKey(secret.apiKey));
      } else if (!secret.configured) {
        setLlmApiKeyHint("");
      }
      if (isTauri && localApi && secret.apiKey) {
        const status = await localApi.saveSecrets({ llmApiKey: secret.apiKey });
        if (requestId === llmSecretRequestSeq.current && serverApiRef.current === api) {
          setSecrets(status);
        }
      } else {
        setSecrets((current) => ({
          ...current,
          llmConfigured: secret.configured,
        }));
      }
    } catch (error) {
      if (error instanceof ApiFailure && error.status === 404) {
        return;
      }
      throw error;
    }
  }, []);

  const refreshServerSettings = useCallback(
    async (api: ServerApi, base: AppSettings = settingsRef.current): Promise<AppSettings> => {
      const requestId = ++settingsRequestSeq.current;
      const serverSettings = await api.settings();
      if (requestId !== settingsRequestSeq.current || serverApiRef.current !== api) {
        return base;
      }
      const merged = mergeServerSettings(base, serverSettings);
      settingsRef.current = merged;
      setSettings(merged);
      if (isTauri && localApi) {
        await localApi.saveSettings(merged).catch(() => undefined);
        // A server push or a newer settings request can land while the local
        // write is in flight. Persist the newest committed value afterwards so
        // the on-disk desktop setting cannot remain on the stale response.
        if (requestId !== settingsRequestSeq.current || serverApiRef.current !== api) {
          await localApi.saveSettings(settingsRef.current).catch(() => undefined);
        }
      }
      return merged;
    },
    [],
  );

  const reloadLocalSettings = useCallback(async (): Promise<void> => {
    if (!localApi) {
      return;
    }
    const requestId = ++settingsRequestSeq.current;
    const next = await localApi.settings();
    if (requestId !== settingsRequestSeq.current) {
      return;
    }
    settingsRef.current = next;
    setSettings(next);
  }, []);

  const applyFormalChanges = useCallback(async (changes: SyncChange[]) => {
    let scheduleChanged = false;
    let refreshSubscriptions = false;
    let serverSecretChanged = false;
    const subscriptionChanges = changes.filter((change) => change.entityType === "subscription");
    const liveStatusChanges = changes.filter((change) => change.entityType === "live_status");
    const subscriptionOps: Array<{ change: SyncChange; streamer: Streamer | null }> = [];
    for (const change of subscriptionChanges) {
      if (change.operation === "delete") {
        subscriptionOps.push({ change, streamer: null });
        continue;
      }
      if (!syncRecordValue(change.payload)) {
        refreshSubscriptions = true;
        continue;
      }
      const streamer = change.payload as unknown as Streamer;
      if (!Number.isFinite(streamer.id)) {
        refreshSubscriptions = true;
        continue;
      }
      subscriptionOps.push({ change, streamer });
    }
    const liveStatusOps: Array<{ change: SyncChange; status: LiveStatus | null }> = [];
    for (const change of liveStatusChanges) {
      if (change.operation === "delete") {
        liveStatusOps.push({ change, status: null });
        continue;
      }
      if (!syncRecordValue(change.payload)) {
        continue;
      }
      const status = change.payload as unknown as LiveStatus;
      if (Number.isFinite(status.streamerId)) {
        liveStatusOps.push({ change, status });
      }
    }

    if (subscriptionOps.length) {
      setSubscriptions((current) => {
        let next = current;
        for (const { change, streamer } of subscriptionOps) {
          if (change.operation === "delete") {
            next = next.filter((streamer) => String(streamer.id) !== change.entityId);
            continue;
          }
          if (!streamer) {
            continue;
          }
          const index = next.findIndex((item) => item.id === streamer.id);
          next = index >= 0
            ? next.map((item, itemIndex) => (itemIndex === index ? streamer : item))
            : [...next, streamer];
        }
        return next;
      });
    }

    if (liveStatusOps.length) {
      setLiveStatuses((current) => {
        let next = current;
        for (const { change, status } of liveStatusOps) {
          if (change.operation === "delete") {
            next = next.filter((status) => String(status.streamerId) !== change.entityId);
            continue;
          }
          if (!status) {
            continue;
          }
          const index = next.findIndex((item) => item.streamerId === status.streamerId);
          next = index >= 0
            ? next.map((item, itemIndex) => (itemIndex === index ? status : item))
            : [...next, status];
        }
        return next;
      });
      const liveById = new Map<number, LiveStatus>();
      for (const { status } of liveStatusOps) {
        if (status) {
          liveById.set(status.streamerId, status);
        }
      }
      if (liveById.size) {
        setSubscriptions((currentSubscriptions) => currentSubscriptions.map((streamer) => {
          const status = liveById.get(streamer.id);
          return status
            ? {
                ...streamer,
                isLive: status.isLive,
                liveTitle: status.title || null,
                liveCover: status.coverUrl,
                liveStartedAt: status.startedAt,
                liveCheckedAt: status.checkedAt,
              }
            : streamer;
        }));
      }
    }

    for (const change of changes) {
      if (change.entityType === "server_settings") {
        settingsRequestSeq.current += 1;
        setSettings((current) => {
          const next = mergeServerSettings(current, change.payload);
          settingsRef.current = next;
          return next;
        });
      } else if (change.entityType === "server_secret") {
        serverSecretChanged = true;
      } else if (change.entityType === "link" || change.entityType === "linked_account") {
        refreshSubscriptions = true;
      } else if (
        change.entityType === "schedule" ||
        change.entityType === "weekly_rule" ||
        change.entityType === "weekly_exception" ||
        change.entityType === "schedule_source" ||
        change.entityType === "source_suppression"
      ) {
        scheduleChanged = true;
      }
    }

    if (scheduleChanged) {
      setScheduleRevision((value) => value + 1);
    }
    if (refreshSubscriptions) {
      await refreshActiveData();
    }
    const currentApi = serverApiRef.current;
    if (serverSecretChanged && currentApi) {
      await syncServerLlmSecret(currentApi);
    }
  }, [refreshActiveData, syncServerLlmSecret]);

  const applySyncSnapshot = useCallback(async (snapshot: SyncBootstrap) => {
    setSubscriptions(snapshot.subscriptions);
    setLiveStatuses(snapshot.liveStatuses);
    settingsRequestSeq.current += 1;
    setSettings((current) => {
      const next = mergeServerSettings(current, snapshot.serverSettings);
      settingsRef.current = next;
      return next;
    });
    setScheduleRevision((value) => value + 1);
  }, []);

  const handleServerEvent = useCallback(async (
    payload: ServerEvent,
    engineOverride?: SyncProtocolEngine | null,
  ) => {
    const message = payload as unknown as Record<string, unknown>;
    const engine = engineOverride === undefined ? syncEngineRef.current : engineOverride;
    if (engine && await engine.handleMessage(message)) {
      return;
    }
    if (message.type === "auth_ok") {
      connectionRef.current = "connected";
      setConnection("connected");
      setConnectionError(null);
      return;
    }
    if (message.type === "job_progress") {
      applyJobProgress(jobProgressFromMessage(message));
      return;
    }
    if (message.type === "data_imported") {
      if (engine && syncStateRef.current.protocolVersion === SYNC_PROTOCOL_VERSION) {
        await engine.forceResync("data_imported");
      } else {
        await refreshActiveData();
        setScheduleRevision((value) => value + 1);
      }
      return;
    }
    if (message.type === "sync_batch") {
      const changes = syncChangesFromMessage(message);
      if (changes?.length) {
        await applyFormalChanges(changes);
        const lastSeq = changes[changes.length - 1].seq;
        setSyncState((current) => ({
          ...current,
          cursorSeq: lastSeq,
          latestSeq: Math.max(current.latestSeq, lastSeq),
          lastSyncAt: new Date().toISOString(),
          status: "connected",
        }));
      }
      return;
    }
    if (message.type === "reset_required") {
      setSyncState((current) => ({
        ...current,
        status: "reset_required",
        lastError: "同步游标已过期，需要重新载入服务端快照",
        latestSeq: typeof message.latestSeq === "number" ? message.latestSeq : current.latestSeq,
        minRetainedSeq:
          typeof message.minRetainedSeq === "number"
            ? message.minRetainedSeq
            : current.minRetainedSeq,
      }));
      return;
    }

    if (typeof payload.seq === "number") {
      if (payload.seq <= seenLiveSeq.current) {
        return;
      }
      seenLiveSeq.current = payload.seq;
    }
    const refreshLegacyData = () => {
      if (legacyRefreshTimer.current) {
        clearTimeout(legacyRefreshTimer.current);
      }
      legacyRefreshTimer.current = setTimeout(() => {
        legacyRefreshTimer.current = null;
        void refreshActiveData().catch(() => undefined);
      }, 120);
    };
    if (payload.type === "live_status") {
      const live = payload.payload as unknown as LiveEventPayload;
      const currentSettings = settingsRef.current;
      if (
        !payload.replayed &&
        currentSettings.notificationsEnabled &&
        live.isLive &&
        !live.previousIsLive &&
        live.liveNotifyEnabled
      ) {
        notifyLive(live);
      }
      if (syncStateRef.current.protocolVersion !== SYNC_PROTOCOL_VERSION) {
        refreshLegacyData();
      }
    } else if (payload.type === "schedule_changed") {
      setScheduleRevision((value) => value + 1);
      if (syncStateRef.current.protocolVersion !== SYNC_PROTOCOL_VERSION) {
        refreshLegacyData();
      }
    } else if (payload.type === "weekly_rule_changed") {
      setScheduleRevision((value) => value + 1);
      if (syncStateRef.current.protocolVersion !== SYNC_PROTOCOL_VERSION) {
        refreshLegacyData();
      }
    } else if (payload.type === "refresh_progress") {
      applyRefreshProgress(payload.payload as RefreshProgress);
    } else if (payload.type === "live_status_snapshot") {
      if (syncStateRef.current.protocolVersion !== SYNC_PROTOCOL_VERSION) {
        refreshLegacyData();
      }
    } else if (payload.type === "resync_required") {
      if (syncStateRef.current.protocolVersion === 2 && engine) {
        await engine.forceResync("server_requested");
      } else {
        refreshLegacyData();
      }
    }
  }, [applyFormalChanges, applyJobProgress, applyRefreshProgress, refreshActiveData]);

  const connectServer = useCallback(
    async (url: string, token: string, keepSettings = true): Promise<ServerApi> => {
      const epoch = ++connectionEpochRef.current;
      settingsRequestSeq.current += 1;
      llmSecretRequestSeq.current += 1;
      const previousApi = serverApiRef.current;
      const previousSyncState = syncStateRef.current;
      const previousToken = serverTokenRef.current;
      const previousConnection = connectionRef.current;
      connectionRef.current = "connecting";
      setConnection("connecting");
      setConnectionError(null);
      setServerBiliConfigured(false);
      setLlmApiKeyHint("");
      const api = new ServerApi({ baseUrl: url, token });
      try {
        const [nextSubscriptions, nextLive, biliStatus] = await Promise.all([
          api.subscriptions(),
          api.liveStatuses(),
          api.serverBiliStatus().catch(() => null),
        ]);
        if (epoch !== connectionEpochRef.current) {
          throw new Error("连接请求已被更新的操作替代");
        }
        const serverSettings = keepSettings ? null : await api.settings();
        if (epoch !== connectionEpochRef.current) {
          throw new Error("连接请求已被更新的操作替代");
        }
        if (isTauri && localApi) {
          await startRemoteMonitorForEpoch(epoch, url, token);
          if (epoch !== connectionEpochRef.current) {
            throw new Error("连接请求已被更新的操作替代");
          }
        }

        serverApiRef.current = api;
        serverTokenRef.current = token;
        setSubscriptions(nextSubscriptions);
        setLiveStatuses(nextLive);
        setServerApi(api);
        setServerToken(token);
        setServerBiliConfigured(biliStatus?.configured === true);
        connectionRef.current = "connected";
        setConnection("connected");
        setSyncDevices([]);
        setSyncDevicesError(null);
        seenLiveSeq.current = 0;
        setSyncState((current) => ({ ...idleSyncState(current.deviceId), status: "connecting" }));
        if (serverSettings) {
          settingsRequestSeq.current += 1;
          const merged = mergeServerSettings(
            { ...settingsRef.current, dataMode: "server" as const },
            serverSettings,
          );
          settingsRef.current = merged;
          setSettings(merged);
        }
        void syncServerLlmSecret(api).catch((error) => showToast(explainError(error)));
        return api;
      } catch (error) {
        if (epoch === connectionEpochRef.current) {
          serverApiRef.current = previousApi;
          serverTokenRef.current = previousToken;
          setServerApi(previousApi);
          setServerToken(previousToken);
          setSyncState(previousSyncState);
          connectionRef.current = previousConnection;
          setConnection(previousConnection);
          setConnectionError(explainError(error));
          if (previousApi && isTauri && localApi && previousToken) {
            void startRemoteMonitorForEpoch(
              epoch,
              resolveHttpBase(previousApi),
              previousToken,
            ).catch(() => undefined);
          }
        }
        throw error;
      }
    },
    [showToast, startRemoteMonitorForEpoch, syncServerLlmSecret],
  );

  useEffect(() => {
    let cancelled = false;
    async function initialize() {
      try {
        if (!isTauri) {
          const nextSettings = readWebDevicePreferences();
          settingsRef.current = nextSettings;
          setSettings(nextSettings);
          setReady(true);
          return;
        }
        if (!localApi) {
          throw new Error("Tauri API 未初始化");
        }
        const boot: Bootstrap = await localApi.bootstrap();
        const token = (await localApi.serverToken()) ?? "";
        const auto = await readAutostart();
        if (cancelled) {
          return;
        }
        settingsRef.current = boot.settings;
        setSettings(boot.settings);
        setSubscriptions(boot.subscriptions);
        setLiveStatuses(boot.liveStatuses);
        setSecrets(boot.secrets);
        serverTokenRef.current = token;
        setServerToken(token);
        setAutostart(auto);
        connectionRef.current = boot.settings.dataMode === "local" ? "local" : "offline";
        setConnection(boot.settings.dataMode === "local" ? "local" : "offline");
        if (boot.settings.dataMode === "server" && boot.settings.serverUrl && token) {
          try {
            const api = await connectServer(boot.settings.serverUrl, token);
            await refreshServerSettings(api, boot.settings);
            const deviceId = syncStateRef.current.deviceId || (await localApi.deviceId());
            if (!hasCompletedServerOnboarding(resolveHttpBase(api), deviceId)) {
              setPendingServerSetup({ api, token, deviceId });
            }
          } catch (error) {
            showToast(explainError(error));
          }
        }
        setReady(true);
      } catch (error) {
        if (!cancelled) {
          showToast(explainError(error));
          setReady(true);
        }
      }
    }
    void initialize();
    return () => {
      cancelled = true;
    };
  }, [connectServer, refreshServerSettings, showToast]);
  useEffect(() => {
    if (!isTauri) {
      return;
    }
    let disposed = false;
    const cleanups: Array<() => void> = [];
    const keepCleanup = (cleanup: () => void) => {
      if (disposed) { cleanup(); } else { cleanups.push(cleanup); }
    };
    const scheduleCacheRefresh = () => {
      if (legacyRefreshTimer.current) {
        clearTimeout(legacyRefreshTimer.current);
      }
      legacyRefreshTimer.current = setTimeout(() => {
        legacyRefreshTimer.current = null;
        if (disposed) {
          return;
        }
        setScheduleRevision((value) => value + 1);
        void refreshActiveData().catch((error) => {
          if (!disposed) {
            setConnectionError(explainError(error));
          }
        });
      }, 120);
    };

    async function install() {
      try {
        const granted = await ensureNotificationPermission();
        if (!granted) {
          showToast("Windows 通知权限未授予，开播提醒不可用");
        }
        keepCleanup(await installNotificationActionHandler());
        const offEvent = await listen<ServerEvent>("server-event", (event) => {
          void handleServerEvent(event.payload).catch((error) => {
            const message = explainError(error);
            setConnectionError(message);
            showToast(message);
          });
        });
        keepCleanup(offEvent);
        const offProgress = await listen<RefreshProgress>("refresh-progress", (event) => {
          applyRefreshProgress(event.payload);
        });
        keepCleanup(offProgress);
        const offStatus = await listen<{
          connected?: boolean;
          connecting?: boolean;
          error?: string;
          protocolVersion?: number;
          deviceId?: string;
          cursorSeq?: number;
          lastSeq?: number;
          latestSeq?: number;
          syncing?: boolean;
        }>("remote-status", (event) => {
          if (event.payload.error) {
            connectionRef.current = "offline";
            setConnection("offline");
            setConnectionError(event.payload.error);
            syncEngineRef.current?.markOffline();

            return;
          }
          if (event.payload.connected) {
            connectionRef.current = "connected";
            setConnection("connected");
            setConnectionError(null);
            syncEngineRef.current?.acceptRemoteStatus(
              event.payload as unknown as Record<string, unknown>,
            );
            // The event itself is not durable. Re-read the local mirror after
            // every connection/apply-status transition so a missed UI event
            // cannot leave React showing a stale cursor view.
            scheduleCacheRefresh();
          } else if (event.payload.connecting) {
            connectionRef.current = "connecting";
            setConnection("connecting");
            syncEngineRef.current?.markConnecting();

          }
        });
        keepCleanup(offStatus);
        const offSyncStatus = await listen<Record<string, unknown>>("sync-status", (event) => {
          syncEngineRef.current?.acceptRemoteStatus(event.payload);
        });
        keepCleanup(offSyncStatus);
        const offSyncApplied = await listen<Record<string, unknown>>("sync-applied", (event) => {
          syncEngineRef.current?.acceptAppliedProgress(event.payload);
          const appliedChanges = Array.isArray(event.payload.changes)
            ? (event.payload.changes as SyncChange[])
            : null;
          if (appliedChanges?.length) {
            void applyFormalChanges(appliedChanges).catch((error) => {
              showToast(explainError(error));
            });
          } else {
            scheduleCacheRefresh();
          }
        });
        keepCleanup(offSyncApplied);
        const offBootstrap = await listen<Record<string, unknown>>("sync-bootstrap-applied", (event) => {
          syncEngineRef.current?.acceptAppliedProgress(event.payload);
          setScheduleRevision((value) => value + 1);
          void (async () => {
            await reloadLocalSettings();
            await refreshActiveData();
          })().catch((error) => showToast(explainError(error)));
        });
        keepCleanup(offBootstrap);
        const offJobProgress = await listen<Record<string, unknown>>("job-progress", (event) => {
          applyJobProgress(jobProgressFromMessage(event.payload));
        });
        keepCleanup(offJobProgress);
        const offError = await listen<string>("monitor-error", (event) => {
          showToast(event.payload);
        });
        keepCleanup(offError);
        const offFanCount = await listen<unknown>("fan-count-updated", () => {
          void refreshActiveData().catch((error) => showToast(explainError(error)));
        });
        keepCleanup(offFanCount);
        const offBiliCookie = await listen<SecretStatus>("bili-cookie-updated", (event) => {
          setSecrets(event.payload);
          showToast("B 站 Cookie 已自动保存");
        });
        keepCleanup(offBiliCookie);
        const offBiliLoginError = await listen<string>("bili-cookie-login-error", (event) => {
          showToast(event.payload);
        });
        keepCleanup(offBiliLoginError);

      } catch (error) {
        if (!disposed) {
          showToast(explainError(error));
        }
      }
    }
    void install();
    return () => {
      disposed = true;
      for (const cleanup of cleanups) {
        cleanup();
      }
    };
  }, [applyFormalChanges, applyJobProgress, applyRefreshProgress, handleServerEvent, refreshActiveData, reloadLocalSettings, showToast]);

  useEffect(() => {
    if (!serverApi || pendingImport || pendingServerSetup || pendingClientMigration) {
      return;
    }
    const api = serverApi;
    let disposed = false;
    let socket: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let attempts = 0;
    let deliberateReconnect = false;
    let engine: SyncProtocolEngine | null = null;


    function connectSocket() {
      if (disposed || isTauri || !engine) {
        return;
      }
      engine.markConnecting();
      connectionRef.current = "connecting";
      setConnection("connecting");
      const url = new URL(api.eventSocketUrl(engine.currentCursor));
      url.searchParams.set("protocol", "2");
      const currentSocket = new WebSocket(url.toString());
      socket = currentSocket;
      engine.setAckTransport((message) => {
        if (currentSocket.readyState !== WebSocket.OPEN) {
          return false;
        }
        try {
          currentSocket.send(JSON.stringify(message));
          return true;
        } catch {
          return false;
        }
      });
      currentSocket.onopen = () => {
        if (disposed || socket !== currentSocket) {
          currentSocket.close();
          return;
        }
        currentSocket.send(JSON.stringify({
          type: "auth",
          token: api.eventToken(),
          deviceId: engine?.state.deviceId,
          protocolVersion: 2,
        }));
      };
      currentSocket.onmessage = (event) => {
        if (disposed || socket !== currentSocket || !engine) {
          return;
        }
        let message: unknown;
        try {
          message = JSON.parse(String(event.data));
        } catch {
          return;
        }
        if (!message || typeof message !== "object") {
          return;
        }
        const value = message as Record<string, unknown>;
        if (value.type === "auth_ok") {
          attempts = 0;
          connectionRef.current = "connected";
          setConnection("connected");
          setConnectionError(null);
        } else if (value.type === "error") {
          connectionRef.current = "offline";
          setConnection("offline");
          setConnectionError(String(value.code ?? "服务器 WebSocket 鉴权失败"));
          return;
        }
        void engine.handleMessage(value).then((handled) => {
          if (!handled) {
            return handleServerEvent(value as unknown as ServerEvent, engine);
          }
          return undefined;
        }).catch((error) => {
          if (!disposed && socket === currentSocket) {
            setConnectionError(explainError(error));
          }
        });
      };
      currentSocket.onerror = () => {
        if (!disposed && socket === currentSocket) {
          engine?.markOffline();
          connectionRef.current = "offline";
          setConnection("offline");
          setConnectionError("服务器实时连接失败，正在重试");
        }
      };
      currentSocket.onclose = () => {
        if (disposed || socket !== currentSocket) {
          return;
        }
        socket = null;
        engine?.markOffline();
        connectionRef.current = "offline";
        setConnection("offline");
        if (deliberateReconnect) {
          deliberateReconnect = false;
          reconnectTimer = setTimeout(connectSocket, 0);
          return;
        }
        attempts += 1;
        reconnectTimer = setTimeout(connectSocket, Math.min(1000 * 2 ** attempts, 15000));
      };
    }

    async function setup() {
      const baseUrl = resolveHttpBase(api);
      const deviceId = await resolveSyncDeviceId();
      if (disposed) {
        return;
      }
      let initial = loadSyncCursor(baseUrl);
      let webCacheAvailable = false;
      let restoredFromWebCache = false;
      if (!isTauri) {
        const cached = await loadWebSyncCache(baseUrl);
        if (disposed) {
          return;
        }
        webCacheAvailable = cached.available;
        if (cached.cache && isWebSyncCacheUsable(cached.cache)) {
          await applySyncSnapshot(cached.cache.snapshot);
          if (disposed) {
            return;
          }
          initial = {
            feedId: cached.cache.meta.feedId,
            cursorSeq: cached.cache.meta.cursorSeq,
            latestSeq: cached.cache.meta.latestSeq,
            minRetainedSeq: cached.cache.meta.minRetainedSeq,
            snapshotRevision: cached.cache.meta.snapshotRevision,
            ackedSeq: cached.cache.meta.ackedSeq,
            lastSyncAt: cached.cache.meta.lastSyncAt,
          };
          restoredFromWebCache = true;
        } else if (cached.available) {
          initial = {
            feedId: "",
            cursorSeq: 0,
            latestSeq: 0,
            minRetainedSeq: 0,
            snapshotRevision: 0,
            ackedSeq: 0,
            lastSyncAt: null,
          };
        }
      }
      if (isTauri && localApi) {
        try {
          const meta = await localApi.syncMeta(settingsRef.current.serverUrl || baseUrl);
          initial = {
            feedId: meta.feedId ?? initial.feedId,
            cursorSeq: meta.cursorSeq,
            latestSeq: meta.latestSeq,
            minRetainedSeq: initial.minRetainedSeq,
            snapshotRevision: initial.snapshotRevision,
            ackedSeq: meta.lastAckSeq,
            lastSyncAt: meta.lastSyncAt,
          };
        } catch {
          // The local sync_meta table may be unavailable during a mixed rollout.
        }
      }
      engine = new SyncProtocolEngine({
        baseUrl,
        token: api.eventToken(),
        deviceId,
        initial,
        onState: (state) => {
          setSyncState(state);
          if (state.status === "error" && state.lastError) {
            setConnectionError(state.lastError);
            connectionRef.current = "offline";
            setConnection("offline");
          }
        },
        onChanges: applyFormalChanges,
        onSnapshot: applySyncSnapshot,
        onJobProgress: applyJobProgress,
        onResetComplete: () => {
          if (isTauri || !socket) {
            return;
          }
          deliberateReconnect = true;
          socket.close(1000, "sync-reset");
        },
        webCache: !isTauri,
      });
      syncEngineRef.current = engine;
      setSyncState(engine.state);
      try {
        await registerSyncDevice(baseUrl, api.eventToken(), deviceId);
      } catch {
        // The device list endpoint is optional until WP2 is fully rolling out.
      }
      if (!isTauri) {
        if (webCacheAvailable && !restoredFromWebCache) {
          try {
            await engine.forceResync("cache_missing");
          } catch (error) {
            if (!disposed) {
              showToast(`同步快照暂不可用：${explainError(error)}`);
            }
          }
        }
        // Settings are environment metadata, not a prerequisite for the realtime
        // protocol. A transient GET failure must not permanently prevent the
        // WebSocket from opening; server_settings changes still arrive via sync.
        try {
          await refreshServerSettings(api);
        } catch (error) {
          if (!disposed && serverApiRef.current === api) {
            showToast(`服务器设置暂不可用：${explainError(error)}`);
          }
        }
        if (disposed) {
          return;
        }
        connectSocket();
      }
    }

    void setup().catch((error) => {
      if (!disposed) {
        setSyncState((current) => ({
          ...current,
          status: "error",
          lastError: explainError(error),
        }));
      }
    });
    return () => {
      disposed = true;
      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
      }
      if (legacyRefreshTimer.current) {
        clearTimeout(legacyRefreshTimer.current);
        legacyRefreshTimer.current = null;
      }
      engine?.dispose();
      if (syncEngineRef.current === engine) {
        syncEngineRef.current = null;
      }
      socket?.close();
    };
  }, [applyFormalChanges, applyJobProgress, applySyncSnapshot, handleServerEvent, pendingImport, pendingServerSetup, pendingClientMigration, refreshActiveData, refreshServerSettings, serverApi, showToast]);

  useEffect(() => {
    if (page !== "settings" || !serverApi) {
      return;
    }
    void refreshSyncDevices();
    void refreshServerSettings(serverApi).catch((error) => showToast(explainError(error)));
    void serverApi
      .serverBiliStatus()
      .then((status) => setServerBiliConfigured(status.configured))
      .catch(() => undefined);
  }, [page, refreshServerSettings, refreshSyncDevices, serverApi, showToast]);

  useEffect(() => {
    if (!ready) {
      return;
    }
    const serverMode = !isTauri || settings.dataMode === "server";
    if (serverMode && syncState.protocolVersion === SYNC_PROTOCOL_VERSION) {
      return;
    }
    let disposed = false;
    const sync = () => {
      if (!disposed) {
        void refreshActiveData().catch(() => undefined);
      }
    };
    const pollMs = Math.max(settings.livePollSeconds, 5) * 1000;
    const initial = window.setTimeout(sync, 2000);
    const interval = window.setInterval(sync, pollMs);
    return () => {
      disposed = true;
      window.clearTimeout(initial);
      window.clearInterval(interval);
    };
  }, [ready, refreshActiveData, settings.dataMode, settings.livePollSeconds, syncState.protocolVersion]);

  useEffect(() => {
    if (!isTauri || !localApi || !serverApi || connection !== "connected") {
      return;
    }
    let cancelled = false;
    async function flush() {
      try {
        const pending = await localApi!.pendingMutations();
        if (!pending.length || cancelled) {
          return;
        }
        const outcomes = await serverApi!.applyMutations(pending);
        const applied = outcomes.some((outcome) => outcome.status === "applied");
        const rejected = outcomes.some(
          (outcome) => outcome.status !== "applied" && outcome.status !== "pending",
        );
        for (const outcome of outcomes) {
          await localApi!.updateMutationStatus(
            outcome.mutationId,
            outcome.status === "applied"
              ? "applied"
              : outcome.status === "pending"
                ? "pending"
                : "blocked",
            outcome.error ?? undefined,
          );
        }
        if ((applied || rejected) && !mutationResyncInFlightRef.current) {
          mutationResyncInFlightRef.current = true;
          try {
            if (applied) {
              try {
                await localApi!.purgeOfflineSchedules();
              } catch {
                // Still force the authoritative resync when cache cleanup fails.
              }
            }
            const engine = syncEngineRef.current;
            if (engine) {
              await engine.forceResync(applied ? "mutation_applied" : "mutation_rejected");
            } else {
              await refreshActiveData();
            }
            setScheduleRevision((value) => value + 1);
          } finally {
            mutationResyncInFlightRef.current = false;
          }
        }
      } catch {
        // Keep the queue intact while the server is unavailable.
      }
    }
    void flush();
    return () => {
      cancelled = true;
    };
  }, [connection, refreshActiveData, serverApi]);

  async function connectFromGate(token: string) {
    setAuthGateError(null);
    try {
      const api = await connectServer(isTauri ? settings.serverUrl : "", token, false);
      if (!isTauri) {
        const merged = {
          ...settingsRef.current,
          dataMode: "server" as const,
          serverUrl: resolveHttpBase(api),
        };
        settingsRef.current = merged;
        setSettings(merged);
        saveWebDevicePreferences(merged);
        const deviceId = syncStateRef.current.deviceId || await resolveSyncDeviceId();
        const serverUrl = resolveHttpBase(api);
        if (!hasCompletedServerOnboarding(serverUrl, deviceId)) {
          markServerOnboardingComplete(serverUrl, deviceId);
        }
        setPage("schedule");
      }
    } catch (error) {
      setAuthGateError(explainError(error));
    }
  }
  async function saveAllSettings(
    draft: AppSettings,
    secretInput: SecretInput,
    autoLaunch: boolean,
  ): Promise<AppSettings> {
    // Any explicit save supersedes an older background settings GET.
    settingsRequestSeq.current += 1;
    llmSecretRequestSeq.current += 1;
    if (!isTauri) {
      if (!serverApi) {
        throw new Error("服务器未连接");
      }
      const remote = await serverApi.settings();
      if (secretInput.llmApiKey?.trim()) {
        const savedSecret = await serverApi.saveLlmSecret(
          secretInput.llmApiKey.trim(),
          draft.llmBaseUrl,
        );
        setLlmApiKeyHint(maskLlmApiKey(secretInput.llmApiKey));
        setSecrets((current) => ({ ...current, llmConfigured: savedSecret.configured }));
      }
      const saved = await serverApi.saveSettings(serverSettingsForSave(draft, remote));
      if (!secretInput.llmApiKey?.trim()) {
        await syncServerLlmSecret(serverApi);
      }
      const merged = mergeServerSettings({ ...draft, dataMode: "server" }, saved);
      settingsRequestSeq.current += 1;
      settingsRef.current = merged;
      setSettings(merged);
      saveWebDevicePreferences(merged);
      return merged;
    }
    if (!localApi) {
      throw new Error("桌面 API 未初始化");
    }
    if (draft.dataMode === "server") {
      const token =
        secretInput.serverToken?.trim() ||
        serverTokenRef.current ||
        (await localApi.serverToken()) ||
        "";
      if (!token) {
        throw new Error("服务器模式需要访问令牌");
      }

      // Validate and stage remote changes with a temporary API. Local settings
      // are not committed until the destination has authenticated successfully.
      const validationApi = new ServerApi({ baseUrl: draft.serverUrl, token });
      await Promise.all([validationApi.subscriptions(), validationApi.liveStatuses()]);
      const remoteSettings = await validationApi.settings();
      await applyAutostart(autoLaunch);
      setAutostart(autoLaunch);
      if (secretInput.llmApiKey?.trim()) {
        await validationApi.saveLlmSecret(secretInput.llmApiKey.trim(), draft.llmBaseUrl);
        setLlmApiKeyHint(maskLlmApiKey(secretInput.llmApiKey));
      }
      const savedServerSettings = await validationApi.saveSettings(
        serverSettingsForSave(draft, remoteSettings),
      );
      const merged = mergeServerSettings(
        { ...draft, dataMode: "server" },
        savedServerSettings,
      );
      const nextSecrets = await localApi.saveSecrets(secretInput);
      await localApi.saveSettings(merged);
      settingsRequestSeq.current += 1;
      settingsRef.current = merged;
      setSecrets(nextSecrets);
      setSettings(merged);
      serverTokenRef.current = token;
      setServerToken(token);
      const api = await connectServer(draft.serverUrl, token);
      void syncServerLlmSecret(api).catch((error) => showToast(explainError(error)));
      const deviceId = syncStateRef.current.deviceId || (await localApi.deviceId());
      const serverUrl = resolveHttpBase(api);
      setPendingServerSetup(
        hasCompletedServerOnboarding(serverUrl, deviceId)
          ? null
          : { api, token, deviceId },
      );
      return merged;
    }

    const committedDraft = { ...draft, youtubeEnabled: draft.youtubeEnabled ?? false };
    await applyAutostart(autoLaunch);
    setAutostart(autoLaunch);
    const localEpoch = ++connectionEpochRef.current;
    activeDataRequestSeq.current += 1;
    syncDevicesRequestSeq.current += 1;
    const monitorGenerationToStop = remoteMonitorGenerationRef.current;
    const nextSecrets = await localApi.saveSecrets(secretInput);
    if (localEpoch !== connectionEpochRef.current) {
      return draft;
    }
    await localApi.saveSettings(committedDraft);
    if (localEpoch !== connectionEpochRef.current) {
      return draft;
    }
    await stopRemoteMonitorGeneration(monitorGenerationToStop);
    if (localEpoch !== connectionEpochRef.current) {
      return draft;
    }
    serverApiRef.current = null;
    setServerApi(null);
    setLlmApiKeyHint("");
    connectionRef.current = "local";
    setConnection("local");
    const boot = await localApi.bootstrap();
    if (localEpoch !== connectionEpochRef.current) {
      return draft;
    }
    settingsRequestSeq.current += 1;
    settingsRef.current = committedDraft;
    setSecrets(nextSecrets);
    setSettings(committedDraft);
    setSubscriptions(boot.subscriptions);
    setLiveStatuses(boot.liveStatuses);
    seenLiveSeq.current = 0;
    return committedDraft;
  }

  function useServerDataForSetup() {
    if (pendingServerSetup) {
      markServerOnboardingComplete(
        resolveHttpBase(pendingServerSetup.api),
        pendingServerSetup.deviceId,
      );
    }
    setPendingClientMigration(null);
    setPendingServerSetup(null);
    setMigrationBusy(false);
    showToast("已选择使用服务端数据，正在同步");
  }

  function deferServerSetup() {
    if (pendingServerSetup) {
      markServerOnboardingComplete(
        resolveHttpBase(pendingServerSetup.api),
        pendingServerSetup.deviceId,
      );
    }
    setPendingServerSetup(null);
    setMigrationBusy(false);
  }

  async function prepareClientMigration(setup = pendingServerSetup) {
    if (!setup || !localApi || migrationBusyRef.current) {
      return;
    }
    migrationBusyRef.current = true;
    setMigrationBusy(true);
    try {
      const snapshot = await localApi.exportSnapshot();
      const preview = await setup.api.previewMigration(snapshot);
      setPendingClientMigration({
        snapshot,
        preview,
        api: setup.api,
        deviceId: setup.deviceId,
        migrationId: crypto.randomUUID(),
      });
      setPendingServerSetup(null);
    } catch (error) {
      showToast(explainError(error));
    } finally {
      migrationBusyRef.current = false;
      setMigrationBusy(false);
    }
  }

  async function openClientMigrationToServer(): Promise<void> {
    if (!serverApi || !localApi) {
      throw new Error("服务器或本地 API 未连接");
    }
    const deviceId = syncStateRef.current.deviceId || (await localApi.deviceId());
    await prepareClientMigration({
      api: serverApi,
      token: serverToken,
      deviceId,
    });
  }

  async function applyClientMigration(strategy: "server" | "client" | "skip") {
    if (!pendingClientMigration || !localApi || migrationBusyRef.current) {
      return;
    }
    migrationBusyRef.current = true;
    setMigrationBusy(true);
    try {
      const migration = pendingClientMigration;
      const expectedServerDigest = migration.preview.serverStateDigest.trim();
      if (!expectedServerDigest) {
        throw new Error("迁移预览缺少服务器状态摘要，请重新生成预览");
      }
      const report = await migration.api.applyMigration(
        migration.snapshot,
        strategy,
        migration.deviceId,
        migration.migrationId,
        expectedServerDigest,
      );
      markServerOnboardingComplete(
        resolveHttpBase(migration.api),
        migration.deviceId,
      );
      setPendingClientMigration(null);
      setPendingServerSetup(null);
      await refreshActiveData();
      setScheduleRevision((value) => value + 1);
      showToast(
        "迁移完成：订阅 " + report.subscriptions + "，日程 " + report.schedules +
          "，周表 " + report.weeklyRules + "，跳过 " + report.skipped,
      );
    } catch (error) {
      if (error instanceof ApiFailure && error.code === "MIGRATION_STALE_PREVIEW") {
        const migration = pendingClientMigration;
        try {
          if (!migration) {
            throw error;
          }
          const preview = await migration.api.previewMigration(migration.snapshot);
          setPendingClientMigration({
            ...migration,
            preview,
            migrationId: crypto.randomUUID(),
          });
          showToast("服务器数据已变化，已重新生成迁移预览，请重新确认");
        } catch (refreshError) {
          showToast(`迁移预览已过期，请重新打开迁移：${explainError(refreshError)}`);
        }
      } else {
        showToast(explainError(error));
      }
    } finally {
      migrationBusyRef.current = false;
      setMigrationBusy(false);
    }
  }

  async function testServerConnection(url: string, token: string): Promise<string> {
    if (!isTauri) {
      if (!serverApi) {
        throw new Error("服务器未连接");
      }
      const health = await serverApi.health();
      return `已连接 v${health.version}`;
    }
    const effectiveToken = token.trim() || serverTokenRef.current;
    const api = new ServerApi({ baseUrl: url, token: effectiveToken });
    const health = await api.health();
    return `已连接 v${health.version}`;
  }

  async function testLlm(input: LlmTestInput): Promise<LlmTestReport> {
    const serverMode = !isTauri || settingsRef.current.dataMode === "server";
    if (serverMode) {
      if (!serverApi) {
        throw new Error("服务器未连接");
      }
      return serverApi.testLlm(input);
    }
    if (!localApi) {
      throw new Error("桌面 API 未初始化");
    }
    return localApi.testLlm(input);
  }

  async function testProxy(proxyPort: number, mode: "local" | "server"): Promise<ProxyTestReport> {
    if (!isTauri || mode === "server") {
      if (!serverApi) {
        throw new Error("服务器未连接");
      }
      return serverApi.testProxy(proxyPort);
    }
    if (!localApi) {
      throw new Error("桌面 API 未初始化");
    }
    return localApi.testProxy(proxyPort);
  }

  async function listLlmModels(input: LlmDiscoveryInput): Promise<string[]> {
    const serverMode = !isTauri || settingsRef.current.dataMode === "server";
    if (serverMode) {
      if (!serverApi) {
        throw new Error("服务器未连接");
      }
      return serverApi.listLlmModels(input);
    }
    if (!localApi) {
      throw new Error("桌面 API 未初始化");
    }
    return localApi.listLlmModels(input);
  }

  async function sendTestNotification() {
    const granted = await ensureNotificationPermission();
    if (!granted) {
      throw new Error("Windows 通知权限未授予");
    }
    notifyLive({
      streamerId: 0,
      mid: "0",
      streamerName: "OshiCal 通知测试",
      previousIsLive: false,
      isLive: true,
      title: "点击这条通知会打开 B 站直播间",
      liveUrl: "https://live.bilibili.com/22637261",
      checkedAt: new Date().toISOString(),
      liveNotifyEnabled: true,
    });
  }
  async function completeImport(strategy: "server" | "client" | "skip") {
    if (!pendingImport || !localApi || importBusyRef.current) {
      return;
    }
    importBusyRef.current = true;
    setImportBusy(true);
    try {
      const report = await localApi.importSnapshot(pendingImport.snapshot, strategy);
      setPendingImport(null);
      await refreshActiveData();
      setScheduleRevision((value) => value + 1);
      showToast(
        `导入完成：订阅 ${report.subscriptions}，日程 ${report.schedules}，跳过 ${report.skipped}`,
      );
    } catch (error) {
      showToast(explainError(error));
    } finally {
      importBusyRef.current = false;
      setImportBusy(false);
    }
  }

  async function openServerImport(): Promise<void> {
    if (!isTauri || !localApi || !serverApi) {
      throw new Error("仅桌面端可把服务端数据导入本地缓存");
    }
    if (importPreviewBusyRef.current) {
      return;
    }
    importPreviewBusyRef.current = true;
    const api = serverApi;
    try {
      const snapshot = await api.snapshot();
      const preview = await localApi.previewSnapshot(snapshot);
      if (serverApiRef.current !== api) {
        throw new Error("服务器连接已切换，请重新生成导入预览");
      }
      setPendingImport({ snapshot, preview, api });
    } finally {
      importPreviewBusyRef.current = false;
    }
  }

  const scheduleActions = useMemo(() => {
    const serverMode = !isTauri || settings.dataMode === "server";
    const filterVisibleSchedules = (items: Schedule[]): Schedule[] => (
      hiddenStreamerIds.size
        ? items.filter((schedule) => !hiddenStreamerIds.has(schedule.streamerId))
        : items
    );

    function remoteScheduleId(schedule: Schedule): number | null {
      const id = schedule.remoteId ?? schedule.id;
      return Number.isSafeInteger(id) && id > 0 ? id : null;
    }

    async function blockPendingCreateForSchedule(schedule: Schedule): Promise<string | null> {
      if (!localApi) {
        return null;
      }
      const pending = await localApi.pendingMutations();
      const createMutation = pending.find((mutation) => {
        if (mutation.operation !== "create" || mutation.entity !== "schedule") {
          return false;
        }
        if (!mutation.payload || typeof mutation.payload !== "object") {
          return false;
        }
        const payload = mutation.payload as Partial<NewScheduleInput>;
        return payload.streamerId === schedule.streamerId &&
          payload.sourceIdentity === schedule.sourceIdentity;
      });
      if (!createMutation) {
        return null;
      }
      await localApi.updateMutationStatus(
        createMutation.mutationId,
        "blocked",
        "本地已删除尚未同步的临时日程",
      );
      return createMutation.mutationId;
    }

    async function localFallbackForCreate(input: NewScheduleInput): Promise<Schedule> {
      if (!localApi) {
        throw new Error("本地缓存不可用");
      }
      const mutation: SyncMutation = {
        mutationId: crypto.randomUUID(),
        operation: "create",
        entity: "schedule",
        payload: { ...input, streamerId: input.streamerId },
        baseUpdatedAt: null,
        deviceId: syncStateRef.current.deviceId || null,
        baseRevision: null,
        status: "pending",
        lastError: null,
        createdAt: new Date().toISOString(),
        appliedAt: null,
      };
      // Persist the durable intent first. If the mirror write then fails, the
      // queued mutation still reconciles the operation on the next sync.
      await localApi.enqueueMutation(mutation);
      return localApi.createCachedSchedule(input);
    }

    return {
      loadCached: serverMode
        ? async (from: string, to: string): Promise<Schedule[] | null> => {
            if (isTauri) {
              if (!localApi) return null;
              try {
                const meta = await localApi.syncMeta(settings.serverUrl);
                if (!meta.initialized) return null;
                return filterVisibleSchedules(await localApi.cachedSchedules(from, to));
              } catch {
                return null;
              }
            }
            if (!serverApi) return null;
            try {
              const cached = await loadWebSyncCache(resolveHttpBase(serverApi));
              if (!cached.available || !isWebSyncCacheUsable(cached.cache)) return null;
              const fromMs = Date.parse(from);
              const toMs = Date.parse(to);
              if (!Number.isFinite(fromMs) || !Number.isFinite(toMs)) return null;
              const enabledStreamers = new Set(
                cached.cache.snapshot.subscriptions
                  .filter((streamer) => streamer.scheduleEnabled)
                  .map((streamer) => streamer.id),
              );
              return filterVisibleSchedules(cached.cache.snapshot.schedules.filter((schedule) => {
                const startMs = Date.parse(schedule.startAt);
                return Number.isFinite(startMs) &&
                  startMs >= fromMs &&
                  startMs < toMs &&
                  !schedule.cancelled &&
                  (enabledStreamers.has(schedule.streamerId) || schedule.sourceKind === "manual");
              }));
            } catch {
              return null;
            }
          }
        : undefined,
      load: async (from: string, to: string) => {
        if (serverMode && serverApi) {
          try {
            return filterVisibleSchedules(await serverApi.schedules(from, to));
          } catch (error) {
            if (isTauri && localApi && isRemoteUncertain(error)) {
              setConnection("offline");
              return filterVisibleSchedules(await localApi.cachedSchedules(from, to));
            }
            throw error;
          }
        }
        if (localApi) {
          return filterVisibleSchedules(await localApi.schedules(from, to));
        }
        return [];
      },
      create: async (input: NewScheduleInput) => {
        if (!serverMode) {
          if (!localApi) {
            throw new Error("本地 API 不可用");
          }
          return localApi.createSchedule(input);
        }
        if (!serverApi) {
          throw new Error("服务器未连接");
        }
        try {
          return await serverApi.createSchedule(input);
        } catch (error) {
          if (isTauri && isRemoteUncertain(error)) {
            setConnection("offline");
            return localFallbackForCreate(input);
          }
          throw error;
        }
      },
      update: async (schedule: Schedule, update: UpdateScheduleInput) => {
        if (!serverMode) {
          if (!localApi) {
            throw new Error("本地 API 不可用");
          }
          return localApi.updateSchedule(schedule.id, update);
        }
        if (!serverApi) {
          throw new Error("服务器未连接");
        }
        const remoteId = remoteScheduleId(schedule);
        if (!remoteId) {
          throw new Error("该日程尚未同步到服务器，请等待新增同步完成后再修改");
        }
        try {
          return await serverApi.updateSchedule(remoteId, update);
        } catch (error) {
          if (isTauri && localApi && isRemoteUncertain(error)) {
            setConnection("offline");
            await localApi.enqueueMutation({
              mutationId: crypto.randomUUID(),
              operation: "update",
              entity: "schedule",
              payload: { id: remoteId, update },
              baseUpdatedAt: schedule.remoteUpdatedAt ?? schedule.updatedAt,
              deviceId: syncStateRef.current.deviceId || null,
              baseRevision: schedule.revision ?? null,
              status: "pending",
              lastError: null,
              createdAt: new Date().toISOString(),
              appliedAt: null,
            });
            return localApi.updateCachedSchedule(
              schedule.cachedLocalId ?? schedule.id,
              update,
            );
          }
          throw error;
        }
      },
      remove: async (schedule: Schedule) => {
        if (!serverMode) {
          if (!localApi) {
            throw new Error("本地 API 不可用");
          }
          await localApi.deleteSchedule(schedule.id);
          return;
        }
        if (!serverApi) {
          throw new Error("服务器未连接");
        }
        const remoteId = remoteScheduleId(schedule);
        if (!remoteId) {
          if (!localApi) {
            throw new Error("该日程尚未同步到服务器，请等待新增同步完成后再删除");
          }
          const cachedId = schedule.cachedLocalId ?? schedule.id;
          const blockedMutationId = await blockPendingCreateForSchedule(schedule);
          try {
            await localApi.deleteCachedSchedule(cachedId);
          } catch (error) {
            if (blockedMutationId) {
              await localApi.updateMutationStatus(blockedMutationId, "pending").catch(() => undefined);
            }
            throw error;
          }
          showToast("已移除尚未同步到服务器的临时日程");
          return;
        }
        if (!isTauri) {
          await serverApi.deleteSchedule(remoteId);
          return;
        }
        if (!localApi) {
          throw new Error("本地缓存未连接");
        }
        const cachedId = schedule.cachedLocalId ?? schedule.id;
        try {
          await serverApi.deleteSchedule(remoteId);
        } catch (error) {
          if (isTauri && isRemoteUncertain(error)) {
            setConnection("offline");
            await localApi.enqueueMutation({
              mutationId: crypto.randomUUID(),
              operation: "delete",
              entity: "schedule",
              payload: { id: remoteId },
              baseUpdatedAt: schedule.remoteUpdatedAt ?? schedule.updatedAt,
              deviceId: syncStateRef.current.deviceId || null,
              baseRevision: schedule.revision ?? null,
              status: "pending",
              lastError: null,
              createdAt: new Date().toISOString(),
              appliedAt: null,
            });
            await localApi.deleteCachedSchedule(cachedId);
            return;
          }
          throw error;
        }
      },
      liveSession: async (scheduleId: number) => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.liveSessionForSchedule(scheduleId);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.liveSessionForSchedule(scheduleId);
      },
      liveOnlineSamples: async (sessionId: number, afterId: number, signal?: AbortSignal) => {
        if (serverMode) {
          if (!serverApi) throw new Error("服务器未连接");
          return serverApi.liveOnlineSamples(sessionId, afterId, 500, signal);
        }
        if (!localApi) throw new Error("本地 API 不可用");
        return localApi.liveOnlineSamples(sessionId, afterId, 500);
      },
      refresh: async () => {
        let report: RefreshReport | undefined;
        if (serverMode) {
          if (!serverApi) throw new Error("服务器未连接");
          report = await refreshAllJob(serverApi, applyRefreshProgress);
        } else if (localApi) {
          report = await localApi.refresh();
        }
        await refreshActiveData();
        if (report?.failedStreamers.length) {
          throw new Error(`刷新完成，但部分来源失败：${report.failedStreamers.join("；")}`);
        }
      },
    };
  }, [hiddenStreamerIds, refreshActiveData, serverApi, settings.dataMode, settings.serverUrl, showToast]);

  const creatorActions = useMemo(() => {
    const serverMode = !isTauri || settings.dataMode === "server";
    return {
      search: async (keyword: string, platform: Platform): Promise<AccountSearchResult[]> => {
        if (platform === "youtube" && !settings.youtubeEnabled) {
          throw new Error("YouTube 功能未开启");
        }
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.search(keyword, platform);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.search(keyword, platform);
      },
      add: async (account: AccountSearchResult): Promise<Streamer> => {
        const platform = account.platform === "youtube" ? "youtube" : "bilibili";
        if (platform === "youtube" && !settings.youtubeEnabled) {
          throw new Error("YouTube 功能未开启");
        }
        const input = {
          platform,
          mid: account.mid,
          name: account.name,
          avatarUrl: account.avatarUrl,
          handle: account.handle ?? null,
          profileUrl: account.profileUrl ?? null,
          uploadsPlaylistId: account.uploadsPlaylistId ?? null,
          roomId: account.roomId,
          aliases: [account.name],
          scheduleEnabled: true,
          liveNotifyEnabled: true,
          liveStatsEnabled: false,
        };
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.addSubscription(input);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.addSubscription(input);
      },
      update: async (id: number, update: UpdateSubscriptionInput): Promise<Streamer> => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.updateSubscription(id, update);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.updateSubscription(id, update);
      },
      reorder: async (ids: number[]): Promise<Streamer[]> => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.reorderSubscriptions(ids);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.reorderSubscriptions(ids);
      },
      remove: async (id: number): Promise<void> => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.deleteSubscription(id);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.deleteSubscription(id);
      },
      addLink: async (id: number, platform: Platform, mid: string): Promise<unknown> => {
        if (platform === "youtube" && !settings.youtubeEnabled) {
          throw new Error("YouTube 功能未开启");
        }
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.addLinkedAccount(id, mid, platform);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.addLinkedAccount(id, mid, platform);
      },
      removeLink: async (id: number, platform: Platform, mid: string): Promise<void> => {
        if (platform === "youtube" && !settings.youtubeEnabled) {
          throw new Error("YouTube 功能未开启");
        }
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          await serverApi.removeLinkedAccount(id, mid, platform);
          return;
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.removeLinkedAccount(id, mid, platform);
      },
      refresh: async (id: number): Promise<RefreshReport> => {
        const streamer = subscriptions.find((item) => item.id === id);
        if (streamer?.platform === "youtube" && !settings.youtubeEnabled) {
          throw new Error("YouTube 功能未开启");
        }
        const streamerName = streamer?.name ?? `主播 ${id}`;
        applyRefreshProgress({
          streamerId: id,
          streamerName,
          stage: "准备刷新（有可共享任务时直接加入）",
          completed: 0,
          total: 0,
          percent: 0,
          done: false,
          failed: false,
        });
        try {
          let report;
          if (serverMode) {
            if (!serverApi) {
              throw new Error("服务器未连接");
            }
            return await refreshStreamerJob(serverApi, id, streamerName, applyRefreshProgress);
          } else {
            if (!localApi) {
              throw new Error("本地 API 不可用");
            }
            report = await localApi.refreshStreamer(id);
          }
          applyRefreshProgress(refreshCompletionProgress(report, id, streamerName));
          return report;
        } catch (error) {
          applyRefreshProgress({
            streamerId: id,
            streamerName,
            stage: `刷新失败：${explainError(error)}`,
            completed: 1,
            total: 1,
            percent: 100,
            done: true,
            failed: true,
          });
          throw error;
        }
      },
      fanHistory: async (streamerId: number, days = 30): Promise<FanHistory> => {
        const streamer = subscriptions.find((item) => item.id === streamerId);
        if (streamer?.platform === "youtube" && !settings.youtubeEnabled) {
          throw new Error("YouTube 功能未开启");
        }
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.fanHistory(streamerId, days);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.fanHistory(streamerId, days);
      },
      refreshFans: async (streamerId: number): Promise<FanHistory> => {
        const streamer = subscriptions.find((item) => item.id === streamerId);
        if (streamer?.platform === "youtube" && !settings.youtubeEnabled) {
          throw new Error("YouTube 功能未开启");
        }
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.refreshFans(streamerId);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.refreshFans(streamerId);
      },
      liveSessions: async (streamerId: number, limit: number) => {
        const streamer = subscriptions.find((item) => item.id === streamerId);
        if (streamer?.platform === "youtube" && !settings.youtubeEnabled) {
          throw new Error("YouTube 功能未开启");
        }
        if (serverMode) {
          if (!serverApi) throw new Error("服务器未连接");
          return serverApi.liveSessions(streamerId, limit);
        }
        if (!localApi) throw new Error("本地 API 不可用");
        return localApi.liveSessions(streamerId, limit);
      },
      liveOnlineSamples: scheduleActions.liveOnlineSamples,
      fertility: async (refresh: boolean): Promise<FertilitySnapshot> => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.fertility(refresh);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.fertility(refresh);
      },
      listWeeklyRules: async (streamerId: number): Promise<WeeklyScheduleRule[]> => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.listWeeklyRules(streamerId);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.listWeeklyRules(streamerId);
      },
      createWeeklyRule: async (
        input: NewWeeklyScheduleRuleInput,
      ): Promise<WeeklyScheduleRule> => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.createWeeklyRule(input);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.createWeeklyRule(input);
      },
      updateWeeklyRule: async (
        id: number,
        update: UpdateWeeklyScheduleRuleInput,
      ): Promise<WeeklyScheduleRule> => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          return serverApi.updateWeeklyRule(id, update);
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        return localApi.updateWeeklyRule(id, update);
      },
      deleteWeeklyRule: async (id: number): Promise<void> => {
        if (serverMode) {
          if (!serverApi) {
            throw new Error("服务器未连接");
          }
          await serverApi.deleteWeeklyRule(id);
          return;
        }
        if (!localApi) {
          throw new Error("本地 API 不可用");
        }
        await localApi.deleteWeeklyRule(id);
      },
    };
  }, [scheduleActions.liveOnlineSamples, applyRefreshProgress, serverApi, settings.dataMode, settings.youtubeEnabled, subscriptions]);

  async function handleCreatorChanged() {
    try {
      await refreshActiveData();
      setScheduleRevision((value) => value + 1);
    } catch (error) {
      showToast(explainError(error));
    }
  }

  function showCreatorMenu(streamer: Streamer, x: number, y: number) {
    const menuWidth = 188;
    const menuHeight = 222;
    setCreatorMenu({
      streamer,
      x: Math.max(8, Math.min(x, window.innerWidth - menuWidth - 8)),
      y: Math.max(8, Math.min(y, window.innerHeight - menuHeight - 8)),
    });
  }

  function showCreatorMenuFromKeyboard(
    event: ReactKeyboardEvent<HTMLDivElement>,
    streamer: Streamer,
  ) {
    if (event.key !== "ContextMenu" && !(event.shiftKey && event.key === "F10")) {
      return;
    }
    event.preventDefault();
    const rect = event.currentTarget.getBoundingClientRect();
    showCreatorMenu(streamer, rect.left + 12, rect.bottom - 2);
  }

  async function openCreatorLiveRoom(streamer: Streamer) {
    setCreatorMenu(null);
    const platform = streamer.platform === "youtube" ? "youtube" : "bilibili";
    if (platform === "youtube" && !settings.youtubeEnabled) {
      showToast("YouTube 功能未开启");
      return;
    }
    const youtubeLiveUrl = streamer.liveUrl
      || (streamer.liveVideoId
        ? `https://www.youtube.com/watch?v=${encodeURIComponent(streamer.liveVideoId)}`
        : null);
    const targetUrl = platform === "youtube"
      ? streamer.isLive && youtubeLiveUrl
        ? youtubeLiveUrl
        : streamer.profileUrl
          || (streamer.handle
            ? `https://www.youtube.com/${streamer.handle.startsWith("@") ? streamer.handle : `@${streamer.handle}`}`
            : streamer.mid
              ? `https://www.youtube.com/channel/${encodeURIComponent(streamer.mid)}`
              : null)
      : streamer.liveUrl
        ?? (streamer.roomId ? `https://live.bilibili.com/${streamer.roomId}` : null);
    if (!targetUrl) {
      showToast(platform === "youtube"
        ? `「${streamer.name}」缺少频道地址`
        : `「${streamer.name}」暂未配置直播间地址`);
      return;
    }
    try {
      await openExternalUrl(targetUrl);
    } catch (error) {
      showToast(explainError(error));
    }
  }

  async function openCreatorHomepage(streamer: Streamer) {
    setCreatorMenu(null);
    const mid = streamer.mid.trim();
    if (streamer.platform === "youtube") {
      if (!settings.youtubeEnabled) {
        showToast("YouTube 功能未开启");
        return;
      }
      const profileUrl = streamer.profileUrl
        || (streamer.handle
          ? `https://www.youtube.com/${streamer.handle.startsWith("@") ? streamer.handle : `@${streamer.handle}`}`
          : mid
            ? `https://www.youtube.com/channel/${encodeURIComponent(mid)}`
            : null);
      if (!profileUrl) {
        showToast(`「${streamer.name}」缺少频道地址`);
        return;
      }
      try {
        await openExternalUrl(profileUrl);
      } catch (error) {
        showToast(explainError(error));
      }
      return;
    }
    if (!mid) {
      showToast(`「${streamer.name}」缺少 UID，无法打开个人主页`);
      return;
    }
    try {
      await openExternalUrl(`https://space.bilibili.com/${encodeURIComponent(mid)}`);
    } catch (error) {
      showToast(explainError(error));
    }
  }

  async function refreshSidebarCreator(streamer: Streamer) {
    setCreatorMenu(null);
    if (sidebarRefreshingId !== null) {
      return;
    }
    setSidebarRefreshingId(streamer.id);
    try {
      await creatorActions.refresh(streamer.id);
      await handleCreatorChanged();
    } catch (error) {
      showToast(explainError(error));
    } finally {
      setSidebarRefreshingId(null);
    }
  }

  async function removeSidebarCreator(streamer: Streamer) {
    setCreatorMenu(null);
    if (!window.confirm(`删除主播「${streamer.name}」？她的日程、官号关联和来源记录会一起删除。`)) {
      return;
    }
    try {
      await creatorActions.remove(streamer.id);
      await handleCreatorChanged();
    } catch (error) {
      showToast(explainError(error));
    }
  }

  function beginSidebarResize(event: ReactPointerEvent<HTMLDivElement>) {
    if (event.button !== 0) {
      return;
    }
    sidebarResizeOrigin.current = { startX: event.clientX, startWidth: sidebarWidth };
    event.currentTarget.setPointerCapture(event.pointerId);
    setSidebarResizing(true);
  }

  function moveSidebarResize(event: ReactPointerEvent<HTMLDivElement>) {
    const origin = sidebarResizeOrigin.current;
    if (!origin) {
      return;
    }
    const next = origin.startWidth + event.clientX - origin.startX;
    setSidebarWidth(Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, next)));
  }

  function endSidebarResize(event: ReactPointerEvent<HTMLDivElement>) {
    sidebarResizeOrigin.current = null;
    setSidebarResizing(false);
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  }

  function resizeSidebarByKeyboard(event: ReactKeyboardEvent<HTMLDivElement>) {
    const step = event.shiftKey ? 32 : 12;
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      setSidebarWidth((width) => Math.max(MIN_SIDEBAR_WIDTH, width - step));
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      setSidebarWidth((width) => Math.min(MAX_SIDEBAR_WIDTH, width + step));
    } else if (event.key === "Home") {
      event.preventDefault();
      setSidebarWidth(DEFAULT_SIDEBAR_WIDTH);
    }
  }

  async function minimizeWindow() {
    if (!desktopWindow) return;
    try {
      await desktopWindow.minimize();
    } catch (error) {
      showToast(explainError(error));
    }
  }

  async function toggleWindowMaximize() {
    if (!desktopWindow) return;
    try {
      await desktopWindow.toggleMaximize();
    } catch (error) {
      showToast(explainError(error));
    }
  }

  async function closeWindow() {
    if (!desktopWindow) return;
    try {
      await desktopWindow.close();
    } catch (error) {
      showToast(explainError(error));
    }
  }

  async function navigateToPage(nextPage: Page) {
    if (nextPage === page) {
      return;
    }
    if (page === "settings") {
      const canLeave = settingsViewRef.current
        ? await settingsViewRef.current.requestLeave()
        : true;
      if (!canLeave) {
        return;
      }
    }
    setPage(nextPage);
  }

  if (!ready) {
    return (
      <div className="boot-screen">
        <div className="brand-mark large"><BrandMark size={38} /></div>
        <LoaderCircle className="spin" size={24} />
        <p>正在载入推历 OshiCal…</p>
      </div>
    );
  }

  if (!isTauri && !serverApi) {
    return <LoginGate error={authGateError} onSubmit={connectFromGate} />;
  }

  const connectionLabel =
    connection === "connected"
      ? "服务器已连接"
      : connection === "connecting"
        ? "正在连接"
        : connection === "local"
          ? "本地模式"
          : "服务器离线";

  return (
    <div
      className={`app-shell ${isTauri ? "is-desktop" : ""} ${sidebarResizing ? "is-resizing-sidebar" : ""}`}
      style={{ "--sidebar-width": `${sidebarWidth}px` } as CSSProperties}
    >
      {desktopWindow ? (
        <div className="window-chrome">
          <div
            className="window-drag-region"
            data-tauri-drag-region
            onDoubleClick={() => void toggleWindowMaximize()}
          />
          <div className="window-controls" role="group" aria-label="窗口控制">
            <button type="button" className="window-control" aria-label="最小化" title="最小化" onClick={() => void minimizeWindow()}>
              <Minus size={15} />
            </button>
            <button type="button" className="window-control" aria-label="最大化或还原" title="最大化或还原" onClick={() => void toggleWindowMaximize()}>
              <Square size={13} />
            </button>
            <button type="button" className="window-control close" aria-label="关闭到托盘" title="关闭到托盘" onClick={() => void closeWindow()}>
              <X size={16} />
            </button>
          </div>
        </div>
      ) : null}
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark"><BrandMark size={27} /></div>
          <div><strong>推历</strong><span>OshiCal · 直播日程</span></div>
        </div>
        <nav className="nav-list">
          <button className={page === "schedule" ? "active" : ""} onClick={() => void navigateToPage("schedule")}><CalendarRange size={19} />日程<span>{visibleSubscriptions.length}</span></button>
          <button className={page === "creators" ? "active" : ""} onClick={() => void navigateToPage("creators")}><UsersRound size={19} />主播库</button>
          <button className={page === "settings" ? "active" : ""} onClick={() => void navigateToPage("settings")}><SettingsIcon size={19} />设置</button>
        </nav>
        <div className="sidebar-creators">
          <p>我的主播</p>
          {visibleSubscriptions.map((streamer) => {
            const menuOpen = creatorMenu?.streamer.id === streamer.id;
            const refreshing = sidebarRefreshingId === streamer.id;
            const platform = streamer.platform === "youtube" ? "youtube" : "bilibili";
            const openLabel = platform === "youtube"
              ? streamer.isLive && streamer.liveUrl ? "双击进入直播间" : "双击打开频道"
              : "双击进入直播间";
            return (
              <div
                className={`mini-creator ${menuOpen ? "is-menu-open" : ""}`}
                key={streamer.id}
                title={`${streamer.name} · ${openLabel} · 右键打开操作菜单`}
                tabIndex={0}
                aria-label={`${streamer.name}，${openLabel}，右键或按 Shift+F10 打开操作菜单`}
                aria-haspopup="menu"
                aria-expanded={menuOpen}
                onDoubleClick={(event) => {
                  if (event.target instanceof Element && event.target.closest("button, a, input, textarea, select")) {
                    return;
                  }
                  void openCreatorLiveRoom(streamer);
                }}
                onContextMenu={(event) => {
                  event.preventDefault();
                  showCreatorMenu(streamer, event.clientX, event.clientY);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && event.target === event.currentTarget) {
                    event.preventDefault();
                    void openCreatorLiveRoom(streamer);
                    return;
                  }
                  showCreatorMenuFromKeyboard(event, streamer);
                }}
              >
                <Avatar url={streamer.avatarUrl} name={streamer.name} size="sm" live={streamer.isLive} />
                <span className="mini-creator-name">{streamer.name}</span>
                {refreshing ? <LoaderCircle className="spin mini-creator-spinner" size={14} aria-label="刷新中" /> : null}
              </div>
            );
          })}
          {!visibleSubscriptions.length ? <span className="muted">暂无订阅</span> : null}
        </div>
        <div className={`connection-chip ${connection}`}>
          {connection === "connected" || connection === "local" ? <Wifi size={15} /> : <WifiOff size={15} />}
          <div><strong>{connectionLabel}</strong><span>{!isTauri || settings.dataMode === "server" ? (isTauri ? settings.serverUrl : window.location.origin) : "本机抓取"}</span></div>
          {!isTauri || settings.dataMode === "server" ? <Cloud size={14} /> : <HardDrive size={14} />}
        </div>
        <div
          className="sidebar-resizer"
          role="separator"
          aria-label="调整左侧栏宽度"
          aria-orientation="vertical"
          aria-valuemin={MIN_SIDEBAR_WIDTH}
          aria-valuemax={MAX_SIDEBAR_WIDTH}
          aria-valuenow={sidebarWidth}
          tabIndex={0}
          title="拖拽调整左侧栏宽度，双击恢复默认"
          onPointerDown={beginSidebarResize}
          onPointerMove={moveSidebarResize}
          onPointerUp={endSidebarResize}
          onPointerCancel={endSidebarResize}
          onDoubleClick={() => setSidebarWidth(DEFAULT_SIDEBAR_WIDTH)}
          onKeyDown={resizeSidebarByKeyboard}
        />
      </aside>

      {creatorMenu && (settings.youtubeEnabled || creatorMenu.streamer.platform !== "youtube") ? (
        <div
          className="context-menu creator-context-menu"
          role="menu"
          aria-label={`${creatorMenu.streamer.name} 的操作菜单`}
          style={{ left: creatorMenu.x, top: creatorMenu.y }}
          onClick={(event) => event.stopPropagation()}
        >
          <div className="context-menu-label" title={creatorMenu.streamer.name}>
            {creatorMenu.streamer.name}
          </div>
          <button role="menuitem" onClick={() => void openCreatorLiveRoom(creatorMenu.streamer)}>
            <ExternalLink size={15} />进入直播间
          </button>
          <button role="menuitem" onClick={() => void openCreatorHomepage(creatorMenu.streamer)}>
            <UserRound size={15} />进入个人主页
          </button>
          <button
            role="menuitem"
            disabled={sidebarRefreshingId !== null}
            title={sidebarRefreshingId !== null ? "已有主播正在刷新" : undefined}
            onClick={() => void refreshSidebarCreator(creatorMenu.streamer)}
          >
            {sidebarRefreshingId === creatorMenu.streamer.id
              ? <LoaderCircle className="spin" size={15} />
              : <RefreshCw size={15} />}
            {sidebarRefreshingId === creatorMenu.streamer.id ? "刷新中…" : "刷新"}
          </button>
          <button
            className="danger"
            role="menuitem"
            disabled={sidebarRefreshingId === creatorMenu.streamer.id}
            title={sidebarRefreshingId === creatorMenu.streamer.id ? "请等待刷新完成后再删除" : undefined}
            onClick={() => void removeSidebarCreator(creatorMenu.streamer)}
          >
            <Trash2 size={15} />删除主播
          </button>
        </div>
      ) : null}

      <main className="main-content">
        {connectionError && (connection === "offline" || connection === "connecting") ? (
          <ErrorBanner message={connectionError} onRetry={() => {
            if (settings.dataMode === "server" && settings.serverUrl && serverToken) {
              void connectServer(settings.serverUrl, serverToken)
                .then((api) => refreshServerSettings(api))
                .catch((error) => showToast(explainError(error)));
            }
          }} />
        ) : null}
        <div hidden={page !== "schedule"}>
          <ScheduleView
            key={`schedule-source:${isTauri ? settings.dataMode : "server"}:${isTauri ? settings.serverUrl : window.location.origin}`}
            active={page === "schedule"}
            subscriptions={visibleSubscriptions}
            liveStatuses={visibleLiveStatuses}
            actions={scheduleActions}
            revision={scheduleRevision}
            onError={showToast}
          />
        </div>
        {page === "creators" ? (
          <CreatorsView subscriptions={visibleSubscriptions} refreshProgress={refreshProgress} liveStatsEnabled={settings.liveStatsEnabled} fertilityEnabled={settings.fertilityEnabled} youtubeEnabled={settings.youtubeEnabled ?? false} actions={creatorActions} onChanged={handleCreatorChanged} onOpenLiveRoom={openCreatorLiveRoom} onError={showToast} />
        ) : page === "settings" ? (
          <SettingsView
            ref={settingsViewRef}
            settings={settings}
            appearance={appearance}
            onAppearanceChange={setAppearance}
            secrets={secrets}
            llmApiKeyHint={llmApiKeyHint}
            biliConfigured={(!isTauri || settings.dataMode === "server") ? serverBiliConfigured : secrets.biliConfigured}
            autostart={autostart}
            tauri={isTauri}
            serverMode={!isTauri || settings.dataMode === "server"}
            sync={syncState}
            devices={syncDevices}
            devicesLoading={syncDevicesLoading}
            devicesError={syncDevicesError}
            activeJobs={Object.values(activeJobs)}
            onRefreshDevices={() => void refreshSyncDevices()}
            onResync={() => {
              if (resyncInFlightRef.current) {
                return;
              }
              const epoch = connectionEpochRef.current;
              if (isTauri && localApi) {
                const serverUrl = settingsRef.current.serverUrl.trim();
                const token = serverTokenRef.current;
                if (!serverUrl || !token) {
                  showToast("服务器连接信息不完整");
                  return;
                }
                resyncInFlightRef.current = true;
                connectionRef.current = "connecting";
                setConnection("connecting");
                void (async () => {
                  try {
                    const result = await localApi.rebuildRemoteSync(serverUrl, token);
                    if (epoch !== connectionEpochRef.current) {
                      await stopRemoteMonitorGeneration(result.monitorGeneration);
                      return;
                    }
                    remoteMonitorGenerationRef.current = result.monitorGeneration;
                    await refreshActiveData();
                    setScheduleRevision((value) => value + 1);
                    showToast("同步缓存已重建");
                  } catch (error) {
                    if (epoch === connectionEpochRef.current) {
                      connectionRef.current = "offline";
                      setConnection("offline");
                      showToast(explainError(error));
                    }
                  } finally {
                    resyncInFlightRef.current = false;
                  }
                })();
                return;
              }
              const engine = syncEngineRef.current;
              if (!engine) {
                showToast("同步引擎尚未就绪");
                return;
              }
              resyncInFlightRef.current = true;
              void engine
                .forceResync()
                .catch((error) => showToast(explainError(error)))
                .finally(() => {
                  resyncInFlightRef.current = false;
                });
            }}
            onImportServer={isTauri ? () => {
              void openServerImport().catch((error) => showToast(explainError(error)));
            } : undefined}
            onMigrateToServer={isTauri ? () => {
              void openClientMigrationToServer().catch((error) => showToast(explainError(error)));
            } : undefined}
            onDeleteDevice={deleteSyncDeviceLog}
            actions={{
              save: saveAllSettings,
              testServer: testServerConnection,
              serverUpdate: async () => {
                if (!serverApi) throw new Error("服务器未连接");
                return serverApi.serverUpdate();
              },
              saveServerUpdateSettings: async (updateSettings: ServerUpdateSettings) => {
                if (!serverApi) throw new Error("服务器未连接");
                return serverApi.saveServerUpdateSettings(updateSettings);
              },
              checkServerUpdate: async () => {
                if (!serverApi) throw new Error("服务器未连接");
                return serverApi.checkServerUpdate();
              },
              installServerUpdate: async () => {
                if (!serverApi) throw new Error("服务器未连接");
                return serverApi.installServerUpdate();
              },
              testProxy,
              testLlm,
              listLlmModels,
              testNotification: sendTestNotification,
              status: async () => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.youtubeStatus();
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.youtubeStatus();
              },
              saveApiKey: async (apiKey: string) => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.saveYoutubeApiKey(apiKey);
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.setYoutubeApiKey(apiKey);
              },
              testApiKey: async () => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.testYoutubeApiKey();
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.testYoutubeApiKey();
              },
              clearApiKey: async () => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.clearYoutubeApiKey();
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.clearYoutubeApiKey();
              },
              startOAuth: async () => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.startYoutubeOAuth();
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.startYoutubeOAuth();
              },
              clearOAuth: async () => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.clearYoutubeOAuth();
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.clearYoutubeOAuth();
              },
              startWebLogin: async () => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.startYoutubeWebLogin();
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.startYoutubeWebLogin();
              },
              saveWebSession: async (cookie: string) => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.saveYoutubeWebSession(cookie);
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.saveYoutubeWebSession(cookie);
              },
              clearWebSession: async () => {
                requireYoutubeEnabled();
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) throw new Error("服务器未连接");
                  return serverApi.clearYoutubeWebSession();
                }
                if (!localApi) throw new Error("桌面 API 未初始化");
                return localApi.clearYoutubeWebSession();
              },
              openBiliBrowserLogin: async () => {
                if (!localApi) {
                  throw new Error("桌面 API 未初始化");
                }
                await localApi.openBiliBrowserLogin();
              },
              startBiliQrLogin: async () => {
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) {
                    throw new Error("服务器未连接");
                  }
                  const login = await serverApi.startServerBiliQrLogin();
                  return {
                    qrcodeKey: login.sessionId,
                    url: login.url,
                    expiresInSeconds: login.expiresInSeconds,
                  };
                }
                if (!localApi) {
                  throw new Error("桌面 API 未初始化");
                }
                return localApi.startBiliQrLogin();
              },
              pollBiliQrLogin: async (qrcodeKey) => {
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) {
                    throw new Error("服务器未连接");
                  }
                  const result = await serverApi.pollServerBiliQrLogin(qrcodeKey);
                  if (result.configured) {
                    setServerBiliConfigured(true);
                  }
                  return {
                    status: result.status,
                    message: result.message,
                    cookieSaved: result.configured,
                  };
                }
                if (!localApi) {
                  throw new Error("桌面 API 未初始化");
                }
                const result = await localApi.pollBiliQrLogin(qrcodeKey);
                if (result.cookieSaved) {
                  setSecrets(await localApi.secretStatus());
                }
                return result;
              },
              clearBiliLogin: async () => {
                if (!isTauri || settings.dataMode === "server") {
                  if (!serverApi) {
                    throw new Error("服务器未连接");
                  }
                  await serverApi.logoutServerBili();
                  setServerBiliConfigured(false);
                  return;
                }
                if (!localApi) {
                  throw new Error("桌面 API 未初始化");
                }
                const status = await localApi.clearBiliLogin();
                setSecrets(status);
              },
            }}
            onError={showToast}
          />
        ) : null}
      </main>

      {toast ? <div className="toast" role="status">{toast}</div> : null}

      {pendingServerSetup ? (
        <Modal
          title="初始化服务端数据"
          onClose={migrationBusy ? () => undefined : deferServerSetup}
          wide
          footer={
            <>
              <button className="button secondary" disabled={migrationBusy} onClick={deferServerSetup}>稍后</button>
              <button
                className="button secondary"
                disabled={migrationBusy || !isTauri}
                onClick={() => void prepareClientMigration()}
              >
                迁移本机数据到服务端
              </button>
              <button className="button primary" disabled={migrationBusy} onClick={useServerDataForSetup}>
                使用服务端数据
              </button>
            </>
          }
        >
          <p className="import-copy">请选择这台设备首次接入服务器时的数据来源。</p>
          <div className="import-stats">
            <strong>{subscriptions.length}<span>服务端订阅</span></strong>
            <strong>{isTauri ? "可迁移" : "仅在线"}<span>本机数据</span></strong>
          </div>
          <p className="form-hint">
            使用服务端数据会以服务器为唯一数据源；迁移本机数据会先由服务端生成冲突预览，确认后才写入服务器。
          </p>
        </Modal>
      ) : null}

      {pendingClientMigration ? (
        <Modal
          title="迁移本机数据到服务端"
          onClose={migrationBusy ? () => undefined : () => setPendingClientMigration(null)}
          wide
          footer={
            <>
              <button className="button secondary" disabled={migrationBusy} onClick={() => setPendingClientMigration(null)}>取消</button>
              <button
                className="button secondary"
                disabled={migrationBusy}
                onClick={() => void applyClientMigration("server")}
              >
                保留服务器，只导入缺失项
              </button>
              <button
                className="button primary"
                disabled={migrationBusy}
                onClick={() => void applyClientMigration("client")}
              >
                客户端覆盖服务器
              </button>
            </>
          }
        >
          <p className="import-copy">服务端已生成迁移预览，提交前服务器业务数据不会改变。</p>
          <div className="import-stats">
            <strong>{pendingClientMigration.snapshot.subscriptions.length}<span>客户端订阅</span></strong>
            <strong>{pendingClientMigration.snapshot.schedules.length}<span>客户端日程</span></strong>
            <strong>{pendingClientMigration.preview.conflicts.length}<span>冲突</span></strong>
          </div>
          <p>检测到 {pendingClientMigration.preview.conflicts.length} 项与服务器冲突的数据。</p>
          {pendingClientMigration.preview.conflicts.length > 0 ? (
            <details>
              <summary>展开冲突明细</summary>
              {pendingClientMigration.preview.conflicts.map((conflict) => (
                <div key={conflict.entity + ":" + conflict.remoteId}>
                  <strong>{conflict.label}</strong>
                  <p>客户端：{JSON.stringify(conflict.local)}</p>
                  <p>服务器：{JSON.stringify(conflict.remote)}</p>
                </div>
              ))}
            </details>
          ) : null}
          <p className="form-hint">迁移完成后所有设备会收到 data_imported 事件并重新拉取服务端快照。</p>
        </Modal>
      ) : null}

      {pendingImport ? (
        <Modal
          title="用服务端数据覆盖本地"
          onClose={importBusy ? () => undefined : () => setPendingImport(null)}
          wide
          footer={
            <>
              <button className="button secondary" disabled={importBusy} onClick={() => void completeImport("client")}>保留客户端</button>
              <button className="button secondary" disabled={importBusy} onClick={() => void completeImport("skip")}>只导入不冲突项</button>
              <button className="button primary" disabled={importBusy} onClick={() => void completeImport("server")}>服务器覆盖客户端</button>
            </>
          }
        >
          <p className="import-copy">服务器不会被修改。本次快照包含：</p>
          <div className="import-stats">
            <strong>{pendingImport.snapshot.subscriptions.length}<span>订阅</span></strong>
            <strong>{pendingImport.snapshot.schedules.length}<span>日程</span></strong>
            <strong>{pendingImport.snapshot.latestEventSeq}<span>事件序号</span></strong>
          </div>
          <p>检测到 {pendingImport.preview.conflicts.length} 项冲突。</p>
          {pendingImport.preview.conflicts.length > 0 ? (
            <details>
              <summary>展开冲突明细</summary>
              {pendingImport.preview.conflicts.map((conflict) => (
                <div key={`${conflict.entity}:${conflict.localId}`}>
                  <strong>{conflict.label}</strong>
                  <p>客户端：{JSON.stringify(conflict.local)}</p>
                  <p>服务器：{JSON.stringify(conflict.remote)}</p>
                </div>
              ))}
            </details>
          ) : null}
          <p className="form-hint">导入时按主播 MID、官号 MID 和日程“主播 + 开始时间 + 标题”去重。冲突策略会统一应用到本次导入。</p>
        </Modal>
      ) : null}
    </div>
  );
}

function LoginGate({
  error,
  onSubmit,
}: {
  error: string | null;
  onSubmit: (token: string) => Promise<void>;
}) {
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <div className="login-shell">
      <div className="login-card">
        <div className="brand-mark large"><BrandMark size={40} /></div>
        <p className="eyebrow">OSHICAL SERVER</p>
        <h1>连接推历 OshiCal 服务器</h1>
        <p>Web 管理界面需要 Bearer Token。令牌只保存在当前页面内存中，刷新后需要重新输入。</p>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            setBusy(true);
            void onSubmit(token).finally(() => setBusy(false));
          }}
        >
          <label><span>访问令牌</span><input type="password" autoFocus value={token} onChange={(event) => setToken(event.target.value)} /></label>
          {error ? <ErrorBanner message={error} /> : null}
          <button className="button primary wide" disabled={busy || !token.trim()}>{busy ? "连接中…" : "连接服务器"}</button>
        </form>
      </div>
    </div>
  );
}
