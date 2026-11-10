#!/usr/bin/env python3
"""Summarize OshiCal refresh timing logs without executing or printing payloads."""
import argparse
import collections
import json
import math
import re
import sys
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")
FIELD = re.compile(r'\b(refresh_id|source_mid|post_id|phase|elapsed_us|outcome)=("[^"\n]*"|[^\s,}:]+)')
PHASES = {
    "refresh_round", "refresh_queue_wait", "source_pipeline", "source_refresh", "source_fetch", "post_store", "cache_lookup",
    "cache_store", "model_slot_wait", "post_analysis", "image_download", "image_prepare", "model_request", "schedule_upsert",
}


def parse_event(line):
    """Support default tracing text and JSON tracing; discard non-timing events."""
    line = ANSI.sub("", line)
    fields = {}
    try:
        data = json.loads(line)
    except (ValueError, TypeError):
        if "刷新阶段耗时" not in line:
            return None
        for key, value in FIELD.findall(line):
            fields[key] = value.strip('"')
    else:
        if not isinstance(data, dict):
            return None
        for span in data.get("spans", []):
            if isinstance(span, dict):
                fields.update(span)
        if isinstance(data.get("span"), dict):
            fields.update(data["span"])
        if isinstance(data.get("fields"), dict):
            fields.update(data["fields"])
        else:
            fields.update(data)
    if fields.get("phase") not in PHASES:
        return None
    raw = fields.get("elapsed_us")
    if isinstance(raw, bool) or (not isinstance(raw, (str, int))):
        return None
    try:
        elapsed = int(raw)
    except (ValueError, TypeError):
        return None
    if elapsed < 0 or fields.get("outcome") not in {"ok", "error", "incomplete"}:
        return None
    event = {key: str(fields[key]) for key in ("refresh_id", "source_mid", "post_id", "phase", "outcome") if key in fields}
    event["elapsed_us"] = elapsed
    return event


def summarize(events, refresh_id=None, latest=False):
    if latest and refresh_id is None:
        refresh_id = next((event["refresh_id"] for event in reversed(events) if event.get("refresh_id")), None)
    if refresh_id is not None:
        events = [event for event in events if event.get("refresh_id") == refresh_id]
    grouped = collections.defaultdict(list)
    for event in events:
        grouped[event["phase"]].append(event)
    phases = []
    for phase, rows in sorted(grouped.items()):
        values = sorted(row["elapsed_us"] / 1000 for row in rows)
        phases.append({"phase": phase, "count": len(rows), "total_ms": sum(values),
                       "avg_ms": sum(values) / len(values), "p95_ms": values[math.ceil(len(values) * .95) - 1],
                       "max_ms": values[-1], "errors": sum(row["outcome"] == "error" for row in rows),
                       "incomplete": sum(row["outcome"] == "incomplete" for row in rows)})
    return {
        "refresh_id": refresh_id,
        "event_count": len(events),
        "wall_ms": [event["elapsed_us"] / 1000 for event in events if event["phase"] == "refresh_round" and event["outcome"] != "incomplete"],
        "phases": phases,
        "slow_posts": sorted((event for event in events if event["phase"] == "post_analysis"), key=lambda event: event["elapsed_us"], reverse=True)[:10],
        "note": "阶段累计时间可能重叠且包含并行任务，不能相加作为整轮实际耗时；wall_ms才是整轮记录。",
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("files", nargs="*", help="UTF-8 log files; omitted means stdin")
    parser.add_argument("--latest", action="store_true", help="only the most recently observed refresh id")
    parser.add_argument("--refresh-id")
    parser.add_argument("--json", action="store_true", help="machine-readable summary")
    parser.add_argument("--max-events", type=int, default=100000)
    args = parser.parse_args(argv)
    if args.max_events < 1:
        parser.error("--max-events must be positive")
    events = []
    streams = [Path(name).open(encoding="utf-8", errors="replace") for name in args.files] if args.files else [sys.stdin]
    try:
        for stream in streams:
            for line in stream:
                event = parse_event(line)
                if event is not None:
                    events.append(event)
                    if len(events) > args.max_events:
                        parser.error("timing event limit exceeded; narrow the log window or raise --max-events")
    finally:
        for stream in streams:
            if stream is not sys.stdin:
                stream.close()
    report = summarize(events, args.refresh_id, args.latest)
    if not report["event_count"]:
        print("未找到符合条件的耗时记录；请确认日志来自带耗时采样的新版本。", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 0
    print("refresh_id:", report["refresh_id"] or "all")
    print("整轮请求实际耗时(ms，含排队):", ", ".join(f"{value:.2f}" for value in report["wall_ms"]) or "尚无整轮结束记录")
    print(f"{'phase':22} {'count':>6} {'total_ms':>12} {'avg_ms':>12} {'p95_ms':>12} {'max_ms':>12} {'error':>6} {'partial':>7}")
    for row in report["phases"]:
        print(f"{row['phase']:22} {row['count']:6} {row['total_ms']:12.2f} {row['avg_ms']:12.2f} {row['p95_ms']:12.2f} {row['max_ms']:12.2f} {row['errors']:6} {row['incomplete']:7}")
    print(report["note"])
    for event in report["slow_posts"]:
        print("慢动态:", event.get("source_mid", "-"), event.get("post_id", "-"), f"{event['elapsed_us'] / 1000:.2f} ms", event["outcome"])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
