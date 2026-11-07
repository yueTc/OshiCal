#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
离线导出: 从 raw JSON 生成 CSV / JSONL / SQLite / XLSX (不需要浏览器)。
用法: python export_offline.py <数据目录>
"""
import csv
import json
import sqlite3
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

CST = timezone(timedelta(hours=8))
STATUS_CN = {"normal": "常规", "menstruating": "经期", "fertile": "易孕期", "ovulating": "排卵期"}
LIVE_CN = {0: "未开播", 1: "直播中", 2: "未开播", 3: "轮播"}


def ts(ms):
    if not ms:
        return ""
    try:
        return datetime.fromtimestamp(ms / 1000, CST).strftime("%Y-%m-%d %H:%M:%S")
    except Exception:
        return ""


def flatten(item, detail):
    u = item.get("user") or {}
    gi = u.get("guildInfo") or {}
    hist = gi.get("history") or []
    guild = ((gi.get("current") or {}).get("name")
             or (hist[-1].get("name") if hist else "")
             or "")
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


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "out").resolve()
    raw = out / "raw"
    base = json.loads((raw / "fertilities_all.json").read_text(encoding="utf-8"))
    roster = base["data"]
    stats_p = raw / "contribution_stats.json"
    fx_p = raw / "currency.json"
    stats = json.loads(stats_p.read_text(encoding="utf-8")) if stats_p.exists() else None
    fx = json.loads(fx_p.read_text(encoding="utf-8")) if fx_p.exists() else None

    details = {}
    ddir = raw / "details"
    if ddir.exists():
        for fp in ddir.glob("*.json"):
            try:
                details[int(fp.stem)] = json.loads(fp.read_text(encoding="utf-8"))
            except Exception:
                pass

    rows = [flatten(it, details.get(it["uid"])) for it in roster]
    cols = list(rows[0].keys())

    with open(out / "laplace_fertilities.csv", "w", newline="", encoding="utf-8-sig") as f:
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        w.writerows(rows)

    with open(out / "laplace_fertilities.jsonl", "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")

    with open(out / "laplace_history.jsonl", "w", encoding="utf-8") as f:
        for it in roster:
            for i, e in enumerate((details.get(it["uid"]) or {}).get("history") or []):
                f.write(json.dumps({
                    "uid": it["uid"], "username": (it.get("user") or {}).get("username"),
                    "seq": i, "periodStart": e.get("periodStart"),
                    "periodStartDate": ts(e.get("periodStart")),
                    "submittedAt": ts(e.get("submittedAt")),
                    "source": e.get("source"),
                    "prediction": e.get("prediction"),
                }, ensure_ascii=False) + "\n")

    # ---- XLSX ----
    from openpyxl import Workbook
    from openpyxl.styles import Font, Alignment, PatternFill
    from openpyxl.utils import get_column_letter

    wb = Workbook()
    ws = wb.active
    ws.title = "角色数据"
    ws.append(cols)
    head_fill = PatternFill("solid", fgColor="1F4E79")
    for c in ws[1]:
        c.font = Font(bold=True, color="FFFFFF")
        c.fill = head_fill
        c.alignment = Alignment(horizontal="center", vertical="center")
    for r in rows:
        ws.append([r[c] for c in cols])
    ws.freeze_panes = "C2"
    ws.auto_filter.ref = ws.dimensions
    for i, c in enumerate(cols, 1):
        sample = [len(str(r[c])) for r in rows[:300]]
        ws.column_dimensions[get_column_letter(i)].width = min(max([len(str(c)) * 2] + sample) + 2, 40)
    ws.column_dimensions["H"].width = 12
    ws.column_dimensions[get_column_letter(cols.index("历史JSON") + 1)].hidden = True

    # 状态分布
    ws4 = wb.create_sheet("状态分布")
    ws4.append(["生理状态", "人数", "占比"])
    from collections import Counter
    cnt = Counter(r["生理状态"] for r in rows)
    for k, v in cnt.most_common():
        ws4.append([k, v, f"{v/len(rows)*100:.1f}%"])
    for c in ws4[1]:
        c.font = Font(bold=True)

    # 公会榜
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
    for c in ws5[1]:
        c.font = Font(bold=True)

    # 投稿统计
    if stats:
        ws2 = wb.create_sheet("投稿统计")
        ws2.append(["指标", "数值"])
        for c in ws2[1]:
            c.font = Font(bold=True)
        acc = stats.get("predictionAccuracy") or {}
        ov = acc.get("overall") or {}
        for k, v in [(k, v) for k, v in stats.items() if k != "predictionAccuracy"] + [
            ("预测平均绝对误差(天)", ov.get("meanAbsErrorDays")),
            ("预测平均误差(天)", ov.get("meanErrorDays")),
            ("预测样本数", ov.get("samples")),
            ("有预测数据的角色数", acc.get("users")),
            ("汇率USD->CNY", ((fx or {}).get("rates") or {}).get("CNY")),
            ("抓取时间", datetime.now(CST).strftime("%Y-%m-%d %H:%M:%S")),
        ]:
            ws2.append([k, v])
        ws2.column_dimensions["A"].width = 32
        ws2.column_dimensions["B"].width = 24

        ws3 = wb.create_sheet("按月准确率")
        ws3.append(["月份", "平均绝对误差(天)", "平均误差(天)", "样本数"])
        for c in ws3[1]:
            c.font = Font(bold=True)
        for m in acc.get("months") or []:
            ws3.append([m.get("month"), m.get("meanAbsErrorDays"), m.get("meanErrorDays"), m.get("samples")])
        for col, w2 in zip("ABCD", (12, 20, 16, 10)):
            ws3.column_dimensions[col].width = w2

    # 历史明细
    ws6 = wb.create_sheet("历史记录")
    ws6.append(["uid", "昵称", "第几条", "经期开始", "提交时间", "来源", "预测值", "误差天数", "预测方法"])
    for c in ws6[1]:
        c.font = Font(bold=True)
    for it in roster:
        name = (it.get("user") or {}).get("username")
        for i, e in enumerate((details.get(it["uid"]) or {}).get("history") or []):
            p = e.get("prediction") or {}
            ws6.append([it["uid"], name, i + 1, ts(e.get("periodStart")), ts(e.get("submittedAt")),
                        e.get("source"), p.get("predictedStart"), p.get("errorDays"), p.get("method")])
    ws6.column_dimensions["B"].width = 22
    ws6.column_dimensions["E"].width = 20
    ws6.column_dimensions["F"].width = 52

    wb.save(out / "laplace_fertilities.xlsx")

    # ---- SQLite ----
    db = out / "laplace.db"
    if db.exists():
        db.unlink()
    con = sqlite3.connect(db)
    c = con.cursor()
    c.execute("""CREATE TABLE characters(uid INTEGER PRIMARY KEY, username TEXT, room INTEGER,
        live_status TEXT, fans INTEGER, guild TEXT, mcn TEXT, avatar TEXT, status TEXT,
        day_in_cycle INTEGER, ovulation_day INTEGER, next_period TEXT, cycle_len INTEGER,
        period_len INTEGER, data_points INTEGER, cycles_elapsed INTEGER, mae REAL,
        mean_error REAL, samples INTEGER, history_count INTEGER, first_record TEXT,
        last_record TEXT, pref_cycle INTEGER, pref_period INTEGER)""")
    for r in rows:
        c.execute("INSERT OR REPLACE INTO characters VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (
            r["uid"], r["昵称"], r["B站直播间"], r["直播状态"] if isinstance(r["直播状态"], int) else str(r["直播状态"]),
            r["粉丝数"], r["公会"], r["MCN"], r["头像"], r["状态原始值"], r["周期内第几天"], r["排卵日"],
            r["下次经期"], r["有效周期长度"], r["有效经期长度"], r["数据点"], r["观测后已过周期"],
            r["平均绝对误差(天)"], r["平均误差(天)"], r["预测样本数"], r["历史记录数"],
            r["首次记录日期"], r["最近记录日期"], r["偏好周期长度"] or None, r["偏好经期长度"] or None))
    c.execute("""CREATE TABLE history(uid INTEGER, seq INTEGER, period_start_date TEXT,
        submitted_at TEXT, source TEXT, predicted_start TEXT, error_days REAL, method TEXT,
        PRIMARY KEY(uid,seq))""")
    for it in roster:
        for i, e in enumerate((details.get(it["uid"]) or {}).get("history") or []):
            p = e.get("prediction") or {}
            c.execute("INSERT OR REPLACE INTO history VALUES(?,?,?,?,?,?,?,?)", (
                it["uid"], i, ts(e.get("periodStart")), ts(e.get("submittedAt")),
                e.get("source"), p.get("predictedStart"), p.get("errorDays"), p.get("method")))
    c.execute("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT)")
    c.execute("INSERT INTO meta VALUES('stats',?)", (json.dumps(stats, ensure_ascii=False),))
    c.execute("INSERT INTO meta VALUES('currency',?)", (json.dumps(fx, ensure_ascii=False),))
    con.commit()
    con.close()

    hrows = sum(1 for _ in open(out / "laplace_history.jsonl", encoding="utf-8"))
    print(f"角色: {len(rows)} | 历史记录: {hrows} | 状态分布: {dict(cnt)}")
    print("输出:", ", ".join(p.name for p in sorted(out.glob("laplace_fertilities.*"))))
    print("       laplace.db, laplace_full.json, laplace_history.jsonl")

    # ---- 汇总报告 ----
    fans_sorted = sorted(rows, key=lambda r: -(r["粉丝数"] or 0))
    live_cnt = Counter(r["直播状态"] for r in rows)
    acc_users = [r for r in rows if r["预测样本数"]]
    best = sorted(acc_users, key=lambda r: (r["平均绝对误差(天)"] if r["平均绝对误差(天)"] is not None else 99))[:10]
    most_rec = sorted(rows, key=lambda r: -r["历史记录数"])[:10]
    lines = [
        "# LAPLACE /ovu 数据抓取报告",
        "",
        f"- 抓取时间：{datetime.now(CST).strftime('%Y-%m-%d %H:%M:%S')} (UTC+8)",
        f"- 角色总数：**{len(rows)}**（当前在册 {json.loads((raw/'fertilities_current.json').read_text(encoding='utf-8'))['total']}，含已过期 {len(rows)}）",
        f"- 历史记录条数：**{hrows}**",
        f"- 投稿总量：**{(stats or {}).get('total')}**（通过 {(stats or {}).get('approved')} / 待审 {(stats or {}).get('pending')} / 驳回 {(stats or {}).get('rejected')}）",
        f"- 平均审核时长：{(stats or {}).get('avgProcessingTimeHours')} 小时",
        "",
        "## 生理状态分布",
        "",
        "| 状态 | 人数 | 占比 |",
        "| --- | --- | --- |",
    ]
    for k, v in cnt.most_common():
        lines.append(f"| {k} | {v} | {v/len(rows)*100:.1f}% |")
    lines += ["", "## 直播状态", "", "| 状态 | 人数 |", "| --- | --- |"]
    for k, v in live_cnt.most_common():
        lines.append(f"| {k} | {v} |")
    lines += ["", "## 粉丝数 Top 15", "", "| 昵称 | uid | 粉丝 | 公会 | 状态 |", "| --- | --- | --- | --- | --- |"]
    for r in fans_sorted[:15]:
        lines.append(f"| {r['昵称']} | {r['uid']} | {r['粉丝数']} | {r['公会'] or '-'} | {r['生理状态']} |")
    lines += ["", "## 记录最多 Top 10", "", "| 昵称 | 历史记录 | 数据点 | 预测样本 | 平均绝对误差(天) |", "| --- | --- | --- | --- | --- |"]
    for r in most_rec:
        lines.append(f"| {r['昵称']} | {r['历史记录数']} | {r['数据点']} | {r['预测样本数']} | {r['平均绝对误差(天)']} |")
    lines += ["", "## 预测最准 Top 10（按平均绝对误差，仅统计有样本者）", "",
              "| 昵称 | 平均绝对误差(天) | 平均误差(天) | 样本数 |", "| --- | --- | --- | --- |"]
    for r in best:
        lines.append(f"| {r['昵称']} | {r['平均绝对误差(天)']} | {r['平均误差(天)']} | {r['预测样本数']} |")
    lines += ["", "## 数据字段说明", "",
              "- `生理状态`：normal 常规 / menstruating 经期 / fertile 易孕期 / ovulating 排卵期",
              "- `周期内第几天` / `排卵日`：当前周期内位置（天）",
              "- `有效周期长度` / `有效经期长度`：模型拟合后的周期参数",
              "- `数据点`：该角色被采纳的经期记录数",
              "- `平均绝对误差(天)`：历史预测与实际开始的偏差，越小越准",
              "- `history`（仅 JSON/SQLite/历史记录表）：每条记录的经期开始时间、提交时间、B 站动态来源链接、预测值与误差",
              ""]
    (out / "README_报告.md").write_text("\n".join(lines), encoding="utf-8")
    print("       报告: README_报告.md")


if __name__ == "__main__":
    main()
