#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
laplace.live 生产版爬虫 —— 纯 HTTP，无浏览器
=============================================
实测结论 (2026-11-07):
  * workers.vrp.moe 的接口零校验: 不需要 Origin / Referer / Cookie / cf_clearance,
    连 requests 默认的 python-requests UA 都能 200。
  * Session + 连接池复用下, 6 并发可跑到 42 req/s 零错误;
    不做连接池时 16 并发才开始出现 ConnectionError(本地 socket 耗尽, 非封禁)。
  * laplace.live 前端 15 次快速连打全 200; 之前的 429 是短时高频探测触发的
    Vercel 限速, 几分钟自动恢复 —— 且前端 HTML 本身不含数据, 服务器上完全不需要访问它。

因此服务器部署方案: 纯 requests + 令牌桶限速, 不需要 Chromium。

用法:
  python laplace_scraper_http.py --out ./data                 # 全量增量抓取
  python laplace_scraper_http.py --out ./data --rate 8        # 限速 8 req/s
  python laplace_scraper_http.py --out ./data --workers 6
  python laplace_scraper_http.py --out ./data --no-details     # 只抓列表+统计
  python laplace_scraper_http.py --out ./data --export-only    # 只重新导出
"""

from __future__ import annotations

import argparse
import csv
import json
import random
import sqlite3
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timedelta, timezone
from pathlib import Path

import requests

WORKER = "https://workers.vrp.moe"
UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
      "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
CST = timezone(timedelta(hours=8))

STATUS_CN = {"normal": "常规", "menstruating": "经期", "fertile": "易孕期", "ovulating": "排卵期"}
LIVE_CN = {0: "未开播", 1: "直播中", 2: "未开播", 3: "轮播"}


# --------------------------------------------------------------------------- #
# 令牌桶限速器
# --------------------------------------------------------------------------- #
class RateLimiter:
    """令牌桶：长期速率 = rate req/s，突发容量 = burst。

    默认 burst = max(1, rate/2)，避免开场一次性打出整桶请求。
    """

    def __init__(self, rate: float, burst: float | None = None):
        self.rate = max(rate, 0.1)
        self.capacity = max(1.0, burst if burst is not None else self.rate / 2)
        self.tokens = min(1.0, self.capacity)
        self.updated = time.monotonic()
        self.lock = threading.Lock()

    def acquire(self):
        with self.lock:
            while True:
                now = time.monotonic()
                self.tokens = min(self.capacity, self.tokens + (now - self.updated) * self.rate)
                self.updated = now
                if self.tokens >= 1.0:
                    self.tokens -= 1.0
                    return
                time.sleep((1.0 - self.tokens) / self.rate)


# --------------------------------------------------------------------------- #
# HTTP 客户端: 连接池 + 限速 + 指数退避重试
# --------------------------------------------------------------------------- #
class Client:
    def __init__(self, rate: float, pool: int = 8):
        self.limiter = RateLimiter(rate)
        self.local = threading.local()
        self.pool = pool
        self.requests_made = 0
        self.lock = threading.Lock()

    def _session(self) -> requests.Session:
        s = getattr(self.local, "s", None)
        if s is None:
            s = requests.Session()
            adapter = requests.adapters.HTTPAdapter(
                pool_connections=self.pool, pool_maxsize=self.pool,
                max_retries=0, pool_block=False)
            s.mount("https://", adapter)
            s.mount("http://", adapter)
            s.headers.update({"User-Agent": UA, "Accept": "application/json",
                              "Accept-Language": "zh-CN,zh;q=0.9"})
            self.local.s = s
        return s

    def get(self, path: str, retries: int = 5):
        url = path if path.startswith("http") else WORKER + path
        last = None
        for attempt in range(retries):
            self.limiter.acquire()
            try:
                with self.lock:
                    self.requests_made += 1
                r = self._session().get(url, timeout=25)
                if r.status_code == 200:
                    try:
                        return r.json()
                    except ValueError:
                        last = f"非 JSON: {r.text[:80]}"
                elif r.status_code in (429, 503):
                    wait = min(30, 2 ** attempt) + random.uniform(0, 1)
                    print(f"  [限速 {r.status_code}] {path} 等待 {wait:.1f}s", file=sys.stderr)
                    time.sleep(wait)
                    continue
                elif r.status_code == 404:
                    return None                      # 角色已被删除
                else:
                    last = f"HTTP {r.status_code}"
            except requests.RequestException as e:
                last = f"{type(e).__name__}: {str(e)[:80]}"
            time.sleep(min(8, 0.8 * 2 ** attempt) + random.uniform(0, 0.4))
        print(f"  [失败] {path} -> {last}", file=sys.stderr)
        return None


# --------------------------------------------------------------------------- #
# 抓取
# --------------------------------------------------------------------------- #
def crawl(out: Path, workers: int, rate: float, with_details: bool, refresh: bool):
    out.mkdir(parents=True, exist_ok=True)
    raw = out / "raw"
    detail_dir = raw / "details"
    raw.mkdir(exist_ok=True)
    detail_dir.mkdir(exist_ok=True)

    cli = Client(rate, pool=max(8, workers))
    t0 = time.time()

    print("[1/4] 拉取列表 / 统计 / 汇率")
    cur = cli.get("/laplace/fertilities")
    full = cli.get("/laplace/fertilities?showOutdated=1")
    stats = cli.get("/laplace/fertility-contribution-stats")
    fx = cli.get("/experiments/currency?symbols=CNY")

    for name, obj in (("fertilities_current.json", cur), ("fertilities_all.json", full)):
        if obj:
            (raw / name).write_text(json.dumps(obj, ensure_ascii=False), encoding="utf-8")
    if stats:
        (raw / "contribution_stats.json").write_text(
            json.dumps(stats, ensure_ascii=False, indent=1), encoding="utf-8")
    if fx:
        (raw / "currency.json").write_text(
            json.dumps(fx, ensure_ascii=False, indent=1), encoding="utf-8")

    base = full or cur
    if not base:
        print("列表拉取失败，终止", file=sys.stderr)
        return 1
    roster = base["data"]
    cur_ids = {x["uid"] for x in (cur or {}).get("data", [])}
    print(f"      在册 {len(cur_ids)} / 全量 {len(roster)} / 投稿 {stats and stats.get('total')}")

    details: dict[int, dict] = {}
    if not with_details:
        print("[2/4] 跳过详情 (--no-details)")
    else:
        todo = []
        for it in roster:
            fp = detail_dir / f"{it['uid']}.json"
            if fp.exists() and not refresh and fp.stat().st_size > 20:
                try:
                    details[it["uid"]] = json.loads(fp.read_text(encoding="utf-8"))
                    continue
                except Exception:
                    pass
            todo.append(it["uid"])
        cached = len(roster) - len(todo)
        print(f"[2/4] 详情: 新增 {len(todo)} / 缓存命中 {cached} / 并发 {workers} / 限速 {rate} req/s")

        done = 0
        lock = threading.Lock()

        def one(uid):
            nonlocal done
            d = cli.get(f"/laplace/fertility/{uid}")
            if d:
                (detail_dir / f"{uid}.json").write_text(
                    json.dumps(d, ensure_ascii=False), encoding="utf-8")
            with lock:
                done += 1
                if done % 200 == 0 or done == len(todo):
                    el = time.time() - t0
                    print(f"      {done}/{len(todo)}  {el:.0f}s  "
                          f"{cli.requests_made/max(el,1):.1f} req/s")
            return uid, d

        with ThreadPoolExecutor(max_workers=workers) as ex:
            futs = [ex.submit(one, u) for u in todo]
            for f in as_completed(futs):
                uid, d = f.result()
                if d:
                    details[uid] = d

        # 回填缓存
        for uid in roster_uids(roster):
            if uid not in details:
                fp = detail_dir / f"{uid}.json"
                if fp.exists():
                    try:
                        details[uid] = json.loads(fp.read_text(encoding="utf-8"))
                    except Exception:
                        pass

    print("[3/4] 导出")
    export(out, roster, details, stats, fx, base.get("total"), cur_ids)
    print(f"[4/4] 完成 {time.time()-t0:.0f}s, 请求数 {cli.requests_made} -> {out}")
    return 0


def roster_uids(roster):
    return [it["uid"] for it in roster]


# --------------------------------------------------------------------------- #
# 扁平化
# --------------------------------------------------------------------------- #
def ts(ms):
    if not ms:
        return ""
    try:
        return datetime.fromtimestamp(ms / 1000, CST).strftime("%Y-%m-%d %H:%M:%S")
    except Exception:
        return ""


def flatten(item: dict, detail: dict | None) -> dict:
    u = item.get("user") or {}
    gi = u.get("guildInfo") or {}
    hist = gi.get("history") or []
    guild = ((gi.get("current") or {}).get("name")
             or (hist[-1].get("name") if hist else "") or "")
    guild_trail = " > ".join(h.get("name", "") for h in hist) if hist else guild
    mi = u.get("mcnInfo") or {}
    mcn = " > ".join(h.get("mcnName", "") for h in (mi.get("history") or []))
    acc = item.get("predictionAccuracy") or {}
    h = (detail or {}).get("history") or []
    pref = (detail or {}).get("userPreferences") or {}
    periods = [e.get("periodStart") for e in h if e.get("periodStart")]

    return {
        "uid": item.get("uid"),
        "昵称": u.get("username", ""),
        "B站直播间": u.get("room", ""),
        "直播状态": LIVE_CN.get(u.get("liveStatus"), u.get("liveStatus")),
        "粉丝数": u.get("liveFansCount"),
        "公会": guild,
        "公会变迁": guild_trail,
        "MCN": mcn,
        "头像": u.get("avatar", ""),
        "生理状态": STATUS_CN.get(item.get("status"), item.get("status")),
        "状态原始值": item.get("status"),
        "周期内第几天": item.get("dayInCycle"),
        "排卵日": item.get("ovulationDay"),
        "下次经期": item.get("nextPeriod"),
        "有效周期长度": item.get("effectiveCycleLength"),
        "有效经期长度": item.get("effectivePeriodLength"),
        "数据点": item.get("dataPoints"),
        "观测后已过周期": item.get("cyclesElapsedSinceObservation"),
        "平均绝对误差(天)": acc.get("meanAbsErrorDays"),
        "平均误差(天)": acc.get("meanErrorDays"),
        "预测样本数": acc.get("samples"),
        "历史记录数": len(h),
        "偏好周期长度": pref.get("cycleLength", ""),
        "偏好经期长度": pref.get("periodLength", ""),
        "首次记录日期": ts(min(periods)) if periods else "",
        "最近记录日期": ts(max(periods)) if periods else "",
        "历史JSON": json.dumps(h, ensure_ascii=False) if h else "",
    }


# --------------------------------------------------------------------------- #
# 导出
# --------------------------------------------------------------------------- #
def export(out: Path, roster, details, stats, fx, total, cur_ids=None):
    from collections import Counter

    rows = [flatten(it, details.get(it["uid"])) for it in roster]
    for it, r in zip(roster, rows):
        r["在册"] = "是" if (cur_ids is None or it["uid"] in cur_ids) else "已过期"
    cols = list(rows[0].keys())
    now = datetime.now(CST).strftime("%Y-%m-%d %H:%M:%S")

    with open(out / "laplace_fertilities.csv", "w", newline="", encoding="utf-8-sig") as f:
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        w.writerows(rows)

    with open(out / "laplace_fertilities.jsonl", "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")

    hist_rows = []
    for it in roster:
        for i, e in enumerate((details.get(it["uid"]) or {}).get("history") or []):
            hist_rows.append({
                "uid": it["uid"], "username": (it.get("user") or {}).get("username"),
                "seq": i, "periodStart": e.get("periodStart"),
                "periodStartDate": ts(e.get("periodStart")),
                "submittedAt": ts(e.get("submittedAt")), "source": e.get("source"),
                "prediction": e.get("prediction")})
    with open(out / "laplace_history.jsonl", "w", encoding="utf-8") as f:
        for r in hist_rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")

    merged = []
    for it in roster:
        d = details.get(it["uid"]) or {}
        m = dict(it)
        m["history"] = d.get("history", [])
        m["userPreferences"] = d.get("userPreferences")
        m["isCurrent"] = None if cur_ids is None else it["uid"] in cur_ids
        merged.append(m)
    (out / "laplace_full.json").write_text(json.dumps(
        {"scrapedAt": now, "total": total, "count": len(merged), "data": merged},
        ensure_ascii=False, indent=1), encoding="utf-8")

    # SQLite
    db = out / "laplace.db"
    if db.exists():
        db.unlink()
    con = sqlite3.connect(db)
    c = con.cursor()
    c.execute("""CREATE TABLE characters(uid INTEGER PRIMARY KEY, username TEXT, room INTEGER,
        live_status TEXT, fans INTEGER, guild TEXT, guild_trail TEXT, mcn TEXT, avatar TEXT,
        status TEXT, day_in_cycle INTEGER, ovulation_day INTEGER, next_period TEXT,
        cycle_len INTEGER, period_len INTEGER, data_points INTEGER, cycles_elapsed INTEGER,
        mae REAL, mean_error REAL, samples INTEGER, history_count INTEGER,
        first_record TEXT, last_record TEXT, pref_cycle INTEGER, pref_period INTEGER,
        is_current INTEGER)""")
    c.execute("""CREATE TABLE history(uid INTEGER, seq INTEGER, period_start_date TEXT,
        submitted_at TEXT, source TEXT, predicted_start TEXT, error_days REAL, method TEXT,
        PRIMARY KEY(uid,seq))""")
    c.execute("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT)")
    for r in rows:
        c.execute("INSERT OR REPLACE INTO characters VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (
            r["uid"], r["昵称"], r["B站直播间"], str(r["直播状态"]), r["粉丝数"], r["公会"],
            r["公会变迁"], r["MCN"], r["头像"], r["状态原始值"], r["周期内第几天"], r["排卵日"],
            r["下次经期"], r["有效周期长度"], r["有效经期长度"], r["数据点"], r["观测后已过周期"],
            r["平均绝对误差(天)"], r["平均误差(天)"], r["预测样本数"], r["历史记录数"],
            r["首次记录日期"], r["最近记录日期"], r["偏好周期长度"] or None,
            r["偏好经期长度"] or None, 1 if r["在册"] == "是" else 0))
    for e in hist_rows:
        p = e["prediction"] or {}
        c.execute("INSERT OR REPLACE INTO history VALUES(?,?,?,?,?,?,?,?)", (
            e["uid"], e["seq"], e["periodStartDate"], e["submittedAt"], e["source"],
            p.get("predictedStart"), p.get("errorDays"), p.get("method")))
    c.execute("INSERT INTO meta VALUES('stats',?)", (json.dumps(stats, ensure_ascii=False),))
    c.execute("INSERT INTO meta VALUES('currency',?)", (json.dumps(fx, ensure_ascii=False),))
    c.execute("INSERT INTO meta VALUES('scrapedAt',?)", (now,))
    con.commit()
    con.close()

    # XLSX
    xlsx_note = ""
    try:
        from openpyxl import Workbook
        from openpyxl.styles import Alignment, Font, PatternFill
        from openpyxl.utils import get_column_letter

        wb = Workbook()
        ws = wb.active
        ws.title = "角色数据"
        ws.append(cols)
        fill = PatternFill("solid", fgColor="1F4E79")
        for cell in ws[1]:
            cell.font = Font(bold=True, color="FFFFFF")
            cell.fill = fill
            cell.alignment = Alignment(horizontal="center", vertical="center")
        for r in rows:
            ws.append([r[k] for k in cols])
        ws.freeze_panes = "C2"
        ws.auto_filter.ref = ws.dimensions
        for i, k in enumerate(cols, 1):
            sample = [len(str(r[k])) for r in rows[:300]]
            ws.column_dimensions[get_column_letter(i)].width = min(
                max([len(str(k)) * 2] + sample) + 2, 40)
        ws.column_dimensions[get_column_letter(cols.index("历史JSON") + 1)].hidden = True

        ws4 = wb.create_sheet("状态分布")
        ws4.append(["生理状态", "人数", "占比"])
        cnt = Counter(r["生理状态"] for r in rows)
        for k, v in cnt.most_common():
            ws4.append([k, v, round(v / len(rows) * 100, 1)])
        for cell in ws4[1]:
            cell.font = Font(bold=True)

        ws5 = wb.create_sheet("公会人数")
        ws5.append(["公会", "签约人数", "总粉丝数"])
        g = {}
        for r in rows:
            key = r["公会"] or "(无)"
            a = g.setdefault(key, [0, 0])
            a[0] += 1
            a[1] += r["粉丝数"] or 0
        for k, (n, fans) in sorted(g.items(), key=lambda x: -x[1][0]):
            ws5.append([k, n, fans])
        for cell in ws5[1]:
            cell.font = Font(bold=True)

        if stats:
            acc = stats.get("predictionAccuracy") or {}
            ov = acc.get("overall") or {}
            ws2 = wb.create_sheet("投稿统计")
            ws2.append(["指标", "数值"])
            for cell in ws2[1]:
                cell.font = Font(bold=True)
            pairs = [(k, v) for k, v in stats.items() if k != "predictionAccuracy"]
            pairs += [("预测平均绝对误差(天)", ov.get("meanAbsErrorDays")),
                      ("预测平均误差(天)", ov.get("meanErrorDays")),
                      ("预测样本数", ov.get("samples")),
                      ("有预测数据的角色数", acc.get("users")),
                      ("汇率USD->CNY", ((fx or {}).get("rates") or {}).get("CNY")),
                      ("抓取时间", now)]
            for k, v in pairs:
                ws2.append([k, v])
            ws2.column_dimensions["A"].width = 30
            ws2.column_dimensions["B"].width = 24

            ws3 = wb.create_sheet("按月准确率")
            ws3.append(["月份", "平均绝对误差(天)", "平均误差(天)", "样本数"])
            for cell in ws3[1]:
                cell.font = Font(bold=True)
            for m in acc.get("months") or []:
                ws3.append([m.get("month"), m.get("meanAbsErrorDays"),
                            m.get("meanErrorDays"), m.get("samples")])
            for col, w2 in zip("ABCD", (12, 20, 16, 10)):
                ws3.column_dimensions[col].width = w2

        ws6 = wb.create_sheet("历史记录")
        ws6.append(["uid", "昵称", "第几条", "经期开始", "提交时间", "来源", "预测值", "误差天数", "预测方法"])
        for cell in ws6[1]:
            cell.font = Font(bold=True)
        for e in hist_rows:
            p = e["prediction"] or {}
            ws6.append([e["uid"], e["username"], e["seq"] + 1, e["periodStartDate"],
                        e["submittedAt"], e["source"], p.get("predictedStart"),
                        p.get("errorDays"), p.get("method")])
        ws6.column_dimensions["B"].width = 22
        ws6.column_dimensions["E"].width = 20
        ws6.column_dimensions["F"].width = 52

        wb.save(out / "laplace_fertilities.xlsx")
        xlsx_note = "laplace_fertilities.xlsx"
    except ImportError:
        xlsx_note = "(未安装 openpyxl，跳过)"

    print(f"      角色 {len(rows)} | 历史 {len(hist_rows)} | 在册 {sum(1 for r in rows if r['在册']=='是')}")
    print(f"      csv / jsonl / json / db / {xlsx_note}")


def main():
    ap = argparse.ArgumentParser(description="laplace.live 生产版爬虫（纯 HTTP）")
    ap.add_argument("--out", default="data")
    ap.add_argument("--workers", type=int, default=6, help="并发线程数")
    ap.add_argument("--rate", type=float, default=8.0, help="全局速率上限 req/s")
    ap.add_argument("--no-details", action="store_true", help="只抓列表与统计")
    ap.add_argument("--refresh", action="store_true", help="忽略详情缓存，全部重抓")
    ap.add_argument("--export-only", action="store_true", help="只从 raw 重新导出")
    a = ap.parse_args()

    out = Path(a.out).resolve()
    if a.export_only:
        raw = out / "raw"
        base = json.loads((raw / "fertilities_all.json").read_text(encoding="utf-8"))
        cur_p = raw / "fertilities_current.json"
        cur = json.loads(cur_p.read_text(encoding="utf-8")) if cur_p.exists() else None
        stats_p, fx_p = raw / "contribution_stats.json", raw / "currency.json"
        stats = json.loads(stats_p.read_text(encoding="utf-8")) if stats_p.exists() else None
        fx = json.loads(fx_p.read_text(encoding="utf-8")) if fx_p.exists() else None
        details = {}
        for fp in (raw / "details").glob("*.json"):
            try:
                details[int(fp.stem)] = json.loads(fp.read_text(encoding="utf-8"))
            except Exception:
                pass
        export(out, base["data"], details, stats, fx, base.get("total"),
               {x["uid"] for x in (cur or {}).get("data", [])} or None)
        return 0

    return crawl(out, a.workers, a.rate, not a.no_details, a.refresh)


if __name__ == "__main__":
    sys.exit(main())
