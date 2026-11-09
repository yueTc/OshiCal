# 桌面端 0.3.14 发布说明

和 [服务端 0.3.22](41-release-server-0.3.22.md) 同源：`crates/core` 的刷新进度播报改动同样作用于桌面端的「刷新全部」。

## 变更

- `crates/core/src/service.rs`：新增 `refresh_all_with_progress`；进度目标改为 `ProgressTarget`，全量刷新用合成条目播报，不再停在 0%。
- 版本号 0.3.14（`scripts/set-version.mjs desktop 0.3.14`）。

## 构建

`desktop-v0.3.14` tag 触发 CI 构建，产物 `OshiCal_0.3.14_x64-setup.exe` + `.sig` + `latest.json`。
