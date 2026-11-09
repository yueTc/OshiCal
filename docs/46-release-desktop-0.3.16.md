# 桌面端 0.3.16

## 说明

桌面端与服务器共用 `crates/core`，0.3.24 的「排队刷新播报」改动同样作用于客户端内置服务：
桌面端自己的定时刷新与手动刷新也会互相排队，现在排队期间进度条会显示已等待秒数。

## 改动

- 跟随 `crates/core` 的 `wait_for_refresh_lock`（`RefreshProgress.stage` 为
  `等待上一轮刷新结束（已等待 N 秒）`）。
- 版本号 0.3.16，无接口变更，与 0.3.15 数据格式兼容。

## 测试

- `cargo test -p oshical-desktop --locked` → 14 passed
- 前端 `npm run test` 与 `tsc --noEmit` 在 CI 内通过

## 升级

安装包 `OshiCal_0.3.16_x64-setup.exe`，自动更新走 `latest.json`。
