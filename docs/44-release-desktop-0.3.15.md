# 桌面端 0.3.15 发布说明

同源改动（`crates/core`）：唯一键冲突回退与来源清理并集，桌面端刷新同样受益。

- `crates/core/src/store.rs`：`upsert_schedule` 在 identity 未命中时回退自然键，避免 `UNIQUE constraint failed` 打断整批刷新。
- `crates/core/src/service.rs`：`RefreshEventState.written_identities` 汇总整轮来源，清理旧来源不再抖动。
- 版本号 0.3.15。
