# 桌面端 0.3.13 发布说明

和 [服务端 0.3.21](39-release-server-0.3.21.md) 同源：桌面端与服务器共用 `crates/core` 的分析管线，所以空结果缓存窗口与「只在识别出新事件时清旧来源」这两处改动在桌面端同时生效。

## 变更

- `crates/core/src/service.rs`：空结果写 `post_analyses` 并保留 12 小时，过期重试；`analysis_succeeded && !written_identities.is_empty()` 才清旧来源。
- `crates/core/src/store.rs`：`load_post_analysis_entry()` 返回 `created_at`。
- 版本号 0.3.13（`scripts/set-version.mjs desktop 0.3.13`）。

## 构建

`desktop-v0.3.13` tag 触发 Windows 构建，产物 `OshiCal_0.3.13_x64-setup.exe` + `.sig` + `latest.json`；本机不构建，全部在 CI 完成。

## 已知

- `text_analyses` / `image_analyses` 已是只读历史表，界面若还按这两张表展示分析缓存，需要改成 `post_analyses`。
