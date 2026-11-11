import { forwardRef, useEffect, useImperativeHandle, useRef, useState } from "react";
import QRCode from "qrcode";
import {
  Activity,
  CheckCircle2,
  Clock3,
  Cloud,
  Download,
  ExternalLink,
  Globe2,
  HardDrive,
  KeyRound,
  ListChecks,
  LoaderCircle,
  LogOut,
  PlugZap,
  QrCode,
  RefreshCw,
  Save,
  Search,
  Target,
} from "lucide-react";
import type {
  AppSettings,
  BiliQrLoginPoll,
  BiliQrLoginStart,
  LlmReasoningEffort,
  LlmTestInput,
  LlmTestReport,
  LlmDiscoveryInput,
  ProxyTestReport,
  SecretStatus,
  ServerUpdateInfo,
  ServerUpdateSettings,
  DataMode,
  UpdateChannel,
} from "../types";
import { LLM_PARALLELISM_MAX, LLM_PARALLELISM_MIN } from "../types";
import type { SecretInput } from "../lib/secret-types";
import type { AppearanceSettings } from "../lib/appearance";
import { explainError, openExternalUrl } from "../lib/api";
import {
  readClientGithubMirror,
  saveClientGithubMirror,
} from "../lib/github-mirror";
import { useStickyHeader } from "../lib/sticky-header";
import {
  mergeServerSettings,
  normalizeLlmParallelism,
  normalizeLlmReasoningEffort,
} from "../lib/sync";
import {
  checkForUpdate,
  currentAppVersion,
  downloadAndInstallUpdate,
  formatBytes,
  readUpdateChannel,
  relaunchApplication,
  saveUpdateChannel,
  type AvailableUpdate,
  type UpdateProgress,
} from "../lib/updater";
import type { DeviceInfo, JobProgress, SyncUiState } from "../lib/sync";
import DeviceSyncPanel from "./DeviceSyncPanel";
import AppearanceSettingsPanel from "./AppearanceSettings";
import { ErrorBanner, Modal, Toggle } from "./ui";

interface SettingsActions {
  save: (settings: AppSettings, secrets: SecretInput, autostart: boolean) => Promise<AppSettings>;
  testServer: (url: string, token: string) => Promise<string>;
  serverUpdate: () => Promise<ServerUpdateInfo>;
  saveServerUpdateSettings: (settings: ServerUpdateSettings) => Promise<ServerUpdateInfo>;
  checkServerUpdate: () => Promise<ServerUpdateInfo>;
  installServerUpdate: () => Promise<ServerUpdateInfo>;
  testProxy: (proxyPort: number, mode: DataMode) => Promise<ProxyTestReport>;
  testLlm: (input: LlmTestInput) => Promise<LlmTestReport>;
  listLlmModels: (input: LlmDiscoveryInput) => Promise<string[]>;
  testNotification: () => Promise<void>;
  openBiliBrowserLogin: () => Promise<void>;
  startBiliQrLogin: () => Promise<BiliQrLoginStart>;
  pollBiliQrLogin: (qrcodeKey: string) => Promise<BiliQrLoginPoll>;
  clearBiliLogin: () => Promise<void>;
}

export interface SettingsViewHandle {
  requestLeave: () => Promise<boolean>;
}

function settingsEqual(left: AppSettings, right: AppSettings): boolean {
  return (
    left.dataMode === right.dataMode &&
    left.serverUrl === right.serverUrl &&
    left.livePollSeconds === right.livePollSeconds &&
    left.dynamicPollSeconds === right.dynamicPollSeconds &&
    left.autoAnalyzeImages === right.autoAnalyzeImages &&
    left.notificationsEnabled === right.notificationsEnabled &&
    left.liveStatsEnabled === right.liveStatsEnabled &&
    left.fertilityEnabled === right.fertilityEnabled &&
    left.llmBaseUrl === right.llmBaseUrl &&
    left.llmModel === right.llmModel &&
    left.llmPromptVersion === right.llmPromptVersion &&
    (left.llmReasoningEffort ?? "auto") === (right.llmReasoningEffort ?? "auto") &&
    (left.llmParallelism ?? LLM_PARALLELISM_MIN) === (right.llmParallelism ?? LLM_PARALLELISM_MIN) &&
    left.proxyPort === right.proxyPort
  );
}

const LLM_REASONING_OPTIONS: ReadonlyArray<{ value: LlmReasoningEffort; label: string }> = [
  { value: "auto", label: "关闭" },
  { value: "low", label: "低" },
  { value: "medium", label: "中" },
  { value: "high", label: "高" },
];

function serverUpdateSettingsFromInfo(info: ServerUpdateInfo): ServerUpdateSettings {
  return {
    channel: info.channel,
    autoCheckEnabled: info.autoCheckEnabled,
    autoUpdateEnabled: info.autoUpdateEnabled,
    updateWindowStart: info.updateWindowStart,
    updateWindowEnd: info.updateWindowEnd,
    githubMirrorUrl: info.githubMirrorUrl,
  };
}

function serverUpdateAutoSettingsEqual(left: ServerUpdateSettings, right: ServerUpdateSettings): boolean {
  return left.autoCheckEnabled === right.autoCheckEnabled
    && left.autoUpdateEnabled === right.autoUpdateEnabled
    && left.updateWindowStart === right.updateWindowStart
    && left.updateWindowEnd === right.updateWindowEnd;
}

function serverUpdateSettingsEqual(left: ServerUpdateSettings, right: ServerUpdateSettings): boolean {
  return left.channel === right.channel
    && serverUpdateAutoSettingsEqual(left, right)
    && left.githubMirrorUrl === right.githubMirrorUrl;
}

interface GithubMirrorFieldProps {
  label: string;
  ariaLabel: string;
  value: string;
  dirty?: boolean;
  saving?: boolean;
  error?: string | null;
  disabled?: boolean;
  onChange: (value: string) => void;
  onClear: () => void;
  onSave?: (value: string) => void;
}

/**
 * GitHub 镜像加速子区块：嵌在「服务器更新」和「软件更新」卡片内部。
 * 服务端随「保存更新设置」一起提交（不传 onSave）；客户端需要立即落盘本机（传 onSave）。
 */
function GithubMirrorField({
  label,
  ariaLabel,
  value,
  dirty = false,
  saving = false,
  error = null,
  disabled = false,
  onChange,
  onClear,
  onSave,
}: GithubMirrorFieldProps) {
  const busy = saving || disabled;
  return (
    <div className="update-mirror-block">
      <div className="update-mirror-head">
        <strong>{label}</strong>
        {dirty ? <em>未保存</em> : null}
      </div>
      <div className={`github-mirror-input-row${onSave ? "" : " no-save"}`}>
        <input
          value={value}
          onChange={(event) => onChange(event.target.value)}
          placeholder="https://gh-proxy.com/"
          aria-label={ariaLabel}
          disabled={busy}
        />
        {onSave ? (
          <button className="button primary" onClick={() => onSave(value)} disabled={busy}>
            {saving ? <LoaderCircle className="spin" size={16} /> : <Save size={16} />}
            保存
          </button>
        ) : null}
        <button className="button secondary" onClick={onClear} disabled={busy || !value.trim()}>
          清除
        </button>
      </div>
      {error ? <ErrorBanner message={error} /> : null}
      <div className="github-mirror-presets">
        <span>常用</span>
        {["https://gh-proxy.com/", "https://gh-proxy.org/"].map((preset) => (
          <button type="button" key={preset} onClick={() => onChange(preset)} disabled={busy}>
            {preset}
          </button>
        ))}
      </div>
    </div>
  );
}

interface SettingsViewProps {
  settings: AppSettings;
  appearance: AppearanceSettings;
  onAppearanceChange: (appearance: AppearanceSettings) => void;
  secrets: SecretStatus;
  llmApiKeyHint: string;
  biliConfigured: boolean;
  autostart: boolean;
  tauri: boolean;
  serverMode: boolean;
  sync: SyncUiState;
  devices: DeviceInfo[];
  devicesLoading: boolean;
  devicesError: string | null;
  activeJobs: JobProgress[];
  onRefreshDevices: () => void;
  onResync: () => void;
  onImportServer?: () => void;
  onMigrateToServer?: () => void;
  onDeleteDevice: (device: DeviceInfo) => Promise<void>;
  actions: SettingsActions;
  onError: (message: string) => void;
}

const SettingsView = forwardRef<SettingsViewHandle, SettingsViewProps>(function SettingsView({
  settings,
  appearance,
  onAppearanceChange,
  secrets,
  llmApiKeyHint,
  biliConfigured,
  autostart,
  tauri,
  serverMode,
  sync,
  devices,
  devicesLoading,
  devicesError,
  activeJobs,
  onRefreshDevices,
  onResync,
  onImportServer,
  onMigrateToServer,
  onDeleteDevice,
  actions,
  onError,
}, ref) {
  const [draft, setDraft] = useState<AppSettings>(settings);
  const [savedDraft, setSavedDraft] = useState<AppSettings>(settings);
  const [savedAutoLaunch, setSavedAutoLaunch] = useState(autostart);
  const [llmApiKey, setLlmApiKey] = useState("");
  const [biliCookie, setBiliCookie] = useState("");
  const [serverToken, setServerToken] = useState("");
  const [autoLaunch, setAutoLaunch] = useState(autostart);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [proxyTesting, setProxyTesting] = useState(false);
  const [proxyReport, setProxyReport] = useState<ProxyTestReport | null>(null);
  const [llmTesting, setLlmTesting] = useState(false);
  const [llmReport, setLlmReport] = useState<LlmTestReport | null>(null);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelsOpen, setModelsOpen] = useState(false);
  const [models, setModels] = useState<string[]>([]);
  const [modelSearch, setModelSearch] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [connectionText, setConnectionText] = useState<string | null>(null);
  const [browserLoginOpening, setBrowserLoginOpening] = useState(false);
  const [browserLoginHint, setBrowserLoginHint] = useState<string | null>(null);
  const [qrOpen, setQrOpen] = useState(false);
  const [qrLoading, setQrLoading] = useState(false);
  const [qrSession, setQrSession] = useState<{ key: string; dataUrl: string } | null>(null);
  const [qrStatus, setQrStatus] = useState<"waiting" | "scanned" | "confirmed" | "expired" | "error">("waiting");
  const [qrMessage, setQrMessage] = useState("等待手机扫码");
  const [leavePromptOpen, setLeavePromptOpen] = useState(false);
  const [leaveSaving, setLeaveSaving] = useState(false);
  const [appVersion, setAppVersion] = useState("");
  const [updateChannel, setUpdateChannel] = useState<UpdateChannel>(readUpdateChannel);
  const [updateChecking, setUpdateChecking] = useState(false);
  const [updateInfo, setUpdateInfo] = useState<AvailableUpdate | null>(null);
  const [updateStatus, setUpdateStatus] = useState<string | null>(null);
  const [updateInstalling, setUpdateInstalling] = useState(false);
  const [updateInstalled, setUpdateInstalled] = useState(false);
  const [updateProgress, setUpdateProgress] = useState<UpdateProgress>({ downloaded: 0, total: null });
  const [serverUpdate, setServerUpdate] = useState<ServerUpdateInfo | null>(null);
  const [serverUpdateDraft, setServerUpdateDraft] = useState<ServerUpdateSettings>({
    channel: "stable",
    autoCheckEnabled: true,
    autoUpdateEnabled: false,
    updateWindowStart: "03:00",
    updateWindowEnd: "05:00",
    githubMirrorUrl: "",
  });
  const [serverUpdateLoading, setServerUpdateLoading] = useState<"load" | "save" | "check" | "install" | null>(null);
  const [serverUpdateError, setServerUpdateError] = useState<string | null>(null);
  const [githubMirrorDraft, setGithubMirrorDraft] = useState(readClientGithubMirror);
  const [githubMirrorEffective, setGithubMirrorEffective] = useState(readClientGithubMirror);
  const [githubMirrorSaving, setGithubMirrorSaving] = useState(false);
  const [githubMirrorError, setGithubMirrorError] = useState<string | null>(null);
  const [bloodhoundOpen, setBloodhoundOpen] = useState(false);
  const [bloodhoundBusy, setBloodhoundBusy] = useState(false);
  const { stickyRef, stickyPinned } = useStickyHeader<HTMLDivElement>();
  const bloodhoundBusyRef = useRef(false);
  const serverUpdateDirtyRef = useRef(false);
  const serverUpdateRef = useRef<ServerUpdateInfo | null>(serverUpdate);
  const serverUpdateDraftRef = useRef(serverUpdateDraft);
  const serverUpdateActionRef = useRef<"save" | "check" | "install" | null>(null);
  const actionsRef = useRef(actions);
  const draftRef = useRef(draft);
  const savedDraftRef = useRef(savedDraft);
  const savingRef = useRef(false);
  const formRevisionRef = useRef(0);
  const connectionTestSeq = useRef(0);
  const connectionTestingRef = useRef(false);
  const llmTestSeq = useRef(0);
  const llmTestingRef = useRef(false);
  const modelDiscoverySeq = useRef(0);
  const modelsLoadingRef = useRef(false);
  const proxyTestSeq = useRef(0);
  const proxyTestingRef = useRef(false);
  const updateActionRef = useRef<"check" | "install" | null>(null);
  const browserLoginBusyRef = useRef(false);
  const biliLogoutBusyRef = useRef(false);
  const qrStartBusyRef = useRef(false);
  const githubMirrorDraftRef = useRef(githubMirrorDraft);
  const githubMirrorEffectiveRef = useRef(githubMirrorEffective);
  const githubMirrorSavingRef = useRef(false);
  const qrSessionRef = useRef<string | null>(null);
  const qrGenerationRef = useRef(0);
  const qrCloseTimerRef = useRef<number | null>(null);
  const wasBiliConfigured = useRef(biliConfigured);
  const leavePromptOpenRef = useRef(false);
  const pendingLeaveResolveRef = useRef<((allowed: boolean) => void) | null>(null);
  const leaveSavingRef = useRef(false);
  const dirtyRef = useRef(false);
  draftRef.current = draft;
  savedDraftRef.current = savedDraft;
  serverUpdateRef.current = serverUpdate;
  serverUpdateDraftRef.current = serverUpdateDraft;
  githubMirrorDraftRef.current = githubMirrorDraft;
  githubMirrorEffectiveRef.current = githubMirrorEffective;
  const clientMirrorDirty = tauri && githubMirrorDraft !== githubMirrorEffective;
  const serverUpdateDirty = serverMode && (
    serverUpdateDirtyRef.current
    || (serverUpdate !== null
      && !serverUpdateSettingsEqual(serverUpdateDraft, serverUpdateSettingsFromInfo(serverUpdate)))
  );
  serverUpdateDirtyRef.current = serverUpdateDirty;
  const isDirty =
    !settingsEqual(draft, savedDraft) ||
    autoLaunch !== savedAutoLaunch ||
    Boolean(llmApiKey.trim() || biliCookie.trim() || serverToken.trim()) ||
    clientMirrorDirty ||
    serverUpdateDirty;

  useEffect(() => {
    if (!wasBiliConfigured.current && biliConfigured) {
      setBrowserLoginHint(
        serverMode
          ? "服务器 B 站登录成功，Cookie 已加密保存在服务端。"
          : "登录成功，B 站 Cookie 已自动保存。",
      );
    }
    wasBiliConfigured.current = biliConfigured;
  }, [biliConfigured, serverMode]);

  useEffect(() => {
    dirtyRef.current = isDirty;
    if (isDirty) {
      setSaved(false);
    }
  }, [isDirty]);

  useEffect(() => {
    if (
      updateStatus !== "已切换到测试版渠道，将同时检查测试版和正式版中更高的版本。"
      && updateStatus !== "已切换到正式版渠道，仅检查正式版更新。"
    ) {
      return;
    }
    const timer = window.setTimeout(() => {
      setUpdateStatus((current) => (current === updateStatus ? null : current));
    }, 3000);
    return () => window.clearTimeout(timer);
  }, [updateStatus]);

  useEffect(() => {
    if (!saved) {
      return;
    }
    const timer = window.setTimeout(() => setSaved(false), 3000);
    return () => window.clearTimeout(timer);
  }, [saved]);

  useEffect(() => {
    const merged = mergeServerSettings(settings, settings);
    const previousSaved = savedDraftRef.current;
    // Background refreshes update fields the user has not touched while
    // preserving edits in progress. A whole-draft replacement here can submit
    // stale values back over another device's newer settings.
    setDraft((current) => {
      if (!dirtyRef.current) {
        return merged;
      }
      const next = { ...current };
      const mergeUntouched = <K extends keyof typeof next>(key: K) => {
        if (current[key] === previousSaved[key]) {
          next[key] = merged[key];
        }
      };
      for (const key of [
        "livePollSeconds",
        "dynamicPollSeconds",
        "autoAnalyzeImages",
        "liveStatsEnabled",
        "fertilityEnabled",
        "llmBaseUrl",
        "llmModel",
        "llmPromptVersion",
        "llmReasoningEffort",
        "llmParallelism",
        "proxyPort",
      ] as const) {
        mergeUntouched(key);
      }
      return next;
    });
    savedDraftRef.current = merged;
    setSavedDraft(merged);
  }, [
    settings.autoAnalyzeImages,
    settings.dynamicPollSeconds,
    settings.livePollSeconds,
    settings.liveStatsEnabled,
    settings.fertilityEnabled,
    settings.llmBaseUrl,
    settings.llmModel,
    settings.llmPromptVersion,
    settings.llmReasoningEffort,
    settings.llmParallelism,
    settings.proxyPort,
  ]);

  useEffect(() => {
    if (!dirtyRef.current) {
      setAutoLaunch(autostart);
      setSavedAutoLaunch(autostart);
    }
  }, [autostart]);

  useEffect(() => {
    actionsRef.current = actions;
  }, [actions]);

  useEffect(() => {
    if (!serverMode) {
      return;
    }
    let disposed = false;
    let loadInFlight = false;
    const apply = (value: ServerUpdateInfo) => {
      if (disposed) {
        return;
      }
      serverUpdateRef.current = value;
      setServerUpdate(value);
      if (!serverUpdateDirtyRef.current) {
        const nextDraft = serverUpdateSettingsFromInfo(value);
        serverUpdateDraftRef.current = nextDraft;
        setServerUpdateDraft(nextDraft);
      }
    };
    const load = async () => {
      if (loadInFlight) {
        return;
      }
      loadInFlight = true;
      try {
        const value = await actionsRef.current.serverUpdate();
        apply(value);
        if (!disposed && serverUpdateActionRef.current === null) {
          setServerUpdateError(null);
        }
      } catch (updateError) {
        if (!disposed) {
          setServerUpdateError(explainError(updateError));
        }
      } finally {
        loadInFlight = false;
      }
    };
    void load();
    const timer = window.setInterval(() => void load(), 10_000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [serverMode]);

  useEffect(() => {
    if (!tauri) {
      return;
    }
    let disposed = false;
    void currentAppVersion()
      .then((version) => {
        if (!disposed) {
          setAppVersion(version);
        }
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
    };
  }, [tauri]);

  useImperativeHandle(ref, () => ({
    requestLeave: () => {
      if (!isDirty) {
        return Promise.resolve(true);
      }
      if (leavePromptOpenRef.current) {
        return Promise.resolve(false);
      }
      leavePromptOpenRef.current = true;
      setError(null);
      setLeavePromptOpen(true);
      return new Promise<boolean>((resolve) => {
        pendingLeaveResolveRef.current = resolve;
      });
    },
  }), [isDirty]);

  useEffect(() => () => {
    qrSessionRef.current = null;
    qrGenerationRef.current += 1;
    if (qrCloseTimerRef.current !== null) {
      window.clearTimeout(qrCloseTimerRef.current);
      qrCloseTimerRef.current = null;
    }
    pendingLeaveResolveRef.current?.(false);
    pendingLeaveResolveRef.current = null;
  }, []);

  function settleLeavePrompt(allowed: boolean) {
    leavePromptOpenRef.current = false;
    setLeavePromptOpen(false);
    const resolve = pendingLeaveResolveRef.current;
    pendingLeaveResolveRef.current = null;
    resolve?.(allowed);
  }

  async function openBrowserLogin() {
    if (browserLoginBusyRef.current) {
      return;
    }
    browserLoginBusyRef.current = true;
    setBrowserLoginOpening(true);
    setBrowserLoginHint(null);
    try {
      await actions.openBiliBrowserLogin();
      setBrowserLoginHint("请在 B 站登录窗口完成登录；检测到 Cookie 后会自动保存并关闭窗口。");
    } catch (loginError) {
      const message = explainError(loginError);
      setError(message);
      onError(message);
    } finally {
      browserLoginBusyRef.current = false;
      setBrowserLoginOpening(false);
    }
  }

  function clearQrCloseTimer() {
    if (qrCloseTimerRef.current !== null) {
      window.clearTimeout(qrCloseTimerRef.current);
      qrCloseTimerRef.current = null;
    }
  }

  async function startQrLogin() {
    if (qrStartBusyRef.current) {
      return;
    }
    qrStartBusyRef.current = true;
    const generation = ++qrGenerationRef.current;
    clearQrCloseTimer();
    qrSessionRef.current = null;
    setQrLoading(true);
    setQrSession(null);
    setQrStatus("waiting");
    setQrMessage("正在生成二维码…");
    try {
      const login = await actions.startBiliQrLogin();
      const dataUrl = await QRCode.toDataURL(login.url, {
        width: 260,
        margin: 1,
        errorCorrectionLevel: "M",
        color: { dark: "#241b25", light: "#fffafc" },
      });
      if (qrGenerationRef.current !== generation) {
        return;
      }
      qrSessionRef.current = login.qrcodeKey;
      setQrSession({ key: login.qrcodeKey, dataUrl });
      setQrOpen(true);
      setQrStatus("waiting");
      setQrMessage("等待手机扫码");
      void pollQrLogin(login.qrcodeKey, generation);
    } catch (loginError) {
      if (qrGenerationRef.current !== generation) {
        return;
      }
      const message = explainError(loginError);
      setQrStatus("error");
      setQrMessage(message);
      setError(message);
      onError(message);
    } finally {
      qrStartBusyRef.current = false;
      if (qrGenerationRef.current === generation) {
        setQrLoading(false);
      }
    }
  }

  async function pollQrLogin(key: string, generation: number) {
    let consecutiveErrors = 0;
    while (qrSessionRef.current === key && qrGenerationRef.current === generation) {
      await new Promise((resolve) => window.setTimeout(resolve, 2000));
      if (qrSessionRef.current !== key || qrGenerationRef.current !== generation) {
        return;
      }
      try {
        const result = await actions.pollBiliQrLogin(key);
        if (qrSessionRef.current !== key || qrGenerationRef.current !== generation) {
          return;
        }
        setQrStatus(result.status);
        setQrMessage(result.message);
        consecutiveErrors = 0;
        if (result.status === "confirmed") {
          qrSessionRef.current = null;
          clearQrCloseTimer();
          qrCloseTimerRef.current = window.setTimeout(() => {
            qrCloseTimerRef.current = null;
            if (qrGenerationRef.current === generation) {
              setQrOpen(false);
            }
          }, 1200);
          return;
        }
        if (result.status === "expired") {
          qrSessionRef.current = null;
          return;
        }
      } catch (pollError) {
        if (qrSessionRef.current !== key || qrGenerationRef.current !== generation) {
          return;
        }
        consecutiveErrors += 1;
        setQrStatus("error");
        setQrMessage(explainError(pollError));
        if (consecutiveErrors >= 3) {
          qrSessionRef.current = null;
          return;
        }
      }
    }
  }

  function closeQrLogin() {
    qrGenerationRef.current += 1;
    qrSessionRef.current = null;
    clearQrCloseTimer();
    setQrOpen(false);
    setQrLoading(false);
  }

  async function logoutBili() {
    if (biliLogoutBusyRef.current) {
      return;
    }
    const message = serverMode
      ? "退出服务器 B 站登录并删除服务端保存的 Cookie？"
      : "退出 B 站登录并删除本机保存的 Cookie？";
    if (!window.confirm(message)) {
      return;
    }
    biliLogoutBusyRef.current = true;
    try {
      await actions.clearBiliLogin();
      setBiliCookie("");
      setBrowserLoginHint(
        serverMode ? "已退出服务器 B 站登录。" : "已退出 B 站登录，本机 Cookie 已删除。",
      );
    } catch (logoutError) {
      const message = explainError(logoutError);
      setError(message);
      onError(message);
    } finally {
      biliLogoutBusyRef.current = false;
    }
  }

  function markFormChanged() {
    formRevisionRef.current += 1;
  }

  function setMode(dataMode: DataMode) {
    markFormChanged();
    connectionTestSeq.current += 1;
    connectionTestingRef.current = false;
    setTesting(false);
    proxyTestSeq.current += 1;
    proxyTestingRef.current = false;
    setProxyTesting(false);
    setConnectionText(null);
    setProxyReport(null);
    setDraft((current) => ({ ...current, dataMode }));
    setSaved(false);
  }

  async function testConnection() {
    if (connectionTestingRef.current) {
      return;
    }
    const testedUrl = draftRef.current.serverUrl.trim();
    if (!testedUrl) {
      setError("请先填写服务器地址");
      return;
    }
    connectionTestingRef.current = true;
    const requestId = ++connectionTestSeq.current;
    const token = serverToken || "";
    setTesting(true);
    setConnectionText(null);
    setError(null);
    try {
      const result = await actions.testServer(testedUrl, token);
      if (
        connectionTestSeq.current === requestId
        && draftRef.current.serverUrl.trim() === testedUrl
      ) {
        setConnectionText(result);
      }
    } catch (testError) {
      if (
        connectionTestSeq.current === requestId
        && draftRef.current.serverUrl.trim() === testedUrl
      ) {
        const message = explainError(testError);
        setError(message);
        onError(message);
      }
    } finally {
      if (connectionTestSeq.current === requestId) {
        connectionTestingRef.current = false;
        setTesting(false);
      }
    }
  }

  async function testLlm() {
    if (llmTestingRef.current) {
      return;
    }
    const current = draftRef.current;
    if (!current.llmBaseUrl.trim() || !current.llmModel.trim()) {
      setError("请先填写日程模型的 Base URL 和模型名");
      return;
    }
    const requestId = ++llmTestSeq.current;
    const input = {
      baseUrl: current.llmBaseUrl.trim(),
      apiKey: llmApiKey.trim() || undefined,
      model: current.llmModel.trim(),
      promptVersion: current.llmPromptVersion,
      reasoningEffort: normalizeLlmReasoningEffort(current.llmReasoningEffort),
    };
    llmTestingRef.current = true;
    setLlmTesting(true);
    setError(null);
    setLlmReport(null);
    try {
      const report = await actions.testLlm(input);
      if (llmTestSeq.current === requestId) {
        setLlmReport(report);
      }
    } catch (testError) {
      if (llmTestSeq.current === requestId) {
        const message = explainError(testError);
        setError(message);
        onError(message);
      }
    } finally {
      if (llmTestSeq.current === requestId) {
        llmTestingRef.current = false;
        setLlmTesting(false);
      }
    }
  }

  async function testProxy() {
    if (proxyTestingRef.current) {
      return;
    }
    const current = draftRef.current;
    const port = current.proxyPort;
    if (port === null || !Number.isInteger(port) || port < 1 || port > 65535) {
      setError("请填写 1 到 65535 之间的本机代理端口");
      return;
    }
    const requestId = ++proxyTestSeq.current;
    proxyTestingRef.current = true;
    setProxyTesting(true);
    setError(null);
    setProxyReport(null);
    try {
      const report = await actions.testProxy(port, current.dataMode);
      if (proxyTestSeq.current === requestId) {
        setProxyReport(report);
      }
    } catch (testError) {
      if (proxyTestSeq.current === requestId) {
        const message = explainError(testError);
        setError(message);
        onError(message);
      }
    } finally {
      if (proxyTestSeq.current === requestId) {
        proxyTestingRef.current = false;
        setProxyTesting(false);
      }
    }
  }

  async function pullModels() {
    if (modelsLoadingRef.current) {
      return;
    }
    const current = draftRef.current;
    if (!current.llmBaseUrl.trim()) {
      setError("请先填写日程模型的 Base URL");
      return;
    }
    const requestId = ++modelDiscoverySeq.current;
    modelsLoadingRef.current = true;
    setModelsLoading(true);
    setError(null);
    try {
      const nextModels = await actions.listLlmModels({
        baseUrl: current.llmBaseUrl.trim(),
        apiKey: llmApiKey.trim() || undefined,
      });
      if (modelDiscoverySeq.current === requestId) {
        setModels(nextModels);
        setModelSearch("");
        setModelsOpen(true);
      }
    } catch (discoveryError) {
      if (modelDiscoverySeq.current === requestId) {
        const message = explainError(discoveryError);
        setError(message);
        onError(message);
      }
    } finally {
      if (modelDiscoverySeq.current === requestId) {
        modelsLoadingRef.current = false;
        setModelsLoading(false);
      }
    }
  }

  function changeReasoningEffort(value: LlmReasoningEffort) {
    markFormChanged();
    setLlmReport(null);
    setDraft((current) => ({ ...current, llmReasoningEffort: value }));
  }

  function changeParallelism(raw: string) {
    markFormChanged();
    const numeric = raw.trim() === "" ? LLM_PARALLELISM_MIN : Number(raw);
    const clamped = normalizeLlmParallelism(numeric, LLM_PARALLELISM_MIN);
    setDraft((current) => ({ ...current, llmParallelism: clamped }));
  }

  async function save(): Promise<boolean> {
    if (savingRef.current) {
      return false;
    }
    savingRef.current = true;
    const submittedRevision = formRevisionRef.current;
    const submittedLlmApiKey = llmApiKey.trim();
    const submittedBiliCookie = biliCookie.trim();
    const submittedServerToken = serverToken.trim();
    const submittedAutoLaunch = autoLaunch;
    const currentDraft = draftRef.current;
    // 并行个数就是同一端点并发请求数，保存前只做 1..=8 的规范化，不再按账号数裁剪。
    const submittedDraft: AppSettings = {
      ...currentDraft,
      llmParallelism: normalizeLlmParallelism(currentDraft.llmParallelism, LLM_PARALLELISM_MIN),
    };
    setSaving(true);
    setError(null);
    setSaved(false);
    try {
      const nextSettings = await actions.save(
        submittedDraft,
        {
          llmApiKey: submittedLlmApiKey || undefined,
          biliCookie: submittedBiliCookie || undefined,
          serverToken: submittedServerToken || undefined,
        },
        submittedAutoLaunch,
      );
      const formUnchanged = formRevisionRef.current === submittedRevision;
      savedDraftRef.current = nextSettings;
      setSavedDraft(nextSettings);
      setSavedAutoLaunch(submittedAutoLaunch);
      setLlmApiKey((current) => current.trim() === submittedLlmApiKey ? "" : current);
      setBiliCookie((current) => current.trim() === submittedBiliCookie ? "" : current);
      setServerToken((current) => current.trim() === submittedServerToken ? "" : current);
      if (formUnchanged) {
        draftRef.current = nextSettings;
        setDraft(nextSettings);
        setSaved(true);
      }
      return formUnchanged;
    } catch (saveError) {
      const message = explainError(saveError);
      setError(message);
      onError(message);
      return false;
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  }

  /**
   * 「月相」开关：开启必须经过确认弹窗，关闭立即生效。
   * 这里只用已保存的配置草稿提交单个字段，不会把手上的其他未保存编辑一起写下去。
   */
  async function setBloodhound(next: boolean): Promise<boolean> {
    if (bloodhoundBusyRef.current) {
      return false;
    }
    bloodhoundBusyRef.current = true;
    setBloodhoundBusy(true);
    setError(null);
    try {
      const base = savedDraftRef.current;
      const payload: AppSettings = { ...base, fertilityEnabled: next };
      const savedSettings = await actions.save(payload, {}, savedAutoLaunch);
      savedDraftRef.current = savedSettings;
      setSavedDraft(savedSettings);
      // 只有当用户没有在别处继续改动时才把草稿整体对齐，避免吞掉未保存编辑。
      setDraft((current) =>
        current.fertilityEnabled === base.fertilityEnabled
          ? { ...current, fertilityEnabled: savedSettings.fertilityEnabled }
          : current,
      );
      setSaved(true);
      return true;
    } catch (saveError) {
      const message = explainError(saveError);
      setError(message);
      onError(message);
      return false;
    } finally {
      bloodhoundBusyRef.current = false;
      setBloodhoundBusy(false);
    }
  }

  async function confirmBloodhound() {
    if (await setBloodhound(true)) {
      setBloodhoundOpen(false);
    }
  }

  async function openLaplaceSite() {
    try {
      await openExternalUrl("https://laplace.live/ovu");
    } catch (openError) {
      onError(explainError(openError));
    }
  }

  async function saveAndLeave() {    if (leaveSavingRef.current) {
      return;
    }
    leaveSavingRef.current = true;
    setLeaveSaving(true);
    try {
      const revision = formRevisionRef.current;
      if (!(await save())) {
        return;
      }
      if (formRevisionRef.current !== revision) {
        setError("设置已保存，但保存期间又产生了新修改，请确认后再次保存。");
        return;
      }
      const latestMirror = githubMirrorDraftRef.current;
      if (
        latestMirror !== githubMirrorEffectiveRef.current
        && !(await saveGithubMirror(latestMirror))
      ) {
        return;
      }
      const currentServerUpdate = serverUpdateRef.current;
      const latestServerDraft = serverUpdateDraftRef.current;
      const hasUnsavedServerUpdate = serverMode && (
        serverUpdateDirtyRef.current
        || (currentServerUpdate !== null
          && !serverUpdateSettingsEqual(
            latestServerDraft,
            serverUpdateSettingsFromInfo(currentServerUpdate),
          ))
      );
      if (hasUnsavedServerUpdate) {
        if (currentServerUpdate === null) {
          setError("服务器更新配置尚未加载完成，暂时无法保存并离开，请稍后重试。");
          return;
        }
        if (!(await saveServerUpdateSettings())) {
          return;
        }
      }
      if (formRevisionRef.current === revision) {
        settleLeavePrompt(true);
      }
    } finally {
      leaveSavingRef.current = false;
      setLeaveSaving(false);
    }
  }

  function applyServerUpdate(value: ServerUpdateInfo) {
    serverUpdateRef.current = value;
    setServerUpdate(value);
    if (!serverUpdateDirtyRef.current) {
      const nextDraft = serverUpdateSettingsFromInfo(value);
      serverUpdateDraftRef.current = nextDraft;
      setServerUpdateDraft(nextDraft);
    }
  }

  function updateServerUpdateDraft(
    updater: (current: ServerUpdateSettings) => ServerUpdateSettings,
  ) {
    setServerUpdateDraft((current) => {
      const next = updater(current);
      const saved = serverUpdateRef.current;
      serverUpdateDraftRef.current = next;
      serverUpdateDirtyRef.current = saved === null
        || !serverUpdateSettingsEqual(next, serverUpdateSettingsFromInfo(saved));
      return next;
    });
  }

  function beginServerUpdateAction(action: "save" | "check" | "install"): boolean {
    if (serverUpdateActionRef.current !== null) {
      return false;
    }
    serverUpdateActionRef.current = action;
    setServerUpdateLoading(action);
    return true;
  }

  function finishServerUpdateAction() {
    serverUpdateActionRef.current = null;
    setServerUpdateLoading(null);
  }

  async function saveGithubMirror(value: string): Promise<boolean> {
    if (githubMirrorSavingRef.current) {
      return false;
    }
    githubMirrorSavingRef.current = true;
    setGithubMirrorSaving(true);
    setGithubMirrorError(null);
    try {
      const normalized = saveClientGithubMirror(value);
      githubMirrorEffectiveRef.current = normalized;
      setGithubMirrorEffective(normalized);
      if (githubMirrorDraftRef.current === value) {
        githubMirrorDraftRef.current = normalized;
        setGithubMirrorDraft(normalized);
      }
      return true;
    } catch (mirrorError) {
      setGithubMirrorError(explainError(mirrorError));
      return false;
    } finally {
      githubMirrorSavingRef.current = false;
      setGithubMirrorSaving(false);
    }
  }

  async function saveServerUpdateSettings(): Promise<boolean> {
    if (!beginServerUpdateAction("save")) {
      return false;
    }
    setServerUpdateError(null);
    const submittedDraft = serverUpdateDraftRef.current;
    const previousChannel = serverUpdateRef.current?.channel;
    let savedValue: ServerUpdateInfo | null = null;
    try {
      const value = await actions.saveServerUpdateSettings(submittedDraft);
      savedValue = value;
      const latestDraft = serverUpdateDraftRef.current;
      const nextDraft = serverUpdateSettingsEqual(latestDraft, submittedDraft)
        ? serverUpdateSettingsFromInfo(value)
        : latestDraft;
      serverUpdateRef.current = value;
      serverUpdateDraftRef.current = nextDraft;
      serverUpdateDirtyRef.current = !serverUpdateSettingsEqual(
        nextDraft,
        serverUpdateSettingsFromInfo(value),
      );
      setServerUpdate(value);
      setServerUpdateDraft(nextDraft);
    } catch (updateError) {
      setServerUpdateError(explainError(updateError));
    } finally {
      finishServerUpdateAction();
    }
    if (
      savedValue
      && previousChannel === "beta"
      && submittedDraft.channel === "stable"
      && savedValue.updateAvailable
    ) {
      const confirmed = window.confirm(
        `已切换到正式版渠道，并获取到最新正式版 v${savedValue.latestVersion}（当前 v${savedValue.currentVersion}）。是否立即更新服务器？更新期间服务会短暂中断，系统会先备份数据，失败时自动回滚。如果正式版版本低于当前测试版，这一步会执行显式降级。`,
      );
      if (confirmed) {
        await installServerUpdate(true);
      }
    }
    return savedValue !== null;
  }

  async function checkServerUpdate() {
    if (!beginServerUpdateAction("check")) {
      return;
    }
    setServerUpdateError(null);
    try {
      applyServerUpdate(await actions.checkServerUpdate());
    } catch (updateError) {
      setServerUpdateError(explainError(updateError));
    } finally {
      finishServerUpdateAction();
    }
  }

  async function installServerUpdate(alreadyConfirmed = false) {
    if (
      !alreadyConfirmed
      && !window.confirm("确定立即更新服务器？更新期间服务会短暂中断，系统会先备份数据，失败时自动回滚。")
    ) {
      return;
    }
    if (!beginServerUpdateAction("install")) {
      return;
    }
    setServerUpdateError(null);
    try {
      applyServerUpdate(await actions.installServerUpdate());
    } catch (updateError) {
      setServerUpdateError(explainError(updateError));
    } finally {
      finishServerUpdateAction();
    }
  }

  async function checkUpdates(
    channel: UpdateChannel = updateChannel,
    promptForUpdate = false,
  ): Promise<AvailableUpdate | null> {
    if (updateActionRef.current !== null) {
      return null;
    }
    updateActionRef.current = "check";
    setUpdateChecking(true);
    setUpdateStatus(channel === "stable" && promptForUpdate ? "正在获取最新正式版…" : null);
    setUpdateInfo(null);
    setUpdateInstalled(false);
    setUpdateProgress({ downloaded: 0, total: null });
    let update: AvailableUpdate | null = null;
    try {
      update = await checkForUpdate(channel);
      setUpdateInfo(update);
      setUpdateStatus(update ? `发现新版本 v${update.version}` : "当前已是最新版本");
    } catch (updateError) {
      setUpdateStatus(`检查更新失败：${explainError(updateError)}`);
    } finally {
      updateActionRef.current = null;
      setUpdateChecking(false);
    }
    if (
      update
      && promptForUpdate
      && window.confirm(
        `已切换到正式版渠道，并获取到最新正式版 v${update.version}（当前 v${update.currentVersion}）。是否立即安装？如果正式版版本低于当前测试版，这一步会执行显式降级。`,
      )
    ) {
      await installUpdate(update, channel);
    }
    return update;
  }

  async function installUpdate(
    update: AvailableUpdate | null = updateInfo,
    channel: UpdateChannel = updateChannel,
  ) {
    if (!update || updateActionRef.current !== null) {
      return;
    }
    updateActionRef.current = "install";
    setUpdateInstalling(true);
    setUpdateInstalled(false);
    setUpdateProgress({ downloaded: 0, total: null });
    setUpdateStatus("正在下载更新…");
    try {
      await downloadAndInstallUpdate((progress) => {
        setUpdateProgress(progress);
        setUpdateStatus(
          progress.total
            ? `正在下载 ${formatBytes(progress.downloaded)} / ${formatBytes(progress.total)}`
            : `已下载 ${formatBytes(progress.downloaded)}`,
        );
      }, channel);
      setUpdateInstalled(true);
      setUpdateStatus("更新已安装，应用即将重启。");
    } catch (updateError) {
      setUpdateStatus(`安装更新失败：${explainError(updateError)}`);
    } finally {
      updateActionRef.current = null;
      setUpdateInstalling(false);
    }
  }

  async function switchUpdateChannel(channel: UpdateChannel) {
    if (updateChecking || updateInstalling || channel === updateChannel) {
      return;
    }
    const previousChannel = updateChannel;
    setUpdateChannel(saveUpdateChannel(channel));
    setUpdateInfo(null);
    setUpdateInstalled(false);
    setUpdateProgress({ downloaded: 0, total: null });
    if (channel === "stable" && previousChannel === "beta") {
      await checkUpdates(channel, true);
      return;
    }
    setUpdateStatus(
      channel === "beta"
        ? "已切换到测试版渠道，将同时检查测试版和正式版中更高的版本。"
        : "已切换到正式版渠道，仅检查正式版更新。",
    );
  }

  async function restartForUpdate() {
    try {
      await relaunchApplication();
    } catch (updateError) {
      setUpdateStatus(`重启应用失败：${explainError(updateError)}`);
    }
  }

  const visibleModels = models.filter((model) =>
    model.toLowerCase().includes(modelSearch.trim().toLowerCase()),
  );
  const llmReasoningEffort = normalizeLlmReasoningEffort(draft.llmReasoningEffort);
  const llmParallelism = normalizeLlmParallelism(draft.llmParallelism, LLM_PARALLELISM_MIN);
  const serverUpdateRunning = serverUpdate?.status === "running";
  const serverUpdateActive =
    serverUpdate?.status === "requested" ||
    serverUpdate?.status === "waiting_window" ||
    serverUpdateRunning;
  const serverUpdateMessage =
    serverUpdateRunning && serverUpdate?.message === "等待宿主机更新代理上线"
      ? "宿主机更新代理正在执行更新，心跳可能暂时中断"
      : serverUpdate?.message ?? "";
  const leaveBusy = saving || leaveSaving;
  const updatePercent =
    updateProgress.total && updateProgress.total > 0
      ? Math.min(100, (updateProgress.downloaded / updateProgress.total) * 100)
      : null;

  return (
    <section className="page settings-page" onChangeCapture={markFormChanged}>
      <div ref={stickyRef} className={`sticky-page-header settings-sticky ${stickyPinned ? "is-pinned" : ""}`}>
        <header className="page-header">
          <div>
            <h1 className="schedule-title">设置</h1>
          </div>
          <button className="button primary" onClick={() => void save()} disabled={saving}>
            <Save size={16} />
            {saving ? "保存中…" : "保存设置"}
          </button>
        </header>
      </div>

      {error ? <ErrorBanner message={error} /> : null}
      {saved ? <div className="success-banner"><CheckCircle2 size={16} />设置已保存</div> : null}

      <div className="settings-layout">
        <section className="settings-card span-2">
          <header><h2>运行模式</h2></header>
          {tauri ? (
            <div className="mode-grid">
              <button className={`mode-card ${draft.dataMode === "local" ? "active" : ""}`} onClick={() => setMode("local")}>
                <HardDrive size={24} />
                <strong>本地模式</strong>
                <span>桌面端直接抓取 B 站并调用日程模型。</span>
              </button>
              <button className={`mode-card ${draft.dataMode === "server" ? "active" : ""}`} onClick={() => setMode("server")}>
                <Cloud size={24} />
                <strong>服务器模式</strong>
                <span>由服务器抓取，通过 HTTP + WebSocket 同步。</span>
              </button>
            </div>
          ) : (
            <div className="browser-mode-note"><Cloud size={20} />当前为服务器 Web 管理界面。</div>
          )}
          {tauri && draft.dataMode === "server" ? (
            <div className="form-grid">
              <label>
                <span>服务器地址</span>
                <input
                  value={draft.serverUrl}
                  onChange={(event) => {
                    connectionTestSeq.current += 1;
                    connectionTestingRef.current = false;
                    setTesting(false);
                    setConnectionText(null);
                    setDraft((current) => ({ ...current, serverUrl: event.target.value }));
                  }}
                  placeholder="https://你的服务器:8787"
                />
              </label>
              <label>
                <span>访问令牌</span>
                <input
                  type="password"
                  autoComplete="off"
                  value={serverToken}
                  onChange={(event) => {
                    connectionTestSeq.current += 1;
                    connectionTestingRef.current = false;
                    setTesting(false);
                    setConnectionText(null);
                    setServerToken(event.target.value);
                  }}
                  placeholder={secrets.serverTokenConfigured ? "已配置，留空则沿用" : "Bearer Token"}
                />
              </label>
              <div className="inline-actions span-2">
                <button className="button secondary" onClick={() => void testConnection()} disabled={testing}>
                  <PlugZap size={16} />{testing ? "连接中…" : "测试连接"}
                </button>
                {connectionText ? <span className="connection-ok"><CheckCircle2 size={15} />{connectionText}</span> : null}
              </div>
            </div>
          ) : null}
        </section>

        {serverMode ? (
          <DeviceSyncPanel
            sync={sync}
            devices={devices}
            loading={devicesLoading}
            error={devicesError}
            activeJobs={activeJobs}
            onRefresh={onRefreshDevices}
            onResync={onResync}
            onImportServer={onImportServer}
            onMigrateToServer={onMigrateToServer}
            onDeleteDevice={onDeleteDevice}
          />
        ) : null}

        {serverMode ? (
          <section className="settings-card span-2 server-update-card">
            <header>
              <h2>服务器更新</h2>
              <span className="setting-scope server">服务器</span>
              <span
                className={
                  serverUpdateRunning
                    ? "update-agent-running"
                    : serverUpdate?.agentOnline
                      ? "update-agent-online"
                      : "update-agent-offline"
                }
              >
                {serverUpdateRunning ? "更新执行中" : serverUpdate?.agentOnline ? "更新代理在线" : "更新代理未在线"}
              </span>
              <div className="update-channel-control">
                <div className="segmented" role="group" aria-label="服务器更新渠道">
                  <button
                    type="button"
                    className={serverUpdateDraft.channel === "stable" ? "active" : undefined}
                    onClick={() => updateServerUpdateDraft((current) => ({ ...current, channel: "stable" }))}
                    disabled={serverUpdateLoading !== null || serverUpdateActive}
                  >
                    正式版
                  </button>
                  <button
                    type="button"
                    className={serverUpdateDraft.channel === "beta" ? "active" : undefined}
                    onClick={() => updateServerUpdateDraft((current) => ({ ...current, channel: "beta" }))}
                    disabled={serverUpdateLoading !== null || serverUpdateActive}
                  >
                    测试版
                  </button>
                </div>
              </div>
            </header>
            <div className="server-update-metrics">
              <div>
                <span>当前版本</span>
                <strong>{serverUpdate ? `v${serverUpdate.currentVersion}` : "读取中…"}</strong>
              </div>
              <div>
                <span>最新版本</span>
                <strong>{serverUpdate?.latestVersion ? `v${serverUpdate.latestVersion}` : "尚未检查"}</strong>
                {serverUpdate?.latestVersion && serverUpdate.latestChannel ? (
                  <small>{serverUpdate.latestChannel === "beta" ? "测试版" : "正式版"}</small>
                ) : null}
              </div>
              <div>
                <span>更新状态</span>
                <strong>{serverUpdateRunning ? "更新执行中" : serverUpdate?.updateAvailable ? "有可用更新" : serverUpdate ? "已是最新" : "读取中"}</strong>
              </div>
            </div>
            {serverUpdateError ? <ErrorBanner message={serverUpdateError} /> : null}
            <div className="settings-stack server-update-options">
              <Toggle
                checked={serverUpdateDraft.autoCheckEnabled}
                disabled={serverUpdateLoading !== null}
                onChange={(value) => {
                  updateServerUpdateDraft((current) => ({
                    ...current,
                    autoCheckEnabled: value,
                    autoUpdateEnabled: value ? current.autoUpdateEnabled : false,
                  }));
                }}
                label="自动检测服务器新版本"
              />
              <Toggle
                checked={serverUpdateDraft.autoUpdateEnabled}
                disabled={serverUpdateLoading !== null}
                onChange={(value) => {
                  updateServerUpdateDraft((current) => ({
                    ...current,
                    autoUpdateEnabled: value,
                    autoCheckEnabled: value ? true : current.autoCheckEnabled,
                  }));
                }}
                label="在指定时间段自动更新"
              />
              <div className="server-update-window">
                <label>
                  <span>开始时间</span>
                  <input
                    type="time"
                    value={serverUpdateDraft.updateWindowStart}
                    disabled={serverUpdateLoading !== null || !serverUpdateDraft.autoUpdateEnabled}
                    onChange={(event) => {
                      const updateWindowStart = event.target.value;
                      updateServerUpdateDraft((current) => ({ ...current, updateWindowStart }));
                    }}
                  />
                </label>
                <label>
                  <span>结束时间</span>
                  <input
                    type="time"
                    value={serverUpdateDraft.updateWindowEnd}
                    disabled={serverUpdateLoading !== null || !serverUpdateDraft.autoUpdateEnabled}
                    onChange={(event) => {
                      const updateWindowEnd = event.target.value;
                      updateServerUpdateDraft((current) => ({ ...current, updateWindowEnd }));
                    }}
                  />
                </label>
              </div>
            </div>
            <GithubMirrorField
              label="GitHub 镜像加速"
              ariaLabel="服务端 GitHub 镜像加速地址"
              value={serverUpdateDraft.githubMirrorUrl}
              dirty={
                serverUpdate !== null
                && serverUpdateDraft.githubMirrorUrl !== serverUpdate.githubMirrorUrl
              }
              disabled={serverUpdateLoading !== null}
              onChange={(value) => {
                updateServerUpdateDraft((current) => ({ ...current, githubMirrorUrl: value }));
              }}
              onClear={() => {
                updateServerUpdateDraft((current) => ({ ...current, githubMirrorUrl: "" }));
              }}
            />
            <div className="inline-actions server-update-actions">
              <button
                className="button secondary"
                onClick={() => void saveServerUpdateSettings()}
                disabled={serverUpdateLoading !== null}
              >
                {serverUpdateLoading === "save" ? <LoaderCircle className="spin" size={16} /> : <Save size={16} />}
                保存更新设置
              </button>
              <button
                className="button secondary"
                onClick={() => void checkServerUpdate()}
                disabled={serverUpdateLoading !== null}
              >
                {serverUpdateLoading === "check" ? <LoaderCircle className="spin" size={16} /> : <RefreshCw size={16} />}
                手动检查
              </button>
              <button
                className="button primary"
                onClick={() => void installServerUpdate()}
                disabled={
                  serverUpdateLoading !== null ||
                  serverUpdateActive ||
                  !serverUpdate?.updateAvailable ||
                  !serverUpdate.agentOnline
                }
              >
                {serverUpdateLoading === "install" ? <LoaderCircle className="spin" size={16} /> : <Download size={16} />}
                立即更新服务器
              </button>
            </div>
            {serverUpdate ? (
              <div className="update-status" role="status">
                <strong>{serverUpdateMessage}</strong>
                <span>阶段：{serverUpdate.phase}</span>
                {serverUpdate.targetVersion ? <span>目标版本：v{serverUpdate.targetVersion}</span> : null}
                {serverUpdateRunning && !serverUpdate.agentOnline ? (
                  <span>更新代理正在执行长任务，心跳可能暂时中断</span>
                ) : null}
                {serverUpdate.lastCheckedAt ? <span>上次检查：{new Date(serverUpdate.lastCheckedAt).toLocaleString("zh-CN")}</span> : null}
                {serverUpdate.lastUpdatedAt ? <span>状态时间：{new Date(serverUpdate.lastUpdatedAt).toLocaleString("zh-CN")}</span> : null}
                {serverUpdate.error ? <span className="server-update-error">{serverUpdate.error}</span> : null}
              </div>
            ) : null}
            {serverUpdate && !serverUpdate.agentOnline && !serverUpdateRunning ? (
              <div className="server-update-agent-missing" role="status">
                <strong>宿主机更新代理没有在线，服务器更新无法执行</strong>
                <p>
                  服务端只负责写更新请求，真正拉镜像、备份、重启容器的是宿主机上的
                  <code>oshical-updater.timer</code>。它每 15 秒跑一次并写心跳，
                  服务端读到的心跳超过 90 秒没有更新就会显示「更新代理未在线」。
                </p>
                <p>按顺序排查（在服务器 shell 里执行）：</p>
                <ol>
                  <li><code>cd /opt/oshical &amp;&amp; sudo bash scripts/install-oshical-updater.sh</code> —— 安装或修复代理、密钥检查和部署路径。</li>
                  <li><code>sudo bash scripts/install-oshical-updater.sh --check</code> —— 检查密钥、timer、最近一次服务结果和心跳。</li>
                  <li><code>sudo journalctl -u oshical-updater.service -n 80 --no-pager</code> —— 查看具体启动错误。</li>
                  <li><code>docker compose exec oshical ls -l /app/data/system</code> —— 确认容器与宿主机指向同一个 <code>data/</code> 卷。</li>
                </ol>
                <p>
                  代理起来后卡片会自动变成「更新代理在线」，不需要重连。安装步骤见
                  <code>docs/06-deployment.md</code> 的「客户端服务器更新」一节。
                </p>
              </div>
            ) : null}
          </section>
        ) : null}

        <div className="settings-section-heading span-2">
          <strong>{serverMode ? "服务端采集设置" : "本地采集设置"}</strong>
          {serverMode ? null : <span>这些参数只在本机执行，不会上传到服务器。</span>}
        </div>

        <section className="settings-card span-2">
          <header><h2>网络代理</h2><span className="setting-scope server">{serverMode ? "服务端同步" : "本机"}</span></header>
          <div className="form-grid proxy-grid">
            <label>
              <span>本机代理端口</span>
              <div className="input-suffix">
                <input
                  type="number"
                  min={1}
                  max={65535}
                  inputMode="numeric"
                  value={draft.proxyPort ?? ""}
                  onChange={(event) => {
                    const proxyPort = event.target.value ? Number(event.target.value) : null;
                    proxyTestSeq.current += 1;
                    proxyTestingRef.current = false;
                    setProxyTesting(false);
                    setDraft((current) => ({ ...current, proxyPort }));
                    setProxyReport(null);
                  }}
                  placeholder="例如 7890"
                />
                <em>端口</em>
              </div>
            </label>
            <label>
              <span>测试目标</span>
              <select defaultValue="google">
                <option value="google">Google（www.google.com）</option>
              </select>
            </label>
            <div className="proxy-test-actions span-2">
              <button
                className="button secondary"
                type="button"
                onClick={() => void testProxy()}
                disabled={proxyTesting}
              >
                {proxyTesting ? <LoaderCircle className="spin" size={16} /> : <Globe2 size={16} />}
                {proxyTesting ? "正在测试 Google…" : "测试代理"}
              </button>
              {draft.proxyPort ? (
                <span className="form-hint">
                  {`请求将由${!tauri || draft.dataMode === "server" ? "服务器" : "本机"}发起，代理地址为 127.0.0.1:${draft.proxyPort}。`}
                </span>
              ) : null}
            </div>
          </div>
          {proxyReport ? (
            <div className={`proxy-test-result ${proxyReport.connected ? "is-connected" : "is-failed"}`} role="status">
              <Globe2 size={24} />
              <div className="proxy-test-copy">
                <strong>{proxyReport.connected ? "代理连接正常" : "代理连接失败"}</strong>
                <span>{proxyReport.message}</span>
                <small>{proxyReport.proxyUrl} · {proxyReport.target}</small>
              </div>
              <div className="proxy-latency">
                <Clock3 size={16} />
                <span>Google 延迟</span>
                <strong>{proxyReport.latencyMs.toLocaleString()} ms</strong>
              </div>
            </div>
          ) : null}
        </section>

        <section className="settings-card">
          <header><h2>刷新与直播统计</h2><span className="setting-scope server">{serverMode ? "服务端同步" : "本机"}</span></header>
          <div className="form-grid single">
            <label>
              <span>开播探测</span>
              <div className="input-suffix"><input type="number" min={10} max={300} value={draft.livePollSeconds} onChange={(event) => setDraft((current) => ({ ...current, livePollSeconds: Number(event.target.value) }))} /><em>秒</em></div>
            </label>
            <label>
              <span>动态刷新</span>
              <div className="input-suffix"><input type="number" min={300} max={3600} step={60} value={draft.dynamicPollSeconds} onChange={(event) => setDraft((current) => ({ ...current, dynamicPollSeconds: Number(event.target.value) }))} /><em>秒</em></div>
            </label>
            <Toggle
              checked={draft.liveStatsEnabled}
              onChange={(value) => setDraft((current) => ({ ...current, liveStatsEnabled: value }))}
              label="直播数据统计"
              labelFirst
            />
          </div>
        </section>

        <section className="settings-card bloodhound-card">
          <header>
            <h2>月相</h2>
            <span className="setting-scope server">{serverMode ? "服务端同步" : "本机"}</span>
            <span>从 LAPLACE 抓取周期参考数据（月隐 / 花期 / 月圆 / 晴日），展示在主播库的主播粉丝数旁边。</span>
          </header>
          <div className="settings-stack">
            {draft.fertilityEnabled ? (
              <>
                <div className="bloodhound-status is-on">
                  <span className="bloodhound-dot" aria-hidden="true" />
                  <div>
                    <strong>已开启</strong>
                    <span>
                      {serverMode ? "服务端" : "本机"}每 6 小时抓取一次 LAPLACE 全量列表，仅在主播库匹配展示；
                      关闭后立即停止抓取。
                    </span>
                  </div>
                  <button
                    className="button secondary"
                    onClick={() => void setBloodhound(false)}
                    disabled={bloodhoundBusy}
                  >
                    {bloodhoundBusy ? "处理中…" : "关闭"}
                  </button>
                </div>
              </>
            ) : (
              <>
                <div className="bloodhound-status">
                  <span className="bloodhound-dot" aria-hidden="true" />
                  <div>
                    <strong>未开启</strong>
                    <span>默认关闭。开启前需要确认免责声明与数据来源说明，未开启时不会发出任何 LAPLACE 请求。</span>
                  </div>
                  <button
                    className="button primary"
                    onClick={() => setBloodhoundOpen(true)}
                    disabled={bloodhoundBusy}
                  >
                    {bloodhoundBusy ? "处理中…" : "开启月相"}
                  </button>
                </div>
              </>
            )}
            <p className="form-hint">
              数据由 LAPLACE（laplace.live）及其投稿者整理与预测，本项目只做只读抓取，不上传任何数据。
            </p>
          </div>
        </section>

        <div className="settings-section-heading span-2">
          <strong>本设备设置</strong>
        </div>

        <AppearanceSettingsPanel value={appearance} onChange={onAppearanceChange} />

        <section className="settings-card device-local-settings">
          <header><h2>提醒与运行</h2><span className="setting-scope local">仅本设备</span></header>
          <div className="settings-stack">
            <Toggle checked={draft.notificationsEnabled} onChange={(value) => setDraft((current) => ({ ...current, notificationsEnabled: value }))} label="Windows 开播通知" />
            {tauri ? <Toggle checked={autoLaunch} onChange={setAutoLaunch} label="开机自启" /> : null}
            {tauri ? (
              <button className="button secondary" onClick={() => void actions.testNotification().catch((notificationError) => onError(explainError(notificationError)))}>
                发送测试通知
              </button>
            ) : null}
          </div>
        </section>

        {tauri ? (
          <section className="settings-card span-2 software-update-card">
            <header>
              <h2>软件更新</h2>
              <span className="setting-scope local">仅桌面端</span>
              <div className="update-channel-control">
                <div className="segmented" role="group" aria-label="桌面端更新渠道">
                  <button
                    type="button"
                    className={updateChannel === "stable" ? "active" : undefined}
                    onClick={() => switchUpdateChannel("stable")}
                    disabled={updateChecking || updateInstalling}
                  >
                    正式版
                  </button>
                  <button
                    type="button"
                    className={updateChannel === "beta" ? "active" : undefined}
                    onClick={() => switchUpdateChannel("beta")}
                    disabled={updateChecking || updateInstalling}
                  >
                    测试版
                  </button>
                </div>
              </div>
            </header>
            <div className="update-version-row">
              <div className="update-version">
                <span>当前版本 · {updateChannel === "beta" ? "测试版渠道" : "正式版渠道"}</span>
                <strong>{appVersion ? `v${appVersion}` : "读取中…"}</strong>
              </div>
              <button className="button secondary" onClick={() => void checkUpdates()} disabled={updateChecking || updateInstalling}>
                <RefreshCw className={updateChecking ? "spin" : undefined} size={16} />
                {updateChecking ? "检查中…" : "检查更新"}
              </button>
            </div>
            {updateStatus ? <div className="update-status" role="status">{updateStatus}</div> : null}
            {updateInfo ? (
              <div className="update-details">
                <div>
                  <strong>v{updateInfo.version}</strong>
                  <span>可更新</span>
                </div>
                {updateInfo.body ? <p className="update-notes">{updateInfo.body}</p> : <p className="form-hint">此版本没有提供发布说明。</p>}
                {updateInfo.date ? <small>发布日期：{updateInfo.date}</small> : null}
                <div className="inline-actions">
                  <button
                    className="button primary"
                    onClick={() => void installUpdate()}
                    disabled={updateInstalling || updateInstalled}
                  >
                    {updateInstalling ? <LoaderCircle className="spin" size={16} /> : <Download size={16} />}
                    {updateInstalling ? "正在安装…" : updateInstalled ? "安装完成" : "下载并安装"}
                  </button>
                  {updateInstalled ? (
                    <button className="button secondary" onClick={() => void restartForUpdate()}>
                      <RefreshCw size={16} />立即重启
                    </button>
                  ) : null}
                </div>
              </div>
            ) : null}
            {updateInstalling ? (
              <div className="update-progress">
                <progress value={updatePercent ?? undefined} max={100} />
                <span>
                  {updateProgress.total
                    ? `${formatBytes(updateProgress.downloaded)} / ${formatBytes(updateProgress.total)}`
                    : `已下载 ${formatBytes(updateProgress.downloaded)}`}
                </span>
              </div>
            ) : null}
            <GithubMirrorField
              label="GitHub 镜像加速"
              ariaLabel="客户端 GitHub 镜像加速地址"
              value={githubMirrorDraft}
              dirty={clientMirrorDirty}
              saving={githubMirrorSaving}
              error={githubMirrorError}
              onChange={(value) => {
                githubMirrorDraftRef.current = value;
                setGithubMirrorDraft(value);
              }}
              onSave={(value) => void saveGithubMirror(value)}
              onClear={() => void saveGithubMirror("")}
            />
          </section>
        ) : null}

        <section className="settings-card span-2">
          <header>
            <h2>日程识别模型</h2>
            <span className="setting-scope server">{serverMode ? "服务端同步" : "本机"}</span>
            <div className="card-header-toggle">
              <Toggle
                checked={draft.autoAnalyzeImages}
                onChange={(value) => setDraft((current) => ({ ...current, autoAnalyzeImages: value }))}
                label=""
                ariaLabel="自动分析动态文字和日程图片"
              />
            </div>
          </header>
          <div className="form-grid">
            <label>
              <span>Base URL</span>
              <input value={draft.llmBaseUrl} onChange={(event) => {
                const llmBaseUrl = event.target.value;
                llmTestSeq.current += 1;
                modelDiscoverySeq.current += 1;
                llmTestingRef.current = false;
                modelsLoadingRef.current = false;
                setLlmTesting(false);
                setModelsLoading(false);
                setDraft((current) => ({ ...current, llmBaseUrl }));
                setLlmReport(null);
              }} placeholder="https://api.example.com/v1" />
            </label>
            <label>
              <span>模型名</span>
              <div className="model-field-row">
                <input value={draft.llmModel} onChange={(event) => {
                  const llmModel = event.target.value;
                  llmTestSeq.current += 1;
                  llmTestingRef.current = false;
                  setLlmTesting(false);
                  setDraft((current) => ({ ...current, llmModel }));
                  setLlmReport(null);
                }} placeholder="文字/视觉模型名称" />
                <button className="button secondary small" type="button" onClick={() => void pullModels()} disabled={modelsLoading}>
                  {modelsLoading ? <LoaderCircle className="spin" size={15} /> : <Download size={15} />}
                  {modelsLoading ? "拉取中…" : "拉取模型"}
                </button>
              </div>
            </label>
            {tauri || serverMode ? (
              <label>
                <span>API Key</span>
                <input
                  type="password"
                  autoComplete="off"
                  value={llmApiKey}
                  onChange={(event) => {
                    llmTestSeq.current += 1;
                    modelDiscoverySeq.current += 1;
                    llmTestingRef.current = false;
                    modelsLoadingRef.current = false;
                    setLlmTesting(false);
                    setModelsLoading(false);
                    setLlmApiKey(event.target.value);
                    setLlmReport(null);
                  }}
                  placeholder={
                    secrets.llmConfigured
                      ? llmApiKeyHint
                        ? llmApiKeyHint + "（已配置，留空则沿用）"
                        : "已配置（不会明文回显），留空则沿用"
                      : serverMode
                        ? "保存到服务端并同步到其他设备"
                        : "保存到 Windows 凭据管理器"
                  }
                />
              </label>
            ) : null}
            <div className="stacked-field">
              <span>思考程度</span>
              <select
                value={llmReasoningEffort}
                onChange={(event) => changeReasoningEffort(event.target.value as LlmReasoningEffort)}
                aria-label="思考程度"
              >
                {LLM_REASONING_OPTIONS.map((option) => (
                  <option key={option.value} value={option.value}>{option.label}</option>
                ))}
              </select>
            </div>
            <label>
              <span>并行个数</span>
              <div className="input-suffix">
                <input
                  type="number"
                  inputMode="numeric"
                  min={LLM_PARALLELISM_MIN}
                  max={LLM_PARALLELISM_MAX}
                  step={1}
                  value={llmParallelism}
                  onChange={(event) => changeParallelism(event.target.value)}
                />
                <em>路</em>
              </div>
            </label>
            <div className="llm-test-panel span-2">
              <div className="inline-actions">
                <button className="button secondary" onClick={() => void testLlm()} disabled={llmTesting}>
                  {llmTesting ? <LoaderCircle className="spin" size={16} /> : <PlugZap size={16} />}
                  {llmTesting ? "正在识别示例图…" : "测试模型"}
                </button>
                <span className="form-hint llm-fixture-hint">固定验收图：10.26–11.01 枝江娱乐直播日历</span>
              </div>
              {llmReport ? (
                <div className={`llm-test-result ${llmReport.connected ? "is-connected" : "is-failed"}`} role="status">
                  <div className="llm-test-metrics">
                    <div>
                      <Activity size={16} />
                      <span>连通性</span>
                      <strong>{llmReport.connected ? "已连接" : "失败"}</strong>
                    </div>
                    <div>
                      <Clock3 size={16} />
                      <span>端到端延迟</span>
                      <strong>{llmReport.latencyMs.toLocaleString()} ms</strong>
                    </div>
                    <div>
                      <ListChecks size={16} />
                      <span>识别事件</span>
                      <strong>{llmReport.eventCount}</strong>
                    </div>
                    <div>
                      <Target size={16} />
                      <span>成功率</span>
                      <strong>{Math.round(llmReport.successRate * 100)}%</strong>
                    </div>
                  </div>
                  <p>{llmReport.message}</p>
                  <small>基准匹配 {llmReport.matchedCount}/{llmReport.expectedCount}；匹配口径为开播时间误差不超过 10 分钟，且主播与主题吻合。</small>
                </div>
              ) : null}
            </div>
          </div>
        </section>

        {tauri || serverMode ? (
          <section className="settings-card span-2">
            <header>
              <h2>B 站登录与 Cookie</h2>
              <span className={`setting-scope ${serverMode ? "server" : "local"}`}>
                {serverMode ? "服务器" : "仅本设备"}
              </span>
              {serverMode ? (
                <span className={`server-login-pill ${biliConfigured ? "is-online" : "is-offline"}`}>
                  {biliConfigured ? "服务端已登录" : "服务端未登录"}
                </span>
              ) : (
                <span>登录后自动保存到 Windows 凭据管理器。</span>
              )}
            </header>
            <div className="bili-login-actions">
              {!serverMode && tauri ? (
                <button className="button secondary" onClick={() => void openBrowserLogin()} disabled={browserLoginOpening}>
                  {browserLoginOpening ? <LoaderCircle className="spin" size={16} /> : <ExternalLink size={16} />}
                  登录窗口自动获取
                </button>
              ) : null}
              <button className="button primary" onClick={() => void startQrLogin()} disabled={qrLoading}>
                {qrLoading ? <LoaderCircle className="spin" size={16} /> : <QrCode size={16} />}
                {serverMode ? "服务器扫码登录" : "扫码登录"}
              </button>
              <button className="button secondary" onClick={() => void logoutBili()} disabled={!biliConfigured}>
                <LogOut size={16} />退出登录
              </button>
            </div>
            {browserLoginHint ? <p className="form-hint login-hint">{browserLoginHint}</p> : null}
            {serverMode ? null : (
              <div className="form-grid">
                <label>
                  <span>手动填写 SESSDATA / Cookie（可选）</span>
                  <input type="password" autoComplete="off" value={biliCookie} onChange={(event) => setBiliCookie(event.target.value)} placeholder={biliConfigured ? "已配置，留空则沿用" : "可选的 B 站 Cookie"} />
                </label>
                <div className="secret-state span-2">
                  <KeyRound size={17} />
                  <span>{biliConfigured ? "B 站 Cookie 已保存" : "使用公开接口 + buvid"}</span>
                </div>
              </div>
            )}
          </section>
        ) : null}
      </div>

      {bloodhoundOpen ? (
        <Modal
          title="开启月相？"
          onClose={() => {
            if (!bloodhoundBusy) {
              setBloodhoundOpen(false);
            }
          }}
          footer={
            <>
              <button
                className="button secondary"
                onClick={() => setBloodhoundOpen(false)}
                disabled={bloodhoundBusy}
              >
                取消
              </button>
              <button
                className="button secondary"
                onClick={() => void openLaplaceSite()}
                disabled={bloodhoundBusy}
              >
                <ExternalLink size={16} />前往 LAPLACE
              </button>
              <button
                className="button primary"
                onClick={() => void confirmBloodhound()}
                disabled={bloodhoundBusy}
              >
                {bloodhoundBusy ? "正在保存…" : "已阅读并同意，开启月相"}
              </button>
            </>
          }
        >
          <div className="bloodhound-confirm">
            <section>
              <h3>免责声明</h3>
              <ul>
                <li>周期数据由 LAPLACE 的投稿者提交，并由其模型推算；本应用只做只读抓取，不生成也不校验内容。</li>
                <li>这些数据仅供娱乐参考，<strong>不是医疗信息</strong>，不构成任何健康建议或诊断依据。</li>
                <li>预测存在误差，可能滞后、缺失或与实际情况不符；请勿据此做出任何医疗、法律或人身决定。</li>
                <li>数据涉及第三方个人信息；开启后这些信息只保存在{serverMode ? "你的服务器" : "本机"}数据库，不会回传给我们。</li>
                <li>「月相」默认关闭；关闭后会立即停止抓取，已缓存的数据会保留在本地数据库中。</li>
              </ul>
            </section>
            <section>
              <h3>致谢</h3>
              <p>
                感谢 <strong>LAPLACE</strong>（<a href="https://laplace.live/ovu" target="_blank" rel="noreferrer">laplace.live/ovu</a>）
                及其所有投稿者、维护者。没有他们长期整理与公开的数据，这个功能无法存在。
                如果 LAPLACE 认为本接入方式不妥，我们会立刻下线该功能。
              </p>
            </section>
            <p className="form-hint">
              点击下方按钮表示你已阅读并理解以上说明，同意{serverMode ? "服务端" : "本机"}每 6 小时抓取一次
              LAPLACE 公开列表；关闭不受限制。
            </p>
          </div>
        </Modal>
      ) : null}

      {leavePromptOpen ? (
        <Modal
          title="保存设置？"
          onClose={() => {
            if (!leaveBusy) {
              settleLeavePrompt(false);
            }
          }}
          footer={
            <>
              <button
                className="button secondary"
                onClick={() => settleLeavePrompt(false)}
                disabled={leaveBusy}
              >
                取消
              </button>
              <button
                className="button secondary"
                onClick={() => settleLeavePrompt(true)}
                disabled={leaveBusy}
              >
                不保存
              </button>
              <button
                className="button primary"
                onClick={() => void saveAndLeave()}
                disabled={leaveBusy}
              >
                {leaveBusy ? <LoaderCircle className="spin" size={16} /> : <Save size={16} />}
                {leaveBusy ? "保存中…" : "保存并离开"}
              </button>
            </>
          }
        >
          <div className="settings-leave-prompt">
            <strong>当前设置尚未保存</strong>
            <p>保存后再离开，或者放弃本次修改。选择“不保存”不会更新设置。</p>
            {error ? <ErrorBanner message={error} /> : null}
          </div>
        </Modal>
      ) : null}

      {qrOpen ? (
        <Modal
          title="B 站扫码登录"
          onClose={closeQrLogin}
          footer={
            <>
              <button className="button secondary" onClick={() => void startQrLogin()} disabled={qrLoading}>
                <RefreshCw size={16} />重新生成
              </button>
            </>
          }
        >
          <div className="qr-login-panel">
            {qrSession ? <img src={qrSession.dataUrl} alt="B 站登录二维码" width={260} height={260} /> : <LoaderCircle className="spin" size={34} />}
            <strong>使用哔哩哔哩手机客户端扫码</strong>
            <p className={`qr-login-status ${qrStatus}`}>{qrMessage}</p>
            {qrStatus === "confirmed" ? (
              <span className="success-inline">
                <CheckCircle2 size={15} />
                {serverMode ? "服务器已安全保存 Cookie" : "Cookie 已安全保存"}
              </span>
            ) : null}
          </div>
        </Modal>
      ) : null}

      {modelsOpen ? (
        <Modal
          title="选择模型"
          onClose={() => setModelsOpen(false)}
          wide
        >
          <div className="model-picker">
            <div className="search-input compact">
              <Search size={17} />
              <input
                autoFocus
                value={modelSearch}
                onChange={(event) => setModelSearch(event.target.value)}
                placeholder="筛选模型 ID"
              />
              <span className="muted">{visibleModels.length}/{models.length}</span>
            </div>
            <div className="model-picker-list">
              {visibleModels.map((model) => (
                <button
                  key={model}
                  className={`model-option ${draft.llmModel === model ? "active" : ""}`}
                  onClick={() => {
                    markFormChanged();
                    setDraft((current) => ({ ...current, llmModel: model }));
                    setLlmReport(null);
                    setModelsOpen(false);
                  }}
                >
                  <span>{model}</span>
                  {draft.llmModel === model ? <CheckCircle2 size={16} /> : null}
                </button>
              ))}
              {!visibleModels.length ? <p className="muted model-picker-empty">没有匹配的模型</p> : null}
            </div>
          </div>
        </Modal>
      ) : null}
    </section>
  );
});

export default SettingsView;
