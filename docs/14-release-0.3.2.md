# OshiCal 0.3.2：寻血猎犬（LAPLACE 生理期数据）

本次同时发布桌面端与服务端，标签 `desktop-v0.3.2`、`server-v0.3.2`。

## 新增

### 寻血猎犬开关

- 设置页新增「寻血猎犬」卡片，默认关闭；关闭状态下**不发出任何 LAPLACE 请求**，接口只返回 `enabled=false` 的空快照。
- 开启必须经过确认弹窗：
  - 免责声明：数据由 LAPLACE 投稿者提交并由其模型推算，本项目只做只读抓取；仅供娱乐参考，不是医疗信息，存在滞后与误差；数据涉及第三方个人信息，只保存在自己的数据库。
  - 致谢：感谢 LAPLACE（laplace.live/ovu）及其投稿者与维护者。
  - 按钮：「前往 LAPLACE」跳转原站、「取消」、「已阅读并同意，开启寻血猎犬」。只有点同意才写入 `fertilityEnabled=true`。
- 关闭不需要确认，保存后立即停止采集循环并清空主播库中的生理期展示。

### 采集

- 服务端：`server/src/runtime.rs` 新增独立后台循环，周期 6 小时，只有开关开启时才调用 `refresh_fertility_if_enabled()`。
- 桌面端本地模式：`spawn_local_monitor` 新增同周期循环，条件为 `dataMode == Local && fertilityEnabled`。
- 数据源：`https://workers.vrp.moe/laplace/fertilities?showOutdated=1`（实测 1638 条，含在册 998 条与历史角色）；直连失败重试 3 次后回退 `r.jina.ai` Reader 文本代理。
- 缓存：SQLite `fertility_cache`，有效期 6 小时；抓取失败回退旧缓存并标记 `stale` + 中文 `warning`，没有缓存才报错。
- 匹配：先用主播自身 UID，未命中再看关联官号；排序 `经期 → 易孕期 → 排卵期 → 常规`。

### 主播库

- 粉丝数左侧新增生理期徽标：`🩸 魔法期 · 第 2 天`、`🌸 易孕期`、`🥚 排卵期`、`💚 安全期`；未收录显示 `· 未收录`，未开启显示 `🐾 未开启`（点击跳到设置页）。
- 点击徽标打开详情弹窗：当前状态说明、周期第几天、下次生理期、排卵日、有效周期/经期长度、数据点、匹配账号 UID、数据来源与抓取时间、缓存提示，以及「前往 LAPLACE」「立即刷新」。

## 接口

- 服务端 `GET /api/fertility?refresh=true|false`。
- 桌面端命令 `list_fertility_statuses(refresh)`。
- 设置项 `fertility_enabled` / `fertilityEnabled`，随 `server_settings` 同步到其他设备。

## 其它改动

- 移除旧的「魔法期」页面与后端实现（`MagicPeriodView.tsx`、`crates/core/src/fertility.rs` 旧版本、`/api/fertility` 旧实现、`fertility_cache` 旧表结构），以本版本的开关式实现替代。
- 日程时间线改为滚动锚定的拖拽/滚轮布局（`src/lib/timeline-layout.ts` + `scripts/timeline-layout.test.ts`），并裁剪不再使用的样式。
- 外链白名单恢复允许 `https://laplace.live`。

## 验证

本机只做源码与静态检查：`npx tsc -b`、`npm run test:web`（含新增 `scripts/fertility.test.ts` 10 项与 `timeline-layout` 4 项）。Rust 格式、Clippy、单元测试与安装包构建全部由 GitHub Actions 执行。

## 免责声明与致谢（随发布一并公示）

- 生理期数据来自 LAPLACE 用户投稿与模型预测，本应用不生成、不校验、不修改这些数据，也不对其准确性负责。
- 这些数据仅供娱乐参考，**不是医疗信息**，不构成任何健康建议、诊断或决策依据。
- 感谢 LAPLACE（<https://laplace.live/ovu>）及其所有投稿者、维护者。如果站方认为本接入方式不妥，我们会立即下线该功能。
