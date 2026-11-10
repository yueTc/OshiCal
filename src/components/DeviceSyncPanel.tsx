import { useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  LoaderCircle,
  Monitor,
  RefreshCw,
  RotateCcw,
  Server,
  Trash2,
  Upload,
  Wifi,
  WifiOff,
} from "lucide-react";
import { explainError } from "../lib/api";
import type { DeviceInfo, JobProgress, SyncStatus, SyncUiState } from "../lib/sync";
import { Modal } from "./ui";

const STATUS_LABELS: Record<SyncStatus, string> = {
  idle: "未启动",
  connecting: "连接中",
  connected: "已同步",
  syncing: "同步中",
  reset_required: "需重置",
  offline: "离线",
  error: "同步错误",
};

function formatTime(value: string | null): string {
  if (!value) {
    return "尚未同步";
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return "时间未知";
  }
  return date.toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

function formatPlatform(value: string): string {
  if (value.includes("windows")) {
    return "Windows";
  }
  if (value.includes("web") || value.includes("browser")) {
    return "Web";
  }
  return value || "未知平台";
}

function shortDeviceId(value: string): string {
  return value.length > 18 ? `${value.slice(0, 8)}…${value.slice(-6)}` : value || "未生成";
}

export default function DeviceSyncPanel({
  sync,
  devices,
  loading,
  error,
  activeJobs,
  onRefresh,
  onResync,
  onImportServer,
  onMigrateToServer,
  onDeleteDevice,
}: {
  sync: SyncUiState;
  devices: DeviceInfo[];
  loading: boolean;
  error: string | null;
  activeJobs: JobProgress[];
  onRefresh: () => void;
  onResync: () => void;
  onImportServer?: () => void;
  onMigrateToServer?: () => void;
  onDeleteDevice: (device: DeviceInfo) => Promise<void>;
}) {
  const [deleteTarget, setDeleteTarget] = useState<DeviceInfo | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const percent =
    sync.latestSeq > 0
      ? Math.max(0, Math.min(100, Math.round((sync.cursorSeq / sync.latestSeq) * 100)))
      : 0;
  const statusLabel = STATUS_LABELS[sync.status];
  const isBusy = sync.status === "syncing" || sync.status === "reset_required";

  function requestDelete(device: DeviceInfo) {
    if (device.online || device.deviceId === sync.deviceId) {
      return;
    }
    setDeleteError(null);
    setDeleteTarget(device);
  }

  async function confirmDelete() {
    if (!deleteTarget) {
      return;
    }
    setDeleting(true);
    setDeleteError(null);
    try {
      await onDeleteDevice(deleteTarget);
      setDeleteTarget(null);
    } catch (error) {
      setDeleteError(explainError(error));
    } finally {
      setDeleting(false);
    }
  }

  return (
    <section className="settings-card span-2 device-sync-panel" aria-live="polite">
      <header className="device-sync-header">
        <div>
          <h2>多设备同步</h2>
        </div>
        <div className={`device-sync-status ${sync.status}`}>
          {sync.status === "connected" ? (
            <CheckCircle2 size={15} />
          ) : sync.status === "offline" ? (
            <WifiOff size={15} />
          ) : isBusy ? (
            <LoaderCircle className={sync.status === "syncing" ? "spin" : ""} size={15} />
          ) : sync.status === "error" || sync.status === "reset_required" ? (
            <AlertTriangle size={15} />
          ) : (
            <Wifi size={15} />
          )}
          {statusLabel}
        </div>
      </header>

      <div className="device-sync-overview">
        <div className="device-sync-metric">
          <span>当前游标</span>
          <strong>{sync.cursorSeq.toLocaleString("zh-CN")}</strong>
          <small>ACK {sync.ackedSeq.toLocaleString("zh-CN")}</small>
        </div>
        <div className="device-sync-metric">
          <span>服务器最新</span>
          <strong>{sync.latestSeq.toLocaleString("zh-CN")}</strong>
          <small>协议 {sync.protocolVersion ?? "旧版"}</small>
        </div>
        <div className="device-sync-metric">
          <span>最后同步</span>
          <strong className="device-sync-time">{formatTime(sync.lastSyncAt)}</strong>
          <small>保留起点 {sync.minRetainedSeq.toLocaleString("zh-CN")}</small>
        </div>
        <div className="device-sync-metric">
          <span>当前设备</span>
          <strong className="mono">{shortDeviceId(sync.deviceId)}</strong>
          <small>{sync.status === "offline" ? "等待重连" : "设备身份已登记"}</small>
        </div>
      </div>

      {sync.latestSeq > 0 ? (
        <div className="device-sync-progress" aria-label={`同步进度 ${percent}%`}>
          <span style={{ width: `${percent}%` }} />
        </div>
      ) : null}

      {sync.lastError ? (
        <div className={`device-sync-alert ${sync.status}`} role="status">
          <AlertTriangle size={16} />
          <span>{sync.lastError}</span>
          {sync.status === "reset_required" || sync.status === "error" ? (
            <button className="button secondary small" onClick={onResync}>
              <RotateCcw size={14} />重新同步
            </button>
          ) : null}
        </div>
      ) : null}

      {activeJobs.length ? (
        <div className="device-job-list">
          <div className="device-sync-section-title">服务器刷新任务</div>
          {activeJobs.map((job) => (
            <div className={`device-job-row ${job.failed ? "is-failed" : job.done ? "is-done" : ""}`} key={job.jobId}>
              <div className="device-job-copy">
                <strong>{job.streamerName || (job.kind === "refresh_all" ? "全量刷新" : "刷新任务")}</strong>
                <span>{job.stage}</span>
              </div>
              <div className="device-job-progress">
                <i style={{ width: `${Math.max(0, Math.min(100, job.percent))}%` }} />
              </div>
              <small>{job.failed ? "失败" : job.done ? "完成" : `${job.completed}/${job.total || "?"}`}</small>
            </div>
          ))}
        </div>
      ) : null}

      <div className="device-sync-section-title">
        <span>已连接设备</span>
        <div className="inline-actions">
          {onMigrateToServer ? (
            <button className="button secondary small" onClick={onMigrateToServer}>
              <Upload size={14} />迁移本机数据到服务端
            </button>
          ) : null}
          {onImportServer ? (
            <button className="button secondary small" onClick={onImportServer}>
              <Download size={14} />用服务端数据覆盖本地
            </button>
          ) : null}
          <button className="button secondary small" onClick={onRefresh} disabled={loading}>
            <RefreshCw className={loading ? "spin" : ""} size={14} />刷新设备
          </button>
        </div>
      </div>

      {error ? <p className="device-sync-empty is-error">{error}</p> : null}
      {loading && !devices.length ? (
        <div className="device-sync-empty"><LoaderCircle className="spin" size={18} />正在读取设备列表…</div>
      ) : null}
      {!loading && !devices.length && !error ? (
        <div className="device-sync-empty">
          <Server size={18} />
          服务端暂无设备记录。首次同步完成后会显示在这里。
        </div>
      ) : null}
      {devices.length ? (
        <div className="device-list">
          {devices.map((device) => (
            <article className={`device-row ${device.online ? "is-online" : ""}`} key={device.deviceId}>
              <div className="device-icon">
                {device.online ? <Wifi size={17} /> : <Monitor size={17} />}
              </div>
              <div className="device-row-copy">
                <strong>
                  {device.name}
                  {device.deviceId === sync.deviceId ? <em>当前设备</em> : null}
                </strong>
                <span>{formatPlatform(device.platform)} · v{device.appVersion} · {shortDeviceId(device.deviceId)}</span>
                <small>最后在线 {formatTime(device.lastSeenAt)} · ACK {device.lastAckSeq.toLocaleString("zh-CN")}</small>
              </div>
              <div className="device-row-actions">
                <span className={`device-online-state ${device.online ? "online" : "offline"}`}>
                  {device.online ? "在线" : "离线"}
                </span>
                {!device.online && device.deviceId !== sync.deviceId ? (
                  <button
                    type="button"
                    className="icon-button device-delete-button"
                    title={`删除 ${device.name} 的离线日志`}
                    aria-label={`删除 ${device.name} 的离线日志`}
                    onClick={() => requestDelete(device)}
                  >
                    <Trash2 size={15} />
                  </button>
                ) : null}
              </div>
            </article>
          ))}
        </div>
      ) : null}

      {deleteTarget ? (
        <Modal
          title="删除离线设备日志？"
          onClose={() => {
            if (!deleting) {
              setDeleteTarget(null);
            }
          }}
          footer={
            <>
              <button
                className="button secondary"
                onClick={() => setDeleteTarget(null)}
                disabled={deleting}
              >
                取消
              </button>
              <button
                className="button danger"
                onClick={() => void confirmDelete()}
                disabled={deleting}
              >
                {deleting ? <LoaderCircle className="spin" size={15} /> : <Trash2 size={15} />}
                {deleting ? "删除中…" : "仅删除日志"}
              </button>
            </>
          }
        >
          <div className="device-delete-prompt">
            <strong>{deleteTarget.name}</strong>
            <p>
              只会删除设备的在线状态和 ACK 记录，不会删除日程、主播、同步数据或其他设备信息。
              该设备下次连接时会自动重新登记。
            </p>
            {deleteError ? <p className="inline-error">{deleteError}</p> : null}
          </div>
        </Modal>
      ) : null}
    </section>
  );
}
