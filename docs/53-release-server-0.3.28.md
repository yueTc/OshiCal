# 服务端 0.3.28：来源清理按事件序号判断，不再连带删掉真日程

0.3.23 给清理加了「按整轮并集」的保护，但判断标准仍然是「这一轮有没有写进去」：一条动态的 `content_digest` 只要变化（正文改了、图片增减），上一轮写入的 `source_identity` 全部换了字符串，就会被当成历史脏数据删掉。来源被删空的日程接着被 `delete_orphan_schedules` 整条删除，表现为「同一场直播时有时无、越刷越少」。

线上实例：枝江娱乐官方周表 `1268554670775730181` 识别出 11 条事件，`2026-11-10T04:14:47Z` 那一轮刷出

```
pruned stale schedule sources post_id=1268554670775730181 removed=3 orphan_schedules=3
removed orphan schedules left by stale sources deleted_schedules=[173, 175, 202]
```

思诺原本该有的 `11-10 19:30 单人日常`、`11-13 17:00` 就这样消失，只剩「思诺 + 心宜」两个人的 `11-15 20:00 有点宜思的综艺`——多人场次走合并分支，来源被重新挂上，所以没被清空。

## 改了什么

判断标准从「这一轮写没写」换成「事件序号还在不在」：

- `crates/core/src/service.rs` 抽出 `fn event_source_identity(post_id, has_images, text_hash, content_digest, event_index) -> String`，编号格式不变（`image:{post}:{digest}:{idx}` / `text:{post}:{hash}:{idx}`），历史日程仍能原地更新。
- 事件循环里先算出编号再判重与匹配主播；**只有真正写进库的序号**才登记为「已被新编号覆盖」，同时进保留集合。
- `crates/core/src/store.rs` 新增 `prune_schedule_sources_beyond(post_id, event_count, keep, replaced)`，替换原先的 `delete_schedule_sources_except` 调用：

| 来源编号尾号 | 处理 |
| --- | --- |
| `>= event_count` | 这一版已经没有第 N 条事件，清掉 |
| 在范围内，且 `replaced` 里该序号已被本轮新编号覆盖 | 同一条事件换了摘要，旧编号清掉（新编号已挂在这条日程上） |
| 在范围内，但这一轮没写进去 | **保留**，这正是本次修的病 |
| 解析不出尾号 / `event_count == 0` / 分析失败 | 一律不动 |

- `keep`（本轮真正写入的来源，含预约卡兜底与确定性改期）无论尾号如何都保留。

跨主播事件（周表里的嘉然 / 乃琳 / 贝拉）不在本次范围：官号来源只挂思诺与心宜，其余场次本来就匹配不到订阅者，属设计取舍。

## 升级

```bash
docker compose pull && docker compose up -d   # 或按 docs/32 的 docker run 方式替换镜像
```

升级后 `/api/health` 的 `version` 应为 `0.3.28`。历史上被误删的日程不会自动回来，需要让对应动态重新分析：

```bash
sqlite3 data/oshical.db "DELETE FROM post_analyses WHERE post_id='1268554670775730181';"
curl -s -X POST -H "Authorization: Bearer $OSHICAL_API_TOKEN" http://127.0.0.1:8787/api/jobs/refresh/15
curl -s -X POST -H "Authorization: Bearer $OSHICAL_API_TOKEN" http://127.0.0.1:8787/api/jobs/refresh/5
```

## 测试

`cargo test --workspace --locked` → core 208 / server 23 + 集成 37 + 14 / 桌面 11 全绿。新增用例：

- `service::pruning_only_drops_events_beyond_the_current_analysis`（尾号 7 的超额来源被清，序号 0、1 原样保留，日程不连带删除）
- `service::digest_change_keeps_the_schedule_and_replaces_the_source`（换图导致摘要变化：日程原地存活，来源换成新编号）
- `service::pruning_skips_everything_when_analysis_failed`（模型报错时来源一条不动）

既有用例 `stale_identities_are_pruned` 语义不变：旧来源的序号被新编号覆盖时仍然被清理，只是不再误伤「序号还在、这一轮没写到」的来源。
