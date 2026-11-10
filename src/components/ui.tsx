import { useState } from "react";
import type { ReactNode } from "react";
import { X } from "lucide-react";
import type { Streamer } from "../types";

export function Avatar({
  url,
  name,
  size = "md",
  live = false,
}: {
  url: string;
  name: string;
  size?: "sm" | "md" | "lg";
  live?: boolean;
}) {
  const [failedUrl, setFailedUrl] = useState<string | null>(null);
  const showImage = Boolean(url) && failedUrl !== url;
  return (
    <span className={`avatar avatar-${size} ${live ? "avatar-live" : ""}`} title={name}>
      <span className="avatar-fallback" aria-hidden="true">{name.slice(0, 1)}</span>
      {showImage ? (
        <img
          key={url}
          src={url}
          alt={name}
          loading="lazy"
          decoding="async"
          referrerPolicy="no-referrer"
          onError={() => setFailedUrl(url)}
        />
      ) : null}
      {live ? <i className="live-dot" aria-label="直播中" /> : null}
    </span>
  );
}

export function Toggle({
  checked,
  onChange,
  label,
  ariaLabel,
  disabled = false,
  labelFirst = false,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
  ariaLabel?: string;
  disabled?: boolean;
  labelFirst?: boolean;
}) {
  return (
    <label
      className={`toggle ${disabled ? "is-disabled" : ""} ${labelFirst && label ? "is-label-first" : ""}`}
      aria-label={ariaLabel}
    >
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(event) => onChange(event.target.checked)}
      />
      <span className="toggle-track" aria-hidden="true" />
      {label ? <span>{label}</span> : null}
    </label>
  );
}

export function Modal({
  title,
  children,
  onClose,
  wide = false,
  footer,
}: {
  title: string;
  children: ReactNode;
  onClose: () => void;
  wide?: boolean;
  footer?: ReactNode;
}) {
  return (
    <div className="modal-backdrop" role="presentation" onMouseDown={onClose}>
      <section
        className={`modal ${wide ? "modal-wide" : ""}`}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <header className="modal-header">
          <h2>{title}</h2>
          <button className="icon-button" onClick={onClose} aria-label="关闭">
            <X size={18} />
          </button>
        </header>
        <div className="modal-body">{children}</div>
        {footer ? <footer className="modal-footer">{footer}</footer> : null}
      </section>
    </div>
  );
}

export function EmptyState({
  title,
  description,
  action,
}: {
  title: string;
  description: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <div className="empty-orbit" aria-hidden="true">
        <span />
      </div>
      <h3>{title}</h3>
      <p>{description}</p>
      {action}
    </div>
  );
}

export function ErrorBanner({ message, onRetry }: { message: string; onRetry?: () => void }) {
  return (
    <div className="error-banner" role="alert">
      <span>{message}</span>
      {onRetry ? (
        <button className="text-button" onClick={onRetry}>
          重试
        </button>
      ) : null}
    </div>
  );
}

export function SubscriptionSelect({
  subscriptions,
  value,
  onChange,
}: {
  subscriptions: Streamer[];
  value: number | null;
  onChange: (value: number) => void;
}) {
  return (
    <select
      value={value ?? ""}
      onChange={(event) => onChange(Number(event.target.value))}
      required
    >
      <option value="" disabled>
        选择主播
      </option>
      {subscriptions.map((streamer) => (
        <option key={streamer.id} value={streamer.id}>
          {streamer.name}
        </option>
      ))}
    </select>
  );
}

