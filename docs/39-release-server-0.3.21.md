# 服务端 0.3.21 发布说明

0.3.20 上线后暴露一个吞吐问题：空结果不入缓存 → 每轮刷新（300 秒）都要把上百条动态重新问一遍模型，一次全量刷新要十几分钟，长过轮询间隔。0.3.21 只改缓存与清理策略。

## 改动

- **空结果也进缓存，12 小时内不重问**：`EMPTY_ANALYSIS_RETRY_HOURS = 12`。缓存行有 `created_at`，`empty_analysis_is_fresh()` 判断窗口；过期才重新分析。既不会把「模型这次没看出来」固化成永久结论，也不会每轮全量重打模型。
- **只在真的识别出新事件时才清旧来源**：`if analysis_succeeded && !written_identities.is_empty()`。模型某一轮返回空不再触发 `delete_schedule_sources_except`，避免偶发漏看把还能用的场次和来源一起删掉。
- `SqliteStore::load_post_analysis_entry()` 返回 `(result_json, created_at)`，`load_post_analysis()` 改为基于它，只返回结果。
- 用例：`empty_result_is_cached_and_retried_after_the_window`（写入 → 命中 → 手工把 `created_at` 改过期 → 重新分析）；`empty_cached_post_analysis_is_retried` 改为「过期的空缓存才重试」。

## 测试

`cargo test --workspace --locked`：core 203 passed / 6 ignored，server 23、desktop 14、集成 37 全绿；`cargo clippy --workspace --all-targets` 无告警；`cargo fmt --all -- --check` 通过。

## 升级

```bash
docker pull ghcr.io/yuetc/oshical-server:0.3.21
# 停旧容器、改名留回滚位，再用原参数起新容器；或
docker compose pull && docker compose up -d
```

升级前建议 `sqlite3 data/oshical.db ".backup 'oshical.db.bak-pre-0321'"`。

## 观察点

- 日志里 `pruned stale schedule sources` 只应在真的落出新事件时出现。
- `post_analyses` 行数应当随刷新推进稳定增长后收敛，而不是每轮暴涨。
- 同一账号串行、多账号并行的行为不变（`llm_parallelism`，默认 1）。
