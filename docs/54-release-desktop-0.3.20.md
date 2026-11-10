# 桌面端 0.3.20：同步服务端 0.3.28 的来源清理修复

桌面端与服务端共用 `crates/core`，`AppService::refresh_all` → `process_source_posts_with_progress` 是同一条分析链路（`src-tauri/src/lib.rs` 用 `AppService::new_with_proxy` 组装），所以 0.3.28 修的「来源清理误删真日程」在桌面端同样生效，无需额外改动。

## 行为变化

- 一条动态的 `content_digest` 变了（正文修改、图片增减）不再导致它上一轮写入的 `source_identity` 被当成脏数据清理。
- 只有「事件序号超出当前分析结果」或「同一序号已被本轮新编号覆盖」的来源才会被清理；被清理来源对应的日程如果一条来源都不剩，仍按原逻辑删除。
- 模型调用失败、或一条事件都没识别出来时，来源一条都不动。

## 影响

- 本地库里被误删的日程不会自动恢复；打开应用后对相应主播触发一次刷新（或等待后台轮询）即可重新落库，命中 `post_analyses` 缓存的会先按 12 小时窗口决定是否重问模型。
- 之前每轮刷新都会出现的 `pruned stale schedule sources` 与 `removed orphan schedules` 日志应当不再对同一条动态反复触发。

## 构建

版本号由 `node scripts/set-version.mjs desktop 0.3.20` 更新（`package.json`、`package-lock.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`Cargo.lock`），构建与签名交给 CI（`.github/workflows/release.yml`，tag `desktop-v0.3.20`）。
