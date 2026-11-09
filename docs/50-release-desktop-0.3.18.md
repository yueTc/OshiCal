# 桌面端 0.3.18

## 说明

桌面端与服务器共用 `crates/core`，0.3.26 的多图请求预算与超时不重试同样作用于客户端：
九宫格动态不再因为请求体过大而必然失败。

## 改动

- 跟随 `crates/core` 的 `MODEL_REQUEST_IMAGE_BUDGET_BYTES` / `max_side_for_budget`。
- 版本号 0.3.18，无接口与数据格式变更。

## 升级

安装包 `OshiCal_0.3.18_x64-setup.exe`，自动更新走 `latest.json`。
