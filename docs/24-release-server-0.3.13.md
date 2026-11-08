# OshiCal Server 0.3.13

本次仅发布服务端，标签为 `server-v0.3.13`。桌面端保持 `0.3.6`。

## 变更

### 主播断流重开不再拆成两场

B 站把主播断流后的重新推流当成一次新的开播（`live_time` 变了），旧逻辑会因此新建一条场次，
于是同一场直播在库里被切成两段：前一段只剩下断流前的几分钟，真正的正片在另一条记录里，
而排班卡片按「开始时间最近」只会关联到前一段。

现在 `ensure_live_session` 会判定这是不是同一场：

- 同 `(streamer_id, started_at)` 命中且已结束：原地重开，保留开始时间、计数器与峰值。
- 未命中，但同主播、同房间的上一段 `ended_at` 距新开播 ≤ 10 分钟（`LIVE_SESSION_MERGE_MINUTES`）：
  并回上一行，`segment_count + 1`，`last_segment_started_at` 记录并进来的是哪一段。
- 同一个 `live_time` 被轮询反复调用：幂等，不会重复合并。
- 超过窗口：正常新建一场。

### 短断流不再清零计数器

旧逻辑在同键命中且场次已结束时会把 `danmaku_count` / `revenue_cents` / `duration_seconds` /
`peak_*` 全部归零重来，导致断流前的数据直接消失。现在改为保留计数器继续累加。

### 修掉「每场都被标成部分缺失」

`CollectorStats::from_session` 原来用 `collector_state == "active"` 猜「是不是恢复的旧场次」，
但全新场次建行时状态本来就是 `active`，于是每个场次从第一个检查点起就是 `partial`，
`complete` 永远不可达（0.3.11 之后所有场次都受影响）。现在由 `start()` 用
`updated_at > created_at` 判定是否续开，只有真正续开的场次才降级。

### 前端

直播数据面板的徽标会写清降级原因：`部分缺失 · 断流 1 次` / `部分缺失 · 重连 3 次`，
悬停说明里给出「断流重开 N 次（已合并为同一场，共 N+1 段），断线窗口内的弹幕和礼物没有计入」。

## 数据库

`live_sessions` 新增两列，由启动迁移自动补齐，不需要手工操作：

- `segment_count INTEGER NOT NULL DEFAULT 1`
- `last_segment_started_at TEXT NOT NULL DEFAULT ''`

`LiveSession.segmentCount` 带 `serde(default = 1)`，旧客户端收到不带该字段的 payload 也能解析。

已落库的拆分场次不会自动合并：0.3.13 只对之后发生的断流生效。

## 已知限制

- 合并后的场次时长按「开始时间 → 结束时间」计算，包含断流的那段空档（通常几十秒到几分钟）。
- 合并依赖 10 分钟窗口；间隔更长的两次开播仍会算作两场。
- `peak_online` / `avg_online` 仍然没有任何写入路径，「最高同时在线」显示的实际是
  `ONLINE_RANK_COUNT` 的高能用户数，这两点本次未处理。

## 验证

- 本机只做源码与格式检查：`rustfmt --check`（5 个 Rust 文件）、`npx tsc --noEmit`，均通过。
- 按 `AGENTS.md`，Clippy（`-D warnings`）、测试与服务器镜像构建全部由 GitHub Actions 完成。
- 新增测试：`reopening_finished_live_session_keeps_counters`、
  `stream_restart_with_new_start_time_merges_into_previous_session`、
  `stream_restart_after_merge_window_stays_a_new_session`、
  `only_a_resumed_session_starts_out_degraded`。
