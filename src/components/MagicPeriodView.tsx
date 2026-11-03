import { useCallback, useEffect, useMemo, useState } from "react";
import { ExternalLink, Flower2, RefreshCw } from "lucide-react";
import type { FertilitySnapshot, FertilityStatus, Streamer } from "../types";
import { explainError, openExternalUrl } from "../lib/api";
import { Avatar, EmptyState, ErrorBanner } from "./ui";

interface FertilityActions {
  load: (refresh: boolean) => Promise<FertilitySnapshot>;
}

const STATUS_META: Record<string, { label: string; emoji: string; className: string }> = {
  menstruating: { label: "魔法期", emoji: "🩸", className: "is-menstruating" },
  fertile: { label: "易孕期", emoji: "🌸", className: "is-fertile" },
  ovulating: { label: "排卵期", emoji: "🥚", className: "is-ovulating" },
  normal: { label: "安全期", emoji: "💚", className: "is-normal" },
};

const STATUS_ORDER = ["menstruating", "fertile", "ovulating", "normal"];

function statusMeta(status: string) {
  return STATUS_META[status] ?? { label: "未知", emoji: "•", className: "is-unknown" };
}

function formatNextPeriod(value: string | null): string {
  if (!value) {
    return "暂未预测";
  }
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  return match ? `${Number(match[2])}月${Number(match[3])}日` : value;
}

function formatFetchedAt(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return "未知";
  }
  return new Intl.DateTimeFormat("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}

function sourceLabel(source: string): string {
  if (source === "laplace") {
    return "LAPLACE 直连";
  }
  if (source === "jina-reader") {
    return "LAPLACE · Reader 代理";
  }
  return source === "none" ? "无数据" : source;
}

export default function MagicPeriodView({
  subscriptions,
  actions,
  onError,
}: {
  subscriptions: Streamer[];
  actions: FertilityActions;
  onError: (message: string) => void;
}) {
  const [snapshot, setSnapshot] = useState<FertilitySnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async (refresh: boolean) => {
    if (refresh) {
      setRefreshing(true);
    } else {
      setLoading(true);
    }
    setError(null);
    try {
      setSnapshot(await actions.load(refresh));
    } catch (loadError) {
      const message = explainError(loadError);
      setError(message);
      onError(message);
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, [actions, onError]);

  useEffect(() => {
    void load(false);
  }, [load]);

  const groups = useMemo(() => {
    const statuses = snapshot?.statuses ?? [];
    return STATUS_ORDER.map((status) => {
      const meta = statusMeta(status);
      return {
        status,
        meta,
        items: statuses.filter((item) => item.status === status),
      };
    }).filter((group) => group.items.length > 0);
  }, [snapshot]);

  const unknownStatuses = useMemo(
    () => (snapshot?.statuses ?? []).filter((item) => !STATUS_ORDER.includes(item.status)),
    [snapshot],
  );

  const openSource = async () => {
    try {
      await openExternalUrl("https://laplace.live/ovu");
    } catch (openError) {
      onError(explainError(openError));
    }
  };

  return (
    <section className="page fertility-page">
      <header className="page-header">
        <div>
          <p className="eyebrow">MAGIC PERIOD</p>
          <h1>魔法期</h1>
          <p className="page-subtitle">查看已订阅主播当前的生理周期状态，数据来自 LAPLACE。</p>
        </div>
        <div className="toolbar-actions">
          <button className="button secondary" onClick={() => void openSource()}>
            <ExternalLink size={16} />查看原站
          </button>
          <button className="button primary" onClick={() => void load(true)} disabled={loading || refreshing}>
            <RefreshCw className={refreshing ? "spin" : ""} size={16} />{refreshing ? "刷新中…" : "刷新数据"}
          </button>
        </div>
      </header>

      <div className="fertility-note">
        <Flower2 size={20} />
        <div>
          <strong>娱乐参考，不是医疗信息</strong>
          <span>LAPLACE 的周期状态由用户贡献和预测生成；页面只展示与当前订阅匹配的数据。</span>
        </div>
        {snapshot ? (
          <span className="fertility-source">
            {sourceLabel(snapshot.source)} · {formatFetchedAt(snapshot.fetchedAt)}
          </span>
        ) : null}
      </div>

      {snapshot?.warning ? <div className="fertility-warning">{snapshot.warning}</div> : null}
      {error ? <ErrorBanner message={error} onRetry={() => void load(true)} /> : null}

      {loading && !snapshot ? (
        <div className="fertility-loading"><RefreshCw className="spin" size={20} />正在抓取魔法期数据…</div>
      ) : subscriptions.length === 0 ? (
        <EmptyState title="还没有订阅主播" description="先在主播库添加主播，再回来查看魔法期。" />
      ) : snapshot && snapshot.statuses.length === 0 ? (
        <EmptyState
          title="暂时没有匹配数据"
          description="LAPLACE 目前没有收录你的订阅主播，或接口暂时不可用。可以稍后刷新，或直接打开原站查看。"
          action={<button className="button secondary" onClick={() => void openSource()}>打开 LAPLACE</button>}
        />
      ) : (
        <>
          <div className="fertility-summary">
            <strong>{snapshot?.matched ?? 0}<span>位已收录</span></strong>
            <strong>{Math.max(0, subscriptions.length - (snapshot?.matched ?? 0))}<span>位未收录</span></strong>
            <strong>{snapshot?.totalAvailable ?? 0}<span>全站样本</span></strong>
          </div>
          <div className="fertility-groups">
            {groups.map((group) => (
              <section className={`fertility-group ${group.meta.className}`} key={group.status}>
                <header className="fertility-group-head">
                  <div><span>{group.meta.emoji}</span><h2>{group.meta.label}</h2></div>
                  <span>{group.items.length} 位</span>
                </header>
                <div className="fertility-grid">
                  {group.items.map((item) => <FertilityCard item={item} key={item.streamerId} />)}
                </div>
              </section>
            ))}
            {unknownStatuses.length ? (
              <section className="fertility-group is-unknown">
                <header className="fertility-group-head"><div><span>•</span><h2>其他状态</h2></div><span>{unknownStatuses.length} 位</span></header>
                <div className="fertility-grid">
                  {unknownStatuses.map((item) => <FertilityCard item={item} key={item.streamerId} />)}
                </div>
              </section>
            ) : null}
          </div>
        </>
      )}
    </section>
  );
}

function FertilityCard({ item }: { item: FertilityStatus }) {
  const meta = statusMeta(item.status);
  return (
    <article className={`fertility-card ${meta.className}`}>
      <Avatar url={item.streamerAvatar} name={item.streamerName} size="lg" />
      <div className="fertility-card-main">
        <div className="fertility-card-title">
          <strong>{item.streamerName}</strong>
          <span className="fertility-status-pill">{meta.emoji} {meta.label}</span>
        </div>
        <p>
          UID {item.mid}
          {item.dayInCycle !== null ? ` · 周期第 ${item.dayInCycle} 天` : ""}
          {item.dataPoints > 0 ? ` · ${item.dataPoints} 条记录` : ""}
        </p>
        <div className="fertility-card-meta">
          <span>下次魔法期 <b>{formatNextPeriod(item.nextPeriod)}</b></span>
          {item.cycleLength !== null ? <span>周期 {item.cycleLength} 天</span> : null}
          {item.periodLength !== null ? <span>经期 {item.periodLength} 天</span> : null}
        </div>
      </div>
    </article>
  );
}
