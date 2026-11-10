# 桌面端 0.3.19：设置页去掉多账号并行，「自动」不再超框

## 改了什么

- 设置页「日程识别模型」卡片删除「多账号并行」整块（账号列表、每账号密钥、添加/删除按钮）。
- 「并行个数」保留，说明文案改为「同时分析几条动态；同一时间最多发起这么多次模型请求」，保存时只做 1..=8 规范化，不再按账号数裁剪，输入框也不再禁用。
- 「思考程度」分段控件的按钮由固定 `width: 36px` 改为 `min-width: 36px` + `padding: 0 9px` + `white-space: nowrap`：中文两字（「自动」）按内容撑开，不再换行溢出到框外；纯图标按钮因为 `box-sizing: border-box` 仍是 36×32。
- 同步删除 `types.ts` / `lib/sync.ts` / `App.tsx` 里的账号类型、映射与两个账号密钥 action。

## 与 0.3.27 的配套

桌面端与服务端共用 `crates/core`，`AppSettings` 已经没有 `llm_accounts` 字段：客户端 0.3.19 连 0.3.27 服务端（或反过来）都按「字段缺省」处理，不会因为多一个未知字段报错。

## 构建

本地出包：`npx tauri build`（或 CI 的 Release workflow）。产物为 `src-tauri/target/release/bundle/nsis/OshiCal_0.3.19_x64-setup.exe`。
