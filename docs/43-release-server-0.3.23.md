# 服务端 0.3.23 发布说明

两个上线后才暴露的问题：同一场次被不同来源写入时撞唯一键导致整批刷新失败；同一条动态出现在主播本人和官号两个来源里时，清理旧来源的日志每轮都响、来源来回抖动。

## 1. 唯一键冲突打断整批刷新

日志长这样：

```
动态刷新失败 error=数据库错误: error returned from database:
(code: 2067) UNIQUE constraint failed: schedules.streamer_id, schedules.start_at, schedules.title
```

`crates/core/src/store.rs` 的 `upsert_schedule` 在带 `source_identity` 时只按 `(streamer_id, source_identity)` 查重，查不到就直接 INSERT。同一个场次如果先由预约卡或另一条动态以别的 identity 落库，这里就会撞 `UNIQUE(streamer_id, start_at, title)`，异常冒到 `refresh_sources` 外层，**这个主播这一轮的所有动态都不再处理**。

改法：identity 查不到时退回自然键 `(streamer_id, start_at, title)` 查一次，命中就复用该行，再把新来源 attach 上去。同一场次因此可以同时挂 `reserve:` 与 `image:` 两个来源。

## 2. 来源清理按「整轮并集」判断

同一条动态可能同时出现在主播本人和官号两个来源里，两条路径各自写自己的 identity。原来的清理只看单遍结果，于是后一遍把前一遍刚写进去的来源当成历史脏数据删掉，下一轮再反过来删，日志每轮刷 `pruned stale schedule sources removed=1`。

改法：`RefreshEventState` 增加 `written_identities: HashMap<String, HashSet<String>>`，清理时用整轮并集，抖动消失。

## 测试

- 新增 `upserting_the_same_slot_with_a_new_identity_reuses_the_row`：先 `reserve:post-77` 建行，再用 `image:post-77:digest:0` 同槽 upsert，断言没有新建、id 不变、两条来源都在。
- `cargo test --workspace --locked`：core 205 passed / 6 ignored，server 23、集成 37 全绿；clippy 无告警；fmt 通过。

## 升级

```bash
docker pull ghcr.io/yuetc/oshical-server:0.3.23
```

替换容器时沿用原参数，升级前备份 `oshical.db`。
