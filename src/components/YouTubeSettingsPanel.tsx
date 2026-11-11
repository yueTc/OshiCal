import { useEffect, useRef, useState } from "react";
import {
  CheckCircle2,
  Cookie,
  ExternalLink,
  Globe2,
  KeyRound,
  LoaderCircle,
  LogOut,
  RefreshCw,
  ShieldCheck,
} from "lucide-react";
import type {
  YoutubeCredentialStatus,
  YoutubeOAuthStart,
  YoutubeWebLoginStart,
} from "../types";
import { explainError, openExternalUrl } from "../lib/api";
import { ErrorBanner, Toggle } from "./ui";

export interface YouTubeSettingsActions {
  status: () => Promise<YoutubeCredentialStatus>;
  saveApiKey: (apiKey: string) => Promise<YoutubeCredentialStatus>;
  testApiKey: () => Promise<YoutubeCredentialStatus>;
  clearApiKey: () => Promise<YoutubeCredentialStatus>;
  startOAuth: () => Promise<YoutubeOAuthStart>;
  clearOAuth: () => Promise<YoutubeCredentialStatus>;
  startWebLogin: () => Promise<YoutubeWebLoginStart>;
  saveWebSession: (cookie: string) => Promise<YoutubeCredentialStatus>;
  clearWebSession: () => Promise<YoutubeCredentialStatus>;
}

interface YouTubeSettingsPanelProps {
  enabled: boolean;
  active: boolean;
  serverMode: boolean;
  actions: YouTubeSettingsActions;
  onEnabledChange: (enabled: boolean) => void;
  onError: (message: string) => void;
}

type Operation = "load" | "save-key" | "test-key" | "clear-key" | "oauth" | "clear-oauth" | "web-login" | "save-session" | "clear-session" | null;

const SAFE_CREDENTIAL_ERROR = "YouTube 凭据操作失败，请检查网络、权限，或重新授权后再试。";
const CREDENTIAL_LIKE_TEXT = /(?:AIza[0-9A-Za-z_-]{20,}|ya29\.[0-9A-Za-z._-]+|Bearer\s+[0-9A-Za-z._~+/-]+=*|(?:^|[;\s])(?:SID|HSID|SSID|APISID|SAPISID|__Secure-[^=\s;]+)=)/i;

function credentialErrorMessage(error: unknown): string {
  const message = explainError(error).trim();
  if (!message || message.length > 240 || CREDENTIAL_LIKE_TEXT.test(message)) {
    return SAFE_CREDENTIAL_ERROR;
  }
  return message;
}

function StatusPill({
  configured,
  expired = false,
  label,
}: {
  configured: boolean;
  expired?: boolean;
  label: string;
}) {
  return (
    <span
      className={`youtube-status-pill ${expired ? "is-expired" : configured ? "is-configured" : "is-missing"}`}
      role="status"
      aria-live="polite"
    >
      {configured && !expired ? <CheckCircle2 size={13} /> : <span className="youtube-status-dot" />}
      {label}
    </span>
  );
}

export default function YouTubeSettingsPanel({
  enabled,
  active,
  serverMode,
  actions,
  onEnabledChange,
  onError,
}: YouTubeSettingsPanelProps) {
  const [status, setStatus] = useState<YoutubeCredentialStatus | null>(null);
  const [operation, setOperation] = useState<Operation>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [webCookie, setWebCookie] = useState("");
  const [statusMessage, setStatusMessage] = useState<string | null>(null);
  const requestSeq = useRef(0);
  const actionsRef = useRef(actions);
  actionsRef.current = actions;
  const enabledRef = useRef(enabled);
  enabledRef.current = enabled;
  const activeRef = useRef(active);
  activeRef.current = active;
  const busy = operation !== null;

  async function loadStatus(showMessage = false) {
    if (!enabledRef.current || !activeRef.current) {
      return null;
    }
    const requestId = ++requestSeq.current;
    setOperation((current) => current ?? "load");
    setActionError(null);
    try {
      const next = await actionsRef.current.status();
      if (requestSeq.current !== requestId) {
        return null;
      }
      setStatus(next);
      if (showMessage) {
        setStatusMessage("YouTube 配置状态已刷新。");
      }
      return next;
    } catch (error) {
      if (requestSeq.current === requestId && enabledRef.current && activeRef.current) {
        const message = credentialErrorMessage(error);
        setActionError(message);
        onError(message);
      }
      return null;
    } finally {
      if (requestSeq.current === requestId && enabledRef.current && activeRef.current) {
        setOperation((current) => current === "load" ? null : current);
      }
    }
  }

  useEffect(() => {
    requestSeq.current += 1;
    setStatus(null);
    setActionError(null);
    setStatusMessage(null);
    setApiKey("");
    setWebCookie("");
    if (!enabled || !active) {
      setOperation(null);
      return;
    }
    void loadStatus();
    const timer = window.setInterval(() => {
      void loadStatus();
    }, 15_000);
    return () => window.clearInterval(timer);
    // The interval reads actions through actionsRef so a data-mode switch cannot
    // keep polling the previous local/server connection.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, enabled]);

  useEffect(() => {
    if (!statusMessage) {
      return;
    }
    const timer = window.setTimeout(() => setStatusMessage(null), 3500);
    return () => window.clearTimeout(timer);
  }, [statusMessage]);

  async function saveApiKey() {
    const nextKey = apiKey.trim();
    if (!nextKey || busy) {
      return;
    }
    setOperation("save-key");
    setActionError(null);
    setStatusMessage(null);
    try {
      const next = await actions.saveApiKey(nextKey);
      setStatus(next);
      setApiKey("");
      setStatusMessage("YouTube API Key 已加密保存。");
    } catch (error) {
      const message = credentialErrorMessage(error);
      setActionError(message);
      onError(message);
    } finally {
      setOperation(null);
    }
  }

  async function testApiKey() {
    if (busy || !status?.apiKeyConfigured) {
      return;
    }
    setOperation("test-key");
    setActionError(null);
    setStatusMessage(null);
    try {
      const next = await actions.testApiKey();
      setStatus(next);
      setStatusMessage(next.apiKeyValid === false
        ? "API Key 测试未通过，请检查 Key、配额和网络。"
        : "YouTube API Key 连接测试通过。");
    } catch (error) {
      const message = credentialErrorMessage(error);
      setActionError(message);
      onError(message);
    } finally {
      setOperation(null);
    }
  }

  async function clearApiKey() {
    if (busy || !window.confirm("清除本用户的 YouTube API Key？已有主播和数据不会被删除。")) {
      return;
    }
    setOperation("clear-key");
    setActionError(null);
    try {
      setStatus(await actions.clearApiKey());
      setApiKey("");
      setStatusMessage("YouTube API Key 已清除。");
    } catch (error) {
      const message = credentialErrorMessage(error);
      setActionError(message);
      onError(message);
    } finally {
      setOperation(null);
    }
  }

  async function startOAuth() {
    if (busy) {
      return;
    }
    setOperation("oauth");
    setActionError(null);
    setStatusMessage(null);
    try {
      const start = await actions.startOAuth();
      // Desktop local mode opens the system browser inside the Tauri command.
      // Only the server/Web flow needs the frontend to open the returned URL.
      if (serverMode) {
        if (!start.authorizationUrl) {
          throw new Error("YouTube OAuth 启动响应缺少授权地址");
        }
        await openExternalUrl(start.authorizationUrl);
      }
      setStatusMessage("已打开 Google 授权页面，完成后返回这里刷新状态。");
      void loadStatus();
    } catch (error) {
      const message = credentialErrorMessage(error);
      setActionError(message);
      onError(message);
    } finally {
      setOperation(null);
    }
  }

  async function clearOAuth() {
    if (busy || !window.confirm("退出 Google OAuth？直播聊天和 Super Chat 统计将停止。")) {
      return;
    }
    setOperation("clear-oauth");
    setActionError(null);
    try {
      setStatus(await actions.clearOAuth());
      setStatusMessage("Google OAuth 已退出。");
    } catch (error) {
      const message = credentialErrorMessage(error);
      setActionError(message);
      onError(message);
    } finally {
      setOperation(null);
    }
  }

  async function startWebLogin() {
    if (busy) {
      return;
    }
    setOperation("web-login");
    setActionError(null);
    setStatusMessage(null);
    try {
      const start = await actions.startWebLogin();
      if (start.loginUrl) {
        await openExternalUrl(start.loginUrl);
      }
      setStatusMessage(start.message || "已打开 YouTube 网页登录。完成会员验证后返回这里刷新状态。");
      void loadStatus();
    } catch (error) {
      const message = credentialErrorMessage(error);
      setActionError(message);
      onError(message);
    } finally {
      setOperation(null);
    }
  }

  async function saveWebSession() {
    const nextCookie = webCookie.trim();
    if (!nextCookie || busy) {
      return;
    }
    setOperation("save-session");
    setActionError(null);
    setStatusMessage(null);
    try {
      setStatus(await actions.saveWebSession(nextCookie));
      setWebCookie("");
      setStatusMessage("YouTube 网页会员会话已加密保存。");
    } catch (error) {
      const message = credentialErrorMessage(error);
      setActionError(message);
      onError(message);
    } finally {
      setOperation(null);
    }
  }

  async function clearWebSession() {
    if (busy || !window.confirm("清除 YouTube 网页会话？会员限定社区帖将停止抓取。")) {
      return;
    }
    setOperation("clear-session");
    setActionError(null);
    try {
      setStatus(await actions.clearWebSession());
      setWebCookie("");
      setStatusMessage("YouTube 网页会话已清除。");
    } catch (error) {
      const message = credentialErrorMessage(error);
      setActionError(message);
      onError(message);
    } finally {
      setOperation(null);
    }
  }

  return (
    <section className="settings-card span-2 youtube-settings-card">
      <header>
        <Globe2 size={19} />
        <h2>YouTube 功能</h2>
        <span className={`setting-scope ${serverMode ? "server" : "local"}`}>
          {serverMode ? "服务器" : "仅本设备"}
        </span>
        <span className="youtube-header-toggle">
          <Toggle
            checked={enabled}
            onChange={onEnabledChange}
            label={enabled ? "已开启" : "默认关闭"}
            ariaLabel="开启或关闭 YouTube 功能"
          />
        </span>
      </header>

      <p className="form-hint youtube-settings-intro">
        开启后才会显示 YouTube 平台入口，并运行频道、社区帖、直播与统计任务。关闭不会删除已有数据或凭据。
      </p>

      {!active ? (
        <div className="youtube-disabled-note" role="status">
          <ShieldCheck size={18} />
          <div>
            <strong>{enabled ? "YouTube 已加入待保存设置" : "YouTube 功能当前关闭"}</strong>
            <span>
              {enabled
                ? "点击页面上方“保存设置”后才会启用后端任务和凭据配置。"
                : "不会发起任何 YouTube 网络请求，主播库中也不会显示 YouTube 卡片。"}
            </span>
          </div>
        </div>
      ) : !enabled ? (
        <div className="youtube-disabled-note" role="status">
          <ShieldCheck size={18} />
          <div>
            <strong>YouTube 关闭尚未保存</strong>
            <span>点击页面上方“保存设置”后停止后台任务并隐藏入口；已有数据和凭据会保留。</span>
          </div>
        </div>
      ) : (
        <>
          {actionError ? <ErrorBanner message={actionError} onRetry={() => void loadStatus(true)} /> : null}
          {status?.lastError ? <div className="youtube-status-warning">{status.lastError}</div> : null}
          {statusMessage ? <div className="success-inline" role="status"><CheckCircle2 size={15} />{statusMessage}</div> : null}

          <div className="youtube-credential-grid">
            <article className="youtube-credential-card">
              <div className="youtube-credential-head">
                <span className="youtube-credential-icon"><KeyRound size={17} /></span>
                <div>
                  <strong>YouTube API Key</strong>
                  <small>频道搜索、频道信息、预约直播和观看人数。</small>
                </div>
                <StatusPill
                  configured={status?.apiKeyConfigured ?? false}
                  label={status?.apiKeyConfigured ? "已保存" : "未配置"}
                />
              </div>
              <div className="youtube-key-row">
                <input
                  type="password"
                  autoComplete="off"
                  autoCapitalize="none"
                  spellCheck={false}
                  value={apiKey}
                  onChange={(event) => setApiKey(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      void saveApiKey();
                    }
                  }}
                  placeholder={status?.apiKeyConfigured ? "已配置，输入新 Key 可覆盖" : "AIza..."}
                  aria-label="YouTube API Key"
                  aria-invalid={status?.apiKeyValid === false}
                  aria-describedby={status?.apiKeyValid === false ? "youtube-api-key-error" : undefined}
                  disabled={busy}
                />
                <button
                  type="button"
                  className="button primary"
                  onClick={() => void saveApiKey()}
                  disabled={busy || !apiKey.trim()}
                >
                  {operation === "save-key" ? <LoaderCircle className="spin" size={15} /> : <KeyRound size={15} />}
                  保存
                </button>
              </div>
              <div className="youtube-credential-actions">
                <button
                  type="button"
                  className="button secondary small"
                  onClick={() => void testApiKey()}
                  disabled={busy || !status?.apiKeyConfigured}
                >
                  {operation === "test-key" ? <LoaderCircle className="spin" size={14} /> : <RefreshCw size={14} />}
                  {operation === "test-key" ? "测试中…" : "测试连接"}
                </button>
                <button
                  type="button"
                  className="text-button danger-text"
                  onClick={() => void clearApiKey()}
                  disabled={busy || !status?.apiKeyConfigured}
                >
                  清除 API Key
                </button>
              </div>
              {status?.apiKeyValid === false ? (
                <p id="youtube-api-key-error" className="youtube-field-error" role="alert">
                  API Key 校验失败，请检查配额或 Key 是否有效。
                </p>
              ) : null}
            </article>

            <article className="youtube-credential-card">
              <div className="youtube-credential-head">
                <span className="youtube-credential-icon"><ShieldCheck size={17} /></span>
                <div>
                  <strong>Google OAuth</strong>
                  <small>官方直播聊天、Super Chat 和会员事件统计。</small>
                </div>
                <StatusPill
                  configured={status?.oauthConfigured ?? false}
                  expired={status?.oauthExpired}
                  label={status?.oauthExpired ? "已过期" : status?.oauthConfigured ? "已授权" : "未授权"}
                />
              </div>
              <div className="youtube-credential-actions wrap">
                <button type="button" className="button primary small" onClick={() => void startOAuth()} disabled={busy}>
                  {operation === "oauth" ? <LoaderCircle className="spin" size={14} /> : <ExternalLink size={14} />}
                  {status?.oauthConfigured ? "重新授权" : "授权 Google 账号"}
                </button>
                <button
                  type="button"
                  className="button secondary small"
                  onClick={() => void loadStatus(true)}
                  disabled={busy}
                >
                  <RefreshCw size={14} />刷新状态
                </button>
                <button
                  type="button"
                  className="text-button danger-text"
                  onClick={() => void clearOAuth()}
                  disabled={busy || !status?.oauthConfigured}
                >
                  <LogOut size={13} />退出授权
                </button>
              </div>
            </article>

            <article className="youtube-credential-card">
              <div className="youtube-credential-head">
                <span className="youtube-credential-icon"><Cookie size={17} /></span>
                <div>
                  <strong>网页会员会话</strong>
                  <small>仅用于读取当前账号有权限访问的会员限定社区帖。</small>
                </div>
                <StatusPill
                  configured={status?.webSessionConfigured ?? false}
                  label={status?.webSessionConfigured ? "已登录" : "未登录"}
                />
              </div>
              <div className="youtube-credential-actions wrap">
                <button type="button" className="button primary small" onClick={() => void startWebLogin()} disabled={busy}>
                  {operation === "web-login" ? <LoaderCircle className="spin" size={14} /> : <ExternalLink size={14} />}
                  {status?.webSessionConfigured ? "重新登录" : "登录 YouTube 网页"}
                </button>
                <button
                  type="button"
                  className="button secondary small"
                  onClick={() => void loadStatus(true)}
                  disabled={busy}
                >
                  <RefreshCw size={14} />刷新状态
                </button>
                <button
                  type="button"
                  className="text-button danger-text"
                  onClick={() => void clearWebSession()}
                  disabled={busy || !status?.webSessionConfigured}
                >
                  清除网页会话
                </button>
              </div>
              {serverMode ? (
                <div className="youtube-session-import">
                  <label>
                    <span>导入网页会话 Cookie</span>
                    <textarea
                      rows={3}
                      autoComplete="off"
                      autoCapitalize="none"
                      spellCheck={false}
                      value={webCookie}
                      onChange={(event) => setWebCookie(event.target.value)}
                      placeholder="登录 Google 后，从受控浏览器的 YouTube 会话复制 Cookie"
                      disabled={busy}
                    />
                  </label>
                  <button
                    type="button"
                    className="button secondary small"
                    onClick={() => void saveWebSession()}
                    disabled={busy || !webCookie.trim()}
                  >
                    {operation === "save-session" ? <LoaderCircle className="spin" size={14} /> : <Cookie size={14} />}
                    保存会话
                  </button>
                </div>
              ) : null}
              <p className="youtube-session-note">无权限、未登录或会员失效时，会员帖会被跳过，不会当作抓取错误。</p>
            </article>
          </div>
        </>
      )}
    </section>
  );
}
