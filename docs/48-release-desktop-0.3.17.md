# 桌面端 0.3.17

## 说明

桌面端与服务器共用 `crates/core`，0.3.25 的模型请求传输层重试同样作用于客户端内置服务：
网络抖动时单条动态不再直接判失败，而是退避 2 秒后最多重试两次。

## 改动

- 跟随 `crates/core` 的 `LLM_SEND_ATTEMPTS = 3`（`request_analysis` 内重试）。
- 版本号 0.3.17，无接口与数据格式变更。

## 测试

- `cargo test -p oshical-desktop --locked` → 14 passed
- 前端测试与 `tsc --noEmit` 在 CI 内通过

## 升级

安装包 `OshiCal_0.3.17_x64-setup.exe`，自动更新走 `latest.json`。
