#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
laplace.live 长期驻留采集服务
==============================
把单次脚本变成可以 7x24 挂在服务器上的 daemon。

相比一次性脚本，这里解决的长期运行问题：

1. **增量**：列表接口已经给了 dataPoints / nextPeriod，只有它们变化才去拉详情。
   稳态下每轮几乎不发请求（1638 个角色里每天只有十几个在变）。
2. **变更留痕**：新增的经期记录写入 changes.jsonl 并 first_seen_at 落库，
   这样"某个角色今天新增了一条记录"这个事实本身也被留存。
3. **自愈**：指数退避 + 熔断（连续失败 N 次进入冷却），冷却后自动半开重试。
4. **可观测**：metrics.prom（node_exporter textfile 格式）、status.json、
   HTTP /healthz 健康检查、status.json 里带 last_success 供外部探活。
5. **不重复不并发**：PID 锁文件（带僵尸检测），上一轮没跑完不会叠第二轮。
6. **数据不损坏**：SQLite WAL + 原子写（tmp→rename）+ 每日快照导出。
7. **优雅退出**：SIGTERM/SIGINT 等在途任务收尾、刷盘、释放锁。

用法:
  python laplace_daemon.py --out /data                       # 常驻
  python laplace_daemon.py --out /data --once                # 跑一轮就退（cron 用）
  python laplace_daemon.py --out /data --fast-interval 900 --full-interval 21600
  python laplace_daemon.py --out /data --health-port 8787    # 开 /healthz

环境变量:
  HTTPS_PROXY / HTTP_PROXY   requests 自动读取，机房 IP 被挑战时挂代理即可
  LAPLACE_ALERT_WEBHOOK      告警 webhook（POST JSON），不配则只写日志
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import shutil
import signal
import sqlite3
import sys
import threading
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from logging.handlers import RotatingFileHandler
from pathlib import Path

from laplace_scraper_http import Client, flatten, ts, STATUS_CN, LIVE_CN  # noqa: F401

CST = timezone(timedelta(hours=8))
VERSION = "1.0.0"
log = logging.getLogger("laplace")


def now_s() -> str:
    return datetime.now(CST).strftime("%Y-%m-%d %H:%M:%S")


def iso() -> str:
    return datetime.now(CST).isoformat(timespec="seconds")


# --------------------------------------------------------------------------- #
# 日志
# --------------------------------------------------------------------------- #
def setup_logging(out: Path, verbose: bool = False):
    out.mkdir(parents=True, exist_ok=True)
    fmt = logging.Formatter("%(asctime)s %(levelname)-7s %(message)s", "%Y-%m-%d %H:%M:%S")
    fh = RotatingFileHandler(out / "crawl.log", maxBytes=5 * 1024 * 1024,
                            backupCount=5, encoding="utf-8")
    fh.setFormatter(fmt)
    sh = logging.StreamHandler(sys.stdout)
    sh.setFormatter(fmt)
    log.setLevel(logging.DEBUG if verbose else logging.INFO)
    log.handlers = [fh, sh]
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except Exception:
        pass


# --------------------------------------------------------------------------- #
# 原子写 / 告警
# --------------------------------------------------------------------------- #
def atomic_write(path: Path, text: str):
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


def send_alert(level: str, title: str, detail: str):
    url = os.environ.get("LAPLACE_ALERT_WEBHOOK", "").strip()
    log.warning(f"[告警/{level}] {title} | {detail}")
    if not url:
        return
    payload = json.dumps({"level": level, "title": title, "detail": detail,
                          "service": "laplace-crawler", "time": iso()},
                         ensure_ascii=False).encode()
    req = urllib.request.Request(url, data=payload, method="POST",
                                 headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req, timeout=10).read()
    except Exception as e:
        log.error(f"告警发送失败: {str(e)[:120]}")


# --------------------------------------------------------------------------- #
# PID 锁（带僵尸检测）
# --------------------------------------------------------------------------- #
class PidLock:
    def __init__(self, path: Path):
        self.path = path
        self.held = False

    def acquire(self) -> bool:
        if self.path.exists():
            try:
                pid = int(self.path.read_text().strip() or 0)
            except Exception:
                pid = 0
            if pid and pid != os.getpid() and self._alive(pid):
                log.error(f"已有实例在运行 (pid={pid})，退出")
                return False
            log.warning(f"清理僵尸锁 (pid={pid})")
            self.path.unlink(missing_ok=True)
        self.path.write_text(str(os.getpid()))
        self.held = True
        return True

    @staticmethod
    def _alive(pid: int) -> bool:
        """跨平台判定 PID 是否真的在运行。

        注意: os.kill(pid, 0) 在 Windows 上对已退出进程也返回成功，
        在 Linux 上对僵尸进程也返回成功，两种情况都会把僵尸锁误判成存活，
        导致崩溃重启后永远无法再启动。所以按平台单独处理。
        """
        if pid <= 0:
            return False
        if sys.platform.startswith("win"):
            import ctypes
            kernel32 = ctypes.windll.kernel32
            SYNCHRONIZE = 0x00100000
            h = kernel32.OpenProcess(SYNCHRONIZE, False, pid)
            if not h:
                return False
            try:
                # 句柄有信号 = 进程已退出
                return kernel32.WaitForSingleObject(h, 0) == 0x102  # WAIT_TIMEOUT
            finally:
                kernel32.CloseHandle(h)
        # Linux / macOS: 走 /proc 更准确
        try:
            proc = f"/proc/{pid}"
            if os.path.isdir("/proc"):
                if not os.path.exists(proc):
                    return False
                with open(f"{proc}/stat", "r", encoding="utf-8", errors="ignore") as f:
                    fields = f.read().split()
                # 第 3 个字段是状态, Z = zombie
                if len(fields) > 2 and fields[2] == "Z":
                    return False
                return True
        except Exception:
            pass
        try:
            os.kill(pid, 0)
            return True
        except OSError:
            return False
        except Exception:
            return False

    def release(self):
        if self.held:
            self.path.unlink(missing_ok=True)
            self.held = False


# --------------------------------------------------------------------------- #
# 存储
# --------------------------------------------------------------------------- #
SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS characters(
  uid INTEGER PRIMARY KEY, username TEXT, room INTEGER, live_status TEXT,
  fans INTEGER, guild TEXT, guild_trail TEXT, mcn TEXT, avatar TEXT,
  status TEXT, day_in_cycle INTEGER, ovulation_day INTEGER, next_period TEXT,
  cycle_len INTEGER, period_len INTEGER, data_points INTEGER,
  cycles_elapsed INTEGER, mae REAL, mean_error REAL, samples INTEGER,
  history_count INTEGER, first_record TEXT, last_record TEXT,
  pref_cycle INTEGER, pref_period INTEGER, is_current INTEGER,
  updated_at TEXT, created_at TEXT, retired_at TEXT);
CREATE TABLE IF NOT EXISTS history(
  uid INTEGER, period_start_date TEXT, submitted_at TEXT, source TEXT,
  predicted_start TEXT, error_days REAL, method TEXT,
  first_seen_at TEXT, PRIMARY KEY(uid, period_start_date));
CREATE TABLE IF NOT EXISTS status_log(
  uid INTEGER, old_status TEXT, new_status TEXT, changed_at TEXT);
CREATE INDEX IF NOT EXISTS idx_hist_uid ON history(uid);
CREATE INDEX IF NOT EXISTS idx_hist_first ON history(first_seen_at);
CREATE INDEX IF NOT EXISTS idx_status_log ON status_log(changed_at);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
"""

COLS = ["uid", "昵称", "B站直播间", "直播状态", "粉丝数", "公会", "公会变迁", "MCN",
        "头像", "生理状态", "状态原始值", "周期内第几天", "排卵日", "下次经期",
        "有效周期长度", "有效经期长度", "数据点", "观测后已过周期",
        "平均绝对误差(天)", "平均误差(天)", "预测样本数", "历史记录数",
        "偏好周期长度", "偏好经期长度", "首次记录日期", "最近记录日期"]


class Store:
    def __init__(self, path: Path):
        self.con = sqlite3.connect(path, timeout=30, check_same_thread=False)
        self.con.executescript(SCHEMA)
        for col in ("created_at TEXT", "retired_at TEXT"):
            try:
                self.con.execute(f"ALTER TABLE characters ADD COLUMN {col}")
            except sqlite3.OperationalError:
                pass
        self.con.execute(
            "CREATE INDEX IF NOT EXISTS idx_char_retired ON characters(retired_at)")
        self.con.commit()
        self.lock = threading.Lock()

    def upsert_character(self, r: dict, is_current: bool):
        with self.lock:
            old = self.con.execute(
                "SELECT status FROM characters WHERE uid=?", (r["uid"],)).fetchone()
            if old is not None and old[0] != r["状态原始值"]:
                self.con.execute(
                    "INSERT INTO status_log VALUES(?,?,?,?)",
                    (r["uid"], old[0], r["状态原始值"], now_s()))
            self.con.execute(
                """INSERT INTO characters(uid,username,room,live_status,fans,guild,guild_trail,
                   mcn,avatar,status,day_in_cycle,ovulation_day,next_period,cycle_len,period_len,
                   data_points,cycles_elapsed,mae,mean_error,samples,history_count,first_record,
                   last_record,pref_cycle,pref_period,is_current,updated_at,created_at)
                   VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                   ON CONFLICT(uid) DO UPDATE SET
                     username=excluded.username, room=excluded.room,
                     live_status=excluded.live_status, fans=excluded.fans,
                     guild=excluded.guild, guild_trail=excluded.guild_trail,
                     mcn=excluded.mcn, avatar=excluded.avatar, status=excluded.status,
                     day_in_cycle=excluded.day_in_cycle, ovulation_day=excluded.ovulation_day,
                     next_period=excluded.next_period, cycle_len=excluded.cycle_len,
                     period_len=excluded.period_len, data_points=excluded.data_points,
                     cycles_elapsed=excluded.cycles_elapsed, mae=excluded.mae,
                     mean_error=excluded.mean_error, samples=excluded.samples,
                     history_count=excluded.history_count, first_record=excluded.first_record,
                     last_record=excluded.last_record, pref_cycle=excluded.pref_cycle,
                     pref_period=excluded.pref_period, is_current=excluded.is_current,
                     updated_at=excluded.updated_at""",
                (r["uid"], r["昵称"], r["B站直播间"], str(r["直播状态"]), r["粉丝数"],
                 r["公会"], r["公会变迁"], r["MCN"], r["头像"], r["状态原始值"],
                 r["周期内第几天"], r["排卵日"], r["下次经期"], r["有效周期长度"],
                 r["有效经期长度"], r["数据点"], r["观测后已过周期"],
                 r["平均绝对误差(天)"], r["平均误差(天)"], r["预测样本数"],
                 r["历史记录数"], r["首次记录日期"], r["最近记录日期"],
                 r["偏好周期长度"] or None, r["偏好经期长度"] or None,
                 1 if is_current else 0, now_s(), now_s()))

    def insert_history(self, uid: int, entries: list) -> list:
        """返回本次新增的记录（用于 changes 日志）。"""
        new = []
        stamp = now_s()
        with self.lock:
            for e in entries:
                p = e.get("prediction") or {}
                key = ts(e.get("periodStart"))
                if not key:
                    continue
                cur = self.con.execute(
                    """INSERT OR IGNORE INTO history(uid,period_start_date,submitted_at,source,
                       predicted_start,error_days,method,first_seen_at)
                       VALUES(?,?,?,?,?,?,?,?)""",
                    (uid, key, ts(e.get("submittedAt")), e.get("source"),
                     p.get("predictedStart"), p.get("errorDays"), p.get("method"), stamp))
                if cur.rowcount:
                    new.append({"uid": uid, "period_start": key,
                                "submitted_at": ts(e.get("submittedAt")),
                                "source": e.get("source"),
                                "predicted_start": p.get("predictedStart"),
                                "error_days": p.get("errorDays"),
                                "method": p.get("method"), "first_seen": stamp})
        return new

    def mark_missing_outdated(self, current_uids: set):
        stamp = now_s()
        with self.lock:
            rows = self.con.execute("SELECT uid FROM characters WHERE is_current=1").fetchall()
            gone = [r[0] for r in rows if r[0] not in current_uids]
            for uid in gone:
                self.con.execute(
                    "UPDATE characters SET is_current=0, retired_at=? WHERE uid=?",
                    (stamp, uid))
            # 重新上线的角色：清空 retired_at
            if current_uids:
                marks = ",".join("?" * len(current_uids))
                self.con.execute(
                    f"UPDATE characters SET retired_at=NULL WHERE uid IN ({marks})",
                    tuple(current_uids))
        return gone

    def counts(self) -> dict:
        with self.lock:
            c = self.con.execute("SELECT COUNT(*) FROM characters").fetchone()[0]
            h = self.con.execute("SELECT COUNT(*) FROM history").fetchone()[0]
            cur = self.con.execute(
                "SELECT COUNT(*) FROM characters WHERE is_current=1").fetchone()[0]
            last = self.con.execute(
                "SELECT MAX(period_start_date) FROM history").fetchone()[0]
            newest = self.con.execute(
                "SELECT MAX(first_seen_at) FROM history").fetchone()[0]
        return {"characters": c, "history": h, "current": cur,
                "last_period": last, "last_new_record": newest}

    def get_meta(self, k, default=None):
        with self.lock:
            r = self.con.execute("SELECT value FROM meta WHERE key=?", (k,)).fetchone()
        return r[0] if r else default

    def set_meta(self, k, v):
        with self.lock:
            self.con.execute("INSERT OR REPLACE INTO meta VALUES(?,?)", (k, str(v)))
            self.con.commit()

    def commit(self):
        with self.lock:
            self.con.commit()

    def close(self):
        try:
            self.con.commit()
            self.con.close()
        except Exception:
            pass

    # ---------------- 变更日报 ----------------
    def digest(self, since: str) -> dict:
        """收集 since 之后的所有变化。since 为 'YYYY-MM-DD HH:MM:SS' 本地时间。"""
        with self.lock:
            new_hist = self.con.execute(
                """SELECT h.uid, c.username, h.period_start_date, h.first_seen_at,
                          h.source, h.error_days
                   FROM history h LEFT JOIN characters c ON c.uid=h.uid
                   WHERE h.first_seen_at > ? ORDER BY h.first_seen_at DESC""",
                (since,)).fetchall()
            st_chg = self.con.execute(
                """SELECT s.uid, c.username, s.old_status, s.new_status, s.changed_at
                   FROM status_log s LEFT JOIN characters c ON c.uid=s.uid
                   WHERE s.changed_at > ? ORDER BY s.changed_at DESC""",
                (since,)).fetchall()
            added = self.con.execute(
                """SELECT uid, username, fans, guild, status FROM characters
                   WHERE created_at > ? ORDER BY fans DESC""", (since,)).fetchall()
            removed = self.con.execute(
                """SELECT uid, username FROM characters
                   WHERE retired_at IS NOT NULL AND retired_at > ? ORDER BY username""",
                (since,)).fetchall()
            total = self.con.execute("SELECT COUNT(*) FROM characters").fetchone()[0]
            hist_total = self.con.execute("SELECT COUNT(*) FROM history").fetchone()[0]
        return {"new_history": new_hist, "status_changes": st_chg,
                "added": added, "removed": removed,
                "characters": total, "history": hist_total}

    def write_digest(self, out: Path, since: str | None) -> Path | None:
        """写 日报-YYYY-MM-DD.md + changes-YYYY-MM-DD.jsonl。"""
        if not since:
            return None
        d = self.digest(since)
        day = datetime.now(CST).strftime("%Y-%m-%d")
        md = out / f"日报-{day}.md"
        js = out / f"changes-{day}.jsonl"

        def cn(x):
            return STATUS_CN.get(x, x or "")

        L = [f"# LAPLACE 变更日报 {day}", "",
             f"- 统计区间：{since} 之后",
             f"- 生成时间：{now_s()}",
             f"- 库内角色：{d['characters']} 个，历史记录 {d['history']} 条",
             ""]

        L += [f"## 新增经期记录（{len(d['new_history'])} 条）", ""]
        if d["new_history"]:
            L += ["| 昵称 | uid | 经期开始 | 首次发现 | 当时预测误差 | 来源 |",
                  "| --- | --- | --- | --- | --- | --- |"]
            for uid, name, per, seen, src, err in d["new_history"]:
                L.append(f"| {name or '-'} | {uid} | {per} | {seen} | "
                         f"{'' if err is None else err} | {src or '-'} |")
        else:
            L.append("无新增。")
        L.append("")

        L += [f"## 生理状态变化（{len(d['status_changes'])} 个）", ""]
        if d["status_changes"]:
            L += ["| 昵称 | 变化 | 时间 |", "| --- | --- | --- |"]
            for uid, name, o, n, t in d["status_changes"]:
                L.append(f"| {name or uid} | {cn(o)} → {cn(n)} | {t} |")
        else:
            L.append("无变化。")
        L.append("")

        L += [f"## 新收录角色（{len(d['added'])} 个）", ""]
        if d["added"]:
            L += ["| 昵称 | uid | 粉丝 | 公会 | 状态 |", "| --- | --- | --- | --- | --- |"]
            for uid, name, fans, guild, st in d["added"]:
                L.append(f"| {name} | {uid} | {fans} | {guild or '-'} | {cn(st)} |")
        else:
            L.append("无新增。")
        L.append("")

        L += [f"## 下线角色（{len(d['removed'])} 个）", ""]
        L.append("、".join(f"{n}({u})" for u, n in d["removed"]) if d["removed"] else "无。")
        L.append("")

        atomic_write(md, "\n".join(L))
        records = [{"type": "new_history", "uid": u, "username": n, "period_start": p,
                    "first_seen": s, "source": src, "error_days": e}
                   for u, n, p, s, src, e in d["new_history"]]
        records += [{"type": "status_change", "uid": u, "username": n,
                     "from": o, "to": nn, "at": t}
                    for u, n, o, nn, t in d["status_changes"]]
        records += [{"type": "added", "uid": u, "username": n} for u, n, *_ in d["added"]]
        records += [{"type": "removed", "uid": u, "username": n} for u, n in d["removed"]]
        atomic_write(js, "\n".join(json.dumps(r, ensure_ascii=False) for r in records))
        log.info(f"日报: {md.name} | 新增记录 {len(d['new_history'])} | "
                 f"状态变化 {len(d['status_changes'])} | "
                 f"新收录 {len(d['added'])} | 下线 {len(d['removed'])}")
        return md

    # ---------------- 快照导出 ----------------
    def snapshot(self, out: Path):
        import csv
        with self.lock:
            cur = self.con.execute(
                f"SELECT {', '.join(['uid','username','room','live_status','fans','guild',\
'guild_trail','mcn','avatar','status','day_in_cycle','ovulation_day','next_period',\
'cycle_len','period_len','data_points','cycles_elapsed','mae','mean_error','samples',\
'history_count','first_record','last_record','pref_cycle','pref_period','is_current'])} "
                "FROM characters ORDER BY fans DESC")
            rows = cur.fetchall()
        with open(out / "laplace_fertilities.csv", "w", newline="", encoding="utf-8-sig") as f:
            w = csv.writer(f)
            w.writerow(COLS + ["在册"])
            for r in rows:
                w.writerow(list(r[:25]) + ["是" if r[25] else "已过期"])

        try:
            from openpyxl import Workbook
            from openpyxl.styles import Alignment, Font, PatternFill
            from openpyxl.utils import get_column_letter

            wb = Workbook()
            ws = wb.active
            ws.title = "角色数据"
            head = COLS + ["在册"]
            ws.append(head)
            fill = PatternFill("solid", fgColor="1F4E79")
            for cell in ws[1]:
                cell.font = Font(bold=True, color="FFFFFF")
                cell.fill = fill
                cell.alignment = Alignment(horizontal="center")
            for r in rows:
                ws.append(list(r[:25]) + ["是" if r[25] else "已过期"])
            ws.freeze_panes = "C2"
            ws.auto_filter.ref = ws.dimensions
            for i, k in enumerate(head, 1):
                ws.column_dimensions[get_column_letter(i)].width = min(
                    max(len(str(k)) * 2, 10) + 2, 40)

            ws2 = wb.create_sheet("变更记录")
            ws2.append(["uid", "经期开始", "提交时间", "首次发现", "来源", "误差天数"])
            for cell in ws2[1]:
                cell.font = Font(bold=True)
            with self.lock:
                hrows = self.con.execute(
                    "SELECT uid,period_start_date,submitted_at,first_seen_at,source,error_days "
                    "FROM history ORDER BY first_seen_at DESC LIMIT 5000").fetchall()
            for r in hrows:
                ws2.append(list(r))
            for col, w2 in zip("ABCDEF", (12, 14, 20, 20, 50, 10)):
                ws2.column_dimensions[col].width = w2
            wb.save(out / "laplace_fertilities.xlsx")
        except ImportError:
            pass


# --------------------------------------------------------------------------- #
# 健康检查
# --------------------------------------------------------------------------- #
class HealthState:
    def __init__(self):
        self.lock = threading.Lock()
        self.data = {"ok": False, "last_success": None, "last_error": None,
                     "consecutive_failures": 0, "cycle": 0, "version": VERSION}

    def update(self, **kw):
        with self.lock:
            self.data.update(kw)

    def snapshot(self):
        with self.lock:
            return dict(self.data)


def start_health_server(port: int, state: HealthState) -> ThreadingHTTPServer:
    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path.split("?")[0] not in ("/healthz", "/health", "/metrics"):
                self.send_response(404)
                self.end_headers()
                return
            d = state.snapshot()
            if self.path.startswith("/metrics"):
                body = (f'laplace_up {1 if d["ok"] else 0}\n'
                        f'laplace_consecutive_failures {d["consecutive_failures"]}\n')
                self.send_response(200)
                self.send_header("Content-Type", "text/plain; version=0.0.4")
                self.end_headers()
                self.wfile.write(body.encode())
                return
            healthy = d["ok"] and d["consecutive_failures"] == 0
            body = json.dumps(d, ensure_ascii=False).encode()
            self.send_response(200 if healthy else 503)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):
            pass

    srv = ThreadingHTTPServer(("0.0.0.0", port), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    log.info(f"健康检查已启动: http://0.0.0.0:{port}/healthz")
    return srv


# --------------------------------------------------------------------------- #
# 主循环
# --------------------------------------------------------------------------- #
class Daemon:
    def __init__(self, cfg, store: Store, state: HealthState):
        self.cfg = cfg
        self.store = store
        self.state = state
        self.cli = Client(cfg.rate, pool=max(8, cfg.workers))
        self.fp: dict[str, list] = {}
        self._detail_cache: dict[int, dict] = {}
        self.fp_path = cfg.out / "fingerprints.json"
        self.changes_path = cfg.out / "changes.jsonl"
        self.stop = threading.Event()
        self.last_fast = 0.0
        # 关键: 全量轮次时间持久化，否则每次 --once 都当成首轮全量
        saved = self.store.get_meta("last_full_ts")
        try:
            self.last_full = float(saved) if saved else 0.0
        except (TypeError, ValueError):
            self.last_full = 0.0
        if not saved:
            log.info("未找到全量基线，本轮按全量执行")
        self.circuit_open_until = 0.0
        if self.fp_path.exists():
            try:
                self.fp = json.loads(self.fp_path.read_text(encoding="utf-8"))
            except Exception:
                self.fp = {}

    # ------------------ 单轮 ------------------
    def run_cycle(self, full: bool) -> dict:
        t0 = time.time()
        base = self.cfg.base_url
        cur = self.cli.get(f"{base}/laplace/fertilities")
        alld = self.cli.get(f"{base}/laplace/fertilities?showOutdated=1")
        stats = self.cli.get(f"{base}/laplace/fertility-contribution-stats")
        if not (cur or alld):
            raise RuntimeError("列表接口不可用")

        roster = (alld or cur)["data"]
        cur_ids = {x["uid"] for x in (cur or {}).get("data", [])}
        gone = self.store.mark_missing_outdated(cur_ids)

        # 决定要拉哪些详情
        todo = []
        for it in roster:
            uid = it["uid"]
            sig = [it.get("dataPoints"), it.get("nextPeriod"), it.get("dayInCycle")]
            old = self.fp.get(str(uid))
            if full or old is None or old[:2] != sig[:2]:
                todo.append(uid)
            elif old[2] != sig[2]:                 # 周期内天数变了，只用列表更新
                pass
        log.info(f"列表 {len(roster)} (在册 {len(cur_ids)}) | 详情待抓 {len(todo)}"
                 + (f" | 下线 {len(gone)}" if gone else ""))

        details_done = 0
        new_records = []
        details: dict[int, dict] = {}
        if todo:
            def work(uid):
                return uid, self.cli.get(f"{base}/laplace/fertility/{uid}")

            with ThreadPoolExecutor(max_workers=self.cfg.workers) as ex:
                futs = [ex.submit(work, u) for u in todo]
                for f in as_completed(futs):
                    try:
                        uid, d = f.result()
                    except Exception as e:
                        log.warning(f"详情异常: {str(e)[:100]}")
                        continue
                    if d:
                        details_done += 1
                        details[uid] = d
                        new_records += self.store.insert_history(uid, d.get("history") or [])

        # 更新角色表 + 指纹
        for it in roster:
            uid = it["uid"]
            row = flatten(it, details.get(uid) or self._cached_detail(uid))
            self.store.upsert_character(row, uid in cur_ids)
            self.fp[str(uid)] = [it.get("dataPoints"), it.get("nextPeriod"), it.get("dayInCycle")]

        # 详情落盘（供离线重导出与审计）
        if details:
            ddir = self.cfg.out / "raw" / "details"
            ddir.mkdir(parents=True, exist_ok=True)
            for uid, d in details.items():
                atomic_write(ddir / f"{uid}.json", json.dumps(d, ensure_ascii=False))
            self._detail_cache.update(details)

        if new_records:
            with open(self.changes_path, "a", encoding="utf-8") as f:
                for r in new_records:
                    f.write(json.dumps(r, ensure_ascii=False) + "\n")
            log.info(f"新增记录 {len(new_records)} 条 -> changes.jsonl")

        atomic_write(self.fp_path, json.dumps(self.fp, ensure_ascii=False))
        if stats:
            self.store.set_meta("stats", json.dumps(stats, ensure_ascii=False))
        self.store.commit()

        c = self.store.counts()
        dur = time.time() - t0
        result = {"duration": dur, "characters": c["characters"], "history": c["history"],
                  "current": c["current"], "details": details_done,
                  "new_records": len(new_records), "requests": self.cli.requests_made,
                  "last_period": c["last_period"]}
        self.write_metrics(result, ok=True)
        return result

    def _cached_detail(self, uid):
        """从磁盘缓存还原详情（跨轮、跨重启都有效）。"""
        d = self._detail_cache.get(uid)
        if d is not None:
            return d
        p = self.cfg.out / "raw" / "details" / f"{uid}.json"
        if p.exists():
            try:
                d = json.loads(p.read_text(encoding="utf-8"))
                self._detail_cache[uid] = d
                return d
            except Exception:
                return None
        return None

    # ------------------ 指标 ------------------
    def write_metrics(self, r: dict, ok: bool):
        st = self.state.snapshot()
        st.pop("ok", None)          # 防止覆盖显式传入的 ok
        if ok:
            self.store.set_meta("last_success_ts", int(time.time()))
            self.store.commit()
        lines = [
            "# HELP laplace_up 上一轮采集是否成功",
            "# TYPE laplace_up gauge",
            f"laplace_up {1 if ok else 0}",
            "laplace_characters_total " + str(r.get("characters", 0)),
            "laplace_characters_current " + str(r.get("current", 0)),
            "laplace_history_total " + str(r.get("history", 0)),
            "laplace_last_cycle_duration_seconds " + f"{r.get('duration', 0):.2f}",
            "laplace_last_cycle_requests " + str(r.get("requests", 0)),
            "laplace_last_cycle_new_records " + str(r.get("new_records", 0)),
            "laplace_consecutive_failures " + str(st.get("consecutive_failures", 0)),
            "laplace_last_success_timestamp_seconds " + str(self._last_ok_ts()),
        ]
        atomic_write(self.cfg.out / "metrics.prom", "\n".join(lines) + "\n")
        payload = {"ok": ok, "updated": iso()}
        payload.update(r)
        payload.update(st)
        atomic_write(self.cfg.out / "status.json",
                     json.dumps(payload, ensure_ascii=False, indent=1))

    def _last_ok_ts(self) -> int:
        v = self.store.get_meta("last_success_ts")
        try:
            return int(v) if v else 0
        except (TypeError, ValueError):
            return 0

    # ------------------ 调度 ------------------
    def should_run_full(self, t: float) -> bool:
        return (t - self.last_full) >= self.cfg.full_interval

    def loop(self):
        log.info(f"服务启动 v{VERSION} | 目录 {self.cfg.out} | "
                 f"快轮 {self.cfg.fast_interval}s / 全量轮 {self.cfg.full_interval}s")
        cycle = 0
        while not self.stop.is_set():
            cycle += 1
            t = time.time()
            full = self.should_run_full(t)

            if t < self.circuit_open_until:
                wait = int(self.circuit_open_until - t)
                log.warning(f"熔断中，剩余 {wait}s")
                self.stop.wait(min(wait, 60))
                continue

            self.state.update(cycle=cycle)
            log.info(f"--- 第 {cycle} 轮 ({'全量' if full else '增量'}) ---")
            try:
                r = None
                last_exc = None
                for attempt in range(1, max(1, self.cfg.cycle_retries) + 1):
                    try:
                        r = self.run_cycle(full)
                        break
                    except Exception as e:
                        last_exc = e
                        if attempt < self.cfg.cycle_retries:
                            wait = min(120, 5 * 2 ** (attempt - 1))
                            log.warning(f"本轮第 {attempt} 次失败，{wait}s 后重试: "
                                        f"{str(e)[:120]}")
                            if self.stop.wait(wait):
                                raise
                if r is None:
                    raise last_exc or RuntimeError("未知失败")
                if full:
                    self.last_full = t
                    self.store.set_meta("last_full_ts", t)
                    self.store.commit()
                self.last_fast = t
                self.state.update(ok=True, last_success=iso(), last_error=None,
                                  consecutive_failures=0)
                log.info(f"完成 {r['duration']:.0f}s | 角色 {r['characters']} | "
                         f"历史 {r['history']} | 详情 {r['details']} | "
                         f"新增 {r['new_records']} | 请求 {r['requests']}")
                if full:
                    self.store.snapshot(self.cfg.out)
                    log.info("已导出快照 csv/xlsx")
                if self.cfg.digest:
                    prev = self.store.get_meta("last_run_at")
                    if prev:
                        self.store.write_digest(self.cfg.out, prev)
                    else:
                        log.info("首次运行，无历史基线，跳过日报（下次起生效）")
                    self.store.set_meta("last_run_at", now_s())
                    self.store.commit()
            except Exception as e:
                st = self.state.snapshot()
                fails = st.get("consecutive_failures", 0) + 1
                self.state.update(ok=False, last_error=f"{type(e).__name__}: {str(e)[:200]}",
                                  consecutive_failures=fails)
                c = self.store.counts()
                self.write_metrics({"duration": 0, "characters": c["characters"],
                                    "history": c["history"], "current": c["current"],
                                    "details": 0, "new_records": 0,
                                    "requests": self.cli.requests_made}, ok=False)
                log.error(f"本轮失败 ({fails}): {str(e)[:200]}")
                if fails >= self.cfg.circuit_threshold:
                    cool = self.cfg.circuit_cooldown
                    self.circuit_open_until = time.time() + cool
                    send_alert("critical", "laplace 采集连续失败，已熔断",
                               f"连续 {fails} 次失败，冷却 {cool}s；最后错误: {str(e)[:200]}")
                elif fails == 1:
                    send_alert("warning", "laplace 采集失败", str(e)[:200])

            if self.cfg.once or (self.cfg.max_cycles and cycle >= self.cfg.max_cycles):
                break

            nxt = self.cfg.fast_interval if not full else min(
                self.cfg.fast_interval, self.cfg.full_interval)
            log.info(f"下一轮 {nxt}s 后")
            if self.stop.wait(nxt):
                break

        log.info("主循环结束")


# --------------------------------------------------------------------------- #
# 入口
# --------------------------------------------------------------------------- #
def main():
    ap = argparse.ArgumentParser(description="laplace.live 长期驻留采集服务")
    ap.add_argument("--out", default="/data")
    ap.add_argument("--workers", type=int, default=6)
    ap.add_argument("--rate", type=float, default=6.0)
    ap.add_argument("--fast-interval", type=int, default=900, help="增量间隔（秒）")
    ap.add_argument("--full-interval", type=int, default=21600, help="全量间隔（秒）")
    ap.add_argument("--circuit-threshold", type=int, default=5, help="连续失败几次熔断")
    ap.add_argument("--circuit-cooldown", type=int, default=1800, help="熔断冷却（秒）")
    ap.add_argument("--health-port", type=int, default=0, help="健康检查端口，0 关闭")
    ap.add_argument("--base-url", default="https://workers.vrp.moe")
    ap.add_argument("--once", action="store_true", help="只跑一轮（cron 用）")
    ap.add_argument("--max-cycles", type=int, default=0, help="最多跑几轮（测试用）")
    ap.add_argument("--limit", type=int, default=0, help="只处理前 N 个角色（测试用）")
    ap.add_argument("--min-free-mb", type=int, default=200, help="磁盘剩余低于此值跳过")
    ap.add_argument("--digest", action="store_true",
                    help="每轮后生成变更日报（日报-YYYY-MM-DD.md）")
    ap.add_argument("--cycle-retries", type=int, default=3,
                    help="单轮失败重试次数（--once 模式下保证成功率）")
    ap.add_argument("--check-age-hours", type=float, default=0,
                    help="看门狗模式：上次成功超过 N 小时则告警并退出 1")
    ap.add_argument("--verbose", action="store_true")
    a = ap.parse_args()

    out = Path(a.out).resolve()
    a.out = out                     # 统一成 Path，Daemon/Store 内部直接用
    setup_logging(out, a.verbose)

    # 磁盘水位
    try:
        free_mb = shutil.disk_usage(out).free / 1024 / 1024
        if free_mb < a.min_free_mb:
            send_alert("critical", "laplace 磁盘不足", f"剩余 {free_mb:.0f}MB < {a.min_free_mb}MB")
            log.error(f"磁盘剩余 {free_mb:.0f}MB，拒绝启动")
            return 2
    except Exception:
        pass

    # ---- 看门狗模式：只检查上次成功时间，不抓数据 ----
    if a.check_age_hours:
        st_p = out / "status.json"
        if not st_p.exists():
            send_alert("critical", "laplace 从未成功运行过",
                       f"找不到 {st_p}")
            log.error("从未成功运行过")
            return 1
        try:
            d0 = json.loads(st_p.read_text(encoding="utf-8"))
            last = datetime.fromisoformat(d0["updated"])
            if last.tzinfo is None:
                last = last.replace(tzinfo=CST)
            age_h = (datetime.now(CST) - last).total_seconds() / 3600
            ok = bool(d0.get("ok"))
        except Exception as e:
            send_alert("critical", "laplace 状态文件损坏", str(e)[:200])
            log.error(f"状态文件无法解析: {e}")
            return 1
        if age_h > a.check_age_hours:
            msg = (f"上次成功 {last.strftime('%Y-%m-%d %H:%M:%S')}，"
                   f"已 {age_h:.1f} 小时 (阈值 {a.check_age_hours}h)，"
                   f"最近一轮 ok={ok}；最后错误: {d0.get('last_error')}")
            send_alert("critical", "laplace 采集已停更", msg)
            log.error(msg)
            return 1
        log.info(f"健康：上次成功 {last.strftime('%Y-%m-%d %H:%M:%S')} ({age_h:.1f}h 前)")
        return 0

    lock = PidLock(out / "crawler.pid")
    if not lock.acquire():
        return 3

    store = Store(out / "laplace.db")
    state = HealthState()
    srv = None
    if a.health_port:
        srv = start_health_server(a.health_port, state)

    d = Daemon(a, store, state)
    if a.limit:
        orig = d.run_cycle
        def limited(full):
            base = a.base_url
            r = d.cli.get(f"{base}/laplace/fertilities?showOutdated=1")
            if r:
                r["data"] = r["data"][:a.limit]
                for it in r["data"]:
                    x = d.cli.get(f"{base}/laplace/fertility/{it['uid']}")
                    if x:
                        d.store.insert_history(it["uid"], x.get("history") or [])
                        d.store.upsert_character(flatten(it, x), True)
                d.fp = {str(it["uid"]): [it.get("dataPoints"), it.get("nextPeriod"), 0]
                        for it in r["data"]}
                d.store.commit()
                c = d.store.counts()
                return {"duration": 0, "characters": c["characters"], "history": c["history"],
                        "current": c["current"], "details": len(r["data"]), "new_records": 0,
                        "requests": d.cli.requests_made}
            raise RuntimeError("limit 模式：列表失败")
        d.run_cycle = limited

    def on_sig(signum, frame):
        log.info(f"收到信号 {signum}，准备退出")
        d.stop.set()

    for s in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(s, on_sig)
        except Exception:
            pass

    rc = 0
    try:
        d.loop()
        # --once 模式下，本轮没成功就让进程以非 0 退出，cron/systemd 才能感知
        if a.once:
            st = state.snapshot()
            if not st.get("ok") or st.get("consecutive_failures", 0) > 0:
                rc = 1
    except KeyboardInterrupt:
        log.info("中断")
    finally:
        try:
            store.close()
        except Exception:
            pass
        if srv:
            try:
                srv.shutdown()
            except Exception:
                pass
        lock.release()
        log.info("已停止，锁已释放")
    return rc


if __name__ == "__main__":
    sys.exit(main())
