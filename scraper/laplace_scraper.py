#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
laplace.live /ovu 全站数据爬虫
--------------------------------
站点结构:
  laplace.live          : Next.js 前端, 由 Vercel Bot Protection 保护 (X-Vercel-Mitigated: challenge)
  workers.vrp.moe       : 真正的数据后端 (Cloudflare 保护), CORS 只放行 laplace.live
  backend.kaiousei.studio: 埋点服务 (/api/event), 非业务数据

接口清单 (从 _next chunk 中提取):
  GET  /laplace/fertilities[?showOutdated=1]        全量列表 (998 / 1638)
  GET  /laplace/fertility/{uid}                     单角色详情 (含 history + userPreferences)
  GET  /laplace/fertility-contribution-stats        投稿统计 + 预测准确率
  GET  /laplace/bilibili-user/{uid}                 B 站原始信息 (含粉丝牌)
  GET  /experiments/currency?symbols=CNY            汇率 (页面上展示用)
  POST /laplace/fertility-contribute                投稿 (只写, 本脚本不调用)

反爬突破:
  1. 纯 requests 直连 laplace.live 必然 429 (Vercel challenge)、直连 workers 大概率
     Cloudflare 1020/JS 挑战。
  2. 用 Playwright 真实 Chromium 过 Vercel checkpoint, 拿到 cf_clearance 等 cookie。
  3. 数据请求不走跨域 fetch (会被 laplace.live 的 CSP connect-src 拦), 而是在
     workers.vrp.moe 源下开标签页, 用同源 fetch 打接口 —— 天然绕过 CORS 与 CSP。
  4. 详情接口按并发 + 退避抓取, 遇到 429/403 自动重建浏览器上下文重新过闸。

用法:
  python laplace_scraper.py --out ./out                  # 列表+统计+全部详情
  python laplace_scraper.py --out ./out --list-only      # 只抓列表和统计
  python laplace_scraper.py --out ./out --concurrency 6  # 调并发
"""

from __future__ import annotations

import argparse
import asyncio
import csv
import json
import os
import sqlite3
import sys
import time
from datetime import datetime, timezone, timedelta
from pathlib import Path

from playwright.async_api import async_playwright, Browser, BrowserContext

WEB = "https://laplace.live/ovu"
WORKER = "https://workers.vrp.moe"
UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
      "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
CST = timezone(timedelta(hours=8))

FETCH_JS = """async (u) => {
  const r = await fetch(u, {credentials:'include', headers:{'accept':'application/json'}});
  const t = await r.text();
  return {status: r.status, body: t};
}"""

STATUS_CN = {
    "normal": "常规",
    "menstruating": "经期",
    "fertile": "易孕期",
    "ovulating": "排卵期",
}
LIVE_CN = {0: "未开播", 1: "直播中", 2: "未开播", 3: "轮播"}


# --------------------------------------------------------------------------- #
# 反爬浏览器管理器
# --------------------------------------------------------------------------- #
class Session:
    def __init__(self, headless: bool = True):
        self.headless = headless
        self.pw = None
        self.browser: Browser | None = None
        self.ctx: BrowserContext | None = None
        self.tab = None  # workers.vrp.moe 同源标签页

    async def start(self):
        self.pw = await async_playwright().start()
        await self._spawn()

    async def _spawn(self):
        self.browser = await self.pw.chromium.launch(
            headless=self.headless,
            args=[
                "--no-sandbox",
                "--disable-dev-shm-usage",
                "--disable-blink-features=AutomationControlled",
            ],
        )
        self.ctx = await self.browser.new_context(
            locale="zh-CN",
            timezone_id="Asia/Shanghai",
            user_agent=UA,
            viewport={"width": 1440, "height": 900},
        )
        await self.ctx.add_init_script(
            "Object.defineProperty(navigator,'webdriver',{get:()=>undefined});"
        )
        page = await self.ctx.new_page()
        await page.goto(WEB, wait_until="domcontentloaded", timeout=90000)
        for _ in range(45):
            await page.wait_for_timeout(1000)
            try:
                if "Security Checkpoint" not in await page.title():
                    break
            except Exception:
                break
        await page.wait_for_timeout(2500)
        # 在 worker 源下建标签页: 后续全部接口用同源 fetch
        self.tab = await self.ctx.new_page()
        await self.tab.goto(f"{WORKER}/laplace/fertility-contribution-stats",
                            wait_until="domcontentloaded", timeout=60000)
        await self.tab.wait_for_timeout(1500)

    async def get(self, path: str, retries: int = 4) -> dict | None:
        """同源 fetch 打 worker 接口；失败自动重建会话。"""
        url = path if path.startswith("http") else WORKER + path
        for attempt in range(retries):
            try:
                res = await self.tab.evaluate(FETCH_JS, url)
                if res["status"] == 200:
                    return json.loads(res["body"])
                if res["status"] in (403, 429, 503) or "Just a moment" in res["body"][:400]:
                    print(f"  [被拦 {res['status']}] {path} -> 重建会话", file=sys.stderr)
                    await self.restart()
                    continue
                print(f"  [HTTP {res['status']}] {path}", file=sys.stderr)
            except Exception as e:
                print(f"  [异常] {path}: {str(e)[:110]}", file=sys.stderr)
                try:
                    await self.restart()
                except Exception:
                    pass
            await asyncio.sleep(1.5 * (attempt + 1))
        return None

    async def restart(self):
        try:
            await self.browser.close()
        except Exception:
            pass
        await asyncio.sleep(1.0)
        await self._spawn()

    async def close(self):
        try:
            await self.browser.close()
        except Exception:
            pass
        if self.pw:
            await self.pw.stop()


# --------------------------------------------------------------------------- #
# 抓取
# --------------------------------------------------------------------------- #
async def crawl(out: Path, concurrency: int = 5, list_only: bool = False,
                headless: bool = True):
    out.mkdir(parents=True, exist_ok=True)
    raw = out / "raw"
    raw.mkdir(exist_ok=True)

    t0 = time.time()
    sess = Session(headless=headless)
    await sess.start()
    print("[1/4] 反爬会话就绪")

    # --- 列表 ---
    print("[2/4] 拉取列表 /laplace/fertilities (含 showOutdated)")
    cur = await sess.get("/laplace/fertilities")
    full = await sess.get("/laplace/fertilities?showOutdated=1")
    stats = await sess.get("/laplace/fertility-contribution-stats")
    fx = await sess.get("/experiments/currency?symbols=CNY")

    if cur:
        (raw / "fertilities_current.json").write_text(
            json.dumps(cur, ensure_ascii=False), encoding="utf-8")
    if full:
        (raw / "fertilities_all.json").write_text(
            json.dumps(full, ensure_ascii=False), encoding="utf-8")
    if stats:
        (raw / "contribution_stats.json").write_text(
            json.dumps(stats, ensure_ascii=False, indent=1), encoding="utf-8")
    if fx:
        (raw / "currency.json").write_text(
            json.dumps(fx, ensure_ascii=False, indent=1), encoding="utf-8")

    base = full or cur
    if not base:
        print("列表拉取失败, 终止", file=sys.stderr)
        await sess.close()
        return 1
    roster = base["data"]
    print(f"      列表 total={base.get('total')} 实取={len(roster)} | 统计total={stats and stats.get('total')}")

    details: dict[int, dict] = {}
    detail_dir = raw / "details"
    detail_dir.mkdir(exist_ok=True)

    if list_only:
        print("[3/4] 跳过详情 (--list-only)")
    else:
        print(f"[3/4] 拉取 {len(roster)} 个角色详情 (含 history), 并发={concurrency}")
        sem = asyncio.Semaphore(concurrency)
        done = 0
        lock = asyncio.Lock()

        async def one(uid: int):
            nonlocal done
            fp = detail_dir / f"{uid}.json"
            if fp.exists() and fp.stat().st_size > 20:
                try:
                    details[uid] = json.loads(fp.read_text(encoding="utf-8"))
                    async with lock:
                        done += 1
                    return
                except Exception:
                    pass
            async with sem:
                d = await sess.get(f"/laplace/fertility/{uid}")
            if d:
                details[uid] = d
                fp.write_text(json.dumps(d, ensure_ascii=False), encoding="utf-8")
            async with lock:
                done += 1
                if done % 100 == 0:
                    print(f"      {done}/{len(roster)}  ({(time.time()-t0):.0f}s)")

        await asyncio.gather(*(one(x["uid"]) for x in roster))
        print(f"      详情成功 {len(details)}/{len(roster)}")

    await sess.close()

    # --- 落库 ---
    print("[4/4] 导出 CSV / JSONL / SQLite / XLSX")
    export(out, roster, details, stats, fx, base.get("total"))
    print(f"完成, 用时 {time.time()-t0:.0f}s -> {out}")
    return 0


# --------------------------------------------------------------------------- #
# 扁平化 + 导出
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
    cur_guild = (gi.get("current") or {}).get("name") or ""
    hist = gi.get("history") or []
    guild_names = " > ".join(h.get("name", "") for h in hist) if hist else cur_guild
    mi = u.get("mcnInfo") or {}
    mcn = " > ".join(h.get("mcnName", "") for h in (mi.get("history") or []))
    acc = item.get("predictionAccuracy") or {}
    h = (detail or {}).get("history") or []
    pref = (detail or {}).get("userPreferences") or {}
    periods = [e.get("periodStart") for e in h if e.get("periodStart")]
    sources = [e.get("source") for e in h if e.get("source")]
    preds = [e.get("prediction") for e in h if e.get("prediction")]

    return {
        "uid": item.get("uid"),
        "昵称": u.get("username", ""),
        "B站直播间": u.get("room", ""),
        "直播状态": LIVE_CN.get(u.get("liveStatus"), u.get("liveStatus")),
        "粉丝数": u.get("liveFansCount"),
        "公会": guild_names,
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
        "记录来源数": len(set(s for s in sources if s)),
        "历史预测命中数": len(preds),
        "历史JSON": json.dumps(h, ensure_ascii=False) if h else "",
    }


def export(out: Path, roster, details, stats, fx, total):
    rows = [flatten(it, details.get(it["uid"])) for it in roster]
    cols = list(rows[0].keys())

    # CSV (UTF-8 BOM, Excel 可直接打开)
    with open(out / "laplace_fertilities.csv", "w", newline="", encoding="utf-8-sig") as f:
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        w.writerows(rows)

    # JSONL
    with open(out / "laplace_fertilities.jsonl", "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")

    # 合并后的完整 JSON (列表 + 详情 + history)
    merged = []
    for it in roster:
        d = details.get(it["uid"]) or {}
        m = dict(it)
        m["history"] = d.get("history", [])
        m["userPreferences"] = d.get("userPreferences")
        merged.append(m)
    (out / "laplace_full.json").write_text(
        json.dumps({"scrapedAt": datetime.now(CST).isoformat(), "total": total,
                    "count": len(merged), "data": merged},
                   ensure_ascii=False, indent=1), encoding="utf-8")

    # SQLite
    db = out / "laplace.db"
    if db.exists():
        db.unlink()
    con = sqlite3.connect(db)
    cur = con.cursor()
    cur.execute("""CREATE TABLE characters(
        uid INTEGER PRIMARY KEY, username TEXT, room INTEGER, live_status INTEGER,
        fans INTEGER, guild TEXT, mcn TEXT, avatar TEXT, status TEXT,
        day_in_cycle INTEGER, ovulation_day INTEGER, next_period TEXT,
        cycle_len INTEGER, period_len INTEGER, data_points INTEGER,
        cycles_elapsed INTEGER, mae REAL, mean_error REAL, samples INTEGER,
        history_count INTEGER, first_record TEXT, last_record TEXT,
        pref_cycle INTEGER, pref_period INTEGER)""")
    cur.execute("""CREATE TABLE history(
        uid INTEGER, seq INTEGER, period_start_ms INTEGER, period_start TEXT,
        submitted_at TEXT, source TEXT, predicted_start TEXT,
        error_days REAL, method TEXT,
        PRIMARY KEY(uid, seq))""")
    cur.execute("""CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT)""")
    for r in rows:
        cur.execute("""INSERT OR REPLACE INTO characters VALUES(
            ?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""", (
            r["uid"], r["昵称"], r["B站直播间"], r["直播状态"] if isinstance(r["直播状态"], int) else None,
            r["粉丝数"], r["公会"], r["MCN"], r["头像"], r["状态原始值"],
            r["周期内第几天"], r["排卵日"], r["下次经期"], r["有效周期长度"],
            r["有效经期长度"], r["数据点"], r["观测后已过周期"],
            r["平均绝对误差(天)"], r["平均误差(天)"], r["预测样本数"],
            r["历史记录数"], r["首次记录日期"], r["最近记录日期"],
            r["偏好周期长度"] or None, r["偏好经期长度"] or None))
        d = details.get(r["uid"]) or {}
        for i, e in enumerate(d.get("history") or []):
            p = e.get("prediction") or {}
            cur.execute("INSERT OR REPLACE INTO history VALUES(?,?,?,?,?,?,?,?,?)", (
                r["uid"], i, e.get("periodStart"), ts(e.get("periodStart")),
                ts(e.get("submittedAt")), e.get("source"),
                p.get("predictedStart"), p.get("errorDays"), p.get("method")))
    cur.execute("INSERT OR REPLACE INTO meta VALUES(?,?)",
                ("stats", json.dumps(stats, ensure_ascii=False) if stats else ""))
    cur.execute("INSERT OR REPLACE INTO meta VALUES(?,?)",
                ("currency", json.dumps(fx, ensure_ascii=False) if fx else ""))
    con.commit()
    con.close()

    # XLSX (openpyxl 可用时)
    try:
        from openpyxl import Workbook
        from openpyxl.styles import Font, Alignment
        from openpyxl.utils import get_column_letter

        wb = Workbook()
        ws = wb.active
        ws.title = "角色数据"
        ws.append(cols)
        for c in ws[1]:
            c.font = Font(bold=True)
            c.alignment = Alignment(horizontal="center")
        for r in rows:
            ws.append([r[c] for c in cols])
        ws.freeze_panes = "B2"
        ws.auto_filter.ref = ws.dimensions
        for i, c in enumerate(cols, 1):
            width = max(len(str(c)) * 2, *(len(str(r[c])[:30]) for r in rows[:200])) + 2
            ws.column_dimensions[get_column_letter(i)].width = min(max(width, 8), 42)

        ws2 = wb.create_sheet("投稿统计")
        if stats:
            ws2.append(["指标", "数值"])
            for c in ws2[1]:
                c.font = Font(bold=True)
            pairs = [(k, v) for k, v in stats.items() if k != "predictionAccuracy"]
            acc = stats.get("predictionAccuracy") or {}
            ov = acc.get("overall") or {}
            pairs += [("overall.meanAbsErrorDays", ov.get("meanAbsErrorDays")),
                      ("overall.meanErrorDays", ov.get("meanErrorDays")),
                      ("overall.samples", ov.get("samples")),
                      ("users", acc.get("users"))]
            for k, v in pairs:
                ws2.append([k, v])
            ws2.column_dimensions["A"].width = 30
            ws2.column_dimensions["B"].width = 20

            ws3 = wb.create_sheet("按月准确率")
            ws3.append(["月份", "平均绝对误差(天)", "平均误差(天)", "样本数"])
            for c in ws3[1]:
                c.font = Font(bold=True)
            for m in acc.get("months") or []:
                ws3.append([m.get("month"), m.get("meanAbsErrorDays"),
                            m.get("meanErrorDays"), m.get("samples")])
            for col, w2 in zip("ABCD", (12, 20, 16, 10)):
                ws3.column_dimensions[col].width = w2
        wb.save(out / "laplace_fertilities.xlsx")
        xlsx = "laplace_fertilities.xlsx"
    except Exception as e:
        xlsx = f"(跳过: {e})"

    print(f"      CSV   : laplace_fertilities.csv  ({len(rows)} 行)")
    print(f"      JSONL : laplace_fertilities.jsonl")
    print(f"      JSON  : laplace_full.json  (含 history)")
    print(f"      SQLite: laplace.db  (characters / history / meta)")
    print(f"      XLSX  : {xlsx}")


def main():
    ap = argparse.ArgumentParser(description="laplace.live /ovu 数据爬虫")
    ap.add_argument("--out", default="out", help="输出目录")
    ap.add_argument("--concurrency", type=int, default=5, help="详情并发数")
    ap.add_argument("--list-only", action="store_true", help="只抓列表+统计")
    ap.add_argument("--headful", action="store_true", help="显示浏览器窗口")
    a = ap.parse_args()
    out = Path(a.out).resolve()
    sys.exit(asyncio.run(crawl(out, a.concurrency, a.list_only, not a.headful)))


if __name__ == "__main__":
    main()
